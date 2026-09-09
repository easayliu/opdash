//! 统一错误：给 axum 一个能直接变成 JSON 响应的错误类型。
//!
//! 只分四档：参数错（400）、ClickHouse 拒绝或失败（按错误码折算成 4xx / 5xx）、连不上（502）、
//! 我们自己的 bug（500）。前端只认 `{"error": "..."}` 这一种形状，状态码用来决定要不要提示
//! 「缩小时间范围」这类动作。

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 请求参数不合法：时间范围颠倒、limit 超上限、不认识的列名……
    #[error("{0}")]
    BadRequest(String),
    /// ClickHouse 返回了非 2xx。`code` 是 `DB::Exception` 的错误码（`X-ClickHouse-Exception-Code`）。
    #[error("ClickHouse 错误 {code}: {message}")]
    ClickHouse { code: i32, message: String },
    /// 请求根本没到 ClickHouse，或者中途断了。
    #[error("ClickHouse 不可用: {0}")]
    Unavailable(String),
    /// 我们自己的问题：结果解析不了、配置对不上等。
    #[error("{0}")]
    Internal(String),
    /// 同时在跑的查询太多，排队也没等到名额。
    #[error("查询太多，请稍后再试")]
    Busy,
    /// 同时跟随的连接太多（每条都在按 `--tail-interval` 轮库）。
    #[error("同时跟随的人太多（上限 {0}），请稍后再试或先停掉别的跟随")]
    TooManyTails(usize),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// ClickHouse 的错误码（`src/Common/ErrorCodes.cpp`），只列用到的几个。
pub mod ch_code {
    pub const UNKNOWN_IDENTIFIER: i32 = 47;
    pub const UNKNOWN_TABLE: i32 = 60;
    pub const UNKNOWN_DATABASE: i32 = 81;
    pub const TOO_MANY_ROWS: i32 = 158;
    pub const TIMEOUT_EXCEEDED: i32 = 159;
    pub const TOO_SLOW: i32 = 160;
    pub const READONLY: i32 = 164;
    pub const REQUIRED_PASSWORD: i32 = 194;
    pub const TOO_MANY_SIMULTANEOUS_QUERIES: i32 = 202;
    /// 单独超 `max_bytes_to_read` 时抛这个，不是 396；396 只在行和字节一起限时出现。
    pub const TOO_MANY_BYTES: i32 = 307;
    pub const SOCKET_TIMEOUT: i32 = 209;
    pub const NETWORK_ERROR: i32 = 210;
    pub const MEMORY_LIMIT_EXCEEDED: i32 = 241;
    pub const TOO_MANY_ROWS_OR_BYTES: i32 = 396;
    pub const CANNOT_COMPILE_REGEXP: i32 = 427;
    pub const AUTHENTICATION_FAILED: i32 = 516;
}

