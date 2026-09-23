import { Suspense, lazy, useMemo, useState } from 'react'
import { CloudDownloadIcon, DownloadIcon, SearchIcon } from 'lucide-react'
import { apiUrl } from '@/api/client'
import { useBillBreakdown, useBillDaily, useBillDetail, useBillPeriods, useBillSummary, useMeta } from '@/api/queries'
import { CostAnalysis } from '@/components/CostAnalysis'
import type { BillAmount, BillPoint, BillProvider } from '@/api/types'
import { StatsLine } from '@/components/StatsLine'
import { StackedBars } from '@/components/charts/StackedBars'
import { Badge, Card, Combobox, EmptyState, ErrorBox, Hint, Input, Spinner, buttonClass } from '@/components/ui'
import {
  AMOUNTS,
  DIMENSIONS,
  PROVIDER_COLORS,
  PROVIDER_LABELS,
  changeRatio,
  dayTick,
  formatChange,
  formatMoney,
  formatMoneyShort,
  periodSpan,
  periodTick,
  shiftPeriod,
} from '@/lib/bills'
import { usePageTitle } from '@/lib/title'

/** 拉取账单的对话框：点了才加载，它带着一套轮询逻辑，没人点就不该进首屏 */
const BillSyncDialog = lazy(() => import('@/components/BillSyncDialog').then((m) => ({ default: m.BillSyncDialog })))
import { useUrlState } from '@/lib/url-state'
import { cn } from '@/lib/utils'

/**
 * 费用页：goscan 同步进来的火山引擎 / 阿里云账单。
 *
 * **时间参数是账期（`YYYY-MM`），而非顶栏的时间范围**——账单按月出具，最近的一条也可能是
 * 昨日才落库，「最近 1 小时」在本页没有意义。因此本页自行维护账期区间，URL 上使用
 * `from_period` / `to_period`，以避开顶栏时间范围的 `from` / `to`（那两个是 unix 毫秒，
 * 顶栏的「记住上次范围」会向每个地址补写，名字相撞便会覆盖账期）。
 *
 * 默认查看**库中最近有数据的那个账期**往前 6 个月，而非以今天为基准：每月 1 日打开页面时
 * 当月账单尚未出具，若以今天起算，首屏将空无一物。
 */
const DEFAULT_MONTHS = 6

/** 两个视图。账单看「钱花在哪个产品上」，分析看「这笔钱该记在哪条业务线头上、照此推算一个月多少」 */
const VIEWS = [
  { value: 'bills', label: '账单', hint: '按账单本身的维度查看：账期趋势、产品与实例排行、明细与导出' },
  {
    value: 'analysis',
    label: '分析',
    hint: '按归属规则分摊到业务线，并以日均推算月度预估；未配置规则时仍可查看按产品的日均',
  },
]
const DETAIL_PAGE = 50

/** 维度筛选在 URL 上就叫维度名本身（`product=云服务器 ECS`），和接口收的参数一致 */
const DIMENSION_KEYS = DIMENSIONS.map((d) => d.value)

