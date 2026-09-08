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

export function formatCompact(n: number): string {
  if (!Number.isFinite(n)) return '-'
  const abs = Math.abs(n)
  if (abs >= 1e9) return `${(n / 1e9).toFixed(abs >= 1e10 ? 0 : 1)}G`
  if (abs >= 1e6) return `${(n / 1e6).toFixed(abs >= 1e7 ? 0 : 1)}M`
  if (abs >= 1e3) return `${(n / 1e3).toFixed(abs >= 1e4 ? 0 : 1)}K`
  if (Number.isInteger(n)) return String(n)
  return n.toFixed(abs < 10 ? 2 : 1)
}

/** 时间轴刻度：挑一个整齐的间隔，让刻度数在 4~8 个。 */
export function timeTicks(fromMs: number, toMs: number): number[] {
  const span = Math.max(1, toMs - fromMs)
  const steps = [
    1_000, 5_000, 10_000, 30_000, 60_000, 120_000, 300_000, 600_000, 900_000, 1_800_000, 3_600_000, 7_200_000,
    10_800_000, 21_600_000, 43_200_000, 86_400_000, 172_800_000, 604_800_000,
  ]
  const step = steps.find((s) => span / s <= 8) ?? steps[steps.length - 1]
  // 天级别的刻度对齐到本地零点，其余按 UTC 秒数取整（整点 / 整分在两种时区下一致）
  const offset = step >= 86_400_000 ? new Date(fromMs).getTimezoneOffset() * 60_000 : 0
  const first = Math.ceil((fromMs - offset) / step) * step + offset
  const ticks: number[] = []
  for (let t = first; t < toMs; t += step) ticks.push(t)
  return ticks
}
