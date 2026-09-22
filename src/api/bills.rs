//! `/api/bills/*`：费用总览、排行、按天趋势、明细和导出。
//!
//! 数据来自 goscan 写入的三张账单表。与日志 / 链路 / 指标三页最大的不同在于**时间粒度是账期**
//! （`YYYY-MM`），而非 unix 毫秒——顶栏的时间范围选择器在本页派不上用场（账单最近的一条也
//! 可能是昨日出具，按「最近 1 小时」查询一无所获），因此这些接口只接受 `from` / `to` 两个账期。
//!
//! 一次请求通常要查两张表（火山一张、阿里云一张），并发发出后在此合并：两朵云的列名口径
//! 不同，统一为同一套维度的工作在 [`crate::query::bills`] 中完成，此处只负责把几份结果按
//! 账期 / 维度值相加。
//!
//! 账单表是可选的：未部署 goscan 时这几个接口一律返回 400 并说明原因，`/api/meta` 中 `bills`
//! 为 null，前端据此不显示费用页。

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use super::{AppState, params::Params};
use crate::clickhouse::Stats;
use crate::error::{Error, Result};
use crate::goscan::{Goscan, SyncRequest};
use crate::query::bills::{
    Amount, BillFilter, BillQueries, BucketRow, DEFAULT_PERIODS, DetailRow, Dimension, KeyRow,
    Kind, MAX_DETAIL_ROWS, MAX_PERIODS, PeriodRange, Provider, TotalRow, shift_period,
};
use crate::query::parse_tz;
use crate::schema::{BillTable, BillTables, Schema};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/bills/periods", get(periods))
        .route("/api/bills/summary", get(summary))
        .route("/api/bills/daily", get(daily))
        .route("/api/bills/breakdown", get(breakdown))
        .route("/api/bills/detail", get(detail))
        .route("/api/bills/export", get(export))
        .route("/api/bills/sync", post(sync))
        .route("/api/bills/sync/{task_id}", get(sync_task))
}

/// 排行一次最多返回多少项。
const MAX_BREAKDOWN: u32 = 200;
const DEFAULT_BREAKDOWN: u32 = 20;

/// 查询串里这些键是控制参数，其余的当维度筛选（`product=云服务器`）解释。
const CONTROL_KEYS: &[&str] =
    &["from", "to", "amount", "provider", "granularity", "by", "limit", "offset", "format", "q"];

/// 账单表在不在。不在就把原因原样告诉前端。
async fn bills(state: &AppState) -> Result<Arc<Schema>> {
    let schema = state.schema.get().await?;
    if schema.bills.is_none() {
        let note = schema.bills_note.clone().unwrap_or_else(|| "账单表不可用".to_owned());
        return Err(Error::bad_request(format!("费用页未启用：{note}")));
    }
    Ok(schema)
}

/// 按 `--timezone` 算出「今天属于哪个账期」。不给 `from` / `to` 时的默认区间从这里起算。
fn current_period(state: &AppState) -> Result<String> {
    let tz = parse_tz(&state.config.timezone)?;
    let now = chrono::DateTime::from_timestamp_millis(state.now_ms())
        .ok_or_else(|| Error::internal("当前时间超出范围"))?;
    Ok(now.with_timezone(&tz).format("%Y-%m").to_string())
}

/// 这次要问哪几张表。
///
/// 阿里云有月度和日度两张，内容是同一批账单的两种粒度，**加在一起就是翻倍**：按账期看时用
/// 月度那张（没有才退到日度），按天看时只能用日度那张。
fn sources<'a>(
    tables: &'a BillTables,
    providers: &[Provider],
    by_day: bool,
) -> Vec<(Kind, &'a BillTable)> {
    let want = |p: Provider| providers.is_empty() || providers.contains(&p);
    let mut out: Vec<(Kind, &BillTable)> = Vec::new();
    if want(Provider::Volcengine)
        && let Some(t) = &tables.volcengine
    {
        out.push((Kind::Volcengine, t));
    }
    if want(Provider::Alicloud) {
        let ali = if by_day {
            tables.alicloud_daily.as_ref().map(|t| (Kind::AlicloudDaily, t))
        } else {
            tables
                .alicloud_monthly
                .as_ref()
                .map(|t| (Kind::AlicloudMonthly, t))
                .or_else(|| tables.alicloud_daily.as_ref().map(|t| (Kind::AlicloudDaily, t)))
        };
        out.extend(ali);
    }
    out
}

