//! goscan 的同步接口：手动拉一次账单。
//!
//! **这是 opdash 唯一一处会往外发「会改状态」的请求。** 别的地方全是对 ClickHouse 的只读查询
//! （每条都带 `readonly=2`），这里也没有破例——账单是 [goscan](../goscan) 去云厂商那儿拉了再写库的，
//! opdash 只是把「拉一次」这个动作转过去：`POST {goscan}/sync` 登记一个后台任务、拿到 task id，
//! 之后订阅 `GET {goscan}/tasks/{id}/events`（SSE，goscan v0.5 起有）看进度，老版本退回轮询
//! `GET {goscan}/tasks/{id}`；`DELETE {goscan}/tasks/{id}` 可以请它在当前这一趟写完后停下。
//! 对接口径以 goscan README 的「手动同步（给 opdash 对接）」一节为准。goscan 那边的注释写得很清楚：
//! 「ExecuteTask only registers the task and returns; the sync itself runs in the background」——
//! 因此触发调用很快返回，账单则要稍后才会入库。
//!
//! 为什么要经 opdash 转一手：goscan 的 HTTP 接口**没有认证**（集群内服务，`pkg/server/server.go`
//! 里只有 RequestID / 日志 / recovery / CORS 几个中间件），而 opdash 有登录。让页面直接去调 goscan
//! 等于将其暴露给浏览器，且跨域也无法通过。
//!
//! 未配置 `--goscan-url` 时该功能整体不存在：接口返回 400 并说明原因，页面上也不显示按钮。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// 事件流闲着多久没收到任何字节就算断了。goscan 闲时每 15 秒推一行 `: ping`，
/// 留出三倍的余量。
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(45);

/// goscan 客户端。
#[derive(Clone)]
pub struct Goscan {
    http: reqwest::Client,
    /// 订阅事件流用的另一个客户端：`http` 带着整请求超时（默认 10 秒），一条同步要推几十分钟，
    /// 会被它半路掐断。这个只限连接和两次读之间的间隔
    stream: reqwest::Client,
    /// `http://goscan.logging.svc.cluster.local:8080`，末尾没有斜杠
    base: String,
}

/// 触发一次同步要告诉 goscan 的东西。字段名和 goscan `TriggerSync` 收的 JSON 一致。
#[derive(Debug, Serialize)]
pub struct SyncRequest {
    pub provider: String,
    /// `standard`（严格按账期逐个拉取）/ `sync-optimal`（比对数据量，只补缺失的部分）。
    /// **不能为空**：goscan 的 `ValidateSyncConfig` 会直接拒绝
    pub sync_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub granularity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_period: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_period: Option<String>,
    /// 已有数据也重新拉
    pub force_update: bool,
}

/// `POST /sync` 的回应：任务已经登记，真正的拉取在后台。
#[derive(Debug, Deserialize, Serialize)]
pub struct SyncStarted {
    pub task_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub provider: String,
}

/// 跑到哪了。goscan 按**趟**上报：一个账期一种粒度算一趟，没跑起来之前没有这一段。
#[derive(Debug, Deserialize)]
pub struct ProgressRow {
    /// 正在拉的那个账期
    #[serde(default)]
    pub period: String,
    /// 这一趟写哪张表：`monthly` / `daily`。火山不分粒度，老版本 goscan 也不报，都是空串
    #[serde(default)]
    pub granularity: String,
    /// 已经拉完几趟
    #[serde(default)]
    pub done: i64,
    /// 一共几趟
    #[serde(default)]
    pub total: i64,
    /// 这一趟已经写入的行数。一趟可能要跑好几分钟，靠它看出还在动（goscan v0.5 起有）
    #[serde(default)]
    pub records: i64,
    /// 这一趟接口报的总行数；按天拉整月时事先不知道，goscan 不报，这里是 0
    #[serde(default)]
    pub records_total: i64,
}

/// 任务是按什么参数发起的。页面接上一个已经在跑的任务（比如 cron 起的那个）时，据此说明它在拉哪几个月。
#[derive(Debug, Default, Deserialize)]
pub struct TaskConfigRow {
    #[serde(default)]
    pub start_period: String,
    #[serde(default)]
    pub end_period: String,
    #[serde(default)]
    pub bill_period: String,
    #[serde(default)]
    pub granularity: String,
}

