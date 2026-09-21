//! `/api/traces/{trace_id}`：定位 + 取数两趟怎么发，超上限截断时指名的那个 span 还在不在。
//!
//! 为什么要分两趟、窗口为什么从窄往宽探，见 `TraceQueries::detail_locate` 和
//! `DETAIL_PROBE_WINDOWS` 的注释。

mod support;

use support::*;

const AT_MS: i64 = 1_767_196_800_000;
const TRACE: &str = "1719ae16e40aca0266d306bb381c8dbc";

/// 定位查询回的一行（轻列）。
fn located(span_id: &str, ts_ms: i64) -> String {
    format!(
        "{{\"span_id\":\"{span_id}\",\"service_name\":\"a\",\"span_name\":\"GET /x\",\"ts_ms\":{ts_ms}}}\n"
    )
}

fn located_many(n: usize, ts_ms: i64) -> String {
    (0..n).map(|i| located(&format!("{i:016x}"), ts_ms)).collect()
}

/// 取数查询回的一行。
fn span_row(span_id: &str, start_us: i64) -> String {
    format!(
        concat!(
            r#"{{"span_id":"{id}","parent_span_id":"","service_name":"a","span_name":"GET /x","#,
            r#""span_kind":"Server","start_us":{us},"duration_ns":1000,"status_code":"Error","#,
            r#""status_message":"boom","scope_name":"","scope_version":"","trace_state":""}}"#,
            "\n"
        ),
        id = span_id,
        us = start_us
    )
}

fn span_rows(n: usize, start_us: i64) -> String {
    (0..n).map(|i| span_row(&format!("{i:016x}"), start_us)).collect()
}

/// 第 `i` 条查询（同样跳过开头两条）绑定的 `param_pN`。
fn param(fake: &FakeClickhouse, i: usize, n: usize) -> String {
    fake.requests()[i + 2].query_value(&format!("param_p{n}")).expect("缺参数")
}

/// 建表结构那两条不算。
fn sql(fake: &FakeClickhouse) -> Vec<String> {
    fake.requests().into_iter().skip(2).map(|r| r.body).collect()
}

#[tokio::test]
async fn the_span_named_in_the_url_survives_truncation() {
    let fake = FakeClickhouse::start().await;
    // 上限 3：定位会多要一行用来判断截断，回 4 行就是「超了」
    let app = app_with_schema(&fake, &["--max-trace-spans", "3"]).await;
    let want = "08810925838aa5c9";
    fake
        // 定位：第一档窗口就取满了，指名的那个不在里面
        .respond(located_many(4, AT_MS))
        // 单独定位指名的那个
        .respond(located(want, AT_MS + 5))
        // 瀑布图那一趟
        .respond(span_rows(3, AT_MS * 1000))
        // 按排序键前缀把它取回来
        .respond(span_row(want, (AT_MS + 5) * 1000));

    let (status, body) =
        get_json(&app, &format!("/api/traces/{TRACE}?at={AT_MS}&span={want}")).await;
    assert_eq!(status, 200, "{body}");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 4, "定位 + 单独定位 + 取数 + 单独取数: {sql:?}");
    assert!(sql[1].contains("AND span_id = {p1:String}"), "单独定位要钉住 span_id: {}", sql[1]);
    assert!(sql[1].contains("LIMIT 1"), "只要一行: {}", sql[1]);

    assert_eq!(body["truncated"], true);
    assert_eq!(body["pinned_span"], want);
    let ids: Vec<&str> =
        body["spans"].as_array().unwrap().iter().map(|s| s["span_id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), 4, "截断的三条加上钉进来的那条: {ids:?}");
    assert!(ids.contains(&want), "指名的那个 span 得在结果里: {ids:?}");
}

#[tokio::test]
async fn a_span_already_in_the_waterfall_costs_no_extra_query() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &["--max-trace-spans", "3"]).await;
    let want = format!("{:016x}", 1);
    fake.respond(located_many(4, AT_MS)).respond(span_rows(3, AT_MS * 1000));

    let (status, body) =
        get_json(&app, &format!("/api/traces/{TRACE}?at={AT_MS}&span={want}")).await;
    assert_eq!(status, 200, "{body}");

    assert_eq!(sql(&fake).len(), 2, "本来就在这批里，不该再查一趟");
    assert_eq!(body["pinned_span"], serde_json::Value::Null);
}

