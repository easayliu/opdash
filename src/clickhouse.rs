//! ClickHouse HTTP 客户端：只读查询、参数绑定、`JSONEachRow` 解码。
//!
//! 走 HTTP 而不是 native 协议，理由和 logpipe / tracepipe 一样：一个 reqwest 就够，不用拖
//! clickhouse-rs 那一堆依赖；而且 `param_*` 参数绑定、`X-ClickHouse-Summary`、`readonly`
//! 这些能力 HTTP 接口都有。
//!
//! **所有用户输入都通过 `{name:Type}` 查询参数传进去**，SQL 文本里只有我们自己写的字面量和
//! 白名单里的列名（见 [`crate::schema`]）。参数值在 URL 里以 `param_name=value` 传，
//! `String` 类型的值 ClickHouse 原样接收，不需要转义；`Array(String)` 要按 SQL 字面量写，
//! 见 [`array_literal`]。
//!
//! 每个请求固定带的设置：
//! * `readonly=2`：禁止写，但允许在 URL 里改设置（`readonly=1` 连 `max_execution_time` 都不让传）；
//! * `max_execution_time`：超时由 ClickHouse 自己掐，返回 159 错误码，比客户端断连干净；
//! * `wait_end_of_query=1`：结果攒完再发，查询中途出错时能收到干净的 5xx，而不是 200 + 半截 JSON；
//! * `output_format_json_quote_64bit_integers=0`：`UInt64`（duration_ns）按数字而不是字符串输出。

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{CONTENT_TYPE, HeaderMap};
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;

use crate::error::{Error, Result, ch_code};

/// 排队等一个查询名额最多等多久。
const QUEUE_WAIT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    endpoint: String,
    user: String,
    password: String,
    timeout: Duration,
    max_read_bytes: u64,
    max_read_rows: u64,
    /// 同时最多几条查询在 ClickHouse 上跑。库是全公司共用的，一个页面刷出十几条 30 秒的
    /// 关键字扫描不能都放出去；排队等不到就回 503，前端提示稍后再试。
    permits: Arc<Semaphore>,
}

#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// `http://host:8123`
    pub endpoint: String,
    pub user: String,
    pub password: String,
    /// 传给 `max_execution_time`；HTTP 层的超时比它多留 10 秒，让 ClickHouse 先报错。
    pub timeout: Duration,
    /// `max_bytes_to_read`，0 = 不限。
    pub max_read_bytes: u64,
    /// `max_rows_to_read`，0 = 不限。
    pub max_read_rows: u64,
    /// 同时在跑的查询上限。
    pub max_concurrent: usize,
}

