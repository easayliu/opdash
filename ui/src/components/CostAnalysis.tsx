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
 */
import { useMemo, useState } from 'react'
import { ChevronRightIcon } from 'lucide-react'
import { useBillAllocation } from '@/api/queries'
import type { BillAllocItem, BillAllocLine, BillAllocPoint, BillsMeta } from '@/api/types'
import { Card, Combobox, EmptyState, ErrorBox, Hint, Spinner } from '@/components/ui'
import { PROVIDER_LABELS, daysInMonth, dayTick, formatMoney, formatMoneyShort, periodTick, shiftPeriod } from '@/lib/bills'
import { seriesVar } from '@/lib/colors'
import { cn } from '@/lib/utils'

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
  onChange,
}: {
  /** 与「账单」视图共用的查询条件：账期、金额口径、云、搜索、维度筛选 */
  base: Record<string, string | undefined>
  ready: boolean
  bills: BillsMeta
  /** 日均的窗口（`days` 参数），空串表示所选账期全部 */
  days: string
  /** 预估哪个月，`YYYY-MM` */
  estimate: string
  onChange: (next: Record<string, string | null>) => void
}) {
  const params = useMemo(() => ({ ...base, days: days || undefined }), [base, days])
  const alloc = useBillAllocation(params, ready)
  const data = alloc.data
  const [open, setOpen] = useState<string | null>(null)

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
            {data.days_by_provider[data.coverage.provider] ?? 0} 天账单求得，仍可参考；区间合计与各业务线金额则明显偏低。
            {bills.sync ? '可点击右上角「拉取账单」，选择日度粒度补齐这几个账期。' : '请让 goscan 以日度粒度补齐这几个账期。'}
          </div>
        </div>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <Hint text="日均 = 该区间的花费 ÷ 有账单的天数。取「最近 7 天」可避开月初扩容等早期波动，更贴近当前水位">
          <span className="text-xs text-muted-fg">日均口径</span>
        </Hint>
        <Combobox
          value={days || 'all'}
          onChange={(v) => onChange({ days: v === 'all' ? null : v })}
          options={WINDOWS}
          clearable={false}
          searchPlaceholder="筛口径…"
          title="日均按多长的窗口计算"
          size="sm"
          className="w-28"
        />
        <Hint
          text={
            prepaid
              ? '月度预估 = 后付费日均 × 该月的自然天数 + 该月的预付费摊销。前一段的前提是用量不变，扩容与活动均不计入；后一段来自已经发生的购买，是确定的'
              : '月度预估 = 日均 × 该月的自然天数。其前提是用量不变，扩容、活动与包年包月的一次性支出均不计入'
          }
        >
          <span className="ml-2 text-xs text-muted-fg">预估</span>
        </Hint>
        <Combobox
          value={estimate}
          onChange={(v) => onChange({ est: v })}
          options={[base.to ?? '', shiftPeriod(base.to ?? '', 1)]
            .filter(Boolean)
            .map((p) => ({ value: p, label: `${p}（${daysInMonth(p)} 天）` }))}
          clearable={false}
          searchPlaceholder="筛月份…"
          title="预估哪个月"
          size="sm"
          className="w-36"
        />
        {alloc.isFetching && <Spinner className="size-4" />}
      </div>

      <div className={cn('grid grid-cols-2 gap-3', bills.allocation ? 'lg:grid-cols-4' : 'lg:grid-cols-3')}>
        <Stat
          label="区间合计"
          value={formatMoney(data?.total ?? 0)}
          hint={`${data?.from ?? ''} 至 ${data?.to ?? ''}${
            prepaid
              ? data?.window_days
                ? `。含预付费摊销，已按天折算到最近 ${data.window_days} 天，与后付费口径一致`
                : '。含预付费按服务期摊到本区间的部分'
              : ''
          }`}
          extra={
            prepaid ? (
              <span className="mt-1 block text-2xs text-muted-fg">
                后付费 {formatMoneyShort(data?.postpaid ?? 0)} · 预付费摊销 {formatMoneyShort(data?.amortized ?? 0)}
              </span>
            ) : undefined
          }
        />
        <Stat
          label="日均"
          value={data?.daily === null || data?.daily === undefined ? '—' : formatMoney(data.daily)}
          hint={
            data?.days
              ? `依已出具的账单求得（${Object.entries(data.days_by_provider)
                  .map(([p, n]) => `${PROVIDER_LABELS[p as keyof typeof PROVIDER_LABELS] ?? p} ${n} 天`)
                  .join('、')}），各云按各自的天数折算后相加${data.window_days ? `；仅取最近 ${data.window_days} 天` : ''}${prepaid ? '。预付费按月摊销，不计入日均' : ''}`
              : '所选区间内没有日粒度的账单：阿里云需同步 granularity=daily，火山引擎的明细自带费用日期'
          }
        />
        <Stat
          label={`${estimate} 预估`}
          value={
            project(data?.daily ?? null, data?.amortized_by_period) === null
              ? '—'
              : formatMoney(project(data?.daily ?? null, data?.amortized_by_period) as number)
          }
          hint={
            prepaid
              ? `后付费日均 × ${nights} 天，加上该月的预付费摊销 ${formatMoney(data?.amortized_by_period?.[estimate] ?? 0)}（已购之物摊过来的，不是估的）`
              : `日均 × ${nights} 天，即维持当前用量时整月的花费；并非该月已出具账单的合计`
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
          label="未归属"
          value={formatMoney(data?.unmatched.amount ?? 0)}
          tone={(data?.unmatched.share ?? 0) > 0.2 ? 'warn' : 'plain'}
          hint="未命中任何归属规则的部分。占比偏高意味着规则有待补充；配置了 unmatched 时，这笔费用已计入指定的业务线"
          extra={
            <span className="mt-1 block text-2xs text-muted-fg">
              占 {((data?.unmatched.share ?? 0) * 100).toFixed(1)}%
            </span>
          }
        />
        )}
      </div>

      <Card
        title="按业务线"
        extra={
          <span className="text-2xs text-muted-fg">
            {bills.allocation ? `${bills.allocation.rules} 条归属规则` : '未配置归属规则'}
          </span>
        }
      >
        {alloc.isPending ? (
          <div className="flex justify-center py-10">
            <Spinner />
          </div>
        ) : !bills.allocation ? (
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
        ) : lines.length === 0 ? (
          <div className="px-4 py-8 text-center text-xs text-muted-fg">所选账期没有可分摊的账单</div>
        ) : (
          <ul className={cn(alloc.isFetching && 'opacity-60 transition-opacity')}>
            {lines.map((line) => (
              <LineRow
                key={line.name}
                line={line}
                nights={nights}
                estimate={estimate}
                open={open === line.name}
                onToggle={() => setOpen(open === line.name ? null : line.name)}
              />
            ))}
          </ul>
        )}
      </Card>

      {(data?.points.length ?? 0) > 0 && lines.length > 0 && (
        <Card
          title={data?.granularity === 'daily' ? '按天的业务线构成' : '按账期的业务线构成'}
          extra={
            <span className="text-2xs text-muted-fg">
              {data?.points.length} {data?.granularity === 'daily' ? '天' : '个账期'}
              {prepaid && ' · 只含后付费'}
            </span>
          }
        >
          <LineBars points={data?.points ?? []} lines={lines.map((l) => l.name)} stale={alloc.isFetching} />
        </Card>
      )}

      <Card title="按产品" extra={<span className="text-2xs text-muted-fg">不分业务线，与账单本身一致</span>}>
        {alloc.isPending ? (
          <div className="flex justify-center py-10">
            <Spinner />
          </div>
        ) : (
          <ItemTable items={data?.products ?? []} nights={nights} />
        )}
      </Card>
    </div>
  )
}

