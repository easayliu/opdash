//! `/api/bills/*`：账单表的识别（改名前后的两套表名）、去重键从哪来、两朵云怎么合并。

mod support;

use support::*;

/// 表结构那三条（列 / 版本 / 排序键）之后发出去的 SQL。
fn sql(fake: &FakeClickhouse) -> Vec<String> {
    fake.requests().into_iter().skip(3).map(|r| r.body).collect()
}

/// 某朵云那条查询。
fn sql_for(fake: &FakeClickhouse, needle: &str) -> String {
    sql(fake)
        .into_iter()
        .find(|s| s.contains(needle))
        .unwrap_or_else(|| panic!("没有发给 {needle} 的查询"))
}

#[tokio::test]
async fn meta_lists_the_bill_tables() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &[]).await;

    let (status, meta) = get_json(&app, "/api/meta").await;
    assert_eq!(status, 200, "{meta}");
    assert_eq!(meta["bills"]["volcengine"]["table"], "volcengine_bill");
    assert_eq!(meta["bills"]["alicloud_monthly"]["table"], "alicloud_bill_monthly");
    assert_eq!(meta["bills"]["providers"], serde_json::json!(["volcengine", "alicloud"]));
    assert_eq!(meta["bills"]["daily_providers"], serde_json::json!(["volcengine", "alicloud"]));
    assert_eq!(meta["bills"]["dedupe"], "group");
    assert!(meta["bills_note"].is_null(), "三张表都在，不该有 note: {meta}");
}

/// goscan 改名之前建的表是 `X_distributed`（本地表 `X_local`）。配置里写的是基础名，
/// opdash 得自己认出来，不然升级 goscan 之前费用页是空的。
#[tokio::test]
async fn resolves_the_old_distributed_suffix() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "_distributed", &[]).await;

    let (status, meta) = get_json(&app, "/api/meta").await;
    assert_eq!(status, 200, "{meta}");
    assert_eq!(meta["bills"]["volcengine"]["table"], "volcengine_bill_distributed");

    fake.respond_to("volcengine_bill_distributed", "").respond_to("alicloud_bill_monthly", "");
    let (status, body) = get_json(&app, "/api/bills/summary?from=2026-08&to=2026-09").await;
    assert_eq!(status, 200, "{body}");
    // 查的是 Distributed 表，不能退回 _local（那只有一个分片的数据）
    let volc = sql_for(&fake, "volcengine_bill");
    assert!(volc.contains("`logs`.`volcengine_bill_distributed`"), "{volc}");
    assert!(!volc.contains("_local"), "{volc}");
}

/// 火山那张表改名前叫 `volcengine_bill_details`。线上是「先按新口径重建表、后改配置」，
/// 两个名字会并存一段时间——**不配环境变量也得认出来**。
#[tokio::test]
async fn resolves_the_old_volcengine_table_name() {
    let fake = FakeClickhouse::start().await;
    let legacy = bill_columns("").replace("volcengine_bill", "volcengine_bill_details");
    fake.respond(format!("{}{}", columns_fixture(), legacy))
        .respond(version_fixture())
        .respond(sorting_keys_fixture("").replace("volcengine_bill", "volcengine_bill_details"));
    let app = app(&fake, &[]).await;

    let (status, meta) = get_json(&app, "/api/meta").await;
    assert_eq!(status, 200, "{meta}");
    assert_eq!(meta["bills"]["volcengine"]["table"], "volcengine_bill_details");
    assert!(meta["bills_note"].is_null(), "{meta}");

    // 配成别的名字的部署就按配的来，不会偷偷退回老名字
    let fake2 = FakeClickhouse::start().await;
    fake2.respond(format!("{}{}", columns_fixture(), legacy)).respond(version_fixture());
    let pinned = support::app(&fake2, &["--volcengine-bill-table", "volc_bill_2026"]).await;
    let (_, meta) = get_json(&pinned, "/api/meta").await;
    assert!(meta["bills"]["volcengine"].is_null(), "{meta}");
    assert!(meta["bills_note"].as_str().unwrap().contains("volc_bill_2026"), "{meta}");
}

/// 没部署 goscan：费用页整个不显示，接口说清楚为什么。
#[tokio::test]
async fn without_goscan_the_page_is_off() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;

    let (status, meta) = get_json(&app, "/api/meta").await;
    assert_eq!(status, 200, "{meta}");
    assert!(meta["bills"].is_null(), "{meta}");
    let note = meta["bills_note"].as_str().unwrap_or_default();
    assert!(note.contains("volcengine_bill") && note.contains("goscan"), "{note}");

    let (status, body) = get_json(&app, "/api/bills/summary").await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("费用页未启用"), "{body}");
}

