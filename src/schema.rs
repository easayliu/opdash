//! 表结构：启动时读 `system.columns`，之后定期刷新。
//!
//! 两张表的固定列是程序知道的；可选列（k8s 元数据 `namespace` / `pod` / `container` / `stream`，
//! 以及采集端配置里 `fields` 加的 `cluster` / `env` / `app`……）随部署不同而不同，只能从库里读。
//! 读到的列名同时也是**白名单**：前端传上来的筛选列名不在这里面的一律拒绝，SQL 里只拼白名单
//! 里的名字，这样动态列也不用走字符串拼接的险路。

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::clickhouse::{Client, Query};
use crate::error::{Error, Result};

/// 列类型的粗分类，前端据此决定筛选控件（字符串列可以下拉 / 等值筛，数字列只展示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    String,
    Int,
    Float,
    DateTime,
    /// ClickHouse 的 JSON 类型（tracepipe v0.2.0 起的属性列）
    Json,
    Map,
    Array,
    Other,
}

impl ColumnKind {
    /// `LowCardinality(Nullable(String))` → String；`Array(Map(...))` → Array。
    pub fn classify(ty: &str) -> Self {
        let mut t = ty.trim();
        loop {
            let unwrapped = ["LowCardinality(", "Nullable("]
                .iter()
                .find_map(|prefix| t.strip_prefix(prefix).and_then(|rest| rest.strip_suffix(')')));
            match unwrapped {
                Some(inner) => t = inner.trim(),
                None => break,
            }
        }
        if t == "String" || t.starts_with("FixedString") || t.starts_with("Enum") {
            ColumnKind::String
        } else if t.starts_with("Int") || t.starts_with("UInt") {
            ColumnKind::Int
        } else if t.starts_with("Float") || t.starts_with("Decimal") {
            ColumnKind::Float
        } else if t.starts_with("DateTime") || t.starts_with("Date") {
            ColumnKind::DateTime
        } else if t == "JSON" || t.starts_with("JSON(") {
            ColumnKind::Json
        } else if t.starts_with("Map(") {
            ColumnKind::Map
        } else if t.starts_with("Array(") {
            ColumnKind::Array
        } else {
            ColumnKind::Other
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub kind: ColumnKind,
}

#[derive(Debug, Clone, Serialize)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    pub fn has(&self, name: &str) -> bool {
        self.column(name).is_some()
    }

