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

/// 账期列表那条查询只 `SELECT DISTINCT period`，回来的行里**没有金额**。
///
/// 这条测试是补的：原来的实现拿「账期 + 金额」那个行结构去解它，空表时一行都没有、错藏得
/// 严严实实，一有数据就是「解析 ClickHouse 第 1 行结果失败: missing field `amount`」。
#[tokio::test]
async fn periods_parses_rows_without_an_amount() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &[]).await;
    fake.respond_to("volcengine_bill", "{\"period\":\"2026-08\"}\n{\"period\":\"2026-09\"}\n")
        .respond_to("alicloud_bill_monthly", "{\"period\":\"2026-09\"}\n");

    let (status, body) = get_json(&app, "/api/bills/periods").await;
    assert_eq!(status, 200, "{body}");
    // 两朵云的账期并起来去重、排序
    assert_eq!(body["periods"], serde_json::json!(["2026-08", "2026-09"]));
    assert_eq!(body["latest"], "2026-09");

    // 查的确实只有账期那一列：多查一列就会把「一进页面就扫全表」的代价带回来
    let sql = sql_for(&fake, "volcengine_bill");
    assert!(sql.contains("SELECT DISTINCT BillPeriod AS period"), "{sql}");
    assert!(!sql.contains("amount"), "{sql}");
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
    // 库里读来的排序键打头，金额列跟在后面：完全相同的副本只算一次，只差金额的并列行各自保留
    assert!(
        volc.contains(
            "GROUP BY `BillPeriod`, `InstanceNo`, `Product`, `PayableAmount`, `PaidAmount`, `OriginalBillAmount`\n"
        ),
        "{volc}"
    );
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
    // goscan 的时间是 Go 的 time.Time：没结束的任务 end_time 报零值，而不是 null。
    // 原样转给页面，页面会当成已经结束，停表并显示「已用 0 秒」
    goscan.respond(
        r#"{"id":"t-1","status":"running","provider":"alicloud","start_time":"2026-09-23T09:10:00+08:00","end_time":"0001-01-01T00:00:00Z"}"#,
    );
    let (_, body) = get_json(&app, "/api/bills/sync/t-1").await;
    assert_eq!(body["done"], false);
    assert_eq!(body["ok"], false);
    assert_eq!(body["started_at"], "2026-09-23T09:10:00+08:00");
    assert!(body["ended_at"].is_null(), "零值时间应当视为尚未结束: {body}");

    // 进度的单位是「趟」：两个账期各拉月表、日表，共四趟。粒度要一并透出，
    // 否则同一个账期出现两次，看着像卡住了
    goscan.respond(
        r#"{"id":"t-1","status":"running","provider":"alicloud","progress":{"period":"2026-09","granularity":"daily","done":3,"total":4}}"#,
    );
    let (_, body) = get_json(&app, "/api/bills/sync/t-1").await;
    assert_eq!(body["progress"]["period"], "2026-09");
    assert_eq!(body["progress"]["granularity"], "daily");
    assert_eq!(body["progress"]["periods_done"], 3);
    assert_eq!(body["progress"]["periods_total"], 4);

    // 火山不分粒度，老版本 goscan 也不报，这个字段就不该出现
    goscan.respond(
        r#"{"id":"t-1","status":"running","provider":"volcengine","progress":{"period":"2026-09","done":0,"total":2}}"#,
    );
    let (_, body) = get_json(&app, "/api/bills/sync/t-1").await;
    assert!(body["progress"].get("granularity").is_none(), "{body}");
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

// ---------------------------------------------------------------------------------------------
// 成本归属（`--bill-alloc`）
// ---------------------------------------------------------------------------------------------

/// 写一份归属规则到临时文件，返回路径。
fn alloc_file(body: &str) -> String {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "opdash-test-alloc-{}-{}.toml",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&path, body).expect("写归属规则");
    path.to_string_lossy().into_owned()
}

const ALLOC: &str = r#"
lines = ["甲线", "乙线", "公共"]
unmatched = "公共"

[[include]]
subscription = ["PayAsYouGo"]

[[rules]]
name = "甲线专用机器"
product = ["云服务器 ECS"]
to = "甲线"
columns = [{ name = "intranet_ip", any_of = ["10.0.0.1"] }]

[[rules]]
name = "ECS 其余部分"
product = ["云服务器 ECS"]
split = { "甲线" = 1, "乙线" = 3 }
"#;

