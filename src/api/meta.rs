//! `/api/health` 与 `/api/meta`。

use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;

use super::AppState;
use crate::schema::{
    ALICLOUD_BILL_COLUMNS, LOG_FIXED_COLUMNS, METRIC_FIXED_COLUMNS, TRACE_FIXED_COLUMNS, Table,
    VOLCENGINE_BILL_COLUMNS,
};

#[derive(Serialize)]
pub struct Health {
    pub ok: bool,
    /// `ok` 或错误原因
    pub clickhouse: String,
    /// `ok` 或错误原因（表不存在、缺列……）
    pub schema: String,
    pub version: &'static str,
}

/// 每次都真的去 ping 一下 ClickHouse：探针就是要知道「现在能不能查」。
pub async fn health(State(state): State<AppState>) -> (StatusCode, Json<Health>) {
    let clickhouse = match state.client.ping().await {
        Ok(()) => "ok".to_owned(),
        Err(e) => e.user_message(),
    };
    let schema = match state.schema.current() {
        Some(_) => "ok".to_owned(),
        None => state.schema.last_error().unwrap_or_else(|| "尚未读取".to_owned()),
    };
    let ok = clickhouse == "ok" && schema == "ok";
    let status = if ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, Json(Health { ok, clickhouse, schema, version: env!("CARGO_PKG_VERSION") }))
}

#[derive(Serialize)]
pub struct Meta {
    pub version: &'static str,
    /// 这套 opdash 属于哪个环境（`--env`），没配就省略
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    pub database: String,
    /// 直方图对齐用的时区（`--timezone`）
    pub timezone: String,
    pub now_ms: i64,
    pub limits: Limits,
    pub server: Server,
    pub logs: TableMeta,
    pub traces: TableMeta,
    /// 指标表；没部署 metricpipe 就是 null，前端据此不显示指标页
    pub metrics: Option<TableMeta>,
    /// 指标页没启用的原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics_note: Option<String>,
    /// goscan 的账单表；没部署 goscan 就是 null，前端据此不显示费用页
    pub bills: Option<BillsMeta>,
    /// 费用页没启用、或者某一张账单表没启用的原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bills_note: Option<String>,
}

/// 账单表的现状。三张各自可选，前端按 `providers` 决定「阿里云 / 火山」这两个筛选项显不显示，
/// 按 `granularities` 决定明细页能不能切到日粒度。
#[derive(Serialize)]
pub struct BillsMeta {
    pub providers: Vec<&'static str>,
    /// 能按天看花费的云；阿里云要同步了日度账单才在里面
    pub daily_providers: Vec<&'static str>,
    pub volcengine: Option<TableMeta>,
    pub alicloud_monthly: Option<TableMeta>,
    pub alicloud_daily: Option<TableMeta>,
    /// 查询时按哪个键去重（`group` / `final` / `off`，见 --bill-dedupe）
    pub dedupe: crate::query::bills::Dedupe,
    /// 配了 `--goscan-url` 才能手动拉账单（`POST /api/bills/sync`），页面据此显示按钮
    pub sync: bool,
    /// 成本归属规则（`--bill-alloc`）；没配就是 null，费用页的分析视图只给按产品的日均
    pub allocation: Option<AllocationMeta>,
}

/// 归属规则的概况。规则的具体内容不外发：页面只需要知道有哪几条业务线。
#[derive(Serialize)]
pub struct AllocationMeta {
    pub lines: Vec<String>,
    pub rules: usize,
}

#[derive(Serialize)]
pub struct Limits {
    pub max_rows: u32,
    pub max_offset: u32,
    pub export_max_rows: u32,
    pub max_trace_spans: u32,
    pub max_range_ms: i64,
    pub query_timeout_ms: u64,
}

#[derive(Serialize)]
pub struct Server {
    pub version: String,
    pub timezone: String,
}

#[derive(Serialize)]
pub struct TableMeta {
    pub table: String,
    pub columns: Vec<crate::schema::Column>,
    /// 固定列之外的字符串列：k8s 元数据、静态 fields。前端按这个列表显示筛选项。
    pub dimensions: Vec<String>,
}

impl TableMeta {
    fn new(table: &Table, fixed: &[&str]) -> Self {
        Self {
            table: table.name.clone(),
            columns: table.columns.clone(),
            dimensions: table.extra_string_columns(fixed).iter().map(|c| c.name.clone()).collect(),
        }
    }
}

pub async fn meta(State(state): State<AppState>) -> crate::error::Result<Json<Meta>> {
    let schema = state.schema.get().await?;
    let cfg = &state.config;
    Ok(Json(Meta {
        version: env!("CARGO_PKG_VERSION"),
        env: cfg.env.clone(),
        database: cfg.database.clone(),
        timezone: cfg.timezone.clone(),
        now_ms: state.now_ms(),
        limits: Limits {
            max_rows: cfg.max_rows,
            max_offset: cfg.max_offset,
            export_max_rows: cfg.export_max_rows,
            max_trace_spans: cfg.max_trace_spans,
            max_range_ms: cfg.max_range.as_millis() as i64,
            query_timeout_ms: cfg.query_timeout.as_millis() as u64,
        },
        server: Server {
            version: schema.server_version.clone(),
            timezone: schema.server_timezone.clone(),
        },
        logs: TableMeta::new(&schema.logs, LOG_FIXED_COLUMNS),
        traces: TableMeta::new(&schema.traces, TRACE_FIXED_COLUMNS),
        metrics: schema.metrics.as_ref().map(|t| TableMeta::new(t, METRIC_FIXED_COLUMNS)),
        metrics_note: schema.metrics_note.clone(),
        bills: schema.bills.as_ref().map(|b| BillsMeta {
            providers: [
                b.volcengine.is_some().then_some("volcengine"),
                (b.alicloud_monthly.is_some() || b.alicloud_daily.is_some()).then_some("alicloud"),
            ]
            .into_iter()
            .flatten()
            .collect(),
            daily_providers: [
                b.volcengine.is_some().then_some("volcengine"),
                b.alicloud_daily.is_some().then_some("alicloud"),
            ]
            .into_iter()
            .flatten()
            .collect(),
            volcengine: b
                .volcengine
                .as_ref()
                .map(|t| TableMeta::new(&t.table, VOLCENGINE_BILL_COLUMNS)),
            alicloud_monthly: b
                .alicloud_monthly
                .as_ref()
                .map(|t| TableMeta::new(&t.table, ALICLOUD_BILL_COLUMNS)),
            alicloud_daily: b
                .alicloud_daily
                .as_ref()
                .map(|t| TableMeta::new(&t.table, ALICLOUD_BILL_COLUMNS)),
            dedupe: cfg.bill_dedupe,
            sync: state.goscan.is_some(),
            allocation: state
                .alloc
                .as_deref()
                .map(|a| AllocationMeta { lines: a.lines.clone(), rules: a.rules.len() }),
        }),
        bills_note: schema.bills_note.clone(),
    }))
}
