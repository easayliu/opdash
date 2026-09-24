//! HTTP 接口。每个页面一个子模块；这里是共享状态、路由拼装和健康检查。

pub mod auth;
pub mod bills;
pub mod datasources;
pub mod logs;
pub mod meta;
pub mod metrics;
pub mod params;
pub mod saved;
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
use crate::saved::SavedQueryStore;
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
    /// 指标名 → 这个指标是什么类型，见 [`metrics::metric_kind`]。
    pub metric_kinds: Arc<MetricKinds>,
    /// 用户收藏的查询（`--saved-query-file`），见 [`crate::saved`]。
    pub saved: Arc<SavedQueryStore>,
    /// goscan 的同步接口（`--goscan-url`）。没配就是 `None`，费用页上不显示「拉取账单」。
    pub goscan: Option<Arc<crate::goscan::Goscan>>,
    /// 成本归属规则（`--bill-alloc`）。没配就是 `None`，费用页的分析视图只给日均与预估。
    pub alloc: Option<Arc<crate::alloc::Alloc>>,
    /// 业务数据源（`--datasources`）。没配就是空的，MCP 里不出现 `db_*` 工具。
    pub datasources: Arc<crate::datasource::Registry>,
}

impl AppState {
    pub fn new(
        config: Config,
        client: Client,
        schema: Arc<SchemaCache>,
        saved: Arc<SavedQueryStore>,
    ) -> Self {
        let tails = Arc::new(Semaphore::new(config.max_tail_streams.max(1)));
        // 建不出客户端（地址离谱）只是没有这个按钮，不该让整个进程起不来
        let goscan = config.goscan_url.as_deref().and_then(|url| match crate::goscan::Goscan::new(
            url,
            config.goscan_timeout,
        ) {
            Ok(g) => Some(Arc::new(g)),
            Err(e) => {
                tracing::warn!(error = %e, url, "连不上 goscan 的配置有问题，手动拉取账单不可用");
                None
            }
        });
        // 文件有误时 main 已带着原因退出（见 crate::alloc），此处只是再读一次，
        // 也让测试仅凭 --bill-alloc 一个参数即可把规则送进来
        let alloc =
            config.bill_alloc.as_deref().and_then(|path| match crate::alloc::Alloc::load(path) {
                Ok(a) => Some(Arc::new(a)),
                Err(e) => {
                    tracing::error!(error = %e, "成本归属规则无效，费用页的业务线分摊已停用");
                    None
                }
            });
        // 同上：文件有误时 main 已经退出，这里读不出来只会是测试里故意给的坏文件
        let datasources = config
            .datasources
            .as_deref()
            .map(|path| match crate::datasource::Registry::load(path) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "数据源配置无效，数据源工具已停用");
                    crate::datasource::Registry::default()
                }
            })
            .unwrap_or_default();
        Self {
            config: Arc::new(config),
            datasources: Arc::new(datasources),
            client,
            schema,
            tails,
            metric_kinds: Arc::new(MetricKinds::default()),
            saved,
            goscan,
            alloc,
        }
    }

    /// 当前时间，unix 毫秒。集中一处方便测试替换。
    pub fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
}

/// 指标类型的小缓存。类型是指标自身的属性（同一个 `metric_name` 不会今天是直方图明天是
/// gauge），但每次查图之前都要问一次；一个服务面板十几块图，不缓存就是十几趟多余的往返。
/// 过期只是为了认得出「表重建、指标换了类型」这种事，正常永远命中。
#[derive(Default)]
pub struct MetricKinds {
    map: parking_lot::Mutex<std::collections::HashMap<String, (std::time::Instant, MetricKind)>>,
}

/// 缓存里存的东西，[`crate::query::metrics::MetricKindRow`] 的一份拷贝。
#[derive(Clone)]
pub struct MetricKind {
    pub metric_type: String,
    pub is_monotonic: u8,
    pub has_bounds: u8,
}

/// 缓存多久。指标类型基本不变，长一点没关系；换了类型最多这么久之后认出来。
const METRIC_KIND_TTL: std::time::Duration = std::time::Duration::from_secs(600);
/// 最多缓存多少个指标名。线上指标名两千上下，超了整个清空重来——这是防内存无限涨的兜底，
/// 不是淘汰策略。
const METRIC_KIND_MAX: usize = 8192;

impl MetricKinds {
    pub fn get(&self, key: &str) -> Option<MetricKind> {
        let map = self.map.lock();
        let (at, kind) = map.get(key)?;
        (at.elapsed() < METRIC_KIND_TTL).then(|| kind.clone())
    }

    pub fn put(&self, key: String, kind: MetricKind) {
        let mut map = self.map.lock();
        if map.len() >= METRIC_KIND_MAX {
            map.clear();
        }
        map.insert(key, (std::time::Instant::now(), kind));
    }
}

/// `/api/*` 路由（不含 `/api/health`）。
pub fn api_router(state: AppState) -> Router {
    Router::new()
        .route("/api/meta", get(meta::meta))
        .merge(logs::routes())
        .merge(tail::routes())
        .merge(traces::routes())
        .merge(metrics::routes())
        .merge(bills::routes())
        .merge(datasources::routes())
        .merge(services::routes())
        .merge(saved::routes())
        .with_state(state)
}

/// 整个应用：API + MCP 端点 + 内嵌前端 + 可选认证（Basic / OIDC）。
///
/// `/api/health` 放在认证外面，k8s 探针不带密码；`/api/auth/*` 也在外面，不然没法登录。
/// `/mcp`（给 AI 助手用的，见 [`crate::mcp`]）和 API 一样在认证里面。
pub fn app(state: AppState, auth: Auth) -> Router {
    let mut protected = api_router(state.clone())
        .merge(crate::mcp::router(state.clone()))
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
