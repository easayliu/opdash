//! span 表（tracepipe 的 `otel_trace`）的 SQL。
//!
//! 排序键是 `(service_name, span_name, toDateTime(timestamp))`：带 service（最好还有 span_name）的
//! 查询走排序键；只按 trace id 查走 `idx_trace_id` bloom filter，不带时间范围也快；什么都不带、
//! 只按时间查的话只能裁剪到天分区，所以时间范围大了就要求先选服务。
//!
//! 属性列是 ClickHouse 的 `JSON` 类型：key 里的点是路径分隔（`http.route` 存成 `http` → `route`），
//! 整列查出来是嵌套对象，服务端拍平回带点的 key。按 key 过滤**必须**写成子列标识符
//! `` span_attributes.`http.route` ``：这样 ClickHouse 只读那一个子列（线上实测 10 分钟数据读 12 MB、
//! 40 ms）；`getSubcolumn(col, {path:String})` 虽然能把路径当参数绑定，但 MergeTree 上会把整个
//! JSON 列读出来再取值（同一查询 5.9 GB、5 秒）。路径不能绑定参数，就按标识符的规则校验后再拼
//! （见 [`attr_path`]）。值保留 OTLP 的类型（状态码是整数），比较时两边都转成字符串。
//!
//! 集群部署时表是 `Distributed`，分片键 `cityHash64(trace_id)`：链路检索分两次往返——先找候选
//! trace id，再 `trace_id IN {ids}` 聚合——避免嵌套分布式子查询；每个请求带
//! `optimize_skip_unused_shards=1`，按 id 查只打对应分片。
//!
//! 链路详情也分两步：先按 trace id 只读轻列定位 span 在哪（bloom filter 误报块读起来便宜），
//! 再按排序键前缀走主键取 JSON 属性这些重列，见 [`TraceQueries::detail_locate`]。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Bindings, Bucket, TimeRange, quote_ident};
use crate::clickhouse::{Query, num};
use crate::error::{Error, Result};
use crate::schema::{TRACE_FIXED_COLUMNS, Table};

/// 详情第二步 `span_name IN` 列表的字面量上限；参数都走 URL，整条 URL 不能超过 64 KB。
const MAX_NAMES_BYTES: usize = 16 * 1024;

/// 四个 JSON 属性列。查一次贵得多（见 [`TraceQueries::detail_fetch`]），单独拎出来，
/// 只有「点开某个 span」时才读。
const HEAVY_COLUMNS: &[&str] = &[
    "resource_attributes",
    "span_attributes",
    "`events.attributes` AS event_attrs",
    "`links.attributes` AS link_attrs",
];

/// 错误分组的分组键。SQL 的 `GROUP BY` 和 [`ErrorGroupRow::group_id`] **必须用同一份**。
///
/// 少一列的后果不是少分几组，而是**两组共用一个 id**——前端拿 id 记展开状态、React 也拿它当
/// key，于是点开一个、长得像的那几组跟着一起展开。线上出过：`dy-control-server` 的两个 `POST`
/// 报同一句 `SSLException: Read timed out`，只有 `peer` 不同（`log.snssdk.com` 和
/// `webcast.amemv.com`），而当时的 id 没带 `peer`，点一个两个一起开。
///
/// `group_id_covers_every_group_by_column` 这个测试钉住两边对得上。
pub const ERROR_GROUP_KEYS: &[&str] =
    &["service_name", "span_kind", "span_name", "exc_type", "exc_msg", "http_status", "peer"];

/// OTLP 的 span kind，存的是这些字符串。
pub const SPAN_KINDS: &[&str] =
    &["Server", "Client", "Internal", "Producer", "Consumer", "Unspecified"];

/// 一个服务的「入口」：收到请求 / 消费消息的 span。服务概览按这两种算请求量和延迟。
pub const ENTRY_KINDS: &[&str] = &["Server", "Consumer"];
/// 对外调用：HTTP 客户端、数据库、发消息。
pub const CLIENT_KINDS: &[&str] = &["Client", "Producer"];

pub fn normalize_kind(raw: &str) -> Result<String> {
    SPAN_KINDS
        .iter()
        .find(|k| k.eq_ignore_ascii_case(raw.trim()))
        .map(|k| (*k).to_owned())
        .ok_or_else(|| {
            Error::bad_request(format!("kind 只能是 {}，不是 {raw:?}", SPAN_KINDS.join(" / ")))
        })
}

/// 32 位小写 hex；16 位的左补零（logpipe 对 16 位 trace id 的处理一样）。
pub fn normalize_trace_id(raw: &str) -> Result<String> {
    let id = raw.trim().to_ascii_lowercase();
    if id.is_empty() || id.len() > 32 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::bad_request(format!("trace id 应为 32 位 hex，不是 {raw:?}")));
    }
    Ok(format!("{id:0>32}"))
}

/// 16 位小写 hex。和 [`normalize_trace_id`] 同一套校验，长度不同。
pub fn normalize_span_id(raw: &str) -> Result<String> {
    let id = raw.trim().to_ascii_lowercase();
    if id.is_empty() || id.len() > 16 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::bad_request(format!("span id 应为 16 位 hex，不是 {raw:?}")));
    }
    Ok(format!("{id:0>16}"))
}

/// 属性过滤：`key=value` 精确匹配，或只给 key 表示「有这个属性」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrFilter {
    pub key: String,
    pub value: Option<String>,
}

impl AttrFilter {
    /// `http.route=/orders` → key / value；`http.route` → 只有 key。
    pub fn parse(raw: &str) -> Result<Self> {
        let (key, value) = match raw.split_once('=') {
            Some((k, v)) => (k.trim(), Some(v.to_owned())),
            None => (raw.trim(), None),
        };
        if key.is_empty() || key.len() > 256 {
            return Err(Error::bad_request(format!("属性过滤写法应为 key=value，不是 {raw:?}")));
        }
        Ok(Self { key: key.to_owned(), value })
    }

    /// 拼成 `toString(col.`key`) = {p}`。指标那边（`resource_attributes` /
    /// `attributes`）用的是同一套写法，所以是 `pub`。
    pub fn sql(&self, column: &str, b: &mut Bindings) -> Result<String> {
        let path = attr_path(column, &self.key)?;
        Ok(match &self.value {
            Some(v) => format!("toString({path}) = {}", b.bind("String", v)),
            None => format!("toString({path}) != ''"),
        })
    }
}