fn queries<'a>(state: &'a AppState, kind: Kind, table: &'a BillTable) -> BillQueries<'a> {
    BillQueries { database: &state.config.database, kind, table, dedupe: state.config.bill_dedupe }
}

/// 账期区间：没给就是最近 [`DEFAULT_PERIODS`] 个。
fn range(state: &AppState, p: &Params) -> Result<PeriodRange> {
    PeriodRange::new(p.get("from"), p.get("to"), &current_period(state)?)
}

/// `provider=volcengine,alicloud`；不给 = 两朵云都要。
fn providers(p: &Params) -> Result<Vec<Provider>> {
    p.get_list("provider").iter().map(|v| Provider::parse(v)).collect()
}

/// 账期区间 + 维度筛选 + 模糊搜。
fn filter(state: &AppState, p: &Params) -> Result<BillFilter> {
    let mut f = BillFilter::new(range(state, p)?);
    for key in p.keys() {
        if CONTROL_KEYS.contains(&key) {
            continue;
        }
        let dim = Dimension::parse(key)?;
        let values = p.get_list(key);
        if !values.is_empty() {
            f.dims.push((dim, values));
        }
    }
    f.q = p.get("q").map(str::to_owned);
    Ok(f)
}

/// 几条并发查询的统计合一份：读量相加，耗时取最慢的那条（它们是并行跑的）。
fn merge_stats(all: &[Stats]) -> Stats {
    Stats {
        read_rows: all.iter().map(|s| s.read_rows).sum(),
        read_bytes: all.iter().map(|s| s.read_bytes).sum(),
        result_rows: all.iter().map(|s| s.result_rows).sum(),
        elapsed_ms: all.iter().map(|s| s.elapsed_ms).fold(0.0, f64::max),
    }
}

/// 金额按分位对齐：ClickHouse 出来的是 Float64，直接相加会积出 0.30000000000000004 这种尾巴。
/// 账单最小单位是分，这里统一到小数点后 4 位（火山有按 0.0001 计价的计费项）。
///
/// 顺手把负零normalize 掉：Rust 的 `Iterator::sum::<f64>()` 以 `-0.0` 为单位元，一个空账期
/// 求和出来就是负零，JSON 里原样写成 `-0.0`，页面上显示「-0.00 元」。
fn round(v: f64) -> f64 {
    let r = (v * 10_000.0).round() / 10_000.0;
    if r == 0.0 { 0.0 } else { r }
}

#[derive(Serialize)]
pub struct PeriodsResponse {
    /// 库里真有数据的账期，从早到晚
    pub periods: Vec<String>,
    /// 最近一个有数据的账期；一条账单都没有时为 null
    pub latest: Option<String>,
    /// 哪几朵云的表能查
    pub providers: Vec<&'static str>,
    pub stats: Stats,
}

/// 有哪些账期。页面一进来先问这个：账单是「昨天出昨天的、月初出上个月的」，默认时间范围
/// 得跟着数据走，而不是跟着今天走。
async fn periods(State(state): State<AppState>, p: Params) -> Result<Json<PeriodsResponse>> {
    let schema = bills(&state).await?;
    let tables = schema.bills.as_ref().expect("bills checked");
    // 往前看满一次查询的上限，够页面把「有数据的那几个月」都列出来
    let current = current_period(&state)?;
    let range = PeriodRange {
        from: shift_period(&current, -(MAX_PERIODS as i32 - 1))?,
        to: current.clone(),
    };
    let sources = sources(tables, &providers(&p)?, false);
    let results = futures_util::future::join_all(sources.iter().map(|(kind, table)| {
        state.client.rows::<BucketRow>(queries(&state, *kind, table).periods(&range))
    }))
    .await;

    let mut periods: Vec<String> = Vec::new();
    let mut stats = Vec::new();
    for r in results {
        let r = r?;
        stats.push(r.stats);
        for row in r.rows {
            if !row.period.is_empty() && !periods.contains(&row.period) {
                periods.push(row.period);
            }
        }
    }
    periods.sort();
    Ok(Json(PeriodsResponse {
        latest: periods.last().cloned(),
        providers: sources.iter().map(|(k, _)| k.provider().as_str()).collect(),
        periods,
        stats: merge_stats(&stats),
    }))
}

