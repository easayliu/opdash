//! MCP 的数据库相关工具：链路里的数据库调用、业务数据源（`--datasources`）、环境标识（`--env`）。
//!
//! ClickHouse 和 Elasticsearch 都走 HTTP，拿假 ClickHouse（只会按顺序回放响应的 HTTP 服务）
//! 就能冒充；MySQL / Redis 是各自的二进制协议，这里只验证不连库也能走到的那部分（目录、配置、
//! 只读校验），协议层的转换在 `src/datasource/` 的单元测试里。

mod support;

use axum::body::Body;
use serde_json::{Value, json};
use support::*;

/// Asia/Shanghai 的 2026-01-01 00:00:00。
const MIDNIGHT_MS: i64 = 1_767_196_800_000;
const TRACE: &str = "1719ae16e40aca0266d306bb381c8dbc";

async fn rpc_with(app: &axum::Router, body: Value, headers: &[(&str, &str)]) -> Value {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    assert_eq!(resp.status(), 200);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

async fn call_tool(app: &axum::Router, name: &str, arguments: Value) -> (bool, Value) {
    let body = rpc_with(
        app,
        request(7, "tools/call", json!({ "name": name, "arguments": arguments })),
        &[],
    )
    .await;
    let result = &body["result"];
    assert!(body.get("error").is_none(), "工具调用不该变成 JSON-RPC 错误: {body}");
    let text = result["content"][0]["text"].as_str().expect("content[0].text");
    let parsed = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
    (result["isError"].as_bool().unwrap_or(false), parsed)
}

async fn tool_names(app: &axum::Router) -> Vec<String> {
    let body = rpc_with(app, request(1, "tools/list", json!({})), &[]).await;
    body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

/// 写一份数据源配置到临时文件，返回路径。
fn datasource_file(body: &str) -> String {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "opdash-test-datasources-{}-{}.toml",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&path, body).expect("写数据源配置");
    path.to_string_lossy().into_owned()
}

/// 一套数据源：一个 MySQL（不会真的连）、一个指向假服务的 ClickHouse、一个指向假服务的 ES。
fn sources(ch: &str, es: &str) -> String {
    format!(
        r#"
[[source]]
name = "order-db"
kind = "mysql"
url = "mysql://reader@10.0.0.5:3306/shop"
description = "订单库"
services = ["order-service"]
aliases = ["rm-demo.mysql.example.com"]

[[source]]
name = "biz-ck"
kind = "clickhouse"
url = "{ch}"
database = "biz"
max_rows = 2

[[source]]
name = "search"
kind = "elasticsearch"
url = "{es}"
env = "测试"
"#
    )
}

fn located(span_id: &str, name: &str, ts_ms: i64) -> String {
    format!(
        "{{\"span_id\":\"{span_id}\",\"service_name\":\"order-service\",\"span_name\":\"{name}\",\"ts_ms\":{ts_ms}}}\n"
    )
}

fn db_row(span_id: &str, start_us: i64, duration_ns: i64, stmt: &str, params: &str) -> String {
    json!({
        "span_id": span_id,
        "parent_span_id": "aaaaaaaaaaaaaaaa",
        "service_name": "order-service",
        "span_name": "SELECT shop.t_order",
        "start_us": start_us,
        "duration_ns": duration_ns,
        "status_code": "Unset",
        "status_message": "",
        "db_system": "mysql",
        "db_name": "shop",
        "db_operation": "SELECT",
        "db_collection": "t_order",
        "db_server": "rm-demo.mysql.example.com",
        "db_port": "3306",
        "statement": stmt,
        "params": params,
    })
    .to_string()
        + "\n"
}

#[tokio::test]
async fn env_label_goes_into_the_handshake() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &["--env", "生产"]).await;
    let body =
        rpc_with(&app, request(1, "initialize", json!({})), &[("host", "opdash.prod.example.com")])
            .await;
    let r = &body["result"];
    assert!(r["serverInfo"]["title"].as_str().unwrap().contains("生产"), "{r}");
    let text = r["instructions"].as_str().unwrap();
    assert!(text.starts_with("【环境：生产】"), "环境是说明的第一句: {text}");
    assert!(text.contains("opdash.prod.example.com"), "{text}");

    // 没配环境名：只说是哪个域名
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    let body =
        rpc_with(&app, request(1, "initialize", json!({})), &[("host", "opdash.test.example.com")])
            .await;
    let text = body["result"]["instructions"].as_str().unwrap();
    assert!(text.starts_with("本 MCP 连接的是 opdash.test.example.com"), "{text}");
    assert_eq!(body["result"]["serverInfo"]["title"], "opdash · 日志 / 链路 / 指标查询");
}

