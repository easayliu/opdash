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
  message: string
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

export interface FacetsResponse {
  field: string
  values: ValueCount[]
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
}

export interface OverviewResponse {
  from_ms: number
  to_ms: number
  services: ServiceStat[]
  stats: Stats
}

export interface OperationStat {
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
}

export interface OperationsResponse {
  service: string
  kind: 'entry' | 'client'
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
}

export interface TimeseriesResponse {
  service: string
  span_name?: string
  width_ms: number
  from_ms: number
  to_ms: number
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
