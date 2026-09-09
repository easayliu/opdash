//! 假 ClickHouse：一个只会按顺序回放预设响应的 HTTP 服务，同时把收到的请求记下来。
//!
//! ClickHouse 走的是 HTTP，对着它就能断言 opdash 发出去的 SQL / `param_*` / 设置 / 认证头，
//! 不用真起一个库。和 logpipe 的 tests/clickhouse_http.rs 同一套思路。

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
pub struct Captured {
    /// `/?readonly=2&param_db=logs...`
    pub target: String,
    /// 全部请求头，小写，`name: value` 一行一个
    pub headers: String,
    /// 请求体（SQL）
    pub body: String,
}

impl Captured {
    /// URL 里的查询参数，已解码。
    pub fn query(&self) -> Vec<(String, String)> {
        let raw = self.target.split_once('?').map(|(_, q)| q).unwrap_or("");
        url_decode_pairs(raw)
    }

    pub fn query_value(&self, key: &str) -> Option<String> {
        self.query().into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{}: ", name.to_lowercase())).map(str::to_owned))
    }
}

fn url_decode_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(b) => {
                    out.push(b);
                    i += 2;
                }
                Err(_) => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone)]
pub struct Canned {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

pub struct FakeClickhouse {
    endpoint: String,
    requests: Arc<Mutex<Vec<Captured>>>,
    responses: Arc<Mutex<VecDeque<Canned>>>,
}

impl FakeClickhouse {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<Captured>>> = Arc::default();
        let responses: Arc<Mutex<VecDeque<Canned>>> = Arc::default();
        {
            let requests = requests.clone();
            let responses = responses.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else { break };
                    let requests = requests.clone();
                    let responses = responses.clone();
                    tokio::spawn(async move {
                        let mut stream = stream;
                        let Some(captured) = read_request(&mut stream).await else { return };
                        requests.lock().unwrap().push(captured);
                        let canned = responses.lock().unwrap().pop_front().unwrap_or(Canned {
                            status: 200,
                            headers: Vec::new(),
                            body: String::new(),
                        });
                        let mut head = format!(
                            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                            canned.status,
                            if canned.status == 200 { "OK" } else { "Error" },
                            canned.body.len()
                        );
                        for (k, v) in &canned.headers {
                            head.push_str(&format!("{k}: {v}\r\n"));
                        }
                        head.push_str("\r\n");
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.write_all(canned.body.as_bytes()).await;
                        let _ = stream.flush().await;
                    });
                }
            });
        }
        Self { endpoint: format!("http://{addr}"), requests, responses }
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// 下一个请求回这个 200 响应体（JSONEachRow 一行一个 JSON）。
    pub fn respond(&self, body: impl Into<String>) -> &Self {
        self.respond_with(200, Vec::new(), body)
    }

    pub fn respond_with(
        &self,
        status: u16,
        headers: Vec<(String, String)>,
        body: impl Into<String>,
    ) -> &Self {
        self.responses.lock().unwrap().push_back(Canned { status, headers, body: body.into() });
        self
    }

    /// 模拟 ClickHouse 报错：HTTP 500 + `X-ClickHouse-Exception-Code` + `Code: N. DB::Exception: ...`。
    pub fn respond_error(&self, code: i32, message: &str) -> &Self {
        self.respond_with(
            500,
            vec![("X-ClickHouse-Exception-Code".into(), code.to_string())],
            format!("Code: {code}. DB::Exception: {message} (SOME_ERROR) (version 24.8.1.1)"),
        )
    }

    pub fn requests(&self) -> Vec<Captured> {
        self.requests.lock().unwrap().clone()
    }

    pub fn last_request(&self) -> Captured {
        self.requests.lock().unwrap().last().cloned().expect("no request captured")
    }
}

/// 读完整个 HTTP 请求：头 + 按 Content-Length 读 body。
async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<Captured> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_owned();
    let target = request_line.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let headers: String = lines
        .filter(|l| !l.is_empty())
        .map(|l| {
            let (k, v) = l.split_once(':').unwrap_or((l, ""));
            format!("{}: {}", k.trim().to_lowercase(), v.trim())
        })
        .collect::<Vec<_>>()
        .join("\n");
    let content_length: usize = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length: "))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = raw[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut buf).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    Some(Captured { target, headers, body: String::from_utf8_lossy(&body).to_string() })
}

