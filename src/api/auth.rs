//! `/api/auth/*`：OIDC 登录 / 回调 / 登出，以及给前端看的「我是谁」。
//!
//! 这几条路由不在认证中间件里面（不然没登录的人进不了登录页）。流程见 [`crate::auth::oidc`]。

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};

use crate::auth::{
    Auth, Identity, LOGIN_COOKIE, cookie,
    oidc::{KIND_LOGIN, LoginTicket, OidcError},
    set_cookie, unauthorized,
};

pub fn routes(auth: Auth) -> Router {
    Router::new()
        .route("/api/auth/me", get(me))
        .route("/api/auth/login", get(login))
        .route("/api/auth/callback", get(callback))
        .route("/api/auth/logout", get(logout))
        .with_state(auth)
}

#[derive(Serialize)]
struct Me {
    /// `none` / `basic` / `oidc`
    mode: &'static str,
    user: Option<User>,
    login_url: Option<&'static str>,
    logout_url: Option<&'static str>,
}

#[derive(Serialize)]
struct User {
    name: String,
    email: Option<String>,
}

/// 当前登录状态。没登录也回 200（前端据此决定要不要跳登录），除非是 Basic 模式下没带密码——
/// 那就 401 让浏览器弹框，和其它接口一致。
async fn me(State(auth): State<Auth>, headers: HeaderMap) -> Response {
    let identity = auth.identify(&headers);
    if auth.enabled() && identity.is_none() && auth.oidc().is_none() {
        return unauthorized(&auth);
    }
    let user = identity.map(|id| match id {
        Identity::Basic { user } => User { name: user, email: None },
        Identity::Session(s) => User { name: s.name, email: s.email },
    });
    let oidc = auth.oidc().is_some();
    Json(Me {
        mode: auth.mode(),
        user,
        login_url: oidc.then_some("/api/auth/login"),
        logout_url: oidc.then_some("/api/auth/logout"),
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