#[derive(Serialize)]
pub struct Point {
    /// 账期 `YYYY-MM`，或按天时的 `YYYY-MM-DD`
    pub t: String,
    pub total: f64,
    /// 各云在这个点上的金额，没有的云不出现
    pub by_provider: BTreeMap<&'static str, f64>,
}

#[derive(Serialize)]
pub struct SummaryResponse {
    pub from: String,
    pub to: String,
    pub amount: Amount,
    /// 请求的账期一个不少，没数据的那个月是 0——中间断掉的月份在图上要看得出来
    pub points: Vec<Point>,
    pub total: f64,
    pub by_provider: BTreeMap<&'static str, f64>,
    pub providers: Vec<&'static str>,
    pub stats: Stats,
}

/// 按账期的总额，分云。费用页顶上那张柱状图和几个大数就是它。
async fn summary(State(state): State<AppState>, p: Params) -> Result<Json<SummaryResponse>> {
    let schema = bills(&state).await?;
    let tables = schema.bills.as_ref().expect("bills checked");
    let amount = Amount::parse(p.get("amount"))?;
    let filter = filter(&state, &p)?;
    let sources = sources(tables, &providers(&p)?, false);
    if sources.is_empty() {
        return Err(Error::bad_request("当前部署没有这些云的账单表"));
    }
    let results = futures_util::future::join_all(sources.iter().map(|(kind, table)| {
        let q = queries(&state, *kind, table).by_period(&filter, amount);
        let client = state.client.clone();
        async move { client.rows::<BucketRow>(q?).await }
    }))
    .await;

    let mut by_period: BTreeMap<String, BTreeMap<&'static str, f64>> = BTreeMap::new();
    let mut stats = Vec::new();
    for ((kind, _), r) in sources.iter().zip(results) {
        let r = r?;
        stats.push(r.stats);
        for row in r.rows {
            *by_period
                .entry(row.period)
                .or_default()
                .entry(kind.provider().as_str())
                .or_default() += row.amount;
        }
    }
    let mut totals: BTreeMap<&'static str, f64> = BTreeMap::new();
    let points: Vec<Point> = filter
        .range
        .periods()
        .into_iter()
        .map(|period| {
            let by_provider = by_period.remove(&period).unwrap_or_default();
            for (provider, amount) in &by_provider {
                *totals.entry(provider).or_default() += *amount;
            }
            Point {
                t: period,
                total: round(by_provider.values().sum()),
                by_provider: by_provider.into_iter().map(|(k, v)| (k, round(v))).collect(),
            }
        })
        .collect();
    Ok(Json(SummaryResponse {
        from: filter.range.from.clone(),
        to: filter.range.to.clone(),
        amount,
        total: round(totals.values().sum()),
        by_provider: totals.into_iter().map(|(k, v)| (k, round(v))).collect(),
        providers: sources.iter().map(|(k, _)| k.provider().as_str()).collect(),
        points,
        stats: merge_stats(&stats),
    }))
}

#[derive(Serialize)]
pub struct DailyResponse {
    pub from: String,
    pub to: String,
    pub amount: Amount,
    /// 只有真有数据的那几天，按日期排
    pub points: Vec<Point>,
    pub total: f64,
    /// 哪几朵云能按天看。阿里云要同步了日度账单才有；火山的明细自带费用日期
    pub providers: Vec<&'static str>,
    pub stats: Stats,
}

