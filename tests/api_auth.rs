//! OIDC 登录流程：假 Keycloak 回放 discovery 和 token 响应，走一遍 login → callback → 带会话访问 → logout。

mod support;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use support::*;

/// 假 Keycloak：和假 ClickHouse 是同一个「按顺序回放」的服务，只是路径不同。
type FakeKeycloak = FakeClickhouse;

fn discovery(issuer: &str) -> String {
    serde_json::json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/protocol/openid-connect/auth"),
        "token_endpoint": format!("{issuer}/protocol/openid-connect/token"),
        "end_session_endpoint": format!("{issuer}/protocol/openid-connect/logout"),
    })
    .to_string()
}

fn jwt(payload: serde_json::Value) -> String {
    format!("eyJhbGciOiJSUzI1NiJ9.{}.c2ln", B64.encode(payload.to_string()))
}

fn id_token(issuer: &str, nonce: &str, extra: serde_json::Value) -> String {
    let mut payload = serde_json::json!({
        "iss": issuer, "aud": "opdash", "exp": chrono::Utc::now().timestamp() + 300,
        "sub": "u-1", "preferred_username": "alice", "email": "alice@example.com", "nonce": nonce,
    });
    for (k, v) in extra.as_object().unwrap() {
        payload[k] = v.clone();
    }
    jwt(payload)
}

fn query_map(url: &str) -> std::collections::HashMap<String, String> {
    let q = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    form_urlencoded::parse(q.as_bytes()).into_owned().collect()
}

/// Set-Cookie 里的 `name=value` 部分。
fn cookie_pair<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .filter(|(k, _)| k == "set-cookie")
        .map(|(_, v)| v.split(';').next().unwrap_or(""))
        .find(|kv| kv.starts_with(&format!("{name}=")))
}

/// 发起登录，拿回登录票 cookie 和授权 URL 里的参数。
async fn begin(app: &axum::Router) -> (String, std::collections::HashMap<String, String>) {
    let (status, headers, _) = get_full(app, "/api/auth/login", &[]).await;
    assert_eq!(status, 303);
    let q = query_map(header_value(&headers, "location").unwrap());
    (cookie_pair(&headers, "opdash_login").unwrap().to_owned(), q)
}

async fn callback(
    app: &axum::Router,
    ticket: &str,
    state: &str,
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    get_full(app, &format!("/api/auth/callback?code=abc&state={state}"), &[("cookie", ticket)])
        .await
}

async fn oidc_app(fake: &FakeClickhouse, kc: &FakeKeycloak, extra: &[&str]) -> axum::Router {
    let issuer = format!("{}/realms/ops", kc.endpoint());
    let mut args = vec![
        "--oidc-issuer",
        issuer.as_str(),
        "--oidc-client-id",
        "opdash",
        "--oidc-client-secret",
        "s3cret",
        "--public-url",
        "https://opdash.example.com",
        "--session-secret",
        "test-key",
    ];
    args.extend_from_slice(extra);
    app_with_schema(fake, &args).await
}

#[tokio::test]
async fn unauthenticated_requests_are_redirected_or_401() {
    let fake = FakeClickhouse::start().await;
    let kc = FakeKeycloak::start().await;
    let app = oidc_app(&fake, &kc, &[]).await;

    // 浏览器导航：302 去登录，带上原路径
    let (status, headers, _) =
        get_full(&app, "/traces?range=1h", &[("accept", "text/html,*/*")]).await;
    assert_eq!(status, 303);
    assert_eq!(
        header_value(&headers, "location"),
        Some("/api/auth/login?next=%2Ftraces%3Frange%3D1h")
    );

    // API / 静态资源：401 JSON，不带 WWW-Authenticate（别弹 Basic 框）
    let (status, headers, body) =
        get_full(&app, "/api/meta", &[("accept", "application/json")]).await;
    assert_eq!(status, 401);
    assert!(header_value(&headers, "www-authenticate").is_none());
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["kind"], "unauthenticated");
    assert_eq!(body["login_url"], "/api/auth/login");
    let (status, _) = get_raw(&app, "/assets/app.js", &[]).await;
    assert_eq!(status, 401);

    // 没登录时 /api/auth/me 也是 200，前端据此跳登录
    let (status, me) = get_json(&app, "/api/auth/me").await;
    assert_eq!(status, 200);
    assert_eq!(me["mode"], "oidc");
    assert!(me["user"].is_null());

    // 健康检查不认证（这里还没人查过表，schema 没加载，所以是 503 而不是 200；重点是不是 401）
    let (status, health) = get_json(&app, "/api/health").await;
    assert_ne!(status, 401, "{health}");
    assert!(health["ok"].is_boolean(), "{health}");
}

