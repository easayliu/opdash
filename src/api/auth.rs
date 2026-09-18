//! `/api/auth/*`：OIDC 登录 / 回调 / 登出，给前端看的「我是谁」，以及签发 API key。
//!
//! 这几条路由不在认证中间件里面（不然没登录的人进不了登录页），要认证的（签 key）自己查身份。
//! 登录流程见 [`crate::auth::oidc`]，API key 是什么见 [`crate::auth`]。

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::auth::{
    Auth, Identity, IssueError, LOGIN_COOKIE, cookie,
    oidc::{KIND_LOGIN, LoginTicket, OidcError},
    set_cookie, unauthorized,
};
use crate::error::Error;

pub fn routes(auth: Auth) -> Router {
    Router::new()
        .route("/api/auth/me", get(me))
        .route("/api/auth/login", get(login))
        .route("/api/auth/callback", get(callback))
        .route("/api/auth/logout", get(logout))
        .route("/api/auth/keys", post(create_key))
        .with_state(auth)
}

#[derive(Serialize)]
struct Me {
    /// `none` / `basic` / `oidc`
    mode: &'static str,
    user: Option<User>,
    /// 这个请求是怎么认出来的：`session` / `basic` / `api_key`；没认出来是 null
    identity: Option<&'static str>,
    login_url: Option<&'static str>,
    logout_url: Option<&'static str>,
    /// 能不能生成 API key、最长多久。没开认证时是 null（什么都不用带，也就不需要 key）
    api_keys: Option<ApiKeysInfo>,
}

#[derive(Serialize)]
struct User {
    name: String,
    email: Option<String>,
}

#[derive(Serialize)]
struct ApiKeysInfo {
    /// `--api-key-ttl`，如 `90d`
    max_ttl: String,
    /// 签名密钥来自配置。false = 每次重启随机，发出去的 key 重启就作废，页面上要提醒
    persistent: bool,
}

/// 当前登录状态。没登录也回 200（前端据此决定要不要跳登录），除非是 Basic 模式下没带密码——
/// 那就 401 让浏览器弹框，和其它接口一致。
async fn me(State(auth): State<Auth>, headers: HeaderMap) -> Response {
    let identity = auth.identify(&headers);
    if auth.enabled() && identity.is_none() && auth.oidc().is_none() {
        return unauthorized(&auth);
    }
    let kind = identity.as_ref().map(Identity::kind);
    let user = identity
        .map(|id| User { name: id.user().to_owned(), email: id.email().map(str::to_owned) });
    let oidc = auth.oidc().is_some();
    Json(Me {
        mode: auth.mode(),
        user,
        identity: kind,
        login_url: oidc.then_some("/api/auth/login"),
        logout_url: oidc.then_some("/api/auth/logout"),
        api_keys: auth.enabled().then(|| ApiKeysInfo {
            max_ttl: crate::mcp::fmt_duration(auth.api_key_max_ttl_secs() * 1000),
            persistent: auth.secret_is_persistent(),
        }),
    })
    .into_response()
}

#[derive(Deserialize, Default)]
struct CreateKey {
    /// 给 key 起的名字，只是标签
    name: Option<String>,
    /// 有效期，`30d` / `12h` 这类写法；不给或超过 `--api-key-ttl` 都按上限算
    ttl: Option<String>,
}

#[derive(Serialize)]
struct CreatedKey {
    /// 完整的 key，**只在这里给一次**，服务端不存
    key: String,
    id: String,
    name: String,
    user: String,
    /// RFC3339
    created_at: String,
    expires_at: String,
    /// 有效多久，如 `90d`
    expires_in: String,
    /// 签名密钥来自配置；false 时重启后这把 key 就失效
    persistent: bool,
    /// MCP 端点的完整地址，页面上拼接入命令用
    mcp_url: String,
}

