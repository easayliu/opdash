//! MCP（Model Context Protocol）服务端：把 opdash 的查询能力交给 AI 助手直接用。
//!
//! 挂在 `POST /mcp`，Streamable HTTP 传输、**无状态**：不发 `Mcp-Session-Id`，不开服务端到客户端的
//! SSE 流，每个 JSON-RPC 请求独立处理、一次 POST 回一个 JSON。Claude Code / Codex / Cursor
//! 这类客户端填一个地址就能接：
//!
//! ```text
//! claude mcp remove opdash 2>/dev/null                    # 装过就先删掉，换成新 key
//! claude mcp add --transport http opdash https://opdash.example.com/mcp \
//!   --header "Authorization: Bearer opdash_…"             # 页面右上角自己生成的 API key
//! ```
//!
//! 工具不直接碰查询层：每个工具把参数翻译成 `/api/*` 的查询串，在**进程内**走一遍同一个 axum
//! Router（[`crate::api::api_router`]），拿到 JSON 之后再整理成给模型看的形状（时间戳转成本地时间、
//! 长 message 截断、去掉 stats / sparkline 这类只有页面才要的字段）。这样参数校验、错误提示、
//! 读量护栏和页面完全是同一套，加一个工具就是加一段翻译，见 [`tools`]。
//!
//! 认证和页面一样走 [`crate::auth`]：登录用户在页面右上角给自己生成一把 API key
//! （`POST /api/auth/keys`），MCP 客户端带 `Authorization: Bearer <key>`。MCP 客户端不会跳浏览器
//! 登录，所以不能直接用 OIDC 会话；key 就是把会话「拿出来」的办法。

pub mod tools;

use std::sync::Arc;

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{Value, json};

use crate::api::AppState;

/// 我们这边实现的协议版本。客户端要的版本在 [`SUPPORTED_VERSIONS`] 里就照它的回，否则回这个。
pub const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

// JSON-RPC 2.0 的标准错误码
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// 工具往进程内 API 发请求时响应体的上限。日志一页最多 `--max-rows` 行、每行 message 截过，
/// 正常远不到；这是防御性的。
const MAX_API_BODY: usize = 64 << 20;

/// MCP 处理器的状态：应用状态 + 一份进程内的 API 路由。
pub struct Mcp {
    pub state: AppState,
    /// 工具通过它「请求自己」。Router 是 `Clone` 的，每次调用 clone 一份走 `oneshot`。
    api: Router,
}

impl Mcp {
    /// 进程内 GET `/api/...`。非 2xx 时把 API 的 `error` 文本原样交回去——那句话本来就是写给人看的
    /// （「不认识的筛选列 x；可用的筛选列: …」），模型照着改参数就行。
    pub async fn get(&self, path: &str, query: &str) -> Result<Value, String> {
        use tower::ServiceExt;
        let uri = if query.is_empty() { path.to_owned() } else { format!("{path}?{query}") };
        let req = Request::builder()
            .uri(&uri)
            .body(Body::empty())
            .map_err(|e| format!("内部请求无效: {e}"))?;
        let resp = match self.api.clone().oneshot(req).await {
            Ok(r) => r,
            Err(never) => match never {},
        };
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), MAX_API_BODY)
            .await
            .map_err(|e| format!("读取内部响应失败: {e}"))?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("内部响应不是 JSON（HTTP {}）: {e}", status.as_u16()))?;
        if status.is_success() {
            return Ok(value);
        }
        let msg = value["error"].as_str().unwrap_or("查询失败").to_owned();
        Err(match value["kind"].as_str() {
            Some("too_heavy") | Some("timeout") => {
                format!("{msg}。建议：缩小时间范围（先看几分钟）或加上服务 / 级别等筛选条件再试")
            }
            _ => msg,
        })
    }

    pub fn tz(&self) -> Tz {
        crate::query::parse_tz(&self.state.config.timezone).unwrap_or(chrono_tz::UTC)
    }
}