impl Error {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Error::BadRequest(msg.into())
    }

    pub fn internal(msg: impl std::fmt::Display) -> Self {
        Error::Internal(msg.to_string())
    }

    /// ClickHouse 错误码 → HTTP 状态码。
    ///
    /// 只挑对用户有意义的几档：超时 / 读太多 / 内存超限 → 504 / 413，前端据此提示「缩小范围」；
    /// 正则写错 → 400（是用户输入的问题）；认证失败、连不上 → 502（部署问题）；其余按 500 算——
    /// 用户改不了 SQL，剩下的大概率是列没建齐或者我们的 SQL 有 bug，日志里会有原文。
    pub fn status(&self) -> StatusCode {
        use ch_code::*;
        match self {
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
            Error::Unavailable(_) => StatusCode::BAD_GATEWAY,
            Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Error::Busy | Error::TooManyTails(_) => StatusCode::SERVICE_UNAVAILABLE,
            Error::ClickHouse { code, .. } => match *code {
                TIMEOUT_EXCEEDED | TOO_SLOW | SOCKET_TIMEOUT => StatusCode::GATEWAY_TIMEOUT,
                TOO_MANY_ROWS | TOO_MANY_BYTES | TOO_MANY_ROWS_OR_BYTES | MEMORY_LIMIT_EXCEEDED => {
                    StatusCode::PAYLOAD_TOO_LARGE
                }
                CANNOT_COMPILE_REGEXP => StatusCode::BAD_REQUEST,
                TOO_MANY_SIMULTANEOUS_QUERIES => StatusCode::SERVICE_UNAVAILABLE,
                AUTHENTICATION_FAILED | REQUIRED_PASSWORD | NETWORK_ERROR | READONLY => {
                    StatusCode::BAD_GATEWAY
                }
                UNKNOWN_TABLE | UNKNOWN_DATABASE | UNKNOWN_IDENTIFIER => {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
        }
    }

    /// 机器可读的分类，和错误响应 JSON 里的 `kind` 是同一个值。跟随的 SSE 流里没有状态码可用，
    /// 只能把它写进事件体，前端两条路径认同一套。
    pub fn kind(&self) -> &'static str {
        match (self, self.status()) {
            (Error::BadRequest(_), _) => "bad_request",
            (_, StatusCode::BAD_REQUEST) => "bad_request",
            (_, StatusCode::GATEWAY_TIMEOUT) => "timeout",
            (_, StatusCode::PAYLOAD_TOO_LARGE) => "too_heavy",
            (_, StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE) => "unavailable",
            _ => "internal",
        }
    }

    /// 给人看的一句话。ClickHouse 的错误文本带着版本号、栈位置这些噪音，只留 `DB::Exception:` 后面
    /// 到 `(CODE_NAME)` 之前的那段，再针对几个常见错误码换成中文提示。
    pub fn user_message(&self) -> String {
        use ch_code::*;
        match self {
            Error::ClickHouse { code, message } => match *code {
                TIMEOUT_EXCEEDED | TOO_SLOW => {
                    "查询超时，请缩小时间范围或加更多筛选条件".to_owned()
                }
                TOO_MANY_ROWS | TOO_MANY_BYTES | TOO_MANY_ROWS_OR_BYTES => {
                    "查询要读的数据太多，请缩小时间范围或加更多筛选条件".to_owned()
                }
                MEMORY_LIMIT_EXCEEDED => "查询内存超限，请缩小时间范围".to_owned(),
                CANNOT_COMPILE_REGEXP => format!("正则表达式无效: {}", trim_regex_message(message)),
                AUTHENTICATION_FAILED | REQUIRED_PASSWORD => {
                    "ClickHouse 认证失败，请检查 opdash 的账号密码配置".to_owned()
                }
                UNKNOWN_TABLE | UNKNOWN_DATABASE => {
                    format!("ClickHouse 里没有配置的库表: {}", trim_ch_message(message))
                }
                UNKNOWN_IDENTIFIER => {
                    format!("表结构和预期不一致（可能缺列）: {}", trim_ch_message(message))
                }
                _ => format!("ClickHouse 错误 {code}: {}", trim_ch_message(message)),
            },
            other => other.to_string(),
        }
    }
}

/// `Code: 47. DB::Exception: Unknown expression identifier ... (UNKNOWN_IDENTIFIER) (version 24.8...)`
/// → `Unknown expression identifier ...`。
pub fn trim_ch_message(raw: &str) -> String {
    let mut s = raw.trim();
    if let Some(idx) = s.find("DB::Exception: ") {
        s = &s[idx + "DB::Exception: ".len()..];
    }
    if let Some(idx) = s.find(" (version ") {
        s = &s[..idx];
    }
    // 结尾的 `(ERROR_NAME)`
    let s = s.trim_end();
    let s = match (s.rfind(" ("), s.ends_with(')')) {
        (Some(idx), true)
            if s[idx + 2..s.len() - 1].chars().all(|c| c.is_ascii_uppercase() || c == '_') =>
        {
            &s[..idx]
        }
        _ => s,
    };
    // 只留第一行，栈信息没人看
    s.lines().next().unwrap_or("").trim().to_owned()
}

