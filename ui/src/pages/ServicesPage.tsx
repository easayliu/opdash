import { useMemo, useState } from 'react'
import { Link, useNavigate } from 'react-router'
import { AlertTriangleIcon, ChartLineIcon, GitBranchIcon, LayoutGridIcon, RotateCwIcon, ScrollTextIcon, SearchIcon, TableIcon } from 'lucide-react'
import { useMeta, useMetricEvents, useOperations, useServices } from '@/api/queries'
import type { MetricEvent, OverviewResponse, ServiceStat } from '@/api/types'
import { Sparkline } from '@/components/charts/Sparkline'
import { StatsLine } from '@/components/StatsLine'
import { Button, Card, EmptyState, ErrorBox, Input, Select, Spinner } from '@/components/ui'
import { change, changeTone, formatChange, healthRank, meaningfulLatency, pct, serviceHealth, type Health } from '@/lib/health'
import { logsHref, metricsHref, serviceHref, tracesHref, type Window } from '@/lib/links'
import { formatDurationMs, formatNumber, formatTs } from '@/lib/time'
import { useTimeRange, useUrlState } from '@/lib/url-state'
import { useIsMobile } from '@/lib/media'
import { cn } from '@/lib/utils'

type SortKey = keyof Pick<ServiceStat, 'service' | 'requests' | 'rps' | 'errors' | 'error_rate' | 'p50_ms' | 'p95_ms' | 'p99_ms' | 'max_ms'>

const COLUMNS: { key: SortKey; label: string; align?: 'right'; title?: string }[] = [
  { key: 'service', label: '服务' },
  { key: 'requests', label: '请求数', align: 'right', title: 'Server + Consumer span 数' },
  { key: 'rps', label: 'QPS', align: 'right', title: '按整个时间范围平均' },
  { key: 'errors', label: '错误', align: 'right' },
  { key: 'error_rate', label: '错误率', align: 'right' },
  { key: 'p50_ms', label: 'P50', align: 'right' },
  { key: 'p95_ms', label: 'P95', align: 'right' },
  { key: 'p99_ms', label: 'P99', align: 'right' },
  { key: 'max_ms', label: '最大', align: 'right' },
]

/** 对比基线。默认昨天同时段：和上一小时比的话，白天永远在涨、晚上永远在跌 */
const COMPARE = [
  { value: 'day', label: '和昨天同时段比', short: '昨天同时段' },
  { value: 'week', label: '和上周同时段比', short: '上周同时段' },
  { value: 'prev', label: '和上一周期比', short: '上一周期' },
] as const
type Compare = (typeof COMPARE)[number]['value']

/** 卡片排序 */
const ORDERS = [
  { value: 'health', label: '异常在前' },
  { value: 'rps', label: '按请求量' },
  { value: 'error', label: '按错误率' },
  { value: 'p95change', label: '按 P95 变化' },
  { value: 'name', label: '按名字' },
] as const
type Order = (typeof ORDERS)[number]['value']

export function ErrorRate({ rate }: { rate: number }) {
  const p = rate * 100
  const tone = p >= 5 ? 'text-danger' : p >= 1 ? 'text-warn' : 'text-muted-fg'
  return (
    <span className={cn('inline-flex items-center justify-end gap-1 tabular-nums', tone)}>
      {p >= 1 && <AlertTriangleIcon className="size-3.5" aria-label={p >= 5 ? '错误率高' : '错误率偏高'} />}
      {pct(rate)}
    </span>
  )
}

/** 健康度的那个点：红 / 黄 / 绿，hover 看原因 */
function HealthDot({ level, reasons }: { level: Health; reasons: string[] }) {
  return (
    <span
      className={cn('inline-block size-2 shrink-0 rounded-full', level === 'bad' && 'bg-danger', level === 'warn' && 'bg-warn', level === 'ok' && 'bg-ok')}
      title={reasons.length ? reasons.join('；') : '正常'}
    />
  )
}

