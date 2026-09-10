import { useCallback, useEffect, useMemo, useState } from 'react'
import { Link, useNavigate } from 'react-router'
import { PlusIcon, SearchIcon, XIcon } from 'lucide-react'
import type { Params } from '@/api/client'
import { useMeta, useMetricCatalog, useMetricExemplars, useMetricLabelValues, useMetricLabels, useMetricQuery } from '@/api/queries'
import type { MetricAgg, MetricField, MetricInfo, MetricQueryResponse } from '@/api/types'
import { LineChart, type ChartMarker, type LineSeries } from '@/components/charts/LineChart'
import { StackedBars } from '@/components/charts/StackedBars'
import { StatsLine } from '@/components/StatsLine'
import { Badge, Button, Card, EmptyState, ErrorBox, Input, Select, Spinner } from '@/components/ui'
import { ColorAssigner } from '@/lib/colors'
import { coveredMetricNames, isErrorLabel, resolveDashboard, type ResolvedPanel } from '@/lib/dashboards'
import { logsHref, serviceHref, traceHref, tracesHref } from '@/lib/links'
import { formatBytes, formatDuration, formatDurationMs, formatTs } from '@/lib/time'
import { useInView } from '@/lib/in-view'
import { useIsMobile } from '@/lib/media'
import { splitList, useTimeRange, useUrlState } from '@/lib/url-state'
import { cn } from '@/lib/utils'

/** 步长：不选就按时间范围自动挑（后端最多 60 个点）。 */
const STEPS = [
  { value: '', label: '自动步长' },
  { value: '10', label: '10 秒' },
  { value: '30', label: '30 秒' },
  { value: '60', label: '1 分钟' },
  { value: '300', label: '5 分钟' },
  { value: '900', label: '15 分钟' },
  { value: '3600', label: '1 小时' },
]

const QUANTILES = ['0.5', '0.9', '0.95', '0.99']

/** 一个可选的算法 = agg + 取哪一列。key 是下拉的值，`agg:field`。 */
interface AggOption {
  key: string
  label: string
  agg: MetricAgg
  field: MetricField
  hint?: string
}

const opt = (agg: MetricAgg, field: MetricField, label: string, hint?: string): AggOption => ({
  key: `${agg}:${field}`,
  label,
  agg,
  field,
  hint,
})

/**
 * 一个指标能怎么看，按类型给：counter 存的是累计值，只有速率和增量有意义；直方图看分位数和
 * 平均值；gauge 看水位。
 */
function aggOptions(info?: MetricInfo): AggOption[] {
  switch (info?.type) {
    case 'Histogram':
      return [
        opt('quantile', 'value', '分位数', '把各时间线的原始桶合并后插值，不是对分位数取平均'),
        opt('mean', 'sum', '平均值', 'sum 的增量 ÷ count 的增量'),
        opt('rate', 'count', '次数 / 秒'),
        opt('increase', 'count', '次数（每步长）'),
        opt('max', 'max', '最大值'),
      ]
    case 'ExponentialHistogram':
    case 'Summary':
      // 指数直方图的桶是 base^i 编码的，没有 explicit_bounds；Summary 的分位数是采集端算好的，
      // 多条时间线合不起来。两者都只看 count / sum
      return [opt('mean', 'sum', '平均值'), opt('rate', 'count', '次数 / 秒'), opt('increase', 'count', '次数（每步长）')]
    case 'Sum':
      return info.monotonic
        ? [
            opt('rate', 'value', '每秒增量', 'Cumulative 的按时间线相减，进程重启认得出来'),
            opt('increase', 'value', '增量（每步长）'),
            opt('last', 'value', '累计值'),
          ]
        : [opt('last', 'value', '最后一个值'), opt('avg', 'value', '平均'), opt('max', 'value', '最大'), opt('min', 'value', '最小'), opt('sum', 'value', '求和')]
    default:
      return [opt('avg', 'value', '平均'), opt('last', 'value', '最后一个值'), opt('max', 'value', '最大'), opt('min', 'value', '最小'), opt('sum', 'value', '求和')]
  }
}

/** 数值 → 人读的字符串。单位是 OTLP 的 UCUM 写法（`ms` / `s` / `By` / `1`）。 */
function valueFormatter(
  info: MetricInfo | undefined,
  agg: MetricAgg,
  field: MetricField,
  percent = false,
): (v: number) => string {
  if (percent) return (v) => `${compact(v * 100)}%`
  const per = agg === 'rate' ? '/s' : ''
  // 取的是次数，就和指标本身的单位无关了
  const unit = field === 'count' || agg === 'count' ? '1' : (info?.unit ?? '').trim()
  if (unit === 's') return (v) => formatDuration(v * 1e9) + per
  if (unit === 'ms') return (v) => formatDurationMs(v) + per
  if (unit === 'us') return (v) => formatDuration(v * 1000) + per
  if (unit === 'ns') return (v) => formatDuration(v) + per
  if (unit === 'By' || unit === 'by') return (v) => formatBytes(v) + per
  if (unit === '%') return (v) => `${compact(v)}%`
  return (v) => compact(v) + per
}

/** 坐标轴和图例都放得下的短数字。 */
function compact(v: number): string {
  if (!Number.isFinite(v)) return '-'
  const a = Math.abs(v)
  if (a === 0) return '0'
  if (a >= 1e9) return `${(v / 1e9).toFixed(2)}G`
  if (a >= 1e6) return `${(v / 1e6).toFixed(2)}M`
  if (a >= 1e4) return `${(v / 1e3).toFixed(1)}k`
  if (a >= 1) return String(Math.round(v * 100) / 100)
  return Number(v.toPrecision(3)).toString()
}

