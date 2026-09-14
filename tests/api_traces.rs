//! `/api/traces/search`：候选查询发几条、时间窗怎么收。
//!
//! 这条路径上两条查询都不便宜（线上实测合计 5200 万行 / 2.1 GB），贵在哪、怎么收窄，
//! 见 `PROBE_WINDOWS_MS` 和 `TraceQueries::summaries` 的注释。

mod support;

use support::*;

const NOW_MS: i64 = 1_767_196_800_000;
const HOUR_MS: i64 = 3_600_000;
const MINUTE_MS: i64 = 60_000;

/// 一行候选。`ts_ms` 是这条 trace 那个「排第一」的匹配 span 的时间戳。
fn candidate(id: &str, ts_ms: i64) -> String {
    format!("{{\"trace_id\":\"{id}\",\"ts_ms\":{ts_ms}}}\n")
}

fn candidates(n: usize, ts_ms: i64) -> String {
    (0..n).map(|i| candidate(&format!("{i:032x}"), ts_ms)).collect()
}

/// `root_count` / `span_count` 可调：没有根 span 的那条会触发「按完整搜索窗补一次」。
fn summary_row(id: &str, root_count: u32, span_count: u32) -> String {
    format!(
        concat!(
            r#"{{"trace_id":"{id}","start_us":1767196800000000,"span_ns":1000,"span_count":{n},"#,
            r#""error_count":0,"root_service":"a","root_name":"GET /x","root_duration_ns":1000,"#,
            r#""root_count":{r},"first_service":"a","first_name":"GET /x","first_duration_ns":1000,"#,
            r#""services":["a"]}}"#,
            "\n"
        ),
        id = id,
        r = root_count,
        n = span_count
    )
}

fn summary(id: &str) -> String {
    summary_row(id, 1, 1)
}

/// 建表结构、版本那两条不算。
fn sql(fake: &FakeClickhouse) -> Vec<String> {
    fake.requests().into_iter().skip(2).map(|r| r.body).collect()
}

/// 第 `i` 条查询（同样跳过开头两条）绑定的 `param_pN`。
fn param(fake: &FakeClickhouse, i: usize, n: usize) -> String {
    fake.requests()[i + 2].query_value(&format!("param_p{n}")).expect("缺参数")
}

fn search_url(extra: &str) -> String {
    format!("/api/traces/search?from={}&to={NOW_MS}{extra}", NOW_MS - HOUR_MS)
}

#[tokio::test]
async fn newest_first_probes_the_tail_of_the_window() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    // 第一级探测窗就凑够 50 条
    fake.respond(candidates(50, NOW_MS - 1000)).respond(summary(&format!("{:032x}", 0)));

    let (status, body) = get_json(&app, &search_url("")).await;
    assert_eq!(status, 200, "{body}");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 2, "探测一次就够，不该再扫整窗: {sql:?}");
    // 候选查询的时间谓词收到了窗口尾部的 1 分钟，而不是整整一小时
    assert_eq!(param(&fake, 0, 0).parse::<i64>().unwrap(), NOW_MS - MINUTE_MS);
    assert_eq!(param(&fake, 0, 1).parse::<i64>().unwrap(), NOW_MS);
}

#[tokio::test]
async fn probing_widens_until_it_finds_enough_then_falls_back_to_the_whole_window() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    // 1 分钟窗只有 3 条、15 分钟窗只有 10 条，都不够 50 → 退回整窗
    fake.respond(candidates(3, NOW_MS - 1000))
        .respond(candidates(10, NOW_MS - 1000))
        .respond(candidates(50, NOW_MS - 1000))
        .respond(summary(&format!("{:032x}", 0)));

    let (status, body) = get_json(&app, &search_url("")).await;
    assert_eq!(status, 200, "{body}");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 4, "两级探测 + 整窗 + 摘要: {sql:?}");
    assert_eq!(param(&fake, 0, 0).parse::<i64>().unwrap(), NOW_MS - MINUTE_MS);
    assert_eq!(param(&fake, 1, 0).parse::<i64>().unwrap(), NOW_MS - 15 * MINUTE_MS);
    // 最后一条才是用户真正要的那个范围
    assert_eq!(param(&fake, 2, 0).parse::<i64>().unwrap(), NOW_MS - HOUR_MS);
}

#[tokio::test]
async fn slowest_first_never_probes() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    fake.respond(candidates(50, NOW_MS - 1000)).respond(summary(&format!("{:032x}", 0)));

    let (status, body) = get_json(&app, &search_url("&sort=duration")).await;
    assert_eq!(status, 200, "{body}");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 2, "按耗时排不能提前停: {sql:?}");
    // 一上来就是整窗：最慢的那条可能在窗口任何位置
    assert_eq!(param(&fake, 0, 0).parse::<i64>().unwrap(), NOW_MS - HOUR_MS);
}

