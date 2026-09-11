/**
 * 服务看板：按 OpenTelemetry 语义约定，把「这个服务上报了哪些指标」翻译成成套的面板。
 *
 * 为什么要这一层：线上 233 个指标名全是 semconv 那套（`http.server.request.duration`、
 * `jvm.memory.used`……），平铺给人看等于让人先背规范。这里把它们按关心的问题分组，
 * 算法（速率还是水位、要不要分位数）也一并定死——这些本来就是由指标类型和语义决定的，
 * 不该让人每次自己选。
 *
 * **一个面板给多个候选（variant）**：线上同时跑着两代 SDK，同一件事有两个名字
 * （`http.server.request.duration` 秒 / `http.server.duration` 毫秒、`jvm.*` / `process.runtime.jvm.*`），
 * 标签名也跟着不一样（`http.response.status_code` / `http.status_code`）。按顺序取第一个
 * 这个服务真的有的，取不到就整块不显示——没有的东西不占地方。
 */
import type { MetricAgg, MetricField, MetricInfo } from '@/api/types'

export interface PanelVariant {
  metric: string
  agg: MetricAgg
  field?: MetricField
  /** 分组标签；不同代 SDK 的 key 不一样，所以跟着 variant 走 */
  by?: string[]
  /** `key=value` 过滤 */
  attr?: string[]
  /** agg=quantile 时要哪几个分位 */
  q?: string
  /** 值是 0~1 的比例，显示成百分比 */
  percent?: boolean
  /**
   * 画成什么。`bars` = 堆叠柱（Cloudflare 控制台的流量图就是这样，服务详情页的
   * 「请求量与错误」也是），计数 / 速率类用它，按状态码堆起来一眼看得出构成；
   * `top` = Top N 表格：按某个标签分组、每行一根占比条，**点一行就把整页按它过滤**——
   * 十几个接口 / topic / 目标地址用表比十二色堆叠柱好读得多，也是 CF 分析页的做法
   * （图在上、Top 表在下、点表过滤）；不给就是折线，水位（gauge）和分位数用。
   */
  kind?: 'bars' | 'line' | 'top'
}

export interface PanelSpec {
  key: string
  title: string
  hint?: string
  variants: PanelVariant[]
}

export interface SectionSpec {
  key: string
  title: string
  hint?: string
  panels: PanelSpec[]
}

/** HTTP 入口：请求量、错误、延迟——RED 那三样。 */
const HTTP_SERVER: SectionSpec = {
  key: 'http_server',
  title: 'HTTP 服务端',
  hint: '这个服务收到的请求',
  panels: [
    {
      key: 'http_server_rate',
      title: '请求量（按状态码）',
      hint: '每秒请求数，按 HTTP 状态码分开；5xx 那条就是在报错',
      variants: [
        { metric: 'http.server.request.duration', agg: 'rate', field: 'count', by: ['http.response.status_code'], kind: 'bars' },
        { metric: 'http.server.duration', agg: 'rate', field: 'count', by: ['http.status_code'], kind: 'bars' },
        { metric: 'http.server.request_count', agg: 'rate', kind: 'bars' },
        // 非 Java 服务没有 http.server.*，用 collector 的 spanmetrics 兜底
        { metric: 'calls', agg: 'rate', by: ['status.code'], attr: ['span.kind=SPAN_KIND_SERVER'], kind: 'bars' },
      ],
    },
    {
      key: 'http_server_latency',
      title: '延迟分位',
      hint: '从直方图的原始桶合并出来的，不是对各实例的分位数取平均',
      variants: [
        { metric: 'http.server.request.duration', agg: 'quantile', q: '0.5,0.95,0.99' },
        { metric: 'http.server.duration', agg: 'quantile', q: '0.5,0.95,0.99' },
        { metric: 'duration', agg: 'quantile', q: '0.5,0.95,0.99', attr: ['span.kind=SPAN_KIND_SERVER'] },
      ],
    },
    {
      key: 'http_route_rate',
      title: 'Top 接口（请求量）',
      hint: '点一行，整页只看这个接口',
      variants: [
        { metric: 'http.server.request.duration', agg: 'rate', field: 'count', by: ['http.route'], kind: 'top' },
        { metric: 'http.server.duration', agg: 'rate', field: 'count', by: ['http.route'], kind: 'top' },
        { metric: 'duration', agg: 'rate', field: 'count', by: ['span.name'], attr: ['span.kind=SPAN_KIND_SERVER'], kind: 'top' },
      ],
    },
    {
      key: 'http_route_p95',
      title: '按接口的 P95',
      hint: '哪个接口在拖后腿',
      variants: [
        { metric: 'http.server.request.duration', agg: 'quantile', q: '0.95', by: ['http.route'] },
        { metric: 'http.server.duration', agg: 'quantile', q: '0.95', by: ['http.route'] },
      ],
    },
    {
      key: 'http_active',
      title: '处理中的请求',
      variants: [{ metric: 'http.server.active_requests', agg: 'last' }],
    },
  ],
}

