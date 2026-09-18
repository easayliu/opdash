//! `/mcp`：JSON-RPC 握手、工具目录、工具调用怎么翻译成 API 查询、认证和 Origin 校验。

mod support;

use axum::body::Body;
use serde_json::{Value, json};
use support::*;

/// Asia/Shanghai 的 2026-01-01 00:00:00。
const MIDNIGHT_MS: i64 = 1_767_196_800_000;
const HOUR_MS: i64 = 3_600_000;
const TRACE: &str = "1719ae16e40aca0266d306bb381c8dbc";

async fn post_full(
    app: &axum::Router,
    body: &Value,
    headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let mut req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let response =
        app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    let bytes = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, headers, bytes)
}

async fn rpc(app: &axum::Router, body: Value) -> (u16, Value) {
    let (status, _, bytes) = post_full(app, &body, &[]).await;
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// 调一个工具，拿回 (isError, 工具结果文本解成的 JSON)。
async fn call_tool(app: &axum::Router, name: &str, arguments: Value) -> (bool, Value) {
    let (status, body) =
        rpc(app, request(7, "tools/call", json!({ "name": name, "arguments": arguments }))).await;
    assert_eq!(status, 200, "{body}");
    let result = &body["result"];
    assert!(body.get("error").is_none(), "工具调用不该变成 JSON-RPC 错误: {body}");
    let text = result["content"][0]["text"].as_str().expect("content[0].text");
    let is_error = result["isError"].as_bool().unwrap_or(false);
    let parsed = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
    (is_error, parsed)
}

/// 建表结构那两条不算。
fn sql(fake: &FakeClickhouse) -> Vec<String> {
    fake.requests().into_iter().skip(2).map(|r| r.body).collect()
}

#[tokio::test]
async fn handshake_lists_tools_and_answers_ping() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;

    let (status, headers, bytes) = post_full(
        &app,
        &request(
            1,
            "initialize",
            json!({ "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "t", "version": "0" } }),
        ),
        &[],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(header_value(&headers, "content-type"), Some("application/json"));
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["id"], 1);
    let r = &body["result"];
    assert_eq!(r["protocolVersion"], "2025-03-26", "客户端要的版本我们支持就照它的回");
    assert_eq!(r["serverInfo"]["name"], "opdash");
    assert!(r["capabilities"]["tools"].is_object());
    let instructions = r["instructions"].as_str().unwrap();
    // 表结构已经读到：维度列名写进说明里，省一次 get_meta
    assert!(instructions.contains("namespace, pod"), "{instructions}");
    assert!(instructions.contains("Asia/Shanghai"), "{instructions}");

    // 不认识的版本退回我们的
    let (_, body) =
        rpc(&app, request(2, "initialize", json!({ "protocolVersion": "1999-01-01" }))).await;
    assert_eq!(body["result"]["protocolVersion"], opdash::mcp::PROTOCOL_VERSION);

    let (status, body) = rpc(&app, request(3, "tools/list", json!({}))).await;
    assert_eq!(status, 200);
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in [
        "get_meta",
        "service_overview",
        "error_groups",
        "search_traces",
        "get_trace",
        "search_logs",
        "list_attrs",
        "query_metric",
        "metric_exemplars",
    ] {
        assert!(names.contains(&want), "缺工具 {want}: {names:?}");
    }
    // 只读标注：客户端据此免掉每次调用的确认
    let first = &body["result"]["tools"][0];
    assert_eq!(first["annotations"]["readOnlyHint"], true, "{first}");

    let (_, body) = rpc(&app, request(4, "ping", json!({}))).await;
    assert_eq!(body["result"], json!({}));

    // 没声明的能力按协议回「没有这个方法」
    let (status, body) = rpc(&app, request(5, "resources/list", json!({}))).await;
    assert_eq!(status, 200);
    assert_eq!(body["error"]["code"], -32601);

    // 通知不回内容
    let (status, _, bytes) =
        post_full(&app, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), &[])
            .await;
    assert_eq!(status, 202);
    assert!(bytes.is_empty());

    // 一批消息：两个请求一个通知 → 两个响应
    let (status, body) = rpc(
        &app,
        json!([request(10, "ping", json!({})), { "jsonrpc": "2.0", "method": "notifications/x" }, request(11, "ping", json!({}))]),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body.as_array().map(Vec::len), Some(2), "{body}");

    // 坏 JSON
    let (status, _, bytes) = post_full(&app, &json!("{not json"), &[]).await;
    // json!("...") 会编码成一个字符串字面量，是合法 JSON 但不是对象 / 数组
    assert_eq!(status, 200);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], -32600);

    // 这一路没有碰过库
    assert!(sql(&fake).is_empty());
}

