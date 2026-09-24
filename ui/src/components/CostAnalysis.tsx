/**
 * 费用页的「分析」视图：把账单摊到业务线，并按日均推算月度预估。
 *
 * 与「账单」视图的分工：那边回答「这笔钱花在哪个产品上」，照着账单本身的维度看；这边回答
 * 「这笔钱该记在哪条业务线头上、照这个花法这个月要花多少」，依据是部署方给的归属规则
 * （后端 `--bill-alloc`）。没配规则时业务线那一层为空，日均与按产品的预估照常可看。
 *
 * **日均的分母是「有账单的天数」，不是自然月的天数**：当月账单尚未出齐，按 30 天摊只会把
 * 日均算低，越到月初越离谱。月度预估则反过来乘以目标月的自然天数（28 至 31 天不等）。
 *
 * 预付费（包年包月）不走日均这条路：它在购买当月一次性出账，由后端按服务期摊到各月，页面上
 * 单独列出。月度预估因此是两段相加——后付费的日均 × 天数，加上那个月的摊销额；摊到未来几个月
 * 的摊销是已经发生的购买摊过来的，所以下个月的预估里它是已知项，不必估。
 *
 * 页尾的「产品费用对比」补上日均看不到的一面：某一天（或近几天）各产品比之前多花或少花了多少。
 */
import { Fragment, useMemo, useState } from 'react'
import { CalendarDaysIcon, ChevronRightIcon, CircleAlertIcon, DownloadIcon, TrendingUpIcon, WalletIcon, type LucideIcon } from 'lucide-react'
import { useBillAllocation, useBillProductDays } from '@/api/queries'
import type { BillAllocItem, BillAllocLine, BillAllocPoint, BillAllocationResponse, BillProductDaysResponse, BillsMeta } from '@/api/types'
import { Card, Combobox, EmptyState, ErrorBox, Hint, InfoHint, Spinner, buttonClass } from '@/components/ui'
import { LineChart, Legend, type LineSeries } from '@/components/charts/LineChart'
import { StackedBars } from '@/components/charts/StackedBars'
import { AMOUNTS, PROVIDER_LABELS, changeRatio, daysInMonth, dayTick, formatChange, formatMoney, formatMoneyShort, formatMoneyTick, periodSlots, periodSpan, periodTick, shiftPeriod } from '@/lib/bills'
import { allocSection, allocSections, allocSummary, currentLabel, thisPeriod, type SectionKey } from '@/lib/allocTable'
import { seriesVar } from '@/lib/colors'
import { COMPARE_MODES, compare, defaultEnd, endOptions, trend, type CompareMode, type Comparison, type ProductDiff, type Range } from '@/lib/productDays'
import { cn } from '@/lib/utils'

/**
 * 「未归属」在列表展开状态、构成图里用的内部键。不直接用「未归属」三个字：部署方完全可能
 * 把某条业务线也叫这个名字，两者便撞在一起
 */
const UNMATCHED = '\u0000unmatched'
/** 未归属统一用中性灰，与各业务线的颜色明显区分，一眼看出这部分还没有着落 */
const UNMATCHED_COLOR = 'color-mix(in srgb, var(--muted-fg) 55%, transparent)'

/**
 * 业务线的颜色按它在**配置里的位置**取，拆分表与构成图共用：接口返回的 lines 会略去空的业务线，
 * 若按那份列表的下标取色，某条线某月恰好为空时，排在它后面的线就全部换了颜色
 */
const lineColor = (order: string[], name: string) => seriesVar(Math.max(0, order.indexOf(name)))

/** 「按产品」默认列出的项数 */
const PRODUCT_TOP = 15

/** 日均按多长的窗口算。`all` = 所选账期全部（URL 上不写 `days`） */
const WINDOWS = [
  { value: 'all', label: '所选账期' },
  { value: '7', label: '最近 7 天' },
  { value: '14', label: '最近 14 天' },
  { value: '30', label: '最近 30 天' },
]

