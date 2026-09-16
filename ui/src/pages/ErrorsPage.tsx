import { useMemo, useState } from 'react'
import { Link } from 'react-router'
import { AlertTriangleIcon, SearchIcon } from 'lucide-react'
import { useErrorGroups, useLogSearch, useMeta, useTraceValues } from '@/api/queries'
import type { ErrorGroup } from '@/api/types'
import { StatsLine } from '@/components/StatsLine'
import { Badge, Button, Card, Combobox, EmptyState, ErrorBox, Input, Spinner, type ComboOption } from '@/components/ui'
import { errorTitle, errorTitleFull, errorWhere, hasDetail, shortException } from '@/lib/errors'
import { errorsHref, logsHref, tracesHref, traceHref, type Window } from '@/lib/links'
import { formatNumber, formatTs } from '@/lib/time'
import { useFrom, useTimeRange, useUrlState } from '@/lib/url-state'
import { cn } from '@/lib/utils'

/**
 * `kind` 三档。默认**入口错误**，因为服务总览上那个错误率就是按入口 span（Server /
 * Consumer）算的——默认值一致，从 dash 点进来看到的才是「那个错误率到底由什么组成」。
 *
 * 不限 kind 的话列表会被下游 HTTP 404 埋掉：线上一小时 5.6 万条错误 span 里 4 万条是
 * Client 的 404，而入口错误一共才 194 条 13 组。
 */
const KINDS = [
  { value: 'entry', label: '入口错误', hint: '这个服务自己对外返回的错误（Server / Consumer），和服务总览上的错误率同一口径' },
  { value: 'client', label: '下游调用', hint: '这个服务调别人时失败的（Client / Producer）' },
  { value: 'all', label: '全部', hint: '两样都看；下游 HTTP 404 通常会占满列表' },
] as const
type Kind = (typeof KINDS)[number]['value']

/** 展开一组时，从样本链路里拉几条错误日志当堆栈用 */
const STACK_LOG_LIMIT = 5
/**
 * 拉堆栈时围着样本那一刻取多宽的时间窗。
 *
 * 一条链路的日志都在它自己那几秒里，前后各一小时已经宽得离谱；但**不给时间范围的代价是
 * 天差地别**：线上实测同一次展开，不给是 2.8 亿行 / 5.2 GB / 3.7 秒，给了是 21 万行 /
 * 15.5 MB / 0.16 秒。日志表按 `(timestamp, level, trace_id)` 排，时间范围直接走主键，
 * 而只按 trace_id 点查只能靠 bloom filter，它有 2.5% 的误判率，摊到 30 天上就是几亿行。
 */
const STACK_WINDOW_MS = 60 * 60_000