impl Client {
    pub fn new(opts: ClientOptions) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(opts.timeout + Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(8)
            .build()
            .map_err(|e| Error::internal(format!("build http client: {e}")))?;
        Ok(Self {
            http,
            endpoint: opts.endpoint.trim_end_matches('/').to_owned(),
            user: opts.user,
            password: opts.password,
            timeout: opts.timeout,
            max_read_bytes: opts.max_read_bytes,
            max_read_rows: opts.max_read_rows,
            permits: Arc::new(Semaphore::new(opts.max_concurrent.max(1))),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// 连通性检查：`SELECT 1`。
    pub async fn ping(&self) -> Result<()> {
        let resp = self.send(&Query::new("SELECT 1"), None).await?;
        let body = resp.text().await?;
        if body.trim() == "1" {
            Ok(())
        } else {
            Err(Error::Unavailable(format!("SELECT 1 返回了 {body:?}")))
        }
    }

    /// 执行查询，结果按 `JSONEachRow` 逐行解析成 `T`。
    pub async fn rows<T: DeserializeOwned>(&self, query: Query) -> Result<Rows<T>> {
        let resp = self.send(&query, Some("JSONEachRow")).await?;
        let stats = Stats::from_headers(resp.headers());
        let body = resp.bytes().await?;
        let mut rows = Vec::new();
        for (i, line) in body.split(|b| *b == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let row = serde_json::from_slice::<T>(line).map_err(|e| {
                let preview = String::from_utf8_lossy(&line[..line.len().min(200)]);
                Error::internal(format!(
                    "解析 ClickHouse 第 {} 行结果失败: {e}; 内容: {preview}",
                    i + 1
                ))
            })?;
            rows.push(row);
        }
        Ok(Rows { rows, stats })
    }

    /// 同一次扫描里顺带算出「不加 LIMIT 一共多少行」：`exact_rows_before_limit=1` + `FORMAT JSON`
    /// 的 `rows_before_limit_at_least`。代价是 ClickHouse 不能读够 LIMIT 就停，所以只给本来就要
    /// 扫完整个范围的查询用（消息关键字没有索引，逐行扫是免不了的），别的查询另起 `count()`。
    pub async fn rows_with_total<T: DeserializeOwned>(
        &self,
        query: Query,
    ) -> Result<RowsWithTotal<T>> {
        let query = query.setting("exact_rows_before_limit", 1);
        let resp = self.send(&query, Some("JSON")).await?;
        let stats = Stats::from_headers(resp.headers());
        let body = resp.bytes().await?;
        let envelope: JsonEnvelope<T> = serde_json::from_slice(&body).map_err(|e| {
            let preview = String::from_utf8_lossy(&body[..body.len().min(200)]);
            Error::internal(format!("解析 ClickHouse JSON 结果失败: {e}; 内容: {preview}"))
        })?;
        Ok(RowsWithTotal { rows: envelope.data, total: envelope.rows_before_limit_at_least, stats })
    }

    /// 发请求，非 2xx 转成 [`Error::ClickHouse`]。`format` 给了就追加 `FORMAT xxx`。
    /// 导出这类要流式转发的场景直接拿 `Response` 用。
    pub async fn send(&self, query: &Query, format: Option<&str>) -> Result<reqwest::Response> {
        let _permit = tokio::time::timeout(QUEUE_WAIT, self.permits.acquire())
            .await
            .map_err(|_| Error::Busy)?
            .map_err(|_| Error::internal("semaphore closed"))?;
        let started = Instant::now();
        let request = self.build(query, format);
        let resp = request.send().await?;
        let status = resp.status();
        if status.is_success() {
            tracing::debug!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                sql = %query.sql.replace('\n', " "),
                params = ?query.params,
                "clickhouse ok"
            );
            return Ok(resp);
        }
        let header_code = resp
            .headers()
            .get("X-ClickHouse-Exception-Code")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<i32>().ok());
        let text = resp.text().await.unwrap_or_default();
        let code = header_code.or_else(|| parse_error_code(&text)).unwrap_or(-1);
        tracing::warn!(
            http = status.as_u16(),
            code,
            elapsed_ms = started.elapsed().as_millis() as u64,
            sql = %query.sql.replace('\n', " "),
            params = ?query.params,
            error = %text.lines().next().unwrap_or(""),
            "clickhouse rejected query"
        );
        Err(Error::ClickHouse { code, message: text })
    }

    fn build(&self, query: &Query, format: Option<&str>) -> reqwest::RequestBuilder {
        let mut url = reqwest::Url::parse(&format!("{}/", self.endpoint))
            .unwrap_or_else(|_| reqwest::Url::parse("http://127.0.0.1:8123/").expect("static url"));
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("readonly", "2");
            // 用户关掉页面 / 改了条件重新查，上一条查询没必要再跑完
            pairs.append_pair("cancel_http_readonly_queries_on_client_close", "1");
            pairs.append_pair("max_execution_time", &self.timeout.as_secs().to_string());
            pairs.append_pair("wait_end_of_query", "1");
            pairs.append_pair("output_format_json_quote_64bit_integers", "0");
            pairs.append_pair("enable_http_compression", "1");
            if self.max_read_bytes > 0 {
                pairs.append_pair("max_bytes_to_read", &self.max_read_bytes.to_string());
            }
            if self.max_read_rows > 0 {
                pairs.append_pair("max_rows_to_read", &self.max_read_rows.to_string());
            }
            for (name, value) in &query.settings {
                pairs.append_pair(name, value);
            }
            for (name, value) in &query.params {
                pairs.append_pair(&format!("param_{name}"), value);
            }
        }
        let sql = match format {
            Some(f) => format!("{}\nFORMAT {f}", query.sql),
            None => query.sql.clone(),
        };
        let mut req = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "text/plain; charset=utf-8")
            .header("X-ClickHouse-User", &self.user)
            .body(sql);
        if !self.password.is_empty() {
            req = req.header("X-ClickHouse-Key", &self.password);
        }
        req
    }
}