function typeTone(t: string): 'accent' | 'info' | 'ok' | 'muted' {
  if (t === 'Sum') return 'accent'
  if (t === 'Gauge') return 'info'
  if (t.endsWith('Histogram') || t === 'Summary') return 'ok'
  return 'muted'
}

const STAT_LABELS = { min: '最小', avg: '平均', max: '最大', last: '最后' } as const

/** 一个指标查询的参数，看板和浏览两边共用（同样的参数 = react-query 同一个缓存条目）。 */
function queryParams(
  rangeParams: Params,
  metric: string,
  service: string[],
  agg: MetricAgg,
  field: MetricField,
  extra: { by?: string[]; attr?: string[]; q?: string; step?: string; limit?: number } = {},
): Params {
  return {
    ...rangeParams,
    metric,
    service,
    agg,
    field,
    by: extra.by,
    attr: extra.attr,
    q: agg === 'quantile' ? (extra.q ?? '0.95') : undefined,
    step: extra.step || undefined,
    limit: extra.limit ?? 20,
  }
}

export function MetricsPage() {
  const { range } = useTimeRange()
  const { params, set } = useUrlState()
  const rangeParams: Params = useMemo(() => ({ from: range.fromMs, to: range.toMs }), [range])
  const view = params.get('view') === 'all' ? 'all' : 'board'
  const service = params.get('service') ?? ''

  // 不带服务的目录：既是「全部指标」那一页的列表，也是服务下拉的来源
  const catalog = useMetricCatalog(rangeParams)
  const allServices = useMemo(() => {
    const set = new Set<string>()
    for (const m of catalog.data?.metrics ?? []) for (const s of m.services) set.add(s)
    return [...set].sort()
  }, [catalog.data])

  // 看板必须锁一个服务；第一次进来自动选一个，免得开局是空的
  useEffect(() => {
    if (view === 'board' && !service && allServices.length) set({ service: allServices[0] }, { replace: true })
  }, [view, service, allServices, set])

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-border bg-card px-3 pt-2 md:px-4">
        <nav className="order-last flex w-full items-stretch gap-1 md:order-none md:w-auto">
          {(['board', 'all'] as const).map((v) => (
            <button
              key={v}
              type="button"
              onClick={() => set({ view: v === 'board' ? null : v })}
              className={cn('cf-tab flex h-9 items-center px-3 text-sm font-medium text-muted-fg hover:text-fg', view === v && 'text-fg')}
              data-active={view === v ? 'true' : undefined}
            >
              {v === 'board' ? '服务看板' : '全部指标'}
            </button>
          ))}
        </nav>
        <span className="flex items-center gap-2 py-2 md:ml-auto">
          {view === 'board' && (
            <Select value={params.get('step') ?? ''} onChange={(e) => set({ step: e.target.value || null })} title="每个点多长时间">
              {STEPS.map((s) => (
                <option key={s.value} value={s.value}>
                  {s.label}
                </option>
              ))}
            </Select>
          )}
          <Select
            value={service}
            onChange={(e) => set({ service: e.target.value || null, metric: null })}
            className={cn('min-w-40', service && 'border-accent text-accent')}
            title="看哪个服务的指标"
          >
            <option value="">{view === 'board' ? '选择服务' : '全部服务'}</option>
            {allServices.map((s) => (
              <option key={s} value={s}>
                {s}
              </option>
            ))}
          </Select>
          {catalog.isFetching && <Spinner className="size-3.5" />}
        </span>
      </header>
      {catalog.isError && <ErrorBox error={catalog.error} onRetry={() => catalog.refetch()} />}
      {view === 'board' ? (
        <MetricDashboard service={service} rangeParams={rangeParams} allServices={allServices} />
      ) : (
        <MetricExplorer service={service} rangeParams={rangeParams} catalogMetrics={catalog.data?.metrics ?? []} loading={catalog.isPending} />
      )}
    </div>
  )
}

/* ------------------------------------------------------------------ 服务看板 */

/**
 * 按语义约定拼出来的成套面板。指标目录锁定这个服务再查一次（`/api/metrics?service=x`），
 * 拿到的就是这个服务真的在报的那些指标名，据此决定显示哪些面板——没有的东西不占地方。
 */