export function CostAnalysis({
  base,
  ready,
  bills,
  days,
  estimate,
}: {
  /** 与「账单」视图共用的查询条件：账期、金额口径、云、搜索、维度筛选 */
  base: Record<string, string | undefined>
  ready: boolean
  bills: BillsMeta
  /** 日均的窗口（`days` 参数），空串表示所选账期全部 */
  days: string
  /** 预估哪个月，`YYYY-MM`。两项口径的选择器在页头，见 `AnalysisControls` */
  estimate: string
}) {
  const params = useMemo(() => ({ ...base, days: days || undefined }), [base, days])
  const alloc = useBillAllocation(params, ready)
  const data = alloc.data
  // 构成图里单独查看的那条业务线；与列表的展开各管各的，互不牵动
  const [focus, setFocus] = useState<string | null>(null)
  const [allProducts, setAllProducts] = useState(false)
  const products = data?.products ?? []

  const nights = daysInMonth(estimate)
  /**
   * 月度预估 = 后付费日均 × 目标月天数 + 该月的预付费摊销。
   *
   * 两段的口径不同，所以分开算：前一段是「照这个花法能花多少」，后一段是「已经买下的东西
   * 摊到这个月多少」，后者不是估出来的。没有日粒度的账单、又没有摊销时给不出预估。
   */
  const project = (daily: number | null, byPeriod?: Record<string, number>) => {
    const amortized = byPeriod?.[estimate] ?? 0
    if (daily === null && !amortized) return null
    return (daily ?? 0) * nights + amortized
  }
  const lines = data?.lines ?? []
  const prepaid = data?.prepaid ?? false

  return (
    <div className="flex flex-col gap-3 md:gap-4">
      {alloc.isError && <ErrorBox error={alloc.error} onRetry={() => alloc.refetch()} />}

      {/* 日度账单只补了几天时，区间合计与各业务线金额都会偏低，而数字本身看不出来 */}
      {data?.coverage && (
        <div role="alert" className="rounded-lg border border-warn/40 bg-warn-soft px-4 py-3 text-xs leading-5 text-fg">
          <div className="font-medium text-warn">{PROVIDER_LABELS[data.coverage.provider]}的日度账单不完整</div>
          <div className="mt-1 text-muted-fg">
            所选账期内，日度账单合计 {formatMoney(data.coverage.daily)}，月度账单合计 {formatMoney(data.coverage.monthly)}，
            日度仅覆盖其 {((data.coverage.daily / data.coverage.monthly) * 100).toFixed(1)}%。日均依现有的
            {data.days_by_provider[data.coverage.provider] ?? 0} 天账单求得，仍可参考；所选账期合计与各业务线金额则明显偏低。
            {bills.sync ? '可点击右上角「同步账单」，以日度粒度补齐这些账期。' : '请在 goscan 中以日度粒度补齐这些账期。'}
          </div>
        </div>
      )}

      <div className={cn('grid grid-cols-2 gap-3', bills.allocation ? 'lg:grid-cols-4' : 'lg:grid-cols-3')}>
        <Stat
          icon={WalletIcon}
          iconTone="brand"
          label={data?.window_days ? `最近 ${data.window_days} 天合计` : `${periodSpan(data?.from ?? base.from ?? '', data?.to ?? base.to ?? '')} 个账期合计`}
          value={formatMoney(data?.total ?? 0)}
          extra={
            prepaid ? (
              <span className="mt-1 block text-2xs text-muted-fg">
                后付费 {formatMoneyShort(data?.postpaid ?? 0)} · 预付费摊销 {formatMoneyShort(data?.amortized ?? 0)}
              </span>
            ) : undefined
          }
        />
        <Stat
          icon={CalendarDaysIcon}
          iconTone="accent"
          label="日均"
          value={data?.daily === null || data?.daily === undefined ? '—' : formatMoney(data.daily)}
        />
        <Stat
          icon={TrendingUpIcon}
          iconTone="accent"
          label={`${estimate} 预估`}
          value={
            project(data?.daily ?? null, data?.amortized_by_period) === null
              ? '—'
              : formatMoney(project(data?.daily ?? null, data?.amortized_by_period) as number)
          }
          extra={
            prepaid ? (
              <span className="mt-1 block text-2xs text-muted-fg">
                其中预付费摊销 {formatMoneyShort(data?.amortized_by_period?.[estimate] ?? 0)}
              </span>
            ) : undefined
          }
        />
        {/* 没配归属规则时，「未归属」恒等于总额，摆出来只是重复一遍 */}
        {bills.allocation && (
        <Stat
          icon={CircleAlertIcon}
          iconTone={(data?.unmatched.share ?? 0) > 0.2 ? 'warn' : 'muted'}
          label="未归属"
          value={formatMoney(data?.unmatched.amount ?? 0)}
          tone={(data?.unmatched.share ?? 0) > 0.2 ? 'warn' : 'plain'}
          extra={
            <span className="mt-1 block text-2xs text-muted-fg">
              占 {((data?.unmatched.share ?? 0) * 100).toFixed(1)}%
            </span>
          }
        />
        )}
      </div>

      {alloc.isPending ? (
        <Card title="按月拆分">
          <div className="flex justify-center py-10">
            <Spinner />
          </div>
        </Card>
      ) : !bills.allocation ? (
        <Card title="按月拆分" extra={<span className="text-2xs text-muted-fg">未配置归属规则</span>}>
          <EmptyState
            title="尚未配置成本归属规则"
            hint={
              <>
                账单只记录产品与实例；机器归属哪条业务线、共用服务按何比例分摊，须由部署方给出。
                以启动参数 <code className="mono">--bill-alloc rules.toml</code> 指向一份规则文件即可，
                写法参见仓库中的 <code className="mono">examples/bill-alloc.toml</code>。
                在此之前，下方「按产品」的日均与月度预估仍可照常使用。
              </>
            }
          />
        </Card>
      ) : data ? (
        <>
          <MonthlySplit
            data={data}
            order={bills.allocation.lines}
            rules={bills.allocation.rules}
            nights={nights}
            estimate={estimate}
            amount={AMOUNTS.find((a) => a.value === (base.amount ?? 'payable')) ?? AMOUNTS[0]}
            windowLabel={WINDOWS.find((w) => w.value === (days || 'all'))?.label ?? '所选账期'}
            stale={alloc.isFetching}
          />
          {data.monthly.length > 0 && <MonthlySummary data={data} stale={alloc.isFetching} />}
        </>
      ) : null}

      {(data?.points.length ?? 0) > 0 && lines.length > 0 && (
        <Card
          title={data?.granularity === 'daily' ? '按天的业务线构成' : '按账期的业务线构成'}
          extra={
            <span className="text-2xs text-muted-fg">
              {data?.points.length} {data?.granularity === 'daily' ? '天' : '个账期'}
              {prepaid && ' · 仅含后付费'}
            </span>
          }
        >
          <LineBars
            points={data?.points ?? []}
            lines={lines}
            order={bills.allocation?.lines ?? lines.map((l) => l.name)}
            // 已并入某条业务线时它就在那条线的柱段里，再画一段便重复了
            unmatched={data?.unmatched_into ? undefined : data?.unmatched}
            absorbed={data?.unmatched_into ? { into: data.unmatched_into, amount: data.unmatched.postpaid } : undefined}
            unit={data?.granularity === 'daily' ? '天' : '账期'}
            nights={nights}
            estimate={estimate}
            focus={focus === UNMATCHED || lines.some((l) => l.name === focus) ? focus : null}
            onFocus={setFocus}
            stale={alloc.isFetching}
          />
        </Card>
      )}

      <Card title="按产品" extra={<span className="text-2xs text-muted-fg">不区分业务线，与账单口径一致</span>}>
        {alloc.isPending ? (
          <div className="flex justify-center py-10">
            <Spinner />
          </div>
        ) : (
          <>
            {/* 七十来个产品一口气铺开，要找的那几个反被淹没；默认只列金额最大的一批 */}
            <ItemTable items={allProducts ? products : products.slice(0, PRODUCT_TOP)} nights={nights} max={Math.max(0, ...products.map((p) => p.amount))} />
            {products.length > PRODUCT_TOP && (
              <button
                type="button"
                onClick={() => setAllProducts(!allProducts)}
                className="w-full border-t border-border px-3 py-2 text-center text-xs text-accent hover:bg-muted/40"
              >
                {allProducts ? `收起，仅显示前 ${PRODUCT_TOP} 项` : `展开其余 ${products.length - PRODUCT_TOP} 项`}
              </button>
            )}
          </>
        )}
      </Card>

      <ProductCompare base={base} ready={ready} prepaid={prepaid} />
    </div>
  )
}

/**
 * 分析视图独有的两项口径：日均按多长的窗口算、预估哪个月。放在页头与共用筛选排在一起，
 * 而不在正文里另起一行——它们与账期、金额口径一样是「怎么算」的条件，不是某一块内容的附属。
 *
 * 与 `CostAnalysis` 使用同一组查询参数，react-query 按键去重，不会多发一次请求。
 */
export function AnalysisControls({
  base,
  ready,
  days,
  estimate,
  onChange,
}: {
  base: Record<string, string | undefined>
  ready: boolean
  days: string
  estimate: string
  onChange: (next: Record<string, string | null>) => void
}) {
  const params = useMemo(() => ({ ...base, days: days || undefined }), [base, days])
  const alloc = useBillAllocation(params, ready)
  const prepaid = alloc.data?.prepaid ?? false
  return (
    <span className="flex flex-wrap items-center gap-2">
      {/* 标签与下拉成组：窄屏换行时不把「预估」和它的下拉拆到两行 */}
      <span className="flex items-center gap-2">
        <InfoHint text="日均 = 统计窗口内的费用 ÷ 有账单的天数。选择「最近 7 天」可排除月初扩容等早期波动，更接近当前水平" className="text-xs text-muted-fg">
          日均口径
        </InfoHint>
        <Combobox
          value={days || 'all'}
          onChange={(v) => onChange({ days: v === 'all' ? null : v })}
          options={WINDOWS}
          clearable={false}
          searchPlaceholder="筛口径…"
          title="日均的统计窗口"
          size="sm"
          className="w-28"
        />
      </span>
      <span className="flex items-center gap-2">
        <InfoHint
          text={
            prepaid
              ? '月度预估 = 后付费日均 × 该账期的自然天数 + 该账期的预付费摊销。前者以用量不变为前提，不计入扩容与活动；后者来自已发生的购买，属确定金额'
              : '月度预估 = 日均 × 该账期的自然天数。以用量不变为前提，不计入扩容、活动与预付费（包年包月）的一次性支出'
          }
          className="text-xs text-muted-fg"
        >
          预估
        </InfoHint>
        <Combobox
          value={estimate}
          onChange={(v) => onChange({ est: v })}
          options={[base.to ?? '', shiftPeriod(base.to ?? '', 1)]
            .filter(Boolean)
            .map((p) => ({ value: p, label: `${p}（${daysInMonth(p)} 天）` }))}
          clearable={false}
          searchPlaceholder="筛账期…"
          title="预估账期"
          size="sm"
          className="w-40"
        />
      </span>
      {alloc.isFetching && <Spinner className="size-4" />}
    </span>
  )
}