/// `Code: 159. DB::Exception: ...` 里的 159。老版本 / 某些代理不回 `X-ClickHouse-Exception-Code`
/// 头时从正文里抠。
pub fn parse_error_code(text: &str) -> Option<i32> {
    let rest = text.trim_start().strip_prefix("Code: ")?;
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    rest[..end].parse().ok()
}

/// 一条待执行的查询：SQL 文本 + 绑定参数 + 附加设置。
#[derive(Debug, Clone)]
pub struct Query {
    sql: String,
    params: Vec<(String, String)>,
    settings: Vec<(&'static str, String)>,
}

impl Query {
    pub fn new(sql: impl Into<String>) -> Self {
        Self { sql: sql.into(), params: Vec::new(), settings: Vec::new() }
    }

    /// 绑定 `{name:Type}` 参数。值的文本格式由 [`ToParam`] 决定。
    pub fn param(mut self, name: impl Into<String>, value: impl ToParam) -> Self {
        self.params.push((name.into(), value.to_param()));
        self
    }

    /// 一次挂上一批已经序列化好的参数（[`crate::query::Bindings`] 产出的）。
    pub fn with_params(mut self, params: Vec<(String, String)>) -> Self {
        self.params.extend(params);
        self
    }

    pub fn setting(mut self, name: &'static str, value: impl ToString) -> Self {
        self.settings.push((name, value.to_string()));
        self
    }

    pub fn sql(&self) -> &str {
        &self.sql
    }

    pub fn params(&self) -> &[(String, String)] {
        &self.params
    }

    pub fn settings(&self) -> &[(&'static str, String)] {
        &self.settings
    }
}

/// 值 → `param_x=` 的文本。`String` 原样；数字按十进制；数组按 SQL 字面量。
pub trait ToParam {
    fn to_param(&self) -> String;
}

/// ClickHouse 按 TSV 的转义规则解析 `param_*` 里 `String` 类型的值：`\d` 这类反斜杠序列会被
/// 解成别的字符（`\b` 变成退格），真实的 TAB / 换行直接报 457。所以发出去之前按同一套规则
/// 转义：`\` → `\\`、TAB → `\t`、LF → `\n`、CR → `\r`、NUL → `\0`。引号和非 ASCII 原样。
/// 正则模式下用户写的 `\d+` 就靠这一步才能原样到达 `match()`。
pub fn tsv_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out
}

impl ToParam for str {
    fn to_param(&self) -> String {
        tsv_escape(self)
    }
}

impl ToParam for String {
    fn to_param(&self) -> String {
        tsv_escape(self)
    }
}

impl<T: ToParam + ?Sized> ToParam for &T {
    fn to_param(&self) -> String {
        (**self).to_param()
    }
}

macro_rules! to_param_display {
    ($($t:ty),*) => {$(
        impl ToParam for $t {
            fn to_param(&self) -> String {
                self.to_string()
            }
        }
    )*};
}
to_param_display!(i32, i64, u32, u64, f64, usize);

impl ToParam for bool {
    fn to_param(&self) -> String {
        if *self { "1" } else { "0" }.to_owned()
    }
}

impl ToParam for [String] {
    fn to_param(&self) -> String {
        array_literal(self.iter().map(String::as_str))
    }
}

impl ToParam for Vec<String> {
    fn to_param(&self) -> String {
        self.as_slice().to_param()
    }
}

impl ToParam for [&str] {
    fn to_param(&self) -> String {
        array_literal(self.iter().copied())
    }
}

/// `Array(String)` 参数的字面量：`['a','it\'s','x\\y']`。单引号、反斜杠和控制字符按 SQL
/// 字符串字面量的规则用反斜杠转义，其它字符（含中文）原样。
pub fn array_literal<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::from("[");
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('\'');
        for c in item.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\'' => out.push_str("\\'"),
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\0' => out.push_str("\\0"),
                c => out.push(c),
            }
        }
        out.push('\'');
    }
    out.push(']');
    out
}

