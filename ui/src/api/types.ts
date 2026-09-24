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
  /** `name` 是给人看的名字：OIDC 给的姓名（中文名优先） */
  user: { name: string; email: string | null } | null
  /** 这个请求是怎么认出来的；没认出来是 null */
  identity: 'session' | 'basic' | 'api_key' | null
  login_url: string | null
  logout_url: string | null
  /** 这个身份能不能管理 API key；没开认证、或者本身就是拿 key 进来的，是 null */
  api_keys: { max_ttl: string } | null
}

/** GET /api/auth/keys 里的一把 key：没有 key 本身（服务端只存哈希）。 */
export interface ApiKeyInfo {
  id: string
  name: string
  user: string
  /** `opdash_<id>.`，拿它和配置里的 key 对号 */
  prefix: string
  created_at: string
  expires_at: string
  last_used_at?: string
  expired: boolean
}

/** POST /api/auth/keys：刚签出来的 API key，`key` 只给这一次。 */
export interface ApiKeyCreated extends ApiKeyInfo {
  key: string
  expires_in: string
  mcp_url: string
}

/** GET /api/saved 里的一条收藏：一个页面地址（路径 + 查询串）加名字，全是本人的。 */
export interface SavedQuery {
  id: string
  name: string
  /** `/logs` / `/traces` / `/services/order` 这样 */
  path: string
  /** 不带开头的 `?`，可以为空 */
  query: string
  created_at: string
  updated_at: string
}

export interface SavedQueryList {
  queries: SavedQuery[]
  /** 每个人最多多少条 */
  max: number
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
  /** goscan 的账单表；没部署就是 null，费用页不显示 */
  bills: BillsMeta | null
  /** 费用页（或其中某一张表）没启用的原因 */
  bills_note?: string
}

export type BillProvider = 'volcengine' | 'alicloud'
/** 金额口径：应付（优惠后）/ 现金支付 / 原价 */
export type BillAmount = 'payable' | 'paid' | 'original'

export interface BillsMeta {
  /** 有账单表的云 */
  providers: BillProvider[]
  /** 能按天看的云：阿里云要同步了日度账单才在里面 */
  daily_providers: BillProvider[]
  volcengine: TableMeta | null
  alicloud_monthly: TableMeta | null
  alicloud_daily: TableMeta | null
  /** 查询时怎么去重，见后端 --bill-dedupe */
  dedupe: 'group' | 'final' | 'off'
  /** 配了 --goscan-url 才能在页面上手动拉账单 */
  sync: boolean
  /** 成本归属规则（后端 --bill-alloc）；没配就是 null，分析视图只给按产品的日均 */
  allocation: BillAllocationMeta | null
}

export interface BillAllocationMeta {
  /** 业务线，顺序即页面上的顺序 */
  lines: string[]
  rules: number
}

/** POST /api/bills/sync：同步任务已经登记，账单要等 goscan 后台拉完才进库 */
export interface BillSyncStarted {
  task_id: string
  provider: BillProvider
  from: string
  to: string
  message: string
}

/**
 * 同步跑到哪了。单位是「趟」——一个账期一种粒度算一趟。
 *
 * 阿里云选「月度 + 日度」时同一个账期要拉两趟（月表一趟、日表一趟），
 * 所以总数是账期数乘以粒度数。
 */
export interface BillSyncProgress {
  /** 正在拉的账期 */
  period: string
  /** 这一趟写哪张表；火山不分粒度，老版本 goscan 也不报，此时没有这个字段 */
  granularity?: 'monthly' | 'daily'
  periods_done: number
  periods_total: number
  /** 这一趟已经写入的行数；一趟可能要跑好几分钟，靠它看出还在动。老版本 goscan 不报，为 0 */
  records: number
  /** 这一趟接口报的总行数；按天拉整月时事先不知道，此时没有这个字段 */
  records_total?: number
}