/// 一笔费用按规则摊到业务线：专属的整笔归一条线，其余的按权重拆，没命中规则的单列出来
/// （配了 unmatched 则同时计入那条线）。
#[tokio::test]
async fn allocation_splits_by_the_configured_rules() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &["--bill-alloc", &alloc_file(ALLOC)]).await;

    let (_, meta) = get_json(&app, "/api/meta").await;
    assert_eq!(meta["bills"]["allocation"]["lines"], serde_json::json!(["甲线", "乙线", "公共"]));
    assert_eq!(meta["bills"]["allocation"]["rules"], 2);

    fake.respond_to(
        "_rule, _product",
        concat!(
            r#"{"rule":0,"product":"云服务器 ECS","amount":300}"#,
            "\n",
            r#"{"rule":1,"product":"云服务器 ECS","amount":400}"#,
            "\n",
            r#"{"rule":-1,"product":"对象存储","amount":100}"#,
            "\n",
        ),
    );
    fake.respond_to(
        "_rule, _bucket",
        concat!(
            r#"{"rule":0,"bucket":"2026-09-20","amount":150}"#,
            "\n",
            r#"{"rule":0,"bucket":"2026-09-21","amount":150}"#,
            "\n",
            r#"{"rule":1,"bucket":"2026-09-20","amount":400}"#,
            "\n",
            r#"{"rule":-1,"bucket":"2026-09-21","amount":100}"#,
            "\n",
        ),
    );
    let (status, body) =
        get_json(&app, "/api/bills/allocation?from=2026-09&to=2026-09&provider=alicloud&days=7")
            .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["configured"], true);
    assert_eq!(body["total"], 800.0);
    // 两天的账单，日均按这两天算，而不是按自然月的 30 天
    assert_eq!(body["days"], 2);
    assert_eq!(body["daily"], 400.0);
    assert_eq!(body["granularity"], "daily");

    let lines = body["lines"].as_array().unwrap();
    let line = |name: &str| {
        lines.iter().find(|l| l["name"] == name).unwrap_or_else(|| panic!("没有业务线 {name}"))
    };
    // 甲线 = 专属的 300 + 其余 400 的四分之一
    assert_eq!(line("甲线")["amount"], 400.0);
    assert_eq!(line("乙线")["amount"], 300.0);
    // 没命中规则的 100 按 unmatched 归入公共，同时单列出来提醒「还有钱没写进规则」
    assert_eq!(line("公共")["amount"], 100.0);
    assert_eq!(body["unmatched"]["amount"], 100.0);
    assert_eq!(body["unmatched"]["items"][0]["product"], "对象存储");

    // 同一个产品由两条规则分别归来，行上标出是哪一条
    let items = line("甲线")["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().any(|i| i["rule"] == "甲线专用机器" && i["amount"] == 300.0), "{items:?}");
    assert!(items.iter().any(|i| i["rule"] == "ECS 其余部分" && i["amount"] == 100.0), "{items:?}");

    // 不分业务线的那份照样在：没配规则的部署只看得到它
    assert_eq!(body["products"][0]["product"], "云服务器 ECS");
    assert_eq!(body["products"][0]["amount"], 700.0);

    // 趋势按天，每一天按同一套规则拆开
    let points = body["points"].as_array().unwrap();
    assert_eq!(points.len(), 2);
    assert_eq!(points[0]["t"], "2026-09-20");
    assert_eq!(points[0]["by_line"]["甲线"], 250.0);
    assert_eq!(points[0]["by_line"]["乙线"], 300.0);
    assert_eq!(points[1]["by_line"]["公共"], 100.0);

    // 规则进了 SQL：分类在库里做，取值一律绑参数
    let q = sql_for(&fake, "_rule, _product");
    assert!(q.contains("multiIf("), "{q}");
    assert!(q.contains("`intranet_ip` IN {"), "{q}");
    assert!(!q.contains("10.0.0.1"), "{q}");
}

/// 没配 `--bill-alloc` 的部署照样能看日均与预估，只是没有业务线这一层。
#[tokio::test]
async fn allocation_without_rules_still_gives_the_daily_average() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &[]).await;

    let (_, meta) = get_json(&app, "/api/meta").await;
    assert!(meta["bills"]["allocation"].is_null(), "{meta}");

    fake.respond_to(
        "_rule, _product",
        "{\"rule\":-1,\"product\":\"云服务器 ECS\",\"amount\":600}\n",
    );
    fake.respond_to(
        "_rule, _bucket",
        concat!(
            r#"{"rule":-1,"bucket":"2026-09-20","amount":300}"#,
            "\n",
            r#"{"rule":-1,"bucket":"2026-09-21","amount":300}"#,
            "\n",
        ),
    );
    let (status, body) =
        get_json(&app, "/api/bills/allocation?from=2026-09&to=2026-09&provider=alicloud").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["configured"], false);
    assert_eq!(body["lines"].as_array().unwrap().len(), 0);
    assert_eq!(body["products"][0]["daily"], 300.0);
    assert_eq!(body["unmatched"]["amount"], 600.0);
    // 一条规则都没有：分类表达式退化成常量，不会在 SQL 里留下空的 multiIf
    let q = sql_for(&fake, "_rule, _product");
    assert!(q.contains("toInt32(-1)"), "{q}");
    assert!(!q.contains("multiIf("), "{q}");
}