export function ErrorsPage() {
  const meta = useMeta()
  const { range } = useTimeRange()
  const { params, set } = useUrlState()
  const kind = (KINDS.find((k) => k.value === params.get('kind'))?.value ?? 'entry') as Kind
  const service = params.get('service') ?? ''
  // 从服务详情页选中某个接口点过来、从「同接口的其它报错」点过来时带的。**必须传给查询**，
  // 不然人以为在看这一个接口的错，看到的却是整个服务的，而页面上一点提示都没有
  const spanName = params.get('span_name') ?? ''
  const opened = params.get('g') ?? ''
  const [needle, setNeedle] = useState('')
  const win: Window = { fromMs: range.fromMs, toMs: range.toMs }

  const q = useErrorGroups({
    from: range.fromMs,
    to: range.toMs,
    kind,
    service: service || undefined,
    span_name: spanName || undefined,
  })
  const services = useTraceValues({ from: range.fromMs, to: range.toMs, field: 'service', limit: 500 })
  const serviceOptions: ComboOption[] = useMemo(() => {
    const vals = services.data?.values ?? []
    const list = service && !vals.some((v) => v.value === service) ? [{ value: service, count: 0 }, ...vals] : vals
    return list.map((v) => ({ value: v.value, note: v.count ? v.count.toLocaleString('zh-CN') : undefined }))
  }, [services.data, service])

  const groups = useMemo(() => {
    const n = needle.trim().toLowerCase()
    if (!n) return q.data?.groups ?? []
    return (q.data?.groups ?? []).filter((g) =>
      [g.service, g.span_name, g.exception, g.message, g.http_status, g.peer].some((f) => f.toLowerCase().includes(n)),
    )
  }, [q.data, needle])
  const kindHint = KINDS.find((k) => k.value === kind)?.hint

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-border bg-card px-3 py-2.5 md:px-4 md:py-3">
        <h1 className="text-base font-semibold">错误</h1>
        {spanName ? (
          <span className="inline-flex min-w-0 max-w-full items-center gap-1.5 rounded-md bg-accent-soft px-2.5 py-1 text-xs text-accent">
            <span className="truncate">{spanName}</span>
            <button type="button" onClick={() => set({ span_name: null, g: null })} title="看整个服务的错误">
              ✕
            </button>
          </span>
        ) : (
          <span className="hidden text-xs text-muted-fg xl:inline">同一种报错归一组，按次数排</span>
        )}
        {q.isFetching && <Spinner className="size-4" />}
        <span className="ml-auto flex flex-wrap items-center gap-2">
          <StatsLine stats={q.data?.stats} className="hidden text-2xs text-muted-fg 2xl:inline" />
          <span className="flex h-8 items-center rounded-md border border-input p-0.5">
            {KINDS.map((k) => (
              <button
                key={k.value}
                type="button"
                title={k.hint}
                onClick={() => set({ kind: k.value === 'entry' ? null : k.value, g: null })}
                className={cn('h-full rounded-sm px-2.5 text-xs text-muted-fg hover:text-fg', kind === k.value && 'bg-accent-soft text-accent')}
              >
                {k.label}
              </button>
            ))}
          </span>
          <Combobox
            value={service}
            onChange={(v) => set({ service: v || null, span_name: null, g: null })}
            options={serviceOptions}
            placeholder="全部服务"
            searchPlaceholder="筛服务名…"
            emptyText="没有匹配的服务"
            loading={services.isPending}
            className="w-52"
            title="只看这个服务的错误"
          />
          <span className="relative">
            <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-fg" />
            <Input value={needle} onChange={(e) => setNeedle(e.target.value)} placeholder="筛报错" className="h-8 w-36 pl-8 text-xs" aria-label="筛报错" />
          </span>
        </span>
      </header>

      <div className="min-h-0 flex-1 overflow-auto p-3 md:p-4">
        {q.isError && <ErrorBox error={q.error} onRetry={() => q.refetch()} />}
        {q.isPending && (
          <div className="flex justify-center py-16">
            <Spinner />
          </div>
        )}
        {q.data && !q.data.groups.length && (
          <EmptyState
            title={service ? `${spanName || service} 这段时间没有${kind === 'client' ? '失败的下游调用' : '错误'}` : '这段时间没有错误'}
            hint={kind === 'entry' ? '这里只看入口 span（Server / Consumer）。服务调下游失败但自己兜住了的，切到「下游调用」看。' : undefined}
          />
        )}
        {q.data && q.data.groups.length > 0 && (
          <>
            <div className="mb-3 flex flex-wrap items-baseline gap-x-4 gap-y-1 text-xs text-muted-fg">
              <span>
                <span className="text-base font-semibold text-fg tabular-nums">{formatNumber(q.data.total)}</span> 条错误
              </span>
              <span>
                <span className="font-medium text-fg tabular-nums">{q.data.groups.length}</span> 种
                {groups.length !== q.data.groups.length && <span className="text-muted-fg">（筛出 {groups.length} 种）</span>}
              </span>
              {kindHint && <span className="hidden md:inline">{kindHint}</span>}
            </div>
            {!groups.length && <EmptyState title="没有匹配的报错" />}
            {groups.length > 0 && (
              <Card className={cn('overflow-hidden', q.isFetching && 'opacity-70')}>
                <ul>
                  {groups.map((g) => (
                    <GroupRow
                      key={g.id}
                      g={g}
                      win={win}
                      open={opened === g.id}
                      // 展开/收起是瞬态 UI 状态，不是「换了在看的东西」，用 replace：
                      // push 的话扫一遍列表展开五组，back 就要按五次才出得去
                      onToggle={() => set({ g: opened === g.id ? null : g.id }, { replace: true })}
                      logDim={meta.data?.logs.dimensions.includes('service_name') ? 'service_name' : 'container'}
                    />
                  ))}
                </ul>
              </Card>
            )}
          </>
        )}
      </div>
    </div>
  )
}