/// `span_attributes` + `http.route` → `` span_attributes.`http.route` ``。
///
/// 路径是 SQL 标识符，进不了参数绑定，这里是它进 SQL 文本前唯一的一道关：反引号、反斜杠、
/// 控制字符一律拒绝（反引号里其它字符都是字面量），长度也卡住。不存在的路径 ClickHouse 返回
/// 空值，不报错。
pub fn attr_path(column: &str, key: &str) -> Result<String> {
    let key = key.trim();
    if key.is_empty()
        || key.len() > 256
        || key.chars().any(|c| c == '`' || c == '\\' || c.is_control())
    {
        return Err(Error::bad_request(format!("非法的属性名: {key:?}")));
    }
    Ok(format!("{column}.`{key}`"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceSort {
    Time,
    Duration,
}

impl TraceSort {
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.map(str::to_ascii_lowercase).as_deref() {
            None | Some("time") => Ok(TraceSort::Time),
            Some("duration") => Ok(TraceSort::Duration),
            Some(other) => {
                Err(Error::bad_request(format!("sort 只能是 time 或 duration，不是 {other:?}")))
            }
        }
    }
}

/// 没选服务时允许的最大时间跨度。再大只能裁剪到天分区，扫的是全集群所有服务的 span。
pub const UNSCOPED_MAX_RANGE_MS: i64 = 6 * 3_600_000;

/// 「最新在前」的候选查询先从时间窗尾部探这几段，凑够 limit 条就不用扫整个窗口。
///
/// `timestamp` 不是排序键前缀（排序键是 `(service_name, span_name, toDateTime(timestamp))`），
/// `ORDER BY timestamp DESC` 没法像日志页那样倒着读、读够就停——线上 `EXPLAIN` 里是
/// `Sorting (Sorting for ORDER BY)`，时间窗内每行的 `trace_id + timestamp` 都得读出来重排。
/// 但最新的 50 条挤在窗口末尾，先查最后一小段就够：线上实测（8000+ span/s）1 分钟窗 124 万行
/// 就凑满 50 条，整窗 1 小时是 3043 万行。
///
/// 为什么最小一级是 1 分钟而不是 5 秒：时间是第三级排序键，每个 `(service_name, span_name)`
/// 组合都得捞一段 granule，5 秒窗实测也要读 103 万行，再小没有收益。
///
/// 探空的代价是多一次往返（约 150 ms），而探空的查询多半锁了服务、走排序键前缀本来就便宜
/// （最冷的服务整窗 6 小时也只有 3.5 万行）。按耗时排不能用：最慢的那条可能在窗口任何位置。
pub const PROBE_WINDOWS_MS: &[i64] = &[60_000, 15 * 60_000];

/// 候选查询的两种取法。差别在于**去重放在哪**，而这决定了 ClickHouse 能不能用惰性物化。
///
/// 带 `LIMIT 1 BY trace_id` 时执行计划是老老实实的 `Sorting`，`trace_id`（32 个字符）得为窗口里
/// 每一行都物化出来再排；去掉之后计划变成 `Limit (preliminary LIMIT)` + `LazilyReadFromMergeTree`
/// ——只读排序列找出前 n 行的位置，`trace_id` 只为这 n 行读。线上 1 小时高峰窗实测（中位 / 5 次）：
///
/// | | 读量 | CPU | 墙钟 |
/// |---|---|---|---|
/// | `LIMIT 1 BY trace_id LIMIT 50` | 285 MiB | 1310 ms | 927 ms |
/// | `LIMIT 500` + 应用层去重 | **189 MiB** | **723 ms** | **136 ms** |
///
/// 代价是多取的行里可能凑不够 `limit` 个不同的 trace。**按耗时排很划算**：最慢的 span 来自不同
/// 链路，实测四个窗口取 500 行能去重出 83 / 114 / 148 / 179 个。**按时间排则不行**：最新的
/// span 扎堆（同一条忙碌链路一毫秒内能写好几个 span），同样取 500 行只剩 46 个——不够 50。
/// 所以按时间排继续用 `PerTrace`，反正它已经靠 [`PROBE_WINDOWS_MS`] 把窗口缩到 1 分钟了。
#[derive(Debug, Clone, Copy)]
pub enum Candidates {
    /// `LIMIT 1 BY trace_id LIMIT n`：库里去重，正好 n 条不同的 trace。
    PerTrace(u32),
    /// `LIMIT n`：不去重，取够多的行让调用方自己去重（见 [`dedup_by_trace`]）。
    Rows(u32),
}

/// 按耗时排时多取几倍的行来抵消重复。10 倍是实测选的：取 `50 × 10` 行在四个窗口上分别去重出
/// 83 / 114 / 148 / 179 个 trace，都够 50 有余；真不够（一条链路占满了最慢的那几百个 span）
/// 就退回 [`Candidates::PerTrace`]。
pub const CANDIDATE_OVERFETCH: u32 = 10;

/// 按出现顺序去重，最多留 `limit` 条。行本来就是按 `ORDER BY` 排好的，所以留下的第一条
/// 就是这个 trace 排得最靠前的那个 span——和 `LIMIT 1 BY trace_id` 的语义一致。
pub fn dedup_by_trace(rows: Vec<CandidateRow>, limit: u32) -> Vec<CandidateRow> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(limit as usize);
    for row in rows {
        if out.len() as u32 >= limit {
            break;
        }
        if seen.insert(row.trace_id.clone()) {
            out.push(row);
        }
    }
    out
}

/// 摘要查询在候选时间跨度之外前后各放宽多少：一条 trace 的其它 span（尤其异步消费的）
/// 落在匹配到的那个 span 前后。见 [`TraceQueries::summaries`]。
pub const SUMMARY_WIDEN_MS: i64 = 10 * 60_000;

/// 候选落在哪一段时间里。摘要查询按它裁剪，而不是按整个搜索窗——差别可以是三个数量级
/// （线上实测 16 毫秒 vs 1 小时）。空列表返回 `None`（调用方这时也不会去查摘要）。
pub fn candidate_range(rows: &[CandidateRow]) -> Option<TimeRange> {
    let from_ms = rows.iter().map(|r| r.ts_ms).min()?;
    let to_ms = rows.iter().map(|r| r.ts_ms).max()?;
    // 时间谓词是左闭右开，最晚那个 span 自己也得落在里面
    Some(TimeRange { from_ms: from_ms.max(0), to_ms: to_ms.max(0) + 1 })
}

/// 链路详情定位（[`TraceQueries::detail_locate`]）的探测窗口：`(往前, 往后)` 毫秒，从窄到宽，
/// 最后一档 `None` 是不带时间条件、扫全部分区。查中一档就收工，见 [`detail_probe_hit`]。
///
/// 为什么不一上来就用最宽的那档：`trace_id` 只有 bloom filter，读量跟窗口里的**真实数据量**
/// 成正比，跟窗口名义上有多宽无关（所以「反正 +24 小时还没发生、不花钱」只在看今天的 trace
/// 时成立，看昨天的立刻现原形）。线上同一条 167 span 的 trace 实测（span 表一天 5.6 亿行）：
///
/// | 窗口 | 读量 | 冷查询 |
/// |---|---|---|
/// | ±1 分钟 | 21.2 万行 / 8.4 MB | 0.28 s |
/// | ±15 分钟 | 47.3 万行 / 19.8 MB | 0.42 s |
/// | -1 小时 ~ +24 小时（改之前一上来就是这档） | 578.8 万行 / 194.5 MB | 4.36 s |
/// | 不限时间 | 2361.3 万行 / 789.4 MB | 7.25 s |
///
/// 而 trace 的跨度几乎都极短：线上 1 小时窗里 726.9 万条 trace，p50 = 0 ms、p99 = 88 ms、
/// p99.9 = 34.5 s；超过 1 分钟的占 0.134%，超过 15 分钟的占 0.0057%。第一档就命中 99.87%，
/// 剩下那些多跑一两趟（每趟约 250 ms）换的是少读两个数量级。
///
/// 最后那档不限时间的兜底不能省：`at` 可能压根没有（顶栏粘一个 trace id 直达），也可能是**猜**
/// 的（页面拿当前时间范围当中心点）。猜错了前几档全空，靠它兜回来——代价是多读约 27%，
/// 猜中省的是两个数量级。
pub const DETAIL_PROBE_WINDOWS: &[Option<(i64, i64)>] = &[
    Some((60_000, 60_000)),
    Some((15 * 60_000, 15 * 60_000)),
    Some((3_600_000, 24 * 3_600_000)),
    None,
];

/// 这一档窗口够不够：定位到了 span，而且它们没贴着窗口边。
///
/// 贴边说明窗口外面可能还有（异步消费、定时补偿的 span 会掉在很后面），得换宽一档重查。
/// 边距取整个窗口宽度的 1/10：±1 分钟的窗留 12 秒，`-1h ~ +24h` 的窗留 2.5 小时。
/// 一个都没定位到自然也不算命中。
pub fn detail_probe_hit(rows: &[LocatedSpan], window: &TimeRange) -> bool {
    let (Some(min), Some(max)) =
        (rows.iter().map(|s| s.ts_ms).min(), rows.iter().map(|s| s.ts_ms).max())
    else {
        return false;
    };
    let margin = ((window.to_ms - window.from_ms) / 10).max(1);
    min - window.from_ms >= margin && window.to_ms - max >= margin
}

#[derive(Debug, Default, Clone)]
pub struct TraceFilter {
    pub range: Option<TimeRange>,
    pub service: Option<String>,
    pub span_name: Option<String>,
    /// 空 = 不限
    pub kinds: Vec<String>,
    pub error_only: bool,
    pub min_duration_ns: Option<u64>,
    pub max_duration_ns: Option<u64>,
    pub attrs: Vec<AttrFilter>,
    pub resource_attrs: Vec<AttrFilter>,
    /// 动态列（静态 fields，比如 cluster）→ 允许的值
    pub dims: Vec<(String, Vec<String>)>,
}

impl TraceFilter {
    pub fn validate(&self) -> Result<()> {
        let Some(range) = &self.range else {
            return Err(Error::bad_request("链路检索需要时间范围（from / to）"));
        };
        if self.service.is_none() && range.span_ms() > UNSCOPED_MAX_RANGE_MS {
            return Err(Error::bad_request("时间范围超过 6 小时时请先选择一个服务再检索"));
        }
        if let (Some(min), Some(max)) = (self.min_duration_ns, self.max_duration_ns)
            && min > max
        {
            return Err(Error::bad_request("最小耗时不能大于最大耗时"));
        }
        Ok(())
    }

    fn where_sql(&self, b: &mut Bindings) -> Result<String> {
        let mut clauses = Vec::new();
        if let Some(range) = &self.range {
            clauses.push(b.time_predicate("timestamp", range));
        }
        if let Some(service) = &self.service {
            clauses.push(format!("service_name = {}", b.bind("String", service)));
        }
        if let Some(name) = &self.span_name {
            clauses.push(format!("span_name = {}", b.bind("String", name)));
        }
        if !self.kinds.is_empty() {
            clauses.push(format!("span_kind IN {}", b.bind("Array(String)", &self.kinds)));
        }
        if self.error_only {
            clauses.push("status_code = 'Error'".to_owned());
        }
        if let Some(min) = self.min_duration_ns {
            clauses.push(format!("duration_ns >= {}", b.bind("UInt64", min)));
        }
        if let Some(max) = self.max_duration_ns {
            clauses.push(format!("duration_ns <= {}", b.bind("UInt64", max)));
        }
        for attr in &self.attrs {
            clauses.push(attr.sql("span_attributes", b)?);
        }
        for attr in &self.resource_attrs {
            clauses.push(attr.sql("resource_attributes", b)?);
        }
        for (column, values) in &self.dims {
            clauses.push(format!(
                "{} IN {}",
                quote_ident(column)?,
                b.bind("Array(String)", values)
            ));
        }
        Ok(if clauses.is_empty() { "1".to_owned() } else { clauses.join("\n  AND ") })
    }
}

/// 候选 trace 以及它那个「排第一」的匹配 span 的时间戳：按时间排时是最新的那个 span，
/// 按耗时排时是最慢的那个。摘要查询靠它把时间谓词收窄到候选真正所在的那一小段，
/// 见 [`TraceQueries::summaries`]。
#[derive(Debug, Deserialize)]
pub struct CandidateRow {
    pub trace_id: String,
    #[serde(deserialize_with = "num::de")]
    pub ts_ms: i64,
}

/// 详情第一步定位到的一个 span：在哪个排序键区间、哪一毫秒。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct LocatedSpan {
    pub span_id: String,
    pub service_name: String,
    pub span_name: String,
    #[serde(deserialize_with = "num::de")]
    pub ts_ms: i64,
}