/// 按天的花费。月度账单没有日期，所以这一页只问「有日粒度」的那几张表。
async fn daily(State(state): State<AppState>, p: Params) -> Result<Json<DailyResponse>> {
    let schema = bills(&state).await?;
    let tables = schema.bills.as_ref().expect("bills checked");
    let amount = Amount::parse(p.get("amount"))?;
    let filter = filter(&state, &p)?;
    let sources = sources(tables, &providers(&p)?, true);
    if sources.is_empty() {
        return Err(Error::bad_request(
            "没有可按天查看的账单表：阿里云需同步日度账单（granularity=daily），火山引擎的明细表自带费用日期",
        ));
    }
    let results = futures_util::future::join_all(sources.iter().map(|(kind, table)| {
        let q = queries(&state, *kind, table).by_day(&filter, amount);
        let client = state.client.clone();
        async move { client.rows::<BucketRow>(q?).await }
    }))
    .await;

    let mut by_day: BTreeMap<String, BTreeMap<&'static str, f64>> = BTreeMap::new();
    let mut stats = Vec::new();
    for ((kind, _), r) in sources.iter().zip(results) {
        let r = r?;
        stats.push(r.stats);
        for row in r.rows {
            *by_day.entry(row.period).or_default().entry(kind.provider().as_str()).or_default() +=
                row.amount;
        }
    }
    let points: Vec<Point> = by_day
        .into_iter()
        .map(|(day, by_provider)| Point {
            t: day,
            total: round(by_provider.values().sum()),
            by_provider: by_provider.into_iter().map(|(k, v)| (k, round(v))).collect(),
        })
        .collect();
    Ok(Json(DailyResponse {
        from: filter.range.from.clone(),
        to: filter.range.to.clone(),
        amount,
        total: round(points.iter().map(|p| p.total).sum()),
        providers: sources.iter().map(|(k, _)| k.provider().as_str()).collect(),
        points,
        stats: merge_stats(&stats),
    }))
}

#[derive(Serialize)]
pub struct BreakdownRow {
    pub key: String,
    pub amount: f64,
    /// 占总额的比例，0~1
    pub share: f64,
    pub by_provider: BTreeMap<&'static str, f64>,
}

#[derive(Serialize)]
pub struct BreakdownResponse {
    pub by: Dimension,
    pub label: &'static str,
    pub from: String,
    pub to: String,
    pub amount: Amount,
    pub rows: Vec<BreakdownRow>,
    /// 排行之外的那些加起来多少（总额 - 列出来的）
    pub other: f64,
    pub total: f64,
    pub stats: Stats,
}

/// 按某个维度排行：产品、地域、账号、实例、项目……跨云合在一起排。
async fn breakdown(State(state): State<AppState>, p: Params) -> Result<Json<BreakdownResponse>> {
    let schema = bills(&state).await?;
    let tables = schema.bills.as_ref().expect("bills checked");
    let amount = Amount::parse(p.get("amount"))?;
    let by = Dimension::parse(p.get("by").unwrap_or("product"))?;
    let limit = p.get_limit("limit", DEFAULT_BREAKDOWN, MAX_BREAKDOWN)?;
    let filter = filter(&state, &p)?;
    let sources = sources(tables, &providers(&p)?, false);
    if sources.is_empty() {
        return Err(Error::bad_request("当前部署没有这些云的账单表"));
    }

    // 排行和总额分两条查：把上万个分类都传回来只为了算一个「其它」不值当。
    // 每张表要的那两条并发发，一张表慢不拖住另一张
    let results = futures_util::future::join_all(sources.iter().map(|(kind, table)| {
        let q = queries(&state, *kind, table);
        let top = q.breakdown(&filter, amount, by, limit);
        let total = q.total(&filter, amount);
        let client = state.client.clone();
        async move {
            let (top, total) =
                tokio::join!(client.rows::<KeyRow>(top?), client.rows::<TotalRow>(total?));
            Ok::<_, Error>((top?, total?))
        }
    }))
    .await;

    let mut merged: BTreeMap<String, BTreeMap<&'static str, f64>> = BTreeMap::new();
    let mut total = 0.0;
    let mut stats = Vec::new();
    for ((kind, _), r) in sources.iter().zip(results) {
        let (top, sum) = r?;
        stats.push(top.stats);
        stats.push(sum.stats);
        for row in top.rows {
            let key = if row.key.is_empty() { "（空）".to_owned() } else { row.key };
            *merged.entry(key).or_default().entry(kind.provider().as_str()).or_default() +=
                row.amount;
        }
        total += sum.rows.first().and_then(|r| r.amount).unwrap_or(0.0);
    }
    let mut rows: Vec<BreakdownRow> = merged
        .into_iter()
        .map(|(key, by_provider)| {
            let amount: f64 = by_provider.values().sum();
            BreakdownRow {
                key,
                amount: round(amount),
                share: if total > 0.0 { amount / total } else { 0.0 },
                by_provider: by_provider.into_iter().map(|(k, v)| (k, round(v))).collect(),
            }
        })
        .collect();
    rows.sort_by(|a, b| b.amount.total_cmp(&a.amount).then_with(|| a.key.cmp(&b.key)));
    rows.truncate(limit as usize);
    let listed: f64 = rows.iter().map(|r| r.amount).sum();
    Ok(Json(BreakdownResponse {
        by,
        label: by.label(),
        from: filter.range.from.clone(),
        to: filter.range.to.clone(),
        amount,
        other: round((total - listed).max(0.0)),
        total: round(total),
        rows,
        stats: merge_stats(&stats),
    }))
}