/** 统计卡片左上角图标的底色：品牌色标「花了多少」，强调色标「按天、往后」，警示色只给占比偏高的未归属 */
const STAT_TONES = {
  brand: 'bg-[color-mix(in_srgb,var(--brand)_14%,transparent)] text-brand',
  accent: 'bg-accent-soft text-accent',
  warn: 'bg-warn-soft text-warn',
  muted: 'bg-muted text-muted-fg',
} as const

function Stat({
  label,
  value,
  icon: Icon,
  iconTone,
  tone = 'plain',
  extra,
}: {
  label: string
  value: string
  icon: LucideIcon
  iconTone: keyof typeof STAT_TONES
  tone?: 'plain' | 'warn'
  extra?: React.ReactNode
}) {
  return (
    <div className="flex gap-3 rounded-lg border border-border bg-card px-4 py-3">
      <span aria-hidden className={cn('flex size-9 shrink-0 items-center justify-center rounded-md', STAT_TONES[iconTone])}>
        <Icon className="size-[18px]" />
      </span>
      <div className="min-w-0">
        <div className="text-xs text-muted-fg">{label}</div>
        <div className={cn('mt-0.5 text-xl font-semibold tabular-nums', tone === 'warn' && 'text-danger')}>{value}</div>
        {extra}
      </div>
    </div>
  )
}

/**
 * 金额格子的底色：单一蓝色由浅到深，按本表月份格子里的最大值折算，深的就是花得多的那条线、
 * 那个月。只动底色、字仍是正文色；上限 26% 保证深色格子上的字依旧清楚，浅色深色主题都一样
 */
const heat = (value: number, max: number) =>
  value > 0 && max > 0 ? `color-mix(in srgb, var(--accent) ${Math.round(4 + (value / max) * 22)}%, transparent)` : undefined

/** 合计行、小计行的底色，对应财务表里的黄底：取品牌橙的浅色，与表身明显区分 */
const TOTAL_BG = 'bg-[color-mix(in_srgb,var(--brand)_12%,var(--card))]'
const SUBTOTAL_BG = 'bg-[color-mix(in_srgb,var(--brand)_6%,var(--card))]'
/** 预估两列的底色：与实绩分开，一眼看出哪几列是推算出来的 */
const ESTIMATE_BG = 'bg-[color-mix(in_srgb,var(--accent)_6%,var(--card))]'

/** 业务线的色块，与构成图的图例同色 */
function Swatch({ color }: { color: string }) {
  return <span aria-hidden className="size-2.5 shrink-0 rounded-[3px]" style={{ background: color }} />
}

/** 金额单元格：0 显示为「—」，一眼分得出「没有花费」与「花了钱」 */
function Money({ value, strong }: { value: number; strong?: boolean }) {
  if (!value) return <span className="text-muted-fg/60">—</span>
  return <span className={cn(strong && 'font-semibold')}>{formatMoney(value)}</span>
}

/**
 * 按月拆分：业务线 × 月份的矩阵，照财务那张《月度费用项目拆分表》的样子排。
 *
 * 一行一条业务线、一列一个账期，末列小计与占比，末行合计。按云、按付费方式分段，用页签切换：
 * 默认看合计（两朵云、后付费与预付费摊销相加），也可只看其中一段与财务表逐格核对。合计页签
 * 再附上日均与月度预估两列，并可点开某条业务线，查看它由哪些产品、按哪条规则构成。
 *
 * 按整月统计，不受「日均口径」影响：那个选项只决定日均按多长的窗口算。
 */
