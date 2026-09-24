/**
 * 费用页的小工具：账期算术、金额格式、维度和口径的名字。
 *
 * 账期是 `YYYY-MM` 字符串，全程不转成 Date——账单里的「9 月」是账期而非某个时刻，
 * 转成 Date 再格式化只会在时区上出错（`new Date('2026-09')` 为 UTC 零点，东八区会显示成 8 月）。
 */
import type { BillAmount, BillProvider } from '@/api/types'
import { seriesVar } from './colors'

/** 金额口径。和后端 `Amount` 一一对应 */
export const AMOUNTS: { value: BillAmount; label: string; hint: string }[] = [
  { value: 'payable', label: '应付', hint: '优惠与代金券抵扣后的应付金额（火山引擎 PayableAmount / 阿里云 pretax_amount）' },
  { value: 'paid', label: '现金', hint: '实际支付的现金金额（火山引擎 PaidAmount / 阿里云 payment_amount）' },
  { value: 'original', label: '原价', hint: '折扣前的原始金额（火山引擎 OriginalBillAmount / 阿里云 pretax_gross_amount）' },
]

/** 排行维度。和后端 `Dimension` 一一对应，顺序就是下拉里的顺序 */
export const DIMENSIONS: { value: string; label: string }[] = [
  { value: 'product', label: '产品' },
  { value: 'item', label: '计费项' },
  { value: 'region', label: '地域' },
  { value: 'zone', label: '可用区' },
  { value: 'account', label: '账号' },
  { value: 'instance', label: '实例' },
  { value: 'project', label: '项目 / 资源组' },
  { value: 'subscription', label: '计费模式' },
  { value: 'currency', label: '币种' },
]

/**
 * 一个维度筛选在 URL 上的值拆成几个：同一维度选多个时写成 `product=A,B`，与接口的约定一致
 * （后端按逗号拆开，同一维度的几个值之间是「或」）。本身带逗号的值因此没法单独筛
 */
export const splitDimValues = (raw: string | null | undefined): string[] => [
  ...new Set(
    (raw ?? '')
      .split(',')
      .map((v) => v.trim())
      .filter(Boolean),
  ),
]

/** 反过来：写回 URL 的值，一个都没有时为 null（删掉这个键） */
export const joinDimValues = (values: string[]): string | null => values.join(',') || null

export const PROVIDER_LABELS: Record<BillProvider, string> = {
  volcengine: '火山引擎',
  alicloud: '阿里云',
}

/** 每朵云在图上的颜色。用图表调色板的前两槽，深浅主题都对 */
export const PROVIDER_COLORS: Record<BillProvider, string> = {
  volcengine: seriesVar(0),
  alicloud: seriesVar(1),
}

/** `YYYY-MM` → 从 0 年 1 月数起的序号，方便加减。不合法回 null */
function monthIndex(period: string): number | null {
  const m = /^(\d{4})-(\d{2})$/.exec(period.trim())
  if (!m) return null
  const month = Number(m[2])
  if (month < 1 || month > 12) return null
  return Number(m[1]) * 12 + month - 1
}

function periodOf(index: number): string {
  const year = Math.floor(index / 12)
  const month = (index % 12) + 1
  return `${String(year).padStart(4, '0')}-${String(month).padStart(2, '0')}`
}

/** 账期加减月份：`shiftPeriod('2026-01', -1)` = `2025-12`。格式不合法时原样返回 */
export function shiftPeriod(period: string, months: number): string {
  const i = monthIndex(period)
  if (i === null) return period
  return periodOf(Math.max(0, i + months))
}

/** 两个账期之间差几个月（含两端就是 +1）。不合法回 0 */
export function periodSpan(from: string, to: string): number {
  const a = monthIndex(from)
  const b = monthIndex(to)
  if (a === null || b === null || b < a) return 0
  return b - a + 1
}

/** 区间里的每个账期，从早到晚 */
export function periodsBetween(from: string, to: string): string[] {
  const a = monthIndex(from)
  const n = periodSpan(from, to)
  if (a === null || n === 0) return []
  return Array.from({ length: n }, (_, i) => periodOf(a + i))
}