/// 聚合出来的一条 trace 的摘要（每个 trace 一行）。
#[derive(Debug, Deserialize)]
pub struct SummaryRow {
    pub trace_id: String,
    #[serde(deserialize_with = "num::de")]
    pub start_us: i64,
    /// 从最早的 span 开始到最晚的 span 结束（含异步消费的那段）
    #[serde(deserialize_with = "num::de")]
    pub span_ns: i64,
    #[serde(deserialize_with = "num::de")]
    pub span_count: u64,
    #[serde(deserialize_with = "num::de")]
    pub error_count: u64,
    pub root_service: String,
    pub root_name: String,
    #[serde(deserialize_with = "num::de")]
    pub root_duration_ns: u64,
    #[serde(deserialize_with = "num::de")]
    pub root_count: u64,
    pub first_service: String,
    pub first_name: String,
    #[serde(deserialize_with = "num::de")]
    pub first_duration_ns: u64,
    pub services: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceSummary {
    pub trace_id: String,
    pub start_us: i64,
    /// 请求耗时 = 根 span 的耗时。根 span 缺失（采样 / 还没入库）时退回最早那个 span 的耗时
    pub duration_ns: u64,
    /// 整条 trace 的时间跨度（最早 span 开始 → 最晚 span 结束），异步消费会让它比 duration_ns 长很多
    pub span_ns: i64,
    pub span_count: u64,
    pub error_count: u64,
    pub root_service: String,
    pub root_name: String,
    /// 没找到 parent_span_id 为空的 span
    pub root_missing: bool,
    pub services: Vec<String>,
}

impl From<SummaryRow> for TraceSummary {
    fn from(r: SummaryRow) -> Self {
        let root_missing = r.root_count == 0;
        Self {
            trace_id: r.trace_id,
            start_us: r.start_us,
            duration_ns: if root_missing { r.first_duration_ns } else { r.root_duration_ns },
            span_ns: r.span_ns.max(0),
            span_count: r.span_count,
            error_count: r.error_count,
            root_service: if root_missing { r.first_service } else { r.root_service },
            root_name: if root_missing { r.first_name } else { r.root_name },
            root_missing,
            services: r.services,
        }
    }
}

/// 链路详情里的一个 span（数据库行的形状）。
#[derive(Debug, Deserialize)]
pub struct SpanRow {
    pub span_id: String,
    pub parent_span_id: String,
    pub service_name: String,
    pub span_name: String,
    pub span_kind: String,
    #[serde(deserialize_with = "num::de")]
    pub start_us: i64,
    #[serde(deserialize_with = "num::de")]
    pub duration_ns: u64,
    pub status_code: String,
    pub status_message: String,
    pub scope_name: String,
    pub scope_version: String,
    pub trace_state: String,
    // 属性 / events / links 的四个 JSON 列只在「取某个 span 的属性」时才查（见
    // [`TraceQueries::detail_fetch`] 的 `heavy`），瀑布图那一趟里它们不在 SELECT 里
    #[serde(default)]
    pub resource_attributes: Value,
    #[serde(default)]
    pub span_attributes: Value,
    #[serde(default, deserialize_with = "num::de_vec")]
    pub event_ts: Vec<i64>,
    #[serde(default)]
    pub event_names: Vec<String>,
    #[serde(default)]
    pub event_attrs: Vec<Value>,
    #[serde(default)]
    pub link_trace_ids: Vec<String>,
    #[serde(default)]
    pub link_span_ids: Vec<String>,
    #[serde(default)]
    pub link_states: Vec<String>,
    #[serde(default)]
    pub link_attrs: Vec<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// 给前端的 span。属性已经拍平成带点的 key。
#[derive(Debug, Clone, Serialize)]
pub struct Span {
    pub span_id: String,
    pub parent_span_id: String,
    pub service: String,
    pub name: String,
    pub kind: String,
    pub start_us: i64,
    pub duration_ns: u64,
    pub status: String,
    pub status_message: String,
    pub scope_name: String,
    pub scope_version: String,
    pub trace_state: String,
    pub attributes: BTreeMap<String, Value>,
    pub resource: BTreeMap<String, Value>,
    pub events: Vec<SpanEvent>,
    pub links: Vec<SpanLink>,
    /// 静态 fields 列（cluster / env……）
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpanEvent {
    pub ts_us: i64,
    pub name: String,
    pub attributes: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpanLink {
    pub trace_id: String,
    pub span_id: String,
    pub trace_state: String,
    pub attributes: BTreeMap<String, Value>,
}

impl From<SpanRow> for Span {
    fn from(r: SpanRow) -> Self {
        let events = r
            .event_ts
            .iter()
            .enumerate()
            .map(|(i, ts)| SpanEvent {
                ts_us: *ts,
                name: r.event_names.get(i).cloned().unwrap_or_default(),
                attributes: r.event_attrs.get(i).map(flatten_json).unwrap_or_default(),
            })
            .collect();
        let links = r
            .link_span_ids
            .iter()
            .enumerate()
            .map(|(i, span_id)| SpanLink {
                trace_id: r.link_trace_ids.get(i).cloned().unwrap_or_default(),
                span_id: span_id.clone(),
                trace_state: r.link_states.get(i).cloned().unwrap_or_default(),
                attributes: r.link_attrs.get(i).map(flatten_json).unwrap_or_default(),
            })
            .collect();
        Self {
            span_id: r.span_id,
            parent_span_id: r.parent_span_id,
            service: r.service_name,
            name: r.span_name,
            kind: r.span_kind,
            start_us: r.start_us,
            duration_ns: r.duration_ns,
            status: r.status_code,
            status_message: r.status_message,
            scope_name: r.scope_name,
            scope_version: r.scope_version,
            trace_state: r.trace_state,
            attributes: flatten_json(&r.span_attributes),
            resource: flatten_json(&r.resource_attributes),
            events,
            links,
            extra: r.extra,
        }
    }
}

/// ClickHouse JSON 列输出的嵌套对象 → 带点的 key：`{"http":{"route":"/x"}}` → `{"http.route":"/x"}`。
/// 数组保持原样（OTLP 的数组属性就是数组）。
pub fn flatten_json(value: &Value) -> BTreeMap<String, Value> {
    fn walk(prefix: &str, value: &Value, out: &mut BTreeMap<String, Value>) {
        match value {
            Value::Object(map) => {
                for (k, v) in map {
                    let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    walk(&key, v, out);
                }
            }
            other => {
                out.insert(prefix.to_owned(), other.clone());
            }
        }
    }
    let mut out = BTreeMap::new();
    if let Value::Object(_) = value {
        walk("", value, &mut out);
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueRow {
    pub value: String,
    #[serde(deserialize_with = "num::de")]
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRow {
    pub key: String,
    #[serde(deserialize_with = "num::de")]
    pub count: u64,
}

/// 属性键 / 值的统计只看最近的这么多个 span：`JSONAllPaths` 要读整个属性列，全范围扫太贵，
/// 而下拉提示要的只是「常见的有哪些」。
pub const ATTR_SAMPLE_ROWS: u32 = 20_000;

pub struct TraceQueries<'a> {
    pub database: &'a str,
    pub table: &'a Table,
}

impl TraceQueries<'_> {
    fn table_ref(&self) -> String {
        format!("`{}`.`{}`", self.database, self.table.name)
    }

    /// 所有 span 表查询都带上：集群上按 trace_id 查时只打对应分片；单机 / `app_log` 上是空操作。
    fn finish(b: Bindings, sql: String) -> Query {
        b.into_query(sql).setting("optimize_skip_unused_shards", 1)
    }

    /// 第一步：满足条件的 trace id，每个 trace 只算一次（`LIMIT 1 BY`），按最新 / 最慢的那个
    /// 匹配 span 排。
    pub fn candidates(
        &self,
        filter: &TraceFilter,
        sort: TraceSort,
        take: Candidates,
    ) -> Result<Query> {
        filter.validate()?;
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        let (dedup_sql, n) = match take {
            Candidates::PerTrace(n) => ("\nLIMIT 1 BY trace_id", n),
            Candidates::Rows(n) => ("", n),
        };
        let limit = b.bind("UInt32", n);
        let order = match sort {
            TraceSort::Time => "timestamp DESC",
            TraceSort::Duration => "duration_ns DESC, timestamp DESC",
        };
        let sql = format!(
            "SELECT trace_id, toUnixTimestamp64Milli(timestamp) AS ts_ms\nFROM {from}\nWHERE {where_sql}\nORDER BY {order}{dedup_sql}\nLIMIT {limit}",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 第二步：这些 trace 各自的摘要。`range` 给了就前后各放宽 [`SUMMARY_WIDEN_MS`] 裁剪分区
    /// （同一条 trace 的其它 span 可能跨出筛选范围），只按 id 查时不限时间，走 bloom filter。
    ///
    /// **`range` 要传候选自己的时间跨度，不是整个搜索窗**（见 [`candidate_range`]）。`trace_id`
    /// 上只有 bloom filter（GRANULARITY 4，2.5% 误报），50 个 id 一起 OR，一个索引块活下来的概率
    /// 是 `1 - 0.975^50 ≈ 72%`——线上 `EXPLAIN indexes=1` 实测 8513/11480，跟期望值对得上。
    /// 也就是说这一步基本挡不住块，**唯一的杠杆是把主键能圈到的时间窗做小**。而「最新的 50 条」
    /// 挤在一起：线上实测这 50 条候选的时间跨度只有 16 毫秒，搜索窗却是 1 小时。锚到候选之后
    /// 主键剩 3408 个 granule（原来 11480），同一条查询 **2603 万行 / 1111 MiB / 1377 ms →
    /// 783 万行 / 362 MiB / 449 ms**。
    ///
    /// 放宽量仍然是 10 分钟：实测再收到 ±1 分钟就会丢 span（一条链路的「总跨度」从 301 秒
    /// 变成 0.13 秒），±10 分钟的聚合结果和现在的实现逐字段一致。
    pub fn summaries(&self, ids: &[String], range: Option<&TimeRange>) -> Result<Query> {
        if ids.is_empty() {
            return Err(Error::bad_request("没有 trace id"));
        }
        let mut b = Bindings::new();
        // `trace_id IN` 放 PREWHERE：自动 PREWHERE 不挑这一条（大数组 + String 列不符合它的启发
        // 式），留在 WHERE 里 ClickHouse 会把聚合要用的八列全读出来再过滤。挪过去之后每行只读
        // `trace_id`，其余列只为命中的行读——线上 1 小时高峰窗实测（中位 / 5 次）
        // **1257 MiB / 2524 ms CPU → 1042 MiB / 1899 ms CPU**；窗口已经收窄过的那条也有
        // 465 → 410 MiB。1042 MiB ÷ 3000 万行 = 34.7 字节，正好一个 `trace_id`，也就是到底了。
        let prewhere = format!("trace_id IN {}", b.bind("Array(String)", ids));
        let where_sql = range
            .map(|r| {
                format!("\nWHERE {}", b.time_predicate("timestamp", &r.widen(SUMMARY_WIDEN_MS)))
            })
            .unwrap_or_default();
        // 写入重试会造成重复行，计数按 span_id 去重
        let sql = format!(
            "SELECT trace_id,\n  toUnixTimestamp64Micro(min(timestamp)) AS start_us,\n  \
             toUnixTimestamp64Nano(max(timestamp + toIntervalNanosecond(duration_ns))) - toUnixTimestamp64Nano(min(timestamp)) AS span_ns,\n  \
             uniqExact(span_id) AS span_count,\n  uniqExactIf(span_id, status_code = 'Error') AS error_count,\n  \
             argMinIf(service_name, timestamp, parent_span_id = '') AS root_service,\n  \
             argMinIf(span_name, timestamp, parent_span_id = '') AS root_name,\n  \
             maxIf(duration_ns, parent_span_id = '') AS root_duration_ns,\n  countIf(parent_span_id = '') AS root_count,\n  \
             argMin(service_name, timestamp) AS first_service,\n  argMin(span_name, timestamp) AS first_name,\n  \
             argMin(duration_ns, timestamp) AS first_duration_ns,\n  arraySort(groupUniqArray(service_name)) AS services\n\
             FROM {from}\nPREWHERE {prewhere}{where_sql}\nGROUP BY trace_id",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 链路详情第一步：找出这条 trace 的 span 都在哪。只读 `span_id` 和排序键那几列，
    /// 重复的（写入重试）只留一份，多取一行用来判断有没有截断。
    ///
    /// 详情为什么分两步：`trace_id` 只有 bloom filter（GRANULARITY 4，默认 2.5% 误报），一天
    /// 一亿多 span 时，过了索引的索引块里绝大多数是误报（线上 EXPLAIN：18371 个 granule 剩 476 个，
    /// 约 119 个索引块，和 4600 × 2.5% 的期望误报数正好对上）。一步到位地把 JSON 属性列、
    /// events / links 都 SELECT 出来，就是在这几百万行误报上读完整 JSON，30 秒超时就是这么来的。
    /// 先只读轻列定位，误报块每行几十字节，亚秒级；再由 [`Self::detail_fetch`] 按排序键前缀
    /// `(service_name, span_name)` 加实际时间跨度走主键取重列，误报块在主键这一层就被挡掉。
    ///
    /// `window` 给了就加时间谓词。窗口宽窄是这一步的**全部**成本（不限时间 789 MB，±1 分钟
    /// 8.4 MB），所以调用方不是随手圈一个大窗，而是按 [`DETAIL_PROBE_WINDOWS`] 从窄往宽探。
    pub fn detail_locate(
        &self,
        trace_id: &str,
        max_spans: u32,
        window: Option<&TimeRange>,
    ) -> Result<Query> {
        let mut b = Bindings::new();
        let id = b.bind("String", trace_id);
        let limit = b.bind("UInt32", max_spans.saturating_add(1));
        let time_sql = match window {
            Some(w) => format!("\n  AND {}", b.time_predicate("timestamp", w)),
            None => String::new(),
        };
        let sql = format!(
            "SELECT span_id, service_name, span_name, toUnixTimestamp64Milli(timestamp) AS ts_ms\nFROM {from}\nWHERE trace_id = {id}{time_sql}\nORDER BY timestamp, span_id\nLIMIT 1 BY span_id\nLIMIT {limit}",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 同一趟定位，但只找指名的那一个 span。
    ///
    /// 用在 URL 上带 `span=` 进来、而它不在 [`Self::detail_locate`] 那批里的时候：截断是按
    /// 时间从早往晚切的，一条几万 span 的 trace 里，出错的那个（分享的链接、错误分组给的样本
    /// 都正指着它）多半落在被切掉的后半段。捞回来才谈得上「打开链接就选中它」。
    ///
    /// 成本和 [`Self::detail_locate`] 同一量级：一样是 `trace_id` 走 bloom filter、只读轻列，
    /// 多一个 `span_id` 等值条件只是少返回几千行——`span_id` 不在排序键上，挡不掉 granule。
    pub fn detail_locate_span(
        &self,
        trace_id: &str,
        span_id: &str,
        window: Option<&TimeRange>,
    ) -> Result<Query> {
        let mut b = Bindings::new();
        let id = b.bind("String", trace_id);
        let span = b.bind("String", span_id);
        let time_sql = match window {
            Some(w) => format!("\n  AND {}", b.time_predicate("timestamp", w)),
            None => String::new(),
        };
        let sql = format!(
            "SELECT span_id, service_name, span_name, toUnixTimestamp64Milli(timestamp) AS ts_ms\nFROM {from}\nWHERE trace_id = {id}\n  AND span_id = {span}{time_sql}\nORDER BY timestamp\nLIMIT 1",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 链路详情第二步：按第一步定位到的 span 取全部列。
    ///
    /// `(service_name, span_name)` 是排序键前缀，加上 `toDateTime(timestamp)` 落在第一步看到的
    /// 毫秒区间里，主键能直接圈到这条 trace 真实所在的 granule；`trace_id` 等值保留给
    /// bloom filter 和分片裁剪。服务名和 span 名按笛卡尔积写成两个 `IN`，比 `Array(Tuple)`
    /// 参数省事，多圈进来的组合数量有限，主键裁剪后多读的 granule 可以忽略。
    ///
    /// **不**把 span id 列表传进去：参数都走 URL，5000 个 id 就是 100 KB，超过 HTTP 库 64 KB
    /// 的 URI 上限（线上一条 5000+ span 的 trace 就是这么报 `builder error for url` 的）。
    /// 不传也不影响一致性：这一步的条件是第一步的子集，`ORDER BY` 和去重方式相同，
    /// 时间区间又卡在第一步看到的最早、最晚那一毫秒之间，取前 `located.len()` 行就是同一批 span。
    ///
    /// `heavy = false` 时四个 JSON 属性列**一列都不取**。它们是这一步的全部成本：线上同一条
    /// 查询实测 **带属性 0.259 GB / 1.8 s，不带 0.002 GB / 50 ms**（另有 22 s、43 s 的样本）。
    /// 原因是主键前缀 `(service_name, span_name, toDateTime(timestamp))` 只能圈到秒级，一条
    /// trace 的几十个 span 会拖进五万行候选，JSON 列又是按 granule 整块读的（每个子列一条流，
    /// 路径一多，读一个 granule 的固定开销就压过了真正要的那几行）。所以瀑布图只取轻列，
    /// 属性等用户点开某个 span 再按 [`Self::detail_span`] 单独取。
    pub fn detail_fetch(
        &self,
        trace_id: &str,
        located: &[LocatedSpan],
        heavy: bool,
    ) -> Result<Query> {
        if located.is_empty() {
            return Err(Error::internal("detail_fetch 需要至少一个 span"));
        }
        let mut b = Bindings::new();
        let id = b.bind("String", trace_id);
        let mut services: Vec<String> = located.iter().map(|s| s.service_name.clone()).collect();
        services.sort();
        services.dedup();
        let mut names: Vec<String> = located.iter().map(|s| s.span_name.clone()).collect();
        names.sort();
        names.dedup();
        let min_ms = located.iter().map(|s| s.ts_ms).min().unwrap_or(0).max(0);
        let max_ms = located.iter().map(|s| s.ts_ms).max().unwrap_or(0).max(0);
        // 存的是纳秒精度，毫秒 X 的 span 落在 [X, X+1) 里，右边界要多放 1 毫秒
        let range = TimeRange { from_ms: min_ms, to_ms: max_ms + 1 };
        let services = b.bind("Array(String)", services);
        // span 名把 SQL / 表名拼进去的服务，一条 trace 能有上千个不同的名字；列表太长就不传，
        // 退化成服务名加时间裁剪，仍然正确，只是主键少切一层
        let names_sql = if names.iter().map(|n| n.len() + 3).sum::<usize>() <= MAX_NAMES_BYTES {
            format!("\n  AND span_name IN {}", b.bind("Array(String)", names))
        } else {
            String::new()
        };
        let time = b.time_predicate("timestamp", &range);
        let limit = b.bind("UInt32", located.len() as u32);
        let cols = self.detail_columns(heavy)?;
        let sql = format!(
            "SELECT {cols}\nFROM {from}\nWHERE trace_id = {id}\n  AND service_name IN {services}{names_sql}\n  AND {time}\nORDER BY timestamp, span_id\nLIMIT 1 BY span_id\nLIMIT {limit}",
            cols = cols.join(", "),
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 详情要取的列。`heavy` 决定带不带四个 JSON 属性列。
    fn detail_columns(&self, heavy: bool) -> Result<Vec<String>> {
        let mut cols: Vec<String> = vec![
            "span_id".into(),
            "parent_span_id".into(),
            "service_name".into(),
            "span_name".into(),
            "span_kind".into(),
            "toUnixTimestamp64Micro(timestamp) AS start_us".into(),
            "duration_ns".into(),
            "status_code".into(),
            "status_message".into(),
            "scope_name".into(),
            "scope_version".into(),
            "trace_state".into(),
            "arrayMap(t -> toUnixTimestamp64Micro(t), `events.timestamp`) AS event_ts".into(),
            "`events.name` AS event_names".into(),
            "`links.trace_id` AS link_trace_ids".into(),
            "`links.span_id` AS link_span_ids".into(),
            "`links.trace_state` AS link_states".into(),
        ];
        if heavy {
            cols.extend(HEAVY_COLUMNS.iter().map(|c| (*c).to_owned()));
        }
        for c in &self.table.columns {
            if !TRACE_FIXED_COLUMNS.contains(&c.name.as_str()) {
                cols.push(quote_ident(&c.name)?);
            }
        }
        Ok(cols)
    }

    /// 某一个 span 的属性 / events / links —— 也就是 [`Self::detail_fetch`] 里省掉的那几列。
    ///
    /// 主键前缀钉到这一个 `(service_name, span_name)`、时间窗只留它自己那一毫秒，读的
    /// granule 从「整条 trace 的五万行候选」缩到一两个：线上实测 0.011 GB / 690 ms。
    pub fn detail_span(&self, trace_id: &str, span: &LocatedSpan) -> Result<Query> {
        let mut b = Bindings::new();
        let id = b.bind("String", trace_id);
        let span_id = b.bind("String", &span.span_id);
        let service = b.bind("String", &span.service_name);
        let name = b.bind("String", &span.span_name);
        // 存的是纳秒精度，毫秒 X 的 span 落在 [X, X+1) 里
        let range = TimeRange { from_ms: span.ts_ms.max(0), to_ms: span.ts_ms.max(0) + 1 };
        let time = b.time_predicate("timestamp", &range);
        // 列取全的（轻列 + 重列）：一行而已，多几列不花钱，SpanRow 和瀑布图那一趟共用
        let sql = format!(
            "SELECT {cols}\nFROM {from}\nWHERE trace_id = {id}\n  AND service_name = {service}\n  AND span_name = {name}\n  AND {time}\n  AND span_id = {span_id}\nLIMIT 1",
            cols = self.detail_columns(true)?.join(", "),
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 时间范围内出现过的服务，按 span 数排。
    pub fn services(&self, range: &TimeRange, limit: u32) -> Result<Query> {
        let mut b = Bindings::new();
        let time = b.time_predicate("timestamp", range);
        let limit = b.bind("UInt32", limit);
        let sql = format!(
            "SELECT service_name AS value, count() AS count\nFROM {from}\nWHERE {time}\nGROUP BY value\nORDER BY count DESC, value\nLIMIT {limit}",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 某个服务的 span_name 列表（走排序键前缀）。`kinds` 限定 span kind。
    pub fn span_names(
        &self,
        range: &TimeRange,
        service: &str,
        kinds: &[&str],
        limit: u32,
    ) -> Result<Query> {
        let mut b = Bindings::new();
        let time = b.time_predicate("timestamp", range);
        let service = b.bind("String", service);
        let kind_sql = if kinds.is_empty() {
            String::new()
        } else {
            format!("\n  AND span_kind IN {}", b.bind("Array(String)", kinds))
        };
        let limit = b.bind("UInt32", limit);
        let sql = format!(
            "SELECT span_name AS value, count() AS count\nFROM {from}\nWHERE {time}\n  AND service_name = {service}{kind_sql}\nGROUP BY value\nORDER BY count DESC, value\nLIMIT {limit}",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 属性键（`column` 是 `span_attributes` 或 `resource_attributes`）。
    pub fn attr_keys(
        &self,
        range: &TimeRange,
        service: Option<&str>,
        column: &str,
        limit: u32,
    ) -> Result<Query> {
        let column = attr_column(column)?;
        let service = require_service(service)?;
        let mut b = Bindings::new();
        let time = b.time_predicate("timestamp", range);
        let service_sql = b.bind("String", service);
        let sample = b.bind("UInt32", ATTR_SAMPLE_ROWS);
        let limit = b.bind("UInt32", limit);
        // LIMIT 必须限在源行上：套在 arrayJoin 外面的话限的是展开之后的行数，读的源行反而更多
        // （实测 10.2 s → 2.6 s）
        let sql = format!(
            "SELECT key, count() AS count\nFROM (\n  SELECT arrayJoin(JSONAllPaths({column})) AS key\n  FROM (\n    SELECT {column}\n    FROM {from}\n    WHERE {time}\n      AND service_name = {service_sql}\n    LIMIT {sample}\n  )\n)\nGROUP BY key\nORDER BY count DESC, key\nLIMIT {limit}",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 某个属性键常见的值。
    pub fn attr_values(
        &self,
        range: &TimeRange,
        service: Option<&str>,
        column: &str,
        key: &str,
        limit: u32,
    ) -> Result<Query> {
        let path = attr_path(attr_column(column)?, key)?;
        let service = require_service(service)?;
        let mut b = Bindings::new();
        let time = b.time_predicate("timestamp", range);
        let service_sql = b.bind("String", service);
        let sample = b.bind("UInt32", ATTR_SAMPLE_ROWS);
        let limit = b.bind("UInt32", limit);
        let sql = format!(
            "SELECT value, count() AS count\nFROM (\n  SELECT toString({path}) AS value\n  FROM {from}\n  WHERE {time}\n    AND service_name = {service_sql}\n  LIMIT {sample}\n)\nWHERE value != ''\nGROUP BY value\nORDER BY count DESC, value\nLIMIT {limit}",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 服务概览 + 卡片上的迷你趋势，**一条查询出两份**。
    ///
    /// 两者的 `WHERE` 一模一样，只差一个 `GROUP BY`，以前是并发发两条、把同一段数据扫两遍。
    /// `GROUPING SETS` 让 ClickHouse 扫一遍同时聚出「整窗每服务一行」和「每服务每桶一行」：
    /// 线上 1 小时窗实测 **两条分开 2025 万行 / 289.0 MB，合并后 1011 万行 / 183.2 MB**。
    /// 总览页当前窗和对比窗各要一份，所以这一改是四条变两条。
    ///
    /// `grouping(bucket)` 分辨这一行是哪一种：**1 是整窗汇总**（`bucket` 没参与分组，值是默认的
    /// 0，不能拿来用），0 是某一个桶。
    ///
    /// `quantilesTDigest` 内存有界、跨分片能合并；默认的 `quantiles` 是 8192 个样本的水塘抽样，
    /// 尾部分位恰恰最不准。单位直接换成毫秒，JSON 里是普通浮点数。分桶那一组用不上分位数，
    /// 但 `GROUPING SETS` 的每一组都会把聚合函数算一遍——多出来的这点 CPU 比多扫一遍表便宜。
    pub fn service_stats(
        &self,
        range: &TimeRange,
        dims: &[(String, Vec<String>)],
        bucket: &Bucket,
    ) -> Result<Query> {
        let mut b = Bindings::new();
        let time = b.time_predicate("timestamp", range);
        let kinds = b.bind("Array(String)", ENTRY_KINDS);
        let mut where_sql = format!("{time}\n  AND span_kind IN {kinds}");
        for (column, values) in dims {
            where_sql.push_str(&format!(
                "\n  AND {} IN {}",
                quote_ident(column)?,
                b.bind("Array(String)", values)
            ));
        }
        let origin = b.bind("Int64", bucket.origin_ms);
        let width = b.bind("Int64", bucket.width_ms);
        // 桶表达式在 GROUP BY 里要原样再写一遍：GROUPING SETS 里引用不了 SELECT 的别名
        let bucket_expr = format!("intDiv(toUnixTimestamp64Milli(timestamp) - {origin}, {width})");
        let sql = format!(
            "SELECT service_name, grouping({bucket_expr}) AS is_total, {bucket_expr} AS bucket,\n  \
             count() AS requests, countIf(status_code = 'Error') AS errors,\n  \
             quantilesTDigest(0.5, 0.95, 0.99)(toFloat64(duration_ns) / 1e6) AS q, max(duration_ns) / 1e6 AS max_ms\n\
             FROM {from}\nWHERE {where_sql}\n\
             GROUP BY GROUPING SETS ((service_name), (service_name, {bucket_expr}))\n\
             ORDER BY is_total DESC, requests DESC, service_name, bucket",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 按 span_name（接口 / 下游调用）的指标。`kinds` 决定看入口还是对外调用。
    ///
    /// 一次可以问好几个服务：总览页上每张异常卡都要写一句「主要是哪个接口」，一张卡各查一次
    /// 的话，一到故障、十几个服务同时报警就是十几条查询——而那正是最需要这一页的时候。
    /// `service_name` 是排序键第一列，`IN` 几个服务照样走排序键前缀，一条顶十条。
    pub fn operations(
        &self,
        range: &TimeRange,
        services: &[&str],
        kinds: &[&str],
    ) -> Result<Query> {
        if services.is_empty() {
            return Err(Error::internal("operations 需要至少一个服务"));
        }
        let mut b = Bindings::new();
        let time = b.time_predicate("timestamp", range);
        let services = b.bind("Array(String)", services);
        let kinds = b.bind("Array(String)", kinds);
        let sql = format!(
            "SELECT service_name, span_name, span_kind, count() AS requests, countIf(status_code = 'Error') AS errors,\n  \
             quantilesTDigest(0.5, 0.95, 0.99)(toFloat64(duration_ns) / 1e6) AS q, max(duration_ns) / 1e6 AS max_ms\n\
             FROM {from}\nWHERE {time}\n  AND service_name IN {services}\n  AND span_kind IN {kinds}\n\
             GROUP BY service_name, span_name, span_kind\nORDER BY requests DESC, service_name, span_name",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 错误分组：把出错的 span 按「同一种报错」归堆，一条查询出整张列表。
    ///
    /// **分组键不能用 `status_message`**：线上一小时 55989 条错误 span 里只有 2 条非空，Java
    /// agent 根本不写它。真正的报错在 `events.attributes` 的 `exception.type` /
    /// `exception.message` 里（19.5% 的错误 span 带），剩下的靠 `http.response.status_code`
    /// 兜底——这两样都取不到时只能给出「哪个接口在错」，具体异常要靠详情层按 `trace_id`
    /// 去日志表拿（被 `GlobalExceptionHandler` 吞掉的异常就是这一类）。
    ///
    /// 读 `Array(JSON)` 的子列走 `arrayMap(x -> x.` 路径 `)`：ClickHouse 只读那一条子列流，
    /// 线上实测一小时 2500 万行读 247 MB / 283 ms。**不能写成 `arrayFirst(x -> …, 列)` 直接对
    /// JSON 数组过滤**，那会退化成 `getSubcolumn` 把整列 JSON 读出来（慢 100 倍，和属性过滤
    /// 同一个坑，见 [`attr_path`]）；所以先 `arrayMap` 成字符串数组，再在字符串数组上 `arrayFirst`。
    ///
    /// `msg_len` 截断异常消息：消息里常带 id、URL、耗时，不截的话同一种错会散成几百组。
    /// 截断之外不做归一化——线上真实数据里截到 160 字符已经够聚（`Connection reset`、
    /// `Read timed out` 这些本来就是定长的），再正则替换数字反而会把「可用容器不足：live-video」
    /// 这种有信息量的消息削平。
    pub fn error_groups(
        &self,
        range: &TimeRange,
        kinds: &[&str],
        service: Option<&str>,
        span_name: Option<&str>,
        msg_len: u32,
        limit: u32,
    ) -> Result<Query> {
        let mut b = Bindings::new();
        let time = b.time_predicate("timestamp", range);
        let kinds_sql = if kinds.is_empty() {
            String::new()
        } else {
            format!("\n  AND span_kind IN {}", b.bind("Array(String)", kinds))
        };
        let service_sql = match service {
            Some(s) => format!("\n  AND service_name = {}", b.bind("String", s)),
            None => String::new(),
        };
        let name_sql = match span_name {
            Some(n) => format!("\n  AND span_name = {}", b.bind("String", n)),
            None => String::new(),
        };
        let msg_len = b.bind("UInt32", msg_len);
        let limit = b.bind("UInt32", limit);
        let sql = format!(
            "SELECT service_name, span_kind, span_name,\n  \
             arrayFirst(t -> t != '', arrayMap(x -> toString(x.`exception.type`), `events.attributes`)) AS exc_type,\n  \
             substring(arrayFirst(t -> t != '', arrayMap(x -> toString(x.`exception.message`), `events.attributes`)), 1, {msg_len}) AS exc_msg,\n  \
             toString(span_attributes.`http.response.status_code`) AS http_status,\n  \
             toString(span_attributes.`server.address`) AS peer,\n  \
             count() AS n, uniqExact(trace_id) AS traces,\n  \
             toUnixTimestamp64Milli(min(timestamp)) AS first_ms, toUnixTimestamp64Milli(max(timestamp)) AS last_ms,\n  \
             argMax(trace_id, timestamp) AS sample_trace, argMax(span_id, timestamp) AS sample_span\n\
             FROM {from}\nWHERE {time}\n  AND status_code = 'Error'{kinds_sql}{service_sql}{name_sql}\n\
             GROUP BY {group_by}\n\
             ORDER BY n DESC, service_name\nLIMIT {limit}",
            from = self.table_ref(),
            group_by = ERROR_GROUP_KEYS.join(", "),
        );
        Ok(Self::finish(b, sql))
    }

    /// 某个服务（可再限定一个 span_name）的入口指标时间序列。
    pub fn timeseries(
        &self,
        range: &TimeRange,
        service: &str,
        span_name: Option<&str>,
        bucket: &Bucket,
    ) -> Result<Query> {
        let mut b = Bindings::new();
        let time = b.time_predicate("timestamp", range);
        let service = b.bind("String", service);
        let name_sql = match span_name {
            Some(n) => format!("\n  AND span_name = {}", b.bind("String", n)),
            None => String::new(),
        };
        let kinds = b.bind("Array(String)", ENTRY_KINDS);
        let origin = b.bind("Int64", bucket.origin_ms);
        let width = b.bind("Int64", bucket.width_ms);
        let sql = format!(
            "SELECT intDiv(toUnixTimestamp64Milli(timestamp) - {origin}, {width}) AS bucket,\n  \
             count() AS requests, countIf(status_code = 'Error') AS errors,\n  \
             quantilesTDigest(0.5, 0.95, 0.99)(toFloat64(duration_ns) / 1e6) AS q\n\
             FROM {from}\nWHERE {time}\n  AND service_name = {service}{name_sql}\n  AND span_kind IN {kinds}\n\
             GROUP BY bucket\nORDER BY bucket",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 耗时 × 时间的热力图：按时间桶和对数耗时档（每个数量级 `bins_per_decade` 档）分组计数。
    /// 格子数有上限（桶数 × 档数），不管底下多少 span，任意时间范围都能一次画出全貌，
    /// 不像检索那样只取前 N 条。
    pub fn heatmap(
        &self,
        filter: &TraceFilter,
        bucket: &Bucket,
        bins_per_decade: u32,
    ) -> Result<Query> {
        filter.validate()?;
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        let origin = b.bind("Int64", bucket.origin_ms);
        let width = b.bind("Int64", bucket.width_ms);
        let bins = b.bind("UInt32", bins_per_decade);
        // 1µs 以下的 span（空跑的 Consumer、duration 为 0 的）全归到最底一档，
        // 不然纵轴要为它们多画三四个数量级；greatest(…, 1) 也顺便挡掉 log10(0)
        let floor = b.bind("Int32", -3 * bins_per_decade as i32);
        let sql = format!(
            "SELECT intDiv(toUnixTimestamp64Milli(timestamp) - {origin}, {width}) AS bucket,\n  \
             greatest(toInt32(floor(log10(greatest(toFloat64(duration_ns), 1) / 1e6) * {bins})), {floor}) AS lvl,\n  \
             count() AS n, countIf(status_code = 'Error') AS errors\n\
             FROM {from}\nWHERE {where_sql}\nGROUP BY bucket, lvl\nORDER BY bucket, lvl",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }
}

/// 属性名 / 属性值为什么必须锁定一个服务：
///
/// 表按 `(service_name, span_name, toDateTime(timestamp))` 排，不带 `service_name` 的 `LIMIT n`
/// 抓到的永远是排序最靠前那个服务的行——线上实测同一条 SQL 连跑四次拿到 33 / 20 / 10 / 10 个 key，
/// 两万行采样全部来自 `ad-onedata` 一个服务。下拉里显示的是别的服务的属性，既不完整也不稳定。
///
/// 想「铺开采样」的几种写法实测都更贵，因为读 `span_attributes` 是按 granule 算的（约 77 MB 一个），
/// 覆盖 K 个 span_name 就得读 K 个 granule：
///
/// * `LIMIT n BY service_name`：59 GB / 24 s（LIMIT BY 不让读取提前停）
/// * 每个服务一个子查询 UNION ALL：6.1 GB / 13 s
/// * 服务内按 span_name UNION ALL 取前 25 个：2.9 GB / 17 s，41 个 key
/// * 服务内 `LIMIT 20 BY span_name`：7.5 GB / 17 s，53 个 key（完整值）
///
/// 所以这里只做力所能及的：锁定服务走主键前缀，采样规模照旧。拿到的是**样本而不是全集**
/// （上面那个服务一小时内有 476 个 span_name，采样能看到 15 个 key，全量是 53 个）——
/// 属性名输入框本来就允许自由输入，下拉只是提示。
fn require_service(service: Option<&str>) -> Result<&str> {
    match service.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Ok(s),
        None => Err(Error::bad_request("查属性名 / 属性值要先选一个服务")),
    }
}

fn attr_column(raw: &str) -> Result<&'static str> {
    match raw {
        "span" | "span_attributes" => Ok("span_attributes"),
        "resource" | "resource_attributes" => Ok("resource_attributes"),
        other => Err(Error::bad_request(format!("scope 只能是 span 或 resource，不是 {other:?}"))),
    }
}

#[derive(Debug, Deserialize)]
pub struct ServiceRow {
    pub service_name: String,
    /// 1 = 整个窗口的汇总，0 = [`ServiceRow::bucket`] 那一格。见 [`TraceQueries::service_stats`]
    #[serde(deserialize_with = "num::de")]
    pub is_total: u8,
    /// `is_total = 1` 的行上没有意义（没参与分组，是默认值）
    #[serde(deserialize_with = "num::de")]
    pub bucket: i64,
    #[serde(deserialize_with = "num::de")]
    pub requests: u64,
    #[serde(deserialize_with = "num::de")]
    pub errors: u64,
    #[serde(deserialize_with = "num::de_vec")]
    pub q: Vec<f64>,
    #[serde(deserialize_with = "num::de")]
    pub max_ms: f64,
}

#[derive(Debug, Deserialize)]
pub struct OperationRow {
    pub service_name: String,
    pub span_name: String,
    pub span_kind: String,
    #[serde(deserialize_with = "num::de")]
    pub requests: u64,
    #[serde(deserialize_with = "num::de")]
    pub errors: u64,
    #[serde(deserialize_with = "num::de_vec")]
    pub q: Vec<f64>,
    #[serde(deserialize_with = "num::de")]
    pub max_ms: f64,
}

/// [`TraceQueries::error_groups`] 的一行 = 一种报错。
#[derive(Debug, Deserialize)]
pub struct ErrorGroupRow {
    pub service_name: String,
    pub span_kind: String,
    pub span_name: String,
    /// 异常类全名；span 上没有 exception 事件时是空串
    pub exc_type: String,
    /// 异常消息，已截断
    pub exc_msg: String,
    /// HTTP 响应码（字符串，OTLP 里是整数），没有就是空串
    pub http_status: String,
    /// `server.address`：Client span 的 `span_name` 只有 `GET` / `POST`，靠它才认得出对端
    pub peer: String,
    #[serde(deserialize_with = "num::de")]
    pub n: u64,
    #[serde(deserialize_with = "num::de")]
    pub traces: u64,
    #[serde(deserialize_with = "num::de")]
    pub first_ms: i64,
    #[serde(deserialize_with = "num::de")]
    pub last_ms: i64,
    pub sample_trace: String,
    pub sample_span: String,
}

impl ErrorGroupRow {
    /// 这一组的身份。前端按它记「哪一组展开着」、拿它做 React key、首页异常卡也靠它直达某一组。
    ///
    /// **取值顺序必须和 [`ERROR_GROUP_KEYS`] 一一对应**：漏掉任何一列，两组就会共用一个 id，
    /// 点开一个另一个跟着开。分隔符用 US（`\u{1f}`），日志里的服务名 / 接口名 / 异常消息都不会
    /// 含有它，不会撞。
    pub fn group_id(&self) -> String {
        [
            self.service_name.as_str(),
            self.span_kind.as_str(),
            self.span_name.as_str(),
            self.exc_type.as_str(),
            self.exc_msg.as_str(),
            self.http_status.as_str(),
            self.peer.as_str(),
        ]
        .join("\u{1f}")
    }
}

#[derive(Debug, Deserialize)]
pub struct TimeseriesRow {
    #[serde(deserialize_with = "num::de")]
    pub bucket: i64,
    #[serde(deserialize_with = "num::de")]
    pub requests: u64,
    #[serde(deserialize_with = "num::de")]
    pub errors: u64,
    #[serde(deserialize_with = "num::de_vec")]
    pub q: Vec<f64>,
}

/// 热力图的一格：时间桶 × 对数耗时档。
#[derive(Debug, Deserialize)]
pub struct HeatmapRow {
    #[serde(deserialize_with = "num::de")]
    pub bucket: i64,
    #[serde(deserialize_with = "num::de")]
    pub lvl: i32,
    #[serde(deserialize_with = "num::de")]
    pub n: u64,
    #[serde(deserialize_with = "num::de")]
    pub errors: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Column, ColumnKind};

    fn table() -> Table {
        let mut cols: Vec<(&str, &str)> =
            TRACE_FIXED_COLUMNS.iter().map(|c| (*c, "String")).collect();
        cols.push(("cluster", "LowCardinality(String)"));
        Table {
            name: "otel_trace".into(),
            columns: cols
                .iter()
                .map(|(n, t)| Column {
                    name: (*n).into(),
                    ty: (*t).into(),
                    kind: ColumnKind::classify(t),
                })
                .collect(),
        }
    }

    fn range() -> TimeRange {
        TimeRange { from_ms: 1_000_000, to_ms: 2_000_000 }
    }
    /// 分组 id 必须覆盖 SQL 里 `GROUP BY` 的每一列。
    ///
    /// 漏一列就会有两组共用一个 id，前端点开一个、另一个跟着开——线上就是漏了 `peer` 才发现的。
    /// 这里逐列改一遍：任何一列变了，id 就必须跟着变。
    #[test]
    fn group_id_covers_every_group_by_column() {
        let base = ErrorGroupRow {
            service_name: "svc".into(),
            span_kind: "Client".into(),
            span_name: "POST".into(),
            exc_type: "javax.net.ssl.SSLException".into(),
            exc_msg: "Read timed out".into(),
            http_status: "500".into(),
            peer: "log.snssdk.com".into(),
            n: 1,
            traces: 1,
            first_ms: 0,
            last_ms: 0,
            sample_trace: "t".into(),
            sample_span: "s".into(),
        };
        let id = base.group_id();
        assert_eq!(
            id.split('\u{1f}').count(),
            ERROR_GROUP_KEYS.len(),
            "group_id 的列数和 GROUP BY 对不上：{id}"
        );

        // 逐列改一遍：只有分组键变了 id 才该变
        let variants = [
            ("service_name", ErrorGroupRow { service_name: "other".into(), ..clone_row(&base) }),
            ("span_kind", ErrorGroupRow { span_kind: "Producer".into(), ..clone_row(&base) }),
            ("span_name", ErrorGroupRow { span_name: "GET".into(), ..clone_row(&base) }),
            (
                "exc_type",
                ErrorGroupRow { exc_type: "java.io.IOException".into(), ..clone_row(&base) },
            ),
            ("exc_msg", ErrorGroupRow { exc_msg: "Broken pipe".into(), ..clone_row(&base) }),
            ("http_status", ErrorGroupRow { http_status: "404".into(), ..clone_row(&base) }),
            // 这一条就是线上那个 bug：只有对端不同
            ("peer", ErrorGroupRow { peer: "webcast.amemv.com".into(), ..clone_row(&base) }),
        ];
        assert_eq!(variants.len(), ERROR_GROUP_KEYS.len());
        for (name, row) in &variants {
            assert_ne!(row.group_id(), id, "只改了 {name}，id 却没变");
        }

        // 反过来：聚合值变了不该换身份，不然翻一次页展开状态就丢了
        let mut same = clone_row(&base);
        same.n = 999;
        same.sample_trace = "another".into();
        assert_eq!(same.group_id(), id);
    }

    fn clone_row(r: &ErrorGroupRow) -> ErrorGroupRow {
        ErrorGroupRow {
            service_name: r.service_name.clone(),
            span_kind: r.span_kind.clone(),
            span_name: r.span_name.clone(),
            exc_type: r.exc_type.clone(),
            exc_msg: r.exc_msg.clone(),
            http_status: r.http_status.clone(),
            peer: r.peer.clone(),
            n: r.n,
            traces: r.traces,
            first_ms: r.first_ms,
            last_ms: r.last_ms,
            sample_trace: r.sample_trace.clone(),
            sample_span: r.sample_span.clone(),
        }
    }

    #[test]
    fn normalizes_ids_and_kinds() {
        assert_eq!(
            normalize_trace_id("ABCDEF0123456789").unwrap(),
            "0000000000000000abcdef0123456789"
        );
        assert_eq!(
            normalize_trace_id("e89a476882236ce0f1186d1522c8f59f").unwrap(),
            "e89a476882236ce0f1186d1522c8f59f"
        );
        assert!(normalize_trace_id("xyz").is_err());
        assert!(normalize_trace_id("").is_err());
        assert_eq!(normalize_kind("server").unwrap(), "Server");
        assert!(normalize_kind("bogus").is_err());
    }

    #[test]
    fn attr_filter_parsing() {
        assert_eq!(
            AttrFilter::parse("http.route=/orders/{id}").unwrap(),
            AttrFilter { key: "http.route".into(), value: Some("/orders/{id}".into()) }
        );
        assert_eq!(AttrFilter::parse("a=b=c").unwrap().value.as_deref(), Some("b=c"));
        assert_eq!(AttrFilter::parse("exception.type").unwrap().value, None);
        assert!(AttrFilter::parse("=x").is_err());
    }

    #[test]
    fn attr_sampling_is_scoped_to_one_service() {
        let table = table();
        let q = TraceQueries { database: "logs", table: &table };

        // 不给服务：直接 400，不再返回「排序最靠前那个服务」的片面结果
        assert!(q.attr_keys(&range(), None, "span", 200).is_err());
        assert!(q.attr_keys(&range(), Some("  "), "span", 200).is_err());
        assert!(q.attr_values(&range(), None, "span", "http.route", 50).is_err());

        let keys = q.attr_keys(&range(), Some("checkout"), "span", 200).unwrap();
        let sql = keys.sql();
        // LIMIT 限在源行上（在 arrayJoin 里层），不是套在展开之后
        assert!(
            sql.contains("SELECT arrayJoin(JSONAllPaths(span_attributes)) AS key\n  FROM (\n    SELECT span_attributes"),
            "{sql}"
        );
        assert!(sql.contains("AND service_name = {p2:String}\n    LIMIT {p3:UInt32}"), "{sql}");
        assert_eq!(keys.params()[2].1, "checkout");
        assert_eq!(keys.params()[3].1, ATTR_SAMPLE_ROWS.to_string());

        let values = q.attr_values(&range(), Some("checkout"), "span", "http.route", 50).unwrap();
        let sql = values.sql();
        assert!(sql.contains("toString(span_attributes.`http.route`)"), "{sql}");
        assert!(sql.contains("AND service_name = {p2:String}"), "{sql}");
        assert!(sql.contains("LIMIT {p3:UInt32}"), "{sql}");
    }

    #[test]
    fn heatmap_sql() {
        let table = table();
        let q = TraceQueries { database: "logs", table: &table };
        let filter = TraceFilter {
            range: Some(range()),
            kinds: vec!["Server".into(), "Consumer".into()],
            error_only: true,
            ..Default::default()
        };
        let hm = q.heatmap(&filter, &Bucket { width_ms: 30_000, origin_ms: 0 }, 4).unwrap();
        let sql = hm.sql();
        assert!(sql.contains("span_kind IN {p2:Array(String)}"), "{sql}");
        assert!(sql.contains("status_code = 'Error'"), "{sql}");
        assert!(
            sql.contains("log10(greatest(toFloat64(duration_ns), 1) / 1e6) * {p5:UInt32}"),
            "{sql}"
        );
        assert!(sql.contains("GROUP BY bucket, lvl"), "{sql}");
        assert_eq!(hm.params()[3].1, "0");
        assert_eq!(hm.params()[4].1, "30000");
        assert_eq!(hm.params()[5].1, "4");
        assert!(sql.contains("{p6:Int32}) AS lvl"), "{sql}");
        assert_eq!(hm.params()[6].1, "-12");
        assert!(
            q.heatmap(&TraceFilter::default(), &Bucket { width_ms: 1, origin_ms: 0 }, 4).is_err()
        );
    }

    #[test]
    fn candidates_and_summaries_sql() {
        let table = table();
        let q = TraceQueries { database: "logs", table: &table };
        let filter = TraceFilter {
            range: Some(range()),
            service: Some("order-service".into()),
            span_name: Some("POST /orders".into()),
            kinds: vec!["Server".into()],
            error_only: true,
            min_duration_ns: Some(500_000_000),
            attrs: vec![
                AttrFilter::parse("http.response.status_code=500").unwrap(),
                AttrFilter::parse("acme.user_id").unwrap(),
            ],
            dims: vec![("cluster".into(), vec!["bj-prod".into()])],
            ..Default::default()
        };
        let query = q.candidates(&filter, TraceSort::Duration, Candidates::PerTrace(50)).unwrap();
        let sql = query.sql();
        // 候选顺带把时间戳带回来，摘要查询靠它收窄时间谓词
        assert!(
            sql.starts_with("SELECT trace_id, toUnixTimestamp64Milli(timestamp) AS ts_ms"),
            "{sql}"
        );
        assert!(sql.contains("service_name = {p2:String}"), "{sql}");
        assert!(sql.contains("span_name = {p3:String}"), "{sql}");
        assert!(sql.contains("span_kind IN {p4:Array(String)}"), "{sql}");
        assert!(sql.contains("status_code = 'Error'"), "{sql}");
        assert!(sql.contains("duration_ns >= {p5:UInt64}"), "{sql}");
        assert!(
            sql.contains("toString(span_attributes.`http.response.status_code`) = {p6:String}"),
            "{sql}"
        );
        assert!(sql.contains("toString(span_attributes.`acme.user_id`) != ''"), "{sql}");
        assert!(sql.contains("`cluster` IN {p7:Array(String)}"), "{sql}");
        assert!(
            sql.ends_with(
                "ORDER BY duration_ns DESC, timestamp DESC\nLIMIT 1 BY trace_id\nLIMIT {p8:UInt32}"
            ),
            "{sql}"
        );
        assert_eq!(query.settings(), &[("optimize_skip_unused_shards", "1".to_owned())]);
        assert_eq!(query.params()[6].1, "500");
        // 属性名带反引号 / 反斜杠的直接拒绝
        let bad = TraceFilter {
            range: Some(range()),
            attrs: vec![AttrFilter { key: "a`b".into(), value: None }],
            ..Default::default()
        };
        assert!(q.candidates(&bad, TraceSort::Time, Candidates::PerTrace(10)).is_err());
        assert!(attr_path("span_attributes", "a\\b").is_err());
        assert_eq!(
            attr_path("span_attributes", " http.route ").unwrap(),
            "span_attributes.`http.route`"
        );

        let s = q.summaries(&["a".into(), "b".into()], Some(&range())).unwrap();
        // trace_id 走 PREWHERE，时间谓词留在 WHERE
        assert!(s.sql().contains("PREWHERE trace_id IN {p0:Array(String)}"), "{}", s.sql());
        assert!(s.sql().contains("\nWHERE timestamp >="), "{}", s.sql());
        assert!(s.sql().contains("uniqExact(span_id) AS span_count"));
        assert_eq!(s.params()[0].1, "['a','b']");
        // 放宽了 10 分钟
        assert_eq!(s.params()[1].1, (1_000_000 - 600_000).to_string());
        assert_eq!(s.params()[2].1, (2_000_000 + 600_000).to_string());
        assert!(q.summaries(&[], None).is_err());
    }

    #[test]
    fn dedup_keeps_the_first_row_of_each_trace() {
        let row = |id: &str, ts_ms| CandidateRow { trace_id: id.into(), ts_ms };
        // 行已经按 ORDER BY 排好，同一条 trace 留排最前的那个 span
        let got = dedup_by_trace(vec![row("a", 3), row("a", 1), row("b", 2), row("c", 9)], 2);
        assert_eq!(
            got.iter().map(|r| (r.trace_id.as_str(), r.ts_ms)).collect::<Vec<_>>(),
            vec![("a", 3), ("b", 2)]
        );
        assert!(dedup_by_trace(Vec::new(), 5).is_empty());
    }

    #[test]
    fn probe_window_is_a_hit_only_when_spans_sit_clear_of_both_edges() {
        let span = |ts_ms| LocatedSpan {
            span_id: "a".into(),
            service_name: "s".into(),
            span_name: "n".into(),
            ts_ms,
        };
        // ±1 分钟的窗（宽 120 秒）边距是 12 秒
        let w = TimeRange { from_ms: 1_000_000, to_ms: 1_120_000 };
        assert!(detail_probe_hit(&[span(1_040_000), span(1_060_000)], &w));
        // 最晚的贴着右边——异步 span 可能还在窗外，得换宽一档
        assert!(!detail_probe_hit(&[span(1_040_000), span(1_119_000)], &w));
        // 最早的贴着左边，同理（at 未必是 trace 的开头，从日志点进来就可能落在中段）
        assert!(!detail_probe_hit(&[span(1_001_000)], &w));
        // 一个都没定位到，也不算命中
        assert!(!detail_probe_hit(&[], &w));
    }

    #[test]
    fn probe_windows_widen_and_end_with_an_unbounded_one() {
        let widths: Vec<i64> = DETAIL_PROBE_WINDOWS
            .iter()
            .filter_map(|w| w.map(|(before, after)| before + after))
            .collect();
        assert!(widths.windows(2).all(|p| p[0] < p[1]), "{widths:?} 必须一档比一档宽");
        // 最后一档不限时间：`at` 可能没有、也可能是猜的，全靠它兜底
        assert_eq!(DETAIL_PROBE_WINDOWS.last(), Some(&None));
    }

    #[test]
    fn candidates_can_skip_limit_by_so_clickhouse_reads_lazily() {
        let table = table();
        let q = TraceQueries { database: "logs", table: &table };
        let filter = TraceFilter { range: Some(range()), ..Default::default() };
        let per = q.candidates(&filter, TraceSort::Duration, Candidates::PerTrace(50)).unwrap();
        assert!(per.sql().ends_with("LIMIT 1 BY trace_id\nLIMIT {p2:UInt32}"), "{}", per.sql());
        assert_eq!(per.params()[2].1, "50");
        let rows = q.candidates(&filter, TraceSort::Duration, Candidates::Rows(500)).unwrap();
        assert!(!rows.sql().contains("LIMIT 1 BY"), "{}", rows.sql());
        assert!(
            rows.sql().ends_with("ORDER BY duration_ns DESC, timestamp DESC\nLIMIT {p2:UInt32}"),
            "{}",
            rows.sql()
        );
        assert_eq!(rows.params()[2].1, "500");
    }

    #[test]
    fn candidate_range_covers_the_newest_candidate() {
        let row = |ts_ms| CandidateRow { trace_id: "a".into(), ts_ms };
        assert!(candidate_range(&[]).is_none());
        let r = candidate_range(&[row(2_000), row(1_000), row(1_500)]).unwrap();
        assert_eq!(r.from_ms, 1_000);
        // 时间谓词左闭右开，最晚那个 span 自己也得落在范围里
        assert_eq!(r.to_ms, 2_001);
        // 负时间戳不该把范围拖到 0 以前（widen 之后还要当参数发出去）
        assert_eq!(candidate_range(&[row(-5)]).unwrap().from_ms, 0);
    }

    #[test]
    fn unscoped_search_is_limited_to_six_hours() {
        let table = table();
        let q = TraceQueries { database: "logs", table: &table };
        let wide = TimeRange { from_ms: 0, to_ms: 7 * 3_600_000 };
        let filter = TraceFilter { range: Some(wide), ..Default::default() };
        assert!(q.candidates(&filter, TraceSort::Time, Candidates::PerTrace(10)).is_err());
        let scoped =
            TraceFilter { range: Some(wide), service: Some("x".into()), ..Default::default() };
        assert!(q.candidates(&scoped, TraceSort::Time, Candidates::PerTrace(10)).is_ok());
        assert!(
            q.candidates(&TraceFilter::default(), TraceSort::Time, Candidates::PerTrace(10))
                .is_err()
        );
    }

    #[test]
    fn detail_locate_reads_light_columns_and_over_fetches() {
        let table = table();
        let q = TraceQueries { database: "logs", table: &table };
        let d = q.detail_locate("abc", 5000, None).unwrap();
        assert!(
            d.sql().starts_with(
                "SELECT span_id, service_name, span_name, toUnixTimestamp64Milli(timestamp) AS ts_ms\nFROM"
            ),
            "{}",
            d.sql()
        );
        assert!(d.sql().contains("WHERE trace_id = {p0:String}\nORDER BY"), "{}", d.sql());
        assert!(
            d.sql().ends_with("ORDER BY timestamp, span_id\nLIMIT 1 BY span_id\nLIMIT {p1:UInt32}")
        );
        assert_eq!(d.params()[1].1, "5001");
        // 重列一个都不碰
        for heavy in ["span_attributes", "resource_attributes", "events.", "links."] {
            assert!(!d.sql().contains(heavy), "{heavy} 不该出现在定位查询里: {}", d.sql());
        }
        assert_eq!(d.settings(), &[("optimize_skip_unused_shards", "1".to_owned())]);
        let w = q.detail_locate("abc", 5000, Some(&range())).unwrap();
        assert!(
            w.sql().contains(
                "WHERE trace_id = {p0:String}\n  AND timestamp >= fromUnixTimestamp64Milli({p2:Int64}) AND timestamp < fromUnixTimestamp64Milli({p3:Int64})\nORDER BY"
            ),
            "{}",
            w.sql()
        );
        assert_eq!(w.params()[2].1, "1000000");
    }

    #[test]
    fn detail_locate_span_asks_for_one_row() {
        let table = table();
        let q = TraceQueries { database: "logs", table: &table };
        let d = q.detail_locate_span("abc", "0881", Some(&range())).unwrap();
        assert!(
            d.sql().contains(
                "WHERE trace_id = {p0:String}\n  AND span_id = {p1:String}\n  AND timestamp >= "
            ),
            "{}",
            d.sql()
        );
        assert!(d.sql().ends_with("ORDER BY timestamp\nLIMIT 1"), "{}", d.sql());
        assert_eq!(d.params()[1].1, "0881");
        // 和定位整条 trace 一样只读轻列
        for heavy in ["span_attributes", "resource_attributes", "events.", "links."] {
            assert!(!d.sql().contains(heavy), "{heavy} 不该出现在定位查询里: {}", d.sql());
        }
        let n = q.detail_locate_span("abc", "0881", None).unwrap();
        assert!(!n.sql().contains("timestamp >="), "不带窗口就没有时间谓词: {}", n.sql());
    }

    #[test]
    fn detail_fetch_pins_sort_key_prefix_and_time_span() {
        let table = table();
        let q = TraceQueries { database: "logs", table: &table };
        let located = vec![
            LocatedSpan {
                span_id: "s2".into(),
                service_name: "gateway".into(),
                span_name: "GET /x".into(),
                ts_ms: 1_700,
            },
            LocatedSpan {
                span_id: "s1".into(),
                service_name: "order".into(),
                span_name: "db.query".into(),
                ts_ms: 1_500,
            },
            LocatedSpan {
                span_id: "s3".into(),
                service_name: "order".into(),
                span_name: "GET /x".into(),
                ts_ms: 1_600,
            },
        ];
        let f = q.detail_fetch("abc", &located, false).unwrap();
        assert!(
            f.sql().contains(
                "WHERE trace_id = {p0:String}\n  AND service_name IN {p1:Array(String)}\n  AND span_name IN {p2:Array(String)}\n  AND timestamp >= fromUnixTimestamp64Milli({p3:Int64}) AND timestamp < fromUnixTimestamp64Milli({p4:Int64})\nORDER BY timestamp, span_id\nLIMIT 1 BY span_id\nLIMIT {p5:UInt32}"
            ),
            "{}",
            f.sql()
        );
        let params = f.params();
        assert_eq!(params[0].1, "abc");
        // 去重且有序，主键裁剪用
        assert_eq!(params[1].1, "['gateway','order']");
        assert_eq!(params[2].1, "['GET /x','db.query']");
        // 毫秒区间：最早的那一毫秒到最晚的那一毫秒加 1（存的是纳秒）
        assert_eq!(params[3].1, "1500");
        assert_eq!(params[4].1, "1701");
        assert_eq!(params[5].1, "3");
        // span id 不进参数：5000 个 id 会把 URL 撑过 64 KB
        assert!(!f.sql().contains("span_id IN"), "{}", f.sql());
        assert_eq!(params.len(), 6);
        // 瀑布图这一趟一列 JSON 属性都不读：它们是详情查询的全部成本
        for heavy in
            ["resource_attributes", "span_attributes", "events.attributes", "links.attributes"]
        {
            assert!(!f.sql().contains(heavy), "{heavy} 不该出现在瀑布图查询里: {}", f.sql());
        }
        assert!(f.sql().contains("`events.name` AS event_names"), "{}", f.sql());
        assert!(f.sql().contains(", `cluster`\n"), "{}", f.sql());
        // 点开某个 span 才取属性，主键前缀钉死到这一个 span
        let one = q.detail_span("abc", &located[0]).unwrap();
        assert!(one.sql().contains("resource_attributes, span_attributes"), "{}", one.sql());
        assert!(
            one.sql().contains("AND service_name = {p2:String}\n  AND span_name = {p3:String}"),
            "{}",
            one.sql()
        );
        assert!(one.sql().contains("AND span_id = {p1:String}\nLIMIT 1"), "{}", one.sql());
        // 时间窗只留这一毫秒
        assert_eq!(one.params()[4].1, "1700");
        assert_eq!(one.params()[5].1, "1701");
        assert_eq!(f.settings(), &[("optimize_skip_unused_shards", "1".to_owned())]);
        assert!(q.detail_fetch("abc", &[], false).is_err());
    }

    /// 5000 个 span 的 trace：参数总量要留在 HTTP 库 64 KB 的 URI 上限之内。
    #[test]
    fn detail_fetch_params_stay_small_for_huge_traces() {
        let table = table();
        let q = TraceQueries { database: "logs", table: &table };
        let located: Vec<LocatedSpan> = (0..5000)
            .map(|i| LocatedSpan {
                span_id: format!("{i:016x}"),
                service_name: format!("svc-{}", i % 3),
                span_name: format!("op-{}", i % 15),
                ts_ms: 1_700_000 + i,
            })
            .collect();
        let f = q.detail_fetch("abc", &located, false).unwrap();
        let bytes: usize = f.params().iter().map(|(k, v)| k.len() + v.len()).sum();
        assert!(bytes < 4 * 1024, "{bytes} bytes of params");
        assert_eq!(f.params()[5].1, "5000");

        // span 名各不相同且很长：列表不进参数，只剩服务名加时间
        let noisy: Vec<LocatedSpan> = (0..2000)
            .map(|i| LocatedSpan {
                span_id: format!("{i:016x}"),
                service_name: "svc".into(),
                span_name: format!("INSERT rawdata.table_{i}_with_a_rather_long_suffix"),
                ts_ms: 1_700_000 + i,
            })
            .collect();
        let f = q.detail_fetch("abc", &noisy, false).unwrap();
        assert!(!f.sql().contains("span_name IN"), "{}", f.sql());
        assert!(
            f.sql().contains("service_name IN {p1:Array(String)}\n  AND timestamp >="),
            "{}",
            f.sql()
        );
        let bytes: usize = f.params().iter().map(|(k, v)| k.len() + v.len()).sum();
        assert!(bytes < 4 * 1024, "{bytes} bytes of params");
    }

    #[test]
    fn flattens_json_attributes() {
        let v: Value = serde_json::from_str(
            r#"{"http":{"response":{"status_code":500},"route":"/x"},"tags":["a"],"flag":true}"#,
        )
        .unwrap();
        let flat = flatten_json(&v);
        assert_eq!(flat["http.response.status_code"], 500);
        assert_eq!(flat["http.route"], "/x");
        assert_eq!(flat["tags"], serde_json::json!(["a"]));
        assert_eq!(flat["flag"], true);
        assert!(flatten_json(&Value::Null).is_empty());
    }

    #[test]
    fn span_row_converts_events_and_links() {
        let json = r#"{"span_id":"s","parent_span_id":"","service_name":"svc","span_name":"n","span_kind":"Server","start_us":"10","duration_ns":5,"status_code":"Error","status_message":"m","scope_name":"","scope_version":"","trace_state":"","resource_attributes":{"service":{"name":"svc"}},"span_attributes":{"http":{"route":"/x"}},"event_ts":[11],"event_names":["exception"],"event_attrs":[{"exception":{"type":"Boom"}}],"link_trace_ids":["t2"],"link_span_ids":["s2"],"link_states":[""],"link_attrs":[{}],"cluster":"bj"}"#;
        let row: SpanRow = serde_json::from_str(json).unwrap();
        let span = Span::from(row);
        assert_eq!(span.start_us, 10);
        assert_eq!(span.attributes["http.route"], "/x");
        assert_eq!(span.resource["service.name"], "svc");
        assert_eq!(span.events.len(), 1);
        assert_eq!(span.events[0].attributes["exception.type"], "Boom");
        assert_eq!(span.links[0].trace_id, "t2");
        assert_eq!(span.extra["cluster"], "bj");
    }

    #[test]
    fn summary_falls_back_when_root_missing() {
        let row: SummaryRow = serde_json::from_str(r#"{"trace_id":"t","start_us":1,"span_ns":100,"span_count":3,"error_count":1,"root_service":"","root_name":"","root_duration_ns":0,"root_count":0,"first_service":"order-service","first_name":"POST /orders","first_duration_ns":42,"services":["order-service"]}"#).unwrap();
        let s = TraceSummary::from(row);
        assert!(s.root_missing);
        assert_eq!(s.duration_ns, 42);
        assert_eq!(s.root_service, "order-service");
    }

    #[test]
    fn service_queries_use_tdigest_and_entry_kinds() {
        let table = table();
        let q = TraceQueries { database: "logs", table: &table };
        let o = q.service_stats(&range(), &[], &Bucket { width_ms: 60_000, origin_ms: 0 }).unwrap();
        assert!(
            o.sql()
                .contains("quantilesTDigest(0.5, 0.95, 0.99)(toFloat64(duration_ns) / 1e6) AS q"),
            "{}",
            o.sql()
        );
        assert!(o.sql().contains("span_kind IN {p2:Array(String)}"));
        assert_eq!(o.params()[2].1, "['Server','Consumer']");
        // 汇总和分桶一条查询出两份：桶表达式在 SELECT、grouping() 和 GROUP BY 里都要原样出现
        let bucket_expr = "intDiv(toUnixTimestamp64Milli(timestamp) - {p3:Int64}, {p4:Int64})";
        assert_eq!(o.sql().matches(bucket_expr).count(), 3, "{}", o.sql());
        assert!(
            o.sql().contains(&format!(
                "GROUP BY GROUPING SETS ((service_name), (service_name, {bucket_expr}))"
            )),
            "{}",
            o.sql()
        );
        let ops = q.operations(&range(), &["svc", "svc2"], CLIENT_KINDS).unwrap();
        assert_eq!(ops.params()[2].1, "['svc','svc2']");
        assert_eq!(ops.params()[3].1, "['Client','Producer']");
        assert!(ops.sql().contains("GROUP BY service_name, span_name, span_kind"), "{}", ops.sql());
        assert!(q.operations(&range(), &[], CLIENT_KINDS).is_err());
        let ts = q
            .timeseries(
                &range(),
                "svc",
                Some("POST /x"),
                &Bucket { width_ms: 60_000, origin_ms: 0 },
            )
            .unwrap();
        assert!(ts.sql().contains("span_name = {p3:String}"), "{}", ts.sql());
        assert!(ts.sql().contains("GROUP BY bucket"));
        let keys = q.attr_keys(&range(), Some("svc"), "span", 100).unwrap();
        assert!(
            keys.sql().contains("arrayJoin(JSONAllPaths(span_attributes)) AS key"),
            "{}",
            keys.sql()
        );
        assert!(q.attr_keys(&range(), Some("checkout"), "bogus", 100).is_err());
        let vals =
            q.attr_values(&range(), Some("checkout"), "resource", "service.version", 50).unwrap();
        assert!(
            vals.sql().contains("toString(resource_attributes.`service.version`) AS value"),
            "{}",
            vals.sql()
        );
        assert!(q.attr_values(&range(), Some("checkout"), "resource", "x`y", 50).is_err());
    }
}
