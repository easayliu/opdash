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
        .route("/api/services/operations", get(operations_many))
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

/// 一次最多问几个服务的接口表。总览页只对不健康的服务问，正常不会有这么多；真有的话
/// `service_name IN` 的列表和返回的行数都会失控，宁可让页面退回一个一个问。
const MAX_SERVICES_PER_QUERY: usize = 24;

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

/// 和 [`compare_window`] 一样，多认一个 `none`：只看当前窗口，省掉一半查询。接口表和时间序列
/// 各自要多跑一条查询才能给出对比，调用方不需要对比时能关掉。
fn compare_window_opt(kind: &str, range: &TimeRange) -> Result<(String, Option<TimeRange>)> {
    if kind == "none" {
        return Ok((kind.to_owned(), None));
    }
    compare_window(kind, range).map(|(kind, prev)| (kind, Some(prev))).map_err(|_| {
        Error::bad_request(format!("compare 只能是 prev / day / week / none，不是 {kind:?}"))
    })
}

/// [`TraceQueries::service_stats`] 一条查询回来的是两种行混在一起的，按 `is_total` 拆开：
/// 前一半是每个服务整窗的汇总，后一半是迷你趋势的每一格。`partition` 保序，所以汇总那批
/// 仍然是 SQL 里 `requests DESC` 的顺序，前端拿到的排序没变。
fn split_totals(rows: Vec<ServiceRow>) -> (Vec<ServiceRow>, Vec<ServiceRow>) {
    rows.into_iter().partition(|r| r.is_total == 1)
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
    // 当前窗、对比窗各一条，每条同时带回整窗汇总和分桶（GROUPING SETS，见 service_stats）
    let (current, previous) = tokio::try_join!(
        state.client.rows::<ServiceRow>(queries.service_stats(&range, &dims, &bucket)?),
        state.client.rows::<ServiceRow>(queries.service_stats(&prev_range, &dims, &prev_bucket)?),
    )?;
    let mut stats = current.stats;
    stats.absorb(&previous.stats);
    let (current, sparks) = split_totals(current.rows);
    let (previous, prev_sparks) = split_totals(previous.rows);

    let secs = (range.span_ms() as f64 / 1000.0).max(1.0);
    let prev_secs = (prev_range.span_ms() as f64 / 1000.0).max(1.0);
    let prev_by_name: std::collections::HashMap<String, PrevStat> = previous
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
    for row in sparks {
        let idx = row.bucket - first;
        if idx < 0 || idx as usize >= count {
            continue;
        }
        let s = spark_by_name.entry(row.service_name).or_insert_with(empty);
        s.requests[idx as usize] = row.requests;
        s.errors[idx as usize] = row.errors;
    }
    let prev_first = prev_bucket.first_index(&prev_range);
    for row in prev_sparks {
        let idx = row.bucket - prev_first;
        if idx < 0 || idx as usize >= count {
            continue;
        }
        spark_by_name.entry(row.service_name).or_insert_with(empty).prev_requests[idx as usize] =
            row.requests;
    }

    let services = current
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
    /// 一次问多个服务时按它分组；单服务那条路上就是路径里的那个名字
    pub service: String,
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
    /// 对比窗口里同一个接口的同一组数。`compare=none`、或者那段时间没有这个接口（新上的接口）
    /// 就是 null
    pub prev: Option<PrevOp>,
}

#[derive(Serialize, Clone, Copy)]
pub struct PrevOp {
    pub requests: u64,
    pub errors: u64,
    pub error_rate: f64,
    pub rps: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
}

#[derive(Serialize)]
pub struct OperationsResponse {
    pub service: String,
    /// `entry`（Server / Consumer）或 `client`（Client / Producer）
    pub kind: String,
    pub from_ms: i64,
    pub to_ms: i64,
    /// 对比窗口怎么取，见 [`compare_window_opt`]。`none` 表示没查对比窗口，每行的 `prev` 都是 null
    pub compare: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_from_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_to_ms: Option<i64>,
    pub operations: Vec<OperationStat>,
    pub stats: Stats,
}

