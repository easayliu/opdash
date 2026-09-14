//! `/api/services/*`：接口表和时间序列怎么和对比窗口对齐。
//!
//! 服务级的「比昨天慢 3 倍」只说明有事，**是哪个接口**才是能动手的信息：接口表当前窗和对比窗
//! 各查一次，按 `(span_name, span_kind)` 对齐；时间序列按相对位置对齐到同一格。这里验的就是
//! 这两处对齐，以及「对比窗有、当前窗没有」的接口不会被悄悄丢掉。
//!
//! 两条查询是并发发的，假库按到达顺序回放，所以测试里把并发上限压成 1——回放顺序才确定
//! （`tokio::try_join!` 按顺序 poll，先当前窗后对比窗）。

mod support;

use support::*;

/// Asia/Shanghai 的 2026-01-01 00:00:00，正好是分桶原点（本地零点），桶序号从 0 开始好算。
const MIDNIGHT_MS: i64 = 1_767_196_800_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

async fn serial_app(fake: &FakeClickhouse) -> axum::Router {
    app_with_schema(fake, &["--max-concurrent-queries", "1"]).await
}

/// 数据查询（跳过启动时读表结构 / 版本的那两条）
fn data_requests(fake: &FakeClickhouse) -> Vec<Captured> {
    fake.requests().into_iter().skip(2).collect()
}

/// 时间条件是第一个绑定的参数，所以 p0 / p1 就是这条查询的时间窗
fn window_of(req: &Captured) -> (i64, i64) {
    let get = |k: &str| req.query_value(k).unwrap().parse::<i64>().unwrap();
    (get("param_p0"), get("param_p1"))
}

fn op_row(name: &str, requests: u64, errors: u64, p95: f64) -> String {
    format!(
        "{{\"span_name\":\"{name}\",\"span_kind\":\"Server\",\"requests\":{requests},\
         \"errors\":{errors},\"q\":[1,{p95},{p95}],\"max_ms\":{p95}}}\n"
    )
}

fn find<'a>(ops: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    ops.iter().find(|o| o["span_name"] == name).unwrap_or_else(|| panic!("没有 {name} 这一行"))
}

#[tokio::test]
async fn operations_pairs_every_span_with_the_compare_window() {
    let fake = FakeClickhouse::start().await;
    let app = serial_app(&fake).await;
    // 当前窗：A 慢了一个数量级，B 是这段时间才有的接口
    fake.respond(op_row("A", 1000, 0, 900.0) + &op_row("B", 500, 0, 10.0));
    // 对比窗：A 当时很快，C 现在一次都没有了
    fake.respond(op_row("A", 1000, 0, 100.0) + &op_row("C", 400, 4, 30.0));

    let (status, body) = get_json(
        &app,
        &format!("/api/services/svc/operations?from={MIDNIGHT_MS}&to={}", MIDNIGHT_MS + HOUR_MS),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    // 默认和昨天同一时段比：对比窗口整体平移 24 小时
    assert_eq!(body["compare"], "day");
    assert_eq!(body["prev_from_ms"], MIDNIGHT_MS - DAY_MS);
    assert_eq!(body["prev_to_ms"], MIDNIGHT_MS + HOUR_MS - DAY_MS);
    let requests = data_requests(&fake);
    assert_eq!(requests.len(), 2, "当前窗和对比窗各一条");
    assert_eq!(window_of(&requests[0]), (MIDNIGHT_MS, MIDNIGHT_MS + HOUR_MS));
    assert_eq!(window_of(&requests[1]), (MIDNIGHT_MS - DAY_MS, MIDNIGHT_MS + HOUR_MS - DAY_MS));

    let ops = body["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 3, "两边的接口并起来：A / B / C");
    let a = find(ops, "A");
    assert_eq!(a["p95_ms"], 900.0);
    assert_eq!(a["prev"]["p95_ms"], 100.0);
    // 对比窗口没有这个接口：prev 是 null，前端据此显示「新」而不是「+∞%」
    assert!(find(ops, "B")["prev"].is_null());
    // 对比窗口有、当前窗一次都没有：补成 0 次的一行，不然「整个接口没了」在表上看不见
    let c = find(ops, "C");
    assert_eq!(c["requests"], 0);
    assert_eq!(c["prev"]["requests"], 400);
    assert_eq!(c["prev"]["errors"], 4);
    assert_eq!(ops.last().unwrap()["span_name"], "C", "没了的接在后面");
}

#[tokio::test]
async fn operations_can_skip_the_compare_window() {
    let fake = FakeClickhouse::start().await;
    let app = serial_app(&fake).await;
    fake.respond(op_row("A", 10, 0, 5.0));

    let (status, body) = get_json(
        &app,
        &format!(
            "/api/services/svc/operations?from={MIDNIGHT_MS}&to={}&compare=none",
            MIDNIGHT_MS + HOUR_MS
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(data_requests(&fake).len(), 1, "不比就只发一条");
    assert_eq!(body["compare"], "none");
    assert!(body.get("prev_from_ms").is_none());
    assert!(body["operations"][0]["prev"].is_null());
}

#[tokio::test]
async fn timeseries_lines_up_the_compare_window_bucket_by_bucket() {
    let fake = FakeClickhouse::start().await;
    let app = serial_app(&fake).await;
    // 一小时范围 → 桶宽 30 秒 → 120 个桶，原点就是 from
    fake.respond("{\"bucket\":5,\"requests\":10,\"errors\":1,\"q\":[1,2,3]}\n");
    fake.respond("{\"bucket\":5,\"requests\":7,\"errors\":0,\"q\":[4,5,6]}\n");

    let (status, body) = get_json(
        &app,
        &format!("/api/services/svc/timeseries?from={MIDNIGHT_MS}&to={}", MIDNIGHT_MS + HOUR_MS),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let requests = data_requests(&fake);
    assert_eq!(requests.len(), 2);
    assert_eq!(window_of(&requests[1]), (MIDNIGHT_MS - DAY_MS, MIDNIGHT_MS + HOUR_MS - DAY_MS));
    let points = body["points"].as_array().unwrap();
    assert_eq!(points.len(), 120);
    // 对比窗口的桶从它自己的起点切，第 i 格对应同一个相对时刻
    assert_eq!(points[5]["requests"], 10);
    assert_eq!(points[5]["p95_ms"], 2.0);
    assert_eq!(points[5]["prev"]["requests"], 7);
    assert_eq!(points[5]["prev"]["p95_ms"], 5.0);
    // 对比窗口那一格没有请求：prev 缺席，曲线在这里断开而不是掉到 0
    assert!(points[4].get("prev").is_none());
}

#[tokio::test]
async fn compare_only_takes_the_four_known_values() {
    let fake = FakeClickhouse::start().await;
    let app = serial_app(&fake).await;
    let (status, body) = get_json(
        &app,
        &format!(
            "/api/services/svc/operations?from={MIDNIGHT_MS}&to={}&compare=yesteryear",
            MIDNIGHT_MS + HOUR_MS
        ),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("none"), "{body}");
}
