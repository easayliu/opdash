/** 后端 /api 的响应形状，和 src/api/*.rs 里的 Serialize 结构一一对应。 */

export interface Stats {
  read_rows: number
  read_bytes: number
  result_rows: number
  elapsed_ms: number
}

export interface ColumnMeta {
  name: string
  type: string
  kind: 'string' | 'int' | 'float' | 'date_time' | 'json' | 'map' | 'array' | 'other'
}

export interface TableMeta {
  table: string
  columns: ColumnMeta[]
  /** 固定列之外的字符串列：k8s 元数据、静态 fields，可作筛选维度 */
  dimensions: string[]
}

/** GET /api/auth/me：登录方式和当前用户。`mode === 'oidc'` 且 `user` 为空 = 该去登录了。 */
export interface AuthMe {
  mode: 'none' | 'basic' | 'oidc'
  user: { name: string; email: string | null } | null
  login_url: string | null
  logout_url: string | null
}

export interface Meta {
  version: string
  database: string
  timezone: string
  now_ms: number
  limits: {
    max_rows: number
    max_offset: number
    export_max_rows: number
    max_trace_spans: number
    max_range_ms: number
    query_timeout_ms: number
  }
  server: { version: string; timezone: string }
  logs: TableMeta
  traces: TableMeta
  /** metricpipe 的表；没部署就是 null，指标页不显示 */
  metrics: TableMeta | null
  /** 指标页没启用的原因 */
  metrics_note?: string
}

export interface LogRow {
  ts_ms: number
  level: string
  trace_id: string
  span_id: string
  thread: string
  logger: string
  /** 可能被服务端截断了，判断见 [`messageTruncated`]（ui/src/lib/log-row.ts） */
  message: string
  /** 截断前有多少字符。等于 message 的长度就是没截；导出那条路不带这个字段 */
  message_len?: number
  file: string
  host: string
  /** 动态列：service_name / namespace / pod / container / stream / cluster …… */
  [extra: string]: unknown
}

export interface LogSearchResponse {
  rows: LogRow[]
  total?: number
  /** 这些词按整词而不是子串匹配（走了 message 上的 token 索引） */
  token_terms?: string[]
  limit: number
  offset: number
  order: 'asc' | 'desc'
  stats: Stats
}

export interface HistogramBucket {
  t_ms: number
  total: number
  counts: Record<string, number>
}

export interface HistogramResponse {
  width_ms: number
  from_ms: number
  to_ms: number
  levels: string[]
  buckets: HistogramBucket[]
  /** 范围内总条数（各桶之和），和 count() 等价 */
  total: number
  stats: Stats
}

export interface ValueCount {
  value: string
  count: number
}

export interface Facet {
  field: string
  /** 计数是近似的（Space-Saving）：一条查询要同时算十来个维度，精确分组得一个维度扫一遍 */
  values: ValueCount[]
}

export interface FacetsResponse {
  facets: Facet[]
  stats: Stats
}

export interface ContextResponse {
  before: LogRow[]
  after: LogRow[]
  window_ms: number
  stats: Stats
}

export interface TraceSummary {
  trace_id: string
  start_us: number
  /** 请求耗时（根 span），纳秒 */
  duration_ns: number
  /** 整条 trace 的跨度（含异步消费），纳秒 */
  span_ns: number
  span_count: number
  error_count: number
  root_service: string
  root_name: string
  root_missing: boolean
  services: string[]
}

export interface TraceSearchResponse {
  traces: TraceSummary[]
  limit: number
  sort: 'time' | 'duration'
  stats: Stats
}

export interface HeatmapCell {
  t_ms: number
  /** 对数耗时档：耗时 ms 落在 [10^(lvl/bins), 10^((lvl+1)/bins))；最底一档（1µs）含更短的 */
  lvl: number
  count: number
  errors: number
}

export interface HeatmapResponse {
  from_ms: number
  to_ms: number
  width_ms: number
  bins_per_decade: number
  /** 实际参与统计的 span kind；没选时是入口 span */
  kinds: string[]
  /** 只有非空格子 */
  cells: HeatmapCell[]
  total: number
  max_count: number
  stats: Stats
}

export type AttrValue = string | number | boolean | null | AttrValue[] | { [k: string]: AttrValue }

