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
    /// 账号名（`preferred_username`，没有就 sub）。**API key 的归属和审计日志按它认人**，
    /// 所以它不跟着显示名走：显示名换成中文姓名之后，之前签出去的 key 还得认得回来。
    #[serde(default)]
    pub user: String,
    /// 给人看的名字，中文姓名优先（`name` claim）。
    pub name: String,
    pub email: Option<String>,
}

impl Session {
    /// 认人用的账号名。老的会话 cookie 里没有 `user` 字段，那时候 `name` 存的就是账号名，
    /// 退回去用它 —— 不然升级之后大家的 key 一下子全「不见了」。
    pub fn account(&self) -> &str {
        if self.user.is_empty() { &self.name } else { &self.user }
    }
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
    /// 显示名。中文姓名一般在这里（Keycloak 把 First + Last name 拼起来给）。
    pub name: Option<String>,
    /// Keycloak 的 First name。中文用户这一栏填的通常是**姓**
    pub given_name: Option<String>,
    /// Keycloak 的 Last name，中文用户这一栏填的通常是**名**
    pub family_name: Option<String>,
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
    #[error("Keycloak 不可用：{0}")]
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
            .map_err(|e| OidcError::Upstream(format!("解析 discovery 失败：{e}")))?;
        if d.issuer.trim_end_matches('/') != self.issuer {
            return Err(OidcError::Invalid(format!(
                "discovery 里的 issuer 是 {:?}，与 --oidc-issuer {:?} 不一致",
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
            .map_err(|e| OidcError::Upstream(format!("解析 token 响应失败：{e}")))?;
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
            return Err(OidcError::Invalid("id_token 的 nonce 不匹配".into()));
        }
        let sub = id.sub.clone().ok_or_else(|| OidcError::Invalid("id_token 没有 sub".into()))?;
        // 账号名认人（key 归属、审计日志），显示名给人看，两者分开
        let account = id.preferred_username.clone().unwrap_or_else(|| sub.clone());
        let name = display_name(&id, &account);

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
        Ok(Session { sub, user: account, name, email: id.email })
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

/// 页面上显示哪个名字：`name` -> `given_name` + `family_name` -> 账号名 -> `email`。
///
/// 中文姓名一般在 `name` 里（Keycloak 里填了 First / Last name 就有），而
/// `preferred_username` 往往是登录用的英文账号 —— 以前是后者优先，所以页面上看到的是拼音。
/// 两栏都空的用户没有 `name` claim，那就还是账号名。
fn display_name(c: &Claims, account: &str) -> String {
    c.name
        .as_deref()
        .map(tidy_name)
        .filter(|s| !s.is_empty())
        .or_else(|| full_name(c))
        .or_else(|| {
            [Some(account), c.email.as_deref()]
                .into_iter()
                .flatten()
                .map(str::trim)
                .find(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| account.to_owned())
}

/// 没有 `name` claim（IdP 没配 full name 映射器）时自己拼。
fn full_name(c: &Claims) -> Option<String> {
    let part = |v: &Option<String>| {
        v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
    };
    match (part(&c.given_name), part(&c.family_name)) {
        // Keycloak 的 name 就是这个顺序（First 在前），中文那边 First 填的是姓，拼出来正好
        (Some(given), Some(family)) => Some(tidy_name(&format!("{given} {family}"))),
        (Some(one), None) | (None, Some(one)) => Some(one),
        (None, None) => None,
    }
}

/// 中文姓名里的空格去掉：Keycloak 把 First + Last name 用空格拼，中文姓填 First、名填 Last，
/// 拼出来中间就多一个空格 —— 那是英文名的习惯。全是汉字才去，`Jane Doe` 这种原样保留。
fn tidy_name(raw: &str) -> String {
    let name = raw.trim();
    let cjk = |c: char| matches!(c, '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}' | '\u{f900}'..='\u{faff}');
    if !name.is_empty() && name.chars().all(|c| c.is_whitespace() || cjk(c)) {
        return name.chars().filter(|c| !c.is_whitespace()).collect();
    }
    // 英文名只把多余的空白压掉
    name.split_whitespace().collect::<Vec<_>>().join(" ")
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

    fn claims(payload: serde_json::Value) -> Claims {
        decode_claims(&jwt(payload)).unwrap()
    }

    /// 页面上要的是中文姓名，不是登录用的英文账号。
    #[test]
    fn display_name_prefers_the_name_claim() {
        let c = claims(serde_json::json!({
            "sub": "u1", "preferred_username": "easayliu", "name": "刘易", "email": "e@x"
        }));
        assert_eq!(display_name(&c, "easayliu"), "刘易");

        // 没有 name（或者只有空白）就还是账号名，再没有才是邮箱
        let c = claims(serde_json::json!({"preferred_username": "easayliu", "name": "  "}));
        assert_eq!(display_name(&c, "easayliu"), "easayliu");
        let c = claims(serde_json::json!({"email": "e@x"}));
        assert_eq!(display_name(&c, "u1"), "u1");
    }

    /// Keycloak 把 First + Last name 用空格拼，中文填进去中间就多一个空格。
    #[test]
    fn chinese_name_loses_the_keycloak_space() {
        let c = claims(serde_json::json!({
            "preferred_username": "zhangsan", "name": "张 三丰",
            "given_name": "张", "family_name": "三丰"
        }));
        assert_eq!(display_name(&c, "zhangsan"), "张三丰");

        // 英文名的空格要留着
        let c = claims(serde_json::json!({"name": "Jane  Doe"}));
        assert_eq!(display_name(&c, "janedoe"), "Jane Doe", "只压掉多余的空白");
    }

    /// IdP 没给 name claim 时自己拿 given_name + family_name 拼。
    #[test]
    fn falls_back_to_given_and_family_name() {
        let c = claims(serde_json::json!({"given_name": "张", "family_name": "三丰"}));
        assert_eq!(display_name(&c, "zhangsan"), "张三丰");
        let c = claims(serde_json::json!({"given_name": "Jane", "family_name": "Doe"}));
        assert_eq!(display_name(&c, "janedoe"), "Jane Doe");
        // 只填了一栏
        let c = claims(serde_json::json!({"given_name": "张"}));
        assert_eq!(display_name(&c, "zhangsan"), "张");
    }

    /// 显示名换成中文之后，账号名不能跟着变：API key 是按它认归属的。
    #[test]
    fn account_stays_the_login_name_and_old_cookies_still_work() {
        let fresh =
            Session {
                sub: "u1".into(), user: "easayliu".into(), name: "刘易".into(), email: None
            };
        assert_eq!(fresh.account(), "easayliu");

        // 升级前签的会话 cookie 里没有 user 字段，那时 name 存的就是账号名
        let old: Session =
            serde_json::from_str(r#"{"sub":"u1","name":"easayliu","email":null}"#).unwrap();
        assert_eq!(old.account(), "easayliu", "老 cookie 的 key 还得认得回来");
    }
}
