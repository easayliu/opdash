//! goscan 的同步接口：手动拉一次账单。
//!
//! **这是 opdash 唯一一处会往外发「会改状态」的请求。** 别的地方全是对 ClickHouse 的只读查询
//! （每条都带 `readonly=2`），这里也没有破例——账单是 [goscan](../goscan) 去云厂商那儿拉了再写库的，
//! opdash 只是把「拉一次」这个动作转过去：`POST {goscan}/sync` 登记一个后台任务、拿到 task id，
//! 之后按 id 轮 `GET {goscan}/tasks/{id}` 看结果。goscan 那边的注释写得很清楚：
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

/// goscan 客户端。
#[derive(Clone)]
pub struct Goscan {
    http: reqwest::Client,
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

/// `GET /tasks/{id}` 中用得上的几个字段。goscan 返回的字段多于此处，其余不向前端透出。
#[derive(Debug, Deserialize)]
pub struct TaskRow {
    pub id: String,
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
}

impl Goscan {
    pub fn new(base: &str, timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| Error::internal(format!("build goscan client: {e}")))?;
        Ok(Self { http, base: base.trim_end_matches('/').to_owned() })
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
            return Err(self.rejected(status.as_u16(), &body));
        }
        serde_json::from_str::<SyncStarted>(&body).map_err(|e| Error::Goscan {
            status: 0,
            message: format!("无法解析 goscan 的响应（{e}）: {body}"),
        })
    }

    /// 查一个任务现在怎么样了。
    pub async fn task(&self, id: &str) -> Result<TaskRow> {
        let url = format!("{}/tasks/{}", self.base, urlencoding(id));
        let resp = self.http.get(&url).send().await.map_err(|e| self.down(e))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(self.rejected(status.as_u16(), &body));
        }
        serde_json::from_str::<TaskRow>(&body).map_err(|e| Error::Goscan {
            status: 0,
            message: format!("无法解析 goscan 返回的任务（{e}）: {body}"),
        })
    }

    /// 连不上 / 超时。
    fn down(&self, e: reqwest::Error) -> Error {
        Error::Goscan {
            status: 0, message: format!("无法连接账单同步服务（{}）: {e}", self.base)
        }
    }

    /// goscan 返回非 2xx。其错误体形如 `{"error":true,"message":"..."}`，从中取出该说明——
    /// 例如 409「已有同步任务在执行」，对点击按钮的人是有效信息。
    fn rejected(&self, status: u16, body: &str) -> Error {
        let message = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_owned))
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| body.lines().next().unwrap_or("").trim().to_owned());
        let message = match status {
            409 => format!("已有同步任务正在执行，请待其完成后再试（{message}）"),
            429 => format!("同步任务已达并发上限，请稍后再试（{message}）"),
            404 => format!("未找到该同步任务（goscan 可能已重启）：{message}"),
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
        let err = g.rejected(409, r#"{"error":true,"message":"task already running"}"#);
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);
        assert!(err.to_string().contains("已有同步任务正在执行"), "{err}");
        assert_eq!(err.kind(), "busy");
        // 不是 JSON 的错误体也要能读
        let err = g.rejected(500, "boom\nstack...");
        assert!(err.to_string().contains("boom"), "{err}");
        assert_eq!(err.status(), axum::http::StatusCode::BAD_GATEWAY);
    }
}