    /// 缺了哪些必需列。
    pub fn missing<'a>(&self, required: &[&'a str]) -> Vec<&'a str> {
        required.iter().copied().filter(|name| !self.has(name)).collect()
    }

    /// 不在 `fixed` 里的字符串列：可选的筛选维度。
    pub fn extra_string_columns(&self, fixed: &[&str]) -> Vec<&Column> {
        self.columns
            .iter()
            .filter(|c| c.kind == ColumnKind::String && !fixed.contains(&c.name.as_str()))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Schema {
    pub logs: Table,
    pub traces: Table,
    pub server_version: String,
    pub server_timezone: String,
    #[serde(skip)]
    pub loaded_at: Instant,
}

/// 带缓存的表结构。`get` 拿缓存，没有就现读；后台任务按周期刷新。
pub struct SchemaCache {
    client: Client,
    database: String,
    log_table: String,
    trace_table: String,
    current: RwLock<Option<Arc<Schema>>>,
    /// 上次读失败的时刻：连不上库时每个请求都去读一次没意义，隔几秒再试。
    last_failure: Mutex<Option<(Instant, String)>>,
}

/// 失败后多久内不再重试。
const RETRY_AFTER: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct ColumnRow {
    table: String,
    name: String,
    #[serde(rename = "type")]
    ty: String,
}

#[derive(Deserialize)]
struct ServerRow {
    version: String,
    timezone: String,
}

impl SchemaCache {
    pub fn new(client: Client, database: &str, log_table: &str, trace_table: &str) -> Self {
        Self {
            client,
            database: database.to_owned(),
            log_table: log_table.to_owned(),
            trace_table: trace_table.to_owned(),
            current: RwLock::new(None),
            last_failure: Mutex::new(None),
        }
    }

    /// 当前缓存（可能为空，比如启动时库还没起来）。
    pub fn current(&self) -> Option<Arc<Schema>> {
        self.current.read().clone()
    }

    /// 上次读取失败的原因，健康检查用。
    pub fn last_error(&self) -> Option<String> {
        self.last_failure.lock().as_ref().map(|(_, e)| e.clone())
    }

    /// 拿表结构：有缓存用缓存，否则读一次。
    pub async fn get(&self) -> Result<Arc<Schema>> {
        if let Some(schema) = self.current() {
            return Ok(schema);
        }
        if let Some((at, err)) = self.last_failure.lock().clone()
            && at.elapsed() < RETRY_AFTER
        {
            return Err(Error::Unavailable(format!("表结构尚未读到（{err}）")));
        }
        self.refresh().await
    }

    /// 从 `system.columns` 重新读。成功就替换缓存，失败保留旧的。
    pub async fn refresh(&self) -> Result<Arc<Schema>> {
        match self.load().await {
            Ok(schema) => {
                let schema = Arc::new(schema);
                *self.current.write() = Some(schema.clone());
                *self.last_failure.lock() = None;
                Ok(schema)
            }
            Err(e) => {
                *self.last_failure.lock() = Some((Instant::now(), e.to_string()));
                Err(e)
            }
        }
    }

    async fn load(&self) -> Result<Schema> {
        let columns = self
            .client
            .rows::<ColumnRow>(
                Query::new(
                    "SELECT table, name, type FROM system.columns \
                     WHERE database = {db:String} AND table IN ({logs:String}, {traces:String}) \
                     ORDER BY table, position",
                )
                .param("db", &self.database)
                .param("logs", &self.log_table)
                .param("traces", &self.trace_table),
            )
            .await?
            .rows;
        let server = self
            .client
            .rows::<ServerRow>(Query::new("SELECT version() AS version, timezone() AS timezone"))
            .await?
            .rows
            .into_iter()
            .next()
            .ok_or_else(|| Error::internal("version() 没有返回结果"))?;

        let table = |name: &str| -> Result<Table> {
            let cols: Vec<Column> = columns
                .iter()
                .filter(|c| c.table == name)
                .map(|c| Column {
                    name: c.name.clone(),
                    ty: c.ty.clone(),
                    kind: ColumnKind::classify(&c.ty),
                })
                .collect();
            if cols.is_empty() {
                return Err(Error::Internal(format!(
                    "ClickHouse 里没有表 {}.{name}（或者没有读 system.columns 的权限）",
                    self.database
                )));
            }
            Ok(Table { name: name.to_owned(), columns: cols })
        };
        let logs = table(&self.log_table)?;
        let traces = table(&self.trace_table)?;

        let missing_logs = logs.missing(LOG_FIXED_COLUMNS);
        if !missing_logs.is_empty() {
            return Err(Error::Internal(format!(
                "日志表 {}.{} 缺列: {}（这些是 logpipe 固定会写的列）",
                self.database,
                self.log_table,
                missing_logs.join(", ")
            )));
        }
        let missing_traces = traces.missing(TRACE_FIXED_COLUMNS);
        if !missing_traces.is_empty() {
            return Err(Error::Internal(format!(
                "span 表 {}.{} 缺列: {}（这些是 tracepipe 固定会写的列）",
                self.database,
                self.trace_table,
                missing_traces.join(", ")
            )));
        }
        // 属性列必须是 JSON 类型（tracepipe v0.2.0 起）。v0.1 的 Map 表查法完全不同，
        // 不做兼容：按 tracepipe README 重建表再来。
        for (name, want_array) in ATTRIBUTE_COLUMNS {
            let Some(col) = traces.column(name) else { continue };
            let ok = if *want_array {
                col.ty.replace(' ', "").strip_prefix("Array(JSON").is_some()
            } else {
                col.kind == ColumnKind::Json
            };
            if !ok {
                return Err(Error::Internal(format!(
                    "span 表 {}.{} 的 {name} 列是 {}，opdash 只支持 tracepipe v0.2.0 起的 JSON 属性列（ClickHouse 25.3+）；请按 tracepipe README 重建这张表",
                    self.database, self.trace_table, col.ty
                )));
            }
        }

        Ok(Schema {
            logs,
            traces,
            server_version: server.version,
            server_timezone: server.timezone,
            loaded_at: Instant::now(),
        })
    }

    /// 后台定期刷新。失败只记日志，缓存里的旧结构继续用。
    pub fn spawn_refresher(self: Arc<Self>, every: Duration) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // 第一个 tick 立即触发，启动时已经读过了，跳过
            loop {
                tick.tick().await;
                match self.refresh().await {
                    Ok(s) => tracing::debug!(
                        log_columns = s.logs.columns.len(),
                        trace_columns = s.traces.columns.len(),
                        "schema refreshed"
                    ),
                    Err(e) => {
                        tracing::warn!(error = %e, "schema refresh failed; keeping the previous one")
                    }
                }
            }
        });
    }
}