function MonthlySplit({
  data,
  order,
  rules,
  nights,
  estimate,
  amount,
  windowLabel,
  stale,
}: {
  data: BillAllocationResponse
  /** 业务线的先后，取配置里的顺序：与财务表的行序一致，也不随金额大小跳动 */
  order: string[]
  rules: number
  nights: number
  estimate: string
  /** 当前金额口径，写进导出文件的标题与口径说明 */
  amount: { label: string; hint: string }
  /** 日均的统计窗口（「所选账期」「最近 7 天」……），同上 */
  windowLabel: string
  stale?: boolean
}) {
  const [tab, setTab] = useState<SectionKey>('all')
  const [exporting, setExporting] = useState(false)
  const [exportError, setExportError] = useState<string | null>(null)
  const onExport = async () => {
    setExporting(true)
    setExportError(null)
    try {
      // 写 xlsx 的那一坨只在点的时候才加载
      const { exportAllocXlsx } = await import('@/lib/allocXlsx')
      await exportAllocXlsx({ data, order, nights, estimate, amountLabel: amount.label, amountHint: amount.hint, windowLabel })
    } catch (e) {
      setExportError(`导出失败：${(e as Error).message}`)
    } finally {
      setExporting(false)
    }
  }
  const [open, setOpen] = useState<string | null>(null)
  const current = thisPeriod()
  const sections = allocSections(data)
  const active = sections.some((x) => x.key === tab) ? tab : 'all'
  const section = allocSection(data, order, active, nights, estimate)
  const { periods, total, cellMax, footDaily, footProject } = section
  // 展开看产品只在合计页签：产品明细是两朵云、两种付费方式合在一起的，拆不到单独一段
  const canExpand = active === 'all'
  const colSpan = 1 + periods.length + 2 + 2

  return (
    <Card
      title="按月拆分"
      extra={
        <span className="flex items-center gap-2">
          <span className="hidden text-2xs text-muted-fg lg:inline">{rules} 条归属规则</span>
          {sections.length > 1 && (
            <span role="group" aria-label="拆分口径" className="flex h-7 items-center rounded-md border border-input p-0.5">
              {sections.map((x) => (
                <button
                  key={x.key}
                  type="button"
                  aria-pressed={active === x.key}
                  onClick={() => setTab(x.key)}
                  className={cn('h-full rounded-sm px-2 text-xs whitespace-nowrap text-muted-fg hover:text-fg', active === x.key && 'bg-accent-soft text-accent')}
                >
                  {x.label}
                </button>
              ))}
            </span>
          )}
          {/* 导出的是全部分段与汇总，不只当前页签：财务要的是整张表 */}
          <button type="button" onClick={onExport} disabled={exporting || stale} className={buttonClass({ size: 'xs' })}>
            <DownloadIcon className="size-3.5" />
            {exporting ? '正在导出…' : '导出 Excel'}
          </button>
        </span>
      }
    >
      {exportError && <p className="border-b border-border bg-danger-soft px-4 py-2 text-xs text-danger">{exportError}</p>}
      <div className={cn('overflow-x-auto', stale && 'opacity-60 transition-opacity')}>
        <table className="w-full text-xs tabular-nums">
          <thead className="bg-muted/50 text-2xs text-muted-fg">
            <tr className="border-b border-border">
              <th className="sticky left-0 z-10 min-w-40 bg-muted px-3 py-2 text-left font-medium">业务线</th>
              {periods.map((p) => (
                <th key={p} className="min-w-24 px-3 py-2 text-right font-medium whitespace-nowrap">
                  {periodTick(p)}
                  {p === current && <span className="ml-1 font-normal text-warn">{currentLabel(data, p)}</span>}
                </th>
              ))}
              <th className="min-w-28 border-l border-border px-3 py-2 text-right font-medium">小计</th>
              <th className="min-w-16 px-3 py-2 text-right font-medium">占比</th>
              <th className="min-w-24 border-l border-dashed border-border bg-accent-soft px-3 py-2 text-right font-medium text-accent">日均</th>
              <th className="min-w-28 bg-accent-soft px-3 py-2 text-right font-medium whitespace-nowrap text-accent">{estimate} 预估</th>
            </tr>
          </thead>
          <tbody>
            {section.rows.map((row) => {
              const k = row.key
              const m = row.byPeriod
              const sub = row.subtotal
              const name = k ?? '未归属'
              const { line, projected, daily } = row
              const isOpen = canExpand && open === name
              const expandable = canExpand && !!line && line.items.length > 0
              return (
                <Fragment key={name}>
                  {/* 整行都能点开；业务线名那个按钮留给键盘与读屏，它的点击冒泡到行上，不另绑一次 */}
                  <tr
                    onClick={expandable ? () => setOpen(isOpen ? null : name) : undefined}
                    className={cn('row-hover border-b border-border/60', expandable && 'cursor-pointer')}
                  >
                    <th scope="row" className="sticky left-0 z-10 bg-card px-3 py-2 text-left font-normal">
                      {expandable ? (
                        <button type="button" aria-expanded={isOpen} className="flex items-center gap-1.5 text-left">
                          <ChevronRightIcon className={cn('size-3.5 shrink-0 text-muted-fg transition-transform', isOpen && 'rotate-90')} />
                          <Swatch color={k === null ? UNMATCHED_COLOR : lineColor(order, k)} />
                          <span className={cn('font-medium', k === null && 'text-muted-fg')}>{name}</span>
                        </button>
                      ) : (
                        <span className={cn('inline-flex items-center gap-1.5', canExpand && 'pl-5')}>
                          <Swatch color={k === null ? UNMATCHED_COLOR : lineColor(order, k)} />
                          <span className={cn('font-medium', k === null && 'text-muted-fg')}>{name}</span>
                        </span>
                      )}
                      {k === null && (
                        <Hint text="未命中任何归属规则的费用，不计入任何业务线。展开可查看涉及的产品，据此补充规则" asChild>
                          <span className="ml-2 rounded bg-warn-soft px-1 text-2xs text-warn">未命中规则</span>
                        </Hint>
                      )}
                    </th>
                    {periods.map((p) => (
                      <td key={p} className="px-3 py-2 text-right whitespace-nowrap" style={{ background: heat(m?.[p] ?? 0, cellMax) }}>
                        <Money value={m?.[p] ?? 0} />
                      </td>
                    ))}
                    <td className="border-l border-border px-3 py-2 text-right whitespace-nowrap">
                      <Money value={sub} strong />
                    </td>
                    <td className="px-3 py-2 text-right text-muted-fg">
                      {total ? (
                        <span className="inline-flex items-center justify-end gap-2">
                          {/* 占比条用这条线自己的颜色，与构成图对得上 */}
                          <span className="hidden h-1.5 w-20 overflow-hidden rounded-full bg-muted sm:block">
                            <span
                              className="block h-full rounded-full"
                              style={{ width: `${Math.min(100, (sub / total) * 100)}%`, background: k === null ? UNMATCHED_COLOR : lineColor(order, k) }}
                            />
                          </span>
                          {/* 百分比定宽：「9.5%」比「15.0%」窄，不定宽的话右对齐会把左边的进度条推得参差不齐 */}
                          <span className="w-12 text-right">{((sub / total) * 100).toFixed(1)}%</span>
                        </span>
                      ) : (
                        '—'
                      )}
                    </td>
                    <td className={cn('border-l border-dashed border-border px-3 py-2 text-right whitespace-nowrap text-muted-fg', ESTIMATE_BG)}>
                      {daily === null ? '—' : formatMoney(daily)}
                    </td>
                    <td className={cn('px-3 py-2 text-right font-medium whitespace-nowrap', ESTIMATE_BG)}>{projected === null ? '—' : formatMoney(projected)}</td>
                  </tr>
                  {isOpen && line && (
                    <tr className="border-b border-border/60 bg-muted/20">
                      <td colSpan={colSpan} className="px-3 py-1">
                        <ItemTable items={line.items} nights={nights} compact />
                      </td>
                    </tr>
                  )}
                </Fragment>
              )
            })}
          </tbody>
          <tfoot>
            <tr className={cn('border-t-2 border-[color-mix(in_srgb,var(--brand)_45%,var(--border))] font-semibold', TOTAL_BG)}>
              <th scope="row" className={cn('sticky left-0 z-10 px-3 py-2 text-left', TOTAL_BG)}>
                <span className={cn(canExpand && 'pl-5')}>{sections.find((x) => x.key === active)?.label ?? '合计'}</span>
              </th>
              {periods.map((p) => (
                <td key={p} className="px-3 py-2 text-right whitespace-nowrap">
                  <Money value={section.colTotals[p]} />
                </td>
              ))}
              <td className="border-l border-border px-3 py-2 text-right whitespace-nowrap">
                <Money value={total} />
              </td>
              <td className="px-3 py-2 text-right">{total ? '100%' : '—'}</td>
              <td className="border-l border-dashed border-border px-3 py-2 text-right whitespace-nowrap text-accent">
                {footDaily === null ? '—' : formatMoney(footDaily)}
              </td>
              <td className="px-3 py-2 text-right whitespace-nowrap text-accent">{footProject === null ? '—' : formatMoney(footProject)}</td>
            </tr>
          </tfoot>
        </table>
      </div>
      {/* 口径说明：与财务表表头那几行说明同一个作用，数字怎么来的写在数字旁边 */}
      <ul className="space-y-0.5 border-t border-border px-4 py-2.5 text-2xs leading-5 text-muted-fg">
        <li className="flex items-center gap-2">
          <span aria-hidden className="flex h-2 w-16 overflow-hidden rounded-full">
            {[4, 11, 18, 26].map((pct) => (
              <span key={pct} className="flex-1" style={{ background: `color-mix(in srgb, var(--accent) ${pct}%, transparent)` }} />
            ))}
          </span>
          格子底色越深，金额越高；深浅按本表各格中的最大值折算。
        </li>
        <li>后付费计入出账所在的账期；预付费（包年包月）按服务期摊入各账期，未配置预付费规则时同样计入出账所在的账期。</li>
        <li>本表按整月统计，不受「日均口径」影响。分段页签中，后付费的预估为该云厂商的日均 × 天数，预付费摊销的预估为摊入该账期的金额。</li>
        {data.unmatched_into && data.unmatched.amount > 0 && (
          <li>
            未命中任何归属规则的 <span className="tabular-nums text-fg">{formatMoney(data.unmatched.amount)}</span> 已按配置计入「
            {data.unmatched_into}」，占 {(data.unmatched.share * 100).toFixed(1)}%，规则有待补充。
          </li>
        )}
      </ul>
    </Card>
  )
}