export interface SpanEvent {
  ts_us: number
  name: string
  attributes: Record<string, AttrValue>
}

export interface SpanLink {
  trace_id: string
  span_id: string
  trace_state: string
  attributes: Record<string, AttrValue>
}

export interface Span {
  span_id: string
  parent_span_id: string
  service: string
  name: string
  kind: string
  start_us: number
  duration_ns: number
  status: string
  status_message: string
  scope_name: string
  scope_version: string
  trace_state: string
  attributes: Record<string, AttrValue>
  resource: Record<string, AttrValue>
  events: SpanEvent[]
  links: SpanLink[]
  extra: Record<string, AttrValue>
}

export interface TraceDetailResponse {
  trace_id: string
  spans: Span[]
  truncated: boolean
  /** 只查了开始时间附近的时间窗口（带 at 参数） */
  windowed: boolean
  /** 实际查的那一段时间（unix 毫秒）；不限时间时是 null */
  window_from_ms: number | null
  window_to_ms: number | null
  /** span 太多，宽窗口那一档装不下，退回了围着 at 的窄窗口——图上只有这一段 */
  narrowed: boolean
  /** 请求里 `span=` 指名的那个 span 被截断切掉了，服务端单独取回来钉在 `spans` 末尾 */
  pinned_span: string | null
  /** span 的属性 / events / links 没跟着回来，点开某个 span 时单独取（那四个 JSON 列是详情查询的全部成本） */
  attributes_lazy: boolean
  stats: Stats
}

export interface SpanAttrsResponse {
  trace_id: string
  span_id: string
  attributes: Record<string, AttrValue>
  resource: Record<string, AttrValue>
  events: SpanEvent[]
  links: SpanLink[]
  stats: Stats
}

export interface ValuesResponse {
  field: string
  values: ValueCount[]
  stats: Stats
}

export interface KeysResponse {
  scope: string
  keys: { key: string; count: number }[]
  stats: Stats
}

export interface PrevStat {
  requests: number
  errors: number
  error_rate: number
  rps: number
  p95_ms: number
}

export interface ServiceStat {
  service: string
  requests: number
  errors: number
  error_rate: number
  rps: number
  p50_ms: number
  p95_ms: number
  p99_ms: number
  max_ms: number
  /** 上一个同样长的时间窗；那段时间没这个服务就是 null */
  prev: PrevStat | null
  /** 迷你趋势，桶宽见 OverviewResponse.spark_width_ms；prev_requests 是对比窗口同一格的，画成灰影 */
  spark: { requests: number[]; errors: number[]; prev_requests: number[] }
}

export interface OverviewResponse {
  from_ms: number
  to_ms: number
  /** 对比窗口怎么取：prev 上一段 / day 昨天同时段 / week 上周同时段 */
  compare: Compare
  prev_from_ms: number
  prev_to_ms: number
  spark_width_ms: number
  services: ServiceStat[]
  stats: Stats
}

/**
 * 一种报错。`/api/errors` 把出错的 span 按「同一种报错」归堆，一行就是一堆。
 *
 * `exception` / `message` 只有 span 上带 exception 事件时才有（线上约五分之一）；
 * 没有的那些退到 `http_status`，两样都空就只剩「哪个接口在错」——具体异常要展开这一组，
 * 按样本 trace id 去日志里拿（被全局异常处理器吞掉的就是这一类）。
 */
export interface ErrorGroup {
  id: string
  service: string
  span_kind: string
  span_name: string
  /** 异常类全名，如 `java.net.SocketException`；没有就是空串 */
  exception: string
  /** 异常消息，服务端已截断到 160 字符 */
  message: string
  /** HTTP 响应码；没有就是空串 */
  http_status: string
  /** `server.address`：Client span 的 span_name 只有 `GET` / `POST`，靠它认对端 */
  peer: string
  count: number
  traces: number
  first_ms: number
  last_ms: number
  sample_trace: string
  sample_span: string
}

export interface ErrorsResponse {
  from_ms: number
  to_ms: number
  kind: string
  /** 这段时间出错的 span 总数 */
  total: number
  groups: ErrorGroup[]
  stats: Stats
}