/// `/mcp` 路由。挂进受认证保护的那一半，见 [`crate::api::app`]。
pub fn router(state: AppState) -> Router {
    let mcp = Arc::new(Mcp { api: crate::api::api_router(state.clone()), state });
    Router::new().route("/mcp", post(handle).get(no_stream).delete(no_stream)).with_state(mcp)
}

/// GET 是客户端想开一条服务端推送的 SSE 流，DELETE 是结束会话；无状态实现两个都没有。
/// 协议规定这种情况回 405。
async fn no_stream() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
        "opdash 的 MCP 端点是无状态的：只接受 POST 的 JSON-RPC 请求，不提供 SSE 流和会话",
    )
        .into_response()
}

async fn handle(State(mcp): State<Arc<Mcp>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Err(resp) = check_origin(&headers) {
        return resp;
    }
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return rpc_response(
                StatusCode::BAD_REQUEST,
                error_message(Value::Null, PARSE_ERROR, format!("请求体不是合法 JSON: {e}")),
            );
        }
    };
    match parsed {
        // 2025-03-26 之前的协议允许一次 POST 一批消息；逐个处理，只回有回应的那些
        Value::Array(items) => {
            if items.is_empty() {
                return rpc_response(
                    StatusCode::BAD_REQUEST,
                    error_message(Value::Null, INVALID_REQUEST, "空的批量请求"),
                );
            }
            // 一批里的请求互不相干，并发跑（并发上限由 ClickHouse 客户端自己的信号量兜着）；
            // join_all 保序，回应的顺序还是请求的顺序
            let out: Vec<Value> =
                futures_util::future::join_all(items.into_iter().map(|item| dispatch(&mcp, item)))
                    .await
                    .into_iter()
                    .flatten()
                    .collect();
            if out.is_empty() {
                accepted()
            } else {
                rpc_response(StatusCode::OK, Value::Array(out))
            }
        }
        other => match dispatch(&mcp, other).await {
            Some(r) => rpc_response(StatusCode::OK, r),
            None => accepted(),
        },
    }
}

/// 协议要求服务端校验 `Origin`，防 DNS rebinding（浏览器里的页面借着本机地址打过来）。
/// 命令行客户端不带 Origin，直接放行；带了就要和 Host 是同一个主机。
#[allow(clippy::result_large_err)] // 只在拒绝时才有 Response，调用处直接 return
fn check_origin(headers: &HeaderMap) -> Result<(), Response> {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .unwrap_or("");
    let origin_host = origin.trim().split("://").nth(1).unwrap_or(origin).trim_end_matches('/');
    if origin == "null" || origin_host.is_empty() || !origin_host.eq_ignore_ascii_case(host) {
        tracing::debug!(origin, host, "MCP 请求的 Origin 和 Host 不一致，拒绝");
        return Err((StatusCode::FORBIDDEN, "Origin 不被允许").into_response());
    }
    Ok(())
}

/// 通知（没有 id 的消息）和客户端发来的响应不需要回应：202、空 body。
fn accepted() -> Response {
    StatusCode::ACCEPTED.into_response()
}

fn rpc_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [("content-type", "application/json"), ("mcp-protocol-version", PROTOCOL_VERSION)],
        body.to_string(),
    )
        .into_response()
}

