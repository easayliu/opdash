import { useMemo, useState, type ReactNode } from 'react'
import { Link, useParams } from 'react-router'
import { useMeta, useOperations, useTimeseries } from '@/api/queries'
import type { OperationStat } from '@/api/types'
import { LineChart, Legend, type LineSeries } from '@/components/charts/LineChart'
import { StackedBars } from '@/components/charts/StackedBars'
import { StatsLine } from '@/components/StatsLine'
import { AnimatePresence } from 'motion/react'
import * as motion from 'motion/react-m'
import { Badge, Button, Card, EmptyState, ErrorBox, Hint, Select, Spinner, buttonClass } from '@/components/ui'
import { Delta, ErrorRate } from '@/pages/ServicesPage'
import { COMPARE, DEFAULT_COMPARE, compareShort, parseCompare, topMovers, type Mover } from '@/lib/compare'
import { FADE } from '@/lib/motion'
import { change, pct } from '@/lib/health'
import { errorsHref, logsHref, metricsHref, tracesHref } from '@/lib/links'
import { formatDurationMs, formatNumber } from '@/lib/time'
import { useTimeRange, useUrlState } from '@/lib/url-state'
import { useIsMobile } from '@/lib/media'
import { cn } from '@/lib/utils'
import { usePageTitle } from '@/lib/title'

// 三条线两两都要分得开（会交叉）：用参考配色前三档 aqua / blue / orange，全对校验通过
const LATENCY_SERIES: LineSeries[] = [
  { key: 'p50_ms', label: 'P50', color: 'var(--chart-3)' },
  { key: 'p95_ms', label: 'P95', color: 'var(--chart-1)' },
  { key: 'p99_ms', label: 'P99', color: 'var(--chart-2)' },
]
const TRAFFIC_SERIES = [
  { key: 'ok', label: '成功', color: 'var(--chart-1)' },
  { key: 'errors', label: '错误', color: 'var(--level-error)' },
]

/** 对比窗口里也有的列（`PrevOp` 上有的那几个）。`最大` 没取对比值，`接口名` 没法比 */
const COMPARABLE = ['requests', 'errors', 'error_rate', 'p50_ms', 'p95_ms', 'p99_ms'] as const
type ComparableCol = (typeof COMPARABLE)[number]
type OpSort = ComparableCol | 'span_name' | 'max_ms'

function isComparable(key: OpSort): key is ComparableCol {
  return (COMPARABLE as readonly string[]).includes(key)
}

interface OpColumn {
  key: OpSort
  label: string
  right?: boolean
  /** 涨了算坏事（错误、延迟）还是无所谓（请求量） */
  upIs?: 'bad' | 'neutral'
}

/** 一个接口在某一列上和对比窗口的变化。没得比（新接口、不可比的列）是 null */
function deltaOf(o: OperationStat, key: OpSort): number | null {
  if (!isComparable(key) || !o.prev) return null
  // 现在一次都没有的接口：只有「次数 −100%」是实话，延迟一列是「没有数据」不是「变成 0 了」
  if (o.requests === 0 && key !== 'requests' && key !== 'errors') return null
  return change(o[key], o.prev[key])
}

/** 排序用：没法比的排最后，`新增`（对比窗口是 0）排最前 */
function sortableDelta(o: OperationStat, key: OpSort): number {
  const d = deltaOf(o, key)
  if (d === null) return -Number.MAX_VALUE
  return d === Infinity ? Number.MAX_VALUE : d
}