/// 规则文件写错时启动就该失败。进程里退而求其次的那条路（记一条错、停用分摊）也要走得通。
#[tokio::test]
async fn a_broken_rule_file_disables_allocation() {
    let path = alloc_file("lines = [\"甲\"]\n[[rules]]\nname = \"x\"\nproduct = [\"ECS\"]\n");
    assert!(opdash::alloc::Alloc::load(std::path::Path::new(&path)).is_err());

    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &["--bill-alloc", &path]).await;
    let (_, meta) = get_json(&app, "/api/meta").await;
    assert!(meta["bills"]["allocation"].is_null(), "{meta}");
}

/// 「只看最近 N 天」要有日度账单；阿里云只同步了月度时说清楚怎么办。
#[tokio::test]
async fn window_needs_daily_bills() {
    let fake = FakeClickhouse::start().await;
    let columns = bill_columns("")
        .lines()
        .filter(|l| !l.contains(r#""table":"alicloud_bill_daily""#))
        .map(|l| format!("{l}\n"))
        .collect::<String>();
    fake.respond(format!("{}{}", columns_fixture(), columns))
        .respond(version_fixture())
        .respond(sorting_keys_fixture(""));
    let app = support::app(&fake, &[]).await;

    let (status, body) =
        get_json(&app, "/api/bills/allocation?from=2026-09&to=2026-09&provider=alicloud&days=7")
            .await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"].as_str().unwrap().contains("granularity=daily"), "{body}");
}

const ALLOC_WITH_PREPAID: &str = r#"
lines = ["甲线", "乙线", "公共"]
unmatched = "公共"

[[rules]]
name = "甲线专用机器"
product = ["云服务器 ECS"]
to = "甲线"
columns = [{ name = "intranet_ip", any_of = ["10.0.0.1"] }]

[[rules]]
name = "ECS 其余部分"
product = ["云服务器 ECS"]
split = { "甲线" = 1, "乙线" = 3 }

[prepaid]
subscription = ["Subscription", "包年包月"]
"#;

/// 预付费按服务期摊到各月：包年的一笔在十二个月里各计十二分之一，摊到查询区间之后的那几个月
/// 也留着（页面据此算下月预估）。日均只按后付费算——预付费是按月摊的，除以天数没有意义。
#[tokio::test]
async fn prepaid_is_amortized_over_its_service_period() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &["--bill-alloc", &alloc_file(ALLOC_WITH_PREPAID)]).await;

    fake.respond_to(
        "GROUP BY _rule, _product\n",
        concat!(
            r#"{"rule":0,"product":"云服务器 ECS","amount":300}"#,
            "\n",
            r#"{"rule":1,"product":"云服务器 ECS","amount":400}"#,
            "\n",
            r#"{"rule":-1,"product":"对象存储","amount":100}"#,
            "\n",
        ),
    );
    fake.respond_to(
        "_rule, _bucket",
        concat!(
            r#"{"rule":0,"bucket":"2026-09-20","amount":400}"#,
            "\n",
            r#"{"rule":-1,"bucket":"2026-09-21","amount":400}"#,
            "\n",
        ),
    );
    // 6 月买的一年期机器 1200（每月 100），9 月买的一个月期 300
    fake.respond_to(
        "_months",
        concat!(
            r#"{"rule":0,"product":"云服务器 ECS","period":"2026-06","months":12,"amount":1200}"#,
            "\n",
            r#"{"rule":-1,"product":"云数据库 RDS","period":"2026-09","months":1,"amount":300}"#,
            "\n",
        ),
    );
    let (status, body) =
        get_json(&app, "/api/bills/allocation?from=2026-09&to=2026-09&provider=alicloud").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["prepaid"], true);
    assert_eq!(body["postpaid"], 800.0);
    // 落在 9 月的摊销：一年期的 100 + 一个月期的 300
    assert_eq!(body["amortized"], 400.0);
    assert_eq!(body["total"], 1200.0);
    // 日均只按后付费：800 ÷ 2 天
    assert_eq!(body["daily"], 400.0);
    // 10 月起只剩那台包年机器的 100，页面用它算下月预估
    assert_eq!(body["amortized_by_period"]["2026-09"], 400.0);
    assert_eq!(body["amortized_by_period"]["2026-10"], 100.0);
    assert_eq!(body["amortized_by_period"]["2027-05"], 100.0);
    assert!(body["amortized_by_period"]["2027-06"].is_null(), "12 个月摊完了: {body}");

    let lines = body["lines"].as_array().unwrap();
    let line = |name: &str| lines.iter().find(|l| l["name"] == name).unwrap();
    // 甲线 = 后付费 400（300 + 400 的四分之一）+ 摊过来的 100
    assert_eq!(line("甲线")["postpaid"], 400.0);
    assert_eq!(line("甲线")["amortized"], 100.0);
    assert_eq!(line("甲线")["amount"], 500.0);
    assert_eq!(line("甲线")["amortized_by_period"]["2026-10"], 100.0);
    // 摊销那一行标了 prepaid，且不给日均
    let items = line("甲线")["items"].as_array().unwrap();
    let prepaid_item = items.iter().find(|i| i["prepaid"] == true).unwrap();
    assert_eq!(prepaid_item["amount"], 100.0);
    assert!(prepaid_item["daily"].is_null(), "{prepaid_item}");
    // 没命中规则的 300 也算未归属
    assert_eq!(body["unmatched"]["amount"], 400.0);

    // 预付费从「按发生月计入」那条路里排掉了，不会两头各算一次
    let as_billed = sql_for(&fake, "GROUP BY _rule, _product\n");
    assert!(as_billed.contains("AND NOT (subscription_type IN {"), "{as_billed}");
}

