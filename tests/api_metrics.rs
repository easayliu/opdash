//! `/api/metrics/*`：发出去的 SQL、时间轴对齐、指标表缺席时的样子。

mod support;

use support::*;

/// Asia/Shanghai 的 2026-01-01 00:00:00，正好是分桶原点（本地零点），桶序号从 0 开始好算。
const MIDNIGHT_MS: i64 = 1_767_196_800_000;
const HOUR_MS: i64 = 3_600_000;

fn window() -> String {
    format!("from={MIDNIGHT_MS}&to={}", MIDNIGHT_MS + HOUR_MS)
}

/// 表结构那两条之后发出去的 SQL。
fn sql(fake: &FakeClickhouse) -> Vec<String> {
    fake.requests().into_iter().skip(2).map(|r| r.body).collect()
}

#[tokio::test]
async fn catalog_only_scans_the_recent_window() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    fake.respond(concat!(
        r#"{"metric_name":"http.server.request.duration","metric_type":"Histogram","metric_unit":"ms","#,
        r#""description":"耗时","temporality":"Cumulative","services":["checkout","cart"],"#,
        "\"is_monotonic\":0,\"points\":120}\n",
    ));

    // 页面选 30 天，目录只看最近 6 小时
    let to = MIDNIGHT_MS + 30 * 24 * HOUR_MS;
    let (status, body) = get_json(&app, &format!("/api/metrics?from={MIDNIGHT_MS}&to={to}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["from_ms"], to - 6 * HOUR_MS);
    assert_eq!(body["to_ms"], to);
    let m = &body["metrics"][0];
    assert_eq!(m["name"], "http.server.request.duration");
    assert_eq!(m["type"], "Histogram");
    assert_eq!(m["monotonic"], false);
    assert_eq!(m["services"], serde_json::json!(["checkout", "cart"]));
}

#[tokio::test]
async fn rate_query_diffs_per_series_and_lays_points_on_one_axis() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    // 一小时 / 最多 60 个点 → 桶宽 1 分钟，原点就是 from
    fake.respond(concat!(
        "{\"bucket\":0,\"keys\":[\"checkout\"],\"v\":1.5}\n",
        "{\"bucket\":2,\"keys\":[\"checkout\"],\"v\":2.5}\n",
        "{\"bucket\":0,\"keys\":[\"cart\"],\"v\":10}\n",
    ));

    let (status, body) = get_json(
        &app,
        &format!(
            "/api/metrics/query?{}&metric=http.server.requests&agg=rate&by=service_name",
            window()
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["width_ms"], 60_000);
    assert_eq!(body["t_ms"].as_array().unwrap().len(), 60);
    assert_eq!(body["t_ms"][0], MIDNIGHT_MS);
    assert_eq!(body["truncated"], false);

    // 量大的排前面，空桶是 null 而不是 0
    let series = body["series"].as_array().unwrap();
    assert_eq!(series.len(), 2);
    assert_eq!(series[0]["name"], "service_name=cart");
    assert_eq!(series[1]["name"], "service_name=checkout");
    assert_eq!(series[1]["values"][0], 1.5);
    assert!(series[1]["values"][1].is_null());
    assert_eq!(series[1]["values"][2], 2.5);
    assert_eq!(series[1]["last"], 2.5);
    assert_eq!(series[1]["labels"][0]["key"], "service_name");

    let sent = &sql(&fake)[0];
    // 累积量按时间线相减，时间线必须带上 resource 属性（同一服务的两个 pod 是两条计数器）
    assert!(sent.contains("lagInFrame"), "{sent}");
    assert!(sent.contains("toString(resource_attributes)"), "{sent}");
    assert!(sent.contains("[toString(`service_name`)] AS keys"), "{sent}");
    // 指标名走参数，不拼进 SQL
    assert!(sent.contains("metric_name = {p"), "{sent}");
    let params = fake.requests()[2].query_value("param_p2");
    assert_eq!(params.as_deref(), Some("http.server.requests"));
}

#[tokio::test]
async fn quantiles_come_from_merged_histogram_buckets() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    // 三个桶各 10 个样本，上界 10 / 20 / 50
    fake.respond("{\"bucket\":0,\"keys\":[],\"counts\":[10,10,10,0],\"bounds\":[10,20,50]}\n");

    let (status, body) = get_json(
        &app,
        &format!("/api/metrics/query?{}&metric=http.duration&agg=quantile&q=0.5,0.95", window()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let series = body["series"].as_array().unwrap();
    assert_eq!(series.len(), 2, "一个分位数一条线: {body}");
    let p50 = series.iter().find(|s| s["name"] == "quantile=p50").unwrap();
    assert_eq!(p50["values"][0], 15.0);
    let p95 = series.iter().find(|s| s["name"] == "quantile=p95").unwrap();
    assert_eq!(p95["values"][0], 45.5);

    let sent = &sql(&fake)[0];
    assert!(sent.contains("sumForEach"), "{sent}");
    assert!(sent.contains("explicit_bounds"), "{sent}");
}

#[tokio::test]
async fn label_filters_and_step_reach_clickhouse() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    fake.respond("");

    let (status, body) = get_json(
        &app,
        &format!(
            "/api/metrics/query?{}&metric=jvm.memory.used&agg=avg&step=300&attr=jvm.memory.type=heap&rattr=k8s.namespace.name=prod&service=checkout",
            window()
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["width_ms"], 300_000);
    assert_eq!(body["t_ms"].as_array().unwrap().len(), 12);

    let sent = &sql(&fake)[0];
    assert!(sent.contains("toString(attributes.`jvm.memory.type`) = {p"), "{sent}");
    assert!(sent.contains("toString(resource_attributes.`k8s.namespace.name`) = {p"), "{sent}");
    assert!(sent.contains("service_name IN {p"), "{sent}");
    // 不用相减的聚合不算时间线，省下读整列 JSON
    assert!(!sent.contains("cityHash64"), "{sent}");
}

#[tokio::test]
async fn exemplars_carry_the_trace_id() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    fake.respond(concat!(
        r#"{"t_ms":1767196800123,"value":1234.5,"trace_id":"a1b2","span_id":"c3d4","#,
        "\"service_name\":\"checkout\"}\n",
    ));

    let (status, body) =
        get_json(&app, &format!("/api/metrics/exemplars?{}&metric=http.duration", window())).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["exemplars"][0]["trace_id"], "a1b2");
    assert_eq!(body["exemplars"][0]["value"], 1234.5);
    assert!(sql(&fake)[0].contains("ARRAY JOIN"), "{}", sql(&fake)[0]);
}

