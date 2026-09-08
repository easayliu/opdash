//! 可选的 HTTP Basic 认证。
//!
//! 内网工具，认证只是「别让随便谁都能翻线上日志」这一档的门槛，所以就一组账号密码，
//! 配在 `--basic-auth`。tower-http 自带的 `ValidateRequestHeaderLayer::basic` 已经标了弃用
//! （clippy -D warnings 过不去），自己写一遍也就二十行。

use axum::{
    body::Body,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;

use crate::config::BasicAuth;

pub async fn require(State(auth): State<BasicAuth>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok())
        .and_then(|raw| String::from_utf8(raw).ok())
        .is_some_and(|creds| constant_time_eq(&creds, &format!("{}:{}", auth.user, auth.password)));
    if ok {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"opdash\", charset=\"UTF-8\"")],
        Body::from("需要登录"),
    )
        .into_response()
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
}