#[tokio::test]
async fn db_tools_appear_only_with_datasources() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    let names = tool_names(&app).await;
    assert!(!names.iter().any(|n| n == "db_query"), "{names:?}");
    assert!(names.iter().any(|n| n == "trace_db_calls"), "从链路看 SQL 不依赖数据源");
    assert!(names.iter().any(|n| n == "db_calls_top"));

    let file = datasource_file(&sources("http://127.0.0.1:1", "http://127.0.0.1:1"));
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &["--datasources", &file, "--env", "测试"]).await;
    let names = tool_names(&app).await;
    for want in ["db_sources", "db_tables", "db_describe", "db_query", "db_slow_queries"] {
        assert!(names.iter().any(|n| n == want), "缺 {want}: {names:?}");
    }
    let (err, out) = call_tool(&app, "db_sources", json!({})).await;
    assert!(!err, "{out}");
    assert_eq!(out["env"], "测试");
    let list = out["sources"].as_array().unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0]["name"], "order-db");
    assert_eq!(list[0]["address"], "10.0.0.5:3306");
    assert_eq!(list[2]["env"], "测试");
    // 业务项目配置里的 JDBC URL 原样传进来，按地址 / aliases 对到数据源，库名从 URL 里取
    let (err, out) = call_tool(
        &app,
        "db_sources",
        json!({ "address": "jdbc:mysql://rm-demo.mysql.example.com:3306/coupon?useSSL=false" }),
    )
    .await;
    assert!(!err, "{out}");
    let list = out["sources"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{out}");
    assert_eq!(list[0]["name"], "order-db");
    assert_eq!(list[0]["matched_database"], "coupon");
    assert!(out.get("all_sources").is_none());
    // 对不上时附上全部数据源，让模型自己认内网 / 公网的另一种写法
    let (err, out) =
        call_tool(&app, "db_sources", json!({ "address": "rm-other.example.com:3306" })).await;
    assert!(!err, "{out}");
    assert!(out["sources"].as_array().unwrap().is_empty());
    assert_eq!(out["all_sources"].as_array().unwrap().len(), 3);
    assert!(out.to_string().contains("aliases"), "{out}");
    // 说明里直接列出数据源，省一次调用
    let body = rpc_with(&app, request(1, "initialize", json!({})), &[]).await;
    let text = body["result"]["instructions"].as_str().unwrap();
    assert!(text.contains("order-db（mysql，order-service）"), "{text}");

    // 写语句在到达库之前就被拒掉；没有这个数据源时列出可用的
    let (err, out) = call_tool(
        &app,
        "db_query",
        json!({ "source": "order-db", "query": "DELETE FROM t_order" }),
    )
    .await;
    assert!(err);
    assert!(out.as_str().unwrap().contains("DELETE"), "{out}");
    let (err, out) =
        call_tool(&app, "db_query", json!({ "source": "nope", "query": "SELECT 1" })).await;
    assert!(err);
    assert!(out.as_str().unwrap().contains("order-db"), "{out}");
}