/** 对外调用：这个服务打给别人的 HTTP 请求。 */
const HTTP_CLIENT: SectionSpec = {
  key: 'http_client',
  title: 'HTTP 客户端',
  hint: '这个服务打出去的请求',
  panels: [
    {
      key: 'http_client_rate',
      title: 'Top 下游（调用量）',
      hint: '点一行，整页只看打给这个地址的调用',
      variants: [
        { metric: 'http.client.request.duration', agg: 'rate', field: 'count', by: ['server.address'], kind: 'top' },
        { metric: 'http.client.duration', agg: 'rate', field: 'count', kind: 'bars' },
        { metric: 'http.client.request_count', agg: 'rate', kind: 'bars' },
      ],
    },
    {
      key: 'http_client_latency',
      title: '延迟分位',
      variants: [
        { metric: 'http.client.request.duration', agg: 'quantile', q: '0.5,0.95,0.99' },
        { metric: 'http.client.duration', agg: 'quantile', q: '0.5,0.95,0.99' },
      ],
    },
  ],
}

/** JVM：内存、GC、线程、CPU。新旧两套命名都认。 */
const JVM: SectionSpec = {
  key: 'jvm',
  title: 'JVM',
  panels: [
    {
      key: 'jvm_memory',
      title: '内存占用（按区）',
      hint: 'heap 一路涨到 limit 附近又掉不下来，就该看 GC 了',
      variants: [
        { metric: 'jvm.memory.used', agg: 'last', by: ['jvm.memory.type'] },
        { metric: 'process.runtime.jvm.memory.usage', agg: 'last', by: ['type'] },
      ],
    },
    {
      key: 'jvm_gc_rate',
      title: 'GC 次数 / 秒',
      variants: [
        { metric: 'jvm.gc.duration', agg: 'rate', field: 'count', by: ['jvm.gc.name'], kind: 'bars' },
        { metric: 'process.runtime.jvm.gc.duration', agg: 'rate', field: 'count', kind: 'bars' },
      ],
    },
    {
      key: 'jvm_gc_p95',
      title: 'GC 停顿 P95',
      variants: [
        { metric: 'jvm.gc.duration', agg: 'quantile', q: '0.95', by: ['jvm.gc.name'] },
        { metric: 'process.runtime.jvm.gc.duration', agg: 'quantile', q: '0.95' },
      ],
    },
    {
      key: 'jvm_threads',
      title: '线程数',
      variants: [
        { metric: 'jvm.thread.count', agg: 'last', by: ['jvm.thread.state'] },
        { metric: 'process.runtime.jvm.threads.count', agg: 'last' },
      ],
    },
    {
      key: 'jvm_cpu',
      title: 'CPU 使用率',
      variants: [
        { metric: 'jvm.cpu.recent_utilization', agg: 'avg', percent: true },
        { metric: 'process.runtime.jvm.cpu.utilization', agg: 'avg', percent: true },
      ],
    },
  ],
}

/** 数据库连接池（HikariCP 这类，OTel 的 db.client.connections.*）。 */
const DB_POOL: SectionSpec = {
  key: 'db_pool',
  title: '数据库连接池',
  panels: [
    {
      key: 'db_conn',
      title: '连接数（按状态）',
      hint: 'used 顶到 max 就是池子不够用了',
      variants: [{ metric: 'db.client.connections.usage', agg: 'last', by: ['state'] }],
    },
    {
      key: 'db_pending',
      title: '等待拿连接的请求',
      variants: [{ metric: 'db.client.connections.pending_requests', agg: 'last' }],
    },
    {
      key: 'db_wait',
      title: '拿连接耗时 P95',
      variants: [{ metric: 'db.client.connections.wait_time', agg: 'quantile', q: '0.95' }],
    },
    {
      key: 'db_use',
      title: '连接占用时长 P95',
      variants: [{ metric: 'db.client.connections.use_time', agg: 'quantile', q: '0.95' }],
    },
  ],
}