export function CostPage() {
  usePageTitle('费用')
  const meta = useMeta()
  const bills = meta.data?.bills
  const { params, set } = useUrlState()
  const periods = useBillPeriods(!!bills)

  const known = periods.data?.periods ?? []
  const latest = periods.data?.latest ?? ''
  const to = params.get('to_period') || latest
  const from = params.get('from_period') || (to ? shiftPeriod(to, -(DEFAULT_MONTHS - 1)) : '')
  const amount = (AMOUNTS.find((a) => a.value === params.get('amount'))?.value ?? 'payable') as BillAmount
  const provider = (bills?.providers.find((p) => p === params.get('provider')) ?? '') as BillProvider | ''
  const by = DIMENSIONS.find((d) => d.value === params.get('by'))?.value ?? 'product'
  const q = params.get('q') ?? ''
  const page = Math.max(0, Number(params.get('page') ?? 0) || 0)
  // 分析视图：业务线分摊、日均与月度预估。两个视图共用账期、金额口径、云与搜索
  const view = params.get('view') === 'analysis' ? 'analysis' : 'bills'
  const days = params.get('days') ?? ''
  const estimate = params.get('est') || (to ? shiftPeriod(to, 1) : '')
  const [syncing, setSyncing] = useState(false)

  // 维度筛选：URL 上的维度键原样往接口传
  const filters = useMemo(() => {
    const out: Record<string, string> = {}
    for (const key of DIMENSION_KEYS) {
      const v = params.get(key)
      if (v) out[key] = v
    }
    return out
  }, [params])

  const base = useMemo(
    () => ({ from, to, amount, provider: provider || undefined, q: q || undefined, ...filters }),
    [from, to, amount, provider, q, filters],
  )
  const ready = !!bills && !!from && !!to

  const billsView = ready && view === 'bills'
  const summary = useBillSummary(base, billsView)
  // 按天只有日度表在的时候才有意义；两朵云都没有日粒度就整块不显示
  const dailyProviders = (bills?.daily_providers ?? []).filter((p) => !provider || p === provider)
  const daily = useBillDaily(base, billsView && dailyProviders.length > 0)
  const breakdown = useBillBreakdown({ ...base, by, limit: 15 }, billsView)
  const detail = useBillDetail(
    {
      ...base,
      provider: provider || undefined,
      granularity: params.get('gran') || undefined,
      limit: DETAIL_PAGE,
      offset: page * DETAIL_PAGE,
    },
    billsView,
  )

  const points = summary.data?.points ?? []
  const current = points.at(-1)
  const previous = points.at(-2)
  const mom = changeRatio(current?.total ?? 0, previous?.total ?? 0)
  const activeFilters = Object.entries(filters)

  // 没部署 goscan：页签本来就不显示，直接进来的给一句原因
  if (meta.data && !bills) {
    return (
      <div className="flex min-h-0 flex-1 items-center justify-center p-6">
        <EmptyState
          title="费用页未启用"
          hint={meta.data.bills_note ?? '当前部署没有 goscan 的账单表。goscan 按账期将火山引擎、阿里云的账单同步至同一数据库，接入后本页方有内容。'}
        />
      </div>
    )
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-border bg-card px-3 py-2.5 md:px-4 md:py-3">
        <h1 className="text-base font-semibold">费用</h1>
        <span role="group" aria-label="视图" className="flex h-8 items-center rounded-md border border-input p-0.5">
          {VIEWS.map((v) => (
            <Hint key={v.value} text={v.hint} asChild>
              <button
                type="button"
                aria-pressed={view === v.value}
                onClick={() => set({ view: v.value === 'bills' ? null : v.value, page: null })}
                className={cn('h-full rounded-sm px-2.5 text-xs text-muted-fg hover:text-fg', view === v.value && 'bg-accent-soft text-accent')}
              >
                {v.label}
              </button>
            </Hint>
          ))}
        </span>
        <Hint text="账单以账期（自然月）为单位出具，与顶栏的时间范围无关；本页单独选择账期">
          <span className="hidden text-xs text-muted-fg xl:inline">云账单，按账期查看</span>
        </Hint>
        {(summary.isFetching || periods.isFetching) && <Spinner className="size-4" />}
        <span className="ml-auto flex flex-wrap items-center gap-2">
          <StatsLine stats={summary.data?.stats} className="hidden text-2xs text-muted-fg 2xl:inline" />
          <PeriodPicker known={known} from={from} to={to} onChange={(next) => set({ ...next, page: null })} />
          <span role="group" aria-label="金额口径" className="flex h-8 items-center rounded-md border border-input p-0.5">
            {AMOUNTS.map((a) => (
              <Hint key={a.value} text={a.hint} asChild>
                <button
                  type="button"
                  aria-pressed={amount === a.value}
                  onClick={() => set({ amount: a.value === 'payable' ? null : a.value })}
                  className={cn('h-full rounded-sm px-2.5 text-xs text-muted-fg hover:text-fg', amount === a.value && 'bg-accent-soft text-accent')}
                >
                  {a.label}
                </button>
              </Hint>
            ))}
          </span>
          {bills?.sync && (
            <Hint text="账单由 goscan 按账期向云厂商拉取，并非实时推送；缺少哪些账期即可就地补拉" asChild>
              <button type="button" onClick={() => setSyncing(true)} className={buttonClass({ size: 'sm' })}>
                <CloudDownloadIcon className="size-4" />
                拉取账单
              </button>
            </Hint>
          )}
        {(bills?.providers.length ?? 0) > 1 && (
            <Combobox
              value={provider}
              onChange={(v) => set({ provider: v || null, page: null })}
              options={(bills?.providers ?? []).map((p) => ({ value: p, label: PROVIDER_LABELS[p] }))}
              placeholder="全部云"
              searchPlaceholder="筛云…"
              emptyText="没有匹配的云"
              title="按云筛选"
              size="sm"
              className="w-28"
            />
          )}
          <span className="relative">
            <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-fg" />
            <Input
              defaultValue={q}
              onKeyDown={(e) => {
                if (e.key === 'Enter') set({ q: (e.target as HTMLInputElement).value || null, page: null })
              }}
              onBlur={(e) => set({ q: e.target.value || null, page: null })}
              placeholder="搜索产品 / 实例"
              className="h-8 w-36 pl-8 text-xs"
              aria-label="搜索产品 / 计费项 / 实例名"
            />
          </span>
        </span>
      </header>

      {syncing && bills && (
        <Suspense fallback={null}>
          <BillSyncDialog providers={bills.providers} from={from} to={to} onClose={() => setSyncing(false)} />
        </Suspense>
      )}

      <div className="min-h-0 flex-1 overflow-auto p-3 md:p-4">
        {summary.isError && <ErrorBox error={summary.error} onRetry={() => summary.refetch()} />}
        {periods.isError && <ErrorBox error={periods.error} onRetry={() => periods.refetch()} />}

        {activeFilters.length > 0 && (
          <div className="mb-3 flex flex-wrap items-center gap-2 text-xs">
            <span className="text-muted-fg">筛选</span>
            {activeFilters.map(([key, value]) => (
              <span key={key} className="inline-flex max-w-full items-center gap-1.5 rounded-md bg-accent-soft px-2.5 py-1 text-accent">
                <span className="shrink-0 text-muted-fg">{DIMENSIONS.find((d) => d.value === key)?.label ?? key}</span>
                <span className="truncate">{value}</span>
                <Hint text="移除该筛选条件" asChild>
                  <button type="button" onClick={() => set({ [key]: null, page: null })}>
                    ✕
                  </button>
                </Hint>
              </span>
            ))}
          </div>
        )}

        {/* 账单表在、但一行数据都没有：这不是「查不到」，是同步还没跑过，说清楚省得对着空页面找原因 */}
        {periods.data && known.length === 0 && (
          <EmptyState
            title="账单表暂无数据"
            hint={
              bills?.sync
                ? '表结构已建好，但尚未同步到任何账单。goscan 按账期定时向云厂商拉取（并非常驻采集），可点击右上角「拉取账单」立即同步一次。'
                : '表结构已建好，但尚未同步到任何账单。goscan 按账期定时向云厂商拉取（并非常驻采集），请先确认其已运行、凭据已配置并至少同步过一次。'
            }
          />
        )}

        {ready && bills && known.length > 0 && view === 'analysis' && (
          <CostAnalysis base={base} ready={ready} bills={bills} days={days} estimate={estimate} onChange={set} />
        )}

        {ready && known.length > 0 && view === 'bills' && (
          <div className="flex flex-col gap-3 md:gap-4">
            <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
              <StatCard
                label={`${current?.t ?? to} 花费`}
                value={formatMoney(current?.total ?? 0)}
                hint={`${AMOUNTS.find((a) => a.value === amount)?.label}口径。金额按账单原币种直接相加；账号中若有非人民币账单，可切换「币种」维度核对`}
                stale={summary.isFetching}
              />
              <StatCard label={`${previous?.t ?? '上一账期'} 花费`} value={formatMoney(previous?.total ?? 0)} stale={summary.isFetching} />
              <StatCard
                label="环比"
                value={formatChange(mom)}
                tone={mom === null ? 'muted' : mom > 0 ? 'danger' : 'ok'}
                hint="以最后一个账期与其上一账期相比；当月账单尚未出齐时，该值天然偏低"
                stale={summary.isFetching}
              />
              <StatCard
                label={`${periodSpan(from, to)} 个账期合计`}
                value={formatMoney(summary.data?.total ?? 0)}
                extra={
                  <span className="mt-1 flex flex-wrap gap-x-2 gap-y-0.5 text-2xs text-muted-fg">
                    {Object.entries(summary.data?.by_provider ?? {}).map(([p, v]) => (
                      <span key={p}>
                        {PROVIDER_LABELS[p as BillProvider] ?? p} {formatMoneyShort(v)}
                      </span>
                    ))}
                  </span>
                }
                stale={summary.isFetching}
              />
            </div>

            <Card title="按账期" extra={<span className="text-2xs text-muted-fg">点击柱体可只看该账期</span>}>
              <PeriodBars
                points={points}
                providers={summary.data?.providers ?? []}
                stale={summary.isFetching}
                onPick={(period) => set({ from_period: period, to_period: period, page: null })}
              />
            </Card>

            {dailyProviders.length > 0 && (
              <Card
                title="按天"
                extra={
                  <span className="text-2xs text-muted-fg">
                    {dailyProviders.map((p) => PROVIDER_LABELS[p]).join(' / ')}
                    {dailyProviders.length < (bills?.providers.length ?? 0) && '（另一朵云未同步日度账单）'}
                  </span>
                }
              >
                <DailyChart points={daily.data?.points ?? []} providers={dailyProviders} stale={daily.isFetching} />
              </Card>
            )}

            <Card
              title={`按${DIMENSIONS.find((d) => d.value === by)?.label}排行`}
              extra={
                <Combobox
                  value={by}
                  onChange={(v) => set({ by: v === 'product' ? null : v })}
                  options={DIMENSIONS}
                  clearable={false}
                  searchPlaceholder="筛维度…"
                  emptyText="没有匹配的维度"
                  title="排行维度"
                  size="sm"
                  className="w-32"
                />
              }
            >
              {breakdown.isError && <ErrorBox error={breakdown.error} onRetry={() => breakdown.refetch()} />}
              <Breakdown
                rows={breakdown.data?.rows ?? []}
                other={breakdown.data?.other ?? 0}
                pending={breakdown.isPending}
                stale={breakdown.isFetching}
                selected={filters[by]}
                onPick={(key) => set({ [by]: filters[by] === key ? null : key, page: null })}
              />
            </Card>

            <Card
              title="明细"
              extra={
                <span className="flex items-center gap-2">
                  {detail.data && (
                    <span className="text-2xs text-muted-fg">
                      {PROVIDER_LABELS[detail.data.provider]}
                      {detail.data.granularity === 'daily' ? ' · 按天' : ' · 按月'}
                      {detail.data.total !== null && ` · ${detail.data.total.toLocaleString('zh-CN')} 行`}
                    </span>
                  )}
                  {bills?.alicloud_daily && bills.alicloud_monthly && (!provider || provider === 'alicloud') && (
                    <Combobox
                      value={params.get('gran') ?? 'monthly'}
                      onChange={(v) => set({ gran: v === 'monthly' ? null : v, page: null })}
                      options={[
                        { value: 'monthly', label: '月度' },
                        { value: 'daily', label: '日度' },
                      ]}
                      clearable={false}
                      searchPlaceholder="筛粒度…"
                      title="阿里云明细粒度"
                      size="sm"
                      className="w-24"
                    />
                  )}
                  <Hint text="导出当前筛选条件下的明细 CSV" asChild>
                    <a href={apiUrl('/bills/export', { ...base, granularity: params.get('gran') || undefined, format: 'csv' })} className={buttonClass({ size: 'sm' })} download>
                      <DownloadIcon className="size-4" />
                      CSV
                    </a>
                  </Hint>
                </span>
              }
            >
              {detail.isError && <ErrorBox error={detail.error} onRetry={() => detail.refetch()} />}
              <DetailTable rows={detail.data?.rows ?? []} pending={detail.isPending} stale={detail.isFetching} />
              <div className="flex items-center justify-end gap-2 border-t border-border px-3 py-2">
                <span className="mr-auto text-2xs text-muted-fg">第 {page * DETAIL_PAGE + 1} – {page * DETAIL_PAGE + (detail.data?.rows.length ?? 0)} 行</span>
                <button type="button" disabled={page === 0} onClick={() => set({ page: page - 1 || null })} className={buttonClass({ size: 'xs' })}>
                  上一页
                </button>
                <button
                  type="button"
                  disabled={(detail.data?.rows.length ?? 0) < DETAIL_PAGE}
                  onClick={() => set({ page: page + 1 })}
                  className={buttonClass({ size: 'xs' })}
                >
                  下一页
                </button>
              </div>
            </Card>
          </div>
        )}
      </div>
    </div>
  )
}

