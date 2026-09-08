//! 查询串解析。
//!
//! 不用 axum 的 `Query<T>`：它不支持重复的键（`attr=k=v` 要能给多个），而且 serde 的报错
//! 是「invalid type: string, expected i64」这种话，用户看不懂是哪个参数错了。
//! 这里自己拆开，逐个参数按名字报错。

use std::collections::HashSet;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default)]
pub struct Params(Vec<(String, String)>);

impl Params {
    pub fn parse(raw: Option<&str>) -> Self {
        Self(
            form_urlencoded::parse(raw.unwrap_or("").as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
        )
    }

    /// 最后一个同名值（`a=1&a=2` → `2`）。空串当没给。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }

    pub fn get_all(&self, key: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
            .collect()
    }

    /// 逗号分隔 + 重复键，都算多个值。
    pub fn get_list(&self, key: &str) -> Vec<String> {
        let mut out = Vec::new();
        for raw in self.get_all(key) {
            for item in raw.split(',') {
                let item = item.trim();
                if !item.is_empty() && !out.iter().any(|x| x == item) {
                    out.push(item.to_owned());
                }
            }
        }
        out
    }

    pub fn get_i64(&self, key: &str) -> Result<Option<i64>> {
        self.get(key)
            .map(|v| {
                v.parse::<i64>()
                    .map_err(|_| Error::bad_request(format!("参数 {key} 应为整数，不是 {v:?}")))
            })
            .transpose()
    }

    pub fn get_u32(&self, key: &str) -> Result<Option<u32>> {
        self.get(key)
            .map(|v| {
                v.parse::<u32>()
                    .map_err(|_| Error::bad_request(format!("参数 {key} 应为非负整数，不是 {v:?}")))
            })
            .transpose()
    }

    pub fn get_f64(&self, key: &str) -> Result<Option<f64>> {
        self.get(key)
            .map(|v| {
                v.parse::<f64>()
                    .map_err(|_| Error::bad_request(format!("参数 {key} 应为数字，不是 {v:?}")))
            })
            .transpose()
    }

    /// `1` / `true` / `yes` 为真，`0` / `false` / `no` 为假。
    pub fn get_bool(&self, key: &str) -> Result<Option<bool>> {
        match self.get(key).map(str::to_ascii_lowercase).as_deref() {
            None => Ok(None),
            Some("1" | "true" | "yes" | "on") => Ok(Some(true)),
            Some("0" | "false" | "no" | "off") => Ok(Some(false)),
            Some(other) => {
                Err(Error::bad_request(format!("参数 {key} 应为 true / false，不是 {other:?}")))
            }
        }
    }

    /// 限制在 `[1, max]`；没给用 `default`。
    pub fn get_limit(&self, key: &str, default: u32, max: u32) -> Result<u32> {
        Ok(self.get_u32(key)?.unwrap_or(default).clamp(1, max))
    }

    /// 出现过的键（去重、保序）。
    pub fn keys(&self) -> Vec<&str> {
        let mut seen = HashSet::new();
        self.0.iter().map(|(k, _)| k.as_str()).filter(|k| seen.insert(*k)).collect()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Params {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> std::result::Result<Self, Self::Rejection> {
        Ok(Params::parse(parts.uri.query()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lists_numbers_bools() {
        let p = Params::parse(Some(
            "level=ERROR,WARN&level=INFO&from=12&regex=true&q=%E6%94%AF%E4%BB%98+x&empty=",
        ));
        assert_eq!(p.get_list("level"), ["ERROR", "WARN", "INFO"]);
        assert_eq!(p.get_i64("from").unwrap(), Some(12));
        assert_eq!(p.get_bool("regex").unwrap(), Some(true));
        assert_eq!(p.get("q"), Some("支付 x"));
        assert_eq!(p.get("empty"), None);
        assert_eq!(p.get("missing"), None);
        assert!(p.get_i64("q").is_err());
        assert_eq!(p.get_limit("limit", 200, 1000).unwrap(), 200);
        assert_eq!(p.keys(), ["level", "from", "regex", "q", "empty"]);
    }

    #[test]
    fn limit_is_clamped() {
        let p = Params::parse(Some("limit=99999&zero=0"));
        assert_eq!(p.get_limit("limit", 200, 1000).unwrap(), 1000);
        assert_eq!(p.get_limit("zero", 200, 1000).unwrap(), 1);
        assert!(Params::parse(Some("limit=-1")).get_u32("limit").is_err());
    }
}