function MetricDashboard({ service, rangeParams, allServices }: { service: string; rangeParams: Params; allServices: string[] }) {
  const { params, set } = useUrlState()
  const { setRange } = useTimeRange()
  const step = params.get('step') ?? ''
  // 一块看板上所有图共用一根十字线：鼠标停在某个时刻，二十张图一起在同一时刻画竖线，
  // 「GC 那一下和延迟尖峰是不是同一时刻」这种问题不用来回对 x 轴
  const [hoverTs, setHoverTs] = useState<number | null>(null)
  const catalog = useMetricCatalog({ ...rangeParams, service }, !!service)
  const metrics = useMemo(() => catalog.data?.metrics ?? [], [catalog.data])
  const sections = useMemo(() => resolveDashboard(metrics), [metrics])
  const uncovered = useMemo(() => {
    const covered = coveredMetricNames()
    return metrics.filter((m) => !covered.has(m.name)).length
  }, [metrics])

  if (!service) {
    return (
      <EmptyState
        title="先选一个服务"
        hint={allServices.length ? '上面的下拉里是最近有上报指标的服务。' : '这段时间没有任何服务上报指标。'}
      />
    )
  }
  if (catalog.isPending) {
    return (
      <div className="flex justify-center py-16">
        <Spinner />
      </div>
    )
  }
  if (!sections.length) {
    return (
      <EmptyState
        title={`${service} 没有看板能认出来的指标`}
        hint={
          metrics.length
            ? `它上报了 ${metrics.length} 个指标，但都不是语义约定里的那几套（HTTP / JVM / 数据库连接池 / Kafka / Go）。去「全部指标」里挑。`
            : '这段时间它一个指标都没报。'
        }
        action={
          <Button size="sm" onClick={() => set({ view: 'all' })}>
            去全部指标
          </Button>
        }
      />
    )
  }

  return (
    <div className="min-h-0 flex-1 overflow-auto p-3 md:p-4">
      <CrossLinks service={service} rangeParams={rangeParams} />
      <Overview service={service} rangeParams={rangeParams} sections={sections} step={step} />
      {sections.map((section) => (
        <section key={section.key} className="mb-5 border-t border-border pt-3 last:mb-0">
          <h2 className="mb-2.5 flex items-baseline gap-2 text-sm font-semibold">
            {section.title}
            {section.hint && <span className="text-2xs font-normal text-muted-fg">{section.hint}</span>}
          </h2>
          {/* 只排两列：一行两张图比三张窄图好读，而且面板多是偶数张，正好排满。
              奇数张时最后一张跨满整行，不留半行空白 */}
          <div className="grid gap-3 lg:grid-cols-2">
            {section.panels.map((panel, i) => (
              <DashboardPanel
                key={panel.key}
                panel={panel}
                service={service}
                rangeParams={rangeParams}
                step={step}
                wide={section.panels.length % 2 === 1 && i === section.panels.length - 1}
                hoverTs={hoverTs}
                onHoverTs={setHoverTs}
                onBrush={(f, t) => setRange({ fromMs: Math.round(f), toMs: Math.round(t), relative: null })}
              />
            ))}
          </div>
        </section>
      ))}
      {uncovered > 0 && (
        <div className="pb-2 text-2xs text-muted-fg">
          这个服务还有 {uncovered} 个指标看板没画（Kafka 的细项、SDK 自己的导出指标之类），在
          <button type="button" className="mx-1 text-accent hover:underline" onClick={() => set({ view: 'all' })}>
            全部指标
          </button>
          里。
        </div>
      )}
    </div>
  )
}

/**
 * 从指标跳到另外两个信号。带的是**当前这个时间窗**（在图上拖选之后就是拖出来的那一段），
 * 三个页面共用一套 `from` / `to` 参数，跳过去看到的就是同一段时间。
 */
function CrossLinks({ service, rangeParams }: { service: string; rangeParams: Params }) {
  const meta = useMeta()
  // 日志表上服务这一维叫什么，按 /api/meta 给的维度列来（老表没有 service_name 就退回 container）
  const logDim = meta.data?.logs.dimensions.includes('service_name') ? 'service_name' : 'container'
  const win = { fromMs: Number(rangeParams.from), toMs: Number(rangeParams.to) }
  const links: { to: string; label: string; title: string }[] = [
    { to: tracesHref({ service, sort: 'duration', kinds: 'Server,Consumer' }, win), label: '最慢的链路', title: '这段时间里这个服务最慢的请求' },
    { to: tracesHref({ service, errorOnly: true }, win), label: '出错的链路', title: '这段时间里出错的请求' },
    { to: logsHref({ dim: logDim, service, levels: 'ERROR,WARN' }, win), label: '错误日志', title: '这段时间这个服务的 ERROR / WARN 日志' },
    { to: serviceHref(service, win), label: '服务概览', title: '按链路算出来的请求量 / 错误率 / 分位数' },
  ]
  return (
    <div className="mb-3 flex flex-wrap items-center gap-2">
      <span className="text-2xs text-muted-fg">在图上横向拖一段可以缩小时间范围，然后跳到</span>
      {links.map((l) => (
        <Link key={l.label} to={l.to} title={l.title}>
          <Button size="xs">{l.label}</Button>
        </Link>
      ))}
    </div>
  )
}

/** 顶部三个数：请求量、错误率、P95。用的是下面 HTTP 面板同一条查询，不额外发请求。 */
function Overview({
  service,
  rangeParams,
  sections,
  step,
}: {
  service: string
  rangeParams: Params
  sections: ReturnType<typeof resolveDashboard>
  step: string
}) {
  const http = sections.find((s) => s.key === 'http_server')
  const ratePanel = http?.panels.find((p) => p.key === 'http_server_rate')
  const latencyPanel = http?.panels.find((p) => p.key === 'http_server_latency')
  const rate = useMetricQuery(
    panelParams(ratePanel, rangeParams, service, step),
    !!ratePanel,
  )
  const latency = useMetricQuery(panelParams(latencyPanel, rangeParams, service, step), !!latencyPanel)
  if (!ratePanel && !latencyPanel) return null

  // 每条时间线在窗口内的平均值就是它的平均速率，各条加起来是总量
  const series = rate.data?.series ?? []
  const total = series.reduce((n, s) => n + (s.avg ?? 0), 0)
  const errors = series.filter((s) => s.labels.some((l) => isErrorLabel(l.value))).reduce((n, s) => n + (s.avg ?? 0), 0)
  const quantile = (p: string) => latency.data?.series.find((s) => s.labels.some((l) => l.value === p))?.avg
  const fmtLatency = valueFormatter(latencyPanel?.info, 'quantile', 'value')
  const show = (v: number | null | undefined) => (v == null ? '-' : fmtLatency(v))

  return (
    <div className="mb-4 grid grid-cols-2 gap-3 lg:grid-cols-4">
      {ratePanel && <Stat label="请求量" value={total > 0 ? `${compact(total)}/s` : '-'} loading={rate.isPending} />}
      {ratePanel && (
        <Stat
          label="错误率"
          value={total > 0 ? `${(100 * (errors / total)).toFixed(2)}%` : '-'}
          tone={total > 0 && errors / total >= 0.01 ? 'danger' : undefined}
          loading={rate.isPending}
        />
      )}
      {latencyPanel && <Stat label="P95 延迟" value={show(quantile('p95'))} loading={latency.isPending} />}
      {latencyPanel && <Stat label="P99 延迟" value={show(quantile('p99'))} loading={latency.isPending} />}
    </div>
  )
}