/** 变化那一小段：`+12%` 红绿灰 */
function Delta({ delta, upIs, className }: { delta: number | null; upIs: 'bad' | 'neutral'; className?: string }) {
  const tone = changeTone(delta, upIs)
  return (
    <span className={cn('shrink-0 text-2xs tabular-nums', tone === 'danger' && 'text-danger', tone === 'ok' && 'text-ok', tone === 'muted' && 'text-muted-fg', className)}>
      {formatChange(delta)}
    </span>
  )
}

/** 一个数字 + 变化 */
function Stat({ label, value, delta, upIs, big, muted }: { label: string; value: string; delta: number | null; upIs: 'bad' | 'neutral'; big?: boolean; muted?: boolean }) {
  return (
    <div className="min-w-0">
      <div className="text-2xs text-muted-fg">{label}</div>
      <div className="flex items-baseline gap-1.5">
        <span className={cn('truncate font-semibold tabular-nums', big ? 'text-xl' : 'text-base', muted && 'font-normal text-muted-fg')}>{value}</span>
        {!muted && <Delta delta={delta} upIs={upIs} />}
      </div>
    </div>
  )
}

function fmtRps(rps: number): string {
  return rps < 10 ? `${rps.toFixed(2)}/s` : `${formatNumber(Math.round(rps))}/s`
}

/** 三个小图标：不进详情页直接跳日志 / 链路 / 指标（hover 才出来，和 CF 列表的行内操作一样） */
function QuickLinks({ service, win, logDim, hasMetrics, className }: { service: string; win: Window; logDim: string; hasMetrics: boolean; className?: string }) {
  const stop = (e: React.MouseEvent) => e.stopPropagation()
  return (
    <span className={cn('flex items-center gap-0.5', className)} onClick={stop}>
      <Link to={logsHref({ dim: logDim, service, levels: 'ERROR,WARN' }, win)} title="错误日志" className="rounded p-1 text-muted-fg hover:bg-muted hover:text-fg">
        <ScrollTextIcon className="size-3.5" />
      </Link>
      <Link to={tracesHref({ service, sort: 'duration', kinds: 'Server,Consumer' }, win)} title="最慢的链路" className="rounded p-1 text-muted-fg hover:bg-muted hover:text-fg">
        <GitBranchIcon className="size-3.5" />
      </Link>
      {hasMetrics && (
        <Link to={metricsHref(service, win)} title="指标看板" className="rounded p-1 text-muted-fg hover:bg-muted hover:text-fg">
          <ChartLineIcon className="size-3.5" />
        </Link>
      )}
    </span>
  )
}

/**
 * 异常服务卡上多的一行：**P95 是哪个接口拖的**。只对被判成异常的服务查（一般就几个），
 * 当前窗和对比窗各查一次入口接口表，挑请求数够多里 P95 最高的那个。
 */
function Contributor({ service, win, prev }: { service: string; win: Window; prev: Window }) {
  const cur = useOperations(service, { from: win.fromMs, to: win.toMs, kind: 'entry' })
  const before = useOperations(service, { from: prev.fromMs, to: prev.toMs, kind: 'entry' })
  const pick = useMemo(() => {
    const ops = (cur.data?.operations ?? []).filter((o) => o.requests >= 30)
    if (!ops.length) return null
    const top = [...ops].sort((a, b) => b.p95_ms - a.p95_ms)[0]
    const old = before.data?.operations.find((o) => o.span_name === top.span_name && o.kind === top.kind)
    return { top, old }
  }, [cur.data, before.data])
  if (!pick) return null
  const { top, old } = pick
  return (
    <div className="mt-1 truncate text-2xs text-muted-fg" title={`${top.span_name}（${formatNumber(top.requests)} 次）`}>
      主要是 <span className="mono text-fg">{top.span_name}</span>：P95 {old ? `${formatDurationMs(old.p95_ms)} → ` : ''}
      <span className="font-semibold text-fg">{formatDurationMs(top.p95_ms)}</span>
    </div>
  )
}