/** GET /api/bills/sync/{task_id}，事件流里每一帧 `task` 也是这个形状 */
export interface BillSyncTask {
  id: string
  status: 'pending' | 'running' | 'completed' | 'failed' | 'cancelled'
  provider: string
  /** 跑完了没有（不管成没成） */
  done: boolean
  /** 跑完且成功 */
  ok: boolean
  /** 写进库的条数 */
  records: number
  /** 从云厂商取回来的条数 */
  fetched: number
  message: string
  error?: string
  started_at?: string
  ended_at?: string
  /** 老版本 goscan 不报进度，这里就是 null，页面退回不确定进度条 */
  progress?: BillSyncProgress
  /** 有人请它停下了：它会把手上这一趟写完再停，这期间 status 仍是 running */
  cancel_requested: boolean
  /** 被停下的同步没跑的那几趟，如 `2026-04 daily`；这些账期的数据原样没动 */
  not_run?: string[]
  /** 任务发起时的账期区间；接上一个已经在跑的任务时据此说明它在拉什么 */
  from?: string
  to?: string
}

/** GET /api/bills/sync/running：这朵云正在进行的同步，没有就是 null */
export interface BillSyncRunning {
  task: BillSyncTask | null
}

/** 一个账期（`2026-09`）或一天（`2026-09-01`）的花费 */
export interface BillPoint {
  t: string
  total: number
  by_provider: Partial<Record<BillProvider, number>>
}

export interface BillPeriodsResponse {
  periods: string[]
  latest: string | null
  providers: BillProvider[]
  stats: Stats
}

export interface BillSummaryResponse {
  from: string
  to: string
  amount: BillAmount
  /** 请求的账期一个不少，没数据的是 0 */
  points: BillPoint[]
  total: number
  by_provider: Partial<Record<BillProvider, number>>
  providers: BillProvider[]
  stats: Stats
}

export interface BillDailyResponse extends Omit<BillSummaryResponse, 'by_provider'> {
  providers: BillProvider[]
}

export interface BillBreakdownRow {
  key: string
  amount: number
  /** 占总额的比例，0~1 */
  share: number
  by_provider: Partial<Record<BillProvider, number>>
}

export interface BillBreakdownResponse {
  by: string
  label: string
  from: string
  to: string
  amount: BillAmount
  rows: BillBreakdownRow[]
  /** 没进排行的那些加起来 */
  other: number
  total: number
  stats: Stats
}

export interface BillDetailRow {
  provider: BillProvider
  period: string
  /** 月度账单没有日期，是空串 */
  day: string
  product: string
  item: string
  instance_id: string
  instance: string
  region: string
  account: string
  project: string
  subscription: string
  usage: string
  usage_unit: string
  currency: string
  amount: number
  original: number
  paid: number
}

/** 分析视图里的一行：某条业务线里的某个产品，或不分业务线时的某个产品 */
export interface BillAllocItem {
  product: string
  /** 按哪条归属规则归来的；未命中任何规则时是 null */
  rule: string | null
  amount: number
  /** 日均；预付费的摊销按月计，以及没有日粒度的账单时，都是 null */
  daily: number | null
  share: number
  /** 这一行是预付费摊销过来的，不是当期实际出账 */
  prepaid: boolean
}

export interface BillAllocLine {
  name: string
  /** 区间合计 = 后付费实际出账 + 落在区间内的预付费摊销 */
  amount: number
  postpaid: number
  amortized: number
  /** 日均，只按后付费算 */
  daily: number | null
  share: number
  /** 各账期的预付费摊销额，含区间之后的若干个月，页面据此算月度预估 */
  amortized_by_period: Record<string, number>
  items: BillAllocItem[]
  /** 按云厂商拆开的日均与摊销，拆分表切到单朵云、单种付费方式时用 */
  by_provider?: Partial<Record<BillProvider, { daily: number | null; amortized_by_period: Record<string, number> }>>
}