/// 登录用户给自己签一把 API key。请求体是 JSON（可以为空）：`{"name": "claude-code", "ttl": "30d"}`。
///
/// 谁都不用审批：key 只代表签发它的这个人、权限和这个人一样（本来就只有「能看」一种权限），
/// 泄露的影响面和他的会话 cookie 泄露一样。拿 API key 再签 API key 不行，见 [`IssueError`]。
async fn create_key(State(auth): State<Auth>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(who) = auth.identify(&headers) else {
        if !auth.enabled() {
            return Error::bad_request("没有开启认证，访问不需要带任何凭证，也就不需要 API key")
                .into_response();
        }
        return unauthorized(&auth);
    };
    let req: CreateKey = if body.iter().all(u8::is_ascii_whitespace) {
        CreateKey::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                return Error::bad_request(format!(
                    "请求体应是 JSON，如 {{\"name\": \"claude-code\", \"ttl\": \"30d\"}}: {e}"
                ))
                .into_response();
            }
        }
    };
    let ttl_secs = match req.ttl.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        None => auth.api_key_max_ttl_secs(),
        Some(raw) => match humantime::parse_duration(raw) {
            Ok(d) => d.as_secs().min(i64::MAX as u64) as i64,
            Err(_) => {
                return Error::bad_request(format!("ttl 写法应像 30d / 12h，不是 {raw:?}"))
                    .into_response();
            }
        },
    };
    let issued = match auth.issue_api_key(&who, req.name.as_deref().unwrap_or(""), ttl_secs) {
        Ok(k) => k,
        Err(IssueError::AuthDisabled) => {
            return Error::bad_request("没有开启认证，不需要 API key").into_response();
        }
        Err(IssueError::KeyCannotMintKey) => {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "API key 不能再签发 API key，请用浏览器登录后生成",
                    "kind": "forbidden",
                })),
            )
                .into_response();
        }
    };
    tracing::info!(
        user = %issued.key.user,
        key_id = %issued.key.id,
        key_name = %issued.key.name,
        expires_in_secs = issued.exp - issued.key.iat,
        via = who.kind(),
        "签发 API key"
    );
    let rfc3339 = |secs: i64| {
        chrono::DateTime::from_timestamp(secs, 0).map(|t| t.to_rfc3339()).unwrap_or_default()
    };
    Json(CreatedKey {
        key: issued.token,
        id: issued.key.id,
        name: issued.key.name,
        user: issued.key.user,
        created_at: rfc3339(issued.key.iat),
        expires_at: rfc3339(issued.exp),
        expires_in: crate::mcp::fmt_duration((issued.exp - issued.key.iat) * 1000),
        persistent: auth.secret_is_persistent(),
        mcp_url: format!("{}/mcp", auth.public_base(&headers)),
    })
    .into_response()
}

#[derive(Deserialize)]
struct LoginQuery {
    next: Option<String>,
}