/**
 * 这个账期有几天。月度预估就是「日均 × 它」。
 *
 * 用 `Date` 只做这一次算术：取下个月的第 0 天即本月最后一天，闰年二月也对。
 * 传入的是账期而非时刻，`Date.UTC` 与本地时区在这里没有差别。
 */
export function daysInMonth(period: string): number {
  const m = /^(\d{4})-(\d{2})$/.exec(period.trim())
  if (!m) return 30
  return new Date(Date.UTC(Number(m[1]), Number(m[2]), 0)).getUTCDate()
}

/** 图上的短标签：`2026-09` → `9月`，一月带上年份当分界 */
export function periodTick(period: string): string {
  const m = /^(\d{4})-(\d{2})$/.exec(period)
  if (!m) return period
  const month = Number(m[2])
  return month === 1 ? `${m[1].slice(2)}年1月` : `${month}月`
}

/** 按天那张图上的短标签：`2026-09-01` → `9/1` */
/**
 * 把一串账期摆成等宽的桶，交给按时间分桶的 `StackedBars` 画。
 *
 * 自然月长短不一（28 至 31 天），按真实时间摆，2 月那根柱子的位置会偏，悬停时按固定桶宽换算
 * 出的下标也会逐月漂移，一年下来就指错了月份。所以把首月 1 日到末月月底这段时间均分，
 * 第 i 个账期占第 i 格：读屏念出的起止时间仍然是真的，柱子之间则等距。
 */
export function periodSlots(periods: string[]): { fromMs: number; toMs: number; widthMs: number; at: (i: number) => number; index: (tMs: number) => number } {
  const start = (p: string) => {
    const [y, m] = p.split('-').map(Number)
    return new Date(y, (m || 1) - 1, 1).getTime()
  }
  if (!periods.length) return { fromMs: 0, toMs: 1, widthMs: 1, at: () => 0, index: () => -1 }
  const fromMs = start(periods[0])
  const toMs = start(shiftPeriod(periods[periods.length - 1], 1))
  const widthMs = (toMs - fromMs) / periods.length
  return {
    fromMs,
    toMs,
    widthMs,
    at: (i) => fromMs + i * widthMs,
    index: (tMs) => Math.round((tMs - fromMs) / widthMs),
  }
}

export function dayTick(day: string): string {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(day)
  return m ? `${Number(m[2])}/${Number(m[3])}` : day
}

const MONEY = new Intl.NumberFormat('zh-CN', { minimumFractionDigits: 2, maximumFractionDigits: 2 })
const MONEY_COMPACT = new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 1 })

/**
 * 金额。**不附币种符号**：币种记录在数据中（`currency` 列），两朵云、不同账号都可能不同，
 * 页面统一写作 ¥ 即与事实不符；需要确认币种时，按「币种」维度排行查看。
 */
export function formatMoney(amount: number): string {
  return MONEY.format(amount)
}

/** 卡片与坐标轴上的短金额：上万显示为 `1.2万`，小额仍保留两位小数 */
export function formatMoneyShort(amount: number): string {
  const abs = Math.abs(amount)
  if (abs >= 100_000_000) return `${MONEY_COMPACT.format(amount / 100_000_000)}亿`
  if (abs >= 10_000) return `${MONEY_COMPACT.format(amount / 10_000)}万`
  return MONEY.format(amount)
}

/**
 * 纵轴刻度用的金额：不足一万时去掉小数（「5,000」而不是「5,000.00」），图表左侧只留了
 * 48px，带两位小数的五位数会被截掉开头；一万以上同 `formatMoneyShort`。
 */
export function formatMoneyTick(amount: number): string {
  if (Math.abs(amount) >= 10_000) return formatMoneyShort(amount)
  return Number.isInteger(amount) ? amount.toLocaleString('zh-CN') : String(Number(amount.toFixed(2)))
}

/** 环比：`(本期 - 上期) / 上期`。上期为 0（无账单）时返回 null，避免显示 ∞ */
export function changeRatio(current: number, previous: number): number | null {
  if (!previous) return null
  return (current - previous) / previous
}

/** `+12.3%` / `-4.0%`；null 显示成「—」 */
export function formatChange(ratio: number | null): string {
  if (ratio === null) return '—'
  const pct = ratio * 100
  const sign = pct > 0 ? '+' : ''
  return `${sign}${pct.toFixed(1)}%`
}
