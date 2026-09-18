//! 认证：可选的 Basic（一组共享密码）和 / 或 OIDC 登录（Keycloak）。两个都没配 = 不认证。
//!
//! 请求进来先看会话 cookie，再看 Basic 头，再看 `Authorization: Bearer` 里的 API key，都没有就拒：
//! 浏览器导航（Accept 带 text/html）在 OIDC 模式下直接 302 去登录，其余（API、静态资源）回 401 JSON——
//! JS 拿到 401 自己跳登录页，静态资源被 302 到登录页只会变成一堆坏掉的脚本。
//!
//! **API key** 是登录用户自己生成的、给 MCP 客户端和脚本用的凭证（`POST /api/auth/keys`）。它和会话
//! cookie 是同一种东西：一小段 JSON（谁、叫什么、什么时候发的）用同一把钥匙 HMAC 签名，带过期时间，
//! 服务端不存。代价是**没法单个吊销**——丢了只能等它过期，或者换 `--session-secret` 让全部作废——
//! 所以 `--api-key-ttl` 默认只有 90 天。opdash 是只读的、没有写库路径，为了一张 key 表去写 ClickHouse
//! 或挂一个文件卷不值得。
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

use serde::{Deserialize, Serialize};

use crate::config::{BasicAuth, Config};
pub use oidc::{Oidc, Session};
pub use session::Sealer;

pub const SESSION_COOKIE: &str = "opdash_session";
pub const LOGIN_COOKIE: &str = "opdash_login";

/// API key 的前缀：一眼认得出、secret 扫描器也好写规则。前缀后面是 [`Sealer`] 签出来的 token。
pub const API_KEY_PREFIX: &str = "opdash_";
/// API key 在 [`Sealer`] 里的 kind，和会话 / 登录票分开，互相冒充不了。
pub const KIND_API_KEY: &str = "apikey";
/// API key 有效期的下限。
pub const API_KEY_MIN_TTL_SECS: i64 = 60;
/// key 的名字最长多少字符（只是给人认的标签）。
const API_KEY_NAME_MAX: usize = 64;

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
    api_key_ttl_secs: i64,
    /// `--session-secret` 配了没有。没配的话密钥是随机的，重启后 API key 全失效，页面上要提醒
    persistent_secret: bool,
}

/// 请求是谁发的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// 过了 Basic 认证；`user` 是配置里的用户名
    Basic { user: String },
    /// 带着有效的 OIDC 会话
    Session(Session),
    /// 带着一把有效的 API key（签发它的人是 `user`）
    ApiKey(ApiKey),
}

impl Identity {
    /// 给日志 / 页面看的名字。
    pub fn user(&self) -> &str {
        match self {
            Identity::Basic { user } => user,
            Identity::Session(s) => &s.name,
            Identity::ApiKey(k) => &k.user,
        }
    }

    pub fn email(&self) -> Option<&str> {
        match self {
            Identity::Basic { .. } => None,
            Identity::Session(s) => s.email.as_deref(),
            Identity::ApiKey(k) => k.email.as_deref(),
        }
    }

    /// `session` / `basic` / `api_key`，`/api/auth/me` 里给前端看。
    pub fn kind(&self) -> &'static str {
        match self {
            Identity::Basic { .. } => "basic",
            Identity::Session(_) => "session",
            Identity::ApiKey(_) => "api_key",
        }
    }
}

/// 签在 API key 里的内容。过期时间在 [`Sealer`] 的信封上，这里不重复。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiKey {
    /// 随机 id，日志里认 key 用（key 本身不进日志）
    pub id: String,
    /// 用户给它起的名字，如 `claude-code`
    pub name: String,
    /// 签发它的人（OIDC 用户名，或 Basic 的账号名）
    pub user: String,
    pub email: Option<String>,
    /// 签发时间，unix 秒
    pub iat: i64,
}

/// 刚签出来的 key：`token` 只在这一刻给用户看一次。
#[derive(Debug, Clone)]
pub struct IssuedKey {
    pub token: String,
    pub key: ApiKey,
    /// 过期时间，unix 秒
    pub exp: i64,
}