function Stat({
  label,
  value,
  hint,
  tone = 'plain',
  extra,
}: {
  label: string
  value: string
  hint?: string
  tone?: 'plain' | 'warn'
  extra?: React.ReactNode
}) {
  const body = (
    <div className="rounded-lg border border-border bg-card px-4 py-3">
      <div className="text-xs text-muted-fg">{label}</div>
      <div className={cn('mt-1 text-xl font-semibold tabular-nums', tone === 'warn' && 'text-danger')}>{value}</div>
      {extra}
    </div>
  )
  return hint ? <Hint text={hint}>{body}</Hint> : body
}

/** 一条业务线。点开看它由哪些产品、按哪条规则构成 */
function LineRow({
  line,
  nights,
  estimate,
  open,
  onToggle,
}: {
  line: BillAllocLine
  nights: number
  estimate: string
  open: boolean
  onToggle: () => void
}) {
  // 这条线在目标月的月度预估：后付费日均 × 天数 + 那个月摊过来的预付费
  const amortized = line.amortized_by_period?.[estimate] ?? 0
  const projected = line.daily === null && !amortized ? null : (line.daily ?? 0) * nights + amortized
  return (
    <li className="border-b border-border/60 last:border-b-0">
      <button type="button" onClick={onToggle} aria-expanded={open} className="row-hover flex w-full items-center gap-3 px-3 py-2 text-left">
        <ChevronRightIcon className={cn('size-3.5 shrink-0 text-muted-fg transition-transform', open && 'rotate-90')} />
        <span className="min-w-0 flex-1">
          <span className="truncate text-xs font-medium">{line.name}</span>
          <span className="mt-1 block h-1.5 w-full rounded-full bg-muted">
            <span className="block h-full rounded-full bg-brand" style={{ width: `${Math.min(100, line.share * 100)}%` }} />
          </span>
        </span>
        <span className="shrink-0 text-right">
          <span className="block text-xs font-semibold tabular-nums">{formatMoney(line.amount)}</span>
          <span className="block text-2xs text-muted-fg tabular-nums">{(line.share * 100).toFixed(1)}%</span>
        </span>
        <span className="hidden w-24 shrink-0 text-right text-xs tabular-nums text-muted-fg sm:block">
          {line.daily === null ? '—' : `${formatMoney(line.daily)}/天`}
        </span>
        <Hint
          text={
            amortized
              ? `${formatMoney((line.daily ?? 0) * nights)}（后付费）+ ${formatMoney(amortized)}（预付费摊销）`
              : '后付费日均 × 该月天数'
          }
          asChild
        >
          <span className="w-24 shrink-0 text-right text-xs font-medium tabular-nums">
            {projected === null ? '—' : formatMoney(projected)}
          </span>
        </Hint>
      </button>
      {open && (
        <div className="border-t border-border/60 bg-muted/20 px-3 py-1">
          <ItemTable items={line.items} nights={nights} compact />
        </div>
      )}
    </li>
  )
}

