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
  opts: {
    service?: string
    spanName?: string
    errorOnly?: boolean
    sort?: 'time' | 'duration'
    kinds?: string
    /** span 属性过滤，`key=value`，可以多个 */
    attrs?: string[]
    /** 只看比这个慢的（毫秒） */
    minMs?: number
  },
  win?: Window,
): string {
  const base = qs(
    {
      service: opts.service,
      span_name: opts.spanName,
      error_only: opts.errorOnly ? 1 : null,
      sort: opts.sort === 'duration' ? 'duration' : null,
      kind: opts.kinds,
      min_ms: opts.minMs != null ? Math.max(0, Math.round(opts.minMs)) : null,
    },
    win,
  )
  // attr 是可重复的键，qs 那套一个键只留一个值，单独拼
  const attrs = (opts.attrs ?? []).map((a) => `attr=${encodeURIComponent(a)}`).join('&')
  return `/traces?${[base, attrs].filter(Boolean).join('&')}`
}

export function traceHref(traceId: string, atMs?: number, spanId?: string): string {
  return `/traces/${traceId}?${qs({ at: atMs ? Math.floor(atMs) : null, span: spanId })}`
}

/**
 * 日志。`dim` 是日志表上「服务」这一维的列名——老表没有 `service_name` 时是 `container`，
 * 取值来自 `/api/meta` 的 dimensions，所以由调用方传进来。
 */
export function logsHref(
  opts: {
    dim?: string
    service?: string
    levels?: string
    traceId?: string
    spanId?: string
    q?: string
    /** 额外的维度筛选（pod / namespace / container……），键必须是 `/api/meta` 给的维度列名 */
    dims?: Record<string, string>
  },
  win?: Window,
): string {
  const parts: Record<string, string | undefined> = {
    level: opts.levels,
    trace_id: opts.traceId,
    span_id: opts.spanId,
    q: opts.q,
    ...opts.dims,
  }
  if (opts.dim && opts.service) parts[opts.dim] = opts.service
  // 按 id 查日志不带时间范围：两张表的 trace_id 都有 bloom filter，点查不需要时间条件，
  // 带上反而会把三小时前的那条挡在外面
  return `/logs?${qs(parts, opts.traceId || opts.spanId ? undefined : win)}`
}

/* ------------------------------------------------ 指标某条线 → 另外两个信号的筛选条件 */

/**
 * 指标的标签键 → 链路那边的 span 属性键。两边都是 OTel 语义约定的同一套名字，所以基本是
 * 原样带过去；`http.route=/orders` 在指标上是标签，在 span 上就是 `span_attributes.http.route`。
 */
const TRACE_ATTR_KEYS = new Set([
  'http.route',
  'http.request.method',
  'http.method',
  'http.response.status_code',
  'http.status_code',
  'url.scheme',
  'http.scheme',
  'error.type',
  'server.address',
  'server.port',
  'net.peer.name',
  'db.system',
  'messaging.system',
  'messaging.destination.name',
])

/** 指标的 resource 标签（`res:` 前缀）→ 日志表的维度列 */
const LOG_DIM_KEYS: Record<string, string> = {
  'res:k8s.pod.name': 'pod',
  'res:k8s.namespace.name': 'namespace',
  'res:k8s.container.name': 'container',
  'res:host.name': 'host',
}

export interface SeriesContext {
  /** 这条线自己指明的服务（按 service_name 分组时） */
  service?: string
  /** 链路：`key=value` 属性过滤 */
  traceAttrs: string[]
  spanName?: string
  errorOnly: boolean
  /** 日志：维度筛选 */
  logDims: Record<string, string>
  /** 弹层上写给人看的一行，如 `http.route=/orders` */
  label: string
  /** 有没有能带过去的东西（没有就只剩服务 + 时间，链接也就没必要强调） */
  useful: boolean
}

/**
 * 把指标上一条线的标签翻译成另外两个信号认得的筛选条件。
 *
 * 这一步是「联动」真正有用的地方：光带服务和时间等于让人到了新页面再自己筛一遍，
 * 而面板明明知道你点的是哪个接口、哪个状态码、哪个 pod。
 */
export function seriesContext(labels: { key: string; value: string }[]): SeriesContext {
  const ctx: SeriesContext = { traceAttrs: [], errorOnly: false, logDims: {}, label: '', useful: false }
  const shown: string[] = []
  for (const { key, value } of labels) {
    if (!value) continue
    // 分位数是我们自己加的标签，不是数据上的维度
    if (key === 'quantile') continue
    shown.push(`${key}=${value}`)
    if (key === 'service_name') {
      ctx.service = value
      ctx.useful = true
    } else if (key === 'span.name') {
      ctx.spanName = value
      ctx.useful = true
    } else if (key === 'status.code') {
      // spanmetrics 的 status.code，值是 STATUS_CODE_ERROR / _OK / _UNSET
      if (value.toUpperCase().includes('ERROR')) {
        ctx.errorOnly = true
        ctx.useful = true
      }
    } else if (TRACE_ATTR_KEYS.has(key)) {
      ctx.traceAttrs.push(`${key}=${value}`)
      ctx.useful = true
    } else if (LOG_DIM_KEYS[key]) {
      ctx.logDims[LOG_DIM_KEYS[key]] = value
      ctx.useful = true
    }
  }
  ctx.label = shown.join(' · ')
  return ctx
}

/** 时长单位（OTLP 的 UCUM 写法）→ 毫秒的换算系数；不是时长就返回 null */
export function msFactor(unit: string | undefined): number | null {
  switch ((unit ?? '').trim()) {
    case 's':
      return 1000
    case 'ms':
      return 1
    case 'us':
      return 1 / 1000
    case 'ns':
      return 1 / 1e6
    default:
      return null
  }
}
