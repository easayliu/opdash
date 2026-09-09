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

    fn sql(&self, column: &str, b: &mut Bindings) -> Result<String> {
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

#[derive(Debug, Deserialize)]
pub struct CandidateRow {
    pub trace_id: String,
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
    pub resource_attributes: Value,
    pub span_attributes: Value,
    #[serde(deserialize_with = "num::de_vec")]
    pub event_ts: Vec<i64>,
    pub event_names: Vec<String>,
    pub event_attrs: Vec<Value>,
    pub link_trace_ids: Vec<String>,
    pub link_span_ids: Vec<String>,
    pub link_states: Vec<String>,
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
    pub fn candidates(&self, filter: &TraceFilter, sort: TraceSort, limit: u32) -> Result<Query> {
        filter.validate()?;
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        let limit = b.bind("UInt32", limit);
        let order = match sort {
            TraceSort::Time => "timestamp DESC",
            TraceSort::Duration => "duration_ns DESC, timestamp DESC",
        };
        let sql = format!(
            "SELECT trace_id\nFROM {from}\nWHERE {where_sql}\nORDER BY {order}\nLIMIT 1 BY trace_id\nLIMIT {limit}",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 第二步：这些 trace 各自的摘要。`range` 给了就前后各放宽 10 分钟裁剪分区（同一条 trace 的
    /// 其它 span 可能跨出筛选范围），只按 id 查时不限时间，走 bloom filter。
    pub fn summaries(&self, ids: &[String], range: Option<&TimeRange>) -> Result<Query> {
        if ids.is_empty() {
            return Err(Error::bad_request("没有 trace id"));
        }
        let mut b = Bindings::new();
        let mut clauses = vec![format!("trace_id IN {}", b.bind("Array(String)", ids))];
        if let Some(range) = range {
            clauses.push(b.time_predicate("timestamp", &range.widen(10 * 60_000)));
        }
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
             FROM {from}\nWHERE {where_sql}\nGROUP BY trace_id",
            from = self.table_ref(),
            where_sql = clauses.join("\n  AND "),
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
    /// `window` 给了就加时间谓词裁剪分区：不限时间要把 30 天的索引块都过一遍；从列表页进来时
    /// 知道 trace 的开始时间，圈到前后一两天就只剩一两个分区。
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
    pub fn detail_fetch(&self, trace_id: &str, located: &[LocatedSpan]) -> Result<Query> {
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
            "resource_attributes".into(),
            "span_attributes".into(),
            "arrayMap(t -> toUnixTimestamp64Micro(t), `events.timestamp`) AS event_ts".into(),
            "`events.name` AS event_names".into(),
            "`events.attributes` AS event_attrs".into(),
            "`links.trace_id` AS link_trace_ids".into(),
            "`links.span_id` AS link_span_ids".into(),
            "`links.trace_state` AS link_states".into(),
            "`links.attributes` AS link_attrs".into(),
        ];
        for c in &self.table.columns {
            if !TRACE_FIXED_COLUMNS.contains(&c.name.as_str()) {
                cols.push(quote_ident(&c.name)?);
            }
        }
        let sql = format!(
            "SELECT {cols}\nFROM {from}\nWHERE trace_id = {id}\n  AND service_name IN {services}{names_sql}\n  AND {time}\nORDER BY timestamp, span_id\nLIMIT 1 BY span_id\nLIMIT {limit}",
            cols = cols.join(", "),
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
        let mut b = Bindings::new();
        let time = b.time_predicate("timestamp", range);
        let service_sql = match service {
            Some(s) => format!("\n  AND service_name = {}", b.bind("String", s)),
            None => String::new(),
        };
        let sample = b.bind("UInt32", ATTR_SAMPLE_ROWS);
        let limit = b.bind("UInt32", limit);
        let sql = format!(
            "SELECT key, count() AS count\nFROM (\n  SELECT arrayJoin(JSONAllPaths({column})) AS key\n  FROM {from}\n  WHERE {time}{service_sql}\n  LIMIT {sample}\n)\nGROUP BY key\nORDER BY count DESC, key\nLIMIT {limit}",
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
        let mut b = Bindings::new();
        let time = b.time_predicate("timestamp", range);
        let service_sql = match service {
            Some(s) => format!("\n  AND service_name = {}", b.bind("String", s)),
            None => String::new(),
        };
        let sample = b.bind("UInt32", ATTR_SAMPLE_ROWS);
        let limit = b.bind("UInt32", limit);
        let sql = format!(
            "SELECT value, count() AS count\nFROM (\n  SELECT toString({path}) AS value\n  FROM {from}\n  WHERE {time}{service_sql}\n  LIMIT {sample}\n)\nWHERE value != ''\nGROUP BY value\nORDER BY count DESC, value\nLIMIT {limit}",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 服务概览：每个服务的入口请求量、错误数、延迟分位。
    /// `quantilesTDigest` 内存有界、跨分片能合并；默认的 `quantiles` 是 8192 个样本的水塘抽样，
    /// 尾部分位恰恰最不准。单位直接换成毫秒，JSON 里是普通浮点数。
    pub fn service_overview(
        &self,
        range: &TimeRange,
        dims: &[(String, Vec<String>)],
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
        let sql = format!(
            "SELECT service_name, count() AS requests, countIf(status_code = 'Error') AS errors,\n  \
             quantilesTDigest(0.5, 0.95, 0.99)(toFloat64(duration_ns) / 1e6) AS q, max(duration_ns) / 1e6 AS max_ms\n\
             FROM {from}\nWHERE {where_sql}\nGROUP BY service_name\nORDER BY requests DESC, service_name",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 某个服务按 span_name（接口 / 下游调用）的指标。`kinds` 决定看入口还是对外调用。
    pub fn operations(&self, range: &TimeRange, service: &str, kinds: &[&str]) -> Result<Query> {
        let mut b = Bindings::new();
        let time = b.time_predicate("timestamp", range);
        let service = b.bind("String", service);
        let kinds = b.bind("Array(String)", kinds);
        let sql = format!(
            "SELECT span_name, span_kind, count() AS requests, countIf(status_code = 'Error') AS errors,\n  \
             quantilesTDigest(0.5, 0.95, 0.99)(toFloat64(duration_ns) / 1e6) AS q, max(duration_ns) / 1e6 AS max_ms\n\
             FROM {from}\nWHERE {time}\n  AND service_name = {service}\n  AND span_kind IN {kinds}\n\
             GROUP BY span_name, span_kind\nORDER BY requests DESC, span_name",
            from = self.table_ref(),
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
        let query = q.candidates(&filter, TraceSort::Duration, 50).unwrap();
        let sql = query.sql();
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
        assert!(q.candidates(&bad, TraceSort::Time, 10).is_err());
        assert!(attr_path("span_attributes", "a\\b").is_err());
        assert_eq!(
            attr_path("span_attributes", " http.route ").unwrap(),
            "span_attributes.`http.route`"
        );

        let s = q.summaries(&["a".into(), "b".into()], Some(&range())).unwrap();
        assert!(s.sql().contains("trace_id IN {p0:Array(String)}"), "{}", s.sql());
        assert!(s.sql().contains("uniqExact(span_id) AS span_count"));
        assert_eq!(s.params()[0].1, "['a','b']");
        // 放宽了 10 分钟
        assert_eq!(s.params()[1].1, (1_000_000 - 600_000).to_string());
        assert_eq!(s.params()[2].1, (2_000_000 + 600_000).to_string());
        assert!(q.summaries(&[], None).is_err());
    }

    #[test]
    fn unscoped_search_is_limited_to_six_hours() {
        let table = table();
        let q = TraceQueries { database: "logs", table: &table };
        let wide = TimeRange { from_ms: 0, to_ms: 7 * 3_600_000 };
        let filter = TraceFilter { range: Some(wide), ..Default::default() };
        assert!(q.candidates(&filter, TraceSort::Time, 10).is_err());
        let scoped =
            TraceFilter { range: Some(wide), service: Some("x".into()), ..Default::default() };
        assert!(q.candidates(&scoped, TraceSort::Time, 10).is_ok());
        assert!(q.candidates(&TraceFilter::default(), TraceSort::Time, 10).is_err());
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
        let f = q.detail_fetch("abc", &located).unwrap();
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
        // 重列在这一步取
        assert!(f.sql().contains("`events.attributes` AS event_attrs"));
        assert!(f.sql().contains(", `cluster`\n"), "{}", f.sql());
        assert_eq!(f.settings(), &[("optimize_skip_unused_shards", "1".to_owned())]);
        assert!(q.detail_fetch("abc", &[]).is_err());
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
        let f = q.detail_fetch("abc", &located).unwrap();
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
        let f = q.detail_fetch("abc", &noisy).unwrap();
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
        let o = q.service_overview(&range(), &[]).unwrap();
        assert!(
            o.sql()
                .contains("quantilesTDigest(0.5, 0.95, 0.99)(toFloat64(duration_ns) / 1e6) AS q"),
            "{}",
            o.sql()
        );
        assert!(o.sql().contains("span_kind IN {p2:Array(String)}"));
        assert_eq!(o.params()[2].1, "['Server','Consumer']");
        let ops = q.operations(&range(), "svc", CLIENT_KINDS).unwrap();
        assert_eq!(ops.params()[3].1, "['Client','Producer']");
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
        assert!(q.attr_keys(&range(), None, "bogus", 100).is_err());
        let vals = q.attr_values(&range(), None, "resource", "service.version", 50).unwrap();
        assert!(
            vals.sql().contains("toString(resource_attributes.`service.version`) AS value"),
            "{}",
            vals.sql()
        );
        assert!(q.attr_values(&range(), None, "resource", "x`y", 50).is_err());
    }
}