fn error_message(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

fn result_message(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

/// 一条消息 → 可能的一条回应。请求（有 `method` 有 `id`）有回应；通知（有 `method` 没 `id`）和
/// 客户端的响应（有 `result` / `error`）没有。
async fn dispatch(mcp: &Mcp, msg: Value) -> Option<Value> {
    let Some(obj) = msg.as_object() else {
        return Some(error_message(Value::Null, INVALID_REQUEST, "消息应是 JSON 对象"));
    };
    let method = obj.get("method").and_then(Value::as_str);
    let id = obj.get("id").cloned();
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        (None, _) => {
            // 客户端回我们的响应（我们从不主动请求，所以不会有）；别的形状就是无效请求
            if obj.contains_key("result") || obj.contains_key("error") {
                return None;
            }
            Some(error_message(Value::Null, INVALID_REQUEST, "消息缺少 method"))
        }
        (Some(method), None) => {
            tracing::debug!(method, "MCP 通知");
            None
        }
        (Some(method), Some(id)) => {
            let started = std::time::Instant::now();
            let out = call(mcp, method, &params).await;
            let elapsed_ms = started.elapsed().as_millis();
            Some(match out {
                Ok(result) => {
                    tracing::debug!(method, elapsed_ms, "MCP 请求完成");
                    result_message(id, result)
                }
                Err(e) => {
                    tracing::debug!(method, elapsed_ms, code = e.code, error = %e.message, "MCP 请求出错");
                    error_message(id, e.code, e.message)
                }
            })
        }
    }
}

async fn call(mcp: &Mcp, method: &str, params: &Value) -> Result<Value, RpcError> {
    match method {
        "initialize" => Ok(initialize(mcp, params).await),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::list(metrics_enabled(mcp).await) })),
        "tools/call" => {
            let name = params["name"]
                .as_str()
                .ok_or_else(|| RpcError::new(INVALID_PARAMS, "tools/call 缺少 name"))?;
            let empty = serde_json::Map::new();
            let arguments = match &params["arguments"] {
                Value::Null => &empty,
                Value::Object(m) => m,
                _ => return Err(RpcError::new(INVALID_PARAMS, "arguments 应是 JSON 对象")),
            };
            let started = std::time::Instant::now();
            let out = tools::call(mcp, name, arguments).await;
            log_tool_call(name, arguments, &out, started.elapsed().as_millis());
            match out {
                Ok(tools::ToolOutput { text, .. }) => Ok(json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": false,
                })),
                // 参数不对、查询失败：作为工具结果回去（isError），模型看得到原因、能自己改参数重试
                Err(tools::ToolError::Failed(text)) => Ok(json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": true,
                })),
                Err(tools::ToolError::Unknown) => Err(RpcError::new(
                    INVALID_PARAMS,
                    format!(
                        "没有叫 {name:?} 的工具；可用: {}",
                        tools::list(metrics_enabled(mcp).await)
                            .iter()
                            .filter_map(|t| t["name"].as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )),
                Err(tools::ToolError::Internal(msg)) => Err(RpcError::new(INTERNAL_ERROR, msg)),
            }
        }
        // 没声明 resources / prompts / logging 能力，客户端问了就按协议回「没有这个方法」
        other => Err(RpcError::new(METHOD_NOT_FOUND, format!("不支持的方法: {other}"))),
    }
}

/// 工具调用记一条 info 日志。`dispatch` 那条只有 `method`，而所有调用的 method 都是 `tools/call`
/// ——看不出调了哪个工具、传了什么、回来是不是空的，「模型是不是老走错工具」这种问题就只能
/// 靠感觉。这条是拿来数的：tool + 参数 + 空不空 + 耗时。
fn log_tool_call(
    name: &str,
    arguments: &serde_json::Map<String, Value>,
    out: &Result<tools::ToolOutput, tools::ToolError>,
    elapsed_ms: u128,
) {
    // 参数原样记一份（截断）：排查「问指标却调了 service_overview」要的就是它
    let args = truncate_for_log(&Value::Object(arguments.clone()).to_string());
    match out {
        Ok(o) => tracing::info!(
            tool = name,
            %args,
            elapsed_ms,
            bytes = o.text.len(),
            empty = o.empty,
            "MCP 工具调用"
        ),
        Err(tools::ToolError::Failed(msg)) => {
            tracing::info!(tool = name, %args, elapsed_ms, error = %truncate_for_log(msg), "MCP 工具失败")
        }
        Err(tools::ToolError::Unknown) => {
            tracing::info!(tool = name, %args, elapsed_ms, error = "没有这个工具", "MCP 工具失败")
        }
        Err(tools::ToolError::Internal(msg)) => {
            tracing::warn!(tool = name, %args, elapsed_ms, error = %msg, "MCP 工具内部错误")
        }
    }
}