#[tokio::test]
async fn trace_db_calls_fills_params_matches_sources_and_flags_repeats() {
    let fake = FakeClickhouse::start().await;
    let file = datasource_file(&sources("http://127.0.0.1:1", "http://127.0.0.1:1"));
    let app = app_with_schema(&fake, &["--datasources", &file]).await;
    let us = MIDNIGHT_MS * 1000;
    let stmt = "select * from t_order where id = ?";
    fake.respond(format!(
        "{}{}{}{}",
        located("aaaaaaaaaaaaaaaa", "GET /orders", MIDNIGHT_MS),
        located("b000000000000001", "SELECT shop.t_order", MIDNIGHT_MS + 1),
        located("b000000000000002", "SELECT shop.t_order", MIDNIGHT_MS + 2),
        located("b000000000000003", "SELECT shop.t_order", MIDNIGHT_MS + 3),
    ))
    .respond(format!(
        "{}{}{}",
        db_row("b000000000000001", us + 1000, 3_000_000, stmt, r#"{"0":"101"}"#),
        db_row("b000000000000002", us + 2000, 5_000_000, stmt, r#"{"0":"102"}"#),
        db_row("b000000000000003", us + 3000, 2_000_000, stmt, "{}"),
    ));
    let (err, out) = call_tool(
        &app,
        "trace_db_calls",
        json!({ "trace_id": TRACE, "at": "2026-01-01T00:00:00+08:00" }),
    )
    .await;
    assert!(!err, "{out}");
    assert_eq!(out["call_count"], 3);
    assert_eq!(out["total_db_ms"], 10.0);
    let calls = out["calls"].as_array().unwrap();
    assert_eq!(calls[0]["statement_filled"], "select * from t_order where id = '101'");
    assert_eq!(calls[0]["source"], "order-db", "按 server.address 对上配置里的别名");
    assert_eq!(calls[0]["server"], "rm-demo.mysql.example.com:3306");
    assert_eq!(calls[0]["duration_ms"], 3.0);
    assert!(calls[2].get("statement_filled").is_none(), "没有参数就不代入");
    assert_eq!(out["repeated"][0]["times"], 3, "同一条语句执行三次，标出来");

    let reqs = fake.requests();
    let sql = &reqs.last().unwrap().body;
    assert!(sql.contains("span_kind = 'Client'"), "{sql}");
    assert!(sql.contains("span_attributes.^db.query.parameter"), "{sql}");
    assert!(
        sql.contains("db.query.text") && sql.contains("db.statement"),
        "新旧两套属性名都认: {sql}"
    );
}

#[tokio::test]
async fn trace_db_calls_falls_back_when_params_cannot_be_read() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    fake.respond(located("b000000000000001", "SELECT shop.t_order", MIDNIGHT_MS))
        .respond_error(62, "Syntax error: failed at position 300 ('^')")
        .respond(db_row("b000000000000001", MIDNIGHT_MS * 1000, 1_000_000, "select 1", ""));
    let (err, out) = call_tool(
        &app,
        "trace_db_calls",
        json!({ "trace_id": TRACE, "at": "2026-01-01T00:00:00+08:00" }),
    )
    .await;
    assert!(!err, "{out}");
    assert_eq!(out["call_count"], 1);
    assert!(out["notes"].to_string().contains("JDBC 参数"), "{out}");
    assert!(out["calls"][0].get("source").is_none(), "没配数据源就不标");
    let last = fake.requests().last().unwrap().body.clone();
    assert!(!last.contains("db.query.parameter"), "退回的那次不带参数: {last}");
}

#[tokio::test]
async fn db_calls_top_groups_by_statement() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    fake.respond(
        json!({
            "service_name": "order-service", "db_system": "mysql", "db_name": "shop",
            "db_operation": "SELECT", "db_collection": "t_order", "db_server": "10.0.0.5", "db_port": "3306",
            "statement": "select * from t_order where user_id = ?",
            "calls": 120, "errors": 0, "p50_ns": 2_000_000.0, "p95_ns": 80_000_000.0,
            "max_ns": 150_000_000, "total_ns": 1_200_000_000,
            "sample_trace": TRACE, "sample_span": "b000000000000001", "sample_ms": MIDNIGHT_MS,
        })
        .to_string()
            + "\n",
    );
    let (err, out) =
        call_tool(&app, "db_calls_top", json!({ "service": "order-service", "sort": "p95" })).await;
    assert!(!err, "{out}");
    let g = &out["groups"][0];
    assert_eq!(g["calls"], 120);
    assert_eq!(g["p95_ms"], 80.0);
    assert_eq!(g["total_ms"], 1200.0);
    assert_eq!(g["server"], "10.0.0.5:3306");
    assert!(g.get("errors").is_none(), "0 个错误就不列");
    let sql = fake.last_request().body;
    assert!(sql.contains("ORDER BY p95_ns DESC"), "{sql}");
    assert!(sql.contains("GROUP BY service_name, db_system"), "{sql}");

    let (err, out) = call_tool(&app, "db_calls_top", json!({ "sort": "slowest" })).await;
    assert!(err, "{out}");
}

