//! `/api/logs/*`：检索、直方图、facet、上下文、导出。

use std::collections::BTreeMap;

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Serialize;

use super::{AppState, params::Params};
use crate::clickhouse::Stats;
use crate::error::{Error, Result};
use crate::query::logs::{
    CONTEXT_WINDOW_MS, CountRow, FacetRow, HistogramRow, LogFilter, LogQueries, LogRow, Order,
    facetable,
};
use crate::query::{Bucket, TimeRange, parse_tz};
use crate::schema::{LOG_FIXED_COLUMNS, Schema};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/logs/search", get(search))
        .route("/api/logs/histogram", get(histogram))
        .route("/api/logs/facets", get(facets))
        .route("/api/logs/context", get(context))
        .route("/api/logs/export", get(export))
}

/// 查询串里这些键是筛选条件以外的控制参数，其余的键都当动态列名处理。
const CONTROL_KEYS: &[&str] = &[
    "from", "to", "q", "regex", "level", "logger", "thread", "host", "trace_id", "span_id",
    "order", "limit", "offset", "field", "format", "ts", "file", "before", "after", "count",
];

/// 从查询串拼筛选条件。动态列名必须是表里真有的字符串列，否则 400 报清楚。
fn build_filter(state: &AppState, schema: &Schema, p: &Params) -> Result<LogFilter> {
    let has_id = p.get("trace_id").is_some() || p.get("span_id").is_some();
    let explicit_range = p.get("from").is_some() || p.get("to").is_some();
    let range = if has_id && !explicit_range {
        None
    } else {
        Some(TimeRange::new(
            p.get_i64("from")?,
            p.get_i64("to")?,
            state.now_ms(),
            state.config.max_range,
        )?)
    };
    let mut dims = Vec::new();
    let dim_columns: Vec<&str> = schema
        .logs
        .extra_string_columns(LOG_FIXED_COLUMNS)
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    for key in p.keys() {
        if CONTROL_KEYS.contains(&key) {
            continue;
        }
        if !dim_columns.contains(&key) {
            return Err(Error::bad_request(format!(
                "不认识的筛选列 {key:?}；日志表可用的筛选列: {}",
                dim_columns.join(", ")
            )));
        }
        let values = p.get_list(key);
        if !values.is_empty() {
            dims.push((key.to_owned(), values));
        }
    }
    let filter = LogFilter {
        range,
        q: p.get("q").unwrap_or("").to_owned(),
        regex: p.get_bool("regex")?.unwrap_or(false),
        levels: p.get_list("level").into_iter().map(|l| l.to_ascii_uppercase()).collect(),
        logger: p.get("logger").map(str::to_owned),
        thread: p.get("thread").map(str::to_owned),
        host: p.get("host").map(str::to_owned),
        trace_id: p.get("trace_id").map(|s| s.trim().to_ascii_lowercase()),
        span_id: p.get("span_id").map(|s| s.trim().to_ascii_lowercase()),
        dims,
    };
    filter.validate()?;
    Ok(filter)
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub rows: Vec<LogRow>,
    /// 满足条件的总数。只在第一页（offset = 0）算；日志页有直方图时会传 `count=0` 关掉它，
    /// 改用直方图各桶之和，不用为了一个数再扫一遍同样的数据。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// 这些词是按整词（而不是子串）匹配的——够长的标识符走了 message 上的 token 索引。
    /// 页面上要提示，不然「搜 id 的前半截搜不到」会很费解。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub token_terms: Vec<String>,
    pub limit: u32,
    pub offset: u32,
    pub order: Order,
    pub stats: Stats,
}

