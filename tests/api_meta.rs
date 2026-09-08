//! `/api/meta`、`/api/health`、Basic 认证。

mod support;

use support::*;

#[tokio::test]
async fn meta_lists_dimensions_from_system_columns() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;

    let (status, meta) = get_json(&app, "/api/meta").await;
    assert_eq!(status, 200, "{meta}");
    assert_eq!(meta["database"], "logs");
    assert_eq!(meta["logs"]["table"], "app_log");
    assert_eq!(
        meta["logs"]["dimensions"],
        serde_json::json!(["service_name", "namespace", "pod", "container", "stream", "cluster"])
    );
    assert_eq!(meta["traces"]["dimensions"], serde_json::json!(["cluster"]));
    assert_eq!(meta["server"]["version"], "24.8.1.1");
    assert_eq!(meta["limits"]["max_rows"], 1000);
    assert!(meta["now_ms"].as_i64().unwrap() > 1_700_000_000_000);

    // 读表结构的那个请求：只读、参数绑定、认证头
    let first = &fake.requests()[0];
    assert!(first.body.contains("system.columns"), "{}", first.body);
    assert_eq!(first.query_value("readonly").as_deref(), Some("2"));
    assert_eq!(first.query_value("param_db").as_deref(), Some("logs"));
    assert_eq!(first.query_value("param_logs").as_deref(), Some("app_log"));
    assert_eq!(first.query_value("wait_end_of_query").as_deref(), Some("1"));
    // 24.8 上这个设置默认是 1，UInt64 会变成 JSON 字符串；必须显式关掉
    assert_eq!(first.query_value("output_format_json_quote_64bit_integers").as_deref(), Some("0"));
    assert_eq!(
        first.query_value("cancel_http_readonly_queries_on_client_close").as_deref(),
        Some("1")
    );
    assert_eq!(first.header("x-clickhouse-user").as_deref(), Some("default"));
    assert!(first.header("x-clickhouse-key").is_none(), "空密码不该发 key 头");
    assert!(first.body.ends_with("FORMAT JSONEachRow"), "{}", first.body);
}

#[tokio::test]
async fn meta_fails_clearly_when_table_is_missing() {
    let fake = FakeClickhouse::start().await;
    // 只回日志表的列，没有 otel_trace
    let only_logs: String = columns_fixture()
        .lines()
        .filter(|l| l.contains("app_log"))
        .map(|l| format!("{l}\n"))
        .collect();
    fake.respond(only_logs).respond(version_fixture());
    let app = app(&fake, &[]).await;

    let (status, body) = get_json(&app, "/api/meta").await;
    assert_eq!(status, 500);
    assert!(body["error"].as_str().unwrap().contains("otel_trace"), "{body}");
}

#[tokio::test]
async fn health_reports_clickhouse_down() {
    // 端口 9（discard）本机没人听，连接会被拒
    let app = app_at("http://127.0.0.1:9", &[]).await;
    let (status, body) = get_json(&app, "/api/health").await;
    assert_eq!(status, 503);
    assert_eq!(body["ok"], false);
    assert_ne!(body["clickhouse"], "ok");
    assert_ne!(body["schema"], "ok");
}

#[tokio::test]
async fn health_ok_when_ping_and_schema_work() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    let _ = get_json(&app, "/api/meta").await; // 触发表结构读取
    fake.respond("1\n");
    let (status, body) = get_json(&app, "/api/health").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["schema"], "ok");
    assert_eq!(body["clickhouse"], "ok");
}

#[tokio::test]
async fn basic_auth_guards_everything_but_health() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &["--basic-auth", "ops:secret"]).await;

    let (status, body) = get_raw(&app, "/api/meta", &[]).await;
    assert_eq!(status, 401, "{}", String::from_utf8_lossy(&body));
    let (status, _) = get_raw(&app, "/", &[]).await;
    assert_eq!(status, 401);

    // base64("ops:secret")
    let (status, _) =
        get_raw(&app, "/api/meta", &[("authorization", "Basic b3BzOnNlY3JldA==")]).await;
    assert_eq!(status, 200);
    let (status, _) = get_raw(&app, "/api/meta", &[("authorization", "Basic b3BzOndyb25n")]).await;
    assert_eq!(status, 401);

    fake.respond("1\n");
    let (status, _) = get_raw(&app, "/api/health", &[]).await;
    assert_eq!(status, 200, "健康检查不该要密码");
}