/// 去重键是**从库里读的排序键**，不是写死的：goscan 改了 ORDER BY 而 opdash 没跟，
/// 按老键去重会静悄悄地少算钱，所以这条要盯住。
#[tokio::test]
async fn dedupes_by_the_sorting_key_read_from_the_database() {
    let fake = FakeClickhouse::start().await;
    fake.respond(format!("{}{}", columns_fixture(), bill_columns("")))
        .respond(version_fixture())
        // 库里的排序键和 goscan 当前 DDL 不一样（这里少了几列），去重要按库里的来
        .respond(concat!(
            r#"{"table":"volcengine_bill","sorting_key":"BillPeriod, InstanceNo, Product"}"#,
            "\n",
            r#"{"table":"alicloud_bill_monthly","sorting_key":"billing_cycle, product_code"}"#,
            "\n",
        ));
    let app = app(&fake, &[]).await;
    fake.respond_to("volcengine_bill", "").respond_to("alicloud_bill_monthly", "");

    let (status, body) = get_json(&app, "/api/bills/summary?from=2026-09&to=2026-09").await;
    assert_eq!(status, 200, "{body}");
    let volc = sql_for(&fake, "volcengine_bill");
    assert!(volc.contains("GROUP BY `BillPeriod`, `InstanceNo`, `Product`\n"), "{volc}");
    // 金额在子查询里取 any()，外层再求和：一行重复的只算一次
    assert!(volc.contains("any(toFloat64OrZero(PayableAmount)) AS _amount"), "{volc}");
    assert!(volc.contains("sum(_amount) AS amount"), "{volc}");
}

/// `--bill-dedupe=final` 换成给表加 FINAL（分片键确定的部署可以这么省一点）。
#[tokio::test]
async fn dedupe_mode_is_configurable() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &["--bill-dedupe", "final"]).await;
    fake.respond_to("volcengine_bill", "").respond_to("alicloud_bill_monthly", "");

    let (status, body) = get_json(&app, "/api/bills/summary?from=2026-09&to=2026-09").await;
    assert_eq!(status, 200, "{body}");
    let volc = sql_for(&fake, "volcengine_bill");
    assert!(volc.contains("`logs`.`volcengine_bill` FINAL"), "{volc}");
    assert!(!volc.contains("GROUP BY `BillPeriod`"), "{volc}");
}

/// 两朵云各查各的，合到同一条账期轴上；请求的账期一个不少，没数据的月是 0。
#[tokio::test]
async fn summary_merges_two_clouds_onto_one_axis() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &[]).await;
    fake.respond_to(
        "volcengine_bill",
        "{\"period\":\"2026-08\",\"amount\":100.5,\"rows\":3}\n{\"period\":\"2026-09\",\"amount\":200,\"rows\":4}\n",
    )
    .respond_to("alicloud_bill_monthly", "{\"period\":\"2026-09\",\"amount\":50.25,\"rows\":2}\n");

    let (status, body) = get_json(&app, "/api/bills/summary?from=2026-07&to=2026-09").await;
    assert_eq!(status, 200, "{body}");
    let points = body["points"].as_array().unwrap();
    assert_eq!(points.len(), 3, "{body}");
    assert_eq!(points[0]["t"], "2026-07");
    assert_eq!(points[0]["total"], 0.0, "没数据的账期也要在轴上: {body}");
    assert_eq!(points[1]["total"], 100.5);
    assert_eq!(points[1]["by_provider"]["volcengine"], 100.5);
    assert_eq!(points[2]["total"], 250.25);
    assert_eq!(points[2]["by_provider"]["alicloud"], 50.25);
    assert_eq!(body["total"], 350.75);
    assert_eq!(body["by_provider"]["volcengine"], 300.5);
    assert_eq!(body["amount"], "payable");

    // 账期是参数，SQL 里不拼字面量
    let volc = sql_for(&fake, "volcengine_bill");
    assert!(!volc.contains("2026-07"), "{volc}");
    let params = fake.requests()[3].query_value("param_p0");
    assert!(params.is_some(), "账期应该按参数传");
}