#[tokio::test]
async fn clickhouse_source_runs_read_only_with_limits() {
    let fake = FakeClickhouse::start().await;
    let biz = FakeClickhouse::start().await;
    let file = datasource_file(&sources(biz.endpoint(), "http://127.0.0.1:1"));
    let app = app_with_schema(&fake, &["--datasources", &file]).await;
    biz.respond(
        json!({
            "meta": [{ "name": "id", "type": "UInt64" }, { "name": "status", "type": "String" }],
            "data": [[1, "PAID"], [2, "NEW"], [3, "NEW"]],
            "rows": 3,
        })
        .to_string(),
    );
    let (err, out) = call_tool(
        &app,
        "db_query",
        json!({ "source": "biz-ck", "query": "SELECT id, status FROM orders;", "limit": 50 }),
    )
    .await;
    assert!(!err, "{out}");
    assert_eq!(out["columns"], json!(["id", "status"]));
    assert_eq!(out["rows"].as_array().unwrap().len(), 2, "数据源 max_rows = 2 封顶");
    assert!(out["notes"].to_string().contains("只返回了前 2 行"), "{out}");
    let req = biz.last_request();
    assert_eq!(req.query_value("readonly").as_deref(), Some("2"));
    assert_eq!(req.query_value("max_result_rows").as_deref(), Some("3"));
    assert_eq!(req.query_value("database").as_deref(), Some("biz"));
    assert!(req.body.trim_end().ends_with("FORMAT JSONCompact"), "{}", req.body);
    assert!(!req.body.contains(';'), "结尾分号去掉了: {}", req.body);

    // 读别处数据的表函数不放行，也不会发到库上
    let before = biz.requests().len();
    let (err, _) = call_tool(
        &app,
        "db_query",
        json!({ "source": "biz-ck", "query": "SELECT * FROM url('http://10.0.0.1/', CSV)" }),
    )
    .await;
    assert!(err);
    assert_eq!(biz.requests().len(), before);

    // 库报错原样说明
    biz.respond_error(60, "Unknown table expression identifier 'nope'");
    let (err, out) =
        call_tool(&app, "db_query", json!({ "source": "biz-ck", "query": "SELECT * FROM nope" }))
            .await;
    assert!(err);
    assert!(out.as_str().unwrap().contains("ClickHouse 错误 60"), "{out}");

    // 表列表：嵌套的表格摊平成对象数组
    biz.respond(
        json!({
            "meta": [{ "name": "database" }, { "name": "table" }, { "name": "total_rows" }],
            "data": [["biz", "orders", 10]],
        })
        .to_string(),
    );
    let (err, out) =
        call_tool(&app, "db_tables", json!({ "source": "biz-ck", "match": "ord" })).await;
    assert!(!err, "{out}");
    assert_eq!(out["tables"][0]["table"], "orders");
    let req = biz.last_request();
    assert_eq!(req.query_value("param_like").as_deref(), Some("%ord%"));
}

#[tokio::test]
async fn clickhouse_source_stops_reading_an_oversized_result() {
    let fake = FakeClickhouse::start().await;
    let biz = FakeClickhouse::start().await;
    let file = datasource_file(&sources(biz.endpoint(), "http://127.0.0.1:1"));
    let app = app_with_schema(&fake, &["--datasources", &file]).await;
    // 库没照 max_result_bytes 截断（比如设置被改掉了）时，opdash 自己也不能把结果全吃进内存
    let big = "x".repeat(70 << 20);
    biz.respond(format!(r#"{{"meta":[{{"name":"s","type":"String"}}],"data":[["{big}"]]}}"#));
    let (err, out) =
        call_tool(&app, "db_query", json!({ "source": "biz-ck", "query": "SELECT s FROM t" }))
            .await;
    assert!(err, "{out}");
    assert!(out.as_str().unwrap().contains("MB 上限"), "{out}");
}

#[tokio::test]
async fn elasticsearch_source_caps_size_and_only_hits_read_endpoints() {
    let fake = FakeClickhouse::start().await;
    let es = FakeClickhouse::start().await;
    let file = datasource_file(&sources("http://127.0.0.1:1", es.endpoint()));
    let app = app_with_schema(&fake, &["--datasources", &file]).await;
    es.respond(
        json!({
            "took": 3, "timed_out": false, "_shards": { "total": 1 },
            "hits": { "total": { "value": 1, "relation": "eq" }, "hits": [
                { "_index": "order-1", "_id": "9", "_score": 1.0, "_source": { "status": "PAID" } }
            ] }
        })
        .to_string(),
    );
    let (err, out) = call_tool(
        &app,
        "db_query",
        json!({ "source": "search", "index": "order-*", "query": "{\"size\": 100000, \"query\": {\"term\": {\"status\": \"PAID\"}}}" }),
    )
    .await;
    assert!(!err, "{out}");
    assert_eq!(out["hits"][0]["_source"]["status"], "PAID");
    assert!(out.get("_shards").is_none());
    let req = es.last_request();
    assert!(req.target.starts_with("/order-*/_search"), "{}", req.target);
    let sent: Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(sent["size"], 50, "size 被压到 limit 以内");
    assert_eq!(sent["timeout"], "15s");

    // 索引名拼不出别的接口
    let (err, _) = call_tool(
        &app,
        "db_query",
        json!({ "source": "search", "index": "x/_delete_by_query", "query": "{}" }),
    )
    .await;
    assert!(err);

    // 不以 { 开头的是 ES SQL
    es.respond(json!({ "columns": [{ "name": "c" }], "rows": [[7]] }).to_string());
    let (err, out) = call_tool(
        &app,
        "db_query",
        json!({ "source": "search", "query": "SELECT count(*) AS c FROM \"order-1\"" }),
    )
    .await;
    assert!(!err, "{out}");
    assert_eq!(out["rows"], json!([[7]]));
    assert!(es.last_request().target.starts_with("/_sql"), "{}", es.last_request().target);
}