/// system.columns 的标准回放：两张表都有，日志表带 k8s 元数据列和 cluster。
pub fn columns_fixture() -> String {
    let logs = [
        ("timestamp", "DateTime64(3, 'Asia/Shanghai')"),
        ("service_name", "LowCardinality(String)"),
        ("level", "LowCardinality(String)"),
        ("logger", "String"),
        ("thread", "String"),
        ("trace_id", "String"),
        ("span_id", "String"),
        ("namespace", "LowCardinality(String)"),
        ("pod", "String"),
        ("container", "LowCardinality(String)"),
        ("stream", "LowCardinality(String)"),
        ("host", "LowCardinality(String)"),
        ("cluster", "LowCardinality(String)"),
        ("file", "String"),
        ("message", "String"),
    ];
    let traces = [
        ("timestamp", "DateTime64(9, 'Asia/Shanghai')"),
        ("trace_id", "String"),
        ("span_id", "String"),
        ("parent_span_id", "String"),
        ("trace_state", "String"),
        ("span_name", "LowCardinality(String)"),
        ("span_kind", "LowCardinality(String)"),
        ("service_name", "LowCardinality(String)"),
        ("duration_ns", "UInt64"),
        ("status_code", "LowCardinality(String)"),
        ("status_message", "String"),
        ("scope_name", "LowCardinality(String)"),
        ("scope_version", "LowCardinality(String)"),
        ("resource_attributes", "JSON"),
        ("span_attributes", "JSON"),
        ("events.timestamp", "Array(DateTime64(9, 'Asia/Shanghai'))"),
        ("events.name", "Array(LowCardinality(String))"),
        ("events.attributes", "Array(JSON)"),
        ("links.trace_id", "Array(String)"),
        ("links.span_id", "Array(String)"),
        ("links.trace_state", "Array(String)"),
        ("links.attributes", "Array(JSON)"),
        ("cluster", "LowCardinality(String)"),
    ];
    let mut out = String::new();
    for (name, ty) in logs {
        out.push_str(&format!(r#"{{"table":"app_log","name":"{name}","type":"{ty}"}}"#));
        out.push('\n');
    }
    for (name, ty) in traces {
        out.push_str(&format!(r#"{{"table":"otel_trace","name":"{name}","type":"{ty}"}}"#));
        out.push('\n');
    }
    out
}

pub fn version_fixture() -> &'static str {
    "{\"version\":\"24.8.1.1\",\"timezone\":\"Asia/Shanghai\"}\n"
}

/// 起一个指向假库的 opdash app，表结构已经回放好（前两个请求）。
pub async fn app_with_schema(fake: &FakeClickhouse, extra_args: &[&str]) -> axum::Router {
    fake.respond(columns_fixture()).respond(version_fixture());
    app(fake, extra_args).await
}

pub async fn app(fake: &FakeClickhouse, extra_args: &[&str]) -> axum::Router {
    app_at(fake.endpoint(), extra_args).await
}

/// 指向任意地址（比如一个没人听的端口）的 app。
pub async fn app_at(endpoint: &str, extra_args: &[&str]) -> axum::Router {
    use clap::Parser;
    use opdash::api::{self, AppState};
    use opdash::clickhouse::{Client, ClientOptions};
    use opdash::config::Config;
    use opdash::schema::SchemaCache;

    let mut args = vec!["opdash", "--clickhouse-url", endpoint];
    args.extend_from_slice(extra_args);
    let config = Config::parse_from(args);
    let client = Client::new(ClientOptions {
        endpoint: config.clickhouse_url.clone(),
        user: config.clickhouse_user.clone(),
        password: config.clickhouse_password.clone(),
        timeout: config.query_timeout,
        max_read_bytes: config.max_read_bytes,
        max_read_rows: config.max_read_rows,
        max_concurrent: config.max_concurrent_queries,
    })
    .unwrap();
    let schema = Arc::new(SchemaCache::new(
        client.clone(),
        &config.database,
        &config.log_table,
        &config.trace_table,
    ));
    let auth = opdash::auth::Auth::from_config(&config);
    let state = AppState::new(config, client, schema);
    api::app(state, auth)
}

/// 发一个 GET，拿回 (状态码, 响应体 JSON)。
pub async fn get_json(app: &axum::Router, uri: &str) -> (u16, serde_json::Value) {
    get_json_with(app, uri, &[]).await
}

pub async fn get_json_with(
    app: &axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> (u16, serde_json::Value) {
    let (status, body) = get_raw(app, uri, headers).await;
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

pub async fn get_raw(app: &axum::Router, uri: &str, headers: &[(&str, &str)]) -> (u16, Vec<u8>) {
    let (status, _, body) = get_full(app, uri, headers).await;
    (status, body)
}

/// 同上，连响应头一起要（认证测试要看 Location / Set-Cookie）。头名小写，同名的各占一项。
pub async fn get_full(
    app: &axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let mut req = axum::http::Request::builder().uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let response = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    let body = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, headers, body)
}

pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// 一条 SSE 事件。注释（keep-alive 的 `:`）在 [`SseStream::next_event`] 里就跳过了。
#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
    pub id: Option<String>,
}

impl SseEvent {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.data).unwrap_or(serde_json::Value::Null)
    }
}

/// 边读边解析的 SSE 连接。跟随是长连接，不能像别的接口那样 `collect()` 等它结束。
pub struct SseStream {
    body: axum::body::Body,
    buf: String,
}

impl SseStream {
    /// 下一条事件；超时或流结束返回 None。
    pub async fn next_event(&mut self, timeout: std::time::Duration) -> Option<SseEvent> {
        use http_body_util::BodyExt;
        loop {
            if let Some(idx) = self.buf.find("\n\n") {
                let raw: String = self.buf.drain(..idx + 2).collect();
                if let Some(event) = parse_sse(&raw) {
                    return Some(event);
                }
                continue;
            }
            let frame = tokio::time::timeout(timeout, self.body.frame()).await.ok()??.ok()?;
            if let Ok(data) = frame.into_data() {
                self.buf.push_str(&String::from_utf8_lossy(&data));
            }
        }
    }
}

fn parse_sse(raw: &str) -> Option<SseEvent> {
    let mut event = String::from("message");
    let mut data: Vec<&str> = Vec::new();
    let mut id = None;
    for line in raw.trim_end().lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = v.trim().to_owned();
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.strip_prefix(' ').unwrap_or(v));
        } else if let Some(v) = line.strip_prefix("id:") {
            id = Some(v.trim().to_owned());
        }
    }
    // 只有注释（keep-alive）的块跳过
    (!data.is_empty()).then(|| SseEvent { event, data: data.join("\n"), id })
}

/// 开一条 SSE 连接，拿到状态码、响应头和还在流着的 body。
pub async fn open_sse(
    app: &axum::Router,
    uri: &str,
    headers: &[(&str, &str)],
) -> (u16, Vec<(String, String)>, SseStream) {
    use axum::body::Body;
    use tower::ServiceExt;

    let mut req = axum::http::Request::builder().uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let response = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status().as_u16();
    let head = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned()))
        .collect();
    (status, head, SseStream { body: response.into_body(), buf: String::new() })
}