#[tokio::test]
async fn full_login_flow_sets_session_and_logout_clears_it() {
    let fake = FakeClickhouse::start().await;
    let kc = FakeKeycloak::start().await;
    let issuer = format!("{}/realms/ops", kc.endpoint());
    let app = oidc_app(&fake, &kc, &[]).await;

    // 1. /login：读 discovery，签登录票，跳授权页
    kc.respond(discovery(&issuer));
    let (status, headers, _) = get_full(&app, "/api/auth/login?next=/logs%3Fq%3Dx", &[]).await;
    assert_eq!(status, 303);
    let location = header_value(&headers, "location").unwrap();
    assert!(location.starts_with(&format!("{issuer}/protocol/openid-connect/auth?")), "{location}");
    let q = query_map(location);
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], "opdash");
    assert_eq!(q["redirect_uri"], "https://opdash.example.com/api/auth/callback");
    assert_eq!(q["scope"], "openid profile email");
    assert_eq!(q["code_challenge_method"], "S256");
    let ticket = cookie_pair(&headers, "opdash_login").unwrap().to_owned();
    let set_cookie = header_value(&headers, "set-cookie").unwrap();
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("Secure"), "{set_cookie}");
    assert!(set_cookie.contains("Path=/api/auth"), "{set_cookie}");

    // 2. /callback：state 不对不认
    let (status, _, _) =
        get_full(&app, "/api/auth/callback?code=abc&state=wrong", &[("cookie", &ticket)]).await;
    assert_eq!(status, 400);
    // 没有登录票也不认
    let (status, _, _) =
        get_full(&app, &format!("/api/auth/callback?code=abc&state={}", q["state"]), &[]).await;
    assert_eq!(status, 400);

    // 3. /callback 正常：换 token，签会话，跳回 next
    kc.respond(
        serde_json::json!({
            "access_token": jwt(serde_json::json!({"realm_access": {"roles": ["ops"]}})),
            "id_token": id_token(&issuer, &q["nonce"], serde_json::json!({})),
            "token_type": "Bearer",
        })
        .to_string(),
    );
    let (status, headers, _) = get_full(
        &app,
        &format!("/api/auth/callback?code=abc&state={}", q["state"]),
        &[("cookie", &ticket)],
    )
    .await;
    assert_eq!(status, 303);
    assert_eq!(header_value(&headers, "location"), Some("/logs?q=x"));
    let session = cookie_pair(&headers, "opdash_session").unwrap().to_owned();
    assert!(
        cookie_pair(&headers, "opdash_login").unwrap().ends_with("opdash_login="),
        "登录票要清掉"
    );

    // token 端点收到的请求：code / verifier / client 密钥
    let token_req = kc.last_request();
    assert!(token_req.target.ends_with("/protocol/openid-connect/token"), "{}", token_req.target);
    let form: std::collections::HashMap<String, String> =
        form_urlencoded::parse(token_req.body.as_bytes()).into_owned().collect();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "abc");
    assert_eq!(form["client_id"], "opdash");
    assert_eq!(form["client_secret"], "s3cret");
    assert_eq!(form["redirect_uri"], "https://opdash.example.com/api/auth/callback");
    let digest = ring::digest::digest(&ring::digest::SHA256, form["code_verifier"].as_bytes());
    assert_eq!(
        B64.encode(digest.as_ref()),
        q["code_challenge"],
        "PKCE challenge 要和 verifier 对上"
    );

    // 4. 带会话访问 API 和页面
    let (status, meta) = get_json_with(&app, "/api/meta", &[("cookie", &session)]).await;
    assert_eq!(status, 200, "{meta}");
    let (status, me) = get_json_with(&app, "/api/auth/me", &[("cookie", &session)]).await;
    assert_eq!(status, 200);
    assert_eq!(me["user"]["name"], "alice");
    assert_eq!(me["user"]["email"], "alice@example.com");
    assert_eq!(me["logout_url"], "/api/auth/logout");
    // 改过的 cookie 不认
    let tampered = format!("{}x", session);
    let (status, _) = get_raw(&app, "/api/meta", &[("cookie", &tampered)]).await;
    assert_eq!(status, 401);

    // 5. 登出：清 cookie，去 Keycloak 结束 SSO 会话
    let (status, headers, _) = get_full(&app, "/api/auth/logout", &[("cookie", &session)]).await;
    assert_eq!(status, 303);
    let location = header_value(&headers, "location").unwrap();
    assert!(
        location.starts_with(&format!("{issuer}/protocol/openid-connect/logout?")),
        "{location}"
    );
    let lq = query_map(location);
    assert_eq!(lq["client_id"], "opdash");
    assert_eq!(lq["post_logout_redirect_uri"], "https://opdash.example.com/");
    let cleared = header_value(&headers, "set-cookie").unwrap();
    assert!(cleared.starts_with("opdash_session=;") && cleared.contains("Max-Age=0"), "{cleared}");
}