/// 为什么没签出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueError {
    /// 没开认证：什么都不用带，也就不需要 key
    AuthDisabled,
    /// 拿着 API key 再签 API key：一把泄露的 key 不该能无限续命
    KeyCannotMintKey,
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
                api_key_ttl_secs: cfg.api_key_ttl.as_secs() as i64,
                persistent_secret: cfg.session_secret.as_deref().is_some_and(|s| !s.is_empty()),
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
                api_key_ttl_secs: 90 * 86_400,
                persistent_secret: false,
            }),
        }
    }

    /// API key 最长有效期（秒），`--api-key-ttl`。
    pub fn api_key_max_ttl_secs(&self) -> i64 {
        self.inner.api_key_ttl_secs
    }

    /// 签名密钥是不是配置里给的（否则重启后 key 全失效）。
    pub fn secret_is_persistent(&self) -> bool {
        self.inner.persistent_secret
    }

    /// 给 `who` 签一把 API key。`ttl_secs` 超过 `--api-key-ttl` 就按上限算，短于 1 分钟按 1 分钟。
    pub fn issue_api_key(
        &self,
        who: &Identity,
        name: &str,
        ttl_secs: i64,
    ) -> Result<IssuedKey, IssueError> {
        if !self.enabled() {
            return Err(IssueError::AuthDisabled);
        }
        if matches!(who, Identity::ApiKey(_)) {
            return Err(IssueError::KeyCannotMintKey);
        }
        let ttl = ttl_secs.clamp(API_KEY_MIN_TTL_SECS, self.inner.api_key_ttl_secs);
        let name: String = name.trim().chars().take(API_KEY_NAME_MAX).collect();
        let now = session::now_secs();
        let key = ApiKey {
            id: session::random_token(6),
            name: if name.is_empty() { "api-key".to_owned() } else { name },
            user: who.user().to_owned(),
            email: who.email().map(str::to_owned),
            iat: now,
        };
        let token = format!("{API_KEY_PREFIX}{}", self.inner.sealer.seal(KIND_API_KEY, &key, ttl));
        Ok(IssuedKey { token, key, exp: now + ttl })
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

    /// 从请求头里认出用户：先会话 cookie，再 Basic，再 Bearer 的 API key。
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
        if let Some(token) = bearer(headers).and_then(|t| t.strip_prefix(API_KEY_PREFIX))
            && let Some(k) = self.inner.sealer.open::<ApiKey>(KIND_API_KEY, token)
        {
            return Some(Identity::ApiKey(k));
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

/// `Authorization: Bearer xxx` 里的 xxx。
pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim()).filter(|t| !t.is_empty())
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
    fn api_keys_roundtrip_and_cannot_mint_more_keys() {
        use clap::Parser;
        let cfg = crate::config::Config::try_parse_from([
            "opdash",
            "--basic-auth",
            "ops:pw",
            "--session-secret",
            "k",
            "--api-key-ttl",
            "2h",
        ])
        .unwrap();
        let auth = Auth::from_config(&cfg);
        assert!(auth.secret_is_persistent());
        let me = Identity::Basic { user: "ops".into() };
        let issued = auth.issue_api_key(&me, "  claude-code ", 10 * 86_400).unwrap();
        assert!(issued.token.starts_with(API_KEY_PREFIX));
        assert_eq!(issued.key.name, "claude-code");
        assert_eq!(issued.key.user, "ops");
        assert_eq!(issued.exp - issued.key.iat, 7200, "超过 --api-key-ttl 按上限算");

        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, format!("Bearer {}", issued.token).parse().unwrap());
        let id = auth.identify(&h).expect("key 要认得");
        assert_eq!(id.kind(), "api_key");
        assert_eq!(id.user(), "ops");
        // 拿 key 再签 key 不行
        assert!(matches!(auth.issue_api_key(&id, "x", 60), Err(IssueError::KeyCannotMintKey)));
        // 改一位、去掉前缀、换 scheme 都不认
        h.insert(header::AUTHORIZATION, format!("Bearer {}x", issued.token).parse().unwrap());
        assert!(auth.identify(&h).is_none());
        h.insert(header::AUTHORIZATION, format!("Bearer {}", &issued.token[7..]).parse().unwrap());
        assert!(auth.identify(&h).is_none());
        h.insert(header::AUTHORIZATION, format!("Basic {}", issued.token).parse().unwrap());
        assert!(auth.identify(&h).is_none());
        // 没开认证就不发 key
        assert!(matches!(
            Auth::disabled().issue_api_key(&me, "x", 60),
            Err(IssueError::AuthDisabled)
        ));
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