async fn search(State(state): State<AppState>, p: Params) -> Result<Json<SearchResponse>> {
    let schema = state.schema.get().await?;
    let filter = build_filter(&state, &schema, &p)?;
    let order = Order::parse(p.get("order"))?;
    let limit = p.get_limit("limit", 200, state.config.max_rows)?;
    let offset = p.get_u32("offset")?.unwrap_or(0);
    if offset > state.config.max_offset {
        return Err(Error::bad_request(format!(
            "最多翻到第 {} 条，再往后请缩小时间范围或加筛选条件",
            state.config.max_offset
        )));
    }
    let queries = LogQueries { database: &state.config.database, table: &schema.logs };
    let want_total = offset == 0 && p.get_bool("count")?.unwrap_or(true);
    let token_terms = filter.token_terms();
    let search_q = queries.search(&filter, order, limit, offset)?;

    if want_total && filter.has_message_predicate() {
        // 关键字没有索引，这一页本来就要扫完整个范围，顺手把总数也数出来，不用扫两遍
        let r = state.client.rows_with_total::<LogRow>(search_q).await?;
        return Ok(Json(SearchResponse {
            rows: r.rows,
            total: r.total,
            token_terms,
            limit,
            offset,
            order,
            stats: r.stats,
        }));
    }
    let rows_fut = state.client.rows::<LogRow>(search_q);
    if want_total {
        // 没有关键字时这一页按排序键读够就停，很快；总数另起一条 count() 并行跑
        let count_q = queries.count(&filter)?;
        let (rows, count) = tokio::join!(rows_fut, state.client.rows::<CountRow>(count_q));
        let rows = rows?;
        // 总数只是个参考，超时了就不给，不能把结果也拖死
        let total = match count {
            Ok(c) => c.rows.first().map(|r| r.count),
            Err(e) => {
                tracing::warn!(error = %e, "count query failed; returning rows without total");
                None
            }
        };
        return Ok(Json(SearchResponse {
            total,
            token_terms,
            limit,
            offset,
            order,
            stats: rows.stats,
            rows: rows.rows,
        }));
    }
    let rows = rows_fut.await?;
    Ok(Json(SearchResponse {
        total: None,
        token_terms,
        limit,
        offset,
        order,
        stats: rows.stats,
        rows: rows.rows,
    }))
}

#[derive(Serialize)]
pub struct HistogramResponse {
    pub width_ms: i64,
    pub from_ms: i64,
    pub to_ms: i64,
    /// 出现过的级别，按严重程度排（前端堆叠顺序）
    pub levels: Vec<String>,
    pub buckets: Vec<HistogramBucket>,
    /// 范围内的总条数 = 各桶之和。时间条件是左闭右开、桶按同一个原点切，每一行都落在某个桶里，
    /// 所以这个和就是 `count()` 的结果——页面拿它当总数，省掉一条扫同样数据的 count 查询。
    pub total: u64,
    pub stats: Stats,
}

#[derive(Serialize)]
pub struct HistogramBucket {
    pub t_ms: i64,
    pub total: u64,
    pub counts: BTreeMap<String, u64>,
}

/// 级别的严重程度顺序；不认识的排最后。
fn level_rank(level: &str) -> usize {
    const ORDER: &[&str] = &["FATAL", "ERROR", "WARN", "WARNING", "INFO", "DEBUG", "TRACE"];
    ORDER.iter().position(|l| l.eq_ignore_ascii_case(level)).unwrap_or(ORDER.len())
}

async fn histogram(State(state): State<AppState>, p: Params) -> Result<Json<HistogramResponse>> {
    let schema = state.schema.get().await?;
    let filter = build_filter(&state, &schema, &p)?;
    let Some(range) = filter.range else {
        return Err(Error::bad_request("直方图需要时间范围（from / to）"));
    };
    let tz = parse_tz(&state.config.timezone)?;
    let bucket = Bucket::choose(&range, tz, 120);
    let queries = LogQueries { database: &state.config.database, table: &schema.logs };
    let result = state.client.rows::<HistogramRow>(queries.histogram(&filter, &bucket)?).await?;

    let first = bucket.first_index(&range);
    let count = bucket.count(&range).max(0) as usize;
    let mut buckets: Vec<HistogramBucket> = (0..count as i64)
        .map(|i| HistogramBucket {
            t_ms: bucket.start_ms(first + i),
            total: 0,
            counts: BTreeMap::new(),
        })
        .collect();
    let mut levels: Vec<String> = Vec::new();
    for row in result.rows {
        let idx = row.bucket - first;
        if idx < 0 || idx as usize >= buckets.len() {
            continue;
        }
        let b = &mut buckets[idx as usize];
        b.total += row.count;
        *b.counts.entry(row.level.clone()).or_default() += row.count;
        if !levels.contains(&row.level) {
            levels.push(row.level);
        }
    }
    levels.sort_by_key(|l| (level_rank(l), l.clone()));
    let total = buckets.iter().map(|b| b.total).sum();
    Ok(Json(HistogramResponse {
        width_ms: bucket.width_ms,
        from_ms: range.from_ms,
        to_ms: range.to_ms,
        levels,
        buckets,
        total,
        stats: result.stats,
    }))
}