/// `X-ClickHouse-Summary` 里的执行统计，回给前端显示「扫描了多少行、花了多久」。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Stats {
    pub read_rows: u64,
    pub read_bytes: u64,
    pub result_rows: u64,
    pub elapsed_ms: f64,
}

impl Stats {
    /// 两次往返合成一份：读量相加，结果行数取后一步的（前一步只是中间结果）。
    pub fn absorb(&mut self, next: &Stats) {
        self.read_rows += next.read_rows;
        self.read_bytes += next.read_bytes;
        self.elapsed_ms += next.elapsed_ms;
        self.result_rows = next.result_rows;
    }

    pub fn from_headers(headers: &HeaderMap) -> Self {
        let Some(raw) = headers.get("X-ClickHouse-Summary").and_then(|v| v.to_str().ok()) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            return Self::default();
        };
        // 值都是字符串形式的数字："read_rows":"1"
        let num = |key: &str| -> u64 {
            value
                .get(key)
                .and_then(|v| match v {
                    serde_json::Value::String(s) => s.parse().ok(),
                    serde_json::Value::Number(n) => n.as_u64(),
                    _ => None,
                })
                .unwrap_or(0)
        };
        Self {
            read_rows: num("read_rows"),
            read_bytes: num("read_bytes"),
            result_rows: num("result_rows"),
            elapsed_ms: num("elapsed_ns") as f64 / 1_000_000.0,
        }
    }
}

#[derive(Debug)]
pub struct Rows<T> {
    pub rows: Vec<T>,
    pub stats: Stats,
}

#[derive(Debug)]
pub struct RowsWithTotal<T> {
    pub rows: Vec<T>,
    /// 不加 LIMIT 时的总行数（`exact_rows_before_limit=1` 下是精确值）
    pub total: Option<u64>,
    pub stats: Stats,
}

/// `FORMAT JSON` 的外壳，只取用得着的两个字段。
#[derive(serde::Deserialize)]
struct JsonEnvelope<T> {
    data: Vec<T>,
    #[serde(default, deserialize_with = "num::de_opt")]
    rows_before_limit_at_least: Option<u64>,
}

/// 数字列的宽容反序列化：数字和字符串形式的数字都收。
///
/// 我们每次都发 `output_format_json_quote_64bit_integers=0`，正常情况下 64 位整数是 JSON 数字；
/// 但这个设置在 24.8 上默认是 1，万一账号的 profile 锁了设置（`readonly=1` 之类）改不了，
/// `"duration_ns":"123"` 这种带引号的形式也得能解，不然整页报错。
pub mod num {
    use std::fmt::Display;
    use std::str::FromStr;

    use serde::{Deserialize, Deserializer, de::Error as _};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrStr<T> {
        Num(T),
        Str(String),
    }

    fn convert<'de, D: Deserializer<'de>, T>(raw: NumOrStr<T>) -> Result<T, D::Error>
    where
        T: FromStr,
        T::Err: Display,
    {
        match raw {
            NumOrStr::Num(n) => Ok(n),
            NumOrStr::Str(s) => {
                s.trim().parse::<T>().map_err(|e| D::Error::custom(format!("{s:?} 不是数字: {e}")))
            }
        }
    }

    pub fn de<'de, D, T>(d: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de> + FromStr,
        T::Err: Display,
    {
        convert::<D, T>(NumOrStr::<T>::deserialize(d)?)
    }

    pub fn de_opt<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de> + FromStr,
        T::Err: Display,
    {
        match Option::<NumOrStr<T>>::deserialize(d)? {
            None => Ok(None),
            Some(raw) => convert::<D, T>(raw).map(Some),
        }
    }