/** 顶部的关键数字。`tone` 仅用于环比：上涨标红，下降以另一种颜色区分 */
function StatCard({
  label,
  value,
  hint,
  tone = 'plain',
  extra,
  stale,
}: {
  label: string
  value: string
  hint?: string
  tone?: 'plain' | 'ok' | 'danger' | 'muted'
  extra?: React.ReactNode
  stale?: boolean
}) {
  const body = (
    <div className={cn('rounded-lg border border-border bg-card px-4 py-3', stale && 'opacity-60 transition-opacity')}>
      <div className="text-xs text-muted-fg">{label}</div>
      <div
        className={cn(
          'mt-1 text-xl font-semibold tabular-nums',
          tone === 'danger' && 'text-danger',
          tone === 'ok' && 'text-accent',
          tone === 'muted' && 'text-muted-fg',
        )}
      >
        {value}
      </div>
      {extra}
    </div>
  )
  return hint ? <Hint text={hint}>{body}</Hint> : body
}

/**
 * 账期柱状图。
 *
 * 不复用 `StackedBars`：它以时间为轴，按毫秒摊开；而账期是**分类**，2 月与 8 月等宽，
 * 若按时间轴绘制，2 月会明显偏窄。分类轴用 flex 排布即可，还省去一套 SVG 坐标计算。
 */