#[tokio::test]
async fn transport_rejects_streams_and_foreign_origins() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;

    let (status, headers, _) = get_full(&app, "/mcp", &[("accept", "text/event-stream")]).await;
    assert_eq!(status, 405);
    assert_eq!(header_value(&headers, "allow"), Some("POST"));

    let ping = request(1, "ping", json!({}));
    let (status, _, _) = post_full(
        &app,
        &ping,
        &[("host", "opdash.example.com"), ("origin", "https://evil.example")],
    )
    .await;
    assert_eq!(status, 403);
    let (status, _, _) = post_full(
        &app,
        &ping,
        &[("host", "opdash.example.com"), ("origin", "https://opdash.example.com")],
    )
    .await;
    assert_eq!(status, 200);
    let (status, _, _) = post_full(&app, &ping, &[("host", "opdash.example.com")]).await;
    assert_eq!(status, 200, "命令行客户端不带 Origin");
}

#[tokio::test]
async fn mcp_takes_a_user_issued_api_key() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &["--basic-auth", "ai:secret"]).await;
    let ping = request(1, "ping", json!({}));
    let (status, _, _) = post_full(&app, &ping, &[]).await;
    assert_eq!(status, 401);

    // 登录用户（这里用 Basic 代替 OIDC 会话）签一把 key，MCP 客户端带 Bearer
    let (status, key) = support::post_json(
        &app,
        "/api/auth/keys",
        r#"{"name":"claude-code"}"#,
        &[("authorization", "Basic YWk6c2VjcmV0")],
    )
    .await;
    assert_eq!(status, 200, "{key}");
    let bearer = format!("Bearer {}", key["key"].as_str().unwrap());
    let (status, _, _) = post_full(&app, &ping, &[("authorization", &bearer)]).await;
    assert_eq!(status, 200);
    let (status, _, _) =
        post_full(&app, &ping, &[("authorization", "Bearer opdash_nope.nope")]).await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn search_logs_translates_into_the_logs_api() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    // 检索 + count 并发，同一个响应体两边都能解
    let row = concat!(
        r#"{"ts_ms":1767196800123,"level":"ERROR","trace_id":"","span_id":"0102030405060708","thread":"main","#,
        r#""logger":"a.B","message":"boom boom boom","file":"/log/a.log","host":"h1","pod":"p-1","count":7}"#,
        "\n"
    );
    fake.respond(row).respond(row);

    let (is_error, out) = call_tool(
        &app,
        "search_logs",
        json!({
            "to": "2026-01-01 01:00:00", "range": "1h", "level": ["error"],
            "filters": { "pod": "p-1" }, "limit": 10, "max_message_chars": 4,
        }),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(out["from"], "2026-01-01T00:00:00.000+08:00");
    assert_eq!(out["to"], "2026-01-01T01:00:00.000+08:00");
    assert_eq!(out["total"], 7);
    assert_eq!(out["returned"], 1);
    let r = &out["rows"][0];
    assert_eq!(r["time"], "2026-01-01T00:00:00.123+08:00");
    assert_eq!(r["level"], "ERROR");
    assert_eq!(r["pod"], "p-1", "维度列原样带过去");
    assert!(r.get("trace_id").is_none(), "空字符串省掉: {r}");
    assert_eq!(r["span_id"], "0102030405060708");
    assert_eq!(r["message"], "boom…[共 14 字符，已截断]");
    assert!(r.get("ts_ms").is_none());

    let reqs: Vec<_> = fake.requests().into_iter().skip(2).collect();
    assert_eq!(reqs.len(), 2, "检索 + count");
    let search = reqs.iter().find(|r| !r.body.contains("count()")).expect("检索那条");
    // 时间条件是位置参数（p0 / p1），按值找
    let bound: Vec<String> = search.query().into_iter().map(|(_, v)| v).collect();
    assert!(bound.contains(&MIDNIGHT_MS.to_string()), "{}", search.target);
    assert!(bound.contains(&(MIDNIGHT_MS + HOUR_MS).to_string()), "{}", search.target);
    assert!(search.body.contains("pod"), "维度筛选进了 SQL: {}", search.body);
    assert!(search.body.contains("level"), "{}", search.body);
    assert!(search.body.contains("LIMIT {p4:UInt32}"), "{}", search.body);
    assert_eq!(search.query_value("param_p4").as_deref(), Some("10"), "limit 是绑定参数");
}