/** 卡上的重启小标 */
function Restarts({ events }: { events: MetricEvent[] }) {
  if (!events.length) return null
  const restarts = events.filter((e) => e.kind === 'restart')
  const starts = events.length - restarts.length
  return (
    <span
      className="inline-flex items-center gap-1 rounded-sm bg-warn-soft px-1.5 py-px text-2xs text-warn"
      title={events.map((e) => `${formatTs(e.t_ms, { ms: false, date: false })} ${e.kind === 'restart' ? '重启' : '新起'} ${e.pod}`).join('\n')}
    >
      <RotateCwIcon className="size-3" />
      {restarts.length > 0 && `${restarts.length} 次重启`}
      {restarts.length > 0 && starts > 0 && ' · '}
      {starts > 0 && `${starts} 个新 pod`}
    </span>
  )
}

/**
 * 服务总览——首页。回答的是「现在谁不对」：先一行全站数字，再是异常服务的大卡（带主因、
 * 重启标记），正常的压成一行一个。数字全带和对比窗口的变化，对比窗口默认昨天同时段。
 */
export function ServicesPage() {
  const { range } = useTimeRange()
  const { params, set } = useUrlState()
  const meta = useMeta()
  const navigate = useNavigate()
  const isMobile = useIsMobile()
  const compare = (COMPARE.find((c) => c.value === params.get('cmp'))?.value ?? 'day') as Compare
  const order = (ORDERS.find((o) => o.value === params.get('sort'))?.value ?? 'health') as Order
  const view = params.get('view') === 'table' ? 'table' : 'cards'
  const onlyBad = params.get('bad') === '1'
  const [needle, setNeedle] = useState('')
  const [tableSort, setTableSort] = useState<{ key: SortKey; desc: boolean }>({ key: 'requests', desc: true })
  const win: Window = { fromMs: range.fromMs, toMs: range.toMs }
  const logDim = meta.data?.logs.dimensions.includes('service_name') ? 'service_name' : 'container'
  const hasMetrics = !!meta.data?.metrics

  const q = useServices({ from: range.fromMs, to: range.toMs, compare })
  const prevWin: Window = { fromMs: q.data?.prev_from_ms ?? range.fromMs, toMs: q.data?.prev_to_ms ?? range.toMs }
  // 全站的重启 / 新 pod，一次查完按服务分
  const events = useMetricEvents({ from: range.fromMs, to: range.toMs, metric: 'jvm.cpu.time' }, hasMetrics)
  const eventsByService = useMemo(() => {
    const m = new Map<string, MetricEvent[]>()
    for (const e of events.data?.events ?? []) m.set(e.service, [...(m.get(e.service) ?? []), e])
    return m
  }, [events.data])

  const all = useMemo(() => (q.data?.services ?? []).map((s) => ({ s, health: serviceHealth(s) })), [q.data])
  const shown = useMemo(() => {
    const n = needle.trim().toLowerCase()
    const list = all.filter((x) => !n || x.s.service.toLowerCase().includes(n)).filter((x) => !onlyBad || x.health.level !== 'ok')
    const by: Record<Order, (a: (typeof list)[number], b: (typeof list)[number]) => number> = {
      health: (a, b) => healthRank(a.health.level) - healthRank(b.health.level) || b.s.rps - a.s.rps,
      rps: (a, b) => b.s.rps - a.s.rps,
      error: (a, b) => b.s.error_rate - a.s.error_rate || b.s.rps - a.s.rps,
      p95change: (a, b) => (change(b.s.p95_ms, b.s.prev?.p95_ms) ?? -Infinity) - (change(a.s.p95_ms, a.s.prev?.p95_ms) ?? -Infinity),
      name: (a, b) => a.s.service.localeCompare(b.s.service),
    }
    return [...list].sort(by[order])
  }, [all, needle, onlyBad, order])
  const bad = shown.filter((x) => x.health.level !== 'ok')
  const fine = shown.filter((x) => x.health.level === 'ok')
  const badCount = all.filter((x) => x.health.level !== 'ok').length
  const tableRows = useMemo(() => {
    const list = shown.map((x) => x.s)
    list.sort((a, b) => {
      const va = a[tableSort.key]
      const vb = b[tableSort.key]
      const c = typeof va === 'string' && typeof vb === 'string' ? va.localeCompare(vb) : Number(va) - Number(vb)
      return tableSort.desc ? -c : c
    })
    return list
  }, [shown, tableSort])
  const cmpShort = COMPARE.find((c) => c.value === compare)!.short

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-border bg-card px-3 py-2.5 md:px-4 md:py-3">
        <h1 className="text-base font-semibold">服务</h1>
        <span className="hidden text-xs text-muted-fg xl:inline">按入口 span（Server / Consumer）算</span>
        {q.isFetching && <Spinner className="size-4" />}
        <span className="ml-auto flex flex-wrap items-center gap-2">
          <Button size="sm" active={onlyBad} onClick={() => set({ bad: onlyBad ? null : '1' })} disabled={!badCount && !onlyBad} title="只看错误率或延迟异常的">
            <AlertTriangleIcon className="size-3.5" />
            异常 {badCount}
          </Button>
          <span className="relative">
            <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-fg" />
            <Input value={needle} onChange={(e) => setNeedle(e.target.value)} placeholder="搜服务" className="h-8 w-36 pl-8 text-xs" aria-label="搜服务" />
          </span>
          <Select value={compare} onChange={(e) => set({ cmp: e.target.value === 'day' ? null : e.target.value })} className="h-8 text-xs" title="所有变化和哪一段时间比">
            {COMPARE.map((c) => (
              <option key={c.value} value={c.value}>
                {c.label}
              </option>
            ))}
          </Select>
          {view === 'cards' && (
            <Select value={order} onChange={(e) => set({ sort: e.target.value === 'health' ? null : e.target.value })} className="h-8 text-xs" title="卡片顺序">
              {ORDERS.map((o) => (
                <option key={o.value} value={o.value}>
                  {o.label}
                </option>
              ))}
            </Select>
          )}
          <span className="flex h-8 items-center rounded-md border border-input p-0.5">
            {(['cards', 'table'] as const).map((v) => (
              <button
                key={v}
                type="button"
                onClick={() => set({ view: v === 'cards' ? null : v })}
                className={cn('flex h-full items-center gap-1 rounded-sm px-2 text-xs text-muted-fg hover:text-fg', view === v && 'bg-accent-soft text-accent')}
                title={v === 'cards' ? '卡片' : '表格'}
              >
                {v === 'cards' ? <LayoutGridIcon className="size-3.5" /> : <TableIcon className="size-3.5" />}
              </button>
            ))}
          </span>
          <StatsLine stats={q.data?.stats} className="hidden text-2xs text-muted-fg 2xl:inline" />
        </span>
      </header>

      <div className="min-h-0 flex-1 overflow-auto p-3 md:p-4">
        {q.isError && <ErrorBox error={q.error} onRetry={() => q.refetch()} />}
        {q.isPending && (
          <div className="flex justify-center py-16">
            <Spinner />
          </div>
        )}
        {q.data && !all.length && <EmptyState title="这个时间范围内没有入口 span" hint="tracepipe 是不是还没接上？或者试试放宽时间范围。" />}
        {q.data && all.length > 0 && (
          <>
            <Summary data={q.data} all={all} restarts={events.data?.events.filter((e) => e.kind === 'restart').length ?? 0} cmpShort={cmpShort} />
            {!shown.length && <EmptyState title={onlyBad ? '没有异常的服务' : '没有匹配的服务'} hint={onlyBad ? '错误率都在 1% 以下，P95 也没比对比窗口明显变差。' : undefined} />}

            {view === 'cards' && bad.length > 0 && (
              <section className="mb-5">
                <h2 className="mb-2 flex items-baseline gap-2 text-sm font-semibold">
                  需要看一眼
                  <span className="text-2xs font-normal text-muted-fg">
                    {bad.length} 个 · 错误率 ≥ 1%，或 P95 比{cmpShort}高 1.5 倍以上（两边都至少 300 次请求才比）
                  </span>
                </h2>
                <div className={cn('grid gap-3 sm:grid-cols-2 xl:grid-cols-3', q.isFetching && 'opacity-70')}>
                  {bad.map(({ s, health }) => (
                    <BigCard key={s.service} s={s} health={health} win={win} prev={prevWin} events={eventsByService.get(s.service) ?? []} logDim={logDim} hasMetrics={hasMetrics} />
                  ))}
                </div>
              </section>
            )}

            {view === 'cards' && fine.length > 0 && (
              <section>
                {bad.length > 0 && (
                  <h2 className="mb-2 flex items-baseline gap-2 text-sm font-semibold">
                    正常 <span className="text-2xs font-normal text-muted-fg">{fine.length} 个</span>
                  </h2>
                )}
                <Card className={cn('overflow-hidden', q.isFetching && 'opacity-70')}>
                  {!isMobile && (
                    <div className="grid grid-cols-[minmax(0,2fr)_1fr_1fr_1fr_7rem_5rem] items-center gap-x-3 border-b border-border bg-muted/40 px-3 py-1.5 text-2xs text-muted-fg">
                      <span>服务</span>
                      <span>请求量</span>
                      <span>错误率</span>
                      <span>P95</span>
                      <span>趋势</span>
                      <span />
                    </div>
                  )}
                  {fine.map(({ s, health }) => (
                    <Row key={s.service} s={s} health={health} win={win} events={eventsByService.get(s.service) ?? []} logDim={logDim} hasMetrics={hasMetrics} mobile={isMobile} />
                  ))}
                </Card>
              </section>
            )}

            {view === 'table' && tableRows.length > 0 && (
              <Card className="overflow-hidden">
                <table className={cn('w-full table-fixed border-collapse text-xs', q.isFetching && 'opacity-70')}>
                  <thead className="text-2xs text-muted-fg">
                    <tr className="border-b border-border bg-muted/40">
                      {COLUMNS.map((c) => (
                        <th
                          key={c.key}
                          title={c.title}
                          className={cn('cursor-pointer px-4 py-2.5 font-medium select-none hover:text-fg', c.align === 'right' ? 'w-24 text-right' : 'text-left', tableSort.key === c.key && 'text-accent')}
                          onClick={() => setTableSort((t) => ({ key: c.key, desc: t.key === c.key ? !t.desc : c.key !== 'service' }))}
                        >
                          {c.label}
                          {tableSort.key === c.key && (tableSort.desc ? ' ▾' : ' ▴')}
                        </th>
                      ))}
                    </tr>
                  </thead>
                  <tbody>
                    {tableRows.map((s) => (
                      <tr key={s.service} className="row-hover cursor-pointer border-b border-border/60 last:border-b-0" onClick={() => navigate(serviceHref(s.service, win))}>
                        <td className="truncate px-4 py-2.5 font-medium" title={s.service}>
                          <span className="mr-2 inline-block align-middle">
                            <HealthDot {...serviceHealth(s)} />
                          </span>
                          {s.service || '(空)'}
                        </td>
                        <td className="px-4 py-2.5 text-right tabular-nums">{formatNumber(s.requests)}</td>
                        <td className="px-4 py-2.5 text-right text-muted-fg tabular-nums">{s.rps < 10 ? s.rps.toFixed(2) : Math.round(s.rps)}</td>
                        <td className="px-4 py-2.5 text-right tabular-nums">{s.errors ? formatNumber(s.errors) : <span className="text-muted-fg">0</span>}</td>
                        <td className="px-4 py-2.5 text-right">
                          <ErrorRate rate={s.error_rate} />
                        </td>
                        <td className="px-4 py-2.5 text-right tabular-nums">{formatDurationMs(s.p50_ms)}</td>
                        <td className="px-4 py-2.5 text-right tabular-nums">{formatDurationMs(s.p95_ms)}</td>
                        <td className="px-4 py-2.5 text-right font-medium tabular-nums">{formatDurationMs(s.p99_ms)}</td>
                        <td className="px-4 py-2.5 text-right text-muted-fg tabular-nums">{formatDurationMs(s.max_ms)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </Card>
            )}
          </>
        )}
      </div>
    </div>
  )
}

/** 顶上一行全站数字：先总后分（CF 每个 Overview 都这样） */
function Summary({ data, all, restarts, cmpShort }: { data: OverviewResponse; all: { s: ServiceStat; health: { level: Health } }[]; restarts: number; cmpShort: string }) {
  const secs = Math.max(1, (data.to_ms - data.from_ms) / 1000)
  const prevSecs = Math.max(1, (data.prev_to_ms - data.prev_from_ms) / 1000)
  let req = 0
  let err = 0
  let preq = 0
  let perr = 0
  let hasPrev = false
  const n = data.services[0]?.spark.requests.length ?? 0
  const spark = new Array<number>(n).fill(0)
  const sparkErr = new Array<number>(n).fill(0)
  const sparkPrev = new Array<number>(n).fill(0)
  for (const { s } of all) {
    req += s.requests
    err += s.errors
    if (s.prev) {
      hasPrev = true
      preq += s.prev.requests
      perr += s.prev.errors
    }
    s.spark.requests.forEach((v, i) => (spark[i] += v))
    s.spark.errors.forEach((v, i) => (sparkErr[i] += v))
    s.spark.prev_requests.forEach((v, i) => (sparkPrev[i] += v))
  }
  const rate = req ? err / req : 0
  const prate = preq ? perr / preq : 0
  const bad = all.filter((x) => x.health.level !== 'ok').length
  return (
    <div className="mb-4 grid grid-cols-2 gap-3 lg:grid-cols-[repeat(4,minmax(0,1fr))_minmax(0,2fr)]">
      <div className="rounded-lg border border-border bg-card px-3.5 py-2.5">
        <Stat label={`全站请求量 · 比${cmpShort}`} value={fmtRps(req / secs)} delta={hasPrev ? change(req / secs, preq / prevSecs) : null} upIs="neutral" big />
      </div>
      <div className="rounded-lg border border-border bg-card px-3.5 py-2.5">
        <Stat label="全站错误率" value={pct(rate)} delta={hasPrev ? change(rate, prate) : null} upIs="bad" big />
      </div>
      <div className={cn('rounded-lg border bg-card px-3.5 py-2.5', bad ? 'border-warn/50' : 'border-border')}>
        <div className="text-2xs text-muted-fg">异常服务</div>
        <div className={cn('text-xl font-semibold tabular-nums', bad && 'text-warn')}>
          {bad} <span className="text-2xs font-normal text-muted-fg">/ {all.length}</span>
        </div>
      </div>
      <div className={cn('rounded-lg border bg-card px-3.5 py-2.5', restarts ? 'border-warn/50' : 'border-border')}>
        <div className="text-2xs text-muted-fg">进程重启</div>
        <div className={cn('text-xl font-semibold tabular-nums', restarts && 'text-warn')}>{restarts}</div>
      </div>
      <div className="col-span-2 rounded-lg border border-border bg-card px-3.5 py-2.5 lg:col-span-1">
        <div className="mb-1 flex items-center gap-3 text-2xs text-muted-fg">
          <span>全站请求趋势</span>
          <span className="inline-flex items-center gap-1">
            <span className="inline-block h-2 w-2 rounded-sm" style={{ background: 'var(--chart-1)' }} />
            现在
          </span>
          <span className="inline-flex items-center gap-1">
            <span className="inline-block h-2 w-2 rounded-sm" style={{ background: 'var(--muted-fg)', opacity: 0.3 }} />
            {cmpShort}
          </span>
        </div>
        <Sparkline requests={spark} errors={sparkErr} prev={sparkPrev} height={34} />
      </div>
    </div>
  )
}

/** 异常服务的大卡：数字 + 主因 + 重启 + 趋势 */
function BigCard({ s, health, win, prev, events, logDim, hasMetrics }: { s: ServiceStat; health: { level: Health; reasons: string[] }; win: Window; prev: Window; events: MetricEvent[]; logDim: string; hasMetrics: boolean }) {
  const latencyOk = meaningfulLatency(s.p95_ms)
  return (
    <Link to={serviceHref(s.service, win)} className={cn('group row-hover block rounded-lg border bg-card p-3.5', health.level === 'bad' ? 'border-danger/50' : 'border-warn/50')}>
      <div className="flex items-center gap-2">
        <HealthDot level={health.level} reasons={health.reasons} />
        <span className="min-w-0 flex-1 truncate text-sm font-semibold" title={s.service}>
          {s.service || '(空)'}
        </span>
        <Restarts events={events} />
        <QuickLinks service={s.service} win={win} logDim={logDim} hasMetrics={hasMetrics} className="opacity-0 group-hover:opacity-100" />
        <span className="text-2xs text-muted-fg tabular-nums">{formatNumber(s.requests)} 次</span>
      </div>
      <div className={cn('mt-1 truncate text-2xs', health.level === 'bad' ? 'text-danger' : 'text-warn')} title={health.reasons.join('；')}>
        {health.reasons.join('；')}
      </div>
      {health.reasons.some((r) => r.startsWith('P95')) && <Contributor service={s.service} win={win} prev={prev} />}
      <div className="mt-3 grid grid-cols-3 gap-2">
        <Stat label="请求量" value={fmtRps(s.rps)} delta={change(s.rps, s.prev?.rps)} upIs="neutral" />
        <Stat label="错误率" value={pct(s.error_rate)} delta={change(s.error_rate, s.prev?.error_rate)} upIs="bad" />
        <Stat label="P95" value={latencyOk ? formatDurationMs(s.p95_ms) : '—'} delta={latencyOk ? change(s.p95_ms, s.prev?.p95_ms) : null} upIs="bad" muted={!latencyOk} />
      </div>
      <Sparkline requests={s.spark.requests} errors={s.spark.errors} prev={s.spark.prev_requests} className="mt-3" />
    </Link>
  )
}

/** 正常服务压成一行 */
function Row({ s, health, win, events, logDim, hasMetrics, mobile }: { s: ServiceStat; health: { level: Health; reasons: string[] }; win: Window; events: MetricEvent[]; logDim: string; hasMetrics: boolean; mobile: boolean }) {
  const latencyOk = meaningfulLatency(s.p95_ms)
  if (mobile) {
    return (
      <Link to={serviceHref(s.service, win)} className="row-hover flex items-center gap-2 border-b border-border/60 px-3 py-2 text-xs last:border-b-0">
        <HealthDot {...health} />
        <span className="min-w-0 flex-1 truncate font-medium">{s.service || '(空)'}</span>
        <Restarts events={events} />
        <span className="tabular-nums">{fmtRps(s.rps)}</span>
        <span className="tabular-nums text-muted-fg">{latencyOk ? formatDurationMs(s.p95_ms) : '—'}</span>
      </Link>
    )
  }
  return (
    <Link to={serviceHref(s.service, win)} className="group row-hover grid grid-cols-[minmax(0,2fr)_1fr_1fr_1fr_7rem_5rem] items-center gap-x-3 border-b border-border/60 px-3 py-1.5 text-xs last:border-b-0">
      <span className="flex min-w-0 items-center gap-2">
        <HealthDot {...health} />
        <span className="truncate font-medium" title={s.service}>
          {s.service || '(空)'}
        </span>
        <Restarts events={events} />
      </span>
      <span className="flex items-baseline gap-1.5 tabular-nums">
        {fmtRps(s.rps)} <Delta delta={change(s.rps, s.prev?.rps)} upIs="neutral" />
      </span>
      <span className="flex items-baseline gap-1.5 tabular-nums">
        {pct(s.error_rate)} <Delta delta={change(s.error_rate, s.prev?.error_rate)} upIs="bad" />
      </span>
      <span className={cn('flex items-baseline gap-1.5 tabular-nums', !latencyOk && 'text-muted-fg')} title={latencyOk ? undefined : '入口 span 几乎不耗时（消费确认类），延迟没意义'}>
        {latencyOk ? formatDurationMs(s.p95_ms) : '—'}
        {latencyOk && <Delta delta={change(s.p95_ms, s.prev?.p95_ms)} upIs="bad" />}
      </span>
      <Sparkline requests={s.spark.requests} errors={s.spark.errors} prev={s.spark.prev_requests} height={20} />
      <QuickLinks service={s.service} win={win} logDim={logDim} hasMetrics={hasMetrics} className="justify-end opacity-0 group-hover:opacity-100" />
    </Link>
  )
}