/// 没配 `[prepaid]` 就还是只统计后付费，包年包月按出账当月原样计入。
#[tokio::test]
async fn without_the_prepaid_section_nothing_is_amortized() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &["--bill-alloc", &alloc_file(ALLOC)]).await;

    fake.respond_to(
        "GROUP BY _rule, _product\n",
        "{\"rule\":-1,\"product\":\"云服务器 ECS\",\"amount\":600}\n",
    );
    fake.respond_to("_rule, _bucket", "{\"rule\":-1,\"bucket\":\"2026-09-20\",\"amount\":600}\n");
    let (status, body) =
        get_json(&app, "/api/bills/allocation?from=2026-09&to=2026-09&provider=alicloud").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["prepaid"], false);
    assert_eq!(body["amortized"], 0.0);
    assert_eq!(body["total"], 600.0);
    // 没有摊销那条查询
    assert!(!sql(&fake).iter().any(|s| s.contains("_months")), "不该有摊销查询");
    assert!(!sql_for(&fake, "GROUP BY _rule, _product\n").contains("AND NOT"));
}

/// 日均按每朵云自己的天数折算再相加。两朵云的同步进度常常不一样——这里火山有两天、阿里云的
/// 日度账单只有一天；若把天数取并集（两天）当分母，阿里云那一天的钱会被摊薄成一半。
#[tokio::test]
async fn daily_average_uses_each_clouds_own_day_count() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &["--bill-alloc", &alloc_file(ALLOC)]).await;

    fake.respond_to(
        "any(if(ProductZh != '', ProductZh, Product)) AS _product",
        "{\"rule\":-1,\"product\":\"云服务器\",\"amount\":200}\n",
    );
    fake.respond_to(
        "any(ifNull(toString(toDateOrNull(ExpenseDate)), '')) AS _bucket",
        concat!(
            r#"{"rule":-1,"bucket":"2026-09-20","amount":100}"#,
            "\n",
            r#"{"rule":-1,"bucket":"2026-09-21","amount":100}"#,
            "\n",
        ),
    );
    fake.respond_to(
        "any(if(product_name != '', product_name, product_code)) AS _product",
        "{\"rule\":-1,\"product\":\"对象存储\",\"amount\":900}\n",
    );
    fake.respond_to(
        "any(toString(billing_date)) AS _bucket",
        "{\"rule\":-1,\"bucket\":\"2026-09-21\",\"amount\":900}\n",
    );
    let (status, body) = get_json(&app, "/api/bills/allocation?from=2026-09&to=2026-09").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["days_by_provider"]["volcengine"], 2);
    assert_eq!(body["days_by_provider"]["alicloud"], 1);
    // 火山 200 ÷ 2 天 + 阿里云 900 ÷ 1 天；取并集的话会是 1100 ÷ 2 = 550
    assert_eq!(body["daily"], 1000.0);
    // 按产品的日均同样各按各的天数
    let products = body["products"].as_array().unwrap();
    let oss = products.iter().find(|p| p["product"] == "对象存储").unwrap();
    assert_eq!(oss["daily"], 900.0);
}