function Stat({ label, value, tone, loading }: { label: string; value: string; tone?: 'danger'; loading?: boolean }) {
  return (
    <div className="rounded-lg border border-border bg-card px-3.5 py-2.5">
      <div className="truncate text-2xs text-muted-fg">{label}</div>
      <div className={cn('truncate text-xl font-semibold tabular-nums', tone === 'danger' && 'text-danger')}>
        {loading ? <Spinner className="size-4" /> : value}
      </div>
    </div>
  )
}

function panelParams(panel: ResolvedPanel | undefined, rangeParams: Params, service: string, step: string): Params {
  if (!panel) return {}
  const v = panel.variant
  return queryParams(rangeParams, v.metric, [service], v.agg, v.field ?? 'value', {
    by: v.by,
    attr: v.attr,
    q: v.q,
    step,
    limit: 12,
  })
}

function DashboardPanel({
  panel,
  service,
  rangeParams,
  step,
  wide,
  hoverTs,
  onHoverTs,
  onBrush,
}: {
  panel: ResolvedPanel
  service: string
  rangeParams: Params
  step: string
  /** 这一排只剩它自己，跨满整行 */
  wide?: boolean
  /** 同一块看板上鼠标所在的时刻，所有图一起画竖线 */
  hoverTs?: number | null
  onHoverTs?: (t: number | null) => void
  onBrush?: (fromMs: number, toMs: number) => void
}) {
  const { set } = useUrlState()
  const isMobile = useIsMobile()
  const v = panel.variant
  // 滚进视口才查，见 useInView
  const [ref, inView] = useInView<HTMLDivElement>()
  const data = useMetricQuery(panelParams(panel, rangeParams, service, step), inView)
  const format = valueFormatter(panel.info, v.agg, v.field ?? 'value', v.percent)
  const { series, points, lastValues, sparse, hasPoints } = useChartData(data.data, v.percent)
  // 点图例可以把某条线摘掉：按接口分组时十几条挤在一起，只想看其中一两条
  const [hidden, setHidden] = useState<ReadonlySet<string>>(new Set())
  const shownSeries = series.filter((x) => !hidden.has(x.label))
  const toggle = (label: string) =>
    setHidden((prev) => {
      const next = new Set(prev)
      next.has(label) ? next.delete(label) : next.add(label)
      return next
    })

  return (
    <Card
      ref={ref}
      className={cn('flex flex-col', wide && 'lg:col-span-2')}
      title={
        <button
          type="button"
          className="flex min-w-0 items-baseline gap-2 text-left hover:text-accent"
          title={`${v.metric}　点开在「全部指标」里继续拆`}
          onClick={() =>
            set({
              view: 'all',
              metric: v.metric,
              agg: v.agg,
              field: v.field ?? 'value',
              q: v.q ?? null,
              by: null,
              attr: null,
            })
          }
        >
          <span className="truncate">{panel.title}</span>
          {panel.hint && (
            <span className="hidden truncate text-2xs font-normal text-muted-fg lg:inline" title={panel.hint}>
              {panel.hint}
            </span>
          )}
          {data.isFetching && <Spinner className="size-3.5 shrink-0" />}
        </button>
      }
    >
      {data.isError ? (
        <ErrorBox error={data.error} onRetry={() => data.refetch()} />
      ) : data.data && !hasPoints && !data.isFetching ? (
        // 一条线都没有，或者有线但整段全是空洞（比如这段时间一次 GC 都没发生）
        <div className="px-4 py-8 text-center text-xs text-muted-fg">这段时间没有数据点</div>
      ) : (
        <div className="flex flex-1 flex-col px-2 pt-2 pb-1">
          {v.kind === 'bars' ? (
            // 计数 / 速率画堆叠柱：按状态码、按接口堆起来，构成一眼看得出（和服务详情页一致）
            <StackedBars
              fromMs={data.data?.from_ms ?? Number(rangeParams.from)}
              toMs={data.data?.to_ms ?? Number(rangeParams.to)}
              widthMs={data.data?.width_ms ?? 60_000}
              buckets={points}
              series={shownSeries}
              height={isMobile ? 140 : 176}
              stale={data.isFetching}
              format={format}
              syncTs={hoverTs}
              onHoverTs={onHoverTs}
              onBrush={onBrush}
            />
          ) : (
            <LineChart
              fromMs={data.data?.from_ms ?? Number(rangeParams.from)}
              toMs={data.data?.to_ms ?? Number(rangeParams.to)}
              widthMs={data.data?.width_ms ?? 60_000}
              points={points}
              series={shownSeries}
              height={isMobile ? 140 : 176}
              stale={data.isFetching}
              format={format}
              connectGaps
              dots={sparse}
              area={shownSeries.length === 1}
              syncTs={hoverTs}
              onHoverTs={onHoverTs}
              onBrush={onBrush}
            />
          )}
          <PanelLegend
            series={series}
            rows={data.data?.series ?? []}
            values={lastValues}
            format={format}
            hidden={hidden}
            onToggle={toggle}
            bars={v.kind === 'bars'}
          />
        </div>
      )}
    </Card>
  )
}