/**
 * 按云与付费方式汇总：对应财务表里的《云费用汇总表》，一行一种付费方式，每朵云一个小计，
 * 末行两云合计。数字就是「按月拆分」各页签的合计行，放在一起便于与财务汇总表核对。
 */
function MonthlySummary({ data, stale }: { data: BillAllocationResponse; stale?: boolean }) {
  const current = thisPeriod()
  const { periods, rows } = allocSummary(data)
  return (
    <Card title="按云厂商与付费方式汇总" extra={<span className="text-2xs text-muted-fg">与「按月拆分」各页签的合计行一致</span>}>
      <div className={cn('overflow-x-auto', stale && 'opacity-60 transition-opacity')}>
        <table className="w-full text-xs tabular-nums">
          <thead className="bg-muted/50 text-2xs text-muted-fg">
            <tr className="border-b border-border">
              <th className="sticky left-0 z-10 min-w-40 bg-muted px-3 py-2 text-left font-medium">云厂商 · 付费方式</th>
              {periods.map((p) => (
                <th key={p} className="min-w-24 px-3 py-2 text-right font-medium whitespace-nowrap">
                  {periodTick(p)}
                  {p === current && <span className="ml-1 font-normal text-warn">{currentLabel(data, p)}</span>}
                </th>
              ))}
              <th className="min-w-28 border-l border-border px-3 py-2 text-right font-medium">小计</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => {
              const values = periods.map((p) => r.byPeriod[p])
              const sub = r.subtotal
              return (
                <tr
                  key={r.key}
                  className={cn(
                    'border-b border-border/60 last:border-b-0',
                    r.tone === 'sub' && cn('font-medium', SUBTOTAL_BG),
                    r.tone === 'total' && cn('border-t-2 border-t-[color-mix(in_srgb,var(--brand)_45%,var(--border))] font-semibold', TOTAL_BG),
                  )}
                >
                  <th
                    scope="row"
                    className={cn('sticky left-0 z-10 px-3 py-2 text-left font-[inherit]', r.tone === 'plain' ? 'bg-card' : r.tone === 'sub' ? SUBTOTAL_BG : TOTAL_BG)}
                  >
                    {r.label}
                  </th>
                  {values.map((v, i) => (
                    <td key={periods[i]} className="px-3 py-2 text-right whitespace-nowrap">
                      <Money value={v} />
                    </td>
                  ))}
                  <td className="border-l border-border px-3 py-2 text-right whitespace-nowrap">
                    <Money value={sub} strong={r.tone === 'plain'} />
                  </td>
                </tr>
              )
            })}
          </tbody>
        </table>
      </div>
    </Card>
  )
}