/// `kind` 参数 → 要看哪几种 span。
fn parse_kinds(p: &Params) -> Result<(&'static str, &'static [&'static str])> {
    match p.get("kind").unwrap_or("entry") {
        "entry" => Ok(("entry", ENTRY_KINDS)),
        "client" => Ok(("client", CLIENT_KINDS)),
        other => Err(Error::bad_request(format!("kind 只能是 entry 或 client，不是 {other:?}"))),
    }
}

/// 接口表查完对齐好的结果，两个 handler（单服务、一次多个服务）共用。
struct Operations {
    rows: Vec<OperationStat>,
    stats: Stats,
    compare: String,
    prev_range: Option<TimeRange>,
}

/// 当前窗和对比窗各查一次，按 `(service, span_name, span_kind)` 对齐成一行，前端按变化排序。
///
/// 两条查询都锁定了 `service_name`（排序键第一列），走排序键前缀，加一条的代价和第一条
/// 差不多，而且是并发发的。
async fn collect_operations(
    state: &AppState,
    queries: &TraceQueries<'_>,
    p: &Params,
    range: &TimeRange,
    services: &[&str],
    kinds: &[&str],
) -> Result<Operations> {
    let (compare, prev_range) = compare_window_opt(p.get("compare").unwrap_or("day"), range)?;
    let current_query = queries.operations(range, services, kinds)?;
    let (current, previous) = match &prev_range {
        Some(prev) => {
            let prev_query = queries.operations(prev, services, kinds)?;
            let (current, previous) = tokio::try_join!(
                state.client.rows::<OperationRow>(current_query),
                state.client.rows::<OperationRow>(prev_query),
            )?;
            (current, Some(previous))
        }
        None => (state.client.rows::<OperationRow>(current_query).await?, None),
    };
    let mut stats = current.stats;
    let secs = (range.span_ms() as f64 / 1000.0).max(1.0);
    let prev_secs = prev_range.as_ref().map_or(1.0, |r| (r.span_ms() as f64 / 1000.0).max(1.0));
    type OpKey = (String, String, String);
    let mut prev_by_op: std::collections::HashMap<OpKey, PrevOp> = match previous {
        Some(previous) => {
            stats.absorb(&previous.stats);
            previous
                .rows
                .into_iter()
                .map(|r| {
                    let stat = PrevOp {
                        requests: r.requests,
                        errors: r.errors,
                        error_rate: if r.requests > 0 {
                            r.errors as f64 / r.requests as f64
                        } else {
                            0.0
                        },
                        rps: r.requests as f64 / prev_secs,
                        p50_ms: pct(&r.q, 0),
                        p95_ms: pct(&r.q, 1),
                        p99_ms: pct(&r.q, 2),
                    };
                    ((r.service_name, r.span_name, r.span_kind), stat)
                })
                .collect()
        }
        None => std::collections::HashMap::new(),
    };
    let mut rows: Vec<OperationStat> = current
        .rows
        .into_iter()
        .map(|r| OperationStat {
            prev: prev_by_op.remove(&(
                r.service_name.clone(),
                r.span_name.clone(),
                r.span_kind.clone(),
            )),
            service: r.service_name,
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
    // 对比窗口有、现在一次都没有的接口：整个接口不见了也是一种「哪些请求变了」，而且是最该被
    // 看见的一种。补成 0 次的一行接在后面（按对比窗口的量排，输出才稳定）
    let mut gone: Vec<(OpKey, PrevOp)> = prev_by_op.into_iter().collect();
    gone.sort_by(|a, b| b.1.requests.cmp(&a.1.requests).then_with(|| a.0.cmp(&b.0)));
    rows.extend(gone.into_iter().map(|((service, span_name, kind), prev)| OperationStat {
        service,
        span_name,
        kind,
        requests: 0,
        errors: 0,
        error_rate: 0.0,
        rps: 0.0,
        p50_ms: 0.0,
        p95_ms: 0.0,
        p99_ms: 0.0,
        max_ms: 0.0,
        prev: Some(prev),
    }));
    Ok(Operations { rows, stats, compare, prev_range })
}

/// 一个服务的接口表。服务级的「比昨天慢了 3 倍」只说明有事，**是哪个接口**才是能动手的信息。
async fn operations(
    State(state): State<AppState>,
    Path(name): Path<String>,
    p: Params,
) -> Result<Json<OperationsResponse>> {
    let schema = state.schema.get().await?;
    let range = range(&state, &p)?;
    let (kind, kinds) = parse_kinds(&p)?;
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let out = collect_operations(&state, &queries, &p, &range, &[name.as_str()], kinds).await?;
    Ok(Json(OperationsResponse {
        service: name,
        kind: kind.to_owned(),
        from_ms: range.from_ms,
        to_ms: range.to_ms,
        compare: out.compare,
        prev_from_ms: out.prev_range.as_ref().map(|r| r.from_ms),
        prev_to_ms: out.prev_range.as_ref().map(|r| r.to_ms),
        operations: out.rows,
        stats: out.stats,
    }))
}

/// 好几个服务的接口表，一条查询出来，每行带 `service`。
///
/// 总览页每张异常卡上那句「主要是哪个接口」用的就是它。以前是一张卡各查一次：现在只有三个
/// 服务不健康所以看不出来，但一到故障、十几个服务同时报警就是十几条查询——而那正是最需要
/// 这一页的时候。同一页上的「头号报错」和「进程重启」早就是一条查全站再按服务分了，这是
/// 漏掉的那个。
async fn operations_many(
    State(state): State<AppState>,
    p: Params,
) -> Result<Json<OperationsResponse>> {
    let schema = state.schema.get().await?;
    let range = range(&state, &p)?;
    let (kind, kinds) = parse_kinds(&p)?;
    let services = p.get_list("service");
    if services.is_empty() {
        return Err(Error::bad_request("至少给一个 service"));
    }
    if services.len() > MAX_SERVICES_PER_QUERY {
        return Err(Error::bad_request(format!("一次最多问 {MAX_SERVICES_PER_QUERY} 个服务")));
    }
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let names: Vec<&str> = services.iter().map(String::as_str).collect();
    let out = collect_operations(&state, &queries, &p, &range, &names, kinds).await?;
    Ok(Json(OperationsResponse {
        service: services.join(","),
        kind: kind.to_owned(),
        from_ms: range.from_ms,
        to_ms: range.to_ms,
        compare: out.compare,
        prev_from_ms: out.prev_range.as_ref().map(|r| r.from_ms),
        prev_to_ms: out.prev_range.as_ref().map(|r| r.to_ms),
        operations: out.rows,
        stats: out.stats,
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
    /// 对比窗口怎么取，见 [`compare_window_opt`]。`none` 表示没查，每个点的 `prev` 都是 null
    pub compare: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_from_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_to_ms: Option<i64>,
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
    /// 对比窗口里相对位置相同的那一格。那一格一个请求都没有就是 null——画成 0 的话延迟曲线
    /// 会被拽到地板上，和「那时候没有流量」分不开
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev: Option<PrevPoint>,
}

#[derive(Serialize, Clone, Copy)]
pub struct PrevPoint {
    pub requests: u64,
    pub errors: u64,
    pub p95_ms: f64,
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
    let (compare, prev_range) = compare_window_opt(p.get("compare").unwrap_or("day"), &range)?;
    let current_query = queries.timeseries(&range, &name, span_name.as_deref(), &bucket)?;
    // 对比窗口的桶按同样的宽度、从它自己的起点切，这样两边第 i 格对应同一个相对时刻
    let prev_bucket = prev_range.as_ref().map(|prev| Bucket {
        width_ms: bucket.width_ms,
        origin_ms: bucket.origin_ms - (range.from_ms - prev.from_ms),
    });
    let (result, previous) = match (&prev_range, &prev_bucket) {
        (Some(prev), Some(prev_bucket)) => {
            let prev_query = queries.timeseries(prev, &name, span_name.as_deref(), prev_bucket)?;
            let (current, previous) = tokio::try_join!(
                state.client.rows::<TimeseriesRow>(current_query),
                state.client.rows::<TimeseriesRow>(prev_query),
            )?;
            (current, Some(previous))
        }
        _ => (state.client.rows::<TimeseriesRow>(current_query).await?, None),
    };
    let mut stats = result.stats;
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
            prev: None,
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
    if let (Some(previous), Some(prev_range), Some(prev_bucket)) =
        (previous, prev_range.as_ref(), prev_bucket.as_ref())
    {
        stats.absorb(&previous.stats);
        let prev_first = prev_bucket.first_index(prev_range);
        for row in previous.rows {
            let idx = row.bucket - prev_first;
            if idx < 0 || idx as usize >= points.len() {
                continue;
            }
            points[idx as usize].prev = Some(PrevPoint {
                requests: row.requests,
                errors: row.errors,
                p95_ms: pct(&row.q, 1),
            });
        }
    }
    Ok(Json(TimeseriesResponse {
        service: name,
        span_name,
        width_ms: bucket.width_ms,
        from_ms: range.from_ms,
        to_ms: range.to_ms,
        compare,
        prev_from_ms: prev_range.as_ref().map(|r| r.from_ms),
        prev_to_ms: prev_range.as_ref().map(|r| r.to_ms),
        points,
        stats,
    }))
}
