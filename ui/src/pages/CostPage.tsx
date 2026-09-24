import { Suspense, lazy, useEffect, useMemo, useRef, useState } from 'react'
import { CloudDownloadIcon, DownloadIcon, FilterIcon, SearchIcon } from 'lucide-react'
import { apiUrl } from '@/api/client'
import { useBillBreakdown, useBillDaily, useBillDetail, useBillFacets, useBillPeriods, useBillSummary, useMeta } from '@/api/queries'
import { CostAnalysis } from '@/components/CostAnalysis'
import type { BillAmount, BillPoint, BillProvider } from '@/api/types'
import { StatsLine } from '@/components/StatsLine'
import { StackedBars } from '@/components/charts/StackedBars'
import { Badge, Button, Card, Combobox, EmptyState, ErrorBox, Hint, InfoHint, Input, Spinner, buttonClass } from '@/components/ui'
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
  formatMoneyTick,
  joinDimValues,
  periodSlots,
  periodSpan,
  periodTick,
  shiftPeriod,
  splitDimValues,
} from '@/lib/bills'
import { usePageTitle } from '@/lib/title'
import { BillDetailTable, ColumnPicker, DETAIL_COLUMNS, DayRangePicker, FACET_DIMS, detailTable, rawColumns, useDetailColumns } from '@/components/BillDetailTable'

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
  { value: 'bills', label: '账单' },
  { value: 'analysis', label: '分析' },
]
const DETAIL_PAGE = 50

