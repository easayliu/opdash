/** 坐标轴的小工具：整齐的刻度、紧凑的数字。 */

/** 0 ~ max 之间挑 count 个左右的整齐刻度（1 / 2 / 5 × 10^n）。 */
export function niceTicks(max: number, count = 4): number[] {
  if (!(max > 0)) return [0]
  const rough = max / count
  const pow = Math.pow(10, Math.floor(Math.log10(rough)))
  const candidates = [1, 2, 5, 10].map((m) => m * pow)
  const step = candidates.find((c) => c >= rough) ?? candidates[candidates.length - 1]
  const ticks: number[] = []
  for (let v = 0; v <= max + 1e-9; v += step) ticks.push(Number(v.toFixed(10)))
  return ticks
}

/** 顶端留一点空气：最大值向上取到刻度 */
export function niceMax(max: number, count = 4): number {
  const ticks = niceTicks(max, count)
  const last = ticks[ticks.length - 1]
  return last >= max ? last : last + (ticks[1] ?? last)
}

/**
 * 不从 0 起的纵轴：上下各取到整刻度，把数据的起伏撑满画布。费用这类「每天都差不多」的量从 0 起画，
 * 涨一成只高出几个像素。全部相等时退回从 0 起——没有起伏可撑，硬撑只会把一条平线画到正中间
 */
export function niceRange(min: number, max: number, count = 4): { lo: number; hi: number; ticks: number[] } {
  if (!(max > min)) {
    const ticks = niceTicks(max, count)
    return { lo: 0, hi: niceMax(max, count), ticks }
  }
  const rough = (max - min) / count
  const pow = Math.pow(10, Math.floor(Math.log10(rough)))
  const step = [1, 2, 5, 10].map((m) => m * pow).find((c) => c >= rough) ?? 10 * pow
  const lo = Math.floor(min / step) * step
  const hi = Math.ceil(max / step) * step
  const ticks: number[] = []
  for (let v = lo; v <= hi + step * 1e-9; v += step) ticks.push(Number(v.toFixed(10)))
  return { lo, hi, ticks }
}

export function formatCompact(n: number): string {
  if (!Number.isFinite(n)) return '-'
  const abs = Math.abs(n)
  if (abs >= 1e9) return `${(n / 1e9).toFixed(abs >= 1e10 ? 0 : 1)}G`
  if (abs >= 1e6) return `${(n / 1e6).toFixed(abs >= 1e7 ? 0 : 1)}M`
  if (abs >= 1e3) return `${(n / 1e3).toFixed(abs >= 1e4 ? 0 : 1)}K`
  if (Number.isInteger(n)) return String(n)
  return n.toFixed(abs < 10 ? 2 : 1)
}

/** 11px 字号下数字和连字符大约 6.5px 一个字：估刻度文字宽度用。 */
const CHAR_PX = 6.5

/**
 * 横轴放得下几个刻度。刻度数原先固定 4~8 个，桌面上正好；手机上画布只剩 300px 上下，
 * 7 个「12:10:00」排在一起就叠成了一串。
 *
 * 按刻度文字的长度估：`formatTick` 按桶宽决定写法——天级「09-24」、小时级「09-24 12:00」、
 * 其余「12:10」——每个刻度留「字宽 + 20px」。秒级桶也按「12:10」估：刻度一旦落到整分钟上
 * 就不写秒（见 `tickUnit`），按带秒的长度估只会白白少放一两个；真到几分钟的跨度、刻度写到
 * 秒了，挤不下的由 `placeTicks` 省掉。宽屏照旧封顶 8 个。
 */
export function tickBudget(plotWidth: number, widthMs: number): number {
  const chars = widthMs >= 3_600_000 && widthMs < 86_400_000 ? 11 : 5
  return Math.min(8, Math.max(2, Math.floor(plotWidth / (chars * CHAR_PX + 20))))
}

/**
 * 时间刻度该写到哪一级：按桶宽写，但刻度本身落在整分钟上时不写秒。
 *
 * 近一小时的桶是 10 秒一格，只按桶宽的话刻度会写成「12:10:00」——末尾的 `:00` 永远是零，
 * 白占一截宽度，窄屏上正是它让相邻刻度挤到一起。
 */
