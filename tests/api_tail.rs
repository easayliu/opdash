//! `/api/logs/tail`：跟随的 SSE 流——推什么、去重、名额、不许被压缩。

mod support;

use std::time::Duration;

use support::*;

const MIDNIGHT_MS: i64 = 1_767_196_800_000;
const HOUR_MS: i64 = 3_600_000;

/// 等一条事件最多多久。跟随间隔在测试里压到 100ms，2 秒足够，卡住时也不至于把 CI 挂死。
const WAIT: Duration = Duration::from_secs(2);

fn row(ts_ms: i64, message: &str) -> String {
    format!(
        concat!(
            r#"{{"ts_ms":{ts},"level":"INFO","trace_id":"","span_id":"","thread":"main","#,
            r#""logger":"a","message":"{msg}","file":"/log/a.log","host":"h1"}}"#,
            "\n"
        ),
        ts = ts_ms,
        msg = message
    )
}

fn tail_uri() -> String {
    format!("/api/logs/tail?from={MIDNIGHT_MS}&to={}", MIDNIGHT_MS + HOUR_MS)
}

async fn tail_app(fake: &FakeClickhouse) -> axum::Router {
    app_with_schema(fake, &["--tail-interval", "100ms"]).await
}

#[tokio::test]
async fn tail_pushes_new_rows_and_skips_the_ones_already_sent() {
    let fake = FakeClickhouse::start().await;
    let app = tail_app(&fake).await;
    // 首轮：窗口里最新的一屏
    fake.respond(row(MIDNIGHT_MS, "first"));
    // 第二轮从游标往后查，会把 first 再捞出来一次（同一毫秒的行要靠指纹去重）
    fake.respond(row(MIDNIGHT_MS, "first") + &row(MIDNIGHT_MS + 5, "second"));

    let (status, headers, mut sse) = open_sse(&app, &tail_uri(), &[]).await;
    assert_eq!(status, 200);
    assert_eq!(header_value(&headers, "content-type"), Some("text/event-stream"));

    let hello = sse.next_event(WAIT).await.expect("hello");
    assert_eq!(hello.event, "hello");
    assert_eq!(hello.json()["interval_ms"], 100);
    assert_eq!(hello.json()["resumed"], false);

    let first = sse.next_event(WAIT).await.expect("第一批");
    assert_eq!(first.event, "rows");
    let rows = first.json()["rows"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["message"], "first");
    // 事件 id 是游标，断线重连时浏览器用它续上
    assert_eq!(first.id.as_deref(), Some(MIDNIGHT_MS.to_string().as_str()));

    let second = sse.next_event(WAIT).await.expect("第二批");
    let rows = second.json()["rows"].as_array().unwrap().clone();
    assert_eq!(rows.len(), 1, "推过的行不该再推一次: {rows:?}");
    assert_eq!(rows[0]["message"], "second");

    // 首轮取的是最新一屏（倒序），之后才顺着游标往后
    let sql = fake.requests().into_iter().skip(2).map(|r| r.body).collect::<Vec<_>>();
    assert!(sql[0].contains("timestamp DESC"), "{}", sql[0]);
    assert!(sql[1].contains("timestamp ASC"), "{}", sql[1]);
}

#[tokio::test]
async fn tail_is_not_compressed() {
    // 压缩器要攒够一块才吐字节，SSE 被压了就成了「几十秒蹦一批」
    let fake = FakeClickhouse::start().await;
    let app = tail_app(&fake).await;
    fake.respond(row(MIDNIGHT_MS, "hi"));

    let (status, headers, mut sse) =
        open_sse(&app, &tail_uri(), &[("accept-encoding", "gzip, br")]).await;
    assert_eq!(status, 200);
    assert_eq!(header_value(&headers, "content-encoding"), None, "{headers:?}");
    assert_eq!(header_value(&headers, "x-accel-buffering"), Some("no"));
    assert_eq!(sse.next_event(WAIT).await.map(|e| e.event).as_deref(), Some("hello"));
}

#[tokio::test]
async fn tail_needs_a_time_range() {
    let fake = FakeClickhouse::start().await;
    let app = tail_app(&fake).await;
    // 按 id 查不带时间范围（走 bloom filter），跟随没有游标起点可用
    let (status, body) = get_json(&app, "/api/logs/tail?trace_id=abc123").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["kind"], "bad_request");
}

#[tokio::test]
async fn tail_streams_are_capped() {
    let fake = FakeClickhouse::start().await;
    let app =
        app_with_schema(&fake, &["--tail-interval", "100ms", "--max-tail-streams", "1"]).await;

    let (status, _, _open) = open_sse(&app, &tail_uri(), &[]).await;
    assert_eq!(status, 200);
    // 名额握在连接手里（`_open` 还活着），第二条只能被挡
    let (status, body) = get_json(&app, &tail_uri()).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["kind"], "unavailable");
}

#[tokio::test]
async fn tail_reports_query_errors_without_dropping_the_stream() {
    let fake = FakeClickhouse::start().await;
    let app = tail_app(&fake).await;
    fake.respond_error(159, "Timeout exceeded");
    fake.respond(row(MIDNIGHT_MS, "back"));

    let (status, _, mut sse) = open_sse(&app, &tail_uri(), &[]).await;
    assert_eq!(status, 200);
    assert_eq!(sse.next_event(WAIT).await.map(|e| e.event).as_deref(), Some("hello"));

    let err = sse.next_event(WAIT).await.expect("报错事件");
    assert_eq!(err.event, "query_error");
    assert_eq!(err.json()["kind"], "timeout");
    // 库超时是暂时的，下一轮接着试，流不断
    assert_eq!(err.json()["fatal"], false);

    let rows = sse.next_event(WAIT).await.expect("恢复后的行");
    assert_eq!(rows.event, "rows");
    assert_eq!(rows.json()["rows"][0]["message"], "back");
}