export function ServiceDetailPage() {
  const { name = '' } = useParams<{ name: string }>()
  const service = decodeURIComponent(name)
  usePageTitle(service)
  const meta = useMeta()
  const { range } = useTimeRange()
  const { params, set } = useUrlState()
  const isMobile = useIsMobile()
  const kind = params.get('kind') === 'client' ? 'client' : 'entry'
  const op = params.get('op') ?? ''
  const compare = parseCompare(params.get('cmp'))
  const cmpShort = compareShort(compare)
  const rangeParams = { from: range.fromMs, to: range.toMs, compare }
  const ops = useOperations(service, { ...rangeParams, kind })
  const ts = useTimeseries(service, { ...rangeParams, span_name: op || undefined })
  const [sort, setSort] = useState<{ key: OpSort; desc: boolean }>({ key: 'requests', desc: true })
  // 按「值」排还是按「和对比窗口的变化」排。找退化的接口时，「P95 最高的」和「P95 涨得最多的」
  // 常常不是同一批：前者是本来就慢的那几个，后者才是今天新坏的
  const [byDelta, setByDelta] = useState(false)
  const [allMovers, setAllMovers] = useState(false)

  const rows = useMemo(() => {
    const list = [...(ops.data?.operations ?? [])]
    list.sort((a, b) => {
      let c: number
      if (sort.key === 'span_name') c = a.span_name.localeCompare(b.span_name)
      else if (byDelta) c = sortableDelta(a, sort.key) - sortableDelta(b, sort.key)
      else c = Number(a[sort.key]) - Number(b[sort.key])
      return sort.desc ? -c : c
    })
    return list
  }, [ops.data, sort, byDelta])
  // 变化榜按「性质 + 影响面」排，和表的排序无关，见 lib/compare.ts
  const movers = useMemo(() => topMovers(ops.data?.operations ?? [], 50), [ops.data])
  const totals = useMemo(() => summarize(ops.data?.operations ?? []), [ops.data])

  const logDim = meta.data?.logs.dimensions.includes('service_name') ? 'service_name' : 'container'
  // 跳走时带上当前这段时间，几个页面看的是同一个窗口
  const win = { fromMs: range.fromMs, toMs: range.toMs }
  const tsPoints = ts.data?.points ?? []
  // 对比窗口那段时间可能根本没有数据（服务是今天才上的）：没有就不画那条线，图例上也别留一条空的
  const hasPrev = tsPoints.some((p) => p.prev && p.prev.requests > 0)
  // 没有请求的桶不画（断线），画成 0 会把延迟曲线拉到地板上
  const points = tsPoints.map((p) => {
    const values: Record<string, number> = p.requests > 0 ? { p50_ms: p.p50_ms, p95_ms: p.p95_ms, p99_ms: p.p99_ms } : {}
    if (p.prev && p.prev.requests > 0) values.prev_p95_ms = p.prev.p95_ms
    return { t_ms: p.t_ms, values }
  })
  const traffic = tsPoints.map((p) => ({
    t_ms: p.t_ms,
    values: { ok: p.requests - p.errors, errors: p.errors, prev_requests: p.prev?.requests ?? 0 },
  }))
  const latencySeries: LineSeries[] = hasPrev
    ? [...LATENCY_SERIES, { key: 'prev_p95_ms', label: `P95 · ${cmpShort}`, color: 'var(--muted-fg)', dashed: true }]
    : LATENCY_SERIES

  const cols: OpColumn[] = [
    { key: 'span_name', label: kind === 'entry' ? '接口 / 操作' : '下游调用' },
    { key: 'requests', label: '次数', right: true, upIs: 'neutral' },
    { key: 'errors', label: '错误', right: true, upIs: 'bad' },
    { key: 'error_rate', label: '错误率', right: true, upIs: 'bad' },
    { key: 'p50_ms', label: 'P50', right: true, upIs: 'bad' },
    { key: 'p95_ms', label: 'P95', right: true, upIs: 'bad' },
    { key: 'p99_ms', label: 'P99', right: true, upIs: 'bad' },
    { key: 'max_ms', label: '最大', right: true },
  ]
  const select = (name: string) => set({ op: op === name ? null : name })

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-border bg-card px-3 py-2.5 md:px-4 md:py-3">
        <Link to="/services" className="text-sm text-muted-fg hover:text-fg">
          ← 服务
        </Link>
        <h1 className="min-w-0 truncate text-base font-semibold">{service}</h1>
        {op && (
          <span className="inline-flex min-w-0 max-w-full items-center gap-1.5 rounded-md bg-accent-soft px-2.5 py-1 text-xs text-accent">
            <span className="truncate">{op}</span>
            <Hint text="看整个服务" asChild>
              <button type="button" onClick={() => set({ op: null })}>
                ✕
              </button>
            </Hint>
          </span>
        )}
        <span className="flex w-full flex-wrap items-center gap-2 md:ml-auto md:w-auto">
          <Hint text="图和表上的变化都和这一段比" asChild>
            <Select
              value={compare}
              onChange={(e) => set({ cmp: e.target.value === DEFAULT_COMPARE ? null : e.target.value })}
              className="h-8 text-xs"
            >
              {COMPARE.map((c) => (
                <option key={c.value} value={c.value}>
                  {c.label}
                </option>
              ))}
            </Select>
          </Hint>
          <Link to={tracesHref({ service, spanName: op || undefined, kinds: 'Server,Consumer', sort: 'duration' }, win)} className={buttonClass({ size: 'sm' })}>
            最慢的链路
          </Link>
          {/* 「错误分组」在「出错的链路」前面：先问是什么错，再决定要不要一条条看链路 */}
          <Link to={errorsHref({ service, spanName: op || undefined }, win)} className={buttonClass({ size: 'sm' })}>
            错误分组
          </Link>
          <Link to={tracesHref({ service, spanName: op || undefined, errorOnly: true }, win)} className={buttonClass({ size: 'sm' })}>
            出错的链路
          </Link>
          <Link to={logsHref({ dim: logDim, service, levels: 'ERROR,WARN' }, win)} className={buttonClass({ size: 'sm' })}>
            错误日志
          </Link>
          {/* 指标表可能没有（没部署 metricpipe），有才给入口 */}
          {meta.data?.metrics && (
            <Link to={metricsHref(service, win)} className={buttonClass({ size: 'sm' })}>
              指标看板
            </Link>
          )}
        </span>
      </header>
      <div className="min-h-0 flex-1 overflow-auto p-3 md:p-4">
        <div className="grid gap-3 md:gap-4 lg:grid-cols-2">
          <Card title={`请求量与错误${op ? `：${op}` : ''}`} extra={<StatsLine stats={ts.data?.stats} />}>
            <div className="px-2 pt-3 pb-1 md:px-3">
              {ts.isError ? (
                <ErrorBox error={ts.error} />
              ) : (
                <>
                  <Legend series={TRAFFIC_SERIES} className="px-1" />
                  <StackedBars
                    label={`请求量与错误${op ? `：${op}` : ''}`}
                    fromMs={ts.data?.from_ms ?? range.fromMs}
                    toMs={ts.data?.to_ms ?? range.toMs}
                    widthMs={ts.data?.width_ms ?? 60_000}
                    buckets={traffic}
                    series={TRAFFIC_SERIES}
                    ghost={hasPrev ? { key: 'prev_requests', label: cmpShort } : undefined}
                    height={isMobile ? 150 : 190}
                    stale={ts.isFetching}
                  />
                </>
              )}
            </div>
          </Card>
          <Card title="延迟分位（毫秒）">
            <div className="px-2 pt-3 pb-1 md:px-3">
              {ts.isError ? (
                <ErrorBox error={ts.error} />
              ) : (
                <>
                  <Legend series={latencySeries} className="px-1" />
                  <LineChart
                    label={`延迟分位${op ? `：${op}` : ''}`}
                    fromMs={ts.data?.from_ms ?? range.fromMs}
                    toMs={ts.data?.to_ms ?? range.toMs}
                    widthMs={ts.data?.width_ms ?? 60_000}
                    points={points}
                    series={latencySeries}
                    height={isMobile ? 150 : 190}
                    stale={ts.isFetching}
                    format={(v) => formatDurationMs(v)}
                  />
                </>
              )}
            </div>
          </Card>
        </div>

        {/* 一个都没变也要把这张卡留着：「看过了，没有接口越过阈值」本身就是答案 */}
        {ops.data && rows.length > 0 && (
          <Card
            className="mt-3 overflow-hidden md:mt-4"
            title={
              <span className="flex items-baseline gap-2">
                和{cmpShort}比，变了的{kind === 'entry' ? '接口' : '下游调用'}
                {movers.length > 0 && <span className="text-2xs font-normal text-muted-fg">{movers.length} 个 · 按影响面排，不按百分比</span>}
              </span>
            }
            extra={
              movers.length > 6 && (
                <Button size="xs" variant="ghost" onClick={() => setAllMovers((v) => !v)}>
                  {allMovers ? '只看前 6 个' : `全部 ${movers.length} 个`}
                </Button>
              )
            }
          >
            {movers.length === 0 ? (
              <div className="px-3 py-3 text-2xs text-muted-fg">
                没有接口明显变化：错误没多起来，P95 也没涨（两边各满 100 次请求才比，少于这个数的几条慢请求就是噪声）。
              </div>
            ) : (
              <ul>
                {/* 「全部 N 个 / 只看前 6 个」一按，多出来的行是淡入的；换对比窗口重算变化榜同理 */}
                <AnimatePresence initial={false}>
                  {(allMovers ? movers : movers.slice(0, 6)).map((m) => (
                    <motion.li key={`${m.op.kind}:${m.op.span_name}`} layout="position" {...FADE}>
                      <MoverRow m={m} selected={op === m.op.span_name} onSelect={() => select(m.op.span_name)} />
                    </motion.li>
                  ))}
                </AnimatePresence>
              </ul>
            )}
          </Card>
        )}

        <Card
          className="mt-3 overflow-hidden md:mt-4"
          title={
            <span className="flex items-center gap-1">
              {(['entry', 'client'] as const).map((k) => (
                <button key={k} type="button" onClick={() => set({ kind: k === 'entry' ? null : k, op: null })} className={cn('rounded-sm px-2.5 py-1', kind === k ? 'bg-accent-soft text-accent' : 'text-muted-fg hover:text-fg')}>
                  {k === 'entry' ? '入口接口' : '下游调用'}
                </button>
              ))}
              {ops.isFetching && <Spinner className="size-3.5" />}
            </span>
          }
          extra={
            <>
              {ops.data?.truncated && (
                <Hint text="这个服务的接口太多（常见于把 SQL / id 拼进了 span 名），只统计了量最大的那些">
                  <Badge tone="warn">接口已截断</Badge>
                </Hint>
              )}
              <StatsLine stats={ops.data?.stats} className="hidden text-2xs text-muted-fg md:inline" />
              {!isMobile && (
                <span className="flex h-7 items-center rounded-md border border-input p-0.5">
                  {(['value', 'delta'] as const).map((m) => (
                    <button
                      key={m}
                      type="button"
                      onClick={() => setByDelta(m === 'delta')}
                      className={cn('h-full rounded-sm px-2 text-2xs text-muted-fg hover:text-fg', byDelta === (m === 'delta') && 'bg-accent-soft text-accent')}
                    >
                      {m === 'value' ? '按值排' : '按变化排'}
                    </button>
                  ))}
                </span>
              )}
            </>
          }
        >
          {totals && (
            <div className="flex flex-wrap items-baseline gap-x-4 gap-y-1 border-b border-border bg-muted/30 px-4 py-2 text-2xs text-muted-fg">
              <span>
                合计 <span className="font-medium text-fg tabular-nums">{formatNumber(totals.requests)}</span> 次{' '}
                <Delta delta={change(totals.requests, totals.comparable ? totals.prevRequests : null)} upIs="neutral" />
              </span>
              <span>
                错误率 <span className="font-medium text-fg tabular-nums">{pct(totals.rate)}</span>{' '}
                <Delta delta={change(totals.rate, totals.comparable ? totals.prevRate : null)} upIs="bad" />
              </span>
              <span className="tabular-nums">
                {totals.live} 个{kind === 'entry' ? '接口' : '下游'}
              </span>
              <span className="hidden sm:inline">和{cmpShort}比</span>
            </div>
          )}
          {ops.isError && <ErrorBox error={ops.error} onRetry={() => ops.refetch()} />}
          {ops.isPending && (
            <div className="flex justify-center py-10">
              <Spinner />
            </div>
          )}
          {ops.data && !rows.length && <EmptyState title={kind === 'entry' ? '没有入口 span' : '没有对外调用的 span'} />}
          {rows.length > 0 && isMobile && (
            <ul className="text-xs">
              {rows.map((o) => (
                // 整行可点是给手指的；键盘走接口名那个按钮
                // eslint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/no-noninteractive-element-interactions
                <li
                  key={`${o.kind}:${o.span_name}`}
                  className={cn('row-hover cursor-pointer border-b border-border/60 px-3 py-2.5 last:border-b-0', op === o.span_name && 'row-selected')}
                  onClick={() => select(o.span_name)}
                >
                  <div className="flex items-center gap-2">
                    <Hint text={`只看这个接口的趋势：${o.span_name}`} asChild>
                      <button
                        type="button"
                        aria-pressed={op === o.span_name}
                        onClick={(e) => {
                          e.stopPropagation()
                          select(o.span_name)
                        }}
                        className="min-w-0 flex-1 cursor-pointer truncate rounded-sm text-left font-medium focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand/60"
                      >
                        {o.span_name} <span className="text-2xs font-normal text-muted-fg">{o.kind}</span>
                      </button>
                    </Hint>
                    <OpTag o={o} />
                    <ErrorRate rate={o.error_rate} />
                  </div>
                  <div className="mt-1 flex flex-wrap gap-x-3 text-2xs text-muted-fg tabular-nums">
                    <span>
                      次数 <span className="text-fg">{formatNumber(o.requests)}</span> <Delta delta={deltaOf(o, 'requests')} upIs="neutral" />
                    </span>
                    <span>错误 {o.errors ? formatNumber(o.errors) : 0}</span>
                    <span>P50 {formatDurationMs(o.p50_ms)}</span>
                    <span>
                      P95 {formatDurationMs(o.p95_ms)} <Delta delta={deltaOf(o, 'p95_ms')} upIs="bad" />
                    </span>
                    <span>
                      P99 <span className="font-medium text-fg">{formatDurationMs(o.p99_ms)}</span>
                    </span>
                  </div>
                </li>
              ))}
            </ul>
          )}
          {rows.length > 0 && !isMobile && (
            <table className="w-full table-fixed border-collapse text-xs">
              <thead className="text-2xs text-muted-fg">
                <tr className="border-b border-border bg-muted/40">
                  {cols.map((c) => (
                    <th
                      key={c.key}
                      // 排序状态挂在 th 上，不是里面那个按钮上（和日志表、服务总览同一个口径）
                      aria-sort={sort.key !== c.key ? 'none' : sort.desc ? 'descending' : 'ascending'}
                      className={cn(
                        'px-4 py-2.5 font-medium select-none',
                        c.right ? 'w-28 text-right' : 'text-left',
                        sort.key === c.key && 'text-accent',
                        byDelta && !isComparable(c.key) && c.key !== 'span_name' && 'opacity-50',
                      )}
                    >
                      <Hint text={byDelta && isComparable(c.key) ? `按「${c.label}和${cmpShort}比的变化」排` : '点击排序'} asChild>
                        <button
                          type="button"
                          className="inline-flex cursor-pointer items-center gap-0.5 rounded-sm hover:text-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand/60"
                          onClick={() => setSort((s) => ({ key: c.key, desc: s.key === c.key ? !s.desc : c.key !== 'span_name' }))}
                        >
                          {c.label}
                          {sort.key === c.key && <span aria-hidden>{sort.desc ? '▾' : '▴'}</span>}
                        </button>
                      </Hint>
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {rows.map((o) => (
                  <tr
                    key={`${o.kind}:${o.span_name}`}
                    className={cn('row-hover cursor-pointer border-b border-border/60 last:border-b-0', op === o.span_name && 'row-selected')}
                    onClick={() => select(o.span_name)}
                  >
                    <td className="truncate px-4 py-2">
                      {/* 整行可点是给鼠标的方便；「只看这个接口」这个动作本身得有个真按钮，
                          不然键盘和读屏在这张表里什么都选不了 */}
                      <Hint text={`只看这个接口的趋势：${o.span_name}`} asChild>
                        <button
                          type="button"
                          aria-pressed={op === o.span_name}
                          onClick={(e) => {
                            e.stopPropagation()
                            select(o.span_name)
                          }}
                          className="max-w-full cursor-pointer truncate rounded-sm text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand/60"
                        >
                          <span className="font-medium">{o.span_name}</span> <span className="text-2xs font-normal text-muted-fg">{o.kind}</span>
                        </button>
                      </Hint>{' '}
                      <OpTag o={o} />
                    </td>
                    {cols.slice(1).map((c) => (
                      <OpCell key={c.key} o={o} col={c} />
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </Card>
      </div>
    </div>
  )
}

/** 接口表汇总一行：次数和错误率能加，分位数不能加（所以延迟看上面那张图） */
function summarize(list: OperationStat[]) {
  if (!list.length) return null
  let requests = 0
  let errors = 0
  let prevRequests = 0
  let prevErrors = 0
  let comparable = false
  let live = 0
  for (const o of list) {
    requests += o.requests
    errors += o.errors
    if (o.requests > 0) live++
    if (o.prev) {
      comparable = true
      prevRequests += o.prev.requests
      prevErrors += o.prev.errors
    }
  }
  return {
    requests,
    errors,
    rate: requests > 0 ? errors / requests : 0,
    prevRequests,
    prevRate: prevRequests > 0 ? prevErrors / prevRequests : 0,
    comparable,
    live,
  }
}

/** 「新接口」/「没了」的小标：整条流量的出现和消失，光看百分比是看不出来的 */
function OpTag({ o }: { o: OperationStat }) {
  if (o.requests === 0 && o.prev) return <Badge tone="warn">没了</Badge>
  if (!o.prev) return <Badge tone="muted">新</Badge>
  return null
}

function OpCell({ o, col }: { o: OperationStat; col: OpColumn }) {
  const live = o.requests > 0
  const value: ReactNode = (() => {
    switch (col.key) {
      case 'requests':
        return formatNumber(o.requests)
      case 'errors':
        return o.errors ? formatNumber(o.errors) : <span className="text-muted-fg">0</span>
      case 'error_rate':
        return live ? <ErrorRate rate={o.error_rate} /> : <span className="text-muted-fg">—</span>
      default:
        // 一次请求都没有的接口，分位数是「没有数据」，不是 0ms
        return live ? formatDurationMs(o[col.key] as number) : <span className="text-muted-fg">—</span>
    }
  })()
  return (
    <td className="px-4 py-2 text-right">
      <div className={cn('tabular-nums', col.key === 'p99_ms' && 'font-medium', col.key === 'max_ms' && 'text-muted-fg')}>{value}</div>
      <div className="h-4 leading-4">
        <Delta delta={deltaOf(o, col.key)} upIs={col.upIs ?? 'neutral'} />
      </div>
    </td>
  )
}

const MOVER_DOT: Record<Mover['tone'], string> = {
  danger: 'bg-danger',
  warn: 'bg-warn',
  ok: 'bg-ok',
  muted: 'bg-muted-fg/50',
}

/** 变化榜的一行：哪个接口、变成什么样了、影响面多大。点一行 = 下面的图只看它 */
function MoverRow({ m, selected, onSelect }: { m: Mover; selected: boolean; onSelect: () => void }) {
  return (
    <div>
      <Hint text={selected ? '再点一下看整个服务' : '只看这个接口的趋势'} asChild>
        <button
          type="button"
          onClick={onSelect}
          className={cn('row-hover flex w-full items-center gap-3 border-b border-border/60 px-3 py-2 text-left last:border-b-0', selected && 'row-selected')}
        >
          <span className={cn('size-2 shrink-0 rounded-full', MOVER_DOT[m.tone])} />
          <span className="min-w-0 flex-1">
            <Hint text={m.op.span_name}>
              <span className="block truncate text-xs font-medium">{m.op.span_name}</span>
            </Hint>
            <span className="block truncate text-2xs text-muted-fg">{m.detail}</span>
          </span>
          <span className="hidden shrink-0 text-2xs text-muted-fg tabular-nums sm:inline">{formatNumber(m.op.requests)} 次</span>
          <Badge tone={m.tone}>{m.impact}</Badge>
        </button>
      </Hint>
    </div>
  )
}