/// 排行跨云合并，`other` 是总额减去列出来的那些。
#[tokio::test]
async fn breakdown_merges_and_reports_the_rest() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &[]).await;
    // 每朵云两条查询：排行 + 总额
    fake.respond_to("GROUP BY _key", "{\"key\":\"云服务器\",\"amount\":80}\n")
        .respond_to("sum(_amount) AS amount, count()", "{\"amount\":100,\"rows\":9}\n")
        .respond_to("GROUP BY _key", "{\"key\":\"云服务器\",\"amount\":20}\n")
        .respond_to("sum(_amount) AS amount, count()", "{\"amount\":40,\"rows\":5}\n");

    let (status, body) =
        get_json(&app, "/api/bills/breakdown?by=product&from=2026-09&to=2026-09&limit=5").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["by"], "product");
    assert_eq!(body["label"], "产品");
    assert_eq!(body["total"], 140.0);
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"], "云服务器");
    assert_eq!(rows[0]["amount"], 100.0, "两朵云的同名产品合成一行: {body}");
    // 140 - 100：没进排行的那些
    assert_eq!(body["other"], 40.0);
}

/// 按天只问有日粒度的表；两朵云的日度表都没有时说清楚。
#[tokio::test]
async fn daily_uses_the_daily_tables_only() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &[]).await;
    fake.respond_to("volcengine_bill", "{\"day\":\"2026-09-01\",\"amount\":10}\n")
        .respond_to("alicloud_bill_daily", "{\"day\":\"2026-09-01\",\"amount\":5}\n");

    let (status, body) = get_json(&app, "/api/bills/daily?from=2026-09&to=2026-09").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["points"][0]["t"], "2026-09-01");
    assert_eq!(body["points"][0]["total"], 15.0);
    // 阿里云走的是日度表，不是月度表
    let ali = sql_for(&fake, "alicloud_bill_daily");
    assert!(ali.contains("billing_date >= toDate(concat("), "{ali}");
    assert!(sql(&fake).iter().all(|s| !s.contains("alicloud_bill_monthly")), "{:?}", sql(&fake));
}

/// 明细一页只出一朵云；没指定就挑一张有的，并把挑的是哪张回给前端。
#[tokio::test]
async fn detail_is_per_provider_and_says_which_one() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &[]).await;
    fake.respond_to(
        "ORDER BY amount DESC",
        concat!(
            r#"{"period":"2026-09","day":"2026-09-01","product":"云服务器 ECS","item":"按量","#,
            r#""instance_id":"i-1","instance":"web-1","region":"华东1","account":"主账号","#,
            r#""project":"默认","subscription":"PayAsYouGo","usage":"720","usage_unit":"小时","#,
            "\"currency\":\"CNY\",\"amount\":12.3456789,\"original\":15,\"paid\":12.3}\n",
        ),
    )
    .respond_to("count() AS rows", "{\"rows\":42}\n");

    let (status, body) =
        get_json(&app, "/api/bills/detail?provider=alicloud&from=2026-09&to=2026-09").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["provider"], "alicloud");
    assert_eq!(body["granularity"], "monthly");
    assert_eq!(body["total"], 42);
    assert_eq!(body["rows"][0]["provider"], "alicloud");
    assert_eq!(body["rows"][0]["instance"], "web-1");
    // 金额对到分位再往下两位，别把 Float64 的尾巴带到页面上
    assert_eq!(body["rows"][0]["amount"], 12.3457);
}

/// 参数错的时候要说人话。
#[tokio::test]
async fn rejects_bad_parameters() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &[]).await;

    for (uri, needle) in [
        ("/api/bills/summary?from=2026-1", "YYYY-MM"),
        ("/api/bills/summary?from=2026-13", "月份"),
        ("/api/bills/summary?from=2020-01&to=2026-09", "单次最多查询"),
        ("/api/bills/summary?from=2026-09&to=2026-08", "晚于"),
        ("/api/bills/breakdown?by=pod", "不认识的维度"),
        ("/api/bills/summary?pod=x", "不认识的维度"),
        ("/api/bills/detail?provider=tencent", "provider"),
        ("/api/bills/detail?granularity=hourly", "granularity"),
        ("/api/bills/export?format=xlsx", "format"),
    ] {
        let (status, body) = get_json(&app, uri).await;
        assert_eq!(status, 400, "{uri} → {body}");
        assert!(body["error"].as_str().unwrap().contains(needle), "{uri} → {body}");
    }
}