/**
 * 面板底下的小图例：名字 + 最后一个值。**高度固定两行**——同一排的卡片靠这个才对得齐，
 * 不然一张图例三行、旁边一行，两张卡片的图表底边就错开了。多出来的收成「+N」。
 */
const LEGEND_MAX = 4

function PanelLegend({
  series,
  rows,
  values,
  format,
  hidden,
  onToggle,
  bars,
}: {
  series: LineSeries[]
  rows: MetricQueryResponse['series']
  /** 最后一个完整桶的值，和 rows 等长 */
  values: (number | null)[]
  format: (v: number) => string
  /** 这个面板画的是柱状图（色块用方块） */
  bars?: boolean
  /** 被点掉的线（按图例名字），点一下摘掉 / 加回来 */
  hidden?: ReadonlySet<string>
  onToggle?: (label: string) => void
}) {
  const labels = useMemo(() => legendLabels(rows), [rows])
  if (!rows.length) return null
  const shown = rows.slice(0, LEGEND_MAX)
  return (
    <div className="mt-auto flex min-h-8 flex-wrap content-start gap-x-3 gap-y-0.5 px-1 pt-1.5 pb-1 text-2xs text-muted-fg">
      {shown.map((s, i) => {
        const off = hidden?.has(s.name)
        return (
          <button
            key={s.name}
            type="button"
            disabled={!onToggle}
            onClick={() => onToggle?.(s.name)}
            title={onToggle ? `${s.name}（点一下只摘掉 / 加回这条线）` : s.name}
            className={cn('inline-flex min-w-0 items-center gap-1.5 disabled:cursor-default', onToggle && 'hover:text-fg', off && 'opacity-40')}
          >
            {/* 色块形状跟着图走：柱状图是小方块，折线是短横 */}
            <span
              className={cn('inline-block shrink-0 rounded-sm', bars ? 'size-2' : 'h-0.5 w-3')}
              style={{ background: series[i]?.color }}
            />
            <span className={cn('mono max-w-52 truncate', off && 'line-through')}>{labels[i]}</span>
            <span className="shrink-0 tabular-nums text-fg">{values[i] != null ? format(values[i] as number) : '-'}</span>
          </button>
        )
      })}
      {rows.length > shown.length && <span title={rows.slice(LEGEND_MAX).map((s) => s.name).join('\n')}>+{rows.length - shown.length} 条</span>}
    </div>
  )
}

/**
 * 图例上写什么：只留有区分度的那部分。
 *
 * 后端给的名字是 `http.route=/x, quantile=p95` 这种全写。键名在一个面板里都一样，没信息量；
 * 取值在所有系列里都一样的键（按接口看 P95 时的 `quantile=p95`）也没信息量，一并去掉——
 * 剩下的才是这条线和别的线的区别。全都一样（只有一条线）就退回指标名。
 */
function legendLabels(rows: MetricQueryResponse['series']): string[] {
  if (!rows.length) return []
  const keys = rows[0].labels.map((l) => l.key)
  const varying = keys.filter((k) => new Set(rows.map((r) => r.labels.find((l) => l.key === k)?.value)).size > 1)
  const use = varying.length ? varying : keys
  return rows.map((r) => {
    const parts = use.map((k) => r.labels.find((l) => l.key === k)?.value || '-')
    return parts.join(' / ') || r.name
  })
}

/**
 * 后端返回的「一条共用时间轴 + 每条线一个数组」→ LineChart 要的形状。
 *
 * 会**丢掉最后一个不完整的桶**：时间范围的右端就是「现在」，最后那一格往往才过了几秒，
 * 速率和计数都只统计了一小截。不丢的话每张图末尾都往下掉一截，图例读数（最后一个值）
 * 也跟着偏小——看图的人会以为量掉下去了。丢掉之后图例读的是最后一个完整桶。
 */
function useChartData(data: MetricQueryResponse | undefined, percent = false) {
  return useMemo(() => {
    const rows = data?.series ?? []
    const colors = new ColorAssigner()
    const series: LineSeries[] = rows.map((s, i) => ({ key: String(i), label: s.name, color: colors.color(s.name) }))
    const width = data?.width_ms ?? 0
    const to = data?.to_ms ?? 0
    const all = data?.t_ms ?? []
    const count = all.length && width > 0 && all[all.length - 1] + width > to ? all.length - 1 : all.length
    const points = all.slice(0, count).map((t, i) => {
      const values: Record<string, number> = {}
      for (const [si, s] of rows.entries()) {
        const v = s.values[i]
        if (v !== null && v !== undefined) values[String(si)] = v
      }
      return { t_ms: t, values }
    })
    // 图例读数：最后一个有值的完整桶
    const lastValues = rows.map((s) => {
      for (let i = count - 1; i >= 0; i--) {
        const v = s.values[i]
        if (v !== null && v !== undefined) return v
      }
      return null
    })
    // 上报周期比步长长时点很稀，只画线段是看不见的（单点线段画不出东西），补上圆点
    const filled = points.filter((p) => Object.keys(p.values).length > 0).length
    return { series, points, lastValues, sparse: filled < 40, hasPoints: filled > 0, percent }
  }, [data, percent])
}

/* ------------------------------------------------------------------ 全部指标 */

