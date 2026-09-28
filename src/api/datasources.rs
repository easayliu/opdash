//! `/api/db/*`：业务数据源（`--datasources`）的列表、结构、只读查询、慢查询。
//!
//! 目前只有 MCP 在用（工具经进程内 Router 调到这里，见 [`crate::mcp`]），但照样是普通的 API：
//! 参数校验、错误形状和其它接口一致，将来页面要用也不必另写一套。认证和其它 `/api/*` 相同。
//!
//! ## 审计
//!
//! 碰到业务库的操作（列表、看结构、查询、慢查询）每次记一条 `target = opdash::audit` 的 info 日志：
//! 谁（账号名、认证方式、API key 的 id）、经哪条路（`mcp` / `http`）、对哪个数据源做了什么、
//! 成没成、用了多久。挂在 `opdash::` 下面，`RUST_LOG=opdash=debug` 这类写法不会把它关掉；
//! 要单独收可以按 target 过滤，如 `RUST_LOG=warn,opdash::audit=info`。
//!
//! 所有登录用户对所有数据源的权限相同，这里只记不拦。

use std::convert::Infallible;
use std::time::Instant;

use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, State},
    http::request::Parts,
    routing::get,
};
use serde_json::{Value, json};

use super::{AppState, params::Params};
use crate::auth::Identity;
use crate::datasource::{Address, Ctx, QueryOpts, SlowOpts, SlowSort};
use crate::error::{Error, Result};

/// 审计日志的 target，见模块文档。
pub const AUDIT_TARGET: &str = "opdash::audit";

/// 审计里记的语句最长多少个字符。比普通日志的 300 长得多：事后要能看出查了什么，
/// 截得太短就只剩 `SELECT … FROM`。
const AUDIT_DETAIL_MAX: usize = 4096;

/// 这次操作是谁发起的：认证中间件放进 extensions 的身份，MCP 的进程内请求也带着它
/// （见 [`crate::mcp::InProcess`]）。没开认证就没有身份。
struct Caller {
    who: Option<Identity>,
    via: &'static str,
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> std::result::Result<Self, Infallible> {
        let via =
            if parts.extensions.get::<crate::mcp::InProcess>().is_some() { "mcp" } else { "http" };
        Ok(Caller { who: parts.extensions.get::<Identity>().cloned(), via })
    }
}

/// 一次数据源操作要记的内容。`detail` 是语句 / 目标表 / 匹配串，没有就空着。
struct Audit<'a> {
    source: &'a str,
    action: &'static str,
    database: Option<&'a str>,
    detail: Option<&'a str>,
}

impl Audit<'_> {
    fn record(&self, caller: &Caller, result: &Result<Value>, started: Instant) {
        let who = caller.who.as_ref();
        let key = match who {
            Some(Identity::ApiKey(k)) => Some(k.id.as_str()),
            _ => None,
        };
        let detail = self.detail.map(audit_line);
        let error = result.as_ref().err().map(Error::user_message);
        tracing::info!(
            target: AUDIT_TARGET,
            user = who.map_or("-", Identity::user),
            auth = who.map_or("none", Identity::kind),
            key,
            via = caller.via,
            source = self.source,
            action = self.action,
            database = self.database,
            detail,
            ok = result.is_ok(),
            error,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "数据源操作"
        );
    }
}

/// 压成一行（换行、连续空白换成一个空格），过长截断。
fn audit_line(s: &str) -> String {
    let mut out = String::new();
    for (n, word) in s.split_whitespace().enumerate() {
        if n > 0 {
            out.push(' ');
        }
        out.push_str(word);
    }
    if out.chars().count() > AUDIT_DETAIL_MAX {
        out = out.chars().take(AUDIT_DETAIL_MAX).collect::<String>() + "…";
    }
    out
}

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

/// `?address=` 只列这个连接地址（业务项目配置里的）指向的数据源，库名取 `database` 参数或
/// 地址路径，写在 `matched_database` 里，见 [`crate::datasource::Source::serves`]。
async fn sources(State(state): State<AppState>, p: Params) -> Result<Json<Value>> {
    let all = state.datasources.all().iter();
    let sources: Vec<Value> = match p.get("address") {
        Some(raw) => {
            let addr = Address::parse(raw).ok_or_else(|| {
                Error::bad_request("address 应为 JDBC URL、mysql:// 等连接地址或 host:port")
            })?;
            let db = p.get("database").map(str::to_owned).or(addr.database.clone());
            all.filter(|s| s.serves(&addr))
                .map(|s| {
                    let mut v = s.summary();
                    if let Some(db) = &db {
                        v["matched_database"] = json!(db);
                    }
                    v
                })
                .collect()
        }
        None => all.map(|s| s.summary()).collect(),
    };
    Ok(Json(json!({ "env": state.config.env, "sources": sources })))
}

async fn tables(
    State(state): State<AppState>,
    caller: Caller,
    Path(source): Path<String>,
    p: Params,
) -> Result<Json<Value>> {
    let src = state.datasources.get(&source)?;
    let limit = p.get_u32("limit")?.unwrap_or(200);
    let started = Instant::now();
    let result = src.tables(p.get("database"), p.get("match"), limit).await;
    Audit {
        source: &src.name,
        action: "tables",
        database: p.get("database"),
        detail: p.get("match"),
    }
    .record(&caller, &result, started);
    let mut out = result?;
    out["source"] = json!(src.name);
    Ok(Json(out))
}

async fn describe(
    State(state): State<AppState>,
    caller: Caller,
    Path(source): Path<String>,
    p: Params,
) -> Result<Json<Value>> {
    let src = state.datasources.get(&source)?;
    let target = p.get("target").ok_or_else(|| Error::bad_request("缺少参数 target"))?;
    let started = Instant::now();
    let result = src.describe(target, p.get("database")).await;
    Audit {
        source: &src.name,
        action: "describe",
        database: p.get("database"),
        detail: Some(target),
    }
    .record(&caller, &result, started);
    let mut out = result?;
    out["source"] = json!(src.name);
    Ok(Json(out))
}

async fn query(
    State(state): State<AppState>,
    caller: Caller,
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
    let database = opts.database.clone();
    let started = Instant::now();
    let result = src.query(q, opts).await;
    // 业务库上的查询，事后要能查得到是谁在什么时候查了什么
    Audit { source: &src.name, action: "query", database: database.as_deref(), detail: Some(q) }
        .record(&caller, &result, started);
    let mut out = result?;
    out["source"] = json!(src.name);
    Ok(Json(out))
}

async fn slow(
    State(state): State<AppState>,
    caller: Caller,
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
    let database = opts.database.clone();
    let started = Instant::now();
    let result = src.slow(opts, ctx).await;
    Audit { source: &src.name, action: "slow", database: database.as_deref(), detail: None }
        .record(&caller, &result, started);
    let mut out = result?;
    out["source"] = json!(src.name);
    Ok(Json(out))
}