/// 手动拉账单：opdash 把动作转给 goscan，自己不碰库。
///
/// 这里的「goscan」就是另一个 [`FakeClickhouse`]——它只是个会回罐头响应、把请求记下来的 HTTP
/// 服务，用来当 goscan 正好合适。
#[tokio::test]
async fn sync_forwards_to_goscan_and_polls_the_task() {
    let fake = FakeClickhouse::start().await;
    let goscan = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &["--goscan-url", goscan.endpoint()]).await;

    // 能拉：/api/meta 里带着这个开关，页面据此显示按钮
    let (_, meta) = get_json(&app, "/api/meta").await;
    assert_eq!(meta["bills"]["sync"], true);

    goscan.respond(r#"{"task_id":"t-1","status":"started","message":"Sync triggered for provider alicloud","provider":"alicloud"}"#);
    let (status, body) = post_json(
        &app,
        "/api/bills/sync",
        &serde_json::json!({ "provider": "alicloud", "from": "2026-08", "to": "2026-09", "force": true })
            .to_string(),
        &[],
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["task_id"], "t-1");
    assert_eq!(body["provider"], "alicloud");

    // 发给 goscan 的是它自己那套字段名；sync_mode 不能空（goscan 会拒），阿里云默认两种粒度都拉
    let sent: serde_json::Value = serde_json::from_str(&goscan.last_request().body).unwrap();
    assert_eq!(sent["provider"], "alicloud");
    assert_eq!(sent["sync_mode"], "standard");
    assert_eq!(sent["granularity"], "both");
    assert_eq!(sent["start_period"], "2026-08");
    assert_eq!(sent["end_period"], "2026-09");
    assert_eq!(sent["force_update"], true);
    assert_eq!(goscan.last_request().target, "/sync");

    // 轮任务状态：跑完了带上写了多少条
    goscan.respond(
        r#"{"id":"t-1","status":"completed","provider":"alicloud","start_time":"2026-09-22T17:00:00+08:00","result":{"success":true,"records_processed":1200,"records_fetched":1200,"message":"ok"}}"#,
    );
    let (status, body) = get_json(&app, "/api/bills/sync/t-1").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["done"], true);
    assert_eq!(body["ok"], true);
    assert_eq!(body["records"], 1200);
    assert_eq!(goscan.last_request().target, "/tasks/t-1");

    // 还在跑的时候 done = false，页面据此接着轮
    goscan.respond(r#"{"id":"t-1","status":"running","provider":"alicloud"}"#);
    let (_, body) = get_json(&app, "/api/bills/sync/t-1").await;
    assert_eq!(body["done"], false);
    assert_eq!(body["ok"], false);
}

/// 火山没有粒度这一说；参数写错、没配 goscan 的时候都要说人话。
#[tokio::test]
async fn sync_rejects_what_goscan_would_reject() {
    let fake = FakeClickhouse::start().await;
    let goscan = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &["--goscan-url", goscan.endpoint()]).await;

    for (body, needle) in [
        (serde_json::json!({ "provider": "tencent" }), "provider"),
        (serde_json::json!({ "provider": "alicloud", "granularity": "hourly" }), "granularity"),
        (serde_json::json!({ "provider": "alicloud", "mode": "turbo" }), "mode"),
        (serde_json::json!({ "provider": "alicloud", "from": "2026-9" }), "YYYY-MM"),
    ] {
        let (status, out) = post_json(&app, "/api/bills/sync", &body.to_string(), &[]).await;
        assert_eq!(status, 400, "{out}");
        assert!(out["error"].as_str().unwrap().contains(needle), "{out}");
    }

    // goscan 说「已经有一个在跑」：原样透 409 出去，别折成 500
    goscan.respond_with(409, Vec::new(), r#"{"error":true,"message":"task already running"}"#);
    let (status, out) = post_json(&app, "/api/bills/sync", r#"{"provider":"alicloud"}"#, &[]).await;
    assert_eq!(status, 409, "{out}");
    assert!(out["error"].as_str().unwrap().contains("已有同步任务正在执行"), "{out}");
    assert_eq!(out["kind"], "busy");

    // 没配 --goscan-url 的部署：接口说清楚为什么，/api/meta 里 sync = false
    let bare = FakeClickhouse::start().await;
    let app = app_with_bills(&bare, "", &[]).await;
    let (_, meta) = get_json(&app, "/api/meta").await;
    assert_eq!(meta["bills"]["sync"], false);
    let (status, out) = post_json(&app, "/api/bills/sync", r#"{"provider":"alicloud"}"#, &[]).await;
    assert_eq!(status, 400, "{out}");
    assert!(out["error"].as_str().unwrap().contains("goscan-url"), "{out}");
}