/** 月度拆分表的一行：某朵云、某种付费方式下，一条业务线在各账期的金额 */
export interface BillAllocMonthRow {
  provider: BillProvider
  /** postpaid 后付费（按出账月份）/ prepaid 预付费按服务期摊到各月的部分 */
  kind: 'postpaid' | 'prepaid'
  /** 业务线；未命中规则、配置里也没给去处的那部分为 null */
  line: string | null
  by_period: Record<string, number>
}

/** 一天（或一个账期）各条业务线的花费 */
export interface BillAllocPoint {
  t: string
  total: number
  by_line: Record<string, number>
}

export interface BillAllocationResponse {
  from: string
  to: string
  amount: BillAmount
  /** 配了归属规则才有业务线这一层 */
  configured: boolean
  /** 只统计了最近这么多天 */
  window_days: number | null
  /** 本次统计覆盖了几天的账单（各云中最多的那个）；没有日粒度的账单时是 0 */
  days: number
  /** 各云各有几天的账单。日均按每朵云自己的天数折算后相加 */
  days_by_provider: Partial<Record<BillProvider, number>>
  /** points 的粒度 */
  granularity: 'daily' | 'monthly'
  /** 配了 [prepaid] 才有预付费摊销这一层 */
  prepaid: boolean
  /** 区间合计 = 后付费实际出账 + 落在区间内的预付费摊销 */
  total: number
  postpaid: number
  amortized: number
  /** 各账期的预付费摊销额，含区间之后的若干个月 */
  amortized_by_period: Record<string, number>
  /** 日均 = 后付费合计 / days；预付费不参与 */
  daily: number | null
  lines: BillAllocLine[]
  /** 未命中任何规则的部分。配了 unmatched 时这笔钱已同时计入那条业务线 */
  unmatched: BillAllocLine
  /** 配置里 `unmatched` 指向的业务线：非空时未归属的钱已计入它，不能再与各业务线相加 */
  unmatched_into: string | null
  products: BillAllocItem[]
  points: BillAllocPoint[]
  /** 月度拆分表：云 × 付费方式 × 业务线 × 账期，按整月统计，不受 days 影响 */
  monthly: BillAllocMonthRow[]
  /** 日度账单明显少于月度账单时给出两边的合计；覆盖正常时是 null */
  coverage: BillCoverage | null
  stats: Stats
}

/**
 * `/api/bills/product-days`：按产品、按天的后付费金额，「产品费用对比」据此比对两段日期。
 * 日期以今天为终点往回数，不跟所选账期走
 */
export interface BillProductDaysResponse {
  amount: BillAmount
  /** 按服务端时区的今天；它的账单必然未出齐 */
  today: string
  /** 连续的每一天（`YYYY-MM-DD`），升序，没有账单的日子也在 */
  days: string[]
  /** 各云的日度账单出到哪一天 */
  last_by_provider: Partial<Record<BillProvider, string>>
  /** 要看、却只有月度账单的云，不在对比之内 */
  monthly_only: BillProvider[]
  rows: { provider: BillProvider; product: string; /** 与 days 一一对应 */ amounts: number[] }[]
  stats: Stats
}

export interface BillCoverage {
  provider: BillProvider
  /** 所选账期内日度账单的合计 */
  daily: number
  /** 同一段账期月度账单的合计 */
  monthly: number
}

export interface BillDetailResponse {
  provider: BillProvider
  granularity: 'monthly' | 'daily'
  from: string
  to: string
  amount: BillAmount
  rows: BillDetailRow[]
  /** 去重之后一共多少行；计数查询失败时是 null */
  total: number | null
  limit: number
  offset: number
  stats: Stats
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
  /** 有服务的接口数撞上了服务端上限（每个服务 200 个），只返回了量最大的那些 */
  truncated: boolean
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
  /** 这个指标的实际类型；这段时间一个点都没有时不带 */
  metric_type?: MetricType
  /** 查回来是空的之类的情况下，给人看的一句话 */
  note?: string
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
