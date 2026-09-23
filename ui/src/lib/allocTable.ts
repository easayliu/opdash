/**
 * 分析视图「按月拆分」「按云厂商与付费方式汇总」两张表的数字。
 *
 * 页面上的表格和 xlsx 导出都从这里取，保证导出的每一格与页面上看到的一致——两边各算一遍，
 * 迟早会有一处口径改了、另一处没改。
 */
import type { BillAllocLine, BillAllocMonthRow, BillAllocationResponse, BillProvider } from '@/api/types'
import { PROVIDER_LABELS, dayTick, periodsBetween } from '@/lib/bills'

/** 拆分表里一段的键：`all` 为各段合计，其余为「云厂商:付费方式」 */
export type SectionKey = 'all' | `${BillProvider}:${BillAllocMonthRow['kind']}`

export const KIND_LABELS: Record<BillAllocMonthRow['kind'], string> = { postpaid: '后付费', prepaid: '预付费摊销' }

/** 段的先后与财务那张拆分表一致：阿里云后付费、阿里云预付费、火山引擎，最后是合计 */
const SECTION_ORDER: SectionKey[] = ['alicloud:postpaid', 'alicloud:prepaid', 'volcengine:postpaid', 'volcengine:prepaid']

/** 当前自然月（`YYYY-MM`）：这一列的账单尚未出齐，表头要注明 */
export function thisPeriod(): string {
  const d = new Date()
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}`
}

/**
 * 当月那一列表头后面的小字：写明账单统计到哪一天（「截至 9/22」），比「未满月」有用——
 * 账单滞后一两天出具，看这一列最想知道的是它覆盖了几天。取所选账期里最后一天有账单的日期；
 * 只有月度账单、拿不到日期时退为「进行中」
 */
export function currentLabel(data: BillAllocationResponse, period: string): string {
  const last = data.granularity === 'daily' ? data.points.at(-1)?.t : undefined
  return last?.startsWith(period) ? `截至 ${dayTick(last)}` : '进行中'
}

/** 有哪些段可看：合计在前，其余按财务表的顺序，只列有数据的 */
export function allocSections(data: BillAllocationResponse): { key: SectionKey; label: string }[] {
  const present = SECTION_ORDER.filter((k) => data.monthly.some((r) => `${r.provider}:${r.kind}` === k))
  const providers = new Set(data.monthly.map((r) => r.provider))
  return [
    { key: 'all', label: providers.size > 1 ? '两云合计' : '合计' },
    ...present.map((k) => {
      const [p, kind] = k.split(':') as [BillProvider, BillAllocMonthRow['kind']]
      return { key: k, label: `${PROVIDER_LABELS[p]} · ${KIND_LABELS[kind]}` }
    }),
  ]
}

export interface AllocRow {
  /** 业务线名；null 是未命中规则、也没并入哪条线的那部分 */
  key: string | null
  name: string
  /** 接口给的这条线（含产品明细），未归属那行是 `data.unmatched` */
  line: BillAllocLine | undefined
  byPeriod: Record<string, number>
  subtotal: number
  /** 占本段合计的比例，0 ~ 1；本段合计为 0 时为 null */
  share: number | null
  daily: number | null
  projected: number | null
}

export interface AllocSectionTable {
  periods: string[]
  rows: AllocRow[]
  colTotals: Record<string, number>
  total: number
  /** 月份格子里的最大值，页面按它给格子上底色 */
  cellMax: number
  footDaily: number | null
  footProject: number | null
}

/**
 * 一段的表：一行一条业务线（按配置里的顺序），一列一个账期，外加小计、占比、日均、预估与合计行。
 *
 * 日均与预估按段取：合计用合并后的数字；后付费段用这朵云自己的日均 × 天数；预付费摊销段
 * 不按天计，预估就是摊入该账期的金额。
 */
export function allocSection(
  data: BillAllocationResponse,
  order: string[],
  key: SectionKey,
  nights: number,
  estimate: string,
): AllocSectionTable {
  const periods = periodsBetween(data.from, data.to)
  const source = key === 'all' ? data.monthly : data.monthly.filter((r) => `${r.provider}:${r.kind}` === key)
  // 业务线 → 账期 → 金额
  const cell = new Map<string | null, Record<string, number>>()
  for (const r of source) {
    const m = cell.get(r.line) ?? {}
    for (const [p, v] of Object.entries(r.by_period)) m[p] = (m[p] ?? 0) + v
    cell.set(r.line, m)
  }
  const keys: (string | null)[] = [...order, ...(cell.has(null) ? [null] : [])]
  const [provider, kind] = key === 'all' ? [null, null] : (key.split(':') as [BillProvider, BillAllocMonthRow['kind']])
  const lineOf = (k: string | null) => (k === null ? data.unmatched : data.lines.find((l) => l.name === k))
  const dailyOf = (l: BillAllocLine | undefined): number | null => {
    if (!l) return null
    if (!provider) return l.daily
    return kind === 'prepaid' ? null : (l.by_provider?.[provider]?.daily ?? null)
  }
  const projectOf = (l: BillAllocLine | undefined): number | null => {
    if (!l) return null
    if (!provider) {
      const amortized = l.amortized_by_period?.[estimate] ?? 0
      return l.daily === null && !amortized ? null : (l.daily ?? 0) * nights + amortized
    }
    const part = l.by_provider?.[provider]
    if (kind === 'prepaid') return part?.amortized_by_period?.[estimate] || null
    return part?.daily == null ? null : part.daily * nights
  }

  const colTotals: Record<string, number> = {}
  for (const p of periods) colTotals[p] = keys.reduce((n, k) => n + (cell.get(k)?.[p] ?? 0), 0)
  const total = periods.reduce((n, p) => n + colTotals[p], 0)
  const rows: AllocRow[] = keys.map((k) => {
    const byPeriod = cell.get(k) ?? {}
    const subtotal = periods.reduce((n, p) => n + (byPeriod[p] ?? 0), 0)
    const line = lineOf(k)
    return {
      key: k,
      name: k ?? '未归属',
      line,
      byPeriod,
      subtotal,
      share: total ? subtotal / total : null,
      daily: dailyOf(line),
      projected: projectOf(line),
    }
  })
  /** 各行相加；一行都给不出时为 null */
  const sumNullable = (values: (number | null)[]) => (values.some((v) => v !== null) ? values.reduce<number>((n, v) => n + (v ?? 0), 0) : null)
  const wholeAmortized = data.amortized_by_period?.[estimate] ?? 0
  return {
    periods,
    rows,
    colTotals,
    total,
    cellMax: Math.max(0, ...rows.flatMap((r) => periods.map((p) => r.byPeriod[p] ?? 0))),
    footDaily: provider ? sumNullable(rows.map((r) => r.daily)) : data.daily,
    footProject: provider
      ? sumNullable(rows.map((r) => r.projected))
      : data.daily === null && !wholeAmortized
        ? null
        : (data.daily ?? 0) * nights + wholeAmortized,
  }
}

export interface AllocSummaryRow {
  key: string
  label: string
  /** plain 一种付费方式；sub 一朵云的小计；total 两云合计 */
  tone: 'plain' | 'sub' | 'total'
  byPeriod: Record<string, number>
  subtotal: number
}

/** 按云厂商与付费方式汇总：一行一种付费方式，每朵云一个小计，末行两云合计 */
export function allocSummary(data: BillAllocationResponse): { periods: string[]; rows: AllocSummaryRow[] } {
  const periods = periodsBetween(data.from, data.to)
  const providers = (['alicloud', 'volcengine'] as BillProvider[]).filter((p) => data.monthly.some((r) => r.provider === p))
  const make = (key: string, label: string, tone: AllocSummaryRow['tone'], pred: (r: BillAllocMonthRow) => boolean): AllocSummaryRow => {
    const byPeriod: Record<string, number> = {}
    for (const p of periods) byPeriod[p] = data.monthly.filter(pred).reduce((n, r) => n + (r.by_period[p] ?? 0), 0)
    return { key, label, tone, byPeriod, subtotal: periods.reduce((n, p) => n + byPeriod[p], 0) }
  }
  const rows: AllocSummaryRow[] = []
  for (const p of providers) {
    const kinds = (['prepaid', 'postpaid'] as const).filter((k) => data.monthly.some((r) => r.provider === p && r.kind === k))
    for (const k of kinds) rows.push(make(`${p}:${k}`, `${PROVIDER_LABELS[p]} · ${KIND_LABELS[k]}`, 'plain', (r) => r.provider === p && r.kind === k))
    if (kinds.length > 1) rows.push(make(`${p}:sub`, `${PROVIDER_LABELS[p]}小计`, 'sub', (r) => r.provider === p))
  }
  rows.push(make('total', providers.length > 1 ? '两云合计' : '合计', 'total', () => true))
  return { periods, rows }
}