/** 产品明细：金额、日均、月度预估 */
function ItemTable({ items, nights, compact, max = Math.max(0, ...items.map((i) => i.amount)) }: { items: BillAllocItem[]; nights: number; compact?: boolean; max?: number }) {
  if (!items.length) return <div className="px-4 py-6 text-center text-xs text-muted-fg">暂无数据</div>
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-xs">
        <thead className={cn('text-2xs text-muted-fg', !compact && 'sticky top-0 bg-card')}>
          <tr className="border-b border-border">
            <th className="px-3 py-2 text-left font-medium">产品</th>
            <th className="px-3 py-2 text-right font-medium">金额</th>
            <th className="px-3 py-2 text-right font-medium">日均</th>
            <th className="px-3 py-2 text-right font-medium">月度预估</th>
          </tr>
        </thead>
        <tbody>
          {items.map((item) => (
            // 同一产品常有后付费与「预付费摊销」两行，产品名、规则都一样，key 里不带 prepaid 就重复——
            // 重复的 key 会让 React 在列表变短（筛选、排序）时留下对不上的旧行
            <tr key={`${item.product}-${item.rule ?? ''}-${item.prepaid ? 'prepaid' : 'postpaid'}`} className="border-b border-border/60 last:border-b-0">
              <td className="max-w-64 truncate px-3 py-1.5">
                {item.product}
                {/* 同一个产品可能由几条规则分别归来，标出这一行是怎么来的 */}
                {item.rule && <span className="ml-2 text-2xs text-muted-fg">{item.rule}</span>}
                {item.prepaid && (
                  <Hint text="预付费（包年包月）按服务期摊入各账期，此处为摊入所选账期的部分，不计入日均" asChild>
                    <span className="ml-2 rounded bg-accent-soft px-1 text-2xs text-accent">预付费摊销</span>
                  </Hint>
                )}
              </td>
              <td className="px-3 py-1.5 text-right whitespace-nowrap tabular-nums">
                <span className="inline-flex items-center justify-end gap-2">
                  {/* 金额的比例条：按本表最大的一项折算，扫一眼就知道钱集中在哪几项 */}
                  {!compact && (
                    <span aria-hidden className="hidden h-1.5 w-24 overflow-hidden rounded-full bg-muted md:block">
                      <span className="block h-full rounded-full bg-accent/70" style={{ width: `${max > 0 ? Math.max(0, (item.amount / max) * 100) : 0}%` }} />
                    </span>
                  )}
                  {formatMoney(item.amount)}
                </span>
              </td>
              <td className="px-3 py-1.5 text-right whitespace-nowrap tabular-nums text-muted-fg">
                {item.daily === null ? '—' : formatMoney(item.daily)}
              </td>
              <td className="px-3 py-1.5 text-right whitespace-nowrap tabular-nums font-medium">
                {item.daily === null ? '—' : formatMoney(item.daily * nights)}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

const WEEKDAYS = '日一二三四五六'

/** 「9/23 周三」 */
const dayLabel = (day: string) => `${dayTick(day)} 周${WEEKDAYS[new Date(`${day}T00:00:00`).getDay()] ?? ''}`

/** 一段日期：单日写「9/23 周三」，多日写「9/17–9/23」 */
const rangeLabel = (r: Range) => (r.from === r.to ? dayLabel(r.to) : `${dayTick(r.from)}–${dayTick(r.to)}`)

/** 涨价标红、降价标强调色，与账单视图「环比」卡片同一套配色 */
const changeTone = (delta: number) => (delta > 0.005 ? 'text-danger' : delta < -0.005 ? 'text-accent' : 'text-muted-fg')

/** 变动额，带正负号；0 显示为「—」 */
const formatDelta = (delta: number) => (Math.abs(delta) < 0.005 ? '—' : `${delta > 0 ? '+' : ''}${formatMoney(delta)}`)

/** 金额口径、云、搜索与维度筛选照用；账期不传——对比的日期由服务端按今天往回数，与账期无关 */
const withoutPeriod = (base: Record<string, string | undefined>) =>
  Object.fromEntries(Object.entries(base).filter(([k]) => k !== 'from' && k !== 'to'))

/**
 * 产品费用对比：两段等长的日期里各产品花了多少，多了还是少了。
 *
 * 回答的是「昨天（这周）哪个产品突然多花了钱」——日均把波动抹平了，按月拆分又太粗，都看不出
 * 这个。比法见 `COMPARE_MODES`，默认截止到哪一天见 `defaultEnd`。只含后付费：预付费在购买那天
 * 一次性出账，逐日比只会冒出一根尖刺。
 *
 * 数据单独查（`/api/bills/product-days`），不取分析接口的：那份受所选账期与「日均口径」约束，
 * 对比段常常落在范围之外。
 */
function ProductCompare({ base, ready, prepaid }: { base: Record<string, string | undefined>; ready: boolean; prepaid: boolean }) {
  const params = useMemo(() => withoutPeriod(base), [base])
  const q = useBillProductDays(params, ready)
  const data = q.data
  const [mode, setMode] = useState<CompareMode>('1')
  const [picked, setPicked] = useState<string | null>(null)
  const [order, setOrder] = useState<'amount' | 'delta'>('amount')
  const [all, setAll] = useState(false)
  const options = data ? endOptions(data, mode) : []
  // 换了比法或筛选之后，原先选的那天可能已不可选，退回默认
  const end = picked && options.includes(picked) ? picked : data ? defaultEnd(data, mode) : null
  const cmp = data && end ? compare(data, mode, end) : null
  const multiCloud = new Set(data?.rows.map((r) => r.provider)).size > 1
  const rows = cmp
    ? [...cmp.rows].sort((a, b) =>
        order === 'delta'
          ? Math.abs(b.delta) - Math.abs(a.delta) || b.current - a.current
          : b.current - a.current || b.previous - a.previous || a.product.localeCompare(b.product),
      )
    : []
  const shown = all ? rows : rows.slice(0, PRODUCT_TOP)

  return (
    <Card
      title="产品费用对比"
      extra={
        <span className="flex flex-wrap items-center justify-end gap-2">
          {q.isFetching && <Spinner className="size-4" />}
          <span role="group" aria-label="排序" className="flex h-7 items-center rounded-md border border-input p-0.5">
            {(
              [
                ['amount', '按金额'],
                ['delta', '按变动'],
              ] as const
            ).map(([key, label]) => (
              <button
                key={key}
                type="button"
                aria-pressed={order === key}
                onClick={() => setOrder(key)}
                className={cn('h-full rounded-sm px-2 text-xs whitespace-nowrap text-muted-fg hover:text-fg', order === key && 'bg-accent-soft text-accent')}
              >
                {label}
              </button>
            ))}
          </span>
          <Combobox
            value={mode}
            onChange={(v) => setMode(v as CompareMode)}
            options={COMPARE_MODES}
            clearable={false}
            searchPlaceholder="筛比法…"
            title="比法"
            size="sm"
            className="w-40"
          />
          {cmp && (
            <Combobox
              value={cmp.current.to}
              onChange={setPicked}
              options={options.map((d) => ({ value: d, label: `截至 ${dayLabel(d)}` }))}
              clearable={false}
              searchPlaceholder="筛日期…"
              title="截止日"
              size="sm"
              className="w-36"
            />
          )}
        </span>
      }
    >
      {q.isError ? (
        <ErrorBox error={q.error} onRetry={() => q.refetch()} />
      ) : !data ? (
        <div className="flex justify-center py-10">
          <Spinner />
        </div>
      ) : !cmp ? (
        <div className="px-4 py-6 text-center text-xs text-muted-fg">
          {data.rows.length
            ? `近 ${data.days.length} 天的日度账单不足以「${COMPARE_MODES.find((m) => m.value === mode)?.label}」，请换一种比法。`
            : '近期没有日度账单，无法按天对比。'}
        </div>
      ) : (
        <div className={cn(q.isFetching && 'opacity-60 transition-opacity')}>
          <CompareSummary cmp={cmp} prepaid={prepaid} />
          <CompareNotes cmp={cmp} data={data} />
          <CompareTable data={data} cmp={cmp} rows={shown} multiCloud={multiCloud} />
          {rows.length > PRODUCT_TOP && (
            <button
              type="button"
              onClick={() => setAll(!all)}
              className="w-full border-t border-border px-3 py-2 text-center text-xs text-accent hover:bg-muted/40"
            >
              {all ? `收起，仅显示前 ${PRODUCT_TOP} 项` : `展开其余 ${rows.length - PRODUCT_TOP} 项`}
            </button>
          )}
        </div>
      )}
    </Card>
  )
}

/** 两段的合计与变动 */
function CompareSummary({ cmp, prepaid }: { cmp: Comparison; prepaid: boolean }) {
  const noun = cmp.basis === 'daily' ? '日均' : '合计'
  const delta = cmp.currentTotal - cmp.previousTotal
  return (
    <div className="flex flex-wrap items-baseline gap-x-5 gap-y-1 border-b border-border px-4 py-2.5 text-xs">
      <span>
        <span className="text-muted-fg">
          {rangeLabel(cmp.current)} {noun}{' '}
        </span>
        <span className="font-semibold tabular-nums">{formatMoney(cmp.currentTotal)}</span>
      </span>
      <span>
        <span className="text-muted-fg">
          {rangeLabel(cmp.previous)} {noun}{' '}
        </span>
        <span className="tabular-nums">{formatMoney(cmp.previousTotal)}</span>
      </span>
      <span className={cn('tabular-nums', changeTone(delta))}>
        {formatDelta(delta)}（{formatChange(changeRatio(cmp.currentTotal, cmp.previousTotal))}）
      </span>
      {prepaid && <span className="ml-auto text-2xs text-muted-fg">仅含后付费</span>}
    </div>
  )
}

/** 比出来的数字可能失真的几种情形，各写一句 */
function CompareNotes({ cmp, data }: { cmp: Comparison; data: BillProductDaysResponse }) {
  const notes: string[] = []
  if (cmp.pending.length) {
    notes.push(
      `${cmp.pending.map((p) => `${PROVIDER_LABELS[p]}的日度账单截至 ${dayTick(data.last_by_provider[p] ?? '')}`).join('，')}，本段不含该云其后的费用，其产品的「下降」并非实际下降。`,
    )
  }
  const gaps = [...cmp.previous.gaps, ...cmp.current.gaps]
  if (gaps.length) {
    notes.push(
      `${gaps.map(dayTick).join('、')} 没有账单，可能尚未同步${cmp.basis === 'daily' ? '；两段有账单的天数不同，改按各自有账单的天数折算日均比较' : ''}。`,
    )
  }
  if (data.monthly_only.length) {
    notes.push(`${data.monthly_only.map((p) => PROVIDER_LABELS[p]).join('、')}仅有月度账单，未计入对比。`)
  }
  if (!notes.length) return null
  return (
    <ul className="space-y-0.5 border-b border-border bg-warn-soft px-4 py-2 text-2xs leading-5 text-warn">
      {notes.map((n) => (
        <li key={n}>{n}</li>
      ))}
    </ul>
  )
}

/**
 * 变动条：从中线出发，涨价向右标红、降价向左用强调色，长度按本表最大的变动额折算。
 * 扫一眼就知道变动集中在哪几个产品，不必逐行读数字
 */
function DeltaBar({ delta, max }: { delta: number; max: number }) {
  const pct = max > 0 ? Math.min(50, (Math.abs(delta) / max) * 50) : 0
  return (
    <span aria-hidden className="relative block h-2.5 w-full">
      <span className="absolute inset-y-0 left-1/2 w-px bg-border" />
      {pct > 0 && (
        <span
          className={cn('absolute inset-y-0 rounded-[2px]', delta > 0 ? 'bg-danger/70' : 'bg-accent/70')}
          style={delta > 0 ? { left: '50%', width: `${pct}%` } : { right: '50%', width: `${pct}%` }}
        />
      )}
    </span>
  )
}

function CompareTable({ data, cmp, rows, multiCloud }: { data: BillProductDaysResponse; cmp: Comparison; rows: ProductDiff[]; multiCloud: boolean }) {
  const [open, setOpen] = useState<string | null>(null)
  if (!rows.length) return <div className="px-4 py-6 text-center text-xs text-muted-fg">两段均无后付费账单</div>
  const suffix = cmp.basis === 'daily' ? ' 日均' : ''
  // 按全部产品取最大值，不只取显示出来的这几行：展开其余项时条长不该跟着变
  const max = Math.max(0, ...cmp.rows.map((r) => Math.abs(r.delta)))
  const colSpan = multiCloud ? 7 : 6
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-xs tabular-nums">
        <thead className="text-2xs text-muted-fg">
          <tr className="border-b border-border">
            <th className="px-3 py-2 text-left font-medium">产品</th>
            {multiCloud && <th className="px-3 py-2 text-left font-medium">云厂商</th>}
            <th className="px-3 py-2 text-right font-medium whitespace-nowrap">
              {rangeLabel(cmp.previous)}
              {suffix}
            </th>
            <th className="px-3 py-2 text-right font-medium whitespace-nowrap">
              {rangeLabel(cmp.current)}
              {suffix}
            </th>
            <th className="hidden w-32 px-3 py-2 text-center font-medium sm:table-cell">变动</th>
            <th className="px-3 py-2 text-right font-medium">变动额</th>
            <th className="px-3 py-2 text-right font-medium">变动率</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => {
            const key = `${r.provider}-${r.product}`
            const isOpen = open === key
            const ratio = changeRatio(r.current, r.previous)
            const amounts = data.rows.find((x) => x.provider === r.provider && x.product === r.product)?.amounts
            return (
              <Fragment key={key}>
                {/* 整行都能点开；产品名那个按钮留给键盘与读屏，它的点击冒泡到行上，不另绑一次 */}
                <tr onClick={() => setOpen(isOpen ? null : key)} className="row-hover cursor-pointer border-b border-border/60 last:border-b-0">
                  <td className="max-w-64 px-3 py-1.5">
                    <button type="button" aria-expanded={isOpen} className="flex max-w-full items-center gap-1.5 text-left">
                      <ChevronRightIcon className={cn('size-3.5 shrink-0 text-muted-fg transition-transform', isOpen && 'rotate-90')} />
                      <span className="truncate">{r.product}</span>
                    </button>
                  </td>
                  {multiCloud && <td className="px-3 py-1.5 whitespace-nowrap text-muted-fg">{PROVIDER_LABELS[r.provider]}</td>}
                  <td className="px-3 py-1.5 text-right whitespace-nowrap text-muted-fg">
                    <Money value={r.previous} />
                  </td>
                  <td className="px-3 py-1.5 text-right font-medium whitespace-nowrap">
                    <Money value={r.current} />
                  </td>
                  <td className="hidden px-3 py-1.5 sm:table-cell">
                    <DeltaBar delta={r.delta} max={max} />
                  </td>
                  <td className={cn('px-3 py-1.5 text-right whitespace-nowrap', changeTone(r.delta))}>{formatDelta(r.delta)}</td>
                  <td className={cn('px-3 py-1.5 text-right whitespace-nowrap', changeTone(r.delta))}>
                    {/* 对比段分文未花的，环比无从谈起，标为新增 */}
                    {ratio === null ? (r.current > 0 ? '新增' : '—') : formatChange(ratio)}
                  </td>
                </tr>
                {isOpen && amounts && (
                  <tr className="border-b border-border/60 bg-muted/20">
                    <td colSpan={colSpan} className="px-3 py-2">
                      <ProductTrend data={data} cmp={cmp} product={r.product} amounts={amounts} />
                    </td>
                  </tr>
                )}
              </Fragment>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}

const DAY_MS = 86_400_000
const dayMs = (day: string) => new Date(`${day}T00:00:00`).getTime()

/**
 * 点开一个产品看到的逐日走势，见 `trend`。纵轴按这个产品自己的金额取上下限、不从 0 起：
 * 日费用大多平稳，从 0 起画时涨一成只高出几个像素
 */
function ProductTrend({ data, cmp, product, amounts }: { data: BillProductDaysResponse; cmp: Comparison; product: string; amounts: number[] }) {
  const t = trend(data, cmp.mode, cmp.current.to, amounts)
  const overlay = t.points.some((p) => p.previousDay)
  const first = t.points[0]?.day ?? ''
  const last = t.points.at(-1)?.day ?? ''
  const series: LineSeries[] = [
    { key: 'current', label: overlay ? `本段 ${rangeLabel(cmp.current)}` : product, color: 'var(--chart-1)' },
    ...(overlay ? [{ key: 'previous', label: `对比段 ${rangeLabel(cmp.previous)}`, color: 'var(--muted-fg)', dashed: true }] : []),
  ]
  const byMs = new Map(t.points.map((p) => [dayMs(p.day), p]))
  return (
    <div>
      <div className="mb-1 flex flex-wrap items-center gap-x-4 gap-y-1 px-1 text-2xs text-muted-fg">
        <span className="font-medium text-fg">{product}</span>
        <span>
          {dayTick(first)}–{dayTick(last)}，共 {t.points.length} 天
        </span>
        {overlay ? (
          <>
            <span>对比段按天对齐叠放：第 1 天对第 1 天</span>
            <Legend series={series} className="ml-auto" />
          </>
        ) : (
          <span>竖线标出比较的两天：{t.marks.map(dayLabel).join(' 与 ')}</span>
        )}
      </div>
      <LineChart
        fromMs={dayMs(first)}
        toMs={dayMs(last) + DAY_MS}
        widthMs={DAY_MS}
        points={t.points.map((p): { t_ms: number; values: Record<string, number> } => ({ t_ms: dayMs(p.day), values: overlay ? { current: p.value, previous: p.previous ?? 0 } : { current: p.value } }))}
        series={series}
        format={formatMoneyTick}
        fitY
        dots
        events={t.marks.map((d) => ({ t_ms: dayMs(d), label: dayLabel(d) }))}
        hoverTitle={(ms) => {
          const p = byMs.get(ms)
          if (!p) return ''
          return p.previousDay ? `${dayLabel(p.day)} · 对比 ${dayLabel(p.previousDay)}` : dayLabel(p.day)
        }}
        label={`${product}的逐日费用`}
        height={170}
      />
    </div>
  )
}

/**
 * 业务线构成的堆叠柱状图。
 *
 * 复用 `StackedBars`（纵轴刻度、网格线、悬停提示与点选某一段都由它提供）。按账期时与
 * 「按账期」那张一样经 `periodSlots` 排成等宽的桶，2 月与 8 月在图上等宽。
 *
 * 点图例或柱子上的某一段即单独查看那条线：只画它、纵轴按它重新缩放（小业务线叠在底下时
 * 只有一两个像素高，不单独拎出来根本看不出走势），并在图上方列出它的金额。再点一次复原。
 */
function LineBars({
  points,
  lines,
  unmatched,
  absorbed,
  unit,
  nights,
  estimate,
  focus,
  onFocus,
  order,
  stale,
}: {
  points: BillAllocPoint[]
  lines: BillAllocLine[]
  /** 未并入任何业务线的未归属费用；传了才在柱顶画一段灰色 */
  unmatched?: BillAllocLine
  /** 未归属已按配置并入的那条业务线及其金额（后付费）：图上不另画，只在那条线上注明 */
  absorbed?: { into: string; amount: number }
  unit: '天' | '账期'
  nights: number
  estimate: string
  focus: string | null
  onFocus: (line: string | null) => void
  /** 配置里业务线的顺序，决定颜色（见 lineColor） */
  order: string[]
  stale?: boolean
}) {
  // 接口的 by_line 只有各业务线，未归属 = 当天合计 − 各线之和。四舍五入会留下几分钱的零头，不算
  const rest = (p: BillAllocPoint) => {
    const v = p.total - Object.values(p.by_line).reduce((a, b) => a + b, 0)
    return v > 0.005 ? v : 0
  }
  const withRest = !!unmatched && points.some((p) => rest(p) > 0)
  // 系列：各业务线在前，未归属压在柱顶。键与显示名分开，见 UNMATCHED
  const series = [
    ...lines.map((l) => ({ key: l.name, line: l, color: lineColor(order, l.name) })),
    ...(withRest && unmatched ? [{ key: UNMATCHED, line: unmatched, color: UNMATCHED_COLOR }] : []),
  ]
  const valueAt = (p: BillAllocPoint, key: string) => (key === UNMATCHED ? rest(p) : (p.by_line[key] ?? 0))
  const current = series.find((x) => x.key === focus)
  const shown = current ? [current] : series
  const toggle = (key: string) => onFocus(focus === key ? null : key)
  const tick = (t: string) => (t.length > 7 ? dayTick(t) : periodTick(t))
  /**
   * 横轴：按天就是真实日期（刻度由 StackedBars 按时间自动挑）；按账期则经 periodSlots 排成
   * 等宽的桶，逐个标「4月」。
   */
  const axis = useMemo(() => {
    if (unit === '天') {
      const DAY = 86_400_000
      const ts = points.map((p) => new Date(`${p.t}T00:00:00`).getTime())
      const fromMs = ts[0] ?? 0
      return {
        fromMs,
        toMs: (ts[ts.length - 1] ?? 0) + DAY,
        widthMs: DAY,
        at: (i: number) => ts[i],
        index: (t: number) => ts.indexOf(t),
        xTicks: undefined,
      }
    }
    const slots = periodSlots(points.map((p) => p.t))
    return { ...slots, xTicks: points.map((p, i) => ({ t_ms: slots.at(i), label: periodTick(p.t) })) }
  }, [points, unit])
  const buckets = points.map((p, i) => ({ t_ms: axis.at(i), values: Object.fromEntries(series.map((x) => [x.key, valueAt(p, x.key)])) }))
  const amortized = current?.line.amortized_by_period?.[estimate] ?? 0
  const projected = !current || (current.line.daily === null && !amortized) ? null : (current.line.daily ?? 0) * nights + amortized
  return (
    <div className="px-2 pt-3 pb-2">
      {current && (
        <div className="mx-1 mb-3 flex flex-wrap items-center gap-x-5 gap-y-1 rounded-md bg-muted/40 px-3 py-2 text-xs">
          <span className="flex items-center gap-1.5 font-medium">
            <span className="size-2 rounded-[2px]" style={{ background: current.color }} />
            {current.line.name}
          </span>
          <span>
            <span className="text-muted-fg">合计 </span>
            <span className="font-semibold tabular-nums">{formatMoney(current.line.amount)}</span>
            <span className="text-muted-fg tabular-nums">（{(current.line.share * 100).toFixed(1)}%）</span>
          </span>
          {absorbed?.into === current.key && absorbed.amount > 0 && (
            <span className="text-warn">
              其中未命中归属规则、按配置计入的 <span className="tabular-nums">{formatMoney(absorbed.amount)}</span>
            </span>
          )}
          {current.line.amortized > 0 && (
            <span className="text-muted-fg">
              后付费 <span className="tabular-nums text-fg">{formatMoney(current.line.postpaid)}</span> · 预付费摊销{' '}
              <span className="tabular-nums text-fg">{formatMoney(current.line.amortized)}</span>
            </span>
          )}
          <span>
            <span className="text-muted-fg">日均 </span>
            <span className="tabular-nums">{current.line.daily === null ? '—' : formatMoney(current.line.daily)}</span>
          </span>
          <span>
            <span className="text-muted-fg">{estimate} 预估 </span>
            <span className="font-semibold tabular-nums">{projected === null ? '—' : formatMoney(projected)}</span>
          </span>
          <button type="button" onClick={() => onFocus(null)} className="ml-auto text-2xs text-accent hover:underline">
            查看全部业务线
          </button>
        </div>
      )}
      <StackedBars
        fromMs={axis.fromMs}
        toMs={axis.toMs}
        widthMs={axis.widthMs}
        buckets={buckets}
        series={shown.map((x) => ({ key: x.key, label: x.line.name, color: x.color }))}
        format={formatMoneyTick}
        valueFormat={formatMoney}
        xTicks={axis.xTicks}
        bucketTitle={(t) => points[axis.index(t)]?.t ?? ''}
        // 点在哪一段就单独查看哪条线；已在单独查看时再点即复原
        onPointClick={({ seriesKey }) => onFocus(seriesKey && seriesKey !== focus ? seriesKey : null)}
        stale={stale}
        label={`按业务线的费用（${unit === '天' ? '按天' : '按账期'}）`}
        height={200}
      />
      <div className="flex flex-wrap items-center gap-x-1 gap-y-1 px-1 text-2xs text-muted-fg">
        {/* 图例即开关，并标出每条线在图中的金额（图只含后付费，所以这里也取后付费） */}
        {series.map((x) => (
          <button
            key={x.key}
            type="button"
            aria-pressed={focus === x.key}
            onClick={() => toggle(x.key)}
            className={cn(
              'flex items-center gap-1.5 rounded px-1.5 py-0.5 hover:bg-muted/60 hover:text-fg',
              focus === x.key && 'bg-accent-soft text-fg',
              focus && focus !== x.key && 'opacity-50',
            )}
          >
            <span className="size-2 rounded-[2px]" style={{ background: x.color }} />
            {x.line.name}
            <span className="tabular-nums text-fg">{formatMoneyShort(x.line.postpaid)}</span>
            {absorbed?.into === x.key && absorbed.amount > 0 && (
              <span className="tabular-nums text-warn">含未归属 {formatMoneyShort(absorbed.amount)}</span>
            )}
          </button>
        ))}
        {points.length > 0 && (
          <span className="ml-auto tabular-nums">
            {tick(points[0].t)} – {tick(points[points.length - 1].t)}
          </span>
        )}
      </div>
    </div>
  )
}
