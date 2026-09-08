//! `/api/traces/*`：链路检索、详情、下拉取值、属性键值。

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde::Serialize;

use super::{AppState, params::Params};
use crate::clickhouse::Stats;
use crate::error::{Error, Result};
use crate::query::TimeRange;
use crate::query::traces::{
    AttrFilter, CLIENT_KINDS, CandidateRow, ENTRY_KINDS, KeyRow, Span, SpanRow, SummaryRow,
    TraceFilter, TraceQueries, TraceSort, TraceSummary, ValueRow, normalize_kind,
    normalize_trace_id,
};
use crate::schema::{Schema, TRACE_FIXED_COLUMNS};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/traces/search", get(search))
        .route("/api/traces/values", get(values))
        .route("/api/traces/attr_keys", get(attr_keys))
        .route("/api/traces/attr_values", get(attr_values))
        .route("/api/traces/{trace_id}", get(detail))
}

const CONTROL_KEYS: &[&str] = &[
    "from",
    "to",
    "service",
    "span_name",
    "kind",
    "error_only",
    "min_ms",
    "max_ms",
    "attr",
    "rattr",
    "trace_id",
    "sort",
    "limit",
    "field",
    "scope",
    "key",
];

fn range(state: &AppState, p: &Params) -> Result<TimeRange> {
    TimeRange::new(p.get_i64("from")?, p.get_i64("to")?, state.now_ms(), state.config.max_range)
}

/// 动态列（静态 fields）筛选：不认识的键直接 400。
fn dims(schema: &Schema, p: &Params) -> Result<Vec<(String, Vec<String>)>> {
    let dim_columns: Vec<&str> = schema
        .traces
        .extra_string_columns(TRACE_FIXED_COLUMNS)
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    let mut dims = Vec::new();
    for key in p.keys() {
        if CONTROL_KEYS.contains(&key) {
            continue;
        }
        if !dim_columns.contains(&key) {
            return Err(Error::bad_request(format!(
                "不认识的筛选列 {key:?}；span 表可用的筛选列: {}",
                if dim_columns.is_empty() {
                    "（无）".to_owned()
                } else {
                    dim_columns.join(", ")
                }
            )));
        }
        let values = p.get_list(key);
        if !values.is_empty() {
            dims.push((key.to_owned(), values));
        }
    }
    Ok(dims)
}

fn build_filter(
    state: &AppState,
    schema: &Schema,
    p: &Params,
    sort: TraceSort,
) -> Result<TraceFilter> {
    let ms_to_ns = |v: Option<f64>| v.map(|ms| (ms.max(0.0) * 1_000_000.0) as u64);
    let mut kinds: Vec<String> =
        p.get_list("kind").iter().map(|k| normalize_kind(k)).collect::<Result<_>>()?;
    // 「最慢的请求」默认看入口 span：不限 kind 的话排在前面的会是慢 SQL、慢下游调用，
    // 而 Java 开发说的慢请求是 Server span
    if sort == TraceSort::Duration && kinds.is_empty() {
        kinds = ENTRY_KINDS.iter().map(|k| (*k).to_owned()).collect();
    }
    Ok(TraceFilter {
        range: Some(range(state, p)?),
        service: p.get("service").map(str::to_owned),
        span_name: p.get("span_name").map(str::to_owned),
        kinds,
        error_only: p.get_bool("error_only")?.unwrap_or(false),
        min_duration_ns: ms_to_ns(p.get_f64("min_ms")?),
        max_duration_ns: ms_to_ns(p.get_f64("max_ms")?),
        attrs: p.get_all("attr").into_iter().map(AttrFilter::parse).collect::<Result<_>>()?,
        resource_attrs: p
            .get_all("rattr")
            .into_iter()
            .map(AttrFilter::parse)
            .collect::<Result<_>>()?,
        dims: dims(schema, p)?,
    })
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub traces: Vec<TraceSummary>,
    pub limit: u32,
    pub sort: TraceSort,
    pub stats: Stats,
}