/// 没被截断、窗口又探到头了：这条 trace 在窗口里是全的，指名的 id 找不到多半是抄错了，
/// 不值得为它扫全部分区（线上 789 MB / 7 s）。
#[tokio::test]
async fn a_missing_span_in_a_complete_trace_does_not_scan_all_partitions() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &["--max-trace-spans", "3"]).await;
    fake
        // 定位：两行，离窗口边还远，第一档就算命中
        .respond(located_many(2, AT_MS))
        // 单独定位：三档带时间的窗口都空
        .respond("")
        .respond("")
        .respond("")
        .respond(span_rows(2, AT_MS * 1000));

    let (status, body) =
        get_json(&app, &format!("/api/traces/{TRACE}?at={AT_MS}&span=08810925838aa5c9")).await;
    assert_eq!(status, 200, "{body}");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 5, "定位 + 三档带时间的单独定位 + 取数: {sql:?}");
    assert!(sql.iter().all(|q| q.contains("timestamp >=")), "不该有不限时间的那一趟: {sql:?}");
    assert_eq!(body["pinned_span"], serde_json::Value::Null);
    assert_eq!(body["spans"].as_array().unwrap().len(), 2);
}

/// 宽的那一档装不下（span 数超上限）：退回一档**从 `at` 往后延**的窗口，而不是拿
/// 「按时间最早的前 N 个」。
///
/// 线上那条被复用的 trace id（21882 个 span、跨 2 小时 13 分）就是这样：最早的 5000 个是
/// 一小时前的另一段，和用户带着 `at` 点进来想看的那一刻毫无关系。往后延是因为 trace 从 `at`
/// 往后跑——定时任务能跑十分钟，围着 `at` 的 ±1 分钟只截得到开头。
#[tokio::test]
async fn a_trace_too_big_to_fit_falls_back_to_a_window_running_forward_from_at() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &["--max-trace-spans", "3"]).await;
    fake
        // ±1 分钟：贴着窗口边，说明 trace 还往外延伸，继续探；这一档留着当最后的退路
        .respond(
            located("aaaaaaaaaaaaaaa1", AT_MS - 59_000)
                + &located("aaaaaaaaaaaaaaa2", AT_MS - 58_000),
        )
        // ±15 分钟：还是贴着边
        .respond(located("aaaaaaaaaaaaaaa1", AT_MS - 899_000) + &located("aaaaaaaaaaaaaaa2", AT_MS))
        // -1 小时 / +24 小时：4 行 > 上限 3，装不下
        .respond(located_many(4, AT_MS))
        // 往后延第一档（-1 分钟 / +2 小时）：3 行，正好装得下，用它
        .respond(located_many(3, AT_MS))
        .respond(span_rows(3, AT_MS * 1000));

    let (status, body) = get_json(&app, &format!("/api/traces/{TRACE}?at={AT_MS}")).await;
    assert_eq!(status, 200, "{body}");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 5, "三档定位 + 一档往后延 + 一趟取数: {sql:?}");
    assert_eq!(body["narrowed"], true);
    assert_eq!(body["truncated"], true, "这条 trace 在这一段之外还有 span");
    assert_eq!(body["windowed"], true);
    // 报的是往后延的那一档：往前只留 1 分钟，往后给到 2 小时
    assert_eq!(body["window_from_ms"], AT_MS - 60_000);
    assert_eq!(body["window_to_ms"], AT_MS + 2 * 3_600_000);
    assert_eq!(body["spans"].as_array().unwrap().len(), 3);
}

/// 往后延的几档也都装不下：这才退回围着 `at` 的窄窗口。
#[tokio::test]
async fn a_trace_too_big_even_looking_forward_falls_back_to_the_window_around_at() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &["--max-trace-spans", "3"]).await;
    fake
        // ±1 分钟：贴着边，留着当退路
        .respond(
            located("aaaaaaaaaaaaaaa1", AT_MS - 59_000)
                + &located("aaaaaaaaaaaaaaa2", AT_MS - 58_000),
        )
        .respond(located("aaaaaaaaaaaaaaa1", AT_MS - 899_000) + &located("aaaaaaaaaaaaaaa2", AT_MS))
        // -1 小时 / +24 小时：装不下
        .respond(located_many(4, AT_MS))
        // 往后延的三档：+2 小时 / +30 分钟 / +5 分钟，全都装不下
        .respond(located_many(4, AT_MS))
        .respond(located_many(4, AT_MS))
        .respond(located_many(4, AT_MS))
        // 取数：用的是退回去的那两个 span
        .respond(span_rows(2, (AT_MS - 59_000) * 1000));

    let (status, body) = get_json(&app, &format!("/api/traces/{TRACE}?at={AT_MS}")).await;
    assert_eq!(status, 200, "{body}");

    assert_eq!(sql(&fake).len(), 7, "三档定位 + 三档往后延 + 一趟取数");
    assert_eq!(body["narrowed"], true);
    assert_eq!(body["window_from_ms"], AT_MS - 60_000);
    assert_eq!(body["window_to_ms"], AT_MS + 60_000, "退到 ±1 分钟那档");
    assert_eq!(body["spans"].as_array().unwrap().len(), 2);
    // 取数的时间条件也跟着退回去的那批走
    assert_eq!(param(&fake, 6, 3).parse::<i64>().unwrap(), AT_MS - 59_000);
}