/// 摊销固定读月度表：日度表往往只补了最近几天，没有三年前的购买记录。
/// 日度账单明显少于月度账单时，页面要能知道，否则区间合计偏低而无人察觉。
#[tokio::test]
async fn prepaid_reads_the_monthly_table_and_thin_daily_bills_are_flagged() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &["--bill-alloc", &alloc_file(ALLOC_WITH_PREPAID)]).await;

    fake.respond_to(
        "GROUP BY _rule, _product\n",
        "{\"rule\":-1,\"product\":\"对象存储\",\"amount\":100}\n",
    );
    fake.respond_to("_rule, _bucket", "{\"rule\":-1,\"bucket\":\"2026-09-22\",\"amount\":100}\n");
    // 一笔五年期的购买：服务期不截断，五年摊满（lookback_months 只管往前找多远）
    fake.respond_to(
        "_months",
        "{\"rule\":-1,\"product\":\"云服务器 ECS\",\"period\":\"2026-09\",\"months\":60,\"amount\":6000}\n",
    );
    // 日度表一整月只有 100，月度表同一段有 3000
    fake.respond_to(
        "AS rows\nFROM (\n  SELECT any(toFloat64(pretax_amount)) AS _amount\n  FROM `logs`.`alicloud_bill_daily`",
        "{\"amount\":100,\"rows\":1}\n",
    );
    fake.respond_to(
        "AS rows\nFROM (\n  SELECT any(toFloat64(pretax_amount)) AS _amount\n  FROM `logs`.`alicloud_bill_monthly`",
        "{\"amount\":3000,\"rows\":9}\n",
    );
    let (status, body) =
        get_json(&app, "/api/bills/allocation?from=2026-09&to=2026-09&provider=alicloud").await;
    assert_eq!(status, 200, "{body}");

    let amortize = sql_for(&fake, "_months");
    assert!(amortize.contains("`logs`.`alicloud_bill_monthly`"), "摊销应读月度表: {amortize}");
    assert_eq!(body["amortized"], 100.0, "6000 ÷ 60 个月: {body}");
    assert_eq!(body["amortized_by_period"]["2027-09"], 100.0);

    assert_eq!(body["coverage"]["provider"], "alicloud");
    assert_eq!(body["coverage"]["daily"], 100.0);
    assert_eq!(body["coverage"]["monthly"], 3000.0);

    // 未归属的两段各归各的：后付费 100、摊销 100，日均只按后付费
    assert_eq!(body["unmatched"]["postpaid"], 100.0);
    assert_eq!(body["unmatched"]["amortized"], 100.0);
    assert_eq!(body["unmatched"]["amount"], 200.0);
    assert_eq!(body["unmatched"]["daily"], 100.0);
}

/// 只看最近 N 天时，后付费只有这 N 天的钱，摊销却是整月的：落在区间内的摊销要按天折算到
/// 同样的天数，区间合计和各线占比才不会被摊销撑大。按月的那份（用于预估）不折算。
#[tokio::test]
async fn a_short_window_scales_the_amortized_part_to_the_same_days() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_bills(&fake, "", &["--bill-alloc", &alloc_file(ALLOC_WITH_PREPAID)]).await;

    fake.respond_to(
        "GROUP BY _rule, _product\n",
        "{\"rule\":-1,\"product\":\"对象存储\",\"amount\":70}\n",
    );
    fake.respond_to("_rule, _bucket", "{\"rule\":-1,\"bucket\":\"2026-09-22\",\"amount\":70}\n");
    fake.respond_to(
        "_months",
        "{\"rule\":-1,\"product\":\"云服务器 ECS\",\"period\":\"2026-09\",\"months\":1,\"amount\":3000}\n",
    );
    let (status, body) =
        get_json(&app, "/api/bills/allocation?from=2026-09&to=2026-09&provider=alicloud&days=7")
            .await;
    assert_eq!(status, 200, "{body}");
    // 9 月 30 天，窗口 7 天：3000 × 7 / 30
    assert_eq!(body["amortized"], 700.0);
    assert_eq!(body["total"], 770.0);
    assert_eq!(body["amortized_by_period"]["2026-09"], 3000.0, "预估用的月度摊销不折算");
}