/** 产品明细：金额、日均、月度预估 */
function ItemTable({ items, nights, compact }: { items: BillAllocItem[]; nights: number; compact?: boolean }) {
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
            <tr key={`${item.product}-${item.rule ?? ''}`} className="border-b border-border/60 last:border-b-0">
              <td className="max-w-64 truncate px-3 py-1.5">
                {item.product}
                {/* 同一个产品可能由几条规则分别归来，标出这一行是怎么来的 */}
                {item.rule && <span className="ml-2 text-2xs text-muted-fg">{item.rule}</span>}
                {item.prepaid && (
                  <Hint text="预付费（包年包月）按服务期摊到各月，此处是摊到本区间的部分，不按天计" asChild>
                    <span className="ml-2 rounded bg-accent-soft px-1 text-2xs text-accent">摊销</span>
                  </Hint>
                )}
              </td>
              <td className="px-3 py-1.5 text-right whitespace-nowrap tabular-nums">{formatMoney(item.amount)}</td>
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

/**
 * 业务线构成的堆叠柱状图。
 *
 * 与「按账期」那张一样按分类等宽排布，而非按时间轴摊开：这里的一根柱子是一天或一个账期，
 * 2 月与 8 月在图上应当等宽。
 */
function LineBars({ points, lines, stale }: { points: BillAllocPoint[]; lines: string[]; stale?: boolean }) {
  const max = Math.max(1, ...points.map((p) => p.total))
  // 点数多时不逐个标注横轴，否则文字叠成一团
  const ticks = points.length <= 31
  const tick = (t: string) => (t.length > 7 ? dayTick(t) : periodTick(t))
  return (
    <div className={cn('px-3 pt-4 pb-2', stale && 'opacity-60 transition-opacity')}>
      <div className="flex h-40 items-end gap-px" role="img" aria-label={`按业务线的花费，共 ${points.length} 个点`}>
        {points.map((p) => (
          <Hint
            key={p.t}
            text={[`${p.t} 合计 ${formatMoney(p.total)}`, ...lines.map((l) => `${l} ${formatMoney(p.by_line[l] ?? 0)}`)].join('\n')}
            asChild
          >
            <span className="flex h-full min-w-0 flex-1 flex-col justify-end gap-px rounded-sm hover:bg-muted/40">
              {lines.map((line, i) => {
                const v = p.by_line[line] ?? 0
                if (v <= 0) return null
                return <span key={line} style={{ height: `${(v / max) * 100}%`, background: seriesVar(i) }} className="w-full rounded-[1px]" />
              })}
              {p.total <= 0 && <span className="h-px w-full bg-border" />}
            </span>
          </Hint>
        ))}
      </div>
      {ticks && (
        <div className="mt-1.5 flex gap-px text-center text-2xs text-muted-fg">
          {points.map((p) => (
            <span key={p.t} className="min-w-0 flex-1 truncate tabular-nums">
              {tick(p.t)}
            </span>
          ))}
        </div>
      )}
      <div className="mt-2 flex flex-wrap items-center gap-3 border-t border-border/60 pt-2 text-2xs text-muted-fg">
        {lines.map((line, i) => (
          <span key={line} className="flex items-center gap-1.5">
            <span className="size-2 rounded-[2px]" style={{ background: seriesVar(i) }} />
            {line}
          </span>
        ))}
        <span className="ml-auto">
          纵轴上限 {formatMoneyShort(max)}
          {!ticks && ` · ${tick(points[0].t)} – ${tick(points[points.length - 1].t)}`}
        </span>
      </div>
    </div>
  )
}