/// 日志里一行别太长：关键字搜索的 q 可能很长，取前面够认出是什么就行。
fn truncate_for_log(s: &str) -> String {
    const MAX: usize = 300;
    if s.chars().count() <= MAX {
        return s.to_owned();
    }
    s.chars().take(MAX).collect::<String>() + "…"
}

/// 这个部署有没有指标表。表结构是带缓存的（`initialize` 通常已经读过一遍），这里基本不会真去
/// 连库；读不到就当有——真没有的话工具调用时会报「指标页未启用」，总比因为库抖了一下就把
/// 半个工具目录藏起来好。
async fn metrics_enabled(mcp: &Mcp) -> bool {
    match mcp.state.schema.get().await {
        Ok(schema) => schema.metrics.is_some(),
        Err(_) => true,
    }
}

async fn initialize(mcp: &Mcp, params: &Value) -> Value {
    let wanted = params["protocolVersion"].as_str().unwrap_or(PROTOCOL_VERSION);
    let version = if SUPPORTED_VERSIONS.contains(&wanted) { wanted } else { PROTOCOL_VERSION };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "opdash",
            "title": "opdash · 日志 / 链路 / 指标查询",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": instructions(mcp).await,
    })
}

/// 给模型的使用说明。表结构读得到就把可筛的维度列名也写进去，省一次 `get_meta`；
/// 库暂时连不上就不写（握手不能因为库挂了而失败，工具调用时再报）。
async fn instructions(mcp: &Mcp) -> String {
    let cfg = &mcp.state.config;
    let tz = mcp.tz();
    let mut s = format!(
        "opdash：线上日志（{db}.{logs}）、链路 span（{db}.{traces}）和指标（{db}.{metrics}）的只读查询，给业务排障用。\n\
         \n\
         排障套路：\n\
         1. 先 service_overview 看「谁不对」（错误率 / P95 和对比时段比，health 字段是结论），再 service_operations 看是哪个接口变了；\n\
         2. error_groups 看在报什么错；拿 sample_trace 用 get_trace 看整条链路，get_span 看某个 span 的属性和异常堆栈；\n\
         3. search_logs 按 trace_id / 关键字翻日志，log_histogram 看错误是从什么时候开始的，log_context 看某条日志前后几行；\n\
         4. 指标：query_metric 看曲线，metric_exemplars 把尖峰直接换成 trace_id（省得拿时间去撞），metric_events 看这个时刻是不是有重启 / 发布。\n\
         \n\
         要按属性筛（search_traces 的 attr、query_metric 的 by / attr）之前先 list_attrs 列一遍属性名，别猜。\n\
         \n\
         【术语：「指标」在 opdash 里是两回事，别走错工具】\n\
         * 服务指标（RED）= 请求量 / 错误率 / P50 / P95 / P99：**从 span 表现算的**，工具是 service_overview /          service_operations / service_timeseries。问「哪个服务不对」「这个接口是不是变慢了」「错误率涨了没」走这边。\n\
         * 上报指标 = metricpipe 采上来的 OTel 指标，名字带点（jvm.gc.duration、jvm.memory.used、process.cpu.time、         http.server.request.duration）：**在指标表里**，工具是 list_metrics / query_metric / metric_exemplars / metric_events。         问 JVM、GC、堆 / 元空间、CPU、线程、连接池、消息积压、缓存命中率这类**进程和资源**的情况走这边。\n\
         * 分不清就先 list_metrics(match=\"关键字\")：只扫最近 6 小时、很便宜。名字命中了就用指标工具，         一个都没命中才回到 service_* 或直接问清楚。用户说「指标」但给的是服务名 + 慢 / 报错 / 量，那是 RED。\n\
         \n\
         时间参数：from / to / at / time 接受 RFC3339（2026-09-18T10:00:00+08:00）、本地时间（2026-09-18 10:00:00，时区 {tz}）、\
         unix 秒或毫秒、相对写法（now-30m、-2h、now）。range 是时间跨度（15m / 1h / 24h / 7d）：不给 from 时 from = to - range，to 默认现在。\
         返回里的时间一律是 {tz} 的本地时间。\n\
         \n\
         时间范围越小越快：日志 message 的关键字是逐行扫的，先看几分钟再放宽；一次查不超过 {max_range}。\
         trace_id 是 32 位 hex，span_id 是 16 位 hex，按 id 查日志不需要时间范围（走索引），但知道大概时刻时带上 at / from / to 会快很多。",
        db = cfg.database,
        logs = cfg.log_table,
        traces = cfg.trace_table,
        metrics = cfg.metric_table,
        tz = cfg.timezone,
        max_range = fmt_duration(cfg.max_range.as_millis() as i64),
    );
    if let Ok(schema) = mcp.state.schema.get().await {
        let dims = |t: &crate::schema::Table, fixed: &[&str]| {
            t.extra_string_columns(fixed).iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        };
        let log_dims = dims(&schema.logs, crate::schema::LOG_FIXED_COLUMNS);
        let trace_dims = dims(&schema.traces, crate::schema::TRACE_FIXED_COLUMNS);
        s.push_str(&format!(
            "\n\n日志表可筛的维度列（search_logs / log_histogram / log_facets 的 filters）：{}。span 表额外可筛：{}。",
            if log_dims.is_empty() { "（无）".to_owned() } else { log_dims.join(", ") },
            if trace_dims.is_empty() { "（无）".to_owned() } else { trace_dims.join(", ") },
        ));
        if schema.metrics.is_none() {
            s.push_str(
                "\n指标表未启用，指标类工具（list_metrics / query_metric / metric_events）用不了。",
            );
        }
    }
    s.push_str(&format!("\n\n现在是 {}。", fmt_time(mcp.state.now_ms(), tz)));
    s
}