#[tokio::test]
async fn bad_arguments_come_back_as_tool_errors_not_rpc_errors() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;

    // API 的 400 文本原样交给模型：它列出了可用的列名
    let (is_error, out) =
        call_tool(&app, "search_logs", json!({ "filters": { "nope": "x" } })).await;
    assert!(is_error);
    let text = out.as_str().unwrap();
    assert!(text.contains("不认识的筛选列"), "{text}");
    assert!(text.contains("namespace"), "{text}");

    // 我们自己的参数校验
    let (is_error, out) = call_tool(&app, "search_logs", json!({ "from": "昨天中午" })).await;
    assert!(is_error);
    assert!(out.as_str().unwrap().contains("看不懂的时间"), "{out}");

    let (is_error, out) = call_tool(&app, "get_trace", json!({})).await;
    assert!(is_error);
    assert!(out.as_str().unwrap().contains("缺少参数 trace_id"), "{out}");

    // 参数名写错（service 写成 service_name）不能被静默忽略：那样查回来的是全站的数
    let (is_error, out) =
        call_tool(&app, "service_operations", json!({ "service_name": "a" })).await;
    assert!(is_error);
    let text = out.as_str().unwrap();
    assert!(text.contains("不认识的参数") && text.contains("service_name"), "{text}");
    assert!(text.contains("compare"), "把认识的参数名列出来: {text}");

    // 没有这个工具才是 JSON-RPC 错误
    let (status, body) =
        rpc(&app, request(1, "tools/call", json!({ "name": "nope", "arguments": {} }))).await;
    assert_eq!(status, 200);
    assert_eq!(body["error"]["code"], -32602);
    assert!(body["error"]["message"].as_str().unwrap().contains("search_logs"));

    assert!(sql(&fake).is_empty(), "参数都不对就不该查库");
}

fn located(span_id: &str, ts_ms: i64) -> String {
    format!(
        "{{\"span_id\":\"{span_id}\",\"service_name\":\"a\",\"span_name\":\"GET /x\",\"ts_ms\":{ts_ms}}}\n"
    )
}

fn span_row(span_id: &str, parent: &str, start_us: i64, duration_ns: i64, status: &str) -> String {
    format!(
        concat!(
            r#"{{"span_id":"{id}","parent_span_id":"{parent}","service_name":"a","span_name":"GET /x","#,
            r#""span_kind":"Server","start_us":{us},"duration_ns":{dur},"status_code":"{status}","#,
            r#""status_message":"","scope_name":"","scope_version":"","trace_state":""}}"#,
            "\n"
        ),
        id = span_id,
        parent = parent,
        us = start_us,
        dur = duration_ns,
        status = status
    )
}

