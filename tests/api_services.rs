//! `/api/services/*`：接口表和时间序列怎么和对比窗口对齐。
//!
//! 服务级的「比昨天慢 3 倍」只说明有事，**是哪个接口**才是能动手的信息：接口表当前窗和对比窗
//! 各查一次，按 `(service, span_name, span_kind)` 对齐；时间序列按相对位置对齐到同一格。这里验的就是
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
    svc_op_row("svc", name, requests, errors, p95)
}

fn svc_op_row(service: &str, name: &str, requests: u64, errors: u64, p95: f64) -> String {
    format!(
        "{{\"service_name\":\"{service}\",\"span_name\":\"{name}\",\"span_kind\":\"Server\",\
         \"requests\":{requests},\"errors\":{errors},\"q\":[1,{p95},{p95}],\"max_ms\":{p95}}}\n"
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

/// 接口表每个服务封顶 200 行（`MAX_OPERATIONS_PER_SERVICE`）：`span_name` 的基数不可控，
/// 把 SQL 拼进 span 名的服务一小时上千个名字。切过之后「对比窗有、当前窗没有」这个判断对这个
/// 服务就不成立了——排不进前 200 不等于接口没了——所以那一段要跳过它。
#[tokio::test]
async fn operations_are_capped_per_service_and_stop_claiming_gone() {
    let fake = FakeClickhouse::start().await;
    let app = serial_app(&fake).await;
    // 当前窗：正好吐满上限
    let full: String = (0..200).map(|i| op_row(&format!("op{i:03}"), 500 - i, 0, 10.0)).collect();
    fake.respond(full);
    // 对比窗：有一个当前窗这一批里没有的接口
    fake.respond(op_row("op000", 500, 0, 10.0) + &op_row("cut", 400, 0, 30.0));

    let (status, body) = get_json(
        &app,
        &format!("/api/services/svc/operations?from={MIDNIGHT_MS}&to={}", MIDNIGHT_MS + HOUR_MS),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["truncated"], true);
    let ops = body["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 200, "没有补出第 201 行");
    assert!(ops.iter().all(|o| o["span_name"] != "cut"), "被切过的服务不补「接口没了」的行");

    // SQL 里确实带着每个服务的上限
    let requests = data_requests(&fake);
    assert!(requests[0].body.contains("LIMIT 200 BY service_name"), "{}", requests[0].body);
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

/// 总览页每张异常卡上那句「主要是哪个接口」：一次问好几个服务，一条查询回来，每行带 service。
/// 以前是一张卡各查一次，十几个服务同时报警就是十几条。
#[tokio::test]
async fn operations_can_answer_several_services_in_one_query() {
    let fake = FakeClickhouse::start().await;
    let app = serial_app(&fake).await;
    fake.respond(svc_op_row("a", "A", 1000, 0, 900.0) + &svc_op_row("b", "B", 500, 0, 10.0));

    let (status, body) = get_json(
        &app,
        &format!(
            "/api/services/operations?service=a,b&compare=none&from={MIDNIGHT_MS}&to={}",
            MIDNIGHT_MS + HOUR_MS
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let requests = data_requests(&fake);
    assert_eq!(requests.len(), 1, "两个服务一条查询");
    // 静态路径要赢过 /api/services/{name}/operations，不能被当成 name = "operations"
    assert_eq!(requests[0].query_value("param_p2").unwrap(), "['a','b']");
    let ops = body["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 2);
    // 每行带上是哪个服务的，前端按它分组
    assert_eq!(find(ops, "A")["service"], "a");
    assert_eq!(find(ops, "B")["service"], "b");

    // 同名接口在不同服务下是两行，不能被对齐成一行
    let fake = FakeClickhouse::start().await;
    let app = serial_app(&fake).await;
    fake.respond(svc_op_row("a", "GET /x", 10, 0, 5.0) + &svc_op_row("b", "GET /x", 20, 0, 9.0));
    fake.respond(svc_op_row("a", "GET /x", 10, 0, 1.0));
    let (status, body) = get_json(
        &app,
        &format!(
            "/api/services/operations?service=a,b&from={MIDNIGHT_MS}&to={}",
            MIDNIGHT_MS + HOUR_MS
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let ops = body["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 2, "{body}");
    let a = ops.iter().find(|o| o["service"] == "a").unwrap();
    let b = ops.iter().find(|o| o["service"] == "b").unwrap();
    assert_eq!(a["prev"]["p95_ms"], 1.0, "a 的对比窗对上了");
    assert!(b["prev"].is_null(), "b 在对比窗里没有，不能借用 a 的");
}

/// 一次问太多服务就拒绝：`IN` 列表和返回行数都会失控，宁可让页面退回一个一个问
#[tokio::test]
async fn operations_refuses_too_many_services() {
    let fake = FakeClickhouse::start().await;
    let app = serial_app(&fake).await;
    let many: Vec<String> = (0..40).map(|i| format!("s{i}")).collect();
    let (status, body) = get_json(
        &app,
        &format!(
            "/api/services/operations?service={}&from={MIDNIGHT_MS}&to={}",
            many.join(","),
            MIDNIGHT_MS + HOUR_MS
        ),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = get_json(
        &app,
        &format!("/api/services/operations?from={MIDNIGHT_MS}&to={}", MIDNIGHT_MS + HOUR_MS),
    )
    .await;
    assert_eq!(status, 400, "一个服务都不给也是 400: {body}");
}
