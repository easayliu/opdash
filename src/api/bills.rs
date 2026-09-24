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

use std::collections::{BTreeMap, BTreeSet};
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
use crate::alloc::Alloc;
use crate::clickhouse::Stats;
use crate::error::{Error, Result};
use crate::goscan::{Goscan, SyncRequest, TaskRow};
use crate::query::bills::{
    AllocDetailRow, AllocPrepaidRow, AllocRow, Amount, BillFilter, BillQueries, BucketRow,
    DEFAULT_PERIODS, DetailRow, DetailSort, Dimension, KeyRow, Kind, MAX_DETAIL_ROWS, MAX_PERIODS,
    PeriodRange, PeriodRow, ProductDayRow, Provider, TotalRow, shift_period,
};
use crate::query::parse_tz;
use crate::schema::{BillTable, BillTables, Schema};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/bills/periods", get(periods))
        .route("/api/bills/summary", get(summary))
        .route("/api/bills/daily", get(daily))
        .route("/api/bills/breakdown", get(breakdown))
        .route("/api/bills/allocation", get(allocation))
        .route("/api/bills/allocation/day", get(allocation_day))
        .route("/api/bills/product-days", get(product_days))
        .route("/api/bills/detail", get(detail))
        .route("/api/bills/facets", get(facets))
        .route("/api/bills/export", get(export))
        .route("/api/bills/sync", post(sync))
        .route("/api/bills/sync/running", get(sync_running))
        .route("/api/bills/sync/{task_id}", get(sync_task).delete(cancel_sync))
        .route("/api/bills/sync/{task_id}/events", get(sync_events))
}

/// 排行一次最多返回多少项。
const MAX_BREAKDOWN: u32 = 200;
const DEFAULT_BREAKDOWN: u32 = 20;

/// 查询串里这些键是控制参数，其余的当维度筛选（`product=云服务器`）解释。
const CONTROL_KEYS: &[&str] = &[
    "from",
    "to",
    "amount",
    "provider",
    "granularity",
    "by",
    "limit",
    "offset",
    "format",
    "q",
    "days",
    "sort",
    "order",
    "dims",
    "cols",
    "day",
    "day_from",
    "day_to",
];

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
        state.client.rows::<PeriodRow>(queries(&state, *kind, table).periods(&range))
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

// ---------------------------------------------------------------------------------------------
// 成本归属（分析视图）
// ---------------------------------------------------------------------------------------------

/// 「只看最近 N 天」最多能往回看多久。再长就该直接选账期了。
const MAX_WINDOW_DAYS: u32 = 366;

/// 预付费的摊销最多往后看几个月。摊到未来的那几个月是**已经发生的购买**摊过来的，
/// 页面据此给出下个月的预估。
const AMORTIZE_FORWARD: i32 = 12;

/// 一行费用：某条业务线里的某个产品，或不分业务线时的某个产品。
#[derive(Serialize)]
pub struct AllocItem {
    pub product: String,
    /// 按哪条规则归来的；未命中任何规则时为 null
    pub rule: Option<String>,
    pub amount: f64,
    /// 日均。预付费的摊销按月计，以及没有日粒度的账单时，都是 null
    pub daily: Option<f64>,
    /// 占本次统计总额的比例，0~1
    pub share: f64,
    /// 这一行是预付费摊销过来的，不是当期实际出账
    pub prepaid: bool,
}

#[derive(Serialize)]
pub struct AllocLine {
    pub name: String,
    /// 区间合计 = 后付费实际出账 + 落在区间内的预付费摊销
    pub amount: f64,
    /// 其中后付费的部分
    pub postpaid: f64,
    /// 其中预付费摊到本区间的部分
    pub amortized: f64,
    /// 日均，只按后付费算：预付费是按月摊的，除以天数没有意义
    pub daily: Option<f64>,
    pub share: f64,
    /// 各账期的预付费摊销额，含区间之后的若干个月（已发生的购买摊过来的），页面据此算月度预估
    pub amortized_by_period: BTreeMap<String, f64>,
    /// 这条线的费用由哪些产品构成，金额从大到小
    pub items: Vec<AllocItem>,
    /// 按云厂商拆开的日均与摊销。月度拆分表切到单朵云、单种付费方式时，日均与预估要用这一段
    /// 自己的数字——合计那份把两朵云、两种付费方式揉在一起，拆不回来
    pub by_provider: BTreeMap<&'static str, ProviderPart>,
}

/// 一条业务线在某朵云上的日均与摊销。
#[derive(Serialize, Default, Clone)]
pub struct ProviderPart {
    /// 后付费日均：按这朵云自己有账单的天数折算。没有日度账单时为 null
    pub daily: Option<f64>,
    /// 这朵云的预付费摊到各账期的金额，含区间之后的若干个月
    pub amortized_by_period: BTreeMap<String, f64>,
}

/// 月度拆分表的一行：某朵云、某种付费方式下，一条业务线在各账期的金额。
///
/// 页面据此排出「业务线 × 月份」的矩阵，按云、按付费方式分段，与财务那张拆分表同一个样子。
/// 按整月统计，不受「最近 N 天」影响。
#[derive(Serialize)]
pub struct AllocMonthRow {
    pub provider: &'static str,
    /// `postpaid` 后付费（按出账月份）/ `prepaid` 预付费按服务期摊到各月的部分
    pub kind: &'static str,
    /// 业务线；未命中规则、配置里也没给去处的那部分为 null
    pub line: Option<String>,
    pub by_period: BTreeMap<String, f64>,
}

/// 一天（或一个账期）各条业务线的花费。
#[derive(Serialize)]
pub struct AllocPoint {
    pub t: String,
    pub total: f64,
    pub by_line: BTreeMap<String, f64>,
}

#[derive(Serialize)]
pub struct AllocationResponse {
    pub from: String,
    pub to: String,
    pub amount: Amount,
    /// 配了 `--bill-alloc` 才有业务线这一层；没配时 `lines` 为空，只有按产品的日均与预估
    pub configured: bool,
    /// 只统计了最近这么多天（`days` 参数）
    pub window_days: Option<u32>,
    /// 本次统计覆盖了几天的账单（取各云中最多的那个），没有日粒度的账单时为 0。日均的分母是
    /// **每朵云自己的天数**（见 `days_by_provider`），而不是自然月的天数——当月账单未出齐，
    /// 按 30 天摊只会把日均算低
    pub days: u32,
    /// 各云各有几天的账单
    pub days_by_provider: BTreeMap<&'static str, u32>,
    /// `points` 的粒度：`daily` 一天一个点，`monthly` 一个账期一个点
    pub granularity: &'static str,
    /// 配了 `[prepaid]` 才有预付费摊销这一层
    pub prepaid: bool,
    /// 区间合计 = 后付费实际出账 + 落在区间内的预付费摊销
    pub total: f64,
    /// 其中后付费的部分
    pub postpaid: f64,
    /// 其中预付费摊到本区间的部分
    pub amortized: f64,
    /// 各账期的预付费摊销额，含区间之后的若干个月，页面据此算月度预估
    pub amortized_by_period: BTreeMap<String, f64>,
    /// 日均 = 后付费合计 / `days`；`days` 为 0 时为 null。**预付费不参与**：它按月摊，
    /// 除以天数只会把两种口径搅在一起
    pub daily: Option<f64>,
    pub lines: Vec<AllocLine>,
    /// 未命中任何规则的部分。配置里给了 `unmatched` 时，这笔钱已同时计入那条业务线，
    /// 此处单列是为了让人看得见「有多少钱还没写进规则」
    pub unmatched: AllocLine,
    /// 配置里 `unmatched` 指向的业务线：非空时 `unmatched` 已计入它，页面不能再把两者相加
    pub unmatched_into: Option<String>,
    /// 不分业务线、只按产品，配没配规则都有
    pub products: Vec<AllocItem>,
    pub points: Vec<AllocPoint>,
    /// 月度拆分表的数据，见 [`AllocMonthRow`]
    pub monthly: Vec<AllocMonthRow>,
    /// 日度账单明显少于月度账单时给出两边的合计，页面据此提示「日度账单不全」；覆盖正常时为 null
    pub coverage: Option<Coverage>,
    pub stats: Stats,
}