/// unix 毫秒 → `2026-09-18T10:25:03.123+08:00`。
pub fn fmt_time(ms: i64, tz: Tz) -> String {
    match Utc.timestamp_millis_opt(ms).single() {
        Some(t) => t.with_timezone(&tz).format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string(),
        None => ms.to_string(),
    }
}

/// 毫秒数 → `31d` / `6h` / `30s` / `1500ms`：能整除就用大单位，不像 humantime 那样把 31 天写成
/// `1month 13h 26m 24s`。
pub fn fmt_duration(ms: i64) -> String {
    const UNITS: &[(i64, &str)] =
        &[(86_400_000, "d"), (3_600_000, "h"), (60_000, "m"), (1000, "s")];
    for (unit, suffix) in UNITS {
        if ms != 0 && ms % unit == 0 {
            return format!("{}{suffix}", ms / unit);
        }
    }
    format!("{ms}ms")
}

/// 模型给的时间 → unix 毫秒。认这几种写法：
///
/// * 数字：unix 毫秒；小于 10^11 的按 unix 秒算（毫秒早就是 10^12 量级了）
/// * `now`、`now-30m`、`-2h`：相对现在
/// * RFC3339：`2026-09-18T10:00:00+08:00`、`2026-09-18T02:00:00Z`
/// * 没带时区的本地时间：`2026-09-18 10:00:00`、`2026-09-18T10:00`、`2026-09-18`，按 `tz` 解释
pub fn parse_time(v: &Value, now_ms: i64, tz: Tz) -> Result<i64, String> {
    if let Some(n) = v.as_f64() {
        return Ok(epoch_number(n));
    }
    let Some(raw) = v.as_str() else {
        return Err(format!("时间应是字符串或数字，不是 {v}"));
    };
    let s = raw.trim();
    if s.is_empty() {
        return Err("时间不能为空".to_owned());
    }
    if s.eq_ignore_ascii_case("now") {
        return Ok(now_ms);
    }
    if let Some(rel) = s.strip_prefix("now-").or_else(|| s.strip_prefix('-')) {
        let d = humantime::parse_duration(rel.trim())
            .map_err(|_| format!("相对时间写法应像 now-30m / -2h，不是 {raw:?}"))?;
        return Ok(now_ms - d.as_millis() as i64);
    }
    if let Ok(n) = s.parse::<f64>() {
        return Ok(epoch_number(n));
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.timestamp_millis());
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return local_to_ms(naive, tz);
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return local_to_ms(d.and_hms_opt(0, 0, 0).expect("00:00:00 总是合法的"), tz);
    }
    Err(format!(
        "看不懂的时间 {raw:?}；接受 RFC3339（2026-09-18T10:00:00+08:00）、本地时间（2026-09-18 10:00:00）、unix 秒 / 毫秒、now-30m 这类相对写法"
    ))
}

