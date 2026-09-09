//! OIDC 授权码 + PKCE，对接 Keycloak（其它标准 OIDC 提供方也能用，角色 claim 的位置是 Keycloak 的）。
//!
//! ```text
//!   浏览器 ─ GET /api/auth/login ─▶ opdash：生成 state / nonce / PKCE，签进 10 分钟的 cookie
//!          ◀─ 302 到 Keycloak 授权页 ──┘
//!   浏览器 ─ 登录 ─▶ Keycloak ─ 302 回 /api/auth/callback?code&state ─▶ opdash
//!                                    opdash ─ POST token 端点（code + verifier + client secret）─▶ Keycloak
//!                                    opdash：校验 id_token 的 iss / aud / exp / nonce，签会话 cookie
//!          ◀─ 302 回原来要去的页面 ───┘
//! ```
//!
//! **id_token 不验签**：它是 opdash 自己通过 TLS 直连 token 端点拿回来的，链路本身已经证明了签发方
//! （OIDC Core 3.1.3.7 第 6 条明说这种情况可以用 TLS 代替验签）。省掉拉 JWKS、跟着轮换的一套。
//! 前提是 issuer 走 https；内网 http 的 Keycloak 要自己知道这意味着什么。

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use url::Url;

use super::session::{Sealer, now_secs, random_token};

#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub end_session_endpoint: Option<String>,
}

pub struct Oidc {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: String,
    pub required_role: Option<String>,
    http: reqwest::Client,
    discovery: RwLock<Option<Arc<Discovery>>>,
}

/// 登录票：从 /login 到 /callback 之间要记住的东西，签在 cookie 里。
#[derive(Debug, Serialize, Deserialize)]
pub struct LoginTicket {
    pub state: String,
    pub nonce: String,
    pub verifier: String,
    pub redirect_uri: String,
    /// 登录完回到哪个页面（站内路径）
    pub next: String,
}

/// 登录后的会话：只留页面要显示、日志要记的几项。角色检查在签会话前做完，这里不带角色。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    pub sub: String,
    pub name: String,
    pub email: Option<String>,
}

/// token 端点响应里用得到的字段。
#[derive(Debug, Deserialize)]
pub struct Tokens {
    pub access_token: Option<String>,
    pub id_token: String,
}

/// id_token / access_token 的 payload 里我们看的字段。
#[derive(Debug, Default, Deserialize)]
pub struct Claims {
    pub iss: Option<String>,
    #[serde(default)]
    pub aud: Audience,
    pub exp: Option<i64>,
    pub nonce: Option<String>,
    pub sub: Option<String>,
    pub preferred_username: Option<String>,
    pub name: Option<String>,
    pub email: Option<String>,
    /// Keycloak：realm 角色在 `realm_access.roles`
    #[serde(default)]
    pub realm_access: RoleList,
    /// Keycloak：client 角色在 `resource_access.<client_id>.roles`
    #[serde(default)]
    pub resource_access: std::collections::HashMap<String, RoleList>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RoleList {
    #[serde(default)]
    pub roles: Vec<String>,
}

/// `aud` 单个 client 时是字符串，多个时是数组。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Default for Audience {
    fn default() -> Self {
        Audience::Many(Vec::new())
    }
}