function PeriodBars({
  points,
  providers,
  stale,
  onPick,
}: {
  points: BillPoint[]
  providers: BillProvider[]
  stale?: boolean
  onPick: (period: string) => void
}) {
  const max = Math.max(1, ...points.map((p) => p.total))
  return (
    <div className={cn('px-3 pt-4 pb-2', stale && 'opacity-60 transition-opacity')}>
      <div className="flex h-40 items-end gap-1.5" role="img" aria-label={`按账期的花费，共 ${points.length} 个账期`}>
        {points.map((p) => (
          <Hint
            key={p.t}
            text={[
              `${p.t} 合计 ${formatMoney(p.total)}`,
              ...Object.entries(p.by_provider).map(([k, v]) => `${PROVIDER_LABELS[k as BillProvider] ?? k} ${formatMoney(v)}`),
            ].join('\n')}
            asChild
          >
            <button type="button" onClick={() => onPick(p.t)} className="flex h-full min-w-0 flex-1 flex-col justify-end gap-px rounded-sm hover:bg-muted/40">
              {/* 单根柱体按云堆叠，顺序与图例一致 */}
              {providers.map((provider) => {
                const v = p.by_provider[provider] ?? 0
                if (v <= 0) return null
                return <span key={provider} style={{ height: `${(v / max) * 100}%`, background: PROVIDER_COLORS[provider] }} className="w-full rounded-[2px]" />
              })}
              {p.total <= 0 && <span className="h-px w-full bg-border" />}
            </button>
          </Hint>
        ))}
      </div>
      <div className="mt-1.5 flex gap-1.5 text-center text-2xs text-muted-fg">
        {points.map((p) => (
          <span key={p.t} className="min-w-0 flex-1 truncate tabular-nums">
            {periodTick(p.t)}
          </span>
        ))}
      </div>
      <div className="mt-2 flex flex-wrap items-center gap-3 border-t border-border/60 pt-2 text-2xs text-muted-fg">
        {providers.map((p) => (
          <span key={p} className="flex items-center gap-1.5">
            <span className="size-2 rounded-[2px]" style={{ background: PROVIDER_COLORS[p] }} />
            {PROVIDER_LABELS[p]}
          </span>
        ))}
        <span className="ml-auto">纵轴上限 {formatMoneyShort(max)}</span>
      </div>
    </div>
  )
}