/// `GET /tasks/{id}` 中用得上的几个字段。goscan 返回的字段多于此处，其余不向前端透出。
#[derive(Debug, Deserialize)]
pub struct TaskRow {
    pub id: String,
    /// `sync` / `notification`。任务列表里还有发企微日报的任务，找「正在进行的同步」时要排除
    #[serde(default, rename = "type")]
    pub kind: String,
    /// `pending` / `running` / `completed` / `failed` / `cancelled`
    pub status: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub end_time: Option<String>,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub result: Option<TaskResultRow>,
    /// 老版本 goscan 不报进度，这里就是 `None`，页面退回不确定进度条
    #[serde(default)]
    pub progress: Option<ProgressRow>,
    #[serde(default)]
    pub config: Option<TaskConfigRow>,
    /// 有人请它停下了。它会把手上这一趟写完再停，这期间 `status` 仍是 `running`
    #[serde(default)]
    pub cancel_requested: bool,
}

impl TaskRow {
    /// 已经结束了（不论成败）。
    pub fn finished(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed" | "cancelled")
    }
}

/// `GET /tasks` 的回应。
#[derive(Debug, Deserialize)]
struct TaskList {
    #[serde(default)]
    tasks: Vec<TaskRow>,
}

/// 调的是哪个接口。同一个状态码在不同接口上意思不同：`POST /sync` 回 409 是「这朵云已经有同步在跑」，
/// `DELETE /tasks/{id}` 回 409 是「任务已经结束，或者不是同步任务」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Trigger,
    Task,
    Cancel,
}

#[derive(Debug, Deserialize)]
pub struct TaskResultRow {
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub records_processed: i64,
    #[serde(default)]
    pub records_fetched: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub error: String,
    /// 被停下的同步没跑的那几趟，如 `2026-04 daily`。这些账期的数据原样没动
    #[serde(default)]
    pub not_run: Vec<String>,
}

