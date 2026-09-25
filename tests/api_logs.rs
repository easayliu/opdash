//! `/api/logs/*`：总数从哪来、发了几条查询。

mod support;

use support::*;

/// Asia/Shanghai 的 2026-01-01 00:00:00，正好是分桶原点（本地零点），桶序号从 0 开始好算。
const MIDNIGHT_MS: i64 = 1_767_196_800_000;
const HOUR_MS: i64 = 3_600_000;

/// 一行同时能当日志行和 count 行解：检索和 count 是并发发出去的，假库按到达顺序回放，
/// 用同一个响应体就不用管谁先到。
fn dual_row() -> String {
    concat!(
        r#"{"ts_ms":1767196800123,"level":"INFO","trace_id":"","span_id":"","thread":"main","#,
        r#""logger":"a","message":"hi","file":"/log/a.log","host":"h1","count":7}"#,
        "\n"
    )
    .to_owned()
}

fn log_sql(fake: &FakeClickhouse) -> Vec<String> {
    // 前两条是启动时读表结构 / 版本
    fake.requests().into_iter().skip(2).map(|r| r.body).collect()
}

#[tokio::test]
async fn histogram_total_is_the_sum_of_buckets() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    // 一小时范围 → 桶宽 30 秒 → 120 个桶，原点就是 from
    fake.respond(concat!(
        "{\"bucket\":0,\"level\":\"INFO\",\"count\":5}\n",
        "{\"bucket\":0,\"level\":\"ERROR\",\"count\":1}\n",
        "{\"bucket\":119,\"level\":\"INFO\",\"count\":30}\n",
    ));

    let (status, body) = get_json(
        &app,
        &format!("/api/logs/histogram?from={MIDNIGHT_MS}&to={}", MIDNIGHT_MS + HOUR_MS),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["buckets"].as_array().unwrap().len(), 120);
    // 时间条件左闭右开、桶按同一原点切，每行都落在某个桶里，所以各桶之和 == count()
    assert_eq!(body["total"], 36);
    let sum: i64 =
        body["buckets"].as_array().unwrap().iter().map(|b| b["total"].as_i64().unwrap()).sum();
    assert_eq!(sum, 36);
}

#[tokio::test]
async fn search_with_count_off_sends_one_query() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    fake.respond(dual_row());

    let (status, body) = get_json(
        &app,
        &format!("/api/logs/search?from={MIDNIGHT_MS}&to={}&count=0", MIDNIGHT_MS + HOUR_MS),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(body["total"].is_null(), "关掉 count 就不该有总数: {body}");

    let sql = log_sql(&fake);
    assert_eq!(sql.len(), 1, "只该发检索这一条: {sql:?}");
    assert!(!sql[0].contains("count()"), "{}", sql[0]);
}

#[tokio::test]
async fn search_counts_when_asked() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    fake.respond(dual_row()).respond(dual_row());

    let (status, body) = get_json(
        &app,
        &format!("/api/logs/search?from={MIDNIGHT_MS}&to={}", MIDNIGHT_MS + HOUR_MS),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["total"], 7);

    let sql = log_sql(&fake);
    assert_eq!(sql.len(), 2, "检索 + count 两条: {sql:?}");
    assert!(sql.iter().any(|s| s.contains("SELECT count() AS count")), "{sql:?}");
}

/// 筛选下拉的候选值：十来个维度一条查询，不是一个维度一条。
///
/// 以前日志页一打开就发 4 条 facets、点「更多筛选」再发 10 条，14 条的 WHERE 一模一样、
/// 只差分组的那一列，每条都要扫一遍整个时间窗。
#[tokio::test]
async fn facets_answer_every_dimension_in_one_query() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    // 一行，每个维度一列
    fake.respond(
        r#"{"host":[["h1",42],["h2",7]],"level":[["INFO",100]],"logger":[]}"#.to_owned() + "\n",
    );

    let (status, body) = get_json(
        &app,
        &format!(
            "/api/logs/facets?from={MIDNIGHT_MS}&to={}&field=host,level,logger",
            MIDNIGHT_MS + HOUR_MS
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let requests: Vec<_> = fake.requests().into_iter().skip(2).collect();
    assert_eq!(requests.len(), 1, "三个维度一条查询");
    assert!(requests[0].body.contains("approx_top_k"), "{}", requests[0].body);

    let facets = body["facets"].as_array().unwrap();
    // 顺序跟着请求里 field 的顺序走，前端按它对号入座
    assert_eq!(
        facets.iter().map(|f| f["field"].as_str().unwrap()).collect::<Vec<_>>(),
        ["host", "level", "logger"]
    );
    assert_eq!(facets[0]["values"][0]["value"], "h1");
    assert_eq!(facets[0]["values"][0]["count"], 42);
    assert_eq!(facets[1]["values"][0]["value"], "INFO");
    assert!(facets[2]["values"].as_array().unwrap().is_empty(), "空维度也占一项");

    // 不能筛的列要挡掉，别悄悄少一个下拉
    let (status, body) = get_json(
        &app,
        &format!(
            "/api/logs/facets?from={MIDNIGHT_MS}&to={}&field=host,message",
            MIDNIGHT_MS + HOUR_MS
        ),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = get_json(
        &app,
        &format!("/api/logs/facets?from={MIDNIGHT_MS}&to={}", MIDNIGHT_MS + HOUR_MS),
    )
    .await;
    assert_eq!(status, 400, "一个维度都不给也是 400: {body}");
}

#[tokio::test]
async fn deeply_nested_keywords_are_rejected_before_any_query() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;

    // 解析器曾经不限深度：几千个 `(`（URL 编码是 %28）就递归到栈溢出、整个进程 abort
    let q = "%28".repeat(100);
    let (status, body) = get_json(
        &app,
        &format!("/api/logs/search?from={MIDNIGHT_MS}&to={}&q={q}a", MIDNIGHT_MS + HOUR_MS),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    assert!(body.to_string().contains("嵌套"), "{body}");
    assert!(log_sql(&fake).is_empty(), "拒绝了就不该再查库");
}