#[derive(Serialize)]
pub struct Coverage {
    pub provider: &'static str,
    /// 所选账期内日度账单的合计
    pub daily: f64,
    /// 同一段账期月度账单的合计
    pub monthly: f64,
}

/// 业务线（或产品表）里的一组行：（产品, 规则名, 是否预付费摊销）→ 金额。
type Items = BTreeMap<(String, String, bool), Money>;

/// 命中第 `rule` 条规则的 `amount` 该分给哪几条业务线。`None` 表示没命中规则、
/// 配置里也没说未归属的去处。
fn spread(alloc: &Alloc, rule: i32, amount: f64) -> Vec<(Option<usize>, f64)> {
    match usize::try_from(rule).ok().and_then(|i| alloc.rules.get(i)) {
        Some(r) => r.split(amount).into_iter().map(|(line, v)| (Some(line), v)).collect(),
        None => vec![(alloc.unmatched, amount)],
    }
}

/// 成本归属：按规则把账单分摊到业务线，并给出日均——「这个月每天花多少、下个月大概多少」。
///
/// 与排行（[`breakdown`]）的区别在于「按什么分」：排行按账单自带的维度分，本接口按部署方给的
/// 归属规则分，一笔共用开销可以按权重摊给几条业务线。没配规则时退化为按产品的日均。
async fn allocation(State(state): State<AppState>, p: Params) -> Result<Json<AllocationResponse>> {
    let schema = bills(&state).await?;
    let tables = schema.bills.as_ref().expect("bills checked");
    let amount = Amount::parse(p.get("amount"))?;
    let filter = filter(&state, &p)?;
    let window_days = match p.get_u32("days")? {
        Some(0) => return Err(Error::bad_request("days 至少为 1")),
        Some(n) if n > MAX_WINDOW_DAYS => {
            return Err(Error::bad_request(format!("days 最多 {MAX_WINDOW_DAYS}")));
        }
        other => other,
    };
    let fallback = Alloc::default();
    let alloc = state.alloc.as_deref().unwrap_or(&fallback);

    // 分析要的是日粒度：日均、最近 N 天都得按天算。没有日度表的云退回月度，钱一分不少，
    // 只是那部分算不出日均
    let prefer_daily = !matches!(p.get("granularity"), Some("monthly"));
    let mut sources = sources(tables, &providers(&p)?, prefer_daily);
    if sources.is_empty() {
        sources = self::sources(tables, &providers(&p)?, false);
    }
    if sources.is_empty() {
        return Err(Error::bad_request("当前部署没有这些云的账单表"));
    }
    if window_days.is_some() && sources.iter().any(|(k, _)| !k.has_days()) {
        return Err(Error::bad_request(
            "按最近几日统计需要日度账单：请为阿里云同步 granularity=daily，或去掉 days 参数改按账期统计",
        ));
    }

    // 一张表一条查询（见 alloc_detail），按产品、按天、按账期都从它的结果里拆。只有「最近 N 天」
    // 时按账期那份要另查整月
    let detail = futures_util::future::join_all(sources.iter().map(|(kind, table)| {
        let q = queries(&state, *kind, table);
        let detail = q.alloc_detail(&filter, amount, alloc, window_days);
        let by_period = window_days.map(|_| q.alloc_by_period(&filter, amount, alloc));
        let client = state.client.clone();
        async move {
            let detail = client.rows::<AllocDetailRow>(detail?);
            let periods = async {
                match by_period {
                    Some(q) => Ok::<_, Error>(Some(client.rows::<AllocRow>(q?).await?)),
                    None => Ok(None),
                }
            };
            let (detail, periods) = tokio::join!(detail, periods);
            Ok::<_, Error>((detail?, periods?))
        }
    }));

    // 预付费的购买记录。与上面那条、下面的覆盖率检查三者互不依赖，一起发出：串行时三段
    // 相加（线上约 0.5 + 0.13 + 0.5 秒），并行只等最慢的那段
    let prepaid_plan = match &alloc.prepaid {
        Some(prepaid) => {
            let lookback = PeriodRange {
                from: shift_period(&filter.range.from, -(prepaid.lookback_months as i32 - 1))?,
                to: filter.range.to.clone(),
            };
            let sources: Vec<(Kind, &BillTable)> = sources
                .iter()
                .map(|(kind, table)| match (kind, &tables.alicloud_monthly) {
                    (Kind::AlicloudDaily, Some(monthly)) => (Kind::AlicloudMonthly, monthly),
                    _ => (*kind, *table),
                })
                .collect();
            Some((prepaid, lookback, sources))
        }
        None => None,
    };
    let prepaid_rows = async {
        let Some((prepaid, lookback, sources)) = &prepaid_plan else { return Vec::new() };
        futures_util::future::join_all(sources.iter().map(|(kind, table)| {
            let q = queries(&state, *kind, table)
                .alloc_prepaid(&filter, amount, alloc, prepaid, lookback);
            let client = state.client.clone();
            let provider = kind.provider().as_str();
            async move {
                match q? {
                    Some(q) => {
                        Ok::<_, Error>(Some((provider, client.rows::<AllocPrepaidRow>(q).await?)))
                    }
                    None => Ok(None),
                }
            }
        }))
        .await
    };
    let (results, prepaid_rows, coverage) =
        tokio::join!(detail, prepaid_rows, coverage(&state, tables, &sources, &filter, amount));

    // 后付费。日均**按每张表自己的天数**折算后再相加：两朵云的同步进度常常不一样（阿里云的
    // 日度账单只拉到昨天、火山的已经出到今天，或者日度表压根只补了几天），若把两边的天数取
    // 并集当分母，天数少的那朵云会被摊薄，日均随之偏低
    let mut by_product: BTreeMap<(i32, String), Money> = BTreeMap::new();
    let mut by_bucket: BTreeMap<(i32, String), f64> = BTreeMap::new();
    // 按「云厂商 + 规则」的后付费日均、按「云厂商 + 规则 + 账期」的摊销：拆分表分段页签的预估用
    let mut rule_daily: BTreeMap<(&'static str, i32), f64> = BTreeMap::new();
    let mut rule_amortized: BTreeMap<(&'static str, i32, String), f64> = BTreeMap::new();
    let mut days_by_provider: BTreeMap<&'static str, u32> = BTreeMap::new();
    // 月度拆分表的原料：（云, 付费方式, 规则, 账期）→ 金额，最后再按规则摊到业务线
    let mut monthly: BTreeMap<(&'static str, &'static str, i32, String), f64> = BTreeMap::new();
    let mut stats = Vec::new();
    for ((kind, _), r) in sources.iter().zip(results) {
        let (detail, periods) = r?;
        let provider = kind.provider().as_str();
        stats.push(detail.stats);
        match periods {
            Some(periods) => {
                stats.push(periods.stats);
                for row in periods.rows {
                    *monthly.entry((provider, "postpaid", row.rule, row.key)).or_default() +=
                        row.amount;
                }
            }
            None => {
                for row in detail.rows.iter().filter(|r| !r.period.is_empty()) {
                    *monthly
                        .entry((provider, "postpaid", row.rule, row.period.clone()))
                        .or_default() += row.amount;
                }
            }
        }
        // 空的 bucket 是落不进任何一天的那几行：计入金额，但不算一天
        let days = if kind.has_days() {
            detail
                .rows
                .iter()
                .filter(|r| !r.bucket.is_empty())
                .map(|r| r.bucket.as_str())
                .collect::<BTreeSet<_>>()
                .len() as u32
        } else {
            0
        };
        if kind.has_days() {
            *days_by_provider.entry(provider).or_default() += days;
        }
        let mut products: BTreeMap<(i32, String), f64> = BTreeMap::new();
        for row in detail.rows {
            *products.entry((row.rule, row.product)).or_default() += row.amount;
            if !row.bucket.is_empty() {
                *by_bucket.entry((row.rule, row.bucket)).or_default() += row.amount;
            }
        }
        for (key, amount) in products {
            let daily = if days > 0 { amount / f64::from(days) } else { 0.0 };
            if days > 0 {
                *rule_daily.entry((provider, key.0)).or_default() += daily;
            }
            *by_product.entry(key).or_default() += Money { amount, daily };
        }
    }
    // 有一张表没有日粒度（阿里云只同步了月度账单），整体的日均就无从谈起
    let has_days = sources.iter().all(|(k, _)| k.has_days());
    let days = days_by_provider.values().copied().max().unwrap_or(0);
    let per_day = |m: Money| (has_days && days > 0).then(|| round(m.daily));

    // 预付费：把每一笔购买按它自己的服务期摊到各月。摊到区间之后那几个月的部分也留着——
    // 那是已经发生的购买摊过来的，正是下个月预估里绕不开的一块。
    //
    // **摊销固定读月度表**：它按月摊，月度粒度正合适；更要紧的是月度表的历史最全——日度表
    // 往往只补了最近几个月，而三年期的机器要回溯到三年前的购买记录才摊得出本月那一份
    let mut amortized: BTreeMap<(i32, String, String), f64> = BTreeMap::new();
    let horizon = shift_period(&filter.range.to, AMORTIZE_FORWARD)?;
    for r in prepaid_rows {
        let Some((provider, rows)) = r? else { continue };
        stats.push(rows.stats);
        for row in rows.rows {
            // 服务期不截断：五年期的机器就摊五年。lookback_months 只管往前找多远
            let months = row.months.clamp(1, MAX_SERVICE_MONTHS);
            let per_month = row.amount / f64::from(months);
            for k in 0..months {
                let period = shift_period(&row.period, k as i32)?;
                // 服务期早已跨过本次查询的范围，或还没摊到这里，都不必留
                if period < filter.range.from || period > horizon {
                    continue;
                }
                if period <= filter.range.to {
                    *monthly.entry((provider, "prepaid", row.rule, period.clone())).or_default() +=
                        per_month;
                }
                *rule_amortized.entry((provider, row.rule, period.clone())).or_default() +=
                    per_month;
                *amortized.entry((row.rule, row.product.clone(), period)).or_default() += per_month;
            }
        }
    }

    // 只看最近 N 天时，后付费只有这 N 天的钱，摊销却是整月的——两者直接相加，区间合计和各线
    // 占比就被摊销撑大了。所以把落在区间内的摊销按天折算到同样的天数：
    // 摊销 × N / 区间的自然天数。按月的那份（amortized_by_period，用于预估）不折算
    let natural_days: u32 = filter.range.periods().iter().map(|p| days_in_period(p)).sum();
    let scale = match window_days {
        Some(n) if natural_days > 0 => (f64::from(n) / f64::from(natural_days)).min(1.0),
        _ => 1.0,
    };

    // 总额以按产品那份为准：火山偶有 ExpenseDate 为空的行，它落不进任何一天，
    // 却是实实在在的钱
    let postpaid: f64 = by_product.values().map(|m| m.amount).sum();
    // 摊到区间之内的那部分才计入本次合计；摊到之后几个月的只用于预估
    let in_range = |period: &str| period <= filter.range.to.as_str();
    let amortized_total: f64 =
        amortized.iter().filter(|((_, _, p), _)| in_range(p)).map(|(_, v)| *v * scale).sum();
    let total = postpaid + amortized_total;

    // 业务线 → 产品 → 金额。同一个产品可能由几条规则分别归来（「某几台机器」与「其余部分」），
    // 规则名一并作键，页面上才看得出这一行是怎么来的；预付费摊销单独标记，它不参与日均
    let n = alloc.lines.len();
    let mut lines: Vec<Items> = vec![BTreeMap::new(); n];
    let mut line_postpaid = vec![Money::default(); n];
    let mut line_amortized = vec![0.0; n];
    let mut line_by_period: Vec<BTreeMap<String, f64>> = vec![BTreeMap::new(); n];
    let mut amortized_by_period: BTreeMap<String, f64> = BTreeMap::new();
    let mut unmatched: Items = BTreeMap::new();
    let mut unmatched_postpaid = Money::default();
    let mut unmatched_amortized = 0.0;
    // 未归属的预付费摊到各月多少：页面给「未归属」算月度预估时要用，与业务线同一口径
    let mut unmatched_by_period: BTreeMap<String, f64> = BTreeMap::new();
    let mut products: Items = BTreeMap::new();
    let rule_of = |rule: i32| usize::try_from(rule).ok().and_then(|i| alloc.rules.get(i));
    let rule_name = |rule: i32| rule_of(rule).map(|r| r.name.clone()).unwrap_or_default();

    for ((rule, product), value) in &by_product {
        *products.entry((product.clone(), String::new(), false)).or_default() += *value;
        if rule_of(*rule).is_none() {
            *unmatched.entry((product.clone(), String::new(), false)).or_default() += *value;
            unmatched_postpaid += *value;
        }
        for (line, share) in spread(alloc, *rule, 1.0) {
            let Some(line) = line else { continue };
            let part = *value * share;
            *lines[line].entry((product.clone(), rule_name(*rule), false)).or_default() += part;
            line_postpaid[line] += part;
        }
    }
    for ((rule, product, period), value) in &amortized {
        *amortized_by_period.entry(period.clone()).or_default() += *value;
        let within = in_range(period);
        let scaled = Money { amount: *value * scale, daily: 0.0 };
        if rule_of(*rule).is_none() {
            *unmatched_by_period.entry(period.clone()).or_default() += *value;
        }
        if within {
            *products.entry((product.clone(), String::new(), true)).or_default() += scaled;
            if rule_of(*rule).is_none() {
                *unmatched.entry((product.clone(), String::new(), true)).or_default() += scaled;
                unmatched_amortized += scaled.amount;
            }
        }
        for (line, share) in spread(alloc, *rule, 1.0) {
            let Some(line) = line else { continue };
            *line_by_period[line].entry(period.clone()).or_default() += *value * share;
            if within {
                *lines[line].entry((product.clone(), rule_name(*rule), true)).or_default() +=
                    scaled * share;
                line_amortized[line] += scaled.amount * share;
            }
        }
    }

    let share_of = |v: f64| if total > 0.0 { v / total } else { 0.0 };
    let items_of = |items: Items| {
        let mut rows: Vec<AllocItem> = items
            .into_iter()
            .map(|((product, rule, prepaid), m)| AllocItem {
                product,
                rule: (!rule.is_empty()).then_some(rule),
                amount: round(m.amount),
                // 预付费是按月摊的，除以天数没有意义
                daily: if prepaid { None } else { per_day(m) },
                share: share_of(m.amount),
                prepaid,
            })
            .collect();
        rows.sort_by(|a, b| b.amount.total_cmp(&a.amount).then_with(|| a.product.cmp(&b.product)));
        rows
    };
    let round_map = |m: BTreeMap<String, f64>| -> BTreeMap<String, f64> {
        m.into_iter().map(|(k, v)| (k, round(v))).collect()
    };

    // 按云厂商的日均与摊销，同样按规则摊到业务线；未命中规则的那份另记给「未归属」
    let mut parts: Vec<BTreeMap<&'static str, ProviderPart>> = vec![BTreeMap::new(); n];
    let mut unmatched_parts: BTreeMap<&'static str, ProviderPart> = BTreeMap::new();
    for (&(provider, rule), &daily) in &rule_daily {
        if rule_of(rule).is_none() {
            *unmatched_parts.entry(provider).or_default().daily.get_or_insert(0.0) += daily;
        }
        for (line, part) in spread(alloc, rule, daily) {
            let Some(line) = line else { continue };
            *parts[line].entry(provider).or_default().daily.get_or_insert(0.0) += part;
        }
    }
    for ((provider, rule, period), &value) in &rule_amortized {
        if rule_of(*rule).is_none() {
            *unmatched_parts
                .entry(provider)
                .or_default()
                .amortized_by_period
                .entry(period.clone())
                .or_default() += value;
        }
        for (line, part) in spread(alloc, *rule, value) {
            let Some(line) = line else { continue };
            *parts[line]
                .entry(provider)
                .or_default()
                .amortized_by_period
                .entry(period.clone())
                .or_default() += part;
        }
    }
    let round_parts =
        |m: BTreeMap<&'static str, ProviderPart>| -> BTreeMap<&'static str, ProviderPart> {
            m.into_iter()
                .map(|(k, p)| {
                    let part = ProviderPart {
                        daily: p.daily.map(round),
                        amortized_by_period: round_map(p.amortized_by_period),
                    };
                    (k, part)
                })
                .collect()
        };

    let mut line_rows: Vec<AllocLine> = alloc
        .lines
        .iter()
        .zip(lines)
        .zip(line_by_period)
        .zip(parts)
        .enumerate()
        .map(|(i, (((name, items), by_period), by_provider))| {
            let sum = line_postpaid[i].amount + line_amortized[i];
            AllocLine {
                name: name.clone(),
                amount: round(sum),
                postpaid: round(line_postpaid[i].amount),
                amortized: round(line_amortized[i]),
                daily: per_day(line_postpaid[i]),
                share: share_of(sum),
                amortized_by_period: round_map(by_period),
                items: items_of(items),
                by_provider: round_parts(by_provider),
            }
        })
        .collect();
    // 空的业务线不必占一行；规则没覆盖到的月份很常见
    line_rows.retain(|l| l.amount != 0.0 || !l.items.is_empty());

    let mut points: BTreeMap<String, (f64, BTreeMap<String, f64>)> = BTreeMap::new();
    for ((rule, bucket), value) in &by_bucket {
        let point = points.entry(bucket.clone()).or_default();
        point.0 += *value;
        for (line, share) in spread(alloc, *rule, *value) {
            if let Some(line) = line {
                *point.1.entry(alloc.lines[line].clone()).or_default() += share;
            }
        }
    }

    // 月度拆分表：按规则摊到业务线，同一（云, 付费方式, 业务线）合成一行
    let mut month_rows: BTreeMap<
        (&'static str, &'static str, Option<usize>),
        BTreeMap<String, f64>,
    > = BTreeMap::new();
    for ((provider, kind, rule, period), value) in monthly {
        for (line, part) in spread(alloc, rule, value) {
            *month_rows
                .entry((provider, kind, line))
                .or_default()
                .entry(period.clone())
                .or_default() += part;
        }
    }
    let monthly: Vec<AllocMonthRow> = month_rows
        .into_iter()
        .map(|((provider, kind, line), by_period)| AllocMonthRow {
            provider,
            kind,
            line: line.map(|i| alloc.lines[i].clone()),
            by_period: round_map(by_period),
        })
        .collect();

    let (coverage, coverage_stats) = coverage?;
    stats.extend(coverage_stats);
    let unmatched_sum = unmatched_postpaid.amount + unmatched_amortized;

    Ok(Json(AllocationResponse {
        from: filter.range.from.clone(),
        to: filter.range.to.clone(),
        amount,
        configured: state.alloc.is_some(),
        window_days,
        days,
        days_by_provider,
        granularity: if has_days { "daily" } else { "monthly" },
        prepaid: alloc.prepaid.is_some(),
        total: round(total),
        postpaid: round(postpaid),
        amortized: round(amortized_total),
        amortized_by_period: round_map(amortized_by_period),
        daily: per_day(by_product.values().copied().sum()),
        lines: line_rows,
        unmatched: AllocLine {
            name: "未归属".to_owned(),
            amount: round(unmatched_sum),
            postpaid: round(unmatched_postpaid.amount),
            amortized: round(unmatched_amortized),
            daily: per_day(unmatched_postpaid),
            share: share_of(unmatched_sum),
            amortized_by_period: round_map(unmatched_by_period),
            items: items_of(unmatched),
            by_provider: round_parts(unmatched_parts),
        },
        unmatched_into: alloc.unmatched.map(|i| alloc.lines[i].clone()),
        products: items_of(products),
        points: points
            .into_iter()
            .map(|(t, (total, by_line))| AllocPoint {
                t,
                total: round(total),
                by_line: by_line.into_iter().map(|(k, v)| (k, round(v))).collect(),
            })
            .collect(),
        monthly,
        coverage,
        stats: merge_stats(&stats),
    }))
}

/// 「产品费用对比」默认往回看几天：够比「近 30 天与前 30 天」，再留两天给账单的滞后。
const DEFAULT_PRODUCT_DAYS: u32 = 62;
/// 最多往回看几天。
const MAX_PRODUCT_DAYS: u32 = 92;

/// 按产品、按天的后付费金额，页面据此比对两段等长的日期区间（某一天与前一天、近 7 天与
/// 前 7 天、与上周同日……）各产品花了多少。
///
/// 日期范围**不跟所选账期走**：以 `--timezone` 的今天为终点往回 `days` 天。对比的前一段常落在
/// 上个月，只选了本月时也要比得出来；也不受「日均口径」影响，只看最近 7 天时照样有前 7 天。
/// 其余筛选（云、搜索、维度、金额口径、归属规则的 include 与预付费排除）与分析视图一致。
///
/// 摆成「日期 × 产品」的矩阵而不是逐行展开：七十来个产品、两个月的天数，展开成对象要重复
/// 几千遍日期与产品名。
#[derive(Serialize)]
pub struct ProductDaysResponse {
    pub amount: Amount,
    /// 按 `--timezone` 的今天。它的账单必然没出齐，页面据此不拿它作默认的比对日
    pub today: String,
    /// 从 `today` 往回 `days` 天的**每一个**日期，升序，没有账单的日子也在：区间按下标切，
    /// 缺了哪天一眼看得出
    pub days: Vec<String>,
    /// 各云的日度账单出到哪一天。两朵云的同步进度常不一致，页面据此默认选各云都已出账的那天
    pub last_by_provider: BTreeMap<&'static str, String>,
    /// 要看、却只有月度账单的云：它们不在对比里，页面要说明
    pub monthly_only: Vec<&'static str>,
    pub rows: Vec<ProductDaysRow>,
    pub stats: Stats,
}

#[derive(Serialize)]
pub struct ProductDaysRow {
    pub provider: &'static str,
    pub product: String,
    /// 与 `days` 一一对应
    pub amounts: Vec<f64>,
}

async fn product_days(
    State(state): State<AppState>,
    p: Params,
) -> Result<Json<ProductDaysResponse>> {
    let schema = bills(&state).await?;
    let tables = schema.bills.as_ref().expect("bills checked");
    let amount = Amount::parse(p.get("amount"))?;
    let n = match p.get_u32("days")? {
        None => DEFAULT_PRODUCT_DAYS,
        Some(n @ 2..=MAX_PRODUCT_DAYS) => n,
        Some(_) => {
            return Err(Error::bad_request(format!("days 须在 2 至 {MAX_PRODUCT_DAYS} 之间")));
        }
    };
    let fallback = Alloc::default();
    let alloc = state.alloc.as_deref().unwrap_or(&fallback);

    let tz = parse_tz(&state.config.timezone)?;
    let today = chrono::DateTime::from_timestamp_millis(state.now_ms())
        .ok_or_else(|| Error::internal("当前时间超出范围"))?
        .with_timezone(&tz)
        .date_naive();
    let since = today - chrono::Days::new(u64::from(n - 1));
    let days: Vec<String> =
        since.iter_days().take(n as usize).map(|d| d.format("%Y-%m-%d").to_string()).collect();
    let mut filter = filter(&state, &p)?;
    filter.range = PeriodRange {
        from: since.format("%Y-%m").to_string(),
        to: today.format("%Y-%m").to_string(),
    };

    let wanted = providers(&p)?;
    let all = sources(tables, &wanted, false);
    let daily: Vec<(Kind, &BillTable)> =
        sources(tables, &wanted, true).into_iter().filter(|(k, _)| k.has_days()).collect();
    let monthly_only: Vec<&'static str> = all
        .iter()
        .map(|(k, _)| k.provider())
        .filter(|p| !daily.iter().any(|(k, _)| k.provider() == *p))
        .map(|p| p.as_str())
        .collect();

    let since_str = since.format("%Y-%m-%d").to_string();
    let results = futures_util::future::join_all(daily.iter().map(|(kind, table)| {
        let q = queries(&state, *kind, table).product_days(&filter, amount, alloc, &since_str);
        let client = state.client.clone();
        let provider = kind.provider().as_str();
        async move {
            match q? {
                Some(q) => Ok::<_, Error>(Some((provider, client.rows::<ProductDayRow>(q).await?))),
                None => Ok(None),
            }
        }
    }))
    .await;

    let index: BTreeMap<&str, usize> =
        days.iter().enumerate().map(|(i, d)| (d.as_str(), i)).collect();
    let mut by_product: BTreeMap<(&'static str, String), Vec<f64>> = BTreeMap::new();
    let mut last_by_provider: BTreeMap<&'static str, String> = BTreeMap::new();
    let mut stats = Vec::new();
    for r in results {
        let Some((provider, rows)) = r? else { continue };
        stats.push(rows.stats);
        for row in rows.rows {
            // 空日期、以及火山偶有的「今天之后」的日期，落不进任何一格
            let Some(&i) = index.get(row.bucket.as_str()) else { continue };
            by_product.entry((provider, row.product)).or_insert_with(|| vec![0.0; days.len()])
                [i] += row.amount;
            let last = last_by_provider.entry(provider).or_default();
            if row.bucket > *last {
                last.clone_from(&row.bucket);
            }
        }
    }
    let rows = by_product
        .into_iter()
        .map(|((provider, product), amounts)| ProductDaysRow {
            provider,
            product,
            amounts: amounts.into_iter().map(round).collect(),
        })
        .filter(|r| r.amounts.iter().any(|v| *v != 0.0))
        .collect();

    Ok(Json(ProductDaysResponse {
        amount,
        today: today.format("%Y-%m-%d").to_string(),
        days,
        last_by_provider,
        monthly_only,
        rows,
        stats: merge_stats(&stats),
    }))
}

/// 按天钻取：某一天与前一天，各业务线由哪些产品构成、各花了多少。
///
/// 「按天的业务线构成」那张图点某一天时用：图只到业务线一层，想知道那一天某条线为什么涨，
/// 得往下看到产品。口径与那张图一致——后付费、按同一套归属规则拆（预付费另走摊销，按天
/// 看没有意义）。产品带着云厂商，页面据此跳到账单明细时知道该看哪张表。
#[derive(Serialize)]
pub struct AllocDayResponse {
    pub day: String,
    /// 前一天，比对用
    pub previous_day: String,
    pub amount: Amount,
    pub configured: bool,
    pub current: f64,
    pub previous: f64,
    /// 各业务线，按配置里的顺序；两天都没花钱的线不列
    pub lines: Vec<AllocDayLine>,
    /// 未命中任何规则的部分。配了 `unmatched` 时它已同时计入那条业务线，见 `unmatched_into`
    pub unmatched: AllocDayLine,
    pub unmatched_into: Option<String>,
    /// 不分业务线，只按「云 + 产品」
    pub products: Vec<AllocDayItem>,
    pub stats: Stats,
}

#[derive(Serialize)]
pub struct AllocDayLine {
    pub name: String,
    pub current: f64,
    pub previous: f64,
    pub items: Vec<AllocDayItem>,
}

#[derive(Serialize)]
pub struct AllocDayItem {
    pub provider: &'static str,
    pub product: String,
    /// 按哪条规则归来的；不分业务线的产品表、未命中规则的部分为 null
    pub rule: Option<String>,
    pub current: f64,
    pub previous: f64,
}

/// （云, 产品, 规则名）→ [当天, 前一天]
type DayItems = BTreeMap<(&'static str, String, String), [f64; 2]>;

async fn allocation_day(
    State(state): State<AppState>,
    p: Params,
) -> Result<Json<AllocDayResponse>> {
    let schema = bills(&state).await?;
    let tables = schema.bills.as_ref().expect("bills checked");
    let amount = Amount::parse(p.get("amount"))?;
    let day = parse_day(p.get("day").ok_or_else(|| Error::bad_request("缺少 day（YYYY-MM-DD）"))?)?;
    let prev = day.pred_opt().ok_or_else(|| Error::bad_request("日期超出范围"))?;
    let fallback = Alloc::default();
    let alloc = state.alloc.as_deref().unwrap_or(&fallback);

    // 账期取这两天所在的月份（前一天可能在上个月），日期条件再收到这两天
    let mut filter = filter(&state, &p)?;
    filter.range =
        PeriodRange { from: prev.format("%Y-%m").to_string(), to: day.format("%Y-%m").to_string() };
    filter.days = Some((prev.to_string(), day.to_string()));
    let sources: Vec<(Kind, &BillTable)> =
        sources(tables, &providers(&p)?, true).into_iter().filter(|(k, _)| k.has_days()).collect();
    if sources.is_empty() {
        return Err(Error::bad_request("按天钻取需要日度账单：请先同步日度账单"));
    }
    let results = futures_util::future::join_all(sources.iter().map(|(kind, table)| {
        let q = queries(&state, *kind, table).alloc_detail(&filter, amount, alloc, None);
        let client = state.client.clone();
        let provider = kind.provider().as_str();
        async move { Ok::<_, Error>((provider, client.rows::<AllocDetailRow>(q?).await?)) }
    }))
    .await;

    let (day_s, prev_s) = (day.to_string(), prev.to_string());
    let rule_of = |rule: i32| usize::try_from(rule).ok().and_then(|i| alloc.rules.get(i));
    let rule_name = |rule: i32| rule_of(rule).map(|r| r.name.clone()).unwrap_or_default();
    let n = alloc.lines.len();
    let mut lines: Vec<DayItems> = vec![BTreeMap::new(); n];
    let mut unmatched: DayItems = BTreeMap::new();
    let mut products: DayItems = BTreeMap::new();
    let mut stats = Vec::new();
    for r in results {
        let (provider, rows) = r?;
        stats.push(rows.stats);
        for row in rows.rows {
            let slot = if row.bucket == day_s {
                0
            } else if row.bucket == prev_s {
                1
            } else {
                continue;
            };
            products.entry((provider, row.product.clone(), String::new())).or_default()[slot] +=
                row.amount;
            if rule_of(row.rule).is_none() {
                unmatched.entry((provider, row.product.clone(), String::new())).or_default()
                    [slot] += row.amount;
            }
            for (line, part) in spread(alloc, row.rule, row.amount) {
                let Some(line) = line else { continue };
                lines[line]
                    .entry((provider, row.product.clone(), rule_name(row.rule)))
                    .or_default()[slot] += part;
            }
        }
    }

    let items_of = |items: DayItems| -> Vec<AllocDayItem> {
        let mut rows: Vec<AllocDayItem> = items
            .into_iter()
            .map(|((provider, product, rule), [cur, prev])| AllocDayItem {
                provider,
                product,
                rule: (!rule.is_empty()).then_some(rule),
                current: round(cur),
                previous: round(prev),
            })
            .filter(|i| i.current != 0.0 || i.previous != 0.0)
            .collect();
        rows.sort_by(|a, b| {
            b.current.total_cmp(&a.current).then_with(|| b.previous.total_cmp(&a.previous))
        });
        rows
    };
    let line_of = |name: String, items: DayItems| -> AllocDayLine {
        let items = items_of(items);
        AllocDayLine {
            name,
            current: round(items.iter().map(|i| i.current).sum()),
            previous: round(items.iter().map(|i| i.previous).sum()),
            items,
        }
    };
    let products = items_of(products);
    Ok(Json(AllocDayResponse {
        day: day_s,
        previous_day: prev_s,
        amount,
        configured: state.alloc.is_some(),
        current: round(products.iter().map(|i| i.current).sum()),
        previous: round(products.iter().map(|i| i.previous).sum()),
        lines: alloc
            .lines
            .iter()
            .zip(lines)
            .map(|(name, items)| line_of(name.clone(), items))
            .filter(|l| !l.items.is_empty())
            .collect(),
        unmatched: line_of("未归属".to_owned(), unmatched),
        unmatched_into: alloc.unmatched.map(|i| alloc.lines[i].clone()),
        products,
        stats: merge_stats(&stats),
    }))
}

/// 日度账单是否明显少于月度账单。
///
/// 两张表是同一批账单的两种粒度，同一段账期的合计理应相当；日度表若只补了几天，分析视图
/// 的区间合计与各业务线金额就会严重偏低，而页面本身看不出来。所以拿两张表的合计比一比，
/// 差得多时如实告诉页面。只看阿里云：火山只有一张表，无从比较。
async fn coverage(
    state: &AppState,
    tables: &BillTables,
    sources: &[(Kind, &BillTable)],
    filter: &BillFilter,
    amount: Amount,
) -> Result<(Option<Coverage>, Vec<Stats>)> {
    let (Some(daily), Some(monthly)) = (&tables.alicloud_daily, &tables.alicloud_monthly) else {
        return Ok((None, Vec::new()));
    };
    if !sources.iter().any(|(k, _)| *k == Kind::AlicloudDaily) {
        return Ok((None, Vec::new()));
    }
    let d = queries(state, Kind::AlicloudDaily, daily).total(filter, amount)?;
    let m = queries(state, Kind::AlicloudMonthly, monthly).total(filter, amount)?;
    let (d, m) = tokio::join!(state.client.rows::<TotalRow>(d), state.client.rows::<TotalRow>(m));
    let (d, m) = (d?, m?);
    let daily_total = d.rows.first().and_then(|r| r.amount).unwrap_or(0.0);
    let monthly_total = m.rows.first().and_then(|r| r.amount).unwrap_or(0.0);
    let stats = vec![d.stats, m.stats];
    if monthly_total <= 0.0 || daily_total >= monthly_total * COVERAGE_WARN {
        return Ok((None, stats));
    }
    let coverage = Coverage {
        provider: Provider::Alicloud.as_str(),
        daily: round(daily_total),
        monthly: round(monthly_total),
    };
    Ok((Some(coverage), stats))
}

/// 日度账单不到月度账单的这个比例时告警。留 5% 的余地：月度表当月的那份可能比日度表
/// 多出一两天，也可能少一两天，差这么一点不值得打扰人。
const COVERAGE_WARN: f64 = 0.95;

/// 服务期最长认几个月。十年以上的服务期在账单里不会出现，出现了多半是脏数据。
const MAX_SERVICE_MONTHS: u32 = 120;

/// `YYYY-MM` 这个月有几天。
fn days_in_period(period: &str) -> u32 {
    let (Some(y), Some(m)) = (
        period.get(..4).and_then(|v| v.parse::<i32>().ok()),
        period.get(5..7).and_then(|v| v.parse::<u32>().ok()),
    ) else {
        return 30;
    };
    let first = chrono::NaiveDate::from_ymd_opt(y, m, 1);
    let next = if m == 12 {
        chrono::NaiveDate::from_ymd_opt(y + 1, 1, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(y, m + 1, 1)
    };
    match (first, next) {
        (Some(a), Some(b)) => (b - a).num_days() as u32,
        _ => 30,
    }
}

/// 一笔后付费：金额，以及它对日均的贡献（金额 ÷ 它所在那张表的天数）。
///
/// 把日均当成一个可以相加的量来累计，是为了让「各云各按各的天数」在任意层级都成立：
/// 一条业务线、一个产品的日均，都是其下各笔贡献之和，不必再追溯每一笔来自哪张表。
#[derive(Debug, Default, Clone, Copy)]
struct Money {
    amount: f64,
    daily: f64,
}

impl std::ops::AddAssign for Money {
    fn add_assign(&mut self, o: Self) {
        self.amount += o.amount;
        self.daily += o.daily;
    }
}

impl std::ops::Mul<f64> for Money {
    type Output = Money;
    fn mul(self, k: f64) -> Money {
        Money { amount: self.amount * k, daily: self.daily * k }
    }
}

impl std::iter::Sum for Money {
    fn sum<I: Iterator<Item = Money>>(iter: I) -> Money {
        let mut total = Money::default();
        for m in iter {
            total += m;
        }
        total
    }
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
    // 按日期筛只有日度表做得到：带了日期就换到日度表，不管 granularity 写的是什么
    if day_range(p)?.is_some() {
        let list = sources(tables, &provider.into_iter().collect::<Vec<_>>(), true);
        return list.into_iter().find(|(k, _)| k.has_days()).ok_or_else(|| {
            Error::bad_request(match provider {
                Some(p) => format!("{}没有日度账单，无法按日期筛选；请先同步日度账单", p.label()),
                None => "当前部署没有日度账单，无法按日期筛选".to_owned(),
            })
        });
    }
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

/// `day_from` / `day_to`（`YYYY-MM-DD`）：明细按日期收窄。只给一端时两端相同，即只看那一天。
fn day_range(p: &Params) -> Result<Option<(String, String)>> {
    let (from, to) = (p.get("day_from"), p.get("day_to"));
    let (Some(from), Some(to)) = (from.or(to), to.or(from)) else { return Ok(None) };
    let (from, to) = (parse_day(from)?, parse_day(to)?);
    if from > to {
        return Err(Error::bad_request(format!("起始日期 {from} 晚于结束日期 {to}")));
    }
    Ok(Some((from.to_string(), to.to_string())))
}

fn parse_day(raw: &str) -> Result<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
        .map_err(|_| Error::bad_request(format!("日期须为 YYYY-MM-DD，不是 {raw:?}")))
}

/// 明细、导出、表头候选值用的筛选：账期与维度之外，再带上日期区间。
fn detail_filter(state: &AppState, p: &Params) -> Result<BillFilter> {
    let mut f = filter(state, p)?;
    f.days = day_range(p)?;
    Ok(f)
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
    let filter = detail_filter(&state, &p)?;
    let (kind, table) = detail_source(tables, &p)?;
    let limit = p.get_limit("limit", 100, MAX_DETAIL_ROWS.min(state.config.max_rows))?;
    let offset = p.get_u32("offset")?.unwrap_or(0);
    if offset > state.config.max_offset {
        return Err(Error::bad_request(format!(
            "最多翻至第 {} 条；如需继续，请缩小账期范围或增加筛选条件",
            state.config.max_offset
        )));
    }
    let sort = DetailSort::parse(p.get("sort"), p.get("order"))?;
    let raw = p.get_list("cols");
    let q = queries(&state, kind, table);
    let (rows, count) = tokio::join!(
        state.client.rows::<DetailRow>(q.detail(&filter, amount, &sort, &raw, limit, offset)?),
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

/// 表头下拉每个维度最多列出多少个值。再多就该用搜索框了。
const FACET_LIMIT: u32 = 200;

#[derive(Serialize)]
pub struct FacetsResponse {
    /// 维度名 → 出现最多的若干个取值，从多到少
    pub facets: BTreeMap<&'static str, Vec<String>>,
    pub stats: Stats,
}

/// 明细表头下拉筛选的候选值，`dims=product,region,…`。与明细同一张表、同一套筛选，
/// 但每个维度不带它自己的那条筛选（见 [`BillQueries::facets`]）。
async fn facets(State(state): State<AppState>, p: Params) -> Result<Json<FacetsResponse>> {
    let schema = bills(&state).await?;
    let tables = schema.bills.as_ref().expect("bills checked");
    let filter = detail_filter(&state, &p)?;
    let (kind, table) = detail_source(tables, &p)?;
    let dims: Vec<Dimension> =
        p.get_list("dims").iter().map(|d| Dimension::parse(d)).collect::<Result<_>>()?;
    if dims.is_empty() {
        return Err(Error::bad_request("dims 至少给一个维度"));
    }
    let q = queries(&state, kind, table).facets(&filter, &dims, FACET_LIMIT)?;
    let rows = state.client.rows::<BTreeMap<String, Vec<String>>>(q).await?;
    let mut row = rows.rows.into_iter().next().unwrap_or_default();
    let facets =
        dims.iter().map(|d| (d.as_str(), row.remove(d.as_str()).unwrap_or_default())).collect();
    Ok(Json(FacetsResponse { facets, stats: rows.stats }))
}

/// 导出明细 CSV / JSONL：ClickHouse 直接出格式化文本，这里只转发字节流。
async fn export(State(state): State<AppState>, p: Params) -> Result<Response> {
    let schema = bills(&state).await?;
    let tables = schema.bills.as_ref().expect("bills checked");
    let amount = Amount::parse(p.get("amount"))?;
    let filter = detail_filter(&state, &p)?;
    let (kind, table) = detail_source(tables, &p)?;
    let limit = p.get_limit("limit", state.config.export_max_rows, state.config.export_max_rows)?;
    let (format, content_type, ext) = match p.get("format").unwrap_or("csv") {
        "csv" => ("CSVWithNames", "text/csv; charset=utf-8", "csv"),
        "jsonl" | "json" => ("JSONEachRow", "application/x-ndjson; charset=utf-8", "jsonl"),
        other => {
            return Err(Error::bad_request(format!("format 只能是 csv 或 jsonl，不是 {other:?}")));
        }
    };
    let sort = DetailSort::parse(p.get("sort"), p.get("order"))?;
    let raw = p.get_list("cols");
    let query = queries(&state, kind, table).export(&filter, amount, &sort, &raw, limit)?;
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
            "当前部署未配置 --goscan-url（OPDASH_GOSCAN_URL），无法同步账单：\
             账单由 goscan 向云厂商同步，opdash 仅负责转发该指令",
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

/// 同步跑到哪了。单位是「趟」——一个账期一种粒度算一趟，页面据此画进度条。
///
/// 阿里云选了「月度 + 日度」时，同一个账期会被拉两趟（一趟写月表、一趟写日表），
/// 因此总数是账期数乘以粒度数，光有账期说不清当前在做什么，粒度要一并带上。
#[derive(Serialize)]
pub struct SyncProgressView {
    /// 正在拉的账期
    pub period: String,
    /// `monthly` / `daily`；火山不分粒度，老版本 goscan 也不报，此时为空
    #[serde(skip_serializing_if = "String::is_empty")]
    pub granularity: String,
    pub periods_done: i64,
    pub periods_total: i64,
    /// 这一趟已经写入的行数。一趟可能要跑好几分钟，靠它看出还在动；老版本 goscan 不报，为 0
    pub records: i64,
    /// 这一趟接口报的总行数；按天拉整月时事先不知道，此时不出现
    #[serde(skip_serializing_if = "Option::is_none")]
    pub records_total: Option<i64>,
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
    /// 跑到第几个账期了；老版本 goscan 不报，就是 null
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<SyncProgressView>,
    /// 有人请它停下了：它会把手上这一趟写完再停，这期间 `status` 仍是 `running`
    pub cancel_requested: bool,
    /// 被停下的同步没跑的那几趟（如 `2026-04 daily`），这些账期的数据原样没动
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_run: Vec<String>,
    /// 任务发起时的账期区间。接上一个已经在跑的任务（比如 cron 起的）时，页面据此说明它在拉什么
    #[serde(skip_serializing_if = "String::is_empty")]
    pub from: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub to: String,
}

/// 查询某个同步任务的当前状态。页面订阅不了事件流（老版本 goscan）时据此轮询，
/// `done` 之后即刷新账单查询的缓存。
async fn sync_task(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
) -> Result<Json<SyncTaskResponse>> {
    let client = goscan(&state)?;
    Ok(Json(task_view(client.task(&task_id).await?)))
}

/// goscan 的任务 → 页面要的形状。轮询、事件流、「正在进行的同步」三条路都走这里，
/// 页面只认一种 JSON。
fn task_view(task: TaskRow) -> SyncTaskResponse {
    let done = task.finished();
    let result = task.result;
    // 账期总数为 0 说明这一版 goscan 还不报进度，别把「0 / 0」当成进度画出来
    let progress = task.progress.filter(|p| p.total > 0).map(|p| SyncProgressView {
        period: p.period,
        granularity: p.granularity,
        periods_done: p.done.clamp(0, p.total),
        periods_total: p.total,
        records: p.records.max(0),
        records_total: (p.records_total > 0).then_some(p.records_total),
    });
    let config = task.config.unwrap_or_default();
    let (from, to) = if !config.start_period.is_empty() {
        (config.start_period, config.end_period)
    } else {
        (config.bill_period.clone(), config.bill_period)
    };
    SyncTaskResponse {
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
        not_run: result.map(|r| r.not_run).unwrap_or_default(),
        cancel_requested: task.cancel_requested,
        id: task.id,
        status: task.status,
        provider: task.provider,
        started_at: real_time(task.start_time),
        ended_at: real_time(task.end_time),
        progress,
        from,
        to,
    }
}

#[derive(Serialize)]
pub struct SyncCancelResponse {
    pub task_id: String,
    /// goscan 回的是 `cancelling`：已请它停下，要等手上这一趟写完
    pub status: String,
    pub message: String,
}

/// 停止一个同步。goscan 立刻答应（202），但会把手上这一趟（一个账期 × 一种粒度）写完才停：
/// 每一趟拉之前都先清空那个账期，半路掐断会留下只写了一半的账期。已经结束的任务回 409。
async fn cancel_sync(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
) -> Result<(axum::http::StatusCode, Json<SyncCancelResponse>)> {
    let client = goscan(&state)?;
    tracing::info!(task_id = %task_id, goscan = client.base(), "请求停止账单同步");
    let r = client.cancel(&task_id).await?;
    Ok((
        axum::http::StatusCode::ACCEPTED,
        Json(SyncCancelResponse {
            task_id: if r.task_id.is_empty() { task_id } else { r.task_id },
            status: if r.status.is_empty() { "cancelling".to_owned() } else { r.status },
            message: r.message,
        }),
    ))
}

#[derive(Serialize)]
pub struct SyncRunningResponse {
    /// 这朵云正在进行的同步；没有就是 null
    pub task: Option<SyncTaskResponse>,
}

/// 这朵云眼下有没有同步在跑（手动的、cron 起的都算）。
///
/// goscan 同一朵云同时只允许一个同步：再点「开始拉取」会被 409 挡回来。与其只报一句「正在同步中」，
/// 不如把那个任务找出来接着看它的进度——关掉对话框再打开时也用得上。
async fn sync_running(
    State(state): State<AppState>,
    p: Params,
) -> Result<Json<SyncRunningResponse>> {
    let client = goscan(&state)?;
    let provider = match p.get("provider") {
        Some(raw) => Some(Provider::parse(raw)?),
        None => None,
    };
    let task = client
        .tasks()
        .await?
        .into_iter()
        // 老版本 goscan 的任务不带 type，一律当同步看；企微日报那种不是
        .filter(|t| t.kind.is_empty() || t.kind == "sync")
        .filter(|t| !t.finished())
        .filter(|t| provider.is_none_or(|p| t.provider == p.as_str()))
        // 同一朵云按理只有一个；万一有多个，看最近开始的那个
        .max_by(|a, b| a.start_time.cmp(&b.start_time))
        .map(task_view);
    Ok(Json(SyncRunningResponse { task }))
}

/// 同步进度的事件流：把 goscan 的 `GET /tasks/{id}/events` 转给浏览器。
///
/// 事件名与 goscan 一致——`task` 是一份任务状态（形状同 [`sync_task`]，已换成页面要的样子），
/// `done` 表示任务已结束、流随即关闭，页面收到后必须 `close()`，否则 `EventSource` 会自己重连。
/// goscan 太旧没有这个接口时回 404，`EventSource` 遇到非 200 不会重连，页面据此退回轮询。
///
/// 转发是逐条的：每解析出一帧就推一帧，不攒。SSE 的压缩已在 [`crate::ui::compression`] 里排除，
/// nginx 的缓冲由 `X-Accel-Buffering: no` 关掉。
async fn sync_events(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
) -> Result<Response> {
    let client = goscan(&state)?;
    let upstream = client.events(&task_id).await?;
    let frames = futures_util::stream::unfold(
        SseReader { body: Box::pin(upstream.bytes_stream()), buf: Vec::new(), over: false },
        |mut r| async move {
            let event = r.next_event().await?;
            Some((Ok::<_, std::convert::Infallible>(event), r))
        },
    );
    Ok(super::tail::sse_response(frames))
}

/// 从 goscan 的事件流里一帧一帧地读，转成发给浏览器的事件。
struct SseReader {
    body: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    /// 按字节攒：一个汉字可能被拆在两块之间，攒满一整帧再解码才不会出乱码
    buf: Vec<u8>,
    /// 已经推过 `done`（或者上游出错），下一次就结束这条流
    over: bool,
}

impl SseReader {
    async fn next_event(&mut self) -> Option<axum::response::sse::Event> {
        use axum::response::sse::Event;
        use futures_util::StreamExt;
        loop {
            if self.over {
                return None;
            }
            // 缓冲里攒够了一帧（以空行结尾）就先把它处理掉
            if let Some(end) = self.buf.windows(2).position(|w| w == b"\n\n") {
                let raw: Vec<u8> = self.buf.drain(..end + 2).collect();
                let frame = String::from_utf8_lossy(&raw);
                let mut name = "message";
                let mut data = String::new();
                for line in frame.lines().map(|l| l.trim_end_matches('\r')) {
                    if let Some(v) = line.strip_prefix("event:") {
                        name = if v.trim() == "task" {
                            "task"
                        } else if v.trim() == "done" {
                            "done"
                        } else {
                            "other"
                        };
                    } else if let Some(v) = line.strip_prefix("data:") {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(v.strip_prefix(' ').unwrap_or(v));
                    }
                    // `: ping` 注释和 `retry:` 都不必转：保活由本端的 KeepAlive 负责
                }
                match name {
                    "task" => match serde_json::from_str::<TaskRow>(&data) {
                        Ok(task) => {
                            if let Ok(event) =
                                Event::default().event("task").json_data(task_view(task))
                            {
                                return Some(event);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "goscan 推来的任务解析失败，跳过这一帧")
                        }
                    },
                    "done" => {
                        self.over = true;
                        return Some(Event::default().event("done").data("{}"));
                    }
                    _ => {}
                }
                continue;
            }
            match self.body.next().await {
                Some(Ok(chunk)) => {
                    // goscan 的行尾是 \n；万一经过的代理改成了 \r\n，先去掉 \r 再找空行
                    self.buf.extend(chunk.iter().copied().filter(|b| *b != b'\r'));
                }
                Some(Err(e)) => {
                    // 上游断了。不推 done：让浏览器的 EventSource 自己重连，重连后第一条就是最新状态；
                    // 任务若已随 goscan 重启而丢失，重连会拿到 404，页面随之退回轮询并说明原因
                    tracing::warn!(error = %e, "goscan 的事件流中断");
                    self.over = true;
                    return None;
                }
                None => return None,
            }
        }
    }
}

/// goscan 的时间是 Go 的 `time.Time`，没发生的事件报的是零值 `0001-01-01T00:00:00Z` 而不是
/// null——任务还在跑，`end_time` 就是它。原样转给页面，页面会当成任务已经结束，停表并算出
/// 负数的已用时（截成「0 秒」）。这里把零值（以及空串）都当作「还没有」。
fn real_time(t: Option<String>) -> Option<String> {
    t.filter(|v| !v.is_empty() && !v.starts_with("0001-01-01"))
}
