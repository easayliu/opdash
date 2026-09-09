//! opdash 可执行入口：解析配置、连 ClickHouse、起 HTTP 服务。

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;

use opdash::api::{self, AppState};
use opdash::auth::Auth;
use opdash::clickhouse::{Client, ClientOptions};
use opdash::config::Config;
use opdash::query::parse_tz;
use opdash::schema::SchemaCache;

fn main() -> anyhow::Result<()> {
    // 要在起任何线程之前改环境变量（remove_var 的 unsafe 就是怕别的线程同时在读），
    // 所以不用 #[tokio::main]，先清理再手动建 runtime
    strip_empty_env();
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run())
}

/// 值为空的 `OPDASH_*` 环境变量等于没配。k8s 的 Secret / compose 的 `${VAR:-}` 留空时传进来的是
/// 空串，clap 会原样交给解析器，`--basic-auth ""` 这种就会报格式错误、起不来。
fn strip_empty_env() {
    let empty: Vec<String> = std::env::vars_os()
        .filter_map(|(k, v)| {
            let k = k.into_string().ok()?;
            (k.starts_with("OPDASH_") && v.is_empty()).then_some(k)
        })
        .collect();
    for k in empty {
        // SAFETY: 在 main 最开头、还没起任何线程时调用，没有并发的 getenv
        unsafe { std::env::remove_var(k) };
    }
}

async fn run() -> anyhow::Result<()> {
    init_logging();
    let config = Config::parse();
    config.validate().map_err(anyhow::Error::msg)?;
    parse_tz(&config.timezone).map_err(|e| anyhow::anyhow!("--timezone: {e}"))?;

    let client = Client::new(ClientOptions {
        endpoint: config.clickhouse_url.clone(),
        user: config.clickhouse_user.clone(),
        password: config.clickhouse_password.clone(),
        timeout: config.query_timeout,
        max_read_bytes: config.max_read_bytes,
        max_read_rows: config.max_read_rows,
        max_concurrent: config.max_concurrent_queries,
    })
    .context("初始化 ClickHouse 客户端")?;

    let schema = Arc::new(SchemaCache::new(
        client.clone(),
        &config.database,
        &config.log_table,
        &config.trace_table,
    ));
    // 库没起来也照样启动：健康检查会报，后台会一直重试。这样部署顺序不用讲究先后。
    match schema.refresh().await {
        Ok(s) => tracing::info!(
            clickhouse = %config.clickhouse_url,
            server = %s.server_version,
            log_table = %format!("{}.{}", config.database, config.log_table),
            log_columns = s.logs.columns.len(),
            trace_table = %format!("{}.{}", config.database, config.trace_table),
            trace_columns = s.traces.columns.len(),
            "表结构已读取"
        ),
        Err(e) => tracing::warn!(error = %e, "启动时读不到表结构，稍后自动重试"),
    }
    Arc::clone(&schema).spawn_refresher(config.schema_refresh);

    let bind = config.bind;
    let auth = Auth::from_config(&config);
    if let Some(oidc) = auth.oidc() {
        // 和表结构一样：Keycloak 没起来也照样启动，登录时再试
        match oidc.discover().await {
            Ok(d) => tracing::info!(issuer = %d.issuer, client_id = %oidc.client_id, "OIDC 已就绪"),
            Err(e) => tracing::warn!(error = %e, "启动时连不上 OIDC 提供方，登录时再试"),
        }
        if config.session_secret.is_none() {
            tracing::info!("没配 --session-secret，会话密钥随机生成：重启后需要重新登录");
        }
    }
    let state = AppState::new(config, client, schema);
    let app = api::app(state, auth.clone());

    let listener =
        tokio::net::TcpListener::bind(bind).await.with_context(|| format!("监听 {bind}"))?;
    tracing::info!(
        "opdash v{} 已启动: http://{bind}{}",
        env!("CARGO_PKG_VERSION"),
        match auth.mode() {
            "oidc" if auth.basic().is_some() => "（OIDC 登录 + Basic 认证）",
            "oidc" => "（OIDC 登录）",
            "basic" => "（已开启 Basic 认证）",
            _ => "",
        }
    );
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    tracing::info!("已退出");
    Ok(())
}

/// Ctrl-C 或 SIGTERM 都算退出信号；axum 收到后停止接新连接，手上的请求做完再退。
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!(error = %e, "监听 Ctrl-C 失败");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "监听 SIGTERM 失败");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => tracing::info!("收到 Ctrl-C，开始退出"),
        _ = terminate => tracing::info!("收到 SIGTERM，开始退出"),
    }
}

/// 本地时间、不打 target、非终端自动关颜色。默认 info，`RUST_LOG=opdash=debug` 能看到每条 SQL。
fn init_logging() {
    use std::io::IsTerminal;
    use tracing_subscriber::{EnvFilter, fmt::time::ChronoLocal};
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_timer(ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f".to_owned()))
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal())
        .init();
}
