//! 对着真实 ClickHouse 跑一遍所有接口。默认跳过；给了 `OPDASH_E2E_CLICKHOUSE_URL` 才跑：
//!
//! ```bash
//! OPDASH_E2E_CLICKHOUSE_URL=http://host:8123 OPDASH_E2E_CLICKHOUSE_USER=x OPDASH_E2E_CLICKHOUSE_PASSWORD=y \
//!   cargo test --test e2e_clickhouse -- --nocapture
//! ```
//!
//! 假 ClickHouse 验不了的东西都在这：参数的 TSV 转义在服务端怎么解、JSON 属性列的子列写法、
//! Distributed 表上 `LIMIT 1 BY` + `IN {ids}`、24.8 与 26.x 的默认值差异。只读，不写任何数据。

mod support;

use std::sync::Arc;

use clap::Parser;
use opdash::api::{self, AppState};
use opdash::clickhouse::{Client, ClientOptions, Query};
use opdash::config::Config;
use opdash::schema::SchemaCache;
use support::get_json;

macro_rules! e2e_or_skip {
    () => {
        match std::env::var("OPDASH_E2E_CLICKHOUSE_URL") {
            Ok(url) if !url.is_empty() => url,
            _ => {
                eprintln!("OPDASH_E2E_CLICKHOUSE_URL 未设置，跳过 E2E");
                return;
            }
        }
    };
}

fn config(url: &str) -> Config {
    let user = std::env::var("OPDASH_E2E_CLICKHOUSE_USER").unwrap_or_else(|_| "default".into());
    let password = std::env::var("OPDASH_E2E_CLICKHOUSE_PASSWORD").unwrap_or_default();
    Config::parse_from([
        "opdash",
        "--clickhouse-url",
        url,
        "--clickhouse-user",
        &user,
        "--clickhouse-password",
        &password,
    ])
}

