//! `/api/services*`：服务概览、接口表、时间序列。全部只看入口 span（Server / Consumer）。

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde::Serialize;

use super::{AppState, params::Params};
use crate::clickhouse::Stats;
use crate::error::{Error, Result};
use crate::query::traces::{
    CLIENT_KINDS, ENTRY_KINDS, OperationRow, ServiceRow, TimeseriesRow, TraceQueries,
};
use crate::query::{Bucket, TimeRange, parse_tz};
use crate::schema::TRACE_FIXED_COLUMNS;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/services", get(overview))
        .route("/api/services/{name}/operations", get(operations))
        .route("/api/services/{name}/timeseries", get(timeseries))
}

fn range(state: &AppState, p: &Params) -> Result<TimeRange> {
    TimeRange::new(p.get_i64("from")?, p.get_i64("to")?, state.now_ms(), state.config.max_range)
}

#[derive(Serialize)]
pub struct ServiceStat {
    pub service: String,
    pub requests: u64,
    pub errors: u64,
    /// 0 ~ 1
    pub error_rate: f64,
    /// 每秒请求数（按整个时间范围平均）
    pub rps: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

fn pct(q: &[f64], i: usize) -> f64 {
    q.get(i).copied().unwrap_or(0.0)
}

#[derive(Serialize)]
pub struct OverviewResponse {
    pub from_ms: i64,
    pub to_ms: i64,
    pub services: Vec<ServiceStat>,
    pub stats: Stats,
}

async fn overview(State(state): State<AppState>, p: Params) -> Result<Json<OverviewResponse>> {
    let schema = state.schema.get().await?;
    let range = range(&state, &p)?;
    let dim_columns: Vec<&str> = schema
        .traces
        .extra_string_columns(TRACE_FIXED_COLUMNS)
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    let mut dims = Vec::new();
    for key in p.keys() {
        if ["from", "to"].contains(&key) {
            continue;
        }
        if !dim_columns.contains(&key) {
            return Err(Error::bad_request(format!("不认识的筛选列 {key:?}")));
        }
        dims.push((key.to_owned(), p.get_list(key)));
    }
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let result = state.client.rows::<ServiceRow>(queries.service_overview(&range, &dims)?).await?;
    let secs = (range.span_ms() as f64 / 1000.0).max(1.0);
    let services = result
        .rows
        .into_iter()
        .map(|r| ServiceStat {
            service: r.service_name,
            requests: r.requests,
            errors: r.errors,
            error_rate: if r.requests > 0 { r.errors as f64 / r.requests as f64 } else { 0.0 },
            rps: r.requests as f64 / secs,
            p50_ms: pct(&r.q, 0),
            p95_ms: pct(&r.q, 1),
            p99_ms: pct(&r.q, 2),
            max_ms: r.max_ms,
        })
        .collect();
    Ok(Json(OverviewResponse {
        from_ms: range.from_ms,
        to_ms: range.to_ms,
        services,
        stats: result.stats,
    }))
}

#[derive(Serialize)]
pub struct OperationStat {
    pub span_name: String,
    pub kind: String,
    pub requests: u64,
    pub errors: u64,
    pub error_rate: f64,
    pub rps: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
}

#[derive(Serialize)]
pub struct OperationsResponse {
    pub service: String,
    /// `entry`（Server / Consumer）或 `client`（Client / Producer）
    pub kind: String,
    pub operations: Vec<OperationStat>,
    pub stats: Stats,
}

async fn operations(
    State(state): State<AppState>,
    Path(name): Path<String>,
    p: Params,
) -> Result<Json<OperationsResponse>> {
    let schema = state.schema.get().await?;
    let range = range(&state, &p)?;
    let kind = p.get("kind").unwrap_or("entry");
    let kinds: &[&str] = match kind {
        "entry" => ENTRY_KINDS,
        "client" => CLIENT_KINDS,
        other => {
            return Err(Error::bad_request(format!("kind 只能是 entry 或 client，不是 {other:?}")));
        }
    };
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let result =
        state.client.rows::<OperationRow>(queries.operations(&range, &name, kinds)?).await?;
    let secs = (range.span_ms() as f64 / 1000.0).max(1.0);
    let operations = result
        .rows
        .into_iter()
        .map(|r| OperationStat {
            span_name: r.span_name,
            kind: r.span_kind,
            requests: r.requests,
            errors: r.errors,
            error_rate: if r.requests > 0 { r.errors as f64 / r.requests as f64 } else { 0.0 },
            rps: r.requests as f64 / secs,
            p50_ms: pct(&r.q, 0),
            p95_ms: pct(&r.q, 1),
            p99_ms: pct(&r.q, 2),
            max_ms: r.max_ms,
        })
        .collect();
    Ok(Json(OperationsResponse {
        service: name,
        kind: kind.to_owned(),
        operations,
        stats: result.stats,
    }))
}

#[derive(Serialize)]
pub struct TimeseriesResponse {
    pub service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span_name: Option<String>,
    pub width_ms: i64,
    pub from_ms: i64,
    pub to_ms: i64,
    pub points: Vec<Point>,
    pub stats: Stats,
}

#[derive(Serialize)]
pub struct Point {
    pub t_ms: i64,
    pub requests: u64,
    pub errors: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}

async fn timeseries(
    State(state): State<AppState>,
    Path(name): Path<String>,
    p: Params,
) -> Result<Json<TimeseriesResponse>> {
    let schema = state.schema.get().await?;
    let range = range(&state, &p)?;
    let tz = parse_tz(&state.config.timezone)?;
    let bucket = Bucket::choose(&range, tz, 120);
    let span_name = p.get("span_name").map(str::to_owned);
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let result = state
        .client
        .rows::<TimeseriesRow>(queries.timeseries(&range, &name, span_name.as_deref(), &bucket)?)
        .await?;
    let first = bucket.first_index(&range);
    let count = bucket.count(&range).max(0) as usize;
    let mut points: Vec<Point> = (0..count as i64)
        .map(|i| Point {
            t_ms: bucket.start_ms(first + i),
            requests: 0,
            errors: 0,
            p50_ms: 0.0,
            p95_ms: 0.0,
            p99_ms: 0.0,
        })
        .collect();
    for row in result.rows {
        let idx = row.bucket - first;
        if idx < 0 || idx as usize >= points.len() {
            continue;
        }
        let pt = &mut points[idx as usize];
        pt.requests = row.requests;
        pt.errors = row.errors;
        pt.p50_ms = pct(&row.q, 0);
        pt.p95_ms = pct(&row.q, 1);
        pt.p99_ms = pct(&row.q, 2);
    }
    Ok(Json(TimeseriesResponse {
        service: name,
        span_name,
        width_ms: bucket.width_ms,
        from_ms: range.from_ms,
        to_ms: range.to_ms,
        points,
        stats: result.stats,
    }))
}
