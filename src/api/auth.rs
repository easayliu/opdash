//! `/api/auth/*`：OIDC 登录 / 回调 / 登出，给前端看的「我是谁」，以及签发 API key。
//!
//! 这几条路由不在认证中间件里面（不然没登录的人进不了登录页），要认证的（签 key）自己查身份。
//! 登录流程见 [`crate::auth::oidc`]，API key 是什么见 [`crate::auth`]。

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::auth::{
    API_KEY_PREFIX, ApiKey, Auth, Identity, IssueError, LOGIN_COOKIE, cookie,
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
        .route("/api/auth/keys", get(list_keys).post(create_key))
        .route("/api/auth/keys/{id}", axum::routing::delete(revoke_key))
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
    /// 这个身份能不能管理 API key（列出 / 生成 / 吊销自己的）、最长多久。没开认证、或者本身就是
    /// 拿 API key 进来的，都是 null
    api_keys: Option<ApiKeysInfo>,
}

#[derive(Serialize)]
struct User {
    /// 给人看的名字：OIDC 给的姓名（中文名优先），其它身份就是账号名
    name: String,
    email: Option<String>,
}

#[derive(Serialize)]
struct ApiKeysInfo {
    /// `--api-key-ttl`，如 `90d`
    max_ttl: String,
}

/// 当前登录状态。没登录也回 200（前端据此决定要不要跳登录），除非是 Basic 模式下没带密码——
/// 那就 401 让浏览器弹框，和其它接口一致。
async fn me(State(auth): State<Auth>, headers: HeaderMap) -> Response {
    let identity = auth.identify(&headers);
    if auth.enabled() && identity.is_none() && auth.oidc().is_none() {
        return unauthorized(&auth);
    }
    let kind = identity.as_ref().map(Identity::kind);
    let user = identity.map(|id| User {
        name: id.display_name().to_owned(),
        email: id.email().map(str::to_owned),
    });
    let oidc = auth.oidc().is_some();
    Json(Me {
        mode: auth.mode(),
        user,
        identity: kind,
        login_url: oidc.then_some("/api/auth/login"),
        logout_url: oidc.then_some("/api/auth/logout"),
        api_keys: (auth.enabled() && kind.is_some_and(|k| k != "api_key")).then(|| ApiKeysInfo {
            max_ttl: crate::mcp::fmt_duration(auth.api_key_max_ttl_secs() * 1000),
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

/// 一把 key 给页面看的形状：没有 key 本身（服务端也没存）。
#[derive(Serialize)]
struct KeyView {
    id: String,
    name: String,
    user: String,
    /// `opdash_<id>.` ——用户拿它和自己配置里的 key 对号
    prefix: String,
    /// RFC3339
    created_at: String,
    expires_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_used_at: Option<String>,
    expired: bool,
}

impl KeyView {
    fn from(k: ApiKey) -> Self {
        Self {
            prefix: format!("{API_KEY_PREFIX}{}.", k.id),
            created_at: rfc3339(k.created_at),
            expires_at: rfc3339(k.expires_at),
            last_used_at: k.last_used_at.map(rfc3339),
            expired: k.expired(),
            id: k.id,
            name: k.name,
            user: k.user,
        }
    }
}

#[derive(Serialize)]
struct CreatedKey {
    /// 完整的 key，**只在这里给一次**，服务端只存哈希
    key: String,
    #[serde(flatten)]
    view: KeyView,
    /// 有效多久，如 `90d`
    expires_in: String,
    /// MCP 端点的完整地址，页面上拼接入命令用
    mcp_url: String,
}

#[derive(Serialize)]
struct KeyList {
    keys: Vec<KeyView>,
}

fn rfc3339(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0).map(|t| t.to_rfc3339()).unwrap_or_default()
}

/// 管理 key 的三个接口共用的身份检查：没登录 401；没开认证 400 说清楚。
#[allow(clippy::result_large_err)] // 只在拒绝时才有 Response，调用处直接 return
fn manager(auth: &Auth, headers: &HeaderMap) -> Result<Identity, Response> {
    match auth.identify(headers) {
        Some(who) => Ok(who),
        None if !auth.enabled() => {
            Err(Error::bad_request(IssueError::AuthDisabled.to_string()).into_response())
        }
        None => Err(unauthorized(auth)),
    }
}

fn issue_error(e: IssueError) -> Response {
    match e {
        IssueError::AuthDisabled => Error::bad_request(e.to_string()).into_response(),
        IssueError::KeyCannotManage => (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": e.to_string(), "kind": "forbidden" })),
        )
            .into_response(),
        IssueError::Store(_) => Error::internal(e).into_response(),
    }
}

/// 我的 key（不含 key 本身），新的在前，过期的标 `expired`。
async fn list_keys(State(auth): State<Auth>, headers: HeaderMap) -> Response {
    let who = match manager(&auth, &headers) {
        Ok(w) => w,
        Err(r) => return r,
    };
    match auth.list_api_keys(&who) {
        Ok(keys) => {
            Json(KeyList { keys: keys.into_iter().map(KeyView::from).collect() }).into_response()
        }
        Err(e) => issue_error(e),
    }
}

/// 登录用户给自己签一把 API key。请求体是 JSON（可以为空）：`{"name": "claude-code", "ttl": "30d"}`。
///
/// 谁都不用审批：key 只代表签发它的这个人、权限和这个人一样（本来就只有「能看」一种权限），
/// 泄露的影响面和他的会话 cookie 泄露一样，而且随时能在页面上吊销。
async fn create_key(State(auth): State<Auth>, headers: HeaderMap, body: Bytes) -> Response {
    let who = match manager(&auth, &headers) {
        Ok(w) => w,
        Err(r) => return r,
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
    let (token, key) = match auth.issue_api_key(&who, req.name.as_deref().unwrap_or(""), ttl_secs) {
        Ok(k) => k,
        Err(e) => return issue_error(e),
    };
    tracing::info!(
        user = %key.user,
        key_id = %key.id,
        key_name = %key.name,
        expires_in_secs = key.expires_at - key.created_at,
        via = who.kind(),
        "签发 API key"
    );
    let expires_in = crate::mcp::fmt_duration((key.expires_at - key.created_at) * 1000);
    Json(CreatedKey {
        key: token,
        view: KeyView::from(key),
        expires_in,
        mcp_url: format!("{}/mcp", auth.public_base(&headers)),
    })
    .into_response()
}

/// 吊销我自己的一把 key。别人的 / 不存在的一律 404，不区分。
async fn revoke_key(
    State(auth): State<Auth>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let who = match manager(&auth, &headers) {
        Ok(w) => w,
        Err(r) => return r,
    };
    match auth.revoke_api_key(&who, id.trim()) {
        Ok(true) => {
            tracing::info!(user = %who.user(), key_id = %id, "吊销 API key");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "没有这把 key（或者它不是你的）", "kind": "not_found" })),
        )
            .into_response(),
        Err(e) => issue_error(e),
    }
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