/// RE2 的报错后面跟着一大段「看这个链接、SQL 里要双写反斜杠」的说明，对页面上的用户没用。
fn trim_regex_message(raw: &str) -> String {
    let msg = trim_ch_message(raw);
    let msg = msg.split(". Look at ").next().unwrap_or(&msg);
    let msg = msg.split(": while executing").next().unwrap_or(msg);
    msg.trim_start_matches("OptimizedRegularExpression: ").to_owned()
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: String,
    /// ClickHouse 的错误码，没有就省略。前端不解析它，只是排查时好对日志。
    #[serde(skip_serializing_if = "Option::is_none")]
    clickhouse_code: Option<i32>,
    /// 机器可读的分类：`bad_request` / `too_heavy` / `timeout` / `unavailable` / `internal`。
    kind: &'a str,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = self.status();
        let kind = self.kind();
        if status.is_server_error() {
            tracing::error!(status = status.as_u16(), error = %self, "request failed");
        } else {
            tracing::debug!(status = status.as_u16(), error = %self, "request rejected");
        }
        let body = ErrorBody {
            error: self.user_message(),
            clickhouse_code: match &self {
                Error::ClickHouse { code, .. } => Some(*code),
                _ => None,
            },
            kind,
        };
        (status, Json(body)).into_response()
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            Error::ClickHouse { code: ch_code::TIMEOUT_EXCEEDED, message: e.to_string() }
        } else if e.is_connect() || e.is_request() {
            Error::Unavailable(e.to_string())
        } else {
            Error::Internal(e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_clickhouse_noise() {
        let raw = "Code: 47. DB::Exception: Unknown expression identifier `nope`. In scope SELECT nope FROM logs.app_log. (UNKNOWN_IDENTIFIER) (version 26.9.1.943 (official build))";
        assert_eq!(
            trim_ch_message(raw),
            "Unknown expression identifier `nope`. In scope SELECT nope FROM logs.app_log."
        );
        assert_eq!(trim_ch_message("plain text"), "plain text");
        assert_eq!(
            trim_ch_message(
                "Code: 159. DB::Exception: Timeout exceeded: elapsed 1006 ms, maximum: 1000 ms. (TIMEOUT_EXCEEDED) (version 24.8)"
            ),
            "Timeout exceeded: elapsed 1006 ms, maximum: 1000 ms."
        );
    }

    #[test]
    fn regex_error_is_short() {
        let raw = "Code: 427. DB::Exception: OptimizedRegularExpression: cannot compile re2: (unclosed, error: missing ): (unclosed. Look at https://github.com/google/re2/wiki/Syntax for reference. Please note that ... (CANNOT_COMPILE_REGEXP) (version 26.9)";
        let e = Error::ClickHouse { code: ch_code::CANNOT_COMPILE_REGEXP, message: raw.into() };
        assert_eq!(
            e.user_message(),
            "正则表达式无效: cannot compile re2: (unclosed, error: missing ): (unclosed"
        );
    }

    /// `max_bytes_to_read` 护栏触发时 ClickHouse 抛 307，得和 158 / 396 一样提示缩小范围，
    /// 而不是落到默认分支给用户看 500 + 英文原文。
    #[test]
    fn too_many_bytes_is_payload_too_large() {
        let raw = "Code: 307. DB::Exception: Limit for rows or bytes to read exceeded, max bytes: 20.00 GiB, current bytes: 20.15 GiB: While executing MergeTreeSelect(pool: ReadPool, algorithm: Thread). (TOO_MANY_BYTES) (version 26.9)";
        let e = Error::ClickHouse { code: ch_code::TOO_MANY_BYTES, message: raw.into() };
        assert_eq!(e.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(e.user_message(), "查询要读的数据太多，请缩小时间范围或加更多筛选条件");
        for code in [ch_code::TOO_MANY_ROWS, ch_code::TOO_MANY_ROWS_OR_BYTES] {
            let e = Error::ClickHouse { code, message: String::new() };
            assert_eq!(e.status(), StatusCode::PAYLOAD_TOO_LARGE);
            assert_eq!(e.user_message(), "查询要读的数据太多，请缩小时间范围或加更多筛选条件");
        }
    }

    #[test]
    fn maps_codes_to_status() {
        let e = Error::ClickHouse { code: ch_code::TIMEOUT_EXCEEDED, message: String::new() };
        assert_eq!(e.status(), StatusCode::GATEWAY_TIMEOUT);
        let e = Error::ClickHouse { code: ch_code::CANNOT_COMPILE_REGEXP, message: String::new() };
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        let e = Error::ClickHouse { code: 9999, message: String::new() };
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(Error::bad_request("x").status(), StatusCode::BAD_REQUEST);
    }
}
