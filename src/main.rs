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
    // 先读配置再起日志：日志时间要按 --timezone 写。参数写错时 clap 自己打印原因退出，不需要日志
    let config = Config::parse();
    let tz = parse_tz(&config.timezone);
    init_logging(*tz.as_ref().unwrap_or(&chrono_tz::UTC));
    config.validate().map_err(anyhow::Error::msg)?;
    tz.map_err(|e| anyhow::anyhow!("--timezone: {e}"))?;

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
        &config.metric_table,
        [
            &config.volcengine_bill_table,
            &config.alicloud_monthly_table,
            &config.alicloud_daily_table,
        ],
    ));
    // 库没起来也照样启动：健康检查会报，后台会一直重试。这样部署顺序不用讲究先后。
    match schema.refresh().await {
        Ok(s) => {
            tracing::info!(
                clickhouse = %config.clickhouse_url,
                server = %s.server_version,
                log_table = %format!("{}.{}", config.database, config.log_table),
                log_columns = s.logs.columns.len(),
                trace_table = %format!("{}.{}", config.database, config.trace_table),
                trace_columns = s.traces.columns.len(),
                metric_columns = s.metrics.as_ref().map_or(0, |t| t.columns.len()),
                "表结构已读取"
            );
            // 指标表是可选的，没有就只是不显示指标页——说一句原因，免得以为是 bug
            if let Some(note) = &s.metrics_note {
                tracing::info!(reason = %note, "指标页未启用");
            }
            // 账单表同理：没部署 goscan 就只是没有费用页
            match (&s.bills, &s.bills_note) {
                (Some(_), note) => {
                    tracing::info!(
                        volcengine = s
                            .bills
                            .as_ref()
                            .and_then(|b| b.volcengine.as_ref())
                            .map(|t| t.table.name.as_str())
                            .unwrap_or("-"),
                        alicloud_monthly = s
                            .bills
                            .as_ref()
                            .and_then(|b| b.alicloud_monthly.as_ref())
                            .map(|t| t.table.name.as_str())
                            .unwrap_or("-"),
                        alicloud_daily = s
                            .bills
                            .as_ref()
                            .and_then(|b| b.alicloud_daily.as_ref())
                            .map(|t| t.table.name.as_str())
                            .unwrap_or("-"),
                        note = note.as_deref().unwrap_or(""),
                        "费用页已启用"
                    );
                }
                (None, Some(note)) => tracing::info!(reason = %note, "费用页未启用"),
                (None, None) => {}
            }
        }
        Err(e) => tracing::warn!(error = %e, "启动时读不到表结构，稍后自动重试"),
    }
    Arc::clone(&schema).spawn_refresher(config.schema_refresh);

    let bind = config.bind;
    let auth = Auth::from_config(&config).map_err(anyhow::Error::msg)?;
    if let Some(store) = auth.key_store() {
        tracing::info!(file = %store.path().display(), "API key 文件已打开（只存哈希；容器里请把它所在目录挂成卷）");
        Arc::clone(store).spawn_flusher(std::time::Duration::from_secs(60));
    }
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
    let saved = Arc::new(
        opdash::saved::SavedQueryStore::open(&config.saved_query_file)
            .map_err(anyhow::Error::msg)?,
    );
    tracing::info!(file = %saved.path().display(), "收藏文件已打开（容器里请把它所在目录挂成卷）");
    // 归属规则有误便不再启动：一条规则写错，页面上的业务线金额即是错的，且从表面看不出来
    if let Some(path) = &config.bill_alloc {
        let alloc = opdash::alloc::Alloc::load(path).map_err(anyhow::Error::msg)?;
        tracing::info!(
            file = %path.display(),
            lines = alloc.lines.len(),
            rules = alloc.rules.len(),
            "成本归属规则已加载"
        );
    }
    // 数据源配置同理：少一个库静默不见，排障时比起不来更难发现
    if let Some(path) = &config.datasources {
        let reg = opdash::datasource::Registry::load(path).map_err(anyhow::Error::msg)?;
        let names: Vec<&str> = reg.all().iter().map(|s| s.name.as_str()).collect();
        tracing::info!(file = %path.display(), sources = ?names, "数据源配置已加载");
    }
    let state = AppState::new(config, client, schema, saved);
    let app = api::app(state, auth.clone());

    let listener =
        tokio::net::TcpListener::bind(bind).await.with_context(|| format!("监听 {bind}"))?;
    tracing::info!(
        "opdash v{} 已启动：http://{bind}{}",
        env!("CARGO_PKG_VERSION"),
        match auth.mode() {
            "oidc" if auth.basic().is_some() => "（OIDC 登录 + Basic 认证）",
            "oidc" => "（OIDC 登录）",
            "basic" => "（已开启 Basic 认证）",
            _ => "",
        }
    );
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    // 内存里攒着的 last_used_at 落盘再走
    if let Some(store) = auth.key_store()
        && let Err(e) = store.flush_if_dirty()
    {
        tracing::warn!(error = %e, "退出前落盘 API key 文件失败");
    }
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
/// 日志时间按 `--timezone` 写，和页面、MCP 工具参数里的时间同一个时区。
///
/// 原来用的是进程的本地时区，容器里没配 `TZ` 就是 UTC：日志记着 08:16 调了工具，参数里却是
/// `at: 12:20`，对照着看要自己换算八小时。也不靠 `TZ` 环境变量——运行时镜像是 debian-slim，
/// 不一定带 tzdata；chrono-tz 把时区数据编进了程序，不依赖镜像。
fn init_logging(tz: chrono_tz::Tz) {
    use std::io::IsTerminal;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt::{format::Writer, time::FormatTime};

    struct ConfiguredZone(chrono_tz::Tz);
    impl FormatTime for ConfiguredZone {
        fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
            let now = chrono::Utc::now().with_timezone(&self.0);
            write!(w, "{}", now.format("%Y-%m-%d %H:%M:%S%.3f"))
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_timer(ConfiguredZone(tz))
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal())
        .init();
}