    pub fn de_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de> + FromStr,
        T::Err: Display,
    {
        Vec::<NumOrStr<T>>::deserialize(d)?.into_iter().map(convert::<D, T>).collect()
    }
}

impl From<Error> for ErrorKind {
    fn from(e: Error) -> Self {
        match e {
            Error::ClickHouse { code: ch_code::TIMEOUT_EXCEEDED, .. } => ErrorKind::Timeout,
            Error::ClickHouse { .. } => ErrorKind::Rejected,
            Error::Unavailable(_) => ErrorKind::Unavailable,
            _ => ErrorKind::Other,
        }
    }
}

/// 给健康检查用的粗分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Timeout,
    Rejected,
    Unavailable,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_literal_escapes() {
        assert_eq!(array_literal(["a", "it's", "x\\y", "支付"]), r"['a','it\'s','x\\y','支付']");
        assert_eq!(array_literal(["a\nb\tc"]), r"['a\nb\tc']");
        assert_eq!(array_literal(std::iter::empty()), "[]");
    }

    #[test]
    fn string_params_are_tsv_escaped() {
        assert_eq!("plain 支付".to_param(), "plain 支付");
        assert_eq!(r"\d+ \w".to_param(), r"\\d+ \\w");
        assert_eq!("a\tb\nc\r".to_param(), r"a\tb\nc\r");
        assert_eq!("it's \"q\"".to_param(), "it's \"q\"");
        assert_eq!(String::from("x\\").to_param(), "x\\\\");
    }

    #[test]
    fn parses_error_code_from_body() {
        assert_eq!(parse_error_code("Code: 159. DB::Exception: Timeout"), Some(159));
        assert_eq!(parse_error_code("  Code: 47. x"), Some(47));
        assert_eq!(parse_error_code("nope"), None);
    }

    #[test]
    fn stats_from_summary_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "X-ClickHouse-Summary",
            r#"{"read_rows":"123","read_bytes":"4567","result_rows":"3","elapsed_ns":"1500000"}"#
                .parse()
                .unwrap(),
        );
        let s = Stats::from_headers(&headers);
        assert_eq!(s.read_rows, 123);
        assert_eq!(s.read_bytes, 4567);
        assert_eq!(s.result_rows, 3);
        assert!((s.elapsed_ms - 1.5).abs() < 1e-9);
        assert_eq!(Stats::from_headers(&HeaderMap::new()).read_rows, 0);
    }

    #[test]
    fn tolerant_numbers() {
        #[derive(serde::Deserialize)]
        struct Row {
            #[serde(deserialize_with = "num::de")]
            a: u64,
            #[serde(deserialize_with = "num::de")]
            b: i64,
            #[serde(deserialize_with = "num::de_vec")]
            c: Vec<i64>,
            #[serde(default, deserialize_with = "num::de_opt")]
            d: Option<u64>,
        }
        let r: Row = serde_json::from_str(r#"{"a":"123","b":-4,"c":["1",2],"d":"9"}"#).unwrap();
        assert_eq!((r.a, r.b, r.c, r.d), (123, -4, vec![1, 2], Some(9)));
        let r: Row = serde_json::from_str(r#"{"a":1,"b":"2","c":[]}"#).unwrap();
        assert_eq!(r.d, None);
        assert!(serde_json::from_str::<Row>(r#"{"a":"x","b":1,"c":[]}"#).is_err());
    }

    #[test]
    fn query_collects_params_and_settings() {
        let q = Query::new("SELECT {a:String}")
            .param("a", "x")
            .param("n", 5u32)
            .param("arr", vec!["p".to_owned()])
            .setting("max_result_rows", 10);
        assert_eq!(
            q.params(),
            &[
                ("a".to_owned(), "x".to_owned()),
                ("n".to_owned(), "5".to_owned()),
                ("arr".to_owned(), "['p']".to_owned())
            ]
        );
        assert_eq!(q.settings(), &[("max_result_rows", "10".to_owned())]);
    }
}