/** 按天的花费。每一天等宽，可直接复用时间轴那套堆叠柱状图 */
function DailyChart({ points, providers, stale }: { points: BillPoint[]; providers: BillProvider[]; stale?: boolean }) {
  const buckets = useMemo(
    () =>
      points.map((p) => ({
        // `2026-09-01` 按本地日零点处理：账单里的「哪一天」是账期概念，不含时刻
        t_ms: new Date(`${p.t}T00:00:00`).getTime(),
        values: Object.fromEntries(providers.map((k) => [k, p.by_provider[k] ?? 0])),
      })),
    [points, providers],
  )
  if (!points.length) {
    return <div className="px-4 py-8 text-center text-xs text-muted-fg">所选账期没有按天的账单数据</div>
  }
  const first = buckets[0].t_ms
  const last = buckets[buckets.length - 1].t_ms
  const DAY = 86_400_000
  return (
    <div className="px-2 py-3">
      <StackedBars
        fromMs={first}
        toMs={last + DAY}
        widthMs={DAY}
        buckets={buckets}
        series={providers.map((p) => ({ key: p, label: PROVIDER_LABELS[p], color: PROVIDER_COLORS[p] }))}
        format={formatMoneyShort}
        stale={stale}
        label="按天的花费"
        height={150}
      />
      <div className="px-2 text-2xs text-muted-fg">
        {points.length} 天，{dayTick(points[0].t)} – {dayTick(points[points.length - 1].t)}
      </div>
    </div>
  )
}

