//! HTTP 接口。每个页面一个子模块；这里是共享状态、路由拼装和健康检查。

pub mod auth;
pub mod logs;
pub mod meta;
pub mod params;
pub mod services;
pub mod tail;
pub mod traces;

use std::sync::Arc;

use axum::{Router, middleware, routing::get};
use tokio::sync::Semaphore;
use tower_http::trace::TraceLayer;

use crate::auth::{self as authn, Auth};
use crate::clickhouse::Client;
use crate::config::Config;
use crate::schema::SchemaCache;
use crate::ui;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub client: Client,
    pub schema: Arc<SchemaCache>,
    /// 跟随连接的名额。每条连接在整个生命周期里都握着一个，长期占着 ClickHouse 的查询频率，
    /// 和单条查询的并发（[`Client`] 自己的信号量）不是一回事，所以单独限。
    pub tails: Arc<Semaphore>,
}

impl AppState {
    pub fn new(config: Config, client: Client, schema: Arc<SchemaCache>) -> Self {
        let tails = Arc::new(Semaphore::new(config.max_tail_streams.max(1)));
        Self { config: Arc::new(config), client, schema, tails }
    }

    /// 当前时间，unix 毫秒。集中一处方便测试替换。
    pub fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
}

/// `/api/*` 路由（不含 `/api/health`）。
pub fn api_router(state: AppState) -> Router {
    Router::new()
        .route("/api/meta", get(meta::meta))
        .merge(logs::routes())
        .merge(tail::routes())
        .merge(traces::routes())
        .merge(services::routes())
        .with_state(state)
}

/// 整个应用：API + 内嵌前端 + 可选认证（Basic / OIDC）。
///
/// `/api/health` 放在认证外面，k8s 探针不带密码；`/api/auth/*` 也在外面，不然没法登录。
pub fn app(state: AppState, auth: Auth) -> Router {
    let mut protected = api_router(state.clone())
        .route("/", get(ui::fallback).post(ui::redirect_root_post))
        .fallback_service(get(ui::fallback));
    if auth.enabled() {
        protected = protected.layer(middleware::from_fn_with_state(auth.clone(), authn::require));
    }
    Router::new()
        .route("/api/health", get(meta::health))
        .with_state(state)
        .merge(auth::routes(auth))
        .merge(protected)
        .layer(ui::compression())
        .layer(TraceLayer::new_for_http())
}
