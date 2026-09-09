//! opdash —— trace 与日志的查询页面，数据来自 logpipe（`app_log`）和 tracepipe（`otel_trace`）
//! 写进 ClickHouse 的两张表。
//!
//! ```text
//!   浏览器 ── /api/* ──▶ axum ── 参数化 SQL ──▶ ClickHouse HTTP
//!      ▲                 │
//!      └── /  静态 SPA ◀─┘ （rust-embed，ui/dist）
//! ```
//!
//! 只读：没有任何写库路径，每个请求都带 `readonly=2`。
//! 认证可选：Basic 一组密码，或 OIDC 跳 Keycloak 登录（`auth` 模块）。

pub mod api;
pub mod auth;
pub mod clickhouse;
pub mod config;
pub mod error;
pub mod query;
pub mod schema;
pub mod ui;