/** 维度筛选在 URL 上就叫维度名本身（`product=云服务器 ECS`），和接口收的参数一致；同一维度选多个用逗号隔开 */
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
  // 手机上顶栏只露账期，其余条件收进「筛选」按钮（同日志页的做法）。按钮上的数字是偏离默认值的
  // 条件有几项：金额口径换了、选了云厂商、搜了关键字——收起时看不见它们，得有个提示说「现在的
  // 数字不是默认口径」。分析视图的日均窗口与预估账期不在这里，挂在对应的统计卡上（见 CostAnalysis）
  const [filtersOpen, setFiltersOpen] = useState(false)
  const panelCount = (amount !== 'payable' ? 1 : 0) + (provider ? 1 : 0) + (q ? 1 : 0)
  const inPanel = !filtersOpen && 'max-md:hidden'
  // 从分析视图钻取过来时，切到账单视图后把明细卷到眼前：它在页面最底下，不卷过去像是什么都没发生
  const detailRef = useRef<HTMLElement>(null)
  const scrollToDetail = useRef(false)
  useEffect(() => {
    if (scrollToDetail.current && view === 'bills' && detailRef.current) {
      scrollToDetail.current = false
      detailRef.current.scrollIntoView?.({ block: 'start', behavior: 'smooth' })
    }
  })

  // 维度筛选：URL 上的维度键原样往接口传；picked 是拆开后的各个值
  const filters = useMemo(() => {
    const out: Record<string, string> = {}
    for (const key of DIMENSION_KEYS) {
      const v = params.get(key)
      if (v) out[key] = v
    }
    return out
  }, [params])
  const picked = useMemo(() => Object.fromEntries(Object.entries(filters).map(([k, v]) => [k, splitDimValues(v)])), [filters])
  const setDim = (dim: string, values: string[]) => set({ [dim]: joinDimValues(values), page: null })

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
  // 明细可显示的列：常用字段，加上这一页所读账单表的全部原始字段。勾了的原始字段才向接口要
  const columns = useDetailColumns()
  // 明细的日期筛选（day_from / day_to）。只有日度表有日期，选了日期就按日度表出明细
  const dayFrom = params.get('day_from') ?? ''
  const dayTo = params.get('day_to') ?? ''
  const gran = dayFrom ? 'daily' : params.get('gran')
  const source = useMemo(() => (bills ? detailTable(bills, provider, gran) : null), [bills, provider, gran])
  const raw = useMemo(() => rawColumns(source), [source])
  const allColumns = useMemo(() => [...DETAIL_COLUMNS, ...raw], [raw])
  const rawCols = raw.filter((c) => columns.shown.has(c.key)).map((c) => c.raw)
  // 明细的排序：URL 上的 sort / order，不写就是按金额从大到小（与接口默认一致）。按原始字段排时
  // 那一列得是显示着的，否则接口不会带上它，退回默认
  const sortCol = allColumns.find((c) => c.sort && c.sort === params.get('sort') && (!c.raw || columns.shown.has(c.key)))
  const detailSort = { key: sortCol?.sort ?? 'amount', dir: (sortCol && params.get('order') === 'asc' ? 'asc' : 'desc') as 'asc' | 'desc' }
  const sortParams = { sort: sortCol ? detailSort.key : undefined, order: sortCol ? detailSort.dir : undefined, cols: rawCols.join(',') || undefined }
  const detailBase = {
    ...base,
    provider: provider || undefined,
    granularity: dayFrom ? undefined : params.get('gran') || undefined,
    day_from: dayFrom || undefined,
    day_to: dayTo || undefined,
  }
  const detail = useBillDetail({ ...detailBase, ...sortParams, limit: DETAIL_PAGE, offset: page * DETAIL_PAGE }, billsView)
  // 表头下拉的候选值：点开任一个下拉才去查，一次查齐所有可筛选的列
  const [wantFacets, setWantFacets] = useState(false)
  const facets = useBillFacets({ ...detailBase, dims: FACET_DIMS.join(',') }, billsView && wantFacets)

  const points = summary.data?.points ?? []
  const current = points.at(-1)
  const previous = points.at(-2)
  const mom = changeRatio(current?.total ?? 0, previous?.total ?? 0)
  const activeFilters = Object.entries(picked).flatMap(([key, values]) => values.map((value) => ({ key, value })))

  // 没部署 goscan：页签本来就不显示，直接进来的给一句原因
  if (meta.data && !bills) {
    return (
      <div className="flex min-h-0 flex-1 items-center justify-center p-6">
        <EmptyState
          title="费用页未启用"
          hint={meta.data.bills_note ?? '当前部署没有 goscan 的账单表。goscan 按账期将火山引擎、阿里云的账单同步至同一数据库，接入后本页即可显示数据。'}
        />
      </div>
    )
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-border bg-card px-3 pt-2 md:px-4">
        {/* 与指标页一致：两个视图就是这一页的导航，视觉上不再放标题；读屏仍需要一个 */}
        <h1 className="sr-only">费用</h1>
        <nav aria-label="视图" className="order-last flex w-full items-stretch gap-1 md:order-none md:w-auto">
          {VIEWS.map((v) => (
            <button
              key={v.value}
              type="button"
              aria-current={view === v.value ? 'page' : undefined}
              onClick={() => set({ view: v.value === 'bills' ? null : v.value, page: null })}
              className={cn('cf-tab flex h-9 items-center px-3 text-sm font-medium text-muted-fg hover:text-fg', view === v.value && 'text-fg')}
              data-active={view === v.value ? 'true' : undefined}
            >
              {v.label}
            </button>
          ))}
        </nav>
        {/* 筛选在前、动作在后：两个视图共用的条件，「同步账单」放在最末 */}
        {/* 手机上第一行是账期、「筛选」与「同步账单」，其余条件在「筛选」展开的面板里：金额口径与
            云厂商一行、搜索一行（靠 `max-md:order-*` 调顺序、一个整行宽的空元素断行）。桌面上照
            原顺序一行排开，没有「筛选」按钮 */}
        <span className="flex w-full flex-wrap items-center gap-2 py-2 md:ml-auto md:w-auto">
          {(summary.isFetching || periods.isFetching) && <Spinner className="size-4 shrink-0 max-md:order-1" />}
          <StatsLine stats={summary.data?.stats} className="hidden text-2xs text-muted-fg 2xl:inline" />
          {/* 换了账期，原先选的日期多半已不在范围里，一并清掉 */}
          <PeriodPicker known={known} from={from} to={to} onChange={(next) => set({ ...next, page: null, day_from: null, day_to: null })} />
          <Button
            size="sm"
            active={filtersOpen || panelCount > 0}
            onClick={() => setFiltersOpen((v) => !v)}
            className="shrink-0 px-2.5 max-md:order-3 md:hidden"
            title="金额口径 / 云厂商 / 搜索等筛选"
            aria-expanded={filtersOpen}
          >
            <FilterIcon className="size-4" />
            {panelCount > 0 && panelCount}
          </Button>
          {filtersOpen && <span aria-hidden className="h-0 basis-full max-md:order-4 md:hidden" />}
          <span className={cn('flex items-center gap-1.5 max-md:order-5', inPanel)}>
            <span role="group" aria-label="金额口径" className="flex h-8 items-center rounded-md border border-input p-0.5">
              {AMOUNTS.map((a) => (
                <button
                  key={a.value}
                  type="button"
                  aria-pressed={amount === a.value}
                  onClick={() => set({ amount: a.value === 'payable' ? null : a.value })}
                  className={cn('h-full rounded-sm px-2.5 text-xs text-muted-fg hover:text-fg', amount === a.value && 'bg-accent-soft text-accent')}
                >
                  {a.label}
                </button>
              ))}
            </span>
            <InfoHint
              text={
                <span className="flex flex-col gap-1">
                  {AMOUNTS.map((a) => (
                    <span key={a.value}>
                      <b className="font-medium">{a.label}</b>：{a.hint}
                    </span>
                  ))}
                </span>
              }
            />
          </span>
          {(bills?.providers.length ?? 0) > 1 && (
            <Combobox
              value={provider}
              onChange={(v) => set({ provider: v || null, page: null })}
              options={(bills?.providers ?? []).map((p) => ({ value: p, label: PROVIDER_LABELS[p] }))}
              placeholder="全部云厂商"
              searchPlaceholder="筛云厂商…"
              emptyText="没有匹配的云厂商"
              title="按云厂商筛选"
              size="sm"
              className={cn('w-32 max-md:order-6 max-md:w-auto max-md:min-w-0 max-md:flex-1', inPanel)}
            />
          )}
          <span className={cn('relative max-md:order-7 max-md:min-w-0 max-md:basis-full', inPanel)}>
            <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-fg" />
            <Input
              defaultValue={q}
              onKeyDown={(e) => {
                if (e.key === 'Enter') set({ q: (e.target as HTMLInputElement).value || null, page: null })
              }}
              onBlur={(e) => set({ q: e.target.value || null, page: null })}
              placeholder="搜索产品 / 实例"
              className="h-8 w-full pl-8 text-xs md:w-36"
              aria-label="搜索产品 / 计费项 / 实例名"
            />
          </span>
          {bills?.sync && (
            // 手机上只留图标、排在第一行：它是动作不是筛选，不该收进「筛选」里
            <button type="button" onClick={() => setSyncing(true)} className={buttonClass({ size: 'sm' }, 'shrink-0 max-md:order-3 max-md:px-2.5')} title="同步账单">
              <CloudDownloadIcon className="size-4" />
              <span className="max-md:sr-only">同步账单</span>
            </button>
          )}
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
            {/* 同一维度选了几个就列几项，各自可去掉 */}
            {activeFilters.map(({ key, value }) => (
              <span key={`${key}\u0000${value}`} className="inline-flex max-w-full items-center gap-1.5 rounded-md bg-accent-soft px-2.5 py-1 text-accent">
                <span className="shrink-0 text-muted-fg">{DIMENSIONS.find((d) => d.value === key)?.label ?? key}</span>
                <span className="truncate">{value}</span>
                <Hint text="移除该筛选条件" asChild>
                  <button type="button" onClick={() => setDim(key, picked[key].filter((v) => v !== value))}>
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
                ? '账单表已创建，但尚未同步任何账单。goscan 按账期定时向云厂商同步（并非实时采集），可点击右上角「同步账单」立即同步一次。'
                : '账单表已创建，但尚未同步任何账单。goscan 按账期定时向云厂商同步（并非实时采集），请确认其已运行、凭据已配置，且至少完成过一次同步。'
            }
          />
        )}

        {ready && bills && known.length > 0 && view === 'analysis' && (
          <CostAnalysis base={base} ready={ready} bills={bills} days={days} estimate={estimate} onParams={set} onFilter={setDim}
            onDrill={(t) => {
              // 跳到账单视图的明细：那一天、那朵云、那个产品。那一天不在所选账期里时，账期换成那个月
              const month = t.day.slice(0, 7)
              scrollToDetail.current = true
              set({
                view: null,
                provider: t.provider,
                product: t.product,
                day_from: t.day,
                day_to: null,
                gran: null,
                page: null,
                ...(month < from || month > to ? { from_period: month, to_period: month } : {}),
              })
            }}
          />
        )}

        {ready && known.length > 0 && view === 'bills' && (
          <div className="flex flex-col gap-3 md:gap-4">
            <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
              <StatCard
                label={`${current?.t ?? to} 费用`}
                value={formatMoney(current?.total ?? 0)}
                stale={summary.isFetching}
              />
              <StatCard label={`${previous?.t ?? '上一账期'} 费用`} value={formatMoney(previous?.total ?? 0)} stale={summary.isFetching} />
              <StatCard
                label="环比"
                value={formatChange(mom)}
                tone={mom === null ? 'muted' : mom > 0 ? 'danger' : 'ok'}
                hint="以最后一个账期与上一账期相比；当月账单尚未出齐时，该值会偏低"
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

            {/* 宽屏上两张图并排：账期只有寥寥几根柱子，独占一整行时柱体被拉得过宽；按天的点多，分到更宽的一侧 */}
            <div className="grid gap-3 md:gap-4 xl:grid-cols-5">
              <Card title="按账期" className={dailyProviders.length > 0 ? 'xl:col-span-2' : 'xl:col-span-5'} extra={<span className="text-2xs text-muted-fg">点击柱体可筛选至该账期</span>}>
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
                  className="xl:col-span-3"
                  extra={
                    <span className="text-2xs text-muted-fg">
                      {dailyProviders.map((p) => PROVIDER_LABELS[p]).join(' / ')}
                      {dailyProviders.length < (bills?.providers.length ?? 0) && '（另一云厂商未同步日度账单）'}
                    </span>
                  }
                >
                  <DailyChart points={daily.data?.points ?? []} providers={dailyProviders} stale={daily.isFetching} />
                </Card>
              )}
            </div>

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
                selected={picked[by] ?? []}
                // 点一项就只看它；已在筛选里的再点一次是把它去掉。要同时看几项，在明细的表头里多选
                onPick={(key) => {
                  const cur = picked[by] ?? []
                  setDim(by, cur.includes(key) ? cur.filter((v) => v !== key) : [key])
                }}
              />
            </Card>

            <Card
              ref={detailRef}
              title="明细"
              extra={
                // 手机上放不下一行时换行；那行说明不许折，否则会被控件挤成一字一行
                <span className="flex flex-wrap items-center justify-end gap-2">
                  {detail.data && (
                    <span className="text-2xs whitespace-nowrap text-muted-fg">
                      {PROVIDER_LABELS[detail.data.provider]}
                      {detail.data.granularity === 'daily' ? ' · 日度' : ' · 月度'}
                      {detail.data.total !== null && ` · ${detail.data.total.toLocaleString('zh-CN')} 行`}
                    </span>
                  )}
                  <DayRangePicker fromPeriod={from} toPeriod={to} dayFrom={dayFrom} dayTo={dayTo} onChange={(next) => set({ ...next, page: null })} />
                  {/* 选了日期时只能看日度表，粒度就不必再选 */}
                  {!dayFrom && bills?.alicloud_daily && bills.alicloud_monthly && (!provider || provider === 'alicloud') && (
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
                  <ColumnPicker shown={columns.shown} raw={raw} table={source?.meta.table} onToggle={columns.toggle} onReset={columns.reset} onSetMany={columns.setMany} />
                  <a href={apiUrl('/bills/export', { ...detailBase, ...sortParams, format: 'csv' })} className={buttonClass({ size: 'sm' })} download>
                    <DownloadIcon className="size-4" />
                    导出 CSV
                  </a>
                </span>
              }
            >
              {detail.isError && <ErrorBox error={detail.error} onRetry={() => detail.refetch()} />}
              <BillDetailTable
                rows={detail.data?.rows ?? []}
                pending={detail.isPending}
                stale={detail.isFetching}
                columns={allColumns}
                shown={columns.shown}
                sort={detailSort}
                onSort={(key) => {
                  // 同一列再点一次换方向；换列时数字与日期先看大的、文字先看 A–Z
                  const col = allColumns.find((c) => c.sort === key)
                  const dir = detailSort.key === key ? (detailSort.dir === 'desc' ? 'asc' : 'desc') : col?.numeric || key === 'day' ? 'desc' : 'asc'
                  const isDefault = key === 'amount' && dir === 'desc'
                  set({ sort: isDefault ? null : key, order: isDefault ? null : dir, page: null })
                }}
                filters={Object.fromEntries(
                  FACET_DIMS.map((dim) => [
                    dim,
                    {
                      multiple: true,
                      value: picked[dim] ?? [],
                      options: (facets.data?.facets[dim] ?? []).map((v) => ({ value: v })),
                      onChange: (values: string[]) => setDim(dim, values),
                      onOpen: () => setWantFacets(true),
                      loading: facets.isFetching,
                    },
                  ]),
                )}
                amountLabel={AMOUNTS.find((a) => a.value === amount)?.label ?? '应付'}
              />
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
  return (
    <div className={cn('rounded-lg border border-border bg-card px-4 py-3', stale && 'opacity-60 transition-opacity')}>
      <div className="text-xs text-muted-fg">{hint ? <InfoHint text={hint}>{label}</InfoHint> : label}</div>
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
}

/**
 * 账期柱状图。复用按时间分桶的 `StackedBars`，账期经 `periodSlots` 排成等宽的桶
 * （自然月长短不一，按真实时间摆会偏），横轴标「4月」而非日期。
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
  const slots = useMemo(() => periodSlots(points.map((p) => p.t)), [points])
  const buckets = useMemo(
    () => points.map((p, i) => ({ t_ms: slots.at(i), values: Object.fromEntries(providers.map((k) => [k, p.by_provider[k] ?? 0])) })),
    [points, providers, slots],
  )
  const periodAt = (tMs: number) => points[slots.index(tMs)]?.t ?? ''
  return (
    <div className="px-2 py-3">
      <StackedBars
        fromMs={slots.fromMs}
        toMs={slots.toMs}
        widthMs={slots.widthMs}
        buckets={buckets}
        series={providers.map((p) => ({ key: p, label: PROVIDER_LABELS[p], color: PROVIDER_COLORS[p] }))}
        format={formatMoneyTick}
        valueFormat={formatMoney}
        xTicks={points.map((p, i) => ({ t_ms: slots.at(i), label: periodTick(p.t) }))}
        bucketTitle={periodAt}
        onPointClick={({ tMs }) => {
          const period = periodAt(tMs)
          if (period) onPick(period)
        }}
        stale={stale}
        label="按账期的费用"
        height={190}
      />
      <div className="flex flex-wrap items-center gap-3 px-2 text-2xs text-muted-fg">
        {providers.map((p) => (
          <span key={p} className="flex items-center gap-1.5">
            <span className="size-2 rounded-[2px]" style={{ background: PROVIDER_COLORS[p] }} />
            {PROVIDER_LABELS[p]}
          </span>
        ))}
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
    return <div className="px-4 py-8 text-center text-xs text-muted-fg">所选账期内没有日度账单</div>
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
        format={formatMoneyTick}
        valueFormat={formatMoney}
        bucketTitle={(t) => points[buckets.findIndex((b) => b.t_ms === t)]?.t ?? ''}
        stale={stale}
        label="按天的费用"
        // 与并排的「按账期」等高，免得右侧那张卡片底部空出一截
        height={190}
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
  selected: string[]
  onPick: (key: string) => void
}) {
  if (pending) {
    return (
      <div className="flex justify-center py-10">
        <Spinner />
      </div>
    )
  }
  if (!rows.length) return <div className="px-4 py-8 text-center text-xs text-muted-fg">所选账期内没有账单</div>
  return (
    <div className={cn(stale && 'opacity-60 transition-opacity')}>
      {/* 一项一行、宽屏分两栏：原先一项占两行，十五项就撑出半屏 */}
      <ul className="grid lg:grid-flow-col lg:grid-cols-2 lg:gap-x-4" style={{ gridTemplateRows: `repeat(${Math.ceil(rows.length / 2)}, auto)` }}>
        {rows.map((r, i) => (
          <li key={r.key} className="min-w-0">
            <button
              type="button"
              onClick={() => onPick(r.key)}
              aria-pressed={selected.includes(r.key)}
              className={cn('row-hover flex w-full items-center gap-3 px-3 py-1 text-left text-xs', selected.includes(r.key) && 'bg-accent-soft/50')}
            >
              <span className="w-5 shrink-0 text-right text-2xs text-muted-fg tabular-nums">{i + 1}</span>
              <span className="flex min-w-0 flex-1 items-baseline gap-2">
                <span className="truncate">{r.key}</span>
                {Object.keys(r.by_provider).length > 1 && <Badge tone="muted">两云均有</Badge>}
              </span>
              {/* 占比条：长度为占总额的比例，一眼可见哪几项占去大半 */}
              <span aria-hidden className="hidden h-1.5 w-20 shrink-0 rounded-full bg-muted sm:block">
                <span className="block h-full rounded-full bg-brand" style={{ width: `${Math.min(100, r.share * 100)}%` }} />
              </span>
              <span className="w-12 shrink-0 text-right text-2xs text-muted-fg tabular-nums">{(r.share * 100).toFixed(1)}%</span>
              <span className="w-24 shrink-0 text-right font-semibold tabular-nums">{formatMoney(r.amount)}</span>
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
    <span className="flex items-center gap-1 text-xs text-muted-fg max-md:order-2 max-md:min-w-0 max-md:flex-[2]">
      <Combobox
        value={from}
        onChange={(v) => onChange({ from_period: v, to_period: v > to ? v : to })}
        options={periodOptions}
        clearable={false}
        searchPlaceholder="筛账期…"
        emptyText="没有匹配的账期"
        title="起始账期"
        size="sm"
        className="w-28 max-md:w-auto max-md:min-w-0 max-md:flex-1"
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
        className="w-28 max-md:w-auto max-md:min-w-0 max-md:flex-1"
      />
    </span>
  )
}