/** 对比窗口怎么取。`none` 只有接口表和时间序列认，意思是不查对比窗口 */
export type Compare = 'prev' | 'day' | 'week'

/** 对比窗口里同一个接口的数 */
export interface PrevOp {
  requests: number
  errors: number
  error_rate: number
  rps: number
  p50_ms: number
  p95_ms: number
  p99_ms: number
}

export interface OperationStat {
  /** 一次问多个服务时按它分组（见 useServiceOperations）；单服务那条路上就是它自己 */
  service: string
  span_name: string
  kind: string
  requests: number
  errors: number
  error_rate: number
  rps: number
  p50_ms: number
  p95_ms: number
  p99_ms: number
  max_ms: number
  /** 对比窗口里的同一个接口；那段时间没有它（新接口）就是 null。
   *  反过来，对比窗口有、现在一次都没有的接口也会出现在表里，requests 是 0 */
  prev: PrevOp | null
}

export interface OperationsResponse {
  service: string
  kind: 'entry' | 'client'
  from_ms: number
  to_ms: number
  compare: Compare | 'none'
  prev_from_ms?: number
  prev_to_ms?: number
  operations: OperationStat[]
  stats: Stats
}

export interface TimeseriesPoint {
  t_ms: number
  requests: number
  errors: number
  p50_ms: number
  p95_ms: number
  p99_ms: number
  /** 对比窗口里相对位置相同的那一格；那一格没有请求就是 null（曲线在这里断开） */
  prev?: { requests: number; errors: number; p95_ms: number } | null
}

export interface TimeseriesResponse {
  service: string
  span_name?: string
  width_ms: number
  from_ms: number
  to_ms: number
  compare: Compare | 'none'
  prev_from_ms?: number
  prev_to_ms?: number
  points: TimeseriesPoint[]
  stats: Stats
}

/** 五种 OTLP 指标类型，同一张表里用 metric_type 区分 */
export type MetricType = 'Gauge' | 'Sum' | 'Histogram' | 'ExponentialHistogram' | 'Summary'

export type MetricAgg = 'avg' | 'sum' | 'min' | 'max' | 'last' | 'count' | 'rate' | 'increase' | 'mean' | 'quantile'

export type MetricField = 'value' | 'count' | 'sum' | 'min' | 'max'

export interface MetricInfo {
  name: string
  type: MetricType
  unit: string
  description: string
  /** Delta 的已经是增量，Cumulative 是进程启动以来的累计值，速率要相减 */
  temporality: 'Delta' | 'Cumulative' | 'Unspecified' | string
  /** counter（只增不减）。up-down counter 是 false */
  monotonic: boolean
  services: string[]
  points: number
}

export interface MetricCatalogResponse {
  /** 实际扫的窗口，可能比页面选的范围窄 */
  from_ms: number
  to_ms: number
  metrics: MetricInfo[]
  stats: Stats
}

export interface MetricNamesResponse {
  names: { name: string; count: number }[]
  stats: Stats
}

export interface MetricSeries {
  labels: { key: string; value: string }[]
  name: string
  /** 和 t_ms 等长，null = 这个桶没数据 */
  values: (number | null)[]
  min: number | null
  max: number | null
  avg: number | null
  last: number | null
}

export interface MetricQueryResponse {
  metric: string
  agg: MetricAgg
  field: MetricField
  by: string[]
  from_ms: number
  to_ms: number
  width_ms: number
  t_ms: number[]
  series: MetricSeries[]
  /** 时间线太多，只返回了最大的那些 */
  truncated: boolean
  stats: Stats
}

export interface MetricExemplar {
  t_ms: number
  value: number
  trace_id: string
  span_id: string
  service: string
}

export interface MetricExemplarsResponse {
  metric: string
  exemplars: MetricExemplar[]
  stats: Stats
}

/** 进程重启 / pod 启动，标在图上 */
export interface MetricEvent {
  t_ms: number
  /** restart = 累积 counter 掉回去了（原地重启）；start = pod 在窗口里第一次出现（新起 / 发布） */
  kind: 'restart' | 'start'
  pod: string
  /** 全站批量查时按这个分到服务上 */
  service: string
}

export interface MetricEventsResponse {
  metric: string
  events: MetricEvent[]
  stats: Stats
}
