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
    CLIENT_KINDS, ENTRY_KINDS, OperationRow, ServiceRow, SparkRow, TimeseriesRow, TraceQueries,
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
    /// 上一个同样长的时间窗里的同一组数（对比用）；那段时间没这个服务就是 null
    pub prev: Option<PrevStat>,
    /// 迷你趋势：每个桶的请求数 / 错误数，桶宽见响应的 `spark_width_ms`
    pub spark: Spark,
}

#[derive(Serialize, Clone, Copy)]
pub struct PrevStat {
    pub requests: u64,
    pub errors: u64,
    pub error_rate: f64,
    pub rps: f64,
    pub p95_ms: f64,
}

#[derive(Serialize, Default, Clone)]
pub struct Spark {
    pub requests: Vec<u64>,
    pub errors: Vec<u64>,
    /// 对比窗口里同一格的请求数，画成浅灰影子：形状一比就知道「今天这个时段本来就该这样」
    /// 还是「今天不一样」
    pub prev_requests: Vec<u64>,
}

fn pct(q: &[f64], i: usize) -> f64 {
    q.get(i).copied().unwrap_or(0.0)
}

#[derive(Serialize)]
pub struct OverviewResponse {
    pub from_ms: i64,
    pub to_ms: i64,
    /// 对比用的窗口，由 `compare` 决定：`prev` 紧挨着的上一段、`day` 昨天同一时段、`week` 上周同一时段
    pub compare: String,
    pub prev_from_ms: i64,
    pub prev_to_ms: i64,
    pub spark_width_ms: i64,
    pub services: Vec<ServiceStat>,
    pub stats: Stats,
}

/// 迷你趋势的桶数。卡片上只有一两百像素宽，再多也看不出来。
const SPARK_BUCKETS: i64 = 30;

/// 对比窗口怎么取。「上一周期」在白天永远在涨、晚上永远在跌（早高峰的自然爬坡会被当成
/// +60%），所以默认和**昨天同一时段**比；有周期性业务的再选上周。
fn compare_window(kind: &str, range: &TimeRange) -> Result<(String, TimeRange)> {
    let shift = match kind {
        "prev" => range.span_ms(),
        "day" => 24 * 3_600_000,
        "week" => 7 * 24 * 3_600_000,
        other => {
            return Err(Error::bad_request(format!(
                "compare 只能是 prev / day / week，不是 {other:?}"
            )));
        }
    };
    Ok((
        kind.to_owned(),
        TimeRange { from_ms: (range.from_ms - shift).max(0), to_ms: (range.to_ms - shift).max(1) },
    ))
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
        if ["from", "to", "compare"].contains(&key) {
            continue;
        }
        if !dim_columns.contains(&key) {
            return Err(Error::bad_request(format!("不认识的筛选列 {key:?}")));
        }
        dims.push((key.to_owned(), p.get_list(key)));
    }
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let (compare, prev_range) = compare_window(p.get("compare").unwrap_or("day"), &range)?;
    let tz = parse_tz(&state.config.timezone)?;
    let bucket = Bucket::choose(&range, tz, SPARK_BUCKETS);
    // 对比窗口的桶按同样的宽度、从它自己的起点切，这样两边第 i 格对应同一个相对时刻
    let prev_bucket = Bucket {
        width_ms: bucket.width_ms,
        origin_ms: bucket.origin_ms - (range.from_ms - prev_range.from_ms),
    };
    // 四条查询互不依赖，一起发
    let (current, previous, sparks, prev_sparks) = tokio::try_join!(
        state.client.rows::<ServiceRow>(queries.service_overview(&range, &dims)?),
        state.client.rows::<ServiceRow>(queries.service_overview(&prev_range, &dims)?),
        state.client.rows::<SparkRow>(queries.service_sparklines(&range, &dims, &bucket)?),
        state.client.rows::<SparkRow>(queries.service_sparklines(
            &prev_range,
            &dims,
            &prev_bucket
        )?),
    )?;
    let mut stats = current.stats;
    stats.absorb(&previous.stats);
    stats.absorb(&sparks.stats);
    stats.absorb(&prev_sparks.stats);

    let secs = (range.span_ms() as f64 / 1000.0).max(1.0);
    let prev_secs = (prev_range.span_ms() as f64 / 1000.0).max(1.0);
    let prev_by_name: std::collections::HashMap<String, PrevStat> = previous
        .rows
        .into_iter()
        .map(|r| {
            let stat = PrevStat {
                requests: r.requests,
                errors: r.errors,
                error_rate: if r.requests > 0 { r.errors as f64 / r.requests as f64 } else { 0.0 },
                rps: r.requests as f64 / prev_secs,
                p95_ms: pct(&r.q, 1),
            };
            (r.service_name, stat)
        })
        .collect();
    let first = bucket.first_index(&range);
    // 最后一格多半是半截的（范围右端就是「现在」），画出来每张卡末尾都掉一截，直接不要
    let mut count = bucket.count(&range).max(0) as usize;
    if count > 1 && bucket.start_ms(first + count as i64 - 1) + bucket.width_ms > range.to_ms {
        count -= 1;
    }
    let empty = || Spark {
        requests: vec![0; count],
        errors: vec![0; count],
        prev_requests: vec![0; count],
    };
    let mut spark_by_name: std::collections::HashMap<String, Spark> =
        std::collections::HashMap::new();
    for row in sparks.rows {
        let idx = row.bucket - first;
        if idx < 0 || idx as usize >= count {
            continue;
        }
        let s = spark_by_name.entry(row.service_name).or_insert_with(empty);
        s.requests[idx as usize] = row.requests;
        s.errors[idx as usize] = row.errors;
    }
    let prev_first = prev_bucket.first_index(&prev_range);
    for row in prev_sparks.rows {
        let idx = row.bucket - prev_first;
        if idx < 0 || idx as usize >= count {
            continue;
        }
        spark_by_name.entry(row.service_name).or_insert_with(empty).prev_requests[idx as usize] =
            row.requests;
    }

    let services = current
        .rows
        .into_iter()
        .map(|r| ServiceStat {
            prev: prev_by_name.get(&r.service_name).map(|p| PrevStat { ..*p }),
            spark: spark_by_name.remove(&r.service_name).unwrap_or_else(empty),
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
        compare,
        prev_from_ms: prev_range.from_ms,
        prev_to_ms: prev_range.to_ms,
        spark_width_ms: bucket.width_ms,
        services,
        stats,
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