#[tokio::test]
async fn bad_parameters_say_which_one() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;

    let (status, body) = get_json(&app, &format!("/api/metrics/query?{}", window())).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("metric"), "{body}");

    let (status, body) =
        get_json(&app, &format!("/api/metrics/query?{}&metric=a&agg=median", window())).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("agg"), "{body}");

    // 步长太小：一小时 1 秒一个点是 3600 个，超上限
    let (status, body) =
        get_json(&app, &format!("/api/metrics/query?{}&metric=a&step=1", window())).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("步长"), "{body}");
}

#[tokio::test]
async fn without_the_table_the_page_is_off_but_logs_still_work() {
    let fake = FakeClickhouse::start().await;
    fake.respond(columns_without("otel_metric")).respond(version_fixture());
    let app = app(&fake, &[]).await;

    let (status, meta) = get_json(&app, "/api/meta").await;
    assert_eq!(status, 200, "{meta}");
    assert!(meta["metrics"].is_null(), "没有指标表就不该有 metrics: {meta}");
    assert!(meta["metrics_note"].as_str().unwrap().contains("otel_metric"), "{meta}");
    // 另外两张表照常
    assert_eq!(meta["logs"]["table"], "app_log");
    assert_eq!(meta["traces"]["table"], "otel_trace");

    let (status, body) = get_json(&app, &format!("/api/metrics?{}", window())).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("指标页未启用"), "{body}");
}
