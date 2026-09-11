/**
 * 服务健康度：把一组数字变成一个颜色和几句原因。
 *
 * 阈值定得很朴素，目的不是精确告警，是让总览页**扫一眼就知道该点哪个**——和 Cloudflare 控制台
 * 一样，语义色用得省但全站一致：错误率、P95 涨幅在服务卡、服务表、看板顶部数字上用同一套判定。
 * 数字都带「和上一个同样长的时间窗比」：P95 = 928ms 本身不说明问题，「比上一小时高 3 倍」才说明。
 */
import type { ServiceStat } from '@/api/types'

export type Health = 'ok' | 'warn' | 'bad'

/** 错误率：≥ 5% 红、≥ 1% 黄 */
export const ERROR_RATE_BAD = 0.05
export const ERROR_RATE_WARN = 0.01
/** P95 比上一周期涨到这么多倍算异常；太短的请求（< 200ms）涨几倍也不算 */
const P95_RATIO_BAD = 3
const P95_RATIO_WARN = 1.5
const P95_FLOOR_MS = 200
/**
 * 按比例判 P95 之前，两个窗口都得有这么多请求。低流量服务几条慢请求就能把 P95 顶上去，
 * 那是统计噪声不是事故——线上 6 个「异常」清一色是每小时几百到几千次的服务，就是这么来的。
 */
export const MIN_SAMPLES = 300

/**
 * 这个 P95 值不值得当延迟看。消息确认类的入口 span 几乎零耗时（几微秒），显示成 `4.1µs −5%`
 * 像 bug；这种服务的延迟一列直接不显示。
 */
export function meaningfulLatency(p95Ms: number): boolean {
  return p95Ms >= 1
}

export function serviceHealth(s: ServiceStat): { level: Health; reasons: string[] } {
  const reasons: string[] = []
  let level: Health = 'ok'
  const bump = (to: Health) => {
    if (to === 'bad' || (to === 'warn' && level === 'ok')) level = to
  }
  if (s.error_rate >= ERROR_RATE_BAD) {
    bump('bad')
    reasons.push(`错误率 ${pct(s.error_rate)}`)
  } else if (s.error_rate >= ERROR_RATE_WARN) {
    bump('warn')
    reasons.push(`错误率 ${pct(s.error_rate)}`)
  }
  const enough = s.requests >= MIN_SAMPLES && (s.prev?.requests ?? 0) >= MIN_SAMPLES
  const ratio = s.prev && s.prev.p95_ms > 0 ? s.p95_ms / s.prev.p95_ms : null
  if (enough && ratio != null && s.p95_ms >= P95_FLOOR_MS) {
    if (ratio >= P95_RATIO_BAD) {
      bump('bad')
      reasons.push(`P95 是对比时段的 ${ratio.toFixed(1)} 倍`)
    } else if (ratio >= P95_RATIO_WARN) {
      bump('warn')
      reasons.push(`P95 是对比时段的 ${ratio.toFixed(1)} 倍`)
    }
  }
  // 对比时段没错、现在开始出错：哪怕比例还没到 1% 也值得看一眼
  if (s.prev && s.prev.errors === 0 && s.errors > 0 && s.error_rate >= 0.002) {
    bump('warn')
    reasons.push('对比时段没有错误，现在开始出错')
  }
  return { level, reasons }
}

export function pct(rate: number): string {
  const p = rate * 100
  if (p === 0) return '0%'
  if (p < 0.1) return '<0.1%'
  return `${p.toFixed(p < 10 ? 1 : 0)}%`
}

/**
 * 变化幅度。null = 没法比（上一周期没这个服务）；Infinity = 上一周期是 0、这一周期有了
 * （错误率从 0 变成有，这是最该被看见的一种变化，不能显示成「—」）
 */
export function change(cur: number, prev: number | null | undefined): number | null {
  if (prev == null || !Number.isFinite(prev)) return null
  if (prev === 0) return cur === 0 ? 0 : Infinity
  return cur / prev - 1
}

/** `+12%` / `−8%` / `3.1×` / `新增`；变化超过 ±100% 用倍数写，更直观 */
export function formatChange(delta: number | null): string {
  if (delta == null) return '—'
  if (delta === Infinity) return '新增'
  if (Math.abs(delta) < 0.005) return '±0%'
  if (delta >= 1) return `${(delta + 1).toFixed(1)}×`
  const sign = delta > 0 ? '+' : '−'
  return `${sign}${Math.round(Math.abs(delta) * 100)}%`
}

/**
 * 变化该用什么颜色。`good` 是「涨了算好还是坏」：错误率、延迟涨了是坏事；请求量涨跌都不算好坏，
 * 只给中性色。
 */
export function changeTone(delta: number | null, upIs: 'bad' | 'neutral'): 'danger' | 'ok' | 'muted' {
  if (delta == null || Math.abs(delta) < 0.05 || upIs === 'neutral') return 'muted'
  return delta > 0 ? 'danger' : 'ok'
}

/** 排序用：坏的排前面，同级按流量 */
export function healthRank(h: Health): number {
  return h === 'bad' ? 0 : h === 'warn' ? 1 : 2
}