#[tokio::test]
async fn get_trace_builds_the_tree_and_can_pull_logs() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    let root = "aaaaaaaaaaaaaaaa";
    let child = "bbbbbbbbbbbbbbbb";
    fake
        // 定位：第一档窗口就探中
        .respond(format!("{}{}", located(root, MIDNIGHT_MS), located(child, MIDNIGHT_MS + 10)))
        // 取数
        .respond(format!(
            "{}{}",
            span_row(root, "", MIDNIGHT_MS * 1000, 100_000_000, "Unset"),
            span_row(child, root, MIDNIGHT_MS * 1000 + 10_000, 5_000_000, "Error"),
        ))
        // include_logs 的那一条（count=0，只有检索）
        .respond(concat!(
            r#"{"ts_ms":1767196800005,"level":"INFO","trace_id":"1719ae16e40aca0266d306bb381c8dbc","span_id":"","#,
            r#""thread":"","logger":"l","message":"hello","file":"/f","host":"h"}"#,
            "\n"
        ));

    let (is_error, out) = call_tool(
        &app,
        "get_trace",
        json!({ "trace_id": TRACE.to_uppercase(), "at": "2026-01-01T00:00:00+08:00", "include_logs": true }),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(out["trace_id"], TRACE, "id 归一成小写");
    assert_eq!(out["span_count"], 2);
    assert_eq!(out["error_count"], 1);
    assert_eq!(out["start"], "2026-01-01T00:00:00.000+08:00");
    assert_eq!(out["span_ms"], 100.0);
    let spans = out["spans"].as_array().unwrap();
    assert_eq!(spans[0]["span_id"], root);
    assert_eq!(spans[0]["depth"], 0);
    assert!(spans[0].get("status").is_none());
    assert_eq!(spans[1]["span_id"], child);
    assert_eq!(spans[1]["depth"], 1);
    assert_eq!(spans[1]["parent_span_id"], root);
    assert_eq!(spans[1]["offset_ms"], 10.0);
    assert_eq!(spans[1]["duration_ms"], 5.0);
    assert_eq!(spans[1]["status"], "ERROR");
    let logs = out["logs"].as_array().unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0]["message"], "hello");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 3, "定位 + 取数 + 日志: {sql:?}");
    // 定位带了 at 附近的时间窗（第一档 ±1 分钟）
    let locate = &fake.requests()[2];
    let bound: Vec<String> = locate.query().into_iter().map(|(_, v)| v).collect();
    assert!(bound.contains(&(MIDNIGHT_MS - 60_000).to_string()), "{}", locate.target);
    assert!(bound.contains(&(MIDNIGHT_MS + 60_000).to_string()), "{}", locate.target);
    // 已经知道这条 trace 的时刻，日志按时间范围查而不是只靠 bloom filter
    let logs_req = &fake.requests()[4];
    let bound: Vec<String> = logs_req.query().into_iter().map(|(_, v)| v).collect();
    assert!(bound.contains(&(MIDNIGHT_MS - HOUR_MS).to_string()), "{}", logs_req.target);
    assert!(bound.contains(&(MIDNIGHT_MS + 100 + HOUR_MS).to_string()), "{}", logs_req.target);
    assert!(logs_req.body.contains("trace_id"), "{}", logs_req.body);
    assert!(logs_req.body.contains("ASC"), "日志正序: {}", logs_req.body);
}

