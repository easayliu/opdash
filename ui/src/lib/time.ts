/**
 * 时间处理。后端一律 unix 毫秒（span 是微秒）；页面上按浏览器本地时区显示——
 * 业务开发和线上服务在同一个时区，用本地时区最不容易看错。
 */

export const QUICK_RANGES: { key: string; label: string; ms: number }[] = [
  { key: '5m', label: '5 分钟', ms: 5 * 60_000 },
  { key: '15m', label: '15 分钟', ms: 15 * 60_000 },
  { key: '1h', label: '1 小时', ms: 3_600_000 },
  { key: '3h', label: '3 小时', ms: 3 * 3_600_000 },
  { key: '6h', label: '6 小时', ms: 6 * 3_600_000 },
  { key: '24h', label: '24 小时', ms: 24 * 3_600_000 },
  { key: '3d', label: '3 天', ms: 3 * 86_400_000 },
  { key: '7d', label: '7 天', ms: 7 * 86_400_000 },
  { key: '30d', label: '30 天', ms: 30 * 86_400_000 },
]

export interface Range {
  fromMs: number
  toMs: number
  /** 相对范围的 key（'1h'），绝对范围为 null */
  relative: string | null
}

export const DEFAULT_RANGE_KEY = '1h'

/** 从 URL 参数解析范围：`range=1h`（相对，每次刷新按当前时间算）或 `from=&to=`（绝对毫秒）。 */
export function parseRange(params: URLSearchParams, now = Date.now()): Range {
  const from = Number(params.get('from'))
  const to = Number(params.get('to'))
  if (params.has('from') && params.has('to') && Number.isFinite(from) && Number.isFinite(to) && from < to) {
    return { fromMs: from, toMs: to, relative: null }
  }
  const key = params.get('range') ?? DEFAULT_RANGE_KEY
  const quick = QUICK_RANGES.find((q) => q.key === key) ?? QUICK_RANGES.find((q) => q.key === DEFAULT_RANGE_KEY)!
  return { fromMs: now - quick.ms, toMs: now, relative: quick.key }
}

/** 把范围写回 URL 参数（原地修改）。 */
export function writeRange(params: URLSearchParams, range: Range): void {
  if (range.relative) {
    params.set('range', range.relative)
    params.delete('from')
    params.delete('to')
  } else {
    params.delete('range')
    params.set('from', String(Math.floor(range.fromMs)))
    params.set('to', String(Math.ceil(range.toMs)))
  }
}

export function rangeLabel(range: Range): string {
  if (range.relative) {
    const q = QUICK_RANGES.find((q) => q.key === range.relative)
    return q ? `最近 ${q.label}` : range.relative
  }
  return `${formatTs(range.fromMs)} ~ ${formatTs(range.toMs)}`
}

const pad = (n: number, w = 2) => String(n).padStart(w, '0')

/** `2026-09-08 16:52:15.123`。`ms` 为 false 不带毫秒；`date` 为 false 只要时分秒。 */
export function formatTs(ms: number, opts: { ms?: boolean; date?: boolean } = {}): string {
  const d = new Date(ms)
  if (Number.isNaN(d.getTime())) return '-'
  const date = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`
  const time = `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
  const frac = opts.ms === false ? '' : `.${pad(d.getMilliseconds(), 3)}`
  return opts.date === false ? `${time}${frac}` : `${date} ${time}${frac}`
}

/** 微秒 → `16:52:15.123456`（span 时间戳，微秒精度） */
export function formatTsMicro(us: number, opts: { date?: boolean } = {}): string {
  const ms = Math.floor(us / 1000)
  const base = formatTs(ms, { ms: false, date: opts.date })
  return `${base}.${pad(us % 1_000_000, 6)}`
}

/** 纳秒 → 人读的时长：`1.23ms` / `4.56s` / `2m 3s` */
export function formatDuration(ns: number): string {
  if (!Number.isFinite(ns) || ns < 0) return '-'
  const us = ns / 1000
  if (us < 1000) return `${us < 10 ? us.toFixed(1) : Math.round(us)}µs`
  const ms = us / 1000
  if (ms < 1000) return `${ms < 10 ? ms.toFixed(2) : ms < 100 ? ms.toFixed(1) : Math.round(ms)}ms`
  const s = ms / 1000
  if (s < 60) return `${s < 10 ? s.toFixed(2) : s.toFixed(1)}s`
  const m = Math.floor(s / 60)
  return `${m}m ${Math.round(s - m * 60)}s`
}

export function formatDurationMs(ms: number): string {
  return formatDuration(ms * 1_000_000)
}

export function formatNumber(n: number): string {
  if (!Number.isFinite(n)) return '-'
  return n.toLocaleString('zh-CN')
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let v = n / 1024
  let i = 0
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i++
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`
}

/** `datetime-local` 输入框的值（本地时间，无时区） */
export function toLocalInputValue(ms: number): string {
  const d = new Date(ms)
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
}

export function fromLocalInputValue(s: string): number | null {
  const ms = new Date(s).getTime()
  return Number.isFinite(ms) ? ms : null
}

/** 桶宽（毫秒）→ 坐标轴刻度用的格式 */
export function formatTick(ms: number, widthMs: number): string {
  const d = new Date(ms)
  if (widthMs >= 86_400_000) return `${pad(d.getMonth() + 1)}-${pad(d.getDate())}`
  if (widthMs >= 3_600_000) return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:00`
  if (widthMs >= 60_000) return `${pad(d.getHours())}:${pad(d.getMinutes())}`
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
}