impl Audience {
    fn contains(&self, client_id: &str) -> bool {
        match self {
            Audience::One(s) => s == client_id,
            Audience::Many(v) => v.iter().any(|s| s == client_id),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("Keycloak 不可用: {0}")]
    Upstream(String),
    #[error("{0}")]
    Invalid(String),
    /// 登录成功但没有要求的角色
    #[error("账号 {user} 没有 {role} 角色")]
    Forbidden { user: String, role: String },
}

const LOGIN_TICKET_TTL: i64 = 10 * 60;
pub const KIND_LOGIN: &str = "login";
pub const KIND_SESSION: &str = "session";

impl Oidc {
    pub fn new(
        issuer: String,
        client_id: String,
        client_secret: Option<String>,
        scopes: String,
        required_role: Option<String>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent(concat!("opdash/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client");
        Self {
            issuer,
            client_id,
            client_secret: client_secret.filter(|s| !s.is_empty()),
            scopes,
            required_role: required_role.filter(|s| !s.is_empty()),
            http,
            discovery: RwLock::new(None),
        }
    }

    /// 读 `.well-known/openid-configuration`，成功后缓存。失败不缓存，下次登录再试——
    /// Keycloak 比 opdash 晚起来不该要求重启。
    pub async fn discover(&self) -> Result<Arc<Discovery>, OidcError> {
        if let Some(d) = self.discovery.read().clone() {
            return Ok(d);
        }
        let url = format!("{}/.well-known/openid-configuration", self.issuer);
        let resp =
            self.http.get(&url).send().await.map_err(|e| OidcError::Upstream(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(OidcError::Upstream(format!("{url} 返回 {}", resp.status())));
        }
        let body = resp.text().await.map_err(|e| OidcError::Upstream(e.to_string()))?;
        let d: Discovery = serde_json::from_str(&body)
            .map_err(|e| OidcError::Upstream(format!("解析 discovery: {e}")))?;
        if d.issuer.trim_end_matches('/') != self.issuer {
            return Err(OidcError::Invalid(format!(
                "discovery 里的 issuer 是 {:?}，和 --oidc-issuer {:?} 不一致",
                d.issuer, self.issuer
            )));
        }
        let d = Arc::new(d);
        *self.discovery.write() = Some(Arc::clone(&d));
        Ok(d)
    }

    /// 生成登录票和对应的授权页地址。
    pub async fn begin(
        &self,
        redirect_uri: String,
        next: String,
    ) -> Result<(LoginTicket, String), OidcError> {
        let d = self.discover().await?;
        let ticket = LoginTicket {
            state: random_token(16),
            nonce: random_token(16),
            verifier: random_token(32),
            redirect_uri,
            next,
        };
        let challenge = {
            let digest = ring::digest::digest(&ring::digest::SHA256, ticket.verifier.as_bytes());
            B64.encode(digest.as_ref())
        };
        let mut url = Url::parse(&d.authorization_endpoint)
            .map_err(|e| OidcError::Invalid(format!("authorization_endpoint: {e}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &ticket.redirect_uri)
            .append_pair("scope", &self.scopes)
            .append_pair("state", &ticket.state)
            .append_pair("nonce", &ticket.nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok((ticket, url.into()))
    }

    /// 用授权码换 token，校验 id_token，返回会话。
    pub async fn finish(&self, ticket: &LoginTicket, code: &str) -> Result<Session, OidcError> {
        let d = self.discover().await?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", ticket.redirect_uri.as_str()),
            ("client_id", self.client_id.as_str()),
            ("code_verifier", ticket.verifier.as_str()),
        ];
        if let Some(secret) = &self.client_secret {
            form.push(("client_secret", secret.as_str()));
        }
        let resp = self
            .http
            .post(&d.token_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|e| OidcError::Upstream(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| OidcError::Upstream(e.to_string()))?;
        if !status.is_success() {
            // Keycloak 的错误体是 {"error":"invalid_grant","error_description":"..."}
            let detail = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["error_description"].as_str().map(str::to_owned))
                .unwrap_or_else(|| body.chars().take(200).collect());
            return Err(OidcError::Invalid(format!("token 端点返回 {status}: {detail}")));
        }
        let tokens: Tokens = serde_json::from_str(&body)
            .map_err(|e| OidcError::Upstream(format!("解析 token 响应: {e}")))?;
        let id = decode_claims(&tokens.id_token)?;
        if id.iss.as_deref().map(|s| s.trim_end_matches('/')) != Some(self.issuer.as_str()) {
            return Err(OidcError::Invalid(format!("id_token 的 iss 是 {:?}", id.iss)));
        }
        if !id.aud.contains(&self.client_id) {
            return Err(OidcError::Invalid("id_token 的 aud 不含本 client".into()));
        }
        if id.exp.is_none_or(|exp| exp <= now_secs()) {
            return Err(OidcError::Invalid("id_token 已过期".into()));
        }
        if id.nonce.as_deref() != Some(ticket.nonce.as_str()) {
            return Err(OidcError::Invalid("id_token 的 nonce 对不上".into()));
        }
        let sub = id.sub.clone().ok_or_else(|| OidcError::Invalid("id_token 没有 sub".into()))?;
        let name = id
            .preferred_username
            .clone()
            .or_else(|| id.name.clone())
            .or_else(|| id.email.clone())
            .unwrap_or_else(|| sub.clone());

        if let Some(role) = &self.required_role {
            // Keycloak 默认只把角色放进 access_token（realm roles 映射器的「Add to ID token」默认关），
            // 两个 token 都看一遍，管理员不用改映射器
            let mut has = has_role(&id, &self.client_id, role);
            if !has && let Some(at) = &tokens.access_token {
                has =
                    decode_claims(at).map(|c| has_role(&c, &self.client_id, role)).unwrap_or(false);
            }
            if !has {
                return Err(OidcError::Forbidden { user: name, role: role.clone() });
            }
        }
        Ok(Session { sub, name, email: id.email })
    }

    /// Keycloak 的登出地址：结束 SSO 会话后跳回 `post_logout`。没有 end_session 端点就返回 None。
    pub fn end_session_url(&self, post_logout: &str) -> Option<String> {
        let d = self.discovery.read().clone()?;
        let mut url = Url::parse(d.end_session_endpoint.as_ref()?).ok()?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("post_logout_redirect_uri", post_logout);
        Some(url.into())
    }

    pub fn seal_ticket(&self, sealer: &Sealer, ticket: &LoginTicket) -> String {
        sealer.seal(KIND_LOGIN, ticket, LOGIN_TICKET_TTL)
    }
}

fn has_role(c: &Claims, client_id: &str, role: &str) -> bool {
    c.realm_access.roles.iter().any(|r| r == role)
        || c.resource_access.get(client_id).is_some_and(|l| l.roles.iter().any(|r| r == role))
}

/// 解 JWT 的 payload 段（不验签，理由见模块注释）。
pub fn decode_claims(jwt: &str) -> Result<Claims, OidcError> {
    let mut parts = jwt.split('.');
    let (Some(_header), Some(payload), Some(_sig)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(OidcError::Invalid("token 不是 JWT".into()));
    };
    let raw =
        B64.decode(payload).map_err(|_| OidcError::Invalid("JWT payload 不是 base64url".into()))?;
    serde_json::from_slice(&raw).map_err(|e| OidcError::Invalid(format!("JWT payload: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(payload: serde_json::Value) -> String {
        format!("eyJhbGciOiJSUzI1NiJ9.{}.sig", B64.encode(payload.to_string()))
    }

    #[test]
    fn decodes_keycloak_shaped_claims() {
        let c = decode_claims(&jwt(serde_json::json!({
            "iss": "https://sso/realms/ops", "aud": ["opdash", "account"], "exp": 1, "sub": "u1",
            "preferred_username": "alice", "email": "a@x",
            "realm_access": {"roles": ["ops"]},
            "resource_access": {"opdash": {"roles": ["viewer"]}}
        })))
        .unwrap();
        assert!(c.aud.contains("opdash"));
        assert!(!c.aud.contains("other"));
        assert!(has_role(&c, "opdash", "ops"));
        assert!(has_role(&c, "opdash", "viewer"));
        assert!(!has_role(&c, "opdash", "admin"));
        assert!(!has_role(&c, "another-client", "viewer"));
        assert_eq!(c.preferred_username.as_deref(), Some("alice"));

        let c = decode_claims(&jwt(serde_json::json!({"aud": "opdash"}))).unwrap();
        assert!(c.aud.contains("opdash"));
        assert!(decode_claims("not.a").is_err());
        assert!(decode_claims("a.!!!.c").is_err());
    }
}