#[tokio::test]
async fn service_overview_scores_health_and_puts_the_sick_first() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    // 当前窗、对比窗各一条查询，每条同时带汇总（is_total=1）和分桶行；这里只给汇总
    let current = concat!(
        r#"{"service_name":"big","requests":10000,"errors":10,"q":[10,50,90],"max_ms":200,"is_total":1,"bucket":0}"#,
        "\n",
        r#"{"service_name":"sick","requests":1000,"errors":80,"q":[10,50,90],"max_ms":200,"is_total":1,"bucket":0}"#,
        "\n",
    );
    let previous = concat!(
        r#"{"service_name":"big","requests":9000,"errors":0,"q":[10,50,90],"max_ms":200,"is_total":1,"bucket":0}"#,
        "\n",
        r#"{"service_name":"sick","requests":1000,"errors":0,"q":[10,50,90],"max_ms":200,"is_total":1,"bucket":0}"#,
        "\n",
    );
    fake.respond(current).respond(previous);

    let (is_error, out) = call_tool(&app, "service_overview", json!({ "range": "1h" })).await;
    assert!(!is_error, "{out}");
    assert_eq!(out["compare"], "day");
    assert_eq!(out["service_count"], 2);
    assert_eq!(out["unhealthy_count"], 2);
    let services = out["services"].as_array().unwrap();
    // 8% 错误率是红，排最前；big 从零错误变成有错误是黄
    assert_eq!(services[0]["service"], "sick");
    assert_eq!(services[0]["health"], "red");
    assert_eq!(services[0]["error_rate_pct"], 8.0);
    assert_eq!(services[1]["service"], "big");
    assert_eq!(services[1]["health"], "yellow");
    assert_eq!(services[1]["vs_prev"]["requests_change_pct"], 11.1);
    assert!(services[1].get("spark").is_none(), "页面才要的东西不给模型");
    assert_eq!(sql(&fake).len(), 2);
}

#[tokio::test]
async fn attrs_are_listable_and_metrics_bridge_to_traces() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;

    // span 属性名：别让模型猜 attr=http.route 这种写法
    fake.respond("{\"key\":\"http.route\",\"count\":9}\n");
    let (is_error, out) = call_tool(&app, "list_attrs", json!({ "service": "checkout" })).await;
    assert!(!is_error, "{out}");
    assert_eq!(out["items"][0]["name"], "http.route");
    assert_eq!(out["items"][0]["count"], 9);
    assert!(sql(&fake).last().unwrap().contains("span_attributes"), "{:?}", sql(&fake));

    // 指标的属性取值走另一个端点，形状统一成 items
    fake.respond("{\"name\":\"heap\",\"count\":3}\n");
    let (is_error, out) = call_tool(
        &app,
        "list_attrs",
        json!({ "on": "metric", "metric": "jvm.memory.used", "key": "jvm.memory.type" }),
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(out["key"], "jvm.memory.type");
    assert_eq!(out["items"][0]["name"], "heap");

    // 指标点上挂的 trace id：尖峰直接换成一条链路
    fake.respond(concat!(
        r#"{"t_ms":1767196800123,"value":1234.5,"trace_id":"a1b2","span_id":"c3d4","#,
        "\"service_name\":\"checkout\"}\n",
    ));
    let (is_error, out) =
        call_tool(&app, "metric_exemplars", json!({ "metric": "http.server.request.duration" }))
            .await;
    assert!(!is_error, "{out}");
    let e = &out["exemplars"][0];
    assert_eq!(e["trace_id"], "a1b2");
    assert_eq!(e["time"], "2026-01-01T00:00:00.123+08:00");
    assert_eq!(e["value"], 1234.5);
    assert!(out["next"].as_str().unwrap().contains("get_trace"));
}

#[tokio::test]
async fn without_a_metric_table_the_metric_tools_are_not_offered() {
    let fake = FakeClickhouse::start().await;
    fake.respond(columns_without("otel_metric")).respond(version_fixture());
    let app = support::app(&fake, &[]).await;

    let (_, body) = rpc(&app, request(1, "tools/list", json!({}))).await;
    let names: Vec<&str> = body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"search_logs"), "{names:?}");
    for gone in ["list_metrics", "query_metric", "metric_events", "metric_exemplars"] {
        assert!(!names.contains(&gone), "指标表都没有，不该列 {gone}: {names:?}");
    }
    // 握手说明里也得提一句
    let (_, body) = rpc(&app, request(2, "initialize", json!({}))).await;
    assert!(body["result"]["instructions"].as_str().unwrap().contains("指标表未启用"));
}
