//! 认证：可选的 Basic（一组共享密码）和 / 或 OIDC 登录（Keycloak）。两个都没配 = 不认证。
//!
//! 请求进来先看会话 cookie，再看 Basic 头，都没有就拒：浏览器导航（Accept 带 text/html）在 OIDC 模式下
//! 直接 302 去登录，其余（API、静态资源）回 401 JSON——JS 拿到 401 自己跳登录页，静态资源被 302 到
//! 登录页只会变成一堆坏掉的脚本。
//!
//! HTTP 处理器在 [`crate::api::auth`]，这里是状态和中间件。

pub mod basic;
pub mod oidc;
pub mod session;

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};

use crate::config::{BasicAuth, Config};
pub use oidc::{Oidc, Session};
pub use session::Sealer;

pub const SESSION_COOKIE: &str = "opdash_session";
pub const LOGIN_COOKIE: &str = "opdash_login";

#[derive(Clone)]
pub struct Auth {
    inner: Arc<Inner>,
}

struct Inner {
    basic: Option<BasicAuth>,
    oidc: Option<Oidc>,
    sealer: Sealer,
    public_url: Option<String>,
    session_ttl_secs: i64,
}

/// 请求是谁发的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// 过了 Basic 认证；`user` 是配置里的用户名
    Basic { user: String },
    /// 带着有效的 OIDC 会话
    Session(Session),
}

impl Auth {
    pub fn from_config(cfg: &Config) -> Self {
        let oidc = cfg.oidc_issuer.clone().zip(cfg.oidc_client_id.clone()).map(|(issuer, id)| {
            Oidc::new(
                issuer,
                id,
                cfg.oidc_client_secret.clone(),
                cfg.oidc_scopes.clone(),
                cfg.oidc_required_role.clone(),
            )
        });
        Self {
            inner: Arc::new(Inner {
                basic: cfg.basic_auth.clone(),
                oidc,
                sealer: Sealer::new(cfg.session_secret.as_deref()),
                public_url: cfg.public_url.clone(),
                session_ttl_secs: cfg.session_ttl.as_secs() as i64,
            }),
        }
    }

    /// 测试 / 无认证用。
    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(Inner {
                basic: None,
                oidc: None,
                sealer: Sealer::new(None),
                public_url: None,
                session_ttl_secs: 3600,
            }),
        }
    }

    pub fn enabled(&self) -> bool {
        self.inner.basic.is_some() || self.inner.oidc.is_some()
    }

    pub fn oidc(&self) -> Option<&Oidc> {
        self.inner.oidc.as_ref()
    }

    pub fn basic(&self) -> Option<&BasicAuth> {
        self.inner.basic.as_ref()
    }

    pub fn sealer(&self) -> &Sealer {
        &self.inner.sealer
    }

    /// 给 `/api/auth/me` 和启动日志看的模式名。
    pub fn mode(&self) -> &'static str {
        match (&self.inner.oidc, &self.inner.basic) {
            (Some(_), _) => "oidc",
            (None, Some(_)) => "basic",
            (None, None) => "none",
        }
    }

    /// 从请求头里认出用户：先会话 cookie，再 Basic。
    pub fn identify(&self, headers: &HeaderMap) -> Option<Identity> {
        if self.inner.oidc.is_some()
            && let Some(token) = cookie(headers, SESSION_COOKIE)
            && let Some(s) = self.inner.sealer.open::<Session>(oidc::KIND_SESSION, token)
        {
            return Some(Identity::Session(s));
        }
        if let Some(b) = &self.inner.basic
            && basic::check(b, headers)
        {
            return Some(Identity::Basic { user: b.user.clone() });
        }
        None
    }

    /// 浏览器访问 opdash 用的根地址（不带末尾斜杠）：配了 `--public-url` 用它，否则按反代头 / Host 推。
    pub fn public_base(&self, headers: &HeaderMap) -> String {
        if let Some(u) = &self.inner.public_url {
            return u.clone();
        }
        let first = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').next())
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let scheme = first("x-forwarded-proto").unwrap_or_else(|| "http".to_owned());
        let host = first("x-forwarded-host")
            .or_else(|| first("host"))
            .unwrap_or_else(|| "localhost".to_owned());
        format!("{scheme}://{host}")
    }

    /// 签好的会话 cookie（Set-Cookie 头的值）。
    pub fn session_cookie(&self, session: &Session, secure: bool) -> HeaderValue {
        let v = self.inner.sealer.seal(oidc::KIND_SESSION, session, self.inner.session_ttl_secs);
        set_cookie(SESSION_COOKIE, &v, "/", self.inner.session_ttl_secs, secure)
    }

    pub fn clear_session_cookie(&self, secure: bool) -> HeaderValue {
        set_cookie(SESSION_COOKIE, "", "/", 0, secure)
    }
}