#[tokio::test]
async fn a_window_narrower_than_the_first_probe_is_queried_directly() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    fake.respond(candidates(2, NOW_MS - 1000)).respond(summary(&format!("{:032x}", 0)));

    let from = NOW_MS - 30_000;
    let (status, body) =
        get_json(&app, &format!("/api/traces/search?from={from}&to={NOW_MS}")).await;
    assert_eq!(status, 200, "{body}");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 2, "30 秒的窗口比探测窗还窄，别白搭一次往返: {sql:?}");
    assert_eq!(param(&fake, 0, 0).parse::<i64>().unwrap(), from);
}

#[tokio::test]
async fn summaries_are_scoped_to_the_candidates_not_the_search_window() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    // 50 条候选挤在 16 毫秒里（线上就是这样），搜索窗却是一小时
    let oldest = NOW_MS - 1_016;
    let newest = NOW_MS - 1_000;
    let rows: String = (0..50)
        .map(|i| candidate(&format!("{i:032x}"), if i == 0 { oldest } else { newest }))
        .collect();
    fake.respond(rows).respond(summary(&format!("{:032x}", 0)));

    let (status, body) = get_json(&app, &search_url("")).await;
    assert_eq!(status, 200, "{body}");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 2);
    assert!(sql[1].contains("GROUP BY trace_id"), "{}", sql[1]);
    // p0 是 id 数组，时间谓词是 p1 / p2：锚在候选的最早 / 最晚上，各放宽 10 分钟
    assert_eq!(param(&fake, 1, 1).parse::<i64>().unwrap(), oldest - 10 * MINUTE_MS);
    assert_eq!(param(&fake, 1, 2).parse::<i64>().unwrap(), newest + 1 + 10 * MINUTE_MS);
}

#[tokio::test]
async fn searching_by_trace_id_skips_the_candidate_query_and_the_time_range() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    let id = format!("{:032x}", 7);
    fake.respond(summary(&id));

    let (status, body) = get_json(&app, &search_url(&format!("&trace_id={id}"))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["traces"].as_array().unwrap().len(), 1);

    let sql = sql(&fake);
    assert_eq!(sql.len(), 1, "只该查摘要: {sql:?}");
    assert!(!sql[0].contains("timestamp >="), "按 id 查不限时间，走 bloom filter: {}", sql[0]);
}

#[tokio::test]
async fn traces_cut_by_the_narrow_window_are_re_fetched_over_the_whole_window() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    let cut = format!("{:032x}", 0);
    let whole = format!("{:032x}", 1);
    // 第一趟：cut 那条在窗口里找不到根 span（root_count=0），说明前面被切了
    fake.respond(candidates(50, NOW_MS - 1000))
        .respond(summary_row(&cut, 0, 7) + &summary_row(&whole, 1, 3))
        // 补捞只带这一个 id，范围回到完整搜索窗
        .respond(summary_row(&cut, 1, 99));

    let (status, body) = get_json(&app, &search_url("")).await;
    assert_eq!(status, 200, "{body}");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 3, "探测 + 收窄的摘要 + 补捞: {sql:?}");
    assert!(sql[2].contains("GROUP BY trace_id"), "{}", sql[2]);
    // 补捞只带被切的那一个 id
    assert_eq!(param(&fake, 2, 0), format!("['{cut}']"));
    // 时间谓词回到用户给的范围（±10 分钟）
    assert_eq!(param(&fake, 2, 1).parse::<i64>().unwrap(), NOW_MS - HOUR_MS - 10 * MINUTE_MS);
    assert_eq!(param(&fake, 2, 2).parse::<i64>().unwrap(), NOW_MS + 10 * MINUTE_MS);

    // 补捞的结果覆盖掉被切的那一份
    let traces = body["traces"].as_array().unwrap();
    let got = traces.iter().find(|t| t["trace_id"] == cut).expect("被切的那条应该还在");
    assert_eq!(got["span_count"], 99, "该用补捞回来的数: {got}");
    let other = traces.iter().find(|t| t["trace_id"] == whole).unwrap();
    assert_eq!(other["span_count"], 3, "有根 span 的那条不该被重查: {other}");
}

#[tokio::test]
async fn a_search_window_barely_wider_than_the_candidates_is_not_narrowed() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    // root_count=0，但窗口没收窄过，就没有「被切」这回事，不该再补一次
    fake.respond(candidates(2, NOW_MS - 1000)).respond(summary_row(&format!("{:032x}", 0), 0, 5));

    // 30 秒的窗口：探测都轮不上，收窄之后（±10 分钟）也不比原来窄
    let from = NOW_MS - 30_000;
    let (status, body) =
        get_json(&app, &format!("/api/traces/search?from={from}&to={NOW_MS}")).await;
    assert_eq!(status, 200, "{body}");

    let sql = sql(&fake);
    assert_eq!(sql.len(), 2, "窗口本来就窄，收窄不了也就不用补: {sql:?}");
    assert_eq!(param(&fake, 1, 1).parse::<i64>().unwrap(), from - 10 * MINUTE_MS);
}