/** 一组报错。折叠时一行讲清「什么错、谁在错、多少次、最后一次什么时候」 */
function GroupRow({ g, win, open, onToggle, logDim }: { g: ErrorGroup; win: Window; open: boolean; onToggle: () => void; logDim: string }) {
  return (
    <li className={cn('border-b border-border/60 last:border-b-0', open && 'bg-muted/30')}>
      <button type="button" onClick={onToggle} className="row-hover flex w-full items-center gap-3 px-3 py-2.5 text-left">
        <AlertTriangleIcon className={cn('size-4 shrink-0', hasDetail(g) ? 'text-danger' : 'text-warn')} />
        <span className="min-w-0 flex-1">
          <span className="flex min-w-0 items-baseline gap-2">
            <span className="truncate text-xs font-semibold" title={errorTitleFull(g)}>
              {errorTitle(g)}
            </span>
            {g.exception && g.http_status && <Badge tone="muted">HTTP {g.http_status}</Badge>}
            {!hasDetail(g) && (
              <Badge tone="warn" title="这些 span 上没有 exception 事件——异常多半被全局异常处理器接住了。展开看日志里的堆栈。">
                无异常信息
              </Badge>
            )}
          </span>
          {g.message && (
            <span className="mono mt-0.5 block truncate text-2xs text-fg/80" title={g.message}>
              {g.message}
            </span>
          )}
          <span className="mt-0.5 block truncate text-2xs text-muted-fg" title={errorWhere(g)}>
            {errorWhere(g)}
          </span>
        </span>
        <span className="shrink-0 text-right">
          <span className="block text-sm font-semibold tabular-nums">{formatNumber(g.count)}</span>
          <span className="block text-2xs text-muted-fg tabular-nums">{formatNumber(g.traces)} 条链路</span>
        </span>
        <span className="hidden shrink-0 text-right text-2xs text-muted-fg tabular-nums sm:block" title={`最早 ${formatTs(g.first_ms)}\n最后 ${formatTs(g.last_ms)}`}>
          <span className="block">最后一次</span>
          <span className="block">{formatTs(g.last_ms, { ms: false, date: false })}</span>
        </span>
      </button>
      {open && <GroupDetail g={g} win={win} logDim={logDim} />}
    </li>
  )
}

/**
 * 展开后的详情。**这里才是「不用自己去选」兑现的地方**：
 *
 * * 异常全文和完整类名（列表上截短了）；
 * * 堆栈——按样本 trace id 去日志表点查（trace_id 上有 bloom filter，不带时间范围也快）。
 *   span 上没有 exception 事件的那五分之四，原因只能在这儿拿到；
 * * 三个去处，每个都已经把服务、接口、时间填好了，点过去不用再筛一遍。
 */