/** 排行：一行一个取值，条形长度表示占比。点击某行即将其加为筛选条件 */
function Breakdown({
  rows,
  other,
  pending,
  stale,
  selected,
  onPick,
}: {
  rows: { key: string; amount: number; share: number; by_provider: Partial<Record<BillProvider, number>> }[]
  other: number
  pending: boolean
  stale?: boolean
  selected?: string
  onPick: (key: string) => void
}) {
  if (pending) {
    return (
      <div className="flex justify-center py-10">
        <Spinner />
      </div>
    )
  }
  if (!rows.length) return <div className="px-4 py-8 text-center text-xs text-muted-fg">所选账期没有账单数据</div>
  return (
    <div className={cn(stale && 'opacity-60 transition-opacity')}>
      <ul>
        {rows.map((r) => (
          <li key={r.key}>
            <button
              type="button"
              onClick={() => onPick(r.key)}
              aria-pressed={selected === r.key}
              className={cn('row-hover flex w-full items-center gap-3 px-3 py-1.5 text-left', selected === r.key && 'bg-accent-soft/50')}
            >
              <span className="min-w-0 flex-1">
                <span className="flex items-baseline gap-2">
                  <span className="truncate text-xs">{r.key}</span>
                  {Object.keys(r.by_provider).length > 1 && <Badge tone="muted">两朵云</Badge>}
                </span>
                {/* 占比条：长度为占总额的比例，一眼可见哪几项占去大半 */}
                <span className="mt-1 block h-1.5 w-full rounded-full bg-muted">
                  <span className="block h-full rounded-full bg-brand" style={{ width: `${Math.min(100, r.share * 100)}%` }} />
                </span>
              </span>
              <span className="shrink-0 text-right">
                <span className="block text-xs font-semibold tabular-nums">{formatMoney(r.amount)}</span>
                <span className="block text-2xs text-muted-fg tabular-nums">{(r.share * 100).toFixed(1)}%</span>
              </span>
            </button>
          </li>
        ))}
      </ul>
      {other > 0 && (
        <div className="border-t border-border/60 px-3 py-2 text-2xs text-muted-fg">
          未进入排行的其余项合计 <span className="tabular-nums text-fg">{formatMoney(other)}</span>
        </div>
      )}
    </div>
  )
}

