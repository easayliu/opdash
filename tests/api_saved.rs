//! `/api/saved`：收藏的查询按人归属——登录的归账号名，没开认证的大家共用一份。
//!
//! 存储本身（去重、上限、校验、重读文件）在 `src/saved.rs` 的单元测试里，这里看的是 HTTP 形状
//! 和身份怎么接进来。

mod support;

use support::*;

#[tokio::test]
async fn crud_without_auth_is_shared() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;

    // 一开始是空的
    let (status, list) = get_json(&app, "/api/saved").await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(list["queries"].as_array().unwrap().len(), 0);
    assert_eq!(list["max"], 200);

    // 收藏：201，名字 / 地址原样回来，开头的 ? 去掉
    let (status, a) = post_json(
        &app,
        "/api/saved",
        r#"{"name": "订单超时", "path": "/logs", "query": "?q=timeout&level=ERROR"}"#,
        &[],
    )
    .await;
    assert_eq!(status, 201, "{a}");
    assert_eq!(a["name"], "订单超时");
    assert_eq!(a["path"], "/logs");
    assert_eq!(a["query"], "q=timeout&level=ERROR");
    assert_eq!(a["id"].as_str().unwrap().len(), 12);
    assert!(a["created_at"].as_str().unwrap().contains('T'), "RFC3339: {a}");
    assert!(a.get("user").is_none(), "列表全是本人的，不用回 user");

    // 没起名 → 用地址当名字；query 可以不给
    let (status, b) = post_json(&app, "/api/saved", r#"{"path": "/services"}"#, &[]).await;
    assert_eq!(status, 201, "{b}");
    assert_eq!(b["name"], "/services");
    assert_eq!(b["query"], "");

    // 同一个地址再收藏一次：409，带上已有那条的 id
    let (status, dup) = post_json(
        &app,
        "/api/saved",
        r#"{"name": "又一条", "path": "/logs", "query": "q=timeout&level=ERROR"}"#,
        &[],
    )
    .await;
    assert_eq!(status, 409, "{dup}");
    assert_eq!(dup["kind"], "conflict");
    assert_eq!(dup["existing"], a["id"]);

    // 参数不对都是 400
    for (body, why) in [
        (r#"{"name": "x"}"#, "缺 path"),
        (r#"{"path": "logs"}"#, "不是 / 开头"),
        (r#"{"path": "//evil.example.com/"}"#, "协议相对地址"),
        (r#"{"path": "/logs?q=1"}"#, "path 里带 ?"),
        (r#"{"path": "/logs", "query": "q=1#x"}"#, "query 里带 #"),
        ("not json", "不是 JSON"),
    ] {
        let (status, err) = post_json(&app, "/api/saved", body, &[]).await;
        assert_eq!(status, 400, "{why}: {err}");
        assert_eq!(err["kind"], "bad_request", "{why}");
    }

    // 列表新的在前
    let (_, list) = get_json(&app, "/api/saved").await;
    let ids: Vec<&str> =
        list["queries"].as_array().unwrap().iter().map(|q| q["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [b["id"].as_str().unwrap(), a["id"].as_str().unwrap()]);

    // 改名；换地址要 path + query 一起
    let id = a["id"].as_str().unwrap();
    let (status, renamed) =
        put_json(&app, &format!("/api/saved/{id}"), r#"{"name": "超时"}"#, &[]).await;
    assert_eq!(status, 200, "{renamed}");
    assert_eq!(renamed["name"], "超时");
    assert_eq!(renamed["query"], "q=timeout&level=ERROR", "只改名不动地址");
    let (status, err) =
        put_json(&app, &format!("/api/saved/{id}"), r#"{"path": "/traces"}"#, &[]).await;
    assert_eq!(status, 400, "{err}");
    let (status, moved) = put_json(
        &app,
        &format!("/api/saved/{id}"),
        r#"{"path": "/traces", "query": "service=order&error_only=1"}"#,
        &[],
    )
    .await;
    assert_eq!(status, 200, "{moved}");
    assert_eq!(moved["path"], "/traces");
    assert_eq!(moved["name"], "超时");
    let (status, err) = put_json(&app, "/api/saved/000000000000", r#"{"name": "x"}"#, &[]).await;
    assert_eq!(status, 404, "{err}");
    assert_eq!(err["kind"], "not_found");

    // 删：204，再删 404
    let (status, _, _) = delete_full(&app, &format!("/api/saved/{id}"), &[]).await;
    assert_eq!(status, 204);
    let (status, _, _) = delete_full(&app, &format!("/api/saved/{id}"), &[]).await;
    assert_eq!(status, 404);
    let (_, list) = get_json(&app, "/api/saved").await;
    assert_eq!(list["queries"].as_array().unwrap().len(), 1);

    // 没开认证：另一个 app 打开同一个文件看到的也是这一份（大家共用）
    let saved_file = {
        let (_, meta) = get_json(&app, "/api/meta").await;
        let _ = meta;
        // app_with_schema 给每个 app 起了自己的文件；这里直接读 app 用的那个不方便，
        // 改用显式文件再验证一遍「共用」和「重启不丢」
        std::env::temp_dir().join(format!("opdash-saved-shared-{}.json", std::process::id()))
    };
    let saved_file = saved_file.to_string_lossy().into_owned();
    let args = ["--saved-query-file", &saved_file];
    let one = app_with_schema(&fake, &args).await;
    let (status, _) = post_json(&one, "/api/saved", r#"{"path": "/errors"}"#, &[]).await;
    assert_eq!(status, 201);
    let two = app_with_schema(&fake, &args).await;
    let (_, list) = get_json(&two, "/api/saved").await;
    assert_eq!(list["queries"][0]["path"], "/errors", "存在文件里，重启后照样有");
    let raw = std::fs::read_to_string(&saved_file).unwrap();
    assert!(raw.contains(r#""user": "*""#), "没开认证归在 * 名下: {raw}");
    std::fs::remove_file(&saved_file).ok();
}

#[tokio::test]
async fn with_auth_saved_queries_belong_to_the_account() {
    let fake = FakeClickhouse::start().await;
    let saved_file =
        std::env::temp_dir().join(format!("opdash-saved-auth-{}.json", std::process::id()));
    let saved_file = saved_file.to_string_lossy().into_owned();
    let app =
        app_with_schema(&fake, &["--basic-auth", "ops:secret", "--saved-query-file", &saved_file])
            .await;
    let basic = [("authorization", "Basic b3BzOnNlY3JldA==")];

    // 没登录：401，和别的接口一样
    let (status, _) = get_raw(&app, "/api/saved", &[]).await;
    assert_eq!(status, 401);
    let (status, _, _) = post_full(&app, "/api/saved", r#"{"path": "/logs"}"#, &[]).await;
    assert_eq!(status, 401);

    // 登录的人收藏：归在账号名下
    let (status, q) = post_json(
        &app,
        "/api/saved",
        r#"{"name": "错误日志", "path": "/logs", "query": "level=ERROR"}"#,
        &basic,
    )
    .await;
    assert_eq!(status, 201, "{q}");
    let raw = std::fs::read_to_string(&saved_file).unwrap();
    assert!(raw.contains(r#""user": "ops""#), "{raw}");
    assert!(!raw.contains(r#""user": "*""#), "{raw}");

    // 拿这个人的 API key 进来：key 代表本人，看到的是同一份
    let (status, key) = post_json(&app, "/api/auth/keys", r#"{"name": "cli"}"#, &basic).await;
    assert_eq!(status, 200, "{key}");
    let bearer = format!("Bearer {}", key["key"].as_str().unwrap());
    let (status, list) = get_json_with(&app, "/api/saved", &[("authorization", &bearer)]).await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(list["queries"][0]["name"], "错误日志");
    let id = q["id"].as_str().unwrap();
    let (status, _, _) =
        delete_full(&app, &format!("/api/saved/{id}"), &[("authorization", &bearer)]).await;
    assert_eq!(status, 204, "key 也能删本人的");
    let (_, list) = get_json_with(&app, "/api/saved", &basic).await;
    assert_eq!(list["queries"].as_array().unwrap().len(), 0);
    std::fs::remove_file(&saved_file).ok();
}