function GroupDetail({ g, win, logDim }: { g: ErrorGroup; win: Window; logDim: string }) {
  const from = useFrom()
  /**
   * 这里**要**带时间范围，和 `logsHref` 按 trace id 跳日志页的规矩相反。
   *
   * 那条规矩是「人手上只有一个 trace id，不知道它是什么时候的，收窄时间范围会把它挡掉」。
   * 而这一组自己就带着 `last_ms`——`sample_trace` 正是 `argMax(trace_id, timestamp)` 取出来的，
   * 两者指的是同一条 span，时刻是确定的。已知时刻还去扫全表没有道理，见 [`STACK_WINDOW_MS`]。
   *
   * `count: 0` 关掉服务端那条并发的 `count()`：这里只显示前几条堆栈，不显示总数，
   * 白扫一遍同样的数据（单这一条就从 2.7 秒降到 1.9 秒）。
   */
  const logs = useLogSearch({
    trace_id: g.sample_trace,
    level: 'ERROR,WARN',
    limit: STACK_LOG_LIMIT,
    order: 'asc',
    count: 0,
    from: g.last_ms - STACK_WINDOW_MS,
    to: g.last_ms + STACK_WINDOW_MS,
  })
  const rows = logs.data?.rows ?? []
  return (
    <div className="border-t border-border/60 px-3 py-3">
      {g.exception && (
        <div className="mono mb-2 text-2xs break-all text-muted-fg">
          {g.exception}
          {g.message && <span className="text-fg">: {g.message}</span>}
        </div>
      )}
      <div className="mb-2 flex flex-wrap items-center gap-2">
        <Link to={traceHref(g.sample_trace, g.last_ms, g.sample_span)} state={from}>
          <Button size="xs" variant="primary">
            看最近这一条链路
          </Button>
        </Link>
        <Link to={tracesHref({ service: g.service, spanName: g.span_name, errorOnly: true }, win)}>
          <Button size="xs">这个接口的全部错误链路</Button>
        </Link>
        <Link to={logsHref({ dim: logDim, service: g.service, levels: 'ERROR,WARN' }, win)}>
          <Button size="xs">这个服务的错误日志</Button>
        </Link>
        <span className="text-2xs text-muted-fg">
          {formatTs(g.first_ms)} 起，共 {formatNumber(g.count)} 次
        </span>
      </div>
      <div className="rounded-md border border-border bg-card">
        <div className="flex items-center gap-2 border-b border-border/60 px-2.5 py-1.5 text-2xs text-muted-fg">
          最近这条链路里的错误日志
          {logs.isFetching && <Spinner className="size-3" />}
          <Link to={logsHref({ traceId: g.sample_trace })} className="ml-auto text-accent hover:underline">
            全部日志
          </Link>
        </div>
        {logs.isError && <ErrorBox error={logs.error} />}
        {logs.data && !rows.length && (
          <div className="px-2.5 py-2 text-2xs text-muted-fg">
            这条链路没有 ERROR / WARN 日志。日志里要打 [TID:…] 才能按 trace id 关联；Go / nginx 这类不打 TID 的服务这里看不到。
          </div>
        )}
        {rows.map((r, i) => (
          <div key={`${r.ts_ms}-${i}`} className="border-t border-border/60 px-2.5 py-1.5 first:border-t-0">
            <div className="flex flex-wrap items-baseline gap-x-2 text-2xs text-muted-fg">
              <span className="tabular-nums">{formatTs(r.ts_ms)}</span>
              <Badge tone={r.level === 'ERROR' ? 'danger' : 'warn'}>{r.level}</Badge>
              <span className="mono truncate" title={r.logger}>
                {shortException(r.logger)}
              </span>
            </div>
            {/* 堆栈本来就是多行，按原样排版；太长的截住，全文去日志页看 */}
            <pre className="mono mt-1 max-h-60 overflow-auto text-2xs leading-5 whitespace-pre-wrap text-fg/90">{r.message}</pre>
          </div>
        ))}
      </div>
      <div className="mt-2 text-2xs text-muted-fg">
        样本链路 <Link to={traceHref(g.sample_trace, g.last_ms, g.sample_span)} state={from} className="mono text-accent hover:underline">{g.sample_trace}</Link>
        ，点进去会直接选中报错的那个 span。这一组还有{' '}
        <Link to={errorsHref({ service: g.service, spanName: g.span_name }, win)} className="text-accent hover:underline">
          同接口的其它报错
        </Link>
        。
      </div>
    </div>
  )
}
