/**
 * 分析视图「产品费用对比」的数字：两段等长的日期区间里各产品花了多少，多了还是少了。
 *
 * 几种比法共用一套：本段是截止日往前 N 天，对比段是再往前 N 天；「与上周同日比」则是截止日
 * 与 7 天前那一天。区间都按 `days` 的下标切——接口给的是连续的每一天，没有账单的日子也占一格。
 *
 * 默认截止到哪一天是这里最要紧的判断。两朵云的出账进度常不一致（阿里云的日度账单只到前天、
 * 火山的已经出到今天），截止到「一朵云已出、另一朵未出」的一天，没出账的那朵云的产品会整片
 * 显示为「降 100%」。所以默认取各云都已出账的最后一天；今天的账单必然未出齐，也不作默认。
 */
import type { BillProductDaysResponse, BillProvider } from '@/api/types'

export type CompareMode = '1' | '7' | '14' | '30' | 'week'

export const COMPARE_MODES: { value: CompareMode; label: string }[] = [
  { value: '1', label: '与前一日比' },
  { value: 'week', label: '与上周同日比' },
  { value: '7', label: '近 7 天与前 7 天' },
  { value: '14', label: '近 14 天与前 14 天' },
  { value: '30', label: '近 30 天与前 30 天' },
]

/** 下标闭区间 */
type Span = [number, number]

/** 截止日下标为 `i` 时两段的范围 */
function plan(mode: CompareMode, i: number): { current: Span; previous: Span } {
  if (mode === 'week') return { current: [i, i], previous: [i - 7, i - 7] }
  const n = Number(mode)
  return { current: [i - n + 1, i], previous: [i - 2 * n + 1, i - n] }
}

/** 截止日至少得是第几天（下标），对比段才完整落在数据之内 */
const minEnd = (mode: CompareMode) => (mode === 'week' ? 7 : 2 * Number(mode) - 1)

/** 可选的截止日，新的在前：对比段须完整落在数据之内，且不晚于任一朵云出账的最后一天 */
export function endOptions(pd: BillProductDaysResponse, mode: CompareMode): string[] {
  const lasts = Object.values(pd.last_by_provider).filter((d): d is string => !!d)
  if (!lasts.length) return []
  const latest = lasts.reduce((a, b) => (a > b ? a : b))
  const out: string[] = []
  for (let i = pd.days.length - 1; i >= minEnd(mode); i--) if (pd.days[i] <= latest) out.push(pd.days[i])
  return out
}

/** 默认截止日：各云都已出账、且不是今天的最后一天；没有这样的一天时退为可选的最新一天 */
export function defaultEnd(pd: BillProductDaysResponse, mode: CompareMode): string | null {
  const options = endOptions(pd, mode)
  const lasts = Object.values(pd.last_by_provider).filter((d): d is string => !!d)
  const cap = lasts.reduce((a, b) => (a < b ? a : b), lasts[0] ?? '')
  return options.find((d) => d < pd.today && d <= cap) ?? options[0] ?? null
}

export interface ProductDiff {
  provider: BillProvider
  product: string
  current: number
  previous: number
  delta: number
}

export interface Range {
  from: string
  to: string
  /** 区间里没有任何账单的日子。多半是那几天的日度账单没同步 */
  gaps: string[]
}

export interface Comparison {
  mode: CompareMode
  current: Range
  previous: Range
  /**
   * 比的是合计还是日均。两段有账单的天数不同（某段缺了几天）时合计没法直接比，改按各自
   * 有账单的天数折算成日均；单日的比法没有折算的余地，照旧比金额
   */
  basis: 'total' | 'daily'
  rows: ProductDiff[]
  currentTotal: number
  previousTotal: number
  /** 账单还没出到截止日的云：它们的产品在本段偏低，比出来的「下降」不是真的下降 */
  pending: BillProvider[]
}

/** 按 `mode` 比对截止到 `end` 的本段与对比段。两段都分文未花的产品不列 */
export function compare(pd: BillProductDaysResponse, mode: CompareMode, end: string): Comparison {
  const i = pd.days.indexOf(end)
  const p = plan(mode, i)
  const sum = (a: number[], [s, e]: Span) => {
    let v = 0
    for (let k = Math.max(0, s); k <= e; k++) v += a[k] ?? 0
    return v
  }
  // 一天一个合计，找出哪几天一分钱账单都没有
  const perDay = pd.days.map((_, k) => pd.rows.reduce((a, r) => a + (r.amounts[k] ?? 0), 0))
  const range = ([s, e]: Span): Range => ({
    from: pd.days[Math.max(0, s)] ?? '',
    to: pd.days[e] ?? '',
    gaps: pd.days.slice(Math.max(0, s), e + 1).filter((_, k) => perDay[Math.max(0, s) + k] === 0),
  })
  const current = range(p.current)
  const previous = range(p.previous)
  const width = p.current[1] - p.current[0] + 1
  const curDays = width - current.gaps.length
  const prevDays = width - previous.gaps.length
  const basis = width > 1 && curDays > 0 && prevDays > 0 && curDays !== prevDays ? 'daily' : 'total'
  const scale = (v: number, days: number) => (basis === 'daily' ? v / days : v)
  const rows = pd.rows
    .map((r) => {
      const cur = scale(sum(r.amounts, p.current), curDays)
      const prev = scale(sum(r.amounts, p.previous), prevDays)
      return {
        provider: r.provider,
        product: r.product,
        current: cur,
        previous: prev,
        delta: cur - prev,
      }
    })
    .filter((r) => r.current !== 0 || r.previous !== 0)
  const total = (k: 'current' | 'previous') => rows.reduce((a, r) => a + r[k], 0)
  const pending = (Object.entries(pd.last_by_provider) as [BillProvider, string][]).filter(([, last]) => last < end).map(([prov]) => prov)
  return { mode, current, previous, basis, rows, currentTotal: total('current'), previousTotal: total('previous'), pending }
}

/** 走势图上的一天 */
export interface TrendPoint {
  day: string
  value: number
  /** 按区间比时，对比段里与它对齐的那一天 */
  previousDay?: string
  previous?: number
}

export interface Trend {
  points: TrendPoint[]
  /** 单日的比法下被比较的那两天，图上画竖线标出 */
  marks: string[]
}

/**
 * 某个产品点开后的逐日走势。
 *
 * 按区间比（近 N 天与前 N 天）时，横轴是本段的 N 天，对比段按天对齐叠上去：第 k 天对第 k 天，
 * 高出的是哪几天一眼看得出。单日的比法（与前一日、与上周同日）只有两个点可叠，改画截止日之前
 * 14 天的一条线，把被比较的两天标出来——看得出那一天是突变还是本来就在涨。
 */
export function trend(pd: BillProductDaysResponse, mode: CompareMode, end: string, amounts: number[]): Trend {
  const i = pd.days.indexOf(end)
  if (mode === '1' || mode === 'week') {
    const from = Math.max(0, i - 13)
    const points = pd.days.slice(from, i + 1).map((day, k) => ({ day, value: amounts[from + k] ?? 0 }))
    const { previous } = plan(mode, i)
    return { points, marks: [pd.days[previous[0]], end].filter(Boolean) }
  }
  const { current, previous } = plan(mode, i)
  const points = pd.days.slice(current[0], current[1] + 1).map((day, k) => ({
    day,
    value: amounts[current[0] + k] ?? 0,
    previousDay: pd.days[previous[0] + k],
    previous: amounts[previous[0] + k] ?? 0,
  }))
  return { points, marks: [] }
}