/**
 * Kafka 客户端。这些是 Kafka 自己的 JMX 指标透出来的，**本身就已经是速率**
 * （`*_rate`），所以取平均值而不是再算一次 rate。
 */
const KAFKA: SectionSpec = {
  key: 'kafka',
  title: 'Kafka',
  panels: [
    {
      key: 'kafka_lag',
      title: '消费堆积（按 topic）',
      hint: 'records_lag_max：还没消费的消息数，一路涨就是消费跟不上',
      variants: [
        { metric: 'kafka.consumer.records_lag_max', agg: 'max', by: ['topic'] },
        { metric: 'kafka.consumer.records_lag', agg: 'max', by: ['topic'] },
      ],
    },
    {
      key: 'kafka_consume',
      title: 'Top topic（消费速率）',
      hint: '点一行，整页只看这个 topic',
      variants: [{ metric: 'kafka.consumer.records_consumed_rate', agg: 'avg', by: ['topic'], kind: 'top' }],
    },
    {
      key: 'kafka_fetch',
      title: '拉取延迟',
      variants: [{ metric: 'kafka.consumer.fetch_latency_avg', agg: 'avg' }],
    },
    {
      key: 'kafka_send',
      title: '生产速率（按 topic）',
      variants: [{ metric: 'kafka.producer.record_send_rate', agg: 'avg', by: ['topic'] }],
    },
    {
      key: 'kafka_send_error',
      title: '生产失败率（按 topic）',
      variants: [{ metric: 'kafka.producer.record_error_rate', agg: 'avg', by: ['topic'] }],
    },
  ],
}

/** Go 运行时（`process.runtime.go.*`）。 */
const GO: SectionSpec = {
  key: 'go',
  title: 'Go 运行时',
  panels: [
    { key: 'go_goroutines', title: 'goroutine 数', variants: [{ metric: 'process.runtime.go.goroutines', agg: 'last' }] },
    { key: 'go_heap', title: '堆内存', variants: [{ metric: 'process.runtime.go.mem.heap_inuse', agg: 'last' }] },
    {
      key: 'go_gc',
      title: 'GC 暂停 P95',
      variants: [{ metric: 'process.runtime.go.gc.pause_ns', agg: 'quantile', q: '0.95' }],
    },
  ],
}

export const SECTIONS: SectionSpec[] = [HTTP_SERVER, HTTP_CLIENT, JVM, DB_POOL, KAFKA, GO]

export interface ResolvedPanel {
  key: string
  title: string
  hint?: string
  variant: PanelVariant
  info: MetricInfo
}

export interface ResolvedSection {
  key: string
  title: string
  hint?: string
  panels: ResolvedPanel[]
}

/**
 * 按这个服务实际有的指标挑面板。取每个面板的第一个能用的候选；一个都没有就不显示这个面板，
 * 整个 section 都空了就不显示 section。
 */
export function resolveDashboard(metrics: MetricInfo[]): ResolvedSection[] {
  const byName = new Map(metrics.map((m) => [m.name, m]))
  const out: ResolvedSection[] = []
  for (const section of SECTIONS) {
    const panels: ResolvedPanel[] = []
    for (const panel of section.panels) {
      for (const variant of panel.variants) {
        const info = byName.get(variant.metric)
        if (!info) continue
        panels.push({ key: panel.key, title: panel.title, hint: panel.hint, variant, info })
        break
      }
    }
    if (panels.length) out.push({ key: section.key, title: section.title, hint: section.hint, panels })
  }
  return out
}

/** 看板覆盖不到的指标：给一句提示，让人知道去「全部指标」里翻。 */
export function coveredMetricNames(): Set<string> {
  const out = new Set<string>()
  for (const s of SECTIONS) for (const p of s.panels) for (const v of p.variants) out.add(v.metric)
  return out
}

/** 状态码标签的值是不是「错误」：HTTP 看 5xx，spanmetrics 看 STATUS_CODE_ERROR。 */
export function isErrorLabel(value: string): boolean {
  if (/^5\d\d$/.test(value.trim())) return true
  return value.toUpperCase().includes('ERROR')
}