function MetricExplorer({
  service,
  rangeParams,
  catalogMetrics,
  loading,
}: {
  service: string
  rangeParams: Params
  catalogMetrics: MetricInfo[]
  loading: boolean
}) {
  const navigate = useNavigate()
  const isMobile = useIsMobile()
  const { params, set, setParams } = useUrlState()
  const { range, setRange } = useTimeRange()

  const metric = params.get('metric') ?? ''
  const services = service ? [service] : splitList(params.get('service'))
  const by = params.getAll('by')
  const attrs = params.getAll('attr')
  const step = params.get('step') ?? ''
  const quantiles = splitList(params.get('q')).length ? splitList(params.get('q')) : ['0.95']
  const showExemplars = params.get('ex') !== '0'

  const shown = useMemo(
    () => (service ? catalogMetrics.filter((m) => m.services.includes(service)) : catalogMetrics),
    [catalogMetrics, service],
  )
  const info = catalogMetrics.find((m) => m.name === metric)
  const options = useMemo(() => aggOptions(info), [info])
  // URL 上没有、或者对这个指标不适用的算法，退回该类型的第一个——换指标时自动切到合适的
  const wanted = `${params.get('agg') ?? ''}:${params.get('field') ?? 'value'}`
  const choice = options.find((o) => o.key === wanted) ?? options[0]

  const serviceKey = services.join(',')
  const byKey = by.join(' ')
  const attrKey = attrs.join(' ')
  const qKey = quantiles.join(',')
  const params_ = useMemo(
    () =>
      queryParams(rangeParams, metric, services, choice.agg, choice.field, {
        by,
        attr: attrs,
        q: qKey,
        step,
        limit: 20,
      }),
    // 依赖里放拼好的字符串而不是数组：数组每次渲染都是新对象，query key 会一直变
    [rangeParams, metric, serviceKey, choice.key, byKey, attrKey, step, qKey],
  )
  const data = useMetricQuery(params_, !!metric)
  const exemplarParams: Params = useMemo(
    () => ({ ...rangeParams, metric, service: services, attr: attrs, limit: 100 }),
    [rangeParams, metric, serviceKey, attrKey],
  )
  const exemplars = useMetricExemplars(exemplarParams, !!metric && showExemplars)

  const rows = data.data?.series ?? []
  const { series, points, sparse } = useChartData(data.data)  // 同样丢掉最后一个不完整的桶
  const format = valueFormatter(info, choice.agg, choice.field)
  const markers: ChartMarker[] = (exemplars.data?.exemplars ?? []).map((e) => ({
    t_ms: e.t_ms,
    value: e.value,
    title: `${formatTs(e.t_ms, { ms: false })}  ${format(e.value)}  ${e.service} — 点开看这次请求`,
    onClick: () => navigate(traceHref(e.trace_id, e.t_ms)),
  }))

  const pick = useCallback(
    (name: string) => {
      setParams((prev) => {
        const p = new URLSearchParams(prev)
        p.set('metric', name)
        // 换指标时旧的分组维度、过滤、算法都不再适用
        for (const k of ['by', 'attr', 'agg', 'field']) p.delete(k)
        return p
      })
    },
    [setParams],
  )
  const setList = useCallback(
    (key: string, values: string[]) => {
      setParams((prev) => {
        const p = new URLSearchParams(prev)
        p.delete(key)
        for (const v of values) p.append(key, v)
        return p
      })
    },
    [setParams],
  )

  return (
    <div className="flex min-h-0 flex-1 flex-col md:flex-row">
      {isMobile ? (
        <div className="border-b border-border bg-card px-3 py-2.5">
          <Select value={metric} onChange={(e) => pick(e.target.value)} className="w-full">
            <option value="">选择指标{loading ? '…' : `（${shown.length}）`}</option>
            {shown.map((m) => (
              <option key={`${m.name}:${m.type}`} value={m.name}>
                {m.name}
              </option>
            ))}
          </Select>
        </div>
      ) : (
        <aside className="flex w-72 shrink-0 flex-col border-r border-border bg-card lg:w-80">
          <MetricList metrics={shown} loading={loading} selected={metric} onSelect={pick} />
        </aside>
      )}

      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        {!metric ? (
          <EmptyState
            title="选一个指标"
            hint={
              shown.length === 0
                ? '这段时间里没有指标数据。metricpipe 起来了吗？OTLP 指标发到它的 4317 / 4318 端口。'
                : 'counter 默认看每秒增量，直方图默认看分位数。想看成套的面板去「服务看板」。'
            }
          />
        ) : (
          <>
            <header className="flex flex-col gap-2.5 border-b border-border bg-card px-3 py-2.5 md:px-4 md:py-3">
              <div className="flex flex-wrap items-center gap-2">
                <span className="mono min-w-0 max-w-full truncate text-sm font-semibold" title={info?.description || metric}>
                  {metric}
                </span>
                {info && <Badge tone={typeTone(info.type)}>{info.type}</Badge>}
                {info?.unit && info.unit !== '1' && <Badge>{info.unit}</Badge>}
                {info?.temporality === 'Cumulative' && <Badge title="存的是累计值，速率是查询时相减出来的">累计</Badge>}
                <span className="ml-auto flex flex-wrap items-center gap-2">
                  <Select
                    value={choice.key}
                    onChange={(e) => {
                      const o = options.find((x) => x.key === e.target.value)
                      if (o) set({ agg: o.agg, field: o.field })
                    }}
                    title={choice.hint}
                  >
                    {options.map((o) => (
                      <option key={o.key} value={o.key}>
                        {o.label}
                      </option>
                    ))}
                  </Select>
                  {choice.agg === 'quantile' && (
                    <span className="flex h-9 items-center gap-0.5 rounded-md border border-input p-0.5">
                      {QUANTILES.map((q) => (
                        <button
                          key={q}
                          type="button"
                          onClick={() => {
                            const next = quantiles.includes(q) ? quantiles.filter((x) => x !== q) : [...quantiles, q]
                            set({ q: next.length ? next.sort().join(',') : '0.95' })
                          }}
                          className={cn('h-full rounded-sm px-2 text-xs font-semibold text-muted-fg hover:bg-muted', quantiles.includes(q) && 'bg-accent-soft text-accent')}
                        >
                          p{Number(q) * 100}
                        </button>
                      ))}
                    </span>
                  )}
                  <Select value={step} onChange={(e) => set({ step: e.target.value || null })} title="每个点多长时间">
                    {STEPS.map((s) => (
                      <option key={s.value} value={s.value}>
                        {s.label}
                      </option>
                    ))}
                  </Select>
                  <Button size="md" active={showExemplars} onClick={() => set({ ex: showExemplars ? '0' : null })} title="指标上挂的 trace id：点圆点直接跳那次请求">
                    exemplar
                  </Button>
                </span>
              </div>
              <GroupBy metric={metric} rangeParams={rangeParams} by={by} onChange={(v) => setList('by', v)} />
              <LabelFilters metric={metric} rangeParams={rangeParams} attrs={attrs} onChange={(v) => setList('attr', v)} />
            </header>

            <div className="min-h-0 flex-1 overflow-auto p-3 md:p-4">
              <Card
                title={
                  <span className="flex items-center gap-2">
                    {choice.label}
                    {data.isFetching && <Spinner className="size-3.5" />}
                  </span>
                }
                extra={<StatsLine stats={data.data?.stats} />}
              >
                {data.isError ? (
                  <ErrorBox error={data.error} onRetry={() => data.refetch()} />
                ) : (
                  <div className="px-2 pt-3 pb-1 md:px-3">
                    <LineChart
                      fromMs={data.data?.from_ms ?? range.fromMs}
                      toMs={data.data?.to_ms ?? range.toMs}
                      widthMs={data.data?.width_ms ?? 60_000}
                      points={points}
                      series={series}
                      height={isMobile ? 200 : 300}
                      stale={data.isFetching}
                      format={format}
                      connectGaps
                      dots={sparse}
                      markers={markers}
                      onBrush={(f, t) => setRange({ fromMs: Math.round(f), toMs: Math.round(t), relative: null })}
                    />
                  </div>
                )}
                {data.data?.truncated && (
                  <div className="border-t border-border px-4 py-2 text-2xs text-muted-fg">
                    时间线太多，只画了最大的 {rows.length} 条——加个过滤条件，或者少选一个分组维度。
                  </div>
                )}
                {data.data && rows.length === 0 && !data.isFetching && (
                  <div className="border-t border-border px-4 py-6 text-center text-xs text-muted-fg">
                    这段时间没有数据点。换个时间范围，或者去掉过滤条件试试。
                  </div>
                )}
              </Card>

              {rows.length > 0 && (
                <Card className="mt-3 overflow-hidden md:mt-4" title={`时间线（${rows.length}）`}>
                  <table className="w-full table-fixed border-collapse text-xs">
                    <thead className="text-2xs text-muted-fg">
                      <tr className="border-b border-border bg-muted/40">
                        <th className="px-4 py-2.5 text-left font-medium">时间线</th>
                        {(['min', 'avg', 'max', 'last'] as const).map((k) => (
                          <th key={k} className="w-20 px-4 py-2.5 text-right font-medium md:w-24">
                            {STAT_LABELS[k]}
                          </th>
                        ))}
                      </tr>
                    </thead>
                    <tbody>
                      {rows.map((s, i) => (
                        <tr key={s.name} className="row-hover border-b border-border/60 last:border-b-0">
                          <td className="truncate px-4 py-2 font-medium" title={s.name}>
                            <span className="mr-2 inline-block h-0.5 w-3 rounded align-middle" style={{ background: series[i]?.color }} />
                            <span className="mono">{s.name}</span>
                          </td>
                          {(['min', 'avg', 'max', 'last'] as const).map((k) => (
                            <td key={k} className={cn('px-4 py-2 text-right tabular-nums', k === 'last' && 'font-medium')}>
                              {s[k] === null ? <span className="text-muted-fg">-</span> : format(s[k] as number)}
                            </td>
                          ))}
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </Card>
              )}

              {showExemplars && !!exemplars.data?.exemplars.length && (
                <div className="mt-2 text-2xs text-muted-fg">
                  图上 {exemplars.data.exemplars.length} 个圆点是 exemplar（指标上挂的 trace id），点一下跳到那次请求的链路。
                </div>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  )
}

/** 左边的指标目录：搜索 + 列表。指标名长，一行一个，选中的高亮。 */
function MetricList({
  metrics,
  loading,
  selected,
  onSelect,
}: {
  metrics: MetricInfo[]
  loading: boolean
  selected: string
  onSelect: (name: string) => void
}) {
  const [q, setQ] = useState('')
  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase()
    if (!needle) return metrics
    return metrics.filter((m) => m.name.toLowerCase().includes(needle) || m.description.toLowerCase().includes(needle))
  }, [metrics, q])

  return (
    <>
      <div className="border-b border-border p-2.5">
        <div className="relative">
          <SearchIcon className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-fg" />
          <Input value={q} onChange={(e) => setQ(e.target.value)} placeholder={`搜指标名（${metrics.length}）`} className="pl-9" aria-label="搜指标" />
        </div>
      </div>
      <div className="min-h-0 flex-1 overflow-auto">
        {loading && (
          <div className="flex justify-center py-10">
            <Spinner />
          </div>
        )}
        {!loading && shown.length === 0 && <div className="px-4 py-8 text-center text-xs text-muted-fg">没有匹配的指标</div>}
        <ul>
          {shown.map((m) => (
            <li key={`${m.name}:${m.type}`}>
              <button
                type="button"
                onClick={() => onSelect(m.name)}
                className={cn('row-hover block w-full border-b border-border/60 px-3 py-2 text-left', m.name === selected && 'row-selected')}
                title={m.description || m.name}
              >
                <span className="mono block truncate text-xs">{m.name}</span>
                <span className="mt-0.5 flex items-center gap-1.5 text-2xs text-muted-fg">
                  <Badge tone={typeTone(m.type)}>{m.type}</Badge>
                  {m.unit && m.unit !== '1' && <span>{m.unit}</span>}
                  <span className="truncate">{m.services.length > 1 ? `${m.services.length} 个服务` : m.services[0]}</span>
                </span>
              </button>
            </li>
          ))}
        </ul>
      </div>
    </>
  )
}

/** 标签名下拉：数据点属性直接给名字，resource 属性带 `res:` 前缀（后端按这个前缀分列）。 */
function useLabelKeys(metric: string, rangeParams: Params): string[] {
  const attrs = useMetricLabels({ ...rangeParams, metric, limit: 200 }, !!metric)
  const res = useMetricLabels({ ...rangeParams, metric, column: 'resource_attributes', limit: 200 }, !!metric)
  return useMemo(
    () => ['service_name', ...(attrs.data?.names ?? []).map((n) => n.name), ...(res.data?.names ?? []).map((n) => `res:${n.name}`)],
    [attrs.data, res.data],
  )
}

function GroupBy({ metric, rangeParams, by, onChange }: { metric: string; rangeParams: Params; by: string[]; onChange: (v: string[]) => void }) {
  const keys = useLabelKeys(metric, rangeParams)
  const [draft, setDraft] = useState('')
  const add = (k: string) => {
    const key = k.trim()
    if (key && !by.includes(key)) onChange([...by, key])
    setDraft('')
  }
  return (
    <div className="flex flex-wrap items-center gap-2">
      <span className="text-sm text-muted-fg">分组</span>
      {by.map((k) => (
        <span key={k} className="mono inline-flex h-8 items-center gap-1.5 rounded-md bg-accent-soft px-2.5 text-xs text-accent">
          {k}
          <button type="button" onClick={() => onChange(by.filter((x) => x !== k))} title="去掉这个维度">
            <XIcon className="size-3.5" />
          </button>
        </span>
      ))}
      <Input
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') {
            e.preventDefault()
            add(draft)
          }
        }}
        list="metric-label-keys"
        placeholder="标签名，如 http.route"
        className="mono h-8 w-full text-xs md:w-56"
        aria-label="分组维度"
      />
      <datalist id="metric-label-keys">
        {keys.map((k) => (
          <option key={k} value={k} />
        ))}
      </datalist>
      <Button size="sm" onClick={() => add(draft)} disabled={!draft.trim()}>
        <PlusIcon className="size-4" />
        分组
      </Button>
      {!by.includes('service_name') && (
        <Button size="sm" variant="ghost" onClick={() => add('service_name')}>
          按服务
        </Button>
      )}
    </div>
  )
}

/** 标签过滤：`key=value`，和链路页的属性过滤同一种写法。 */
function LabelFilters({ metric, rangeParams, attrs, onChange }: { metric: string; rangeParams: Params; attrs: string[]; onChange: (v: string[]) => void }) {
  const [key, setKey] = useState('')
  const [value, setValue] = useState('')
  // 值的下拉只对数据点属性有：service_name 是列，resource 属性走另一个 column
  const isAttr = !!key && !key.startsWith('res:') && key !== 'service_name'
  const values = useMetricLabelValues({ ...rangeParams, metric, key, limit: 100 }, !!metric && isAttr)
  useEffect(() => setValue(''), [key])
  const add = () => {
    const k = key.trim()
    if (!k) return
    const item = value.trim() ? `${k}=${value.trim()}` : k
    if (!attrs.includes(item)) onChange([...attrs, item])
    setKey('')
    setValue('')
  }
  return (
    <div className="flex flex-wrap items-center gap-2">
      <span className="text-sm text-muted-fg">过滤</span>
      {attrs.map((a) => (
        <span key={a} className="mono inline-flex h-8 items-center gap-1.5 rounded-md bg-accent-soft px-2.5 text-xs text-accent">
          {a}
          <button type="button" onClick={() => onChange(attrs.filter((x) => x !== a))} title="去掉">
            <XIcon className="size-3.5" />
          </button>
        </span>
      ))}
      <Input value={key} onChange={(e) => setKey(e.target.value)} list="metric-label-keys" placeholder="标签名" className="mono h-8 w-full text-xs md:w-48" aria-label="标签名" />
      <span className="hidden text-muted-fg md:inline">=</span>
      <Input
        value={value}
        onChange={(e) => setValue(e.target.value)}
        list="metric-label-values"
        placeholder="值（留空 = 只要有这个标签）"
        className="mono h-8 min-w-0 flex-1 text-xs md:w-56 md:flex-none"
        aria-label="标签值"
        onKeyDown={(e) => {
          if (e.key === 'Enter') {
            e.preventDefault()
            add()
          }
        }}
      />
      <datalist id="metric-label-values">
        {(values.data?.names ?? []).map((v) => (
          <option key={v.name} value={v.name} />
        ))}
      </datalist>
      <Button size="sm" onClick={add} disabled={!key.trim()}>
        <PlusIcon className="size-4" />
        加条件
      </Button>
    </div>
  )
}
