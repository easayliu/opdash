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