impl Goscan {
    pub fn new(base: &str, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| Error::internal(format!("build goscan client: {e}")))?;
        let stream = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(STREAM_READ_TIMEOUT)
            .build()
            .map_err(|e| Error::internal(format!("build goscan stream client: {e}")))?;
        Ok(Self { http, stream, base: base.trim_end_matches('/').to_owned() })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// 触发一次同步，拿回 task id。
    pub async fn trigger(&self, req: &SyncRequest) -> Result<SyncStarted> {
        let url = format!("{}/sync", self.base);
        // 手写 JSON 体而不是 `.json()`：那要给 reqwest 开 `json` feature，而序列化本来就是
        // serde_json 的活，这里只是少一个 feature 开关
        let payload = serde_json::to_string(req).map_err(Error::internal)?;
        let resp = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|e| self.down(e))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(self.rejected(Op::Trigger, status.as_u16(), &body));
        }
        serde_json::from_str::<SyncStarted>(&body).map_err(|e| Error::Goscan {
            status: 0,
            message: format!("无法解析 goscan 的响应（{e}）：{body}"),
        })
    }

    /// 请一个同步停下。goscan 立刻回 202，但任务会把手上这一趟写完才停——一趟拉之前会先清空
    /// 那个账期，半路掐断会留下「清空了、只写了一半」的账期，所以它不提供立即中断。
    pub async fn cancel(&self, id: &str) -> Result<SyncStarted> {
        let url = format!("{}/tasks/{}", self.base, urlencoding(id));
        let resp = self.http.delete(&url).send().await.map_err(|e| self.down(e))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(self.rejected(Op::Cancel, status.as_u16(), &body));
        }
        serde_json::from_str::<SyncStarted>(&body).map_err(|e| Error::Goscan {
            status: 0,
            message: format!("无法解析 goscan 的响应（{e}）：{body}"),
        })
    }

    /// 所有任务：进行中的，加上最近结束的若干个。
    pub async fn tasks(&self) -> Result<Vec<TaskRow>> {
        let url = format!("{}/tasks", self.base);
        let resp = self.http.get(&url).send().await.map_err(|e| self.down(e))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(self.rejected(Op::Task, status.as_u16(), &body));
        }
        serde_json::from_str::<TaskList>(&body).map(|l| l.tasks).map_err(|e| Error::Goscan {
            status: 0,
            message: format!("无法解析 goscan 返回的任务列表（{e}）：{body}"),
        })
    }

    /// 订阅一个任务的事件流（SSE）。回来的是还没读的响应，调用方边读边转。
    ///
    /// 老版本 goscan 没有这个接口，gin 回一个纯文本的 404；任务查不到时回的是 JSON 的 404。
    /// 两种都当 404 报上去，页面据此退回轮询——轮询那条路会把「任务查不到」说清楚。
    pub async fn events(&self, id: &str) -> Result<reqwest::Response> {
        let url = format!("{}/tasks/{}/events", self.base, urlencoding(id));
        let resp = self
            .stream
            .get(&url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|e| self.down(e))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(self.rejected(Op::Task, status.as_u16(), &body));
        }
        Ok(resp)
    }

    /// 查一个任务现在怎么样了。
    pub async fn task(&self, id: &str) -> Result<TaskRow> {
        let url = format!("{}/tasks/{}", self.base, urlencoding(id));
        let resp = self.http.get(&url).send().await.map_err(|e| self.down(e))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(self.rejected(Op::Task, status.as_u16(), &body));
        }
        serde_json::from_str::<TaskRow>(&body).map_err(|e| Error::Goscan {
            status: 0,
            message: format!("无法解析 goscan 返回的任务（{e}）：{body}"),
        })
    }

    /// 连不上 / 超时。
    fn down(&self, e: reqwest::Error) -> Error {
        Error::Goscan {
            status: 0, message: format!("无法连接账单同步服务（{}）：{e}", self.base)
        }
    }

    /// goscan 返回非 2xx。其错误体形如 `{"error":true,"message":"..."}`，从中取出该说明——
    /// 例如 409「已有同步任务在执行」，对点击按钮的人是有效信息。
    fn rejected(&self, op: Op, status: u16, body: &str) -> Error {
        let message = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_owned))
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| body.lines().next().unwrap_or("").trim().to_owned());
        let message = match (op, status) {
            (Op::Trigger, 409) => format!("已有同步任务正在执行，请待其完成后再试（{message}）"),
            (Op::Cancel, 409) => format!("任务已经结束，或不是同步任务，无须停止（{message}）"),
            (_, 429) => format!("同步任务已达并发上限，请稍后再试（{message}）"),
            (_, 404) => format!("未找到该同步任务（goscan 可能已重启）：{message}"),
            _ => format!("账单同步服务拒绝了本次请求（HTTP {status}）：{message}"),
        };
        Error::Goscan { status, message }
    }
}

/// task id 是 goscan 生成的 UUID，这里只防拼 URL 时的意外：非 `[A-Za-z0-9_-]` 一律挡掉，
/// 不做百分号编码——若出现其他字符即说明调用方传错，直接报错胜过拼出一个异常的 URL。
fn urlencoding(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_is_sanitised() {
        assert_eq!(urlencoding("3f2a-4b1c_9"), "3f2a-4b1c_9");
        assert_eq!(urlencoding("../../system/status"), "systemstatus");
    }

    #[test]
    fn rejection_messages_explain_the_status() {
        let g = Goscan::new("http://goscan:8080", Duration::from_secs(1)).unwrap();
        let err =
            g.rejected(Op::Trigger, 409, r#"{"error":true,"message":"task already running"}"#);
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);
        assert!(err.to_string().contains("已有同步任务正在执行"), "{err}");
        assert_eq!(err.kind(), "busy");
        // 不是 JSON 的错误体也要能读
        let err = g.rejected(Op::Task, 500, "boom\nstack...");
        assert!(err.to_string().contains("boom"), "{err}");
        assert_eq!(err.status(), axum::http::StatusCode::BAD_GATEWAY);
        // 同是 409，停止接口的意思是「任务已经结束」，不是「已有同步在跑」
        let err = g.rejected(Op::Cancel, 409, r#"{"error":true,"message":"task is not running"}"#);
        assert!(err.to_string().contains("任务已经结束"), "{err}");
        let err = g.rejected(Op::Task, 404, r#"{"error":true,"message":"Task not found"}"#);
        assert_eq!(err.status(), axum::http::StatusCode::NOT_FOUND);
        assert_eq!(err.kind(), "not_found");
    }
}
