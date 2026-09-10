/**
 * 页面之间互相跳的地址，都在这里拼。
 *
 * 三个信号（日志 / 链路 / 指标）加服务概览，两两之间都要能跳，而且**跳过去看到的得是同一段
 * 时间、同一个服务**——散在各个页面里手写 query string 迟早会有一处忘了带 `from` / `to`。
 *
 * 从「一个时刻」（一条日志、一个 span）跳到按时间段看的页面时，前后各放宽
 * [`WINDOW_AROUND_MS`]：只给那一毫秒的话，指标图上一个点都没有；放宽到半小时才看得出
 * 尖峰是从什么时候开始的。
 */

/** 从一个时刻跳到时间段视图时，前后各放宽多少 */
export const WINDOW_AROUND_MS = 15 * 60_000

export interface Window {
  fromMs: number
  toMs: number
}

/** 一个时刻 → 前后放宽的时间窗 */
export function around(tsMs: number, spanMs = WINDOW_AROUND_MS): Window {
  return { fromMs: Math.max(0, Math.round(tsMs - spanMs)), toMs: Math.round(tsMs + spanMs) }
}

function qs(parts: Record<string, string | number | boolean | null | undefined>, win?: Window): string {
  const p = new URLSearchParams()
  for (const [k, v] of Object.entries(parts)) {
    if (v === null || v === undefined || v === '' || v === false) continue
    p.set(k, String(v))
  }
  if (win) {
    p.set('from', String(Math.floor(win.fromMs)))
    p.set('to', String(Math.ceil(win.toMs)))
  }
  return p.toString()
}

/** 指标看板（服务看板那一页） */
export function metricsHref(service: string, win?: Window): string {
  return `/metrics?${qs({ service }, win)}`
}

/** 指标里的某一个指标（「全部指标」那一页） */
export function metricHref(service: string, metric: string, win?: Window): string {
  return `/metrics?${qs({ view: 'all', service, metric }, win)}`
}

/** 服务概览（按链路算出来的请求量 / 错误率 / 分位数） */
export function serviceHref(service: string, win?: Window): string {
  return `/services/${encodeURIComponent(service)}?${qs({}, win)}`
}

export function tracesHref(
  opts: { service?: string; spanName?: string; errorOnly?: boolean; sort?: 'time' | 'duration'; kinds?: string },
  win?: Window,
): string {
  return `/traces?${qs(
    {
      service: opts.service,
      span_name: opts.spanName,
      error_only: opts.errorOnly ? 1 : null,
      sort: opts.sort === 'duration' ? 'duration' : null,
      kind: opts.kinds,
    },
    win,
  )}`
}

export function traceHref(traceId: string, atMs?: number, spanId?: string): string {
  return `/traces/${traceId}?${qs({ at: atMs ? Math.floor(atMs) : null, span: spanId })}`
}

/**
 * 日志。`dim` 是日志表上「服务」这一维的列名——老表没有 `service_name` 时是 `container`，
 * 取值来自 `/api/meta` 的 dimensions，所以由调用方传进来。
 */
export function logsHref(
  opts: { dim?: string; service?: string; levels?: string; traceId?: string; spanId?: string; q?: string },
  win?: Window,
): string {
  const parts: Record<string, string | undefined> = {
    level: opts.levels,
    trace_id: opts.traceId,
    span_id: opts.spanId,
    q: opts.q,
  }
  if (opts.dim && opts.service) parts[opts.dim] = opts.service
  // 按 id 查日志不带时间范围：两张表的 trace_id 都有 bloom filter，点查不需要时间条件，
  // 带上反而会把三小时前的那条挡在外面
  return `/logs?${qs(parts, opts.traceId || opts.spanId ? undefined : win)}`
}