async fn search(State(state): State<AppState>, p: Params) -> Result<Json<SearchResponse>> {
    let schema = state.schema.get().await?;
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let sort = TraceSort::parse(p.get("sort"))?;
    let limit = p.get_limit("limit", 50, state.config.max_rows.min(500))?;

    // 直接给了 trace id：跳过检索，只查这一条的摘要（不限时间，走 bloom filter）
    let (ids, range, mut stats) = if let Some(raw) = p.get("trace_id") {
        (vec![normalize_trace_id(raw)?], None, Stats::default())
    } else {
        let filter = build_filter(&state, &schema, &p, sort)?;
        let candidates =
            state.client.rows::<CandidateRow>(queries.candidates(&filter, sort, limit)?).await?;
        (
            candidates.rows.into_iter().map(|r| r.trace_id).collect::<Vec<_>>(),
            filter.range,
            candidates.stats,
        )
    };
    if ids.is_empty() {
        return Ok(Json(SearchResponse { traces: Vec::new(), limit, sort, stats }));
    }
    let summaries =
        state.client.rows::<SummaryRow>(queries.summaries(&ids, range.as_ref())?).await?;
    stats.read_rows += summaries.stats.read_rows;
    stats.read_bytes += summaries.stats.read_bytes;
    stats.elapsed_ms += summaries.stats.elapsed_ms;
    stats.result_rows = summaries.stats.result_rows;
    // 保持候选查询的顺序（按时间 / 按耗时）
    let mut by_id: std::collections::HashMap<String, TraceSummary> =
        summaries.rows.into_iter().map(|r| (r.trace_id.clone(), TraceSummary::from(r))).collect();
    let traces: Vec<TraceSummary> = ids.iter().filter_map(|id| by_id.remove(id)).collect();
    // 候选查询刚看到的 trace 在聚合查询里找不到：正常不该发生（同一张表、时间范围还放宽了 10 分钟），
    // 真出现多半是分片 / 副本一时不一致，把 id 记下来好对着库查
    if traces.len() < ids.len() {
        let missing: Vec<&str> = ids
            .iter()
            .filter(|id| !traces.iter().any(|t| &t.trace_id == *id))
            .map(String::as_str)
            .collect();
        tracing::warn!(
            candidates = ids.len(),
            aggregated = traces.len(),
            ?missing,
            "trace summaries missing for some candidate ids"
        );
    }
    Ok(Json(SearchResponse { traces, limit, sort, stats }))
}

#[derive(Serialize)]
pub struct DetailResponse {
    pub trace_id: String,
    pub spans: Vec<Span>,
    /// span 数超过了 `--max-trace-spans`，只返回了前面这些
    pub truncated: bool,
    pub stats: Stats,
}

async fn detail(
    State(state): State<AppState>,
    Path(trace_id): Path<String>,
) -> Result<Json<DetailResponse>> {
    let schema = state.schema.get().await?;
    let trace_id = normalize_trace_id(&trace_id)?;
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let max = state.config.max_trace_spans;
    let result = state.client.rows::<SpanRow>(queries.detail(&trace_id, max)?).await?;
    let truncated = result.rows.len() > max as usize;
    let spans: Vec<Span> = result.rows.into_iter().take(max as usize).map(Span::from).collect();
    Ok(Json(DetailResponse { trace_id, spans, truncated, stats: result.stats }))
}

#[derive(Serialize)]
pub struct ValuesResponse {
    pub field: String,
    pub values: Vec<ValueRow>,
    pub stats: Stats,
}

/// `field=service`：时间范围内的服务；`field=span_name&service=x[&kind=entry|client|all]`：某服务的操作。
async fn values(State(state): State<AppState>, p: Params) -> Result<Json<ValuesResponse>> {
    let schema = state.schema.get().await?;
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let range = range(&state, &p)?;
    let limit = p.get_limit("limit", 200, 2000)?;
    let field = p.get("field").unwrap_or("service");
    let query = match field {
        "service" | "service_name" => queries.services(&range, limit)?,
        "span_name" => {
            let service = p
                .get("service")
                .ok_or_else(|| Error::bad_request("field=span_name 需要 service"))?;
            let kinds: &[&str] = match p.get("kind").unwrap_or("all") {
                "entry" => ENTRY_KINDS,
                "client" => CLIENT_KINDS,
                "all" => &[],
                other => &[Box::leak(normalize_kind(other)?.into_boxed_str())],
            };
            queries.span_names(&range, service, kinds, limit)?
        }
        other => {
            return Err(Error::bad_request(format!(
                "field 只能是 service 或 span_name，不是 {other:?}"
            )));
        }
    };
    let result = state.client.rows::<ValueRow>(query).await?;
    Ok(Json(ValuesResponse { field: field.to_owned(), values: result.rows, stats: result.stats }))
}

#[derive(Serialize)]
pub struct KeysResponse {
    pub scope: String,
    pub keys: Vec<KeyRow>,
    pub stats: Stats,
}

async fn attr_keys(State(state): State<AppState>, p: Params) -> Result<Json<KeysResponse>> {
    let schema = state.schema.get().await?;
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let range = range(&state, &p)?;
    let scope = p.get("scope").unwrap_or("span");
    let limit = p.get_limit("limit", 200, 1000)?;
    let result = state
        .client
        .rows::<KeyRow>(queries.attr_keys(&range, p.get("service"), scope, limit)?)
        .await?;
    Ok(Json(KeysResponse { scope: scope.to_owned(), keys: result.rows, stats: result.stats }))
}

async fn attr_values(State(state): State<AppState>, p: Params) -> Result<Json<ValuesResponse>> {
    let schema = state.schema.get().await?;
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let range = range(&state, &p)?;
    let scope = p.get("scope").unwrap_or("span");
    let key = p.get("key").ok_or_else(|| Error::bad_request("缺少参数 key"))?;
    let limit = p.get_limit("limit", 50, 500)?;
    let result = state
        .client
        .rows::<ValueRow>(queries.attr_values(&range, p.get("service"), scope, key, limit)?)
        .await?;
    Ok(Json(ValuesResponse { field: key.to_owned(), values: result.rows, stats: result.stats }))
}
