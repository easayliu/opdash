import { Suspense, useEffect, useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { CloudDownloadIcon, XIcon } from 'lucide-react'
import { ApiError, apiDelete, apiGet, apiPost } from '@/api/client'
import { useBillSyncProgress } from '@/api/sync'
import type { BillProvider, BillSyncProgress, BillSyncRunning, BillSyncStarted, BillSyncTask } from '@/api/types'
import { Button, Combobox, Hint, ModalPanel, Spinner } from '@/components/ui'
import { PROVIDER_LABELS, periodsBetween } from '@/lib/bills'

/**
 * 手动拉一次账单。
 *
 * 账单并非推送而来，而是 goscan 按 cron 向云厂商拉取——刚接入、补历史账期，或当日调度尚未
 * 到点时，页面上便是空的。本对话框把「拉取一次」转交 goscan：
 * `POST /api/bills/sync` 登记一个后台任务并取得 task id，此后订阅它的事件流看进度（老版本 goscan
 * 没有事件流时退回每两秒轮询一次），完成后刷新账单查询的缓存，页面数字随之更新。
 *
 * **一次拉取需要时间**（按账期逐页调用云厂商 API），耗时数十秒至数分钟均属正常，因此此处
 * 不等待结果返回。关闭对话框后任务继续执行；重新打开时，若这朵云仍有同步在进行（包括 cron 起的），
 * 直接接上它的进度——goscan 同一朵云同时只允许一个同步，再点「开始拉取」也只会被 409 挡回来。
 *
 * 同步可以中途停下，但停在两趟之间：goscan 每一趟拉之前都先清空那个账期，半路掐断会留下只写了
 * 一半的账期，所以它会把手上这一趟写完再停。没跑的那几趟由 goscan 报回来，数据原样没动。
 *
 * 进度条的单位是**趟**：一个账期一种粒度算一趟，goscan 也只按趟上报（见它的 `TaskProgress`）。
 * 账期内翻了几页拿不到——那是各家 SDK 包装里的事，只进了日志。所以拉一个月的月度账单看到的是
 * 「0 / 1 → 1 / 1」，拉半年的月度加日度才有细腻的刻度；goscan 版本旧到不报进度时退回不确定进度条。
 */
/** 已用时：`1 分 12 秒`。任务在服务端跑，这里按它的开始时间算 */
function elapsed(startedAt: string | undefined, endedAt: string | undefined): string {
  if (!startedAt) return ''
  const from = new Date(startedAt).getTime()
  const to = endedAt ? new Date(endedAt).getTime() : Date.now()
  const seconds = Math.max(0, Math.round((to - from) / 1000))
  return seconds < 60 ? `${seconds} 秒` : `${Math.floor(seconds / 60)} 分 ${seconds % 60} 秒`
}

/**
 * 已用时，任务未结束时每秒走一次。
 *
 * 不能指望轮询带动刷新：轮询回来的状态若与上次相同（进度还停在同一趟），react-query 会沿用
 * 旧对象、组件不重新渲染，已用时就一直停在「0 秒」。所以自己起一个一秒的钟，任务结束即停表，
 * 此后按服务端给的结束时间显示定值。
 */
export function Elapsed({ startedAt, endedAt: rawEnd }: { startedAt: string; endedAt?: string }) {
  // Go 的零值时间（0001-01-01）表示「还没结束」，服务端已经滤掉，这里再兜一层：
  // 把它当成结束时间，钟就停了，还会算出负数截成「0 秒」
  const endedAt = rawEnd && new Date(rawEnd).getFullYear() > 1970 ? rawEnd : undefined
  const [, tick] = useState(0)
  useEffect(() => {
    if (endedAt) return
    const timer = setInterval(() => tick((n) => n + 1), 1000)
    return () => clearInterval(timer)
  }, [endedAt])
  return <span className="tabular-nums">已用 {elapsed(startedAt, endedAt)}</span>
}

/**
 * 进度条。goscan 报了账期进度就画确定的那种，没报就画一条来回扫的不确定条——
 * **不确定时不能显示百分比**：编一个数字出来比转圈更容易让人误判还要等多久。
 */
function ProgressBar({ task }: { task?: BillSyncTask }) {
  const progress = task?.progress
  const ratio = progress ? Math.min(1, progress.periods_done / Math.max(1, progress.periods_total)) : null
  return (
    <div
      role="progressbar"
      aria-label="账单拉取进度"
      aria-valuemin={0}
      aria-valuemax={progress?.periods_total ?? undefined}
      aria-valuenow={progress?.periods_done ?? undefined}
      aria-valuetext={progress ? `${progress.periods_done} / ${progress.periods_total} 趟` : '正在拉取'}
      className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-muted"
    >
      {ratio === null ? (
        <span className="block h-full w-1/3 animate-[cf-indeterminate_1.4s_ease-in-out_infinite] rounded-full bg-brand" />
      ) : (
        <span className="block h-full rounded-full bg-brand transition-[width] duration-500" style={{ width: `${ratio * 100}%` }} />
      )}
    </div>
  )
}

const GRANULARITIES = [
  { value: 'both', label: '月度 + 日度' },
  { value: 'monthly', label: '只要月度' },
  { value: 'daily', label: '只要日度' },
]

/** 进度里那一趟写的是哪张表。火山不分粒度，goscan 不报时这里就是空的 */
const GRANULARITY_LABELS: Record<string, string> = { monthly: '月度', daily: '日度' }

/** goscan 报的「没跑的那一趟」形如 `2026-04 daily`，换成 `2026-04 日度` */
function passLabel(pass: string): string {
  const [period, granularity] = pass.split(' ')
  const label = GRANULARITY_LABELS[granularity ?? '']
  return label ? `${period} ${label}` : pass
}

/**
 * 这一趟在拉什么：`2026-05 日度`。
 *
 * 选了「月度 + 日度」时同一个账期会出现两趟，只报账期会让人以为进度卡住了。
 */
function pulling(progress: BillSyncProgress): string {
  if (!progress.period) return ''
  const label = GRANULARITY_LABELS[progress.granularity ?? '']
  return label ? `${progress.period} ${label}` : progress.period
}

export function BillSyncDialog({
  providers,
  from,
  to,
  onClose,
}: {
  providers: BillProvider[]
  /** 默认拉页面上正在看的那段账期 */
  from: string
  to: string
  onClose: () => void
}) {
  const qc = useQueryClient()
  const [provider, setProvider] = useState<BillProvider>(providers[0] ?? 'alicloud')
  const [fromPeriod, setFromPeriod] = useState(from)
  const [toPeriod, setToPeriod] = useState(to)
  const [granularity, setGranularity] = useState('both')
  const [force, setForce] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [started, setStarted] = useState<BillSyncStarted | null>(null)
  /** 接上的是一个早已在跑的同步（不是本窗口发起的），给一句说明 */
  const [adopted, setAdopted] = useState(false)
  const [stopping, setStopping] = useState(false)
  const [stopError, setStopError] = useState<string | null>(null)
  const task = useBillSyncProgress(started?.task_id ?? null)

  /** 这朵云眼下有没有同步在跑；有就接上它的进度。找到了返回 true */
  const adopt = async (p: BillProvider): Promise<boolean> => {
    const running = await apiGet<BillSyncRunning>('/bills/sync/running', { provider: p })
    const t = running?.task
    if (!t?.id) return false
    setStarted({ task_id: t.id, provider: p, from: t.from ?? '', to: t.to ?? '', message: '' })
    setAdopted(true)
    return true
  }

  // 打开窗口、切换云时先看一眼：关掉窗口再打开，或者 cron 正在跑，都能直接看到进度
  useEffect(() => {
    if (started) return
    void adopt(provider).catch(() => {
      // 只是顺带看一眼：查不到不妨碍发起新的同步
    })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [provider])

  // 换了任务，停止按钮的状态从头算
  useEffect(() => {
    setStopping(false)
    setStopError(null)
  }, [started?.task_id])

  const stop = async () => {
    if (!started) return
    setStopping(true)
    setStopError(null)
    try {
      await apiDelete(`/bills/sync/${encodeURIComponent(started.task_id)}`)
    } catch (err) {
      setStopping(false)
      setStopError((err as Error).message)
    }
  }
  const cancelling = !task.data?.done && (stopping || !!task.data?.cancel_requested)

  // 完成后将账单相关查询全部作废：页面的图与表会自行重查，无需手动刷新
  const done = task.data?.done
  useEffect(() => {
    if (done) void qc.invalidateQueries({ queryKey: ['bills'], refetchType: 'all' })
  }, [done, qc])

  const months = periodsBetween(fromPeriod, toPeriod).length
  // 一个账期一种粒度算一趟：阿里云选「月度 + 日度」时要拉两倍的趟数，
  // 这直接决定了要等多久，填完账期就该看得见
  const pulls = months * (provider === 'alicloud' && granularity === 'both' ? 2 : 1)
  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      setStarted(
        await apiPost<BillSyncStarted>('/bills/sync', {
          provider,
          from: fromPeriod,
          to: toPeriod,
          granularity: provider === 'alicloud' ? granularity : undefined,
          force,
        }),
      )
    } catch (err) {
      // 这朵云已经有同步在跑：与其只报一句「正在同步中」，不如直接接上它的进度
      if (err instanceof ApiError && err.status === 409 && (await adopt(provider).catch(() => false))) return
      setError((err as Error).message)
    } finally {
      setBusy(false)
    }
  }

  return (
    <Suspense fallback={null}>
      <ModalPanel
        open
        onOpenChange={(next) => !next && onClose()}
        labelledBy="bill-sync-title"
        className="fixed inset-x-3 top-24 z-50 mx-auto flex max-h-[calc(100dvh-8rem)] max-w-lg flex-col overflow-hidden rounded-lg border border-border bg-card shadow-xl md:inset-x-auto md:left-1/2 md:w-[32rem] md:-translate-x-1/2"
      >
        <header className="flex shrink-0 items-start gap-2 border-b border-border px-4 py-3">
          <CloudDownloadIcon className="mt-0.5 size-4 shrink-0 text-muted-fg" />
          <div className="min-w-0 flex-1">
            <h2 id="bill-sync-title" className="text-sm font-semibold">
              拉取账单
            </h2>
            <p className="mt-0.5 text-2xs text-muted-fg">
              立即让 goscan 向云厂商拉取一次。任务完成后账单才会入库，其间页面数字不会变化
            </p>
          </div>
          <Button variant="ghost" className="px-2" onClick={onClose} title="关闭 (Esc)">
            <XIcon className="size-4" />
          </Button>
        </header>

        <form onSubmit={submit} className="flex flex-col gap-3 px-4 py-3 text-xs">
          <div className="flex items-center gap-2">
            <span className="w-16 shrink-0 text-muted-fg" aria-hidden="true">
              云
            </span>
            <Combobox
              value={provider}
              onChange={(v) => setProvider(v as BillProvider)}
              options={providers.map((p) => ({ value: p, label: PROVIDER_LABELS[p] }))}
              clearable={false}
              searchPlaceholder="筛云…"
              title="云"
              disabled={!!started}
              size="sm"
              floating
              className="flex-1"
            />
          </div>
          <label className="flex items-center gap-2">
            <span className="w-16 shrink-0 text-muted-fg">账期</span>
            <input
              value={fromPeriod}
              onChange={(e) => setFromPeriod(e.target.value)}
              placeholder="2026-04"
              aria-label="起始账期"
              disabled={!!started}
              className="h-8 w-28 rounded-md border border-input bg-card px-2 text-fg tabular-nums focus:border-brand focus:ring-2 focus:ring-brand/25 focus:outline-none"
            />
            <span className="text-muted-fg">至</span>
            <input
              value={toPeriod}
              onChange={(e) => setToPeriod(e.target.value)}
              placeholder="2026-09"
              aria-label="结束账期"
              disabled={!!started}
              className="h-8 w-28 rounded-md border border-input bg-card px-2 text-fg tabular-nums focus:border-brand focus:ring-2 focus:ring-brand/25 focus:outline-none"
            />
            <span className="text-2xs text-muted-fg">
              {months > 0 ? `共 ${months} 个账期${pulls > months ? `、${pulls} 趟` : ''}` : '账期格式为 2026-09'}
            </span>
          </label>
          {provider === 'alicloud' && (
            <div className="flex items-center gap-2">
              <span className="w-16 shrink-0 text-muted-fg" aria-hidden="true">
                粒度
              </span>
              <Combobox
                value={granularity}
                onChange={setGranularity}
                options={GRANULARITIES}
                clearable={false}
                searchPlaceholder="筛粒度…"
                title="粒度"
                disabled={!!started}
                size="sm"
                floating
                className="flex-1"
              />
            </div>
          )}
          <Hint text="不勾选时，goscan 遇到已有数据的账期会跳过；补数据或云厂商调整过账单时请勾选">
            <label className="flex items-center gap-2">
              <span className="w-16 shrink-0" aria-hidden="true" />
              <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} disabled={!!started} className="size-3.5 accent-[var(--brand)]" />
              已有数据也重新拉取
            </label>
          </Hint>

          {error && <p className="rounded-md bg-danger-soft px-3 py-2 text-danger">{error}</p>}

          {started && adopted && (
            <p className="rounded-md bg-accent-soft px-3 py-2 text-accent">
              {PROVIDER_LABELS[started.provider]}已有一个同步正在进行（可能由定时任务发起），同一朵云同时只能有一个同步，以下是它的进度
            </p>
          )}

          {started && (
            <div className="rounded-md border border-border bg-muted/30 px-3 py-2">
              <div className="flex items-center gap-2">
                {!task.data?.done && <Spinner className="size-3.5" />}
                <span className="font-medium">
                  {task.data?.done
                    ? task.data.status === 'cancelled'
                      ? '已停止'
                      : task.data.ok
                        ? '同步完成'
                        : '同步失败'
                    : cancelling
                      ? '正在停止…'
                      : task.data?.status === 'running'
                        ? '正在拉取…'
                        : '已提交，等待执行…'}
                </span>
                <span className="ml-auto text-2xs text-muted-fg">
                  {PROVIDER_LABELS[started.provider]}
                  {started.from && ` · ${started.from}${started.to && started.to !== started.from ? ` 至 ${started.to}` : ''}`}
                </span>
              </div>
              <ProgressBar task={task.data} />
              <p className="mt-1 flex flex-wrap items-center gap-x-2 text-2xs text-muted-fg">
                {task.data?.progress && (
                  <span className="tabular-nums">
                    {task.data.progress.periods_done} / {task.data.progress.periods_total} 趟
                    {pulling(task.data.progress) && ` · 正在拉 ${pulling(task.data.progress)}`}
                  </span>
                )}
                {!task.data?.done && !!task.data?.progress?.records && (
                  <span className="tabular-nums">
                    本趟已写入 {task.data.progress.records.toLocaleString('zh-CN')}
                    {task.data.progress.records_total ? ` / ${task.data.progress.records_total.toLocaleString('zh-CN')}` : ''} 行
                  </span>
                )}
                {task.data?.started_at && <Elapsed startedAt={task.data.started_at} endedAt={task.data.ended_at} />}
              </p>
              {task.data?.done && task.data.ok && (
                <p className="mt-1 text-2xs text-muted-fg">
                  取回 {task.data.fetched.toLocaleString('zh-CN')} 条，写入 {task.data.records.toLocaleString('zh-CN')} 条；页面数据已重新查询
                </p>
              )}
              {task.data?.done && task.data.status === 'cancelled' && (
                <p className="mt-1 text-2xs text-muted-fg">
                  停止前已写入 {task.data.records.toLocaleString('zh-CN')} 条；页面数据已重新查询
                  {task.data.not_run?.length
                    ? `。未执行：${task.data.not_run.map(passLabel).join('、')}，这些账期的数据保持原样`
                    : ''}
                </p>
              )}
              {task.data?.done && !task.data.ok && task.data.status !== 'cancelled' && (
                <p className="mt-1 text-2xs text-danger">{task.data.error || task.data.message || '详情请查看 goscan 日志'}</p>
              )}
              {!task.data?.done && (
                <p className="mt-1 text-2xs text-muted-fg">
                  {cancelling
                    ? '已请求停止：goscan 会把当前这一趟写完再停，以免留下只写了一半的账期，通常需要数秒至数分钟'
                    : '需按账期逐页调用云厂商接口，通常耗时数十秒至数分钟；关闭本窗口不会中断任务，重新打开仍可查看进度'}
                </p>
              )}
              {stopError && <p className="mt-1 text-2xs text-danger">无法停止：{stopError}</p>}
              {task.error && <p className="mt-1 text-2xs text-danger">无法获取任务状态：{task.error.message}</p>}
            </div>
          )}

          <div className="flex items-center justify-end gap-2 pt-1">
            <Button type="button" variant="ghost" onClick={onClose}>
              {started?.task_id && task.data?.done ? '完成' : '关闭'}
            </Button>
            {!started && (
              <Button type="submit" variant="primary" disabled={busy || months === 0}>
                {busy ? '提交中…' : '开始拉取'}
              </Button>
            )}
            {started && !task.data?.done && (
              <Hint text="goscan 会把当前这一趟（一个账期 × 一种粒度）写完再停，未执行的账期保持原样" asChild>
                <Button type="button" variant="danger" onClick={stop} disabled={cancelling}>
                  {cancelling ? '正在停止…' : '停止同步'}
                </Button>
              </Hint>
            )}
            {started && task.data?.done && (
              <Button
                type="button"
                onClick={() => {
                  setStarted(null)
                  setAdopted(false)
                  setError(null)
                }}
              >
                再拉取一次
              </Button>
            )}
          </div>
        </form>
      </ModalPanel>
    </Suspense>
  )
}