#[derive(Serialize)]
pub struct DetailResponse {
    /// 这一页是哪朵云的。两朵云的明细列不一样，一页只看一朵
    pub provider: &'static str,
    /// `monthly` / `daily`；火山的明细本来就是按天的，这里是 `daily`
    pub granularity: &'static str,
    pub from: String,
    pub to: String,
    pub amount: Amount,
    pub rows: Vec<DetailRow>,
    /// 去重之后一共多少行
    pub total: Option<u64>,
    pub limit: u32,
    pub offset: u32,
    pub stats: Stats,
}

/// 选定出明细的那张表。未指定时按可用性择一，前端将其回显在切换按钮上。
fn detail_source<'a>(tables: &'a BillTables, p: &Params) -> Result<(Kind, &'a BillTable)> {
    let provider = match p.get("provider") {
        Some(raw) => Some(Provider::parse(raw)?),
        None => None,
    };
    let daily = match p.get("granularity") {
        None => false,
        Some("daily") => true,
        Some("monthly") => false,
        Some(other) => {
            return Err(Error::bad_request(format!(
                "granularity 只能是 monthly 或 daily，不是 {other:?}"
            )));
        }
    };
    let list = sources(tables, &provider.into_iter().collect::<Vec<_>>(), daily);
    list.into_iter().next().ok_or_else(|| {
        Error::bad_request(match provider {
            Some(p) => format!("{}（{}）没有对应的账单表", p.label(), p.as_str()),
            None => "当前部署没有任何账单表".to_owned(),
        })
    })
}

fn granularity_of(kind: Kind) -> &'static str {
    match kind {
        Kind::AlicloudMonthly => "monthly",
        _ => "daily",
    }
}

async fn detail(State(state): State<AppState>, p: Params) -> Result<Json<DetailResponse>> {
    let schema = bills(&state).await?;
    let tables = schema.bills.as_ref().expect("bills checked");
    let amount = Amount::parse(p.get("amount"))?;
    let filter = filter(&state, &p)?;
    let (kind, table) = detail_source(tables, &p)?;
    let limit = p.get_limit("limit", 100, MAX_DETAIL_ROWS.min(state.config.max_rows))?;
    let offset = p.get_u32("offset")?.unwrap_or(0);
    if offset > state.config.max_offset {
        return Err(Error::bad_request(format!(
            "最多翻至第 {} 条；如需继续，请缩小账期范围或增加筛选条件",
            state.config.max_offset
        )));
    }
    let q = queries(&state, kind, table);
    let (rows, count) = tokio::join!(
        state.client.rows::<DetailRow>(q.detail(&filter, amount, limit, offset)?),
        state.client.rows::<TotalRow>(q.detail_count(&filter)?),
    );
    let rows = rows?;
    // 总数只是个参考，失败了不能把结果也拖死
    let total = match count {
        Ok(c) => c.rows.first().and_then(|r| r.rows),
        Err(e) => {
            tracing::warn!(error = %e, "账单明细的计数查询失败，只回这一页");
            None
        }
    };
    let provider = kind.provider().as_str();
    Ok(Json(DetailResponse {
        provider,
        granularity: granularity_of(kind),
        from: filter.range.from.clone(),
        to: filter.range.to.clone(),
        amount,
        rows: rows
            .rows
            .into_iter()
            .map(|mut r| {
                r.provider = provider.to_owned();
                r.amount = round(r.amount);
                r.original = round(r.original);
                r.paid = round(r.paid);
                r
            })
            .collect(),
        total,
        limit,
        offset,
        stats: rows.stats,
    }))
}

