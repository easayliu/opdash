//! HTTP Basic：一组共享账号密码，配在 `--basic-auth`。
//!
//! tower-http 自带的 `ValidateRequestHeaderLayer::basic` 已经标了弃用（clippy -D warnings 过不去），
//! 自己写也就十几行。

use axum::http::{HeaderMap, header};
use base64::Engine;

use crate::config::BasicAuth;

/// Authorization 头里的账号密码对不对。
pub fn check(auth: &BasicAuth, headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok())
        .and_then(|raw| String::from_utf8(raw).ok())
        .is_some_and(|creds| constant_time_eq(&creds, &format!("{}:{}", auth.user, auth.password)))
}

/// 长度不同直接判否，长度相同时逐字节比较完再出结果，不给按前缀猜密码的机会。
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_whole_string() {
        assert!(constant_time_eq("ops:pw", "ops:pw"));
        assert!(!constant_time_eq("ops:pw", "ops:pW"));
        assert!(!constant_time_eq("ops:pw", "ops:pw1"));
    }

    #[test]
    fn checks_header() {
        let auth = BasicAuth { user: "ops".into(), password: "secret".into() };
        let mut h = HeaderMap::new();
        assert!(!check(&auth, &h));
        h.insert(header::AUTHORIZATION, "Basic b3BzOnNlY3JldA==".parse().unwrap());
        assert!(check(&auth, &h));
        h.insert(header::AUTHORIZATION, "Basic b3BzOndyb25n".parse().unwrap());
        assert!(!check(&auth, &h));
    }
}