/// 四个属性列及其是否为数组（events / links 是 `Array(JSON)`）。
pub const ATTRIBUTE_COLUMNS: &[(&str, bool)] = &[
    ("resource_attributes", false),
    ("span_attributes", false),
    ("events.attributes", true),
    ("links.attributes", true),
];

/// logpipe 固定写的列（`LogEvent` 的字段）。
pub const LOG_FIXED_COLUMNS: &[&str] =
    &["timestamp", "level", "trace_id", "span_id", "thread", "logger", "message", "file", "host"];

/// tracepipe 固定写的列（`SpanEvent` 的字段，events / links 按 Nested 平铺）。
pub const TRACE_FIXED_COLUMNS: &[&str] = &[
    "timestamp",
    "trace_id",
    "span_id",
    "parent_span_id",
    "trace_state",
    "span_name",
    "span_kind",
    "service_name",
    "duration_ns",
    "status_code",
    "status_message",
    "scope_name",
    "scope_version",
    "resource_attributes",
    "span_attributes",
    "events.timestamp",
    "events.name",
    "events.attributes",
    "links.trace_id",
    "links.span_id",
    "links.trace_state",
    "links.attributes",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_types() {
        assert_eq!(ColumnKind::classify("String"), ColumnKind::String);
        assert_eq!(ColumnKind::classify("LowCardinality(String)"), ColumnKind::String);
        assert_eq!(ColumnKind::classify("LowCardinality(Nullable(String))"), ColumnKind::String);
        assert_eq!(ColumnKind::classify("UInt64"), ColumnKind::Int);
        assert_eq!(ColumnKind::classify("Int64"), ColumnKind::Int);
        assert_eq!(ColumnKind::classify("Float64"), ColumnKind::Float);
        assert_eq!(ColumnKind::classify("DateTime64(3, 'Asia/Shanghai')"), ColumnKind::DateTime);
        assert_eq!(ColumnKind::classify("Map(LowCardinality(String), String)"), ColumnKind::Map);
        assert_eq!(ColumnKind::classify("JSON"), ColumnKind::Json);
        assert_eq!(ColumnKind::classify("JSON(max_dynamic_paths = 4096)"), ColumnKind::Json);
        assert_eq!(ColumnKind::classify("Array(JSON)"), ColumnKind::Array);
        assert_eq!(ColumnKind::classify("Array(DateTime64(9))"), ColumnKind::Array);
        assert_eq!(ColumnKind::classify("UUID"), ColumnKind::Other);
    }

    #[test]
    fn extra_columns_are_the_non_fixed_strings() {
        let table = Table {
            name: "app_log".into(),
            columns: ["timestamp", "level", "message", "namespace", "pod", "replica"]
                .iter()
                .map(|n| Column {
                    name: (*n).to_owned(),
                    ty: if *n == "timestamp" {
                        "DateTime64(3)".into()
                    } else if *n == "replica" {
                        "Int64".into()
                    } else {
                        "String".into()
                    },
                    kind: ColumnKind::classify(if *n == "timestamp" {
                        "DateTime64(3)"
                    } else if *n == "replica" {
                        "Int64"
                    } else {
                        "String"
                    }),
                })
                .collect(),
        };
        let extra: Vec<&str> = table
            .extra_string_columns(&["timestamp", "level", "message"])
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(extra, ["namespace", "pod"]);
        assert_eq!(table.missing(&["level", "trace_id"]), ["trace_id"]);
    }
}
