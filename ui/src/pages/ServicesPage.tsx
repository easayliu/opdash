import { memo, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { useVirtualizer } from '@tanstack/react-virtual'
import { Link, useNavigate } from 'react-router'
import { AlertTriangleIcon, ChartLineIcon, GitBranchIcon, LayoutGridIcon, RotateCwIcon, ScrollTextIcon, SearchIcon, TableIcon } from 'lucide-react'
import { useErrorGroups, useMeta, useMetricEvents, useServiceOperations, useServices } from '@/api/queries'
import type { ErrorGroup, MetricEvent, OperationStat, OverviewResponse, ServiceStat } from '@/api/types'
import { Sparkline } from '@/components/charts/Sparkline'
import { StatsLine } from '@/components/StatsLine'
import { Button, Card, EmptyState, ErrorBox, Input, Select, Spinner } from '@/components/ui'
import { COMPARE, DEFAULT_COMPARE, compareShort, parseCompare, topMovers, type Compare } from '@/lib/compare'
import { change, changeTone, formatChange, healthRank, meaningfulLatency, pct, serviceHealth, type Health } from '@/lib/health'
import { errorTitle, errorTitleFull } from '@/lib/errors'
import { errorsHref, logsHref, metricsHref, serviceHref, tracesHref, type Window } from '@/lib/links'
import { formatDurationMs, formatNumber, formatTs } from '@/lib/time'
import { useTimeRange, useUrlState } from '@/lib/url-state'
import { useIsMobile } from '@/lib/media'
import { cn, scrollParent } from '@/lib/utils'

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

/** 卡片排序 */
/** 一次向后端问几个服务的接口表，和后端的 MAX_SERVICES_PER_QUERY 对齐。异常服务比这还多的话，
 *  排在后面的卡就不显示「主要是哪个接口」了——那种时候整个集群都在烧，这一行不是重点 */
const MAX_CONTRIBUTORS = 24

/** 稳定的空数组：每次 render 新建 `[]` 会让下面那些 memo 组件全部白跑 */
const NO_EVENTS: MetricEvent[] = []
const NO_OPS: OperationStat[] = []

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
export function Delta({ delta, upIs, className }: { delta: number | null; upIs: 'bad' | 'neutral'; className?: string }) {
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
 * 异常服务卡上多的一行：**是哪个接口的事**。只对被判成异常的服务查（一般就几个），接口表
 * 一次返回两个窗口，挑法和详情页的变化榜共用（见 lib/compare.ts）：错误爆了就说错误，慢了
 * 就说慢了。一个接口都没越过阈值（劣化摊薄在几百个接口上）才退回去说当前 P95 最高的那个。
 */
/**
 * 异常卡上那句「主要是哪个接口」。
 *
 * 数据由 [`ServicesPage`] 一条查询问回来再按服务分（见 `useServiceOperations`），这里只负责
 * 挑一行显示：以前是每张卡自己 `useOperations`，十几个服务同时报警就是十几条查询。
 */
const Contributor = memo(function Contributor({ ops }: { ops: OperationStat[] }) {
  const line = useMemo(() => {
    const mover = topMovers(ops, 1)[0]
    if (mover) return { name: mover.op.span_name, text: mover.detail, requests: mover.op.requests }
    const top = ops.filter((o) => o.requests >= 30).sort((a, b) => b.p95_ms - a.p95_ms)[0]
    if (!top) return null
    const from = top.prev ? `${formatDurationMs(top.prev.p95_ms)} → ` : ''
    return { name: top.span_name, text: `P95 ${from}${formatDurationMs(top.p95_ms)}`, requests: top.requests }
  }, [ops])
  if (!line) return null
  return (
    <div className="mt-1 truncate text-2xs text-muted-fg" title={`${line.name}（${formatNumber(line.requests)} 次）`}>
      主要是 <span className="mono text-fg">{line.name}</span>：<span className="font-medium text-fg">{line.text}</span>
    </div>
  )
})

/**
 * 异常卡上的「主要在报什么错」。**这一行是 dash 到报错之间唯一的一跳**：以前得点进服务详情、
 * 再点「出错的链路」、再在瀑布图里找红条，才能知道 6% 的错误率背后是哪一句异常。
 *
 * 点它直接进错误分组页并展开这一组（`errorsHref` 的 `group`），不经过服务详情。
 */
function TopError({ g, win }: { g: ErrorGroup; win: Window }) {
  const stop = (e: React.MouseEvent) => e.stopPropagation()
  return (
    <Link
      to={errorsHref({ service: g.service, group: g.id }, win)}
      onClick={stop}
      className="mt-1 flex min-w-0 items-baseline gap-1 text-2xs text-muted-fg hover:text-fg"
      title={`${errorTitleFull(g)}${g.message ? `: ${g.message}` : ''}（${formatNumber(g.count)} 次）`}
    >
      <AlertTriangleIcon className="size-3 shrink-0 translate-y-0.5 text-danger" />
      <span className="min-w-0 truncate">
        <span className="mono font-medium text-fg">{errorTitle(g)}</span>
        {g.message && <span className="text-fg/80">: {g.message}</span>}
      </span>
      <span className="shrink-0 tabular-nums">{formatNumber(g.count)} 次</span>
    </Link>
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
/**
 * 正常服务那张长列表：只渲染视口里的那十几行。
 *
 * 线上 83 个服务，全渲染出来是 83 行 × 每行一张趋势图；再加上下面的排序、筛选，任何一次
 * 重渲染都得把它们走一遍。日志表和瀑布图早就是虚拟化的，这里照搬同一套（`useVirtualizer`
 * + `scrollParent`）：滚动容器是外层那个 `overflow-auto`，列表前面还有概览卡和异常卡，
 * 所以要量一下自己在容器里的起点（`scrollMargin`）。
 *
 * 行高固定（一行文字），但还是挂上 `measureElement` 兜底——字号或间距一改，估算值就不准了。
 */
function FineList({
  items,
  win,
  compare,
  eventsByService,
  logDim,
  hasMetrics,
  mobile,
}: {
  items: { s: ServiceStat; health: { level: Health; reasons: string[] } }[]
  win: Window
  compare: Compare
  eventsByService: Map<string, MetricEvent[]>
  logDim: string
  hasMetrics: boolean
  mobile: boolean
}) {
  const hostRef = useRef<HTMLDivElement>(null)
  const scrollEl = useRef<HTMLElement | null>(null)
  const [margin, setMargin] = useState(0)
  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => (scrollEl.current ??= scrollParent(hostRef.current)),
    estimateSize: () => (mobile ? 33 : 29),
    overscan: 8,
    scrollMargin: margin,
    getItemKey: (i) => items[i].s.service,
  })
  useLayoutEffect(() => {
    const host = hostRef.current
    const box = scrollEl.current ?? scrollParent(host)
    if (!host || !box) return
    scrollEl.current = box
    const m = Math.max(0, Math.round(host.getBoundingClientRect().top - box.getBoundingClientRect().top + box.scrollTop))
    setMargin((prev) => (prev === m ? prev : m))
  })
  return (
    <div ref={hostRef} className="relative" style={{ height: virtualizer.getTotalSize() }}>
      {virtualizer.getVirtualItems().map((v) => {
        const { s, health } = items[v.index]
        return (
          <div
            key={v.key}
            data-index={v.index}
            ref={virtualizer.measureElement}
            className="absolute top-0 left-0 w-full"
            style={{ transform: `translateY(${v.start - margin}px)` }}
          >
            <Row
              s={s}
              health={health}
              win={win}
              compare={compare}
              events={eventsByService.get(s.service) ?? NO_EVENTS}
              logDim={logDim}
              hasMetrics={hasMetrics}
              mobile={mobile}
              last={v.index === items.length - 1}
            />
          </div>
        )
      })}
    </div>
  )
}

export function ServicesPage() {
  const { range } = useTimeRange()
  const { params, set } = useUrlState()
  const meta = useMeta()
  const navigate = useNavigate()
  const isMobile = useIsMobile()
  const compare = parseCompare(params.get('cmp'))
  const order = (ORDERS.find((o) => o.value === params.get('sort'))?.value ?? 'health') as Order
  // 表格 9 列手机塞不下，窄屏一律卡片
  const view = !isMobile && params.get('view') === 'table' ? 'table' : 'cards'
  const onlyBad = params.get('bad') === '1'
  const [needle, setNeedle] = useState('')
  const [tableSort, setTableSort] = useState<{ key: SortKey; desc: boolean }>({ key: 'requests', desc: true })
  // 引用要稳：win 传给下面每一张 memo 过的卡，每次 render 新建对象的话 memo 就白加了
  const win: Window = useMemo(() => ({ fromMs: range.fromMs, toMs: range.toMs }), [range.fromMs, range.toMs])
  const logDim = meta.data?.logs.dimensions.includes('service_name') ? 'service_name' : 'container'
  const hasMetrics = !!meta.data?.metrics

  const q = useServices({ from: range.fromMs, to: range.toMs, compare })
  // 全站的重启 / 新 pod，一次查完按服务分
  const events = useMetricEvents({ from: range.fromMs, to: range.toMs, metric: 'jvm.cpu.time' }, hasMetrics)
  const eventsByService = useMemo(() => {
    const m = new Map<string, MetricEvent[]>()
    for (const e of events.data?.events ?? []) m.set(e.service, [...(m.get(e.service) ?? []), e])
    return m
  }, [events.data])

  // 全站错误分组一次查完按服务分：异常卡上要写出「主要在报什么错」，一张卡各查一次的话
  // 十几张卡就是十几条查询，而这一条线上是 172 ms。返回已按次数降序，每个服务第一条就是它的头号报错
  const errs = useErrorGroups({ from: range.fromMs, to: range.toMs })
  const topErrorByService = useMemo(() => {
    const m = new Map<string, ErrorGroup>()
    for (const g of errs.data?.groups ?? []) if (!m.has(g.service)) m.set(g.service, g)
    return m
  }, [errs.data])

  const all = useMemo(() => (q.data?.services ?? []).map((s) => ({ s, health: serviceHealth(s) })), [q.data])

  // 异常卡上的「主要是哪个接口」：所有异常服务一条查询问完再按服务分，和上面的错误分组、
  // 重启事件一个路子。按 all 算而不是按过滤后的 bad，这样在搜索框里打字不会重新发查询
  const contributorNames = useMemo(
    () => all.filter((x) => x.health.level !== 'ok').map((x) => x.s.service).slice(0, MAX_CONTRIBUTORS),
    [all],
  )
  const contributors = useServiceOperations(
    contributorNames,
    { from: range.fromMs, to: range.toMs, kind: 'entry', compare },
    contributorNames.length > 0,
  )
  const opsByService = useMemo(() => {
    const m = new Map<string, OperationStat[]>()
    for (const o of contributors.data?.operations ?? []) {
      const list = m.get(o.service)
      if (list) list.push(o)
      else m.set(o.service, [o])
    }
    return m
  }, [contributors.data])
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
  const cmpShort = compareShort(compare)

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-border bg-card px-3 py-2.5 md:px-4 md:py-3">
        <h1 className="text-base font-semibold">服务</h1>
        <span className="hidden text-xs text-muted-fg xl:inline">按入口 span（Server / Consumer）算</span>
        {q.isFetching && <Spinner className="size-4" />}
        <span className="ml-auto flex flex-wrap items-center gap-2">
          <StatsLine stats={q.data?.stats} className="hidden text-2xs text-muted-fg 2xl:inline" />
          <Button size="sm" active={onlyBad} onClick={() => set({ bad: onlyBad ? null : '1' })} disabled={!badCount && !onlyBad} title="只看错误率或延迟异常的">
            <AlertTriangleIcon className="size-3.5" />
            异常 {badCount}
          </Button>
          <span className="relative">
            <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-fg" />
            <Input value={needle} onChange={(e) => setNeedle(e.target.value)} placeholder="搜服务" className="h-8 w-36 pl-8 text-xs" aria-label="搜服务" />
          </span>
          <Select value={compare} onChange={(e) => set({ cmp: e.target.value === DEFAULT_COMPARE ? null : e.target.value })} className="h-8 text-xs" title="所有变化和哪一段时间比">
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
          <span className="hidden h-8 items-center rounded-md border border-input p-0.5 md:flex">
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
                    {bad.length} 个<span className="hidden sm:inline"> · 错误率 ≥ 1%，或 P95 比{cmpShort}高 1.5 倍以上（两边都至少 300 次请求才比）</span>
                  </span>
                </h2>
                <div className={cn('grid gap-3 sm:grid-cols-2 xl:grid-cols-3', q.isFetching && 'opacity-70')}>
                  {bad.map(({ s, health }) => (
                    <BigCard
                      key={s.service}
                      s={s}
                      health={health}
                      win={win}
                      compare={compare}
                      events={eventsByService.get(s.service) ?? NO_EVENTS}
                      topError={topErrorByService.get(s.service)}
                      ops={opsByService.get(s.service) ?? NO_OPS}
                      logDim={logDim}
                      hasMetrics={hasMetrics}
                    />
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
                  <FineList items={fine} win={win} compare={compare} eventsByService={eventsByService} logDim={logDim} hasMetrics={hasMetrics} mobile={isMobile} />
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
                      <tr key={s.service} className="row-hover cursor-pointer border-b border-border/60 last:border-b-0" onClick={() => navigate(serviceHref(s.service, win, compare))}>
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

/**
 * 异常服务的大卡：数字 + 主因 + 重启 + 趋势。
 *
 * `memo` 不是锦上添花：这一页有 80 多张卡 / 行，不包的话在搜索框里打一个字就要把它们连同
 * 里面的趋势图全部重新渲染一遍。前提是传进来的 props 引用稳定——`win` 在上面 useMemo 过，
 * 空数组用的是 NO_EVENTS / NO_OPS 这两个常量。
 */
const BigCard = memo(function BigCard({ s, health, win, compare, events, topError, ops, logDim, hasMetrics }: { s: ServiceStat; health: { level: Health; reasons: string[] }; win: Window; compare: Compare; events: MetricEvent[]; topError?: ErrorGroup; ops: OperationStat[]; logDim: string; hasMetrics: boolean }) {
  const latencyOk = meaningfulLatency(s.p95_ms)
  const isMobile = useIsMobile()
  return (
    <Link to={serviceHref(s.service, win, compare)} className={cn('group row-hover flex flex-col rounded-lg border bg-card p-3.5', health.level === 'bad' ? 'border-danger/50' : 'border-warn/50')}>
      <div className="flex items-center gap-2">
        <HealthDot level={health.level} reasons={health.reasons} />
        <span className="min-w-0 flex-1 truncate text-sm font-semibold" title={s.service}>
          {s.service || '(空)'}
        </span>
        <Restarts events={events} />
        {!isMobile && <QuickLinks service={s.service} win={win} logDim={logDim} hasMetrics={hasMetrics} className="opacity-0 group-hover:opacity-100" />}
        <span className="text-2xs text-muted-fg tabular-nums">{formatNumber(s.requests)} 次</span>
      </div>
      <div className={cn('mt-1 truncate text-2xs', health.level === 'bad' ? 'text-danger' : 'text-warn')} title={health.reasons.join('；')}>
        {health.reasons.join('；')}
      </div>
      <Contributor ops={ops} />
      {topError && <TopError g={topError} win={win} />}
      {/* 原因区可能是一行也可能两行（带主因），指标和火花图贴底对齐，同一行的卡片才对得齐 */}
      <div className="mt-auto grid grid-cols-3 gap-2 pt-3">
        <Stat label="请求量" value={fmtRps(s.rps)} delta={change(s.rps, s.prev?.rps)} upIs="neutral" />
        <Stat label="错误率" value={pct(s.error_rate)} delta={change(s.error_rate, s.prev?.error_rate)} upIs="bad" />
        <Stat label="P95" value={latencyOk ? formatDurationMs(s.p95_ms) : '—'} delta={latencyOk ? change(s.p95_ms, s.prev?.p95_ms) : null} upIs="bad" muted={!latencyOk} />
      </div>
      <Sparkline requests={s.spark.requests} errors={s.spark.errors} prev={s.spark.prev_requests} className="mt-3" />
    </Link>
  )
})

/** 正常服务压成一行。同样包 memo，理由见 BigCard */
const Row = memo(function Row({ s, health, win, compare, events, logDim, hasMetrics, mobile, last }: { s: ServiceStat; health: { level: Health; reasons: string[] }; win: Window; compare: Compare; events: MetricEvent[]; logDim: string; hasMetrics: boolean; mobile: boolean; last?: boolean }) {
  const latencyOk = meaningfulLatency(s.p95_ms)
  if (mobile) {
    return (
      <Link to={serviceHref(s.service, win, compare)} className={cn('row-hover flex items-center gap-2 border-b border-border/60 px-3 py-2 text-xs', last && 'border-b-0')}>
        <HealthDot {...health} />
        <span className="min-w-0 flex-1 truncate font-medium">{s.service || '(空)'}</span>
        <Restarts events={events} />
        <span className="tabular-nums">{fmtRps(s.rps)}</span>
        <span className="tabular-nums text-muted-fg">{latencyOk ? formatDurationMs(s.p95_ms) : '—'}</span>
      </Link>
    )
  }
  return (
    <Link to={serviceHref(s.service, win, compare)} className={cn('group row-hover grid grid-cols-[minmax(0,2fr)_1fr_1fr_1fr_7rem_5rem] items-center gap-x-3 border-b border-border/60 px-3 py-1.5 text-xs', last && 'border-b-0')}>
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
})