#[tokio::test]
async fn rejects_bad_id_token_and_missing_role() {
    let fake = FakeClickhouse::start().await;
    let kc = FakeKeycloak::start().await;
    let issuer = format!("{}/realms/ops", kc.endpoint());
    let app = oidc_app(&fake, &kc, &["--oidc-required-role", "ops"]).await;

    // discovery 只在第一次登录时读，之后缓存；假服务是按顺序回放的，所以只排一次
    kc.respond(discovery(&issuer));

    // nonce 对不上
    let (ticket, q) = begin(&app).await;
    kc.respond(
        serde_json::json!({"id_token": id_token(&issuer, "other-nonce", serde_json::json!({}))})
            .to_string(),
    );
    let (status, _, body) = callback(&app, &ticket, &q["state"]).await;
    assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));

    // 登录成功但没有 ops 角色 → 403 提示页
    let (ticket, q) = begin(&app).await;
    kc.respond(
        serde_json::json!({
            "access_token": jwt(serde_json::json!({"realm_access": {"roles": ["viewer"]}})),
            "id_token": id_token(&issuer, &q["nonce"], serde_json::json!({})),
        })
        .to_string(),
    );
    let (status, headers, body) = callback(&app, &ticket, &q["state"]).await;
    assert_eq!(status, 403, "{}", String::from_utf8_lossy(&body));
    assert!(cookie_pair(&headers, "opdash_session").is_none(), "没权限不该发会话");
    assert!(String::from_utf8_lossy(&body).contains("换个账号登录"));

    // 角色在 client 角色里也认
    let (ticket, q) = begin(&app).await;
    kc.respond(
        serde_json::json!({
            "id_token": id_token(&issuer, &q["nonce"],
                serde_json::json!({"resource_access": {"opdash": {"roles": ["ops"]}}})),
        })
        .to_string(),
    );
    let (status, _, _) = callback(&app, &ticket, &q["state"]).await;
    assert_eq!(status, 303);

    // Keycloak 那边拒了（用户取消）
    let (ticket, q) = begin(&app).await;
    let (status, _, body) = get_full(
        &app,
        &format!(
            "/api/auth/callback?error=access_denied&error_description=cancel&state={}",
            q["state"]
        ),
        &[("cookie", &ticket)],
    )
    .await;
    assert_eq!(status, 400);
    assert!(String::from_utf8_lossy(&body).contains("access_denied"));
}

#[tokio::test]
async fn basic_auth_still_works_alongside_oidc() {
    let fake = FakeClickhouse::start().await;
    let kc = FakeKeycloak::start().await;
    let app = oidc_app(&fake, &kc, &["--basic-auth", "ops:secret"]).await;

    let (status, _) =
        get_raw(&app, "/api/meta", &[("authorization", "Basic b3BzOnNlY3JldA==")]).await;
    assert_eq!(status, 200);
    let (status, me) =
        get_json_with(&app, "/api/auth/me", &[("authorization", "Basic b3BzOnNlY3JldA==")]).await;
    assert_eq!(status, 200);
    assert_eq!(me["user"]["name"], "ops");
    assert_eq!(me["mode"], "oidc");
}

#[tokio::test]
async fn login_without_oidc_is_404_and_me_reports_mode() {
    let fake = FakeClickhouse::start().await;
    let app = app_with_schema(&fake, &[]).await;
    let (status, _) = get_raw(&app, "/api/auth/login", &[]).await;
    assert_eq!(status, 404);
    let (status, me) = get_json(&app, "/api/auth/me").await;
    assert_eq!(status, 200);
    assert_eq!(me["mode"], "none");
    assert!(me["user"].is_null());

    // 只开 Basic：me 没带密码就 401 + 弹框头，和其它接口一致
    let app = app_with_schema(&fake, &["--basic-auth", "ops:secret"]).await;
    let (status, headers, _) = get_full(&app, "/api/auth/me", &[]).await;
    assert_eq!(status, 401);
    assert!(header_value(&headers, "www-authenticate").is_some());
}