/// 中间件：没认出人就拒。
pub async fn require(State(auth): State<Auth>, req: Request, next: Next) -> Response {
    if auth.identify(req.headers()).is_some() {
        return next.run(req).await;
    }
    if auth.oidc().is_some() && is_navigation(&req) {
        let next_path = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .filter(|p| p.starts_with('/'))
            .unwrap_or("/");
        let mut q = form_urlencoded::Serializer::new(String::new());
        q.append_pair("next", next_path);
        return Redirect::to(&format!("/api/auth/login?{}", q.finish())).into_response();
    }
    unauthorized(&auth)
}

/// 401：只开了 Basic 时带 WWW-Authenticate 让浏览器弹框；开了 OIDC 就不弹（弹框和跳登录页打架），
/// 前端按 `login_url` 自己跳。
pub fn unauthorized(auth: &Auth) -> Response {
    if auth.oidc().is_some() {
        let body = serde_json::json!({
            "error": "需要登录",
            "kind": "unauthenticated",
            "login_url": "/api/auth/login",
        });
        return (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"opdash\", charset=\"UTF-8\"")],
        Body::from("需要登录"),
    )
        .into_response()
}

/// 浏览器地址栏 / 链接发起的 GET：Accept 里明确要 text/html。fetch 默认是 `*/*`，不算。
fn is_navigation(req: &Request) -> bool {
    req.method() == Method::GET
        && req
            .headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.contains("text/html"))
}

/// Cookie 头里某个名字的值（不解码：我们的值都是 base64url，没有需要转义的字符）。
pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get_all(header::COOKIE).iter().find_map(|v| {
        v.to_str().ok()?.split(';').find_map(|kv| {
            let (k, v) = kv.trim().split_once('=')?;
            (k == name).then_some(v)
        })
    })
}

/// `max_age` 为 0 表示删除。HttpOnly + SameSite=Lax：JS 摸不到，跨站请求不带，顶层导航（Keycloak 跳回来）带。
pub fn set_cookie(name: &str, value: &str, path: &str, max_age: i64, secure: bool) -> HeaderValue {
    let mut s = format!("{name}={value}; Path={path}; Max-Age={max_age}; HttpOnly; SameSite=Lax");
    if secure {
        s.push_str("; Secure");
    }
    HeaderValue::from_str(&s).expect("cookie value is base64url")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cookie_among_others() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; opdash_session=abc.def; b=2".parse().unwrap());
        assert_eq!(cookie(&h, SESSION_COOKIE), Some("abc.def"));
        assert_eq!(cookie(&h, "b"), Some("2"));
        assert_eq!(cookie(&h, "zz"), None);
    }

    #[test]
    fn public_base_prefers_forwarded_headers() {
        let auth = Auth::disabled();
        let mut h = HeaderMap::new();
        h.insert("host", "opdash:4880".parse().unwrap());
        assert_eq!(auth.public_base(&h), "http://opdash:4880");
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        h.insert("x-forwarded-host", "opdash.example.com, inner".parse().unwrap());
        assert_eq!(auth.public_base(&h), "https://opdash.example.com");
    }
}
