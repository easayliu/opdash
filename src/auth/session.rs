//! 签名 cookie：把一小段 JSON 用 HMAC-SHA256 签一下塞进 cookie，服务端不用存任何会话状态。
//!
//! 格式 `base64url(json).base64url(hmac)`。JSON 里带过期时间，签名覆盖整段，改哪一位都验不过。
//! 同一把钥匙签两种东西：登录中的临时票（state / PKCE verifier / nonce，10 分钟）和登录后的会话。
//! 两者的 `kind` 字段不同，拿会话 cookie 冒充登录票（或反过来）也过不了。

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::hmac;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub struct Sealer {
    key: hmac::Key,
}

#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    kind: &'static str,
    /// 过期时间，unix 秒
    exp: i64,
    v: T,
}

/// serde 反序列化 `&'static str` 不行，读的时候 kind 用 String。
#[derive(Deserialize)]
struct EnvelopeOwned<T> {
    kind: String,
    exp: i64,
    v: T,
}

impl Sealer {
    /// 没配密钥就随机一把：进程重启后旧 cookie 全部失效，单副本部署可以接受。
    pub fn new(secret: Option<&str>) -> Self {
        let key = match secret {
            Some(s) if !s.is_empty() => hmac::Key::new(hmac::HMAC_SHA256, s.as_bytes()),
            _ => {
                let rng = ring::rand::SystemRandom::new();
                hmac::Key::generate(hmac::HMAC_SHA256, &rng).expect("system rng")
            }
        };
        Self { key }
    }

    pub fn seal<T: Serialize>(&self, kind: &'static str, value: &T, ttl_secs: i64) -> String {
        let exp = now_secs() + ttl_secs;
        let payload = serde_json::to_vec(&Envelope { kind, exp, v: value }).expect("serialize");
        let tag = hmac::sign(&self.key, &payload);
        format!("{}.{}", B64.encode(&payload), B64.encode(tag.as_ref()))
    }

    /// 签名不对、格式不对、过期、kind 不符都返回 None——对调用方来说都等于「没登录」。
    pub fn open<T: DeserializeOwned>(&self, kind: &str, token: &str) -> Option<T> {
        let (payload, tag) = token.split_once('.')?;
        let payload = B64.decode(payload).ok()?;
        let tag = B64.decode(tag).ok()?;
        hmac::verify(&self.key, &payload, &tag).ok()?;
        let env: EnvelopeOwned<T> = serde_json::from_slice(&payload).ok()?;
        if env.kind != kind || env.exp <= now_secs() {
            return None;
        }
        Some(env.v)
    }
}

pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `n` 个随机字节的 base64url，给 state / nonce / PKCE verifier 用。
pub fn random_token(n: usize) -> String {
    use ring::rand::SecureRandom;
    let mut buf = vec![0u8; n];
    ring::rand::SystemRandom::new().fill(&mut buf).expect("system rng");
    B64.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_reject_tampering() {
        let s = Sealer::new(Some("k"));
        let token = s.seal("session", &vec!["a", "b"], 60);
        assert_eq!(s.open::<Vec<String>>("session", &token), Some(vec!["a".into(), "b".into()]));
        // 换 kind、改一个字符、换钥匙都不认
        assert!(s.open::<Vec<String>>("login", &token).is_none());
        let mut bad = token.clone();
        bad.replace_range(0..1, if token.starts_with('A') { "B" } else { "A" });
        assert!(s.open::<Vec<String>>("session", &bad).is_none());
        assert!(Sealer::new(Some("other")).open::<Vec<String>>("session", &token).is_none());
    }

    #[test]
    fn expired_is_none() {
        let s = Sealer::new(None);
        let token = s.seal("session", &1u8, -1);
        assert!(s.open::<u8>("session", &token).is_none());
    }

    #[test]
    fn random_tokens_differ() {
        assert_ne!(random_token(16), random_token(16));
        assert_eq!(random_token(32).len(), 43);
    }
}