/** 明细表。两朵云的列已在服务端对齐，此处只负责排版 */
function DetailTable({ rows, pending, stale }: { rows: import('@/api/types').BillDetailRow[]; pending: boolean; stale?: boolean }) {
  if (pending) {
    return (
      <div className="flex justify-center py-10">
        <Spinner />
      </div>
    )
  }
  if (!rows.length) return <div className="px-4 py-8 text-center text-xs text-muted-fg">没有符合条件的账单明细</div>
  return (
    <div className={cn('overflow-x-auto', stale && 'opacity-60 transition-opacity')}>
      <table className="w-full text-xs">
        <thead className="sticky top-0 bg-card text-2xs text-muted-fg">
          <tr className="border-b border-border">
            <th className="px-3 py-2 text-left font-medium">账期</th>
            <th className="px-3 py-2 text-left font-medium">产品</th>
            <th className="px-3 py-2 text-left font-medium">计费项</th>
            <th className="px-3 py-2 text-left font-medium">实例</th>
            <th className="px-3 py-2 text-left font-medium">地域</th>
            <th className="px-3 py-2 text-right font-medium">用量</th>
            <th className="px-3 py-2 text-right font-medium">金额</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r, i) => (
            <tr key={`${r.instance_id}-${r.item}-${i}`} className="border-b border-border/60 last:border-b-0">
              <td className="px-3 py-1.5 whitespace-nowrap tabular-nums">{r.day || r.period}</td>
              <td className="max-w-40 truncate px-3 py-1.5">
                <Hint text={`${PROVIDER_LABELS[r.provider]} · ${r.account || '—'} · ${r.subscription || '—'}`}>
                  <span>{r.product}</span>
                </Hint>
              </td>
              <td className="max-w-40 truncate px-3 py-1.5 text-muted-fg">{r.item}</td>
              <td className="max-w-48 truncate px-3 py-1.5">
                <Hint text={r.instance_id || r.instance}>
                  <span className="mono">{r.instance || r.instance_id || '—'}</span>
                </Hint>
              </td>
              <td className="px-3 py-1.5 whitespace-nowrap text-muted-fg">{r.region || '—'}</td>
              <td className="px-3 py-1.5 text-right whitespace-nowrap tabular-nums text-muted-fg">
                {r.usage ? `${r.usage} ${r.usage_unit}` : '—'}
              </td>
              <td className="px-3 py-1.5 text-right whitespace-nowrap tabular-nums font-medium">
                {formatMoney(r.amount)}
                {r.original > r.amount && (
                  <Hint text={`原价 ${formatMoney(r.original)}`}>
                    <span className="ml-1 text-2xs text-muted-fg line-through">{formatMoneyShort(r.original)}</span>
                  </Hint>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

/** 账期区间选择。选项仅列出库中确有账单的月份，不会选出空窗口 */
function PeriodPicker({
  known,
  from,
  to,
  onChange,
}: {
  known: string[]
  from: string
  to: string
  onChange: (next: { from_period: string; to_period: string }) => void
}) {
  // 库中尚无账期（还未同步）时给一个空壳，避免选择器无故消失
  const options = known.length ? known : [to].filter(Boolean)
  // 最近的账期排在最前：要改的多半是最近几个月，不用滚到底
  const periodOptions = [...options].reverse().map((p) => ({ value: p }))
  return (
    <span className="flex items-center gap-1 text-xs text-muted-fg">
      <Combobox
        value={from}
        onChange={(v) => onChange({ from_period: v, to_period: v > to ? v : to })}
        options={periodOptions}
        clearable={false}
        searchPlaceholder="筛账期…"
        emptyText="没有匹配的账期"
        title="起始账期"
        size="sm"
        className="w-28"
      />
      <span>至</span>
      <Combobox
        value={to}
        onChange={(v) => onChange({ from_period: v < from ? v : from, to_period: v })}
        options={periodOptions}
        clearable={false}
        searchPlaceholder="筛账期…"
        emptyText="没有匹配的账期"
        title="结束账期"
        size="sm"
        className="w-28"
      />
    </span>
  )
}
