//! `/api/db/*`：业务数据源（`--datasources`）的列表、结构、只读查询、慢查询。
//!
//! 目前只有 MCP 在用（工具经进程内 Router 调到这里，见 [`crate::mcp`]），但照样是普通的 API：
//! 参数校验、错误形状和其它接口一致，将来页面要用也不必另写一套。认证和其它 `/api/*` 相同。

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde_json::{Value, json};

use super::{AppState, params::Params};
use crate::datasource::{Ctx, QueryOpts, SlowOpts, SlowSort};
use crate::error::{Error, Result};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/db/sources", get(sources))
        .route("/api/db/{source}/tables", get(tables))
        .route("/api/db/{source}/describe", get(describe))
        .route("/api/db/{source}/query", get(query))
        .route("/api/db/{source}/slow", get(slow))
}

/// 慢查询默认看最近多久。
const DEFAULT_SLOW_RANGE_MS: i64 = 3_600_000;

fn ctx(state: &AppState) -> Ctx {
    Ctx {
        tz: crate::query::parse_tz(&state.config.timezone).unwrap_or(chrono_tz::UTC),
        now_ms: state.now_ms(),
    }
}

async fn sources(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "env": state.config.env,
        "sources": state.datasources.all().iter().map(|s| s.summary()).collect::<Vec<_>>(),
    }))
}

async fn tables(
    State(state): State<AppState>,
    Path(source): Path<String>,
    p: Params,
) -> Result<Json<Value>> {
    let src = state.datasources.get(&source)?;
    let limit = p.get_u32("limit")?.unwrap_or(200);
    let mut out = src.tables(p.get("database"), p.get("match"), limit).await?;
    out["source"] = json!(src.name);
    Ok(Json(out))
}

async fn describe(
    State(state): State<AppState>,
    Path(source): Path<String>,
    p: Params,
) -> Result<Json<Value>> {
    let src = state.datasources.get(&source)?;
    let target = p.get("target").ok_or_else(|| Error::bad_request("缺少参数 target"))?;
    let mut out = src.describe(target, p.get("database")).await?;
    out["source"] = json!(src.name);
    Ok(Json(out))
}

async fn query(
    State(state): State<AppState>,
    Path(source): Path<String>,
    p: Params,
) -> Result<Json<Value>> {
    let src = state.datasources.get(&source)?;
    let q = p.get("q").ok_or_else(|| Error::bad_request("缺少参数 q（要执行的查询）"))?;
    let opts = QueryOpts {
        database: p.get("database").map(str::to_owned),
        index: p.get("index").map(str::to_owned),
        limit: p.get_u32("limit")?.unwrap_or(0),
    };
    let result = src.query(q, opts).await;
    // 模型执行了什么都记一笔：业务库上的查询，事后要能查得到是谁在什么时候查了什么
    tracing::info!(
        source = %src.name,
        query = %crate::datasource::sql::one_line(q),
        ok = result.is_ok(),
        "数据源查询"
    );
    let mut out = result?;
    out["source"] = json!(src.name);
    Ok(Json(out))
}

async fn slow(
    State(state): State<AppState>,
    Path(source): Path<String>,
    p: Params,
) -> Result<Json<Value>> {
    let src = state.datasources.get(&source)?;
    let ctx = ctx(&state);
    let to_ms = p.get_i64("to")?.unwrap_or(ctx.now_ms);
    let from_ms = p.get_i64("from")?.unwrap_or(to_ms - DEFAULT_SLOW_RANGE_MS);
    if from_ms > to_ms {
        return Err(Error::bad_request("from 不能晚于 to"));
    }
    let opts = SlowOpts {
        database: p.get("database").map(str::to_owned),
        from_ms,
        to_ms,
        min_ms: p.get_f64("min_ms")?.unwrap_or(0.0).max(0.0),
        sort: SlowSort::parse(p.get("sort"))?,
        limit: p.get_u32("limit")?.unwrap_or(20),
    };
    let mut out = src.slow(opts, ctx).await?;
    out["source"] = json!(src.name);
    Ok(Json(out))
}
