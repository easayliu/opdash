//! `/api/health` 与 `/api/meta`。

use axum::{Json, extract::State, http::StatusCode};
use serde::Serialize;

use super::AppState;
use crate::schema::{LOG_FIXED_COLUMNS, TRACE_FIXED_COLUMNS, Table};

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
    pub database: String,
    /// 直方图对齐用的时区（`--timezone`）
    pub timezone: String,
    pub now_ms: i64,
    pub limits: Limits,
    pub server: Server,
    pub logs: TableMeta,
    pub traces: TableMeta,
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
    }))
}