/// 导出明细 CSV / JSONL：ClickHouse 直接出格式化文本，这里只转发字节流。
async fn export(State(state): State<AppState>, p: Params) -> Result<Response> {
    let schema = bills(&state).await?;
    let tables = schema.bills.as_ref().expect("bills checked");
    let amount = Amount::parse(p.get("amount"))?;
    let filter = filter(&state, &p)?;
    let (kind, table) = detail_source(tables, &p)?;
    let limit = p.get_limit("limit", state.config.export_max_rows, state.config.export_max_rows)?;
    let (format, content_type, ext) = match p.get("format").unwrap_or("csv") {
        "csv" => ("CSVWithNames", "text/csv; charset=utf-8", "csv"),
        "jsonl" | "json" => ("JSONEachRow", "application/x-ndjson; charset=utf-8", "jsonl"),
        other => {
            return Err(Error::bad_request(format!("format 只能是 csv 或 jsonl，不是 {other:?}")));
        }
    };
    let query = queries(&state, kind, table).export(&filter, amount, limit)?;
    let resp = state.client.send(&query, Some(format)).await?;
    let name = format!(
        "bills-{}-{}-{}.{ext}",
        kind.provider().as_str(),
        filter.range.from,
        filter.range.to
    );
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

/// 给 MCP 用：把「最近 N 个账期」翻成区间，省得工具那边自己算月份。
pub fn recent_range(current: &str, months: u32) -> Result<PeriodRange> {
    let months = months.clamp(1, MAX_PERIODS as u32) as i32;
    PeriodRange::new(Some(&shift_period(current, -(months - 1))?), Some(current), current)
}

/// 默认看几个账期，给 MCP 的工具描述用。
pub const DEFAULT_RANGE_MONTHS: usize = DEFAULT_PERIODS;

// ---------------------------------------------------------------------------------------------
// 手动拉一次账单（转给 goscan）
// ---------------------------------------------------------------------------------------------

/// 同步任务的模式。`standard` 老老实实按账期拉，`sync-optimal` 按数据量比对只补差的那部分。
/// 手动触发的意图是「把这几个月补齐」，因此默认 standard。
const SYNC_MODES: &[&str] = &["standard", "sync-optimal"];
/// 阿里云的粒度。火山的明细本身带有费用日期，无此选项
const GRANULARITIES: &[&str] = &["monthly", "daily", "both"];

fn goscan(state: &AppState) -> Result<&Goscan> {
    state.goscan.as_deref().ok_or_else(|| {
        Error::bad_request(
            "当前部署未配置 --goscan-url（OPDASH_GOSCAN_URL），无法拉取账单：\
             账单由 goscan 向云厂商拉取，opdash 仅负责转发该指令",
        )
    })
}

/// `POST /api/bills/sync` 的请求体。
#[derive(Debug, Deserialize)]
pub struct SyncBody {
    /// `volcengine` / `alicloud`
    pub provider: String,
    /// 起止账期 `YYYY-MM`；不给就是最近 [`DEFAULT_PERIODS`] 个（和页面上的默认区间一个口径）
    pub from: Option<String>,
    pub to: Option<String>,
    /// 阿里云专用：`monthly` / `daily` / `both`（默认 both，两种粒度一起拉）
    pub granularity: Option<String>,
    /// 已经有数据的账期也重新拉一遍
    #[serde(default)]
    pub force: bool,
    /// `standard`（默认）/ `sync-optimal`
    pub mode: Option<String>,
}

#[derive(Serialize)]
pub struct SyncStartedResponse {
    /// 拿它去轮 `/api/bills/sync/{task_id}`
    pub task_id: String,
    pub provider: &'static str,
    pub from: String,
    pub to: String,
    /// goscan 说的那句话，原样带上
    pub message: String,
}

/// 手动拉一次账单：把动作转给 goscan，拿回一个 task id。
///
/// **这是 opdash 唯一一个会引起外部状态变化的接口**，因此它位于认证之内（与 `/api/*` 的其余部分
/// 一致），而 goscan 自身的 HTTP 接口并无认证。触发调用很快返回，账单需等 goscan 后台拉取完成
/// 后才会入库——按 task id 轮询 [`sync_task`] 查看结果。
async fn sync(
    State(state): State<AppState>,
    Json(body): Json<SyncBody>,
) -> Result<Json<SyncStartedResponse>> {
    let client = goscan(&state)?;
    let provider = Provider::parse(&body.provider)?;
    let range =
        PeriodRange::new(body.from.as_deref(), body.to.as_deref(), &current_period(&state)?)?;
    let mode = body.mode.as_deref().unwrap_or("standard");
    if !SYNC_MODES.contains(&mode) {
        return Err(Error::bad_request(format!(
            "mode 只能是 {}，不是 {mode:?}",
            SYNC_MODES.join(" / ")
        )));
    }
    // 粒度只对阿里云有意义：火山那张明细表本来就是按天的
    let granularity = match (provider, body.granularity.as_deref()) {
        (Provider::Volcengine, _) => None,
        (Provider::Alicloud, None) => Some("both".to_owned()),
        (Provider::Alicloud, Some(g)) if GRANULARITIES.contains(&g) => Some(g.to_owned()),
        (Provider::Alicloud, Some(g)) => {
            return Err(Error::bad_request(format!(
                "granularity 只能是 {}，不是 {g:?}",
                GRANULARITIES.join(" / ")
            )));
        }
    };
    let req = SyncRequest {
        provider: provider.as_str().to_owned(),
        sync_mode: mode.to_owned(),
        granularity,
        start_period: Some(range.from.clone()),
        end_period: Some(range.to.clone()),
        force_update: body.force,
    };
    tracing::info!(
        provider = provider.as_str(),
        from = %range.from,
        to = %range.to,
        force = body.force,
        mode,
        goscan = client.base(),
        "手动触发账单同步"
    );
    let started = client.trigger(&req).await?;
    Ok(Json(SyncStartedResponse {
        task_id: started.task_id,
        provider: provider.as_str(),
        from: range.from,
        to: range.to,
        message: started.message,
    }))
}

#[derive(Serialize)]
pub struct SyncTaskResponse {
    pub id: String,
    /// `pending` / `running` / `completed` / `failed` / `cancelled`
    pub status: String,
    pub provider: String,
    /// 跑完了没有（不管成没成）
    pub done: bool,
    /// 跑完且成功
    pub ok: bool,
    /// 写进库的条数
    pub records: i64,
    /// 从云厂商那儿取回来的条数
    pub fetched: i64,
    pub message: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
}

/// 查询某个同步任务的当前状态。页面据此轮询，`done` 之后即刷新账单查询的缓存。
async fn sync_task(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
) -> Result<Json<SyncTaskResponse>> {
    let client = goscan(&state)?;
    let task = client.task(&task_id).await?;
    let done = matches!(task.status.as_str(), "completed" | "failed" | "cancelled");
    let result = task.result;
    Ok(Json(SyncTaskResponse {
        ok: task.status == "completed" && result.as_ref().is_none_or(|r| r.success),
        done,
        records: result.as_ref().map_or(0, |r| r.records_processed),
        fetched: result.as_ref().map_or(0, |r| r.records_fetched),
        message: result.as_ref().map(|r| r.message.clone()).unwrap_or_default(),
        error: if task.error.is_empty() {
            result.as_ref().map(|r| r.error.clone()).unwrap_or_default()
        } else {
            task.error
        },
        id: task.id,
        status: task.status,
        provider: task.provider,
        started_at: task.start_time,
        ended_at: task.end_time,
    }))
}