export function tickUnit(widthMs: number, ticks: number[]): number {
  const step = ticks.length > 1 ? ticks[1] - ticks[0] : 0
  return widthMs < 60_000 && step >= 60_000 ? 60_000 : widthMs
}

export interface PlacedTick {
  t: number
  x: number
  text: string
  anchor: 'start' | 'middle' | 'end'
}

/**
 * 摆刻度文字：默认居中；贴着画布边缘的朝里对齐，免得半截字被裁掉（费用页按天看半年，
 * 最后一个「09-24」只剩「09-2」）；朝里挪了之后跟前一个撞上的，干脆不写——刻度只是读数
 * 的参照，少一个不丢信息，叠成一团才是真的读不出来。
 */
export function placeTicks(ticks: { t: number; x: number; text: string }[], svgWidth: number): PlacedTick[] {
  const out: PlacedTick[] = []
  let prevRight = -Infinity
  for (const { t, x, text } of ticks) {
    const w = text.length * CHAR_PX
    let anchor: PlacedTick['anchor'] = 'middle'
    let left = x - w / 2
    if (x + w / 2 > svgWidth) {
      anchor = 'end'
      left = x - w
    } else if (left < 0) {
      anchor = 'start'
      left = x
    }
    if (left < prevRight + 6) continue
    out.push({ t, x, text, anchor })
    prevRight = anchor === 'end' ? x : anchor === 'start' ? x + w : x + w / 2
  }
  return out
}

/** 时间轴刻度：挑一个整齐的间隔，让刻度数不超过 `maxCount`（默认 8 个，窄图见 `tickBudget`）。 */
export function timeTicks(fromMs: number, toMs: number, maxCount = 8): number[] {
  const span = Math.max(1, toMs - fromMs)
  const steps = [
    1_000, 5_000, 10_000, 30_000, 60_000, 120_000, 300_000, 600_000, 900_000, 1_800_000, 3_600_000, 7_200_000,
    10_800_000, 21_600_000, 43_200_000, 86_400_000, 172_800_000, 604_800_000,
    // 两周、四周、八周：只有费用页按天看半年账单时用得上，否则 176 天会排出 25 个刻度挤成一团；
    // 八周是给手机的，那里半年只放得下四五个刻度
    1_209_600_000, 2_419_200_000, 4_838_400_000,
  ]
  const step = steps.find((s) => span / s <= maxCount) ?? steps[steps.length - 1]
  // 天级别的刻度对齐到本地零点，其余按 UTC 秒数取整（整点 / 整分在两种时区下一致）
  const offset = step >= 86_400_000 ? new Date(fromMs).getTimezoneOffset() * 60_000 : 0
  const first = Math.ceil((fromMs - offset) / step) * step + offset
  const ticks: number[] = []
  for (let t = first; t < toMs; t += step) ticks.push(t)
  return ticks
}

/**
 * 横轴的定义域：`[fromMs, toMs]` 是**不够**的，它盖不住要画的那些桶。
 *
 * 后端按固定网格分桶（原点是本地零点），首尾两个桶因此会越界：
 * `first_index = floor((from - origin) / width)`，所以第一个桶最早能比 `from` 早整整一格；
 * 末桶同理会越过 `to`。把桶按 `[from, to]` 去定位，第一根柱子就画到了坐标轴左边的刻度栏里，
 * 正好压在「0」那个标签上——线上截图里就是这么糊的。
 *
 * 所以定义域取「请求的范围」和「实际拿到的桶」的并集。直方图的横轴本来就该是整格的。
 */
export function bucketDomain(fromMs: number, toMs: number, buckets: { t_ms: number }[], widthMs: number): { x0: number; x1: number } {
  const w = Math.max(1, widthMs)
  let x0 = fromMs
  let x1 = toMs
  // 扫一遍取最早 / 最晚，不假设入参有序：热力图的格子是按「时间桶 × 耗时档」聚合出来的，顺序不保证
  for (const b of buckets) {
    if (b.t_ms < x0) x0 = b.t_ms
    if (b.t_ms + w > x1) x1 = b.t_ms + w
  }
  return { x0, x1 }
}