/// 生成登录票，跳 Keycloak。
async fn login(
    State(auth): State<Auth>,
    headers: HeaderMap,
    Query(q): Query<LoginQuery>,
) -> Response {
    let Some(oidc) = auth.oidc() else {
        return (StatusCode::NOT_FOUND, "没有配置 OIDC 登录").into_response();
    };
    let base = auth.public_base(&headers);
    let redirect_uri = format!("{base}/api/auth/callback");
    // 只接受站内路径，别让登录链接把人带去别的站
    let next =
        q.next.filter(|n| n.starts_with('/') && !n.starts_with("//")).unwrap_or_else(|| "/".into());
    match oidc.begin(redirect_uri, next).await {
        Ok((ticket, url)) => {
            let sealed = oidc.seal_ticket(auth.sealer(), &ticket);
            let cookie =
                set_cookie(LOGIN_COOKIE, &sealed, "/api/auth", 600, base.starts_with("https://"));
            ([(header::SET_COOKIE, cookie)], Redirect::to(&url)).into_response()
        }
        Err(e) => error_page(&auth, e),
    }
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Keycloak 跳回来：核对 state，换 token，签会话，回原页面。
async fn callback(
    State(auth): State<Auth>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let Some(oidc) = auth.oidc() else {
        return (StatusCode::NOT_FOUND, "没有配置 OIDC 登录").into_response();
    };
    let secure = auth.public_base(&headers).starts_with("https://");
    let clear_ticket = set_cookie(LOGIN_COOKIE, "", "/api/auth", 0, secure);
    if let Some(err) = q.error {
        let detail = q.error_description.unwrap_or_default();
        return error_page(
            &auth,
            OidcError::Invalid(format!("Keycloak 拒绝了登录: {err} {detail}")),
        );
    }
    let ticket: Option<LoginTicket> =
        cookie(&headers, LOGIN_COOKIE).and_then(|t| auth.sealer().open(KIND_LOGIN, t));
    let Some(ticket) = ticket else {
        return error_page(
            &auth,
            OidcError::Invalid("登录票不存在或已过期（10 分钟内没完成登录），请重新登录".into()),
        );
    };
    if q.state.as_deref() != Some(ticket.state.as_str()) {
        return error_page(&auth, OidcError::Invalid("state 对不上，请重新登录".into()));
    }
    let Some(code) = q.code else {
        return error_page(&auth, OidcError::Invalid("回调里没有 code".into()));
    };
    match oidc.finish(&ticket, &code).await {
        Ok(session) => {
            tracing::info!(user = %session.name, sub = %session.sub, "登录成功");
            // 两个 Set-Cookie：数组形式的 IntoResponse 是 insert，第二个会顶掉第一个，得 append
            let mut resp = Redirect::to(&ticket.next).into_response();
            resp.headers_mut().append(header::SET_COOKIE, auth.session_cookie(&session, secure));
            resp.headers_mut().append(header::SET_COOKIE, clear_ticket);
            resp
        }
        Err(e) => {
            let mut resp = error_page(&auth, e);
            resp.headers_mut().append(header::SET_COOKIE, clear_ticket);
            resp
        }
    }
}

/// 清会话 cookie，再去 Keycloak 结束 SSO 会话（不然下次点登录直接又进来了）。
async fn logout(State(auth): State<Auth>, headers: HeaderMap) -> Response {
    let base = auth.public_base(&headers);
    let clear = auth.clear_session_cookie(base.starts_with("https://"));
    let target = auth
        .oidc()
        .and_then(|o| o.end_session_url(&format!("{base}/")))
        .unwrap_or_else(|| "/".to_owned());
    ([(header::SET_COOKIE, clear)], Redirect::to(&target)).into_response()
}

/// 登录失败的提示页：Keycloak 跳回来的是一次顶层导航，回 JSON 用户看不懂。
fn error_page(auth: &Auth, err: OidcError) -> Response {
    let (status, title) = match &err {
        OidcError::Upstream(_) => (StatusCode::BAD_GATEWAY, "登录服务不可用"),
        OidcError::Invalid(_) => (StatusCode::BAD_REQUEST, "登录失败"),
        OidcError::Forbidden { .. } => (StatusCode::FORBIDDEN, "没有权限"),
    };
    if status.is_server_error() {
        tracing::error!(error = %err, "OIDC 登录失败");
    } else {
        tracing::warn!(error = %err, "OIDC 登录被拒");
    }
    let mut actions = String::from(r#"<a href="/api/auth/login">重新登录</a>"#);
    if matches!(err, OidcError::Forbidden { .. }) && auth.oidc().is_some() {
        // 换个账号登录：先把 Keycloak 那边的 SSO 会话也退掉
        actions = r#"<a href="/api/auth/logout">换个账号登录</a>"#.to_owned();
    }
    let html = format!(
        r#"<!doctype html><meta charset="utf-8"><title>{title} · opdash</title>
<style>body{{font:14px/1.6 system-ui,sans-serif;max-width:32rem;margin:15vh auto;padding:0 1rem;color:#313131}}
h1{{font-size:18px}}p{{color:#595959}}a{{color:#2f7bbf}}</style>
<h1>{title}</h1><p>{}</p><p>{actions}</p>"#,
        html_escape(&err.to_string())
    );
    (status, Html(html)).into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
