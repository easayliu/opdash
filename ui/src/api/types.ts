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