/// 窄窗口里没几个 span（`at` 偏了几分钟）就别拿它当退路：退回一张几乎空的瀑布图更糟。
#[tokio::test]
async fn a_nearly_empty_window_is_not_used_as_the_fallback() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &["--max-trace-spans", "30"]).await;
    fake
        // ±1 分钟：2 个 span，不到上限的十分之一，太空
        .respond(
            located("aaaaaaaaaaaaaaa1", AT_MS - 59_000)
                + &located("aaaaaaaaaaaaaaa2", AT_MS - 58_000),
        )
        // ±15 分钟：5 个，够用了
        .respond(located_many(5, AT_MS - 899_000))
        // -1 小时 / +24 小时：31 行 > 上限 30
        .respond(located_many(31, AT_MS))
        // 往后延的三档也都装不下
        .respond(located_many(31, AT_MS))
        .respond(located_many(31, AT_MS))
        .respond(located_many(31, AT_MS))
        .respond(span_rows(5, (AT_MS - 899_000) * 1000));

    let (status, body) = get_json(&app, &format!("/api/traces/{TRACE}?at={AT_MS}")).await;
    assert_eq!(status, 200, "{body}");

    assert_eq!(body["narrowed"], true);
    assert_eq!(body["window_from_ms"], AT_MS - 900_000, "退到 ±15 分钟那档，不是更窄的: {body}");
    assert_eq!(body["spans"].as_array().unwrap().len(), 5);
}

/// 上一档已经装了半个上限还多：不再去探不限时间那一档（线上 5.1 GB / 39 s），
/// 反正扫回来也会被截断、再退回上一档。
#[tokio::test]
async fn a_hopeless_trace_never_scans_all_partitions() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &["--max-trace-spans", "3"]).await;
    // 三档带时间的都贴着各自窗口的边、都是 2 行（2 * 2 > 3，够「半个上限还多」）
    let edge = |before: i64| {
        located("aaaaaaaaaaaaaaa1", AT_MS - before + 1_000) + &located("aaaaaaaaaaaaaaa2", AT_MS)
    };
    fake.respond(edge(60_000))
        .respond(edge(900_000))
        .respond(edge(3_600_000))
        .respond(span_rows(2, AT_MS * 1000));

    let (status, body) = get_json(&app, &format!("/api/traces/{TRACE}?at={AT_MS}")).await;
    assert_eq!(status, 200, "{body}");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 4, "三档定位 + 取数，没有不限时间那一趟: {sql:?}");
    assert!(sql.iter().all(|q| q.contains("timestamp >=")), "不该有不限时间的查询: {sql:?}");
    assert_eq!(body["windowed"], true);
}

/// 没带 `at`（顶栏粘一个 trace id 直达、手贴链接）：先锚在「现在」往回探，别一上来就扫全部分区。
#[tokio::test]
async fn without_at_it_probes_around_now_first() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &["--max-trace-spans", "30"]).await;
    let now_ms = chrono::Utc::now().timestamp_millis();
    // 近 15 分钟这一档就命中：5 个 span，都在窗口正中间
    fake.respond(located_many(5, now_ms - 60_000)).respond(span_rows(5, (now_ms - 60_000) * 1000));

    let (status, body) = get_json(&app, &format!("/api/traces/{TRACE}")).await;
    assert_eq!(status, 200, "{body}");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 2, "一档定位 + 一趟取数: {sql:?}");
    assert!(sql[0].contains("timestamp >="), "第一趟就该带时间条件: {sql:?}");
    assert_eq!(body["windowed"], true);
    assert_eq!(body["spans"].as_array().unwrap().len(), 5);
    let from = body["window_from_ms"].as_i64().unwrap();
    let to = body["window_to_ms"].as_i64().unwrap();
    assert_eq!(to - from, 30 * 60_000, "±15 分钟那一档: {body}");
    assert!((from - (now_ms - 15 * 60_000)).abs() < 5_000, "窗口该锚在现在: {body}");
}

/// 三档都没探中才落到不限时间那一档——兜底还在，老 trace 照样查得全。
#[tokio::test]
async fn without_at_the_full_scan_is_still_the_last_resort() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &["--max-trace-spans", "30"]).await;
    let old = AT_MS; // 2025 年的时间戳，锚在「现在」的三档窗口都够不着
    fake.respond("")
        .respond("")
        .respond("")
        .respond(located_many(5, old))
        .respond(span_rows(5, old * 1000));

    let (status, body) = get_json(&app, &format!("/api/traces/{TRACE}")).await;
    assert_eq!(status, 200, "{body}");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 5, "三档空 + 不限时间 + 取数: {sql:?}");
    assert!(!sql[3].contains("timestamp >="), "最后一趟是不限时间的兜底: {sql:?}");
    assert_eq!(body["windowed"], false);
    assert_eq!(body["spans"].as_array().unwrap().len(), 5);
}