fn epoch_number(n: f64) -> i64 {
    if n.abs() < 1e11 { (n * 1000.0).round() as i64 } else { n.round() as i64 }
}

fn local_to_ms(naive: NaiveDateTime, tz: Tz) -> Result<i64, String> {
    tz.from_local_datetime(&naive)
        .earliest()
        .map(|t| t.timestamp_millis())
        .ok_or_else(|| format!("{naive} 在时区 {tz} 里不存在"))
}

/// `15m` / `1h` / `2d` → 毫秒。
pub fn parse_range(raw: &str) -> Result<i64, String> {
    let d = humantime::parse_duration(raw.trim())
        .map_err(|_| format!("range 写法应像 15m / 1h / 24h / 7d，不是 {raw:?}"))?;
    let ms = d.as_millis() as i64;
    if ms <= 0 {
        return Err("range 必须大于 0".to_owned());
    }
    Ok(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_767_196_800_000; // 2026-01-01 00:00 Asia/Shanghai

    fn sh() -> Tz {
        chrono_tz::Asia::Shanghai
    }

    #[test]
    fn parses_all_the_time_spellings() {
        let p = |v: Value| parse_time(&v, NOW, sh()).unwrap();
        assert_eq!(p(json!(NOW)), NOW);
        assert_eq!(p(json!(NOW / 1000)), NOW, "unix 秒也认");
        assert_eq!(p(json!(NOW.to_string())), NOW);
        assert_eq!(p(json!("now")), NOW);
        assert_eq!(p(json!("now-30m")), NOW - 30 * 60_000);
        assert_eq!(p(json!("-2h")), NOW - 2 * 3_600_000);
        assert_eq!(p(json!("2026-01-01T00:00:00+08:00")), NOW);
        assert_eq!(p(json!("2025-12-31T16:00:00Z")), NOW);
        assert_eq!(p(json!("2026-01-01 00:00:00")), NOW, "没带时区按配置的时区算");
        assert_eq!(p(json!("2026-01-01T00:00:00.500")), NOW + 500);
        assert_eq!(p(json!("2026-01-01 00:00")), NOW);
        assert_eq!(p(json!("2026-01-01")), NOW);
        assert!(parse_time(&json!("昨天"), NOW, sh()).is_err());
        assert!(parse_time(&json!(true), NOW, sh()).is_err());
    }

    #[test]
    fn formats_in_the_configured_zone() {
        assert_eq!(fmt_time(NOW + 123, sh()), "2026-01-01T00:00:00.123+08:00");
        assert_eq!(fmt_duration(31 * 86_400_000), "31d");
        assert_eq!(fmt_duration(90_000), "90s");
        assert_eq!(fmt_duration(1_500), "1500ms");
        assert_eq!(fmt_duration(0), "0ms");
        assert_eq!(parse_range("15m").unwrap(), 900_000);
        assert_eq!(parse_range("1h 30m").unwrap(), 5_400_000);
        assert!(parse_range("abc").is_err());
    }

    #[test]
    fn origin_must_match_host_when_present() {
        let mut h = HeaderMap::new();
        assert!(check_origin(&h).is_ok(), "命令行客户端不带 Origin");
        h.insert(header::HOST, "opdash.example.com".parse().unwrap());
        h.insert(header::ORIGIN, "https://opdash.example.com".parse().unwrap());
        assert!(check_origin(&h).is_ok());
        h.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(check_origin(&h).is_err());
        h.insert(header::ORIGIN, "null".parse().unwrap());
        assert!(check_origin(&h).is_err());
    }
}
