//! HTTP 接口。每个页面一个子模块；这里是共享状态、路由拼装和健康检查。

pub mod auth;
pub mod logs;
pub mod meta;
pub mod params;
pub mod services;
pub mod traces;

use std::sync::Arc;

use axum::{Router, middleware, routing::get};
use tower_http::trace::TraceLayer;

use crate::clickhouse::Client;
use crate::config::{BasicAuth, Config};
use crate::schema::SchemaCache;
use crate::ui;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub client: Client,
    pub schema: Arc<SchemaCache>,
}

impl AppState {
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
        .merge(traces::routes())
        .merge(services::routes())
        .with_state(state)
}

/// 整个应用：API + 内嵌前端 + 可选 Basic 认证。
///
/// `/api/health` 放在认证外面，k8s 探针不带密码。
pub fn app(state: AppState, auth: Option<&BasicAuth>) -> Router {
    let mut protected = api_router(state.clone())
        .route("/", get(ui::fallback).post(ui::redirect_root_post))
        .fallback_service(get(ui::fallback));
    if let Some(auth) = auth {
        protected = protected.layer(middleware::from_fn_with_state(auth.clone(), auth::require));
    }
    Router::new()
        .route("/api/health", get(meta::health))
        .with_state(state)
        .merge(protected)
        .layer(ui::compression())
        .layer(TraceLayer::new_for_http())
}