#[derive(Serialize)]
pub struct FacetsResponse {
    pub field: String,
    pub values: Vec<FacetRow>,
    pub stats: Stats,
}

async fn facets(State(state): State<AppState>, p: Params) -> Result<Json<FacetsResponse>> {
    let schema = state.schema.get().await?;
    let field = p.get("field").ok_or_else(|| Error::bad_request("缺少参数 field"))?.to_owned();
    if !facetable(&schema.logs, &field) {
        return Err(Error::bad_request(format!("列 {field:?} 不支持统计取值")));
    }
    let filter = build_filter(&state, &schema, &p)?;
    let limit = p.get_limit("limit", 50, 500)?;
    let queries = LogQueries { database: &state.config.database, table: &schema.logs };
    let result = state.client.rows::<FacetRow>(queries.facets(&filter, &field, limit)?).await?;
    Ok(Json(FacetsResponse { field, values: result.rows, stats: result.stats }))
}

#[derive(Serialize)]
pub struct ContextResponse {
    /// 锚点之前的行（含锚点那一毫秒），时间升序
    pub before: Vec<LogRow>,
    /// 锚点之后的行，时间升序
    pub after: Vec<LogRow>,
    /// 往前 / 往后最多找了多远（毫秒）；行数不足说明这个窗口内就这么多
    pub window_ms: i64,
    pub stats: Stats,
}

async fn context(State(state): State<AppState>, p: Params) -> Result<Json<ContextResponse>> {
    let schema = state.schema.get().await?;
    let host = p.get("host").ok_or_else(|| Error::bad_request("缺少参数 host"))?;
    let file = p.get("file").ok_or_else(|| Error::bad_request("缺少参数 file"))?;
    let ts = p.get_i64("ts")?.ok_or_else(|| Error::bad_request("缺少参数 ts（unix 毫秒）"))?;
    let before_n = p.get_limit("before", 50, 500)?;
    let after_n = p.get_limit("after", 50, 500)?;
    let queries = LogQueries { database: &state.config.database, table: &schema.logs };
    let (before, after) = tokio::join!(
        state.client.rows::<LogRow>(queries.context(host, file, ts, true, before_n)?),
        state.client.rows::<LogRow>(queries.context(host, file, ts, false, after_n)?),
    );
    let (before, after) = (before?, after?);
    let mut before_rows = before.rows;
    before_rows.reverse();
    let stats = Stats {
        read_rows: before.stats.read_rows + after.stats.read_rows,
        read_bytes: before.stats.read_bytes + after.stats.read_bytes,
        result_rows: before.stats.result_rows + after.stats.result_rows,
        elapsed_ms: before.stats.elapsed_ms.max(after.stats.elapsed_ms),
    };
    Ok(Json(ContextResponse {
        before: before_rows,
        after: after.rows,
        window_ms: CONTEXT_WINDOW_MS,
        stats,
    }))
}

/// 导出 CSV / JSONL：ClickHouse 直接出格式化文本，这里只是转发字节流。
async fn export(State(state): State<AppState>, p: Params) -> Result<Response> {
    let schema = state.schema.get().await?;
    let filter = build_filter(&state, &schema, &p)?;
    let order = Order::parse(p.get("order"))?;
    let limit = p.get_limit("limit", state.config.export_max_rows, state.config.export_max_rows)?;
    let (format, content_type, ext) = match p.get("format").unwrap_or("csv") {
        "csv" => ("CSVWithNames", "text/csv; charset=utf-8", "csv"),
        "jsonl" | "json" => ("JSONEachRow", "application/x-ndjson; charset=utf-8", "jsonl"),
        other => {
            return Err(Error::bad_request(format!("format 只能是 csv 或 jsonl，不是 {other:?}")));
        }
    };
    let queries = LogQueries { database: &state.config.database, table: &schema.logs };
    let resp = state.client.send(&queries.export(&filter, order, limit)?, Some(format)).await?;
    let name = match &filter.range {
        Some(r) => format!("logs-{}-{}.{ext}", r.from_ms / 1000, r.to_ms / 1000),
        None => format!(
            "logs-{}.{ext}",
            filter.trace_id.as_deref().or(filter.span_id.as_deref()).unwrap_or("export")
        ),
    };
    let body = Body::from_stream(resp.bytes_stream());
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    Ok(response.into_response())
}