async fn app(url: &str) -> (axum::Router, Client) {
    let config = config(url);
    let client = Client::new(ClientOptions {
        endpoint: config.clickhouse_url.clone(),
        user: config.clickhouse_user.clone(),
        password: config.clickhouse_password.clone(),
        timeout: config.query_timeout,
        max_read_bytes: config.max_read_bytes,
        max_read_rows: config.max_read_rows,
        max_concurrent: config.max_concurrent_queries,
    })
    .unwrap();
    let schema = Arc::new(SchemaCache::new(
        client.clone(),
        &config.database,
        &config.log_table,
        &config.trace_table,
        &config.metric_table,
    ));
    schema.refresh().await.expect("读表结构");
    let state = AppState::new(config, client.clone(), schema);
    (api::app(state, opdash::auth::Auth::disabled()), client)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// String 参数里的反斜杠、TAB、换行、中文都要原样到达 ClickHouse。
#[tokio::test]
async fn string_params_round_trip() {
    let url = e2e_or_skip!();
    let (_, client) = app(&url).await;
    #[derive(serde::Deserialize)]
    struct Row {
        s: String,
        arr: Vec<String>,
    }
    let value = "a\\d+\tb\nc 'q' \"z\" 支付";
    let rows = client
        .rows::<Row>(
            Query::new("SELECT {s:String} AS s, {a:Array(String)} AS arr")
                .param("s", value)
                .param("a", vec!["x\ny".to_owned(), "it's".to_owned(), "back\\slash".to_owned()]),
        )
        .await
        .unwrap()
        .rows;
    assert_eq!(rows[0].s, value);
    assert_eq!(rows[0].arr, ["x\ny", "it's", "back\\slash"]);
}

#[tokio::test]
async fn every_endpoint_answers() {
    let url = e2e_or_skip!();
    let (app, _) = app(&url).await;
    let now = now_ms();
    let from = now - 30 * 60_000;

    let (status, meta) = get_json(&app, "/api/meta").await;
    assert_eq!(status, 200, "{meta}");
    let dims: Vec<String> = meta["logs"]["dimensions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();

    let (status, body) = get_json(&app, "/api/health").await;
    assert_eq!(status, 200, "{body}");

    let (status, body) =
        get_json(&app, &format!("/api/logs/search?from={from}&to={now}&limit=5")).await;
    assert_eq!(status, 200, "{body}");
    let rows = body["rows"].as_array().unwrap();
    assert!(body["total"].is_number(), "{body}");

    // 关键字搜索：单次扫描连总数一起给
    let (status, body) =
        get_json(&app, &format!("/api/logs/search?from={from}&to={now}&q=a&limit=2")).await;
    assert_eq!(status, 200, "{body}");
    assert!(body["total"].is_number(), "{body}");

    // 正则里的反斜杠要能用
    let (status, body) =
        get_json(&app, &format!("/api/logs/search?from={from}&to={now}&q=%5Cd%2B&regex=1&limit=1"))
            .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = get_json(
        &app,
        &format!("/api/logs/search?from={from}&to={now}&q=(unclosed&regex=1&limit=1"),
    )
    .await;
    assert_eq!(status, 400, "坏正则应 400: {body}");

    let (status, body) = get_json(&app, &format!("/api/logs/histogram?from={from}&to={now}")).await;
    assert_eq!(status, 200, "{body}");
    assert!(body["buckets"].as_array().unwrap().len() <= 121);

    for dim in dims.iter().take(2) {
        let (status, body) =
            get_json(&app, &format!("/api/logs/facets?from={from}&to={now}&field={dim}")).await;
        assert_eq!(status, 200, "{body}");
    }
    let (status, body) =
        get_json(&app, &format!("/api/logs/search?from={from}&to={now}&bogus=1")).await;
    assert_eq!(status, 400, "{body}");

    if let Some(row) = rows.first() {
        let host = row["host"].as_str().unwrap();
        let file = row["file"].as_str().unwrap();
        let ts = row["ts_ms"].as_i64().unwrap();
        let (status, body) = get_json(
            &app,
            &format!(
                "/api/logs/context?host={}&file={}&ts={ts}&before=3&after=3",
                urlenc(host),
                urlenc(file)
            ),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (status, raw) = support::get_raw(
            &app,
            &format!("/api/logs/export?from={from}&to={now}&limit=3&format=csv"),
            &[],
        )
        .await;
        assert_eq!(status, 200);
        let text = String::from_utf8_lossy(&raw);
        assert!(text.starts_with("\"time\",\"level\""), "{text}");
    }

    // trace：服务列表 → 该服务的链路 → 详情 → 关联日志
    let (status, body) =
        get_json(&app, &format!("/api/traces/values?from={from}&to={now}&field=service&limit=3"))
            .await;
    assert_eq!(status, 200, "{body}");
    if let Some(service) = body["values"][0]["value"].as_str().map(str::to_owned) {
        let (status, body) = get_json(
            &app,
            &format!(
                "/api/traces/search?from={from}&to={now}&service={}&sort=duration&limit=3",
                urlenc(&service)
            ),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        if let Some(trace_id) = body["traces"][0]["trace_id"].as_str().map(str::to_owned) {
            let (status, detail) = get_json(&app, &format!("/api/traces/{trace_id}")).await;
            assert_eq!(status, 200, "{detail}");
            let spans = detail["spans"].as_array().unwrap();
            assert!(!spans.is_empty());
            // 瀑布图那一趟不带属性（四个 JSON 列是详情查询的全部成本）
            assert_eq!(detail["attributes_lazy"], true, "{detail}");
            assert_eq!(spans[0]["attributes"], serde_json::json!({}), "{}", spans[0]);
            // 点开某个 span 才取它的属性
            let span_id = spans[0]["span_id"].as_str().unwrap().to_owned();
            let (status, attrs) =
                get_json(&app, &format!("/api/traces/{trace_id}/spans/{span_id}")).await;
            assert_eq!(status, 200, "{attrs}");
            assert_eq!(attrs["span_id"], span_id);
            assert!(attrs["attributes"].is_object(), "{attrs}");
            assert!(attrs["resource"].is_object(), "{attrs}");
            // 库里没有的 span 要报得清楚，不能 500
            let (status, body) =
                get_json(&app, &format!("/api/traces/{trace_id}/spans/ffffffffffffffff")).await;
            assert_eq!(status, 400, "{body}");
            let (status, body) =
                get_json(&app, &format!("/api/logs/search?trace_id={trace_id}&limit=5")).await;
            assert_eq!(status, 200, "{body}");
            let (status, body) =
                get_json(&app, &format!("/api/traces/search?trace_id={trace_id}")).await;
            assert_eq!(status, 200, "{body}");
            assert_eq!(body["traces"][0]["trace_id"], trace_id);
        }
        let (status, body) = get_json(
            &app,
            &format!(
                "/api/traces/attr_keys?from={from}&to={now}&service={}&limit=5",
                urlenc(&service)
            ),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        if let Some(key) = body["keys"][0]["key"].as_str().map(str::to_owned) {
            let (status, body) = get_json(
                &app,
                &format!(
                    "/api/traces/attr_values?from={from}&to={now}&service={}&key={}&limit=3",
                    urlenc(&service),
                    urlenc(&key)
                ),
            )
            .await;
            assert_eq!(status, 200, "{body}");
            let (status, body) = get_json(
                &app,
                &format!(
                    "/api/traces/search?from={from}&to={now}&service={}&attr={}&limit=2",
                    urlenc(&service),
                    urlenc(&key)
                ),
            )
            .await;
            assert_eq!(status, 200, "{body}");
        }
        let (status, body) = get_json(
            &app,
            &format!("/api/services/{}/operations?from={from}&to={now}", urlenc(&service)),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (status, body) = get_json(
            &app,
            &format!("/api/services/{}/timeseries?from={from}&to={now}", urlenc(&service)),
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }
    let (status, body) = get_json(&app, &format!("/api/services?from={from}&to={now}")).await;
    assert_eq!(status, 200, "{body}");
    // 不选服务、范围超过 6 小时 → 400
    let (status, body) =
        get_json(&app, &format!("/api/traces/search?from={}&to={now}", now - 7 * 3_600_000)).await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = get_json(&app, "/api/traces/not-a-trace-id").await;
    assert_eq!(status, 400, "{body}");
}

/// 指标那套 SQL（窗口函数、`sumForEach`、Nested 上的 `ARRAY JOIN`）假库验不了，必须对着真库跑。
/// 没部署 metricpipe 的环境自动跳过。
#[tokio::test]
async fn metric_endpoints_answer() {
    let url = e2e_or_skip!();
    let (app, _) = app(&url).await;
    let now = now_ms();
    let from = now - 30 * 60_000;

    let (_, meta) = get_json(&app, "/api/meta").await;
    if meta["metrics"].is_null() {
        eprintln!("指标表不可用（{}），跳过", meta["metrics_note"]);
        return;
    }

    let (status, catalog) = get_json(&app, &format!("/api/metrics?from={from}&to={now}")).await;
    assert_eq!(status, 200, "{catalog}");
    let Some(metric) = catalog["metrics"].as_array().and_then(|m| m.first()).cloned() else {
        eprintln!("最近半小时没有指标数据，跳过");
        return;
    };
    let name = metric["name"].as_str().unwrap().to_owned();
    let ty = metric["type"].as_str().unwrap().to_owned();
    let base = format!("/api/metrics/query?from={from}&to={now}&metric={}", urlenc(&name));

    // 每种类型都按页面上的默认算法查一遍
    let (agg, field) = match ty.as_str() {
        "Histogram" => ("quantile", "value"),
        "ExponentialHistogram" | "Summary" => ("rate", "count"),
        "Sum" if metric["monotonic"].as_bool() == Some(true) => ("rate", "value"),
        _ => ("avg", "value"),
    };
    let (status, body) = get_json(&app, &format!("{base}&agg={agg}&field={field}")).await;
    assert_eq!(status, 200, "{ty} 的默认算法: {body}");
    let buckets = body["t_ms"].as_array().unwrap().len();
    assert!(buckets > 0 && buckets <= 61, "{buckets} 个桶");
    for s in body["series"].as_array().unwrap() {
        assert_eq!(s["values"].as_array().unwrap().len(), buckets, "每条线都铺在同一条时间轴上");
    }

    // 累积量相减：换个 agg 再来一遍，SQL 里那套窗口函数得能在集群上跑
    for agg in ["rate", "increase", "avg", "last", "max", "mean"] {
        let field = if ty.ends_with("Histogram") || ty == "Summary" {
            if agg == "mean" { "sum" } else { "count" }
        } else {
            "value"
        };
        let (status, body) = get_json(&app, &format!("{base}&agg={agg}&field={field}")).await;
        assert_eq!(status, 200, "agg={agg} field={field}: {body}");
    }

    // 标签：查出来的 key 要能直接拿去分组和过滤
    let (status, labels) = get_json(
        &app,
        &format!("/api/metrics/labels?from={from}&to={now}&metric={}&limit=5", urlenc(&name)),
    )
    .await;
    assert_eq!(status, 200, "{labels}");
    let (status, body) =
        get_json(&app, &format!("{base}&agg={agg}&field={field}&by=service_name")).await;
    assert_eq!(status, 200, "{body}");
    if let Some(key) = labels["names"][0]["name"].as_str().map(str::to_owned) {
        let (status, body) =
            get_json(&app, &format!("{base}&agg={agg}&field={field}&by={}", urlenc(&key))).await;
        assert_eq!(status, 200, "按 {key} 分组: {body}");
        let (status, values) = get_json(
            &app,
            &format!(
                "/api/metrics/label_values?from={from}&to={now}&metric={}&key={}&limit=3",
                urlenc(&name),
                urlenc(&key)
            ),
        )
        .await;
        assert_eq!(status, 200, "{values}");
        if let Some(v) = values["names"][0]["name"].as_str() {
            let (status, body) = get_json(
                &app,
                &format!("{base}&agg={agg}&field={field}&attr={}", urlenc(&format!("{key}={v}"))),
            )
            .await;
            assert_eq!(status, 200, "按 {key}={v} 过滤: {body}");
        }
    }

    // 直方图分位数：多个 q 一次出
    if ty == "Histogram" {
        let (status, body) = get_json(&app, &format!("{base}&agg=quantile&q=0.5,0.95,0.99")).await;
        assert_eq!(status, 200, "{body}");
    }

    let (status, body) = get_json(
        &app,
        &format!("/api/metrics/exemplars?from={from}&to={now}&metric={}&limit=5", urlenc(&name)),
    )
    .await;
    assert_eq!(status, 200, "{body}");

    let (status, body) = get_json(&app, &format!("{base}&agg=bogus")).await;
    assert_eq!(status, 400, "{body}");
}

fn urlenc(s: &str) -> String {
    form_urlencoded::byte_serialize(s.as_bytes()).collect()
}
