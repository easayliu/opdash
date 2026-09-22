//! 表结构：启动时读 `system.columns`，之后定期刷新。
//!
//! 三张表的固定列是程序知道的；可选列（k8s 元数据 `namespace` / `pod` / `container` / `stream`，
//! 以及采集端配置里 `fields` 加的 `cluster` / `env` / `app`……）随部署不同而不同，只能从库里读。
//! 指标表（metricpipe 的 `otel_metric`）是**可选**的：没部署 metricpipe 的地方这张表不存在，
//! 那就把它当没有（[`Schema::metrics`] 为 `None`、前端不显示指标页），日志和链路照常——
//! 少一张表不该让整个 schema 读失败。goscan 的三张账单表（[`Schema::bills`]）同理，而且
//! 三张之间也各自可选：只接了一朵云是常态。
//!
//! 账单表还多读一样东西：**建表时的排序键**（`system.tables.sorting_key`）。它就是
//! `ReplacingMergeTree` 的去重键，查询要按它再去重一次，原因见 [`crate::query::bills`]。
//!
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

/// 一张账单表，外加它的去重键。
#[derive(Debug, Clone, Serialize)]
pub struct BillTable {
    #[serde(flatten)]
    pub table: Table,
    /// 去重键 = 建表时的排序键（`ReplacingMergeTree` 按它去重）。从 `system.tables` 读，
    /// 读不到（权限不够、集群上 Distributed 表自己没有排序键）才退回 goscan 当前 DDL 的那套。
    pub dedupe: Vec<String>,
}

/// goscan 写的三张账单表。三张各自可选：只接了一朵云、或者只同步了月度粒度都正常。
#[derive(Debug, Clone, Default, Serialize)]
pub struct BillTables {
    pub volcengine: Option<BillTable>,
    pub alicloud_monthly: Option<BillTable>,
    pub alicloud_daily: Option<BillTable>,
}

impl BillTables {
    /// 至少有一张能查。
    pub fn any(&self) -> bool {
        self.volcengine.is_some()
            || self.alicloud_monthly.is_some()
            || self.alicloud_daily.is_some()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Schema {
    pub logs: Table,
    pub traces: Table,
    /// 指标表，没部署 metricpipe 时为 `None`
    pub metrics: Option<Table>,
    /// 指标表没启用的原因（表不存在 / 缺列 / 属性列不是 JSON），给 `/api/meta` 显示
    pub metrics_note: Option<String>,
    /// 账单表，没部署 goscan 时为 `None`
    pub bills: Option<BillTables>,
    /// 费用页没启用的原因
    pub bills_note: Option<String>,
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
    metric_table: String,
    /// goscan 的三张账单表，配置里的名字（集群上真正的表名可能带 `_distributed` 后缀，见
    /// [`resolve_bill_table`]）
    bill_tables: [String; 3],
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

#[derive(Deserialize)]
struct SortingKeyRow {
    table: String,
    sorting_key: String,
}

impl SchemaCache {
    pub fn new(
        client: Client,
        database: &str,
        log_table: &str,
        trace_table: &str,
        metric_table: &str,
        bill_tables: [&str; 3],
    ) -> Self {
        Self {
            client,
            database: database.to_owned(),
            log_table: log_table.to_owned(),
            trace_table: trace_table.to_owned(),
            metric_table: metric_table.to_owned(),
            bill_tables: bill_tables.map(str::to_owned),
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
        // 账单表在集群上叫 `X_distributed`（goscan 的建表口径），单机上就叫 `X`：两个名字
        // 一起问，哪个真在库里哪个算数，见 [`Self::load_bills`]
        let bill_candidates: Vec<String> = self
            .bill_tables
            .iter()
            .enumerate()
            .flat_map(|(slot, n)| bill_table_candidates(slot, n))
            .collect();
        let columns = self
            .client
            .rows::<ColumnRow>(
                Query::new(
                    "SELECT table, name, type FROM system.columns \
                     WHERE database = {db:String} \
                       AND (table IN ({logs:String}, {traces:String}, {metrics:String}) \
                            OR table IN {bills:Array(String)}) \
                     ORDER BY table, position",
                )
                .param("db", &self.database)
                .param("logs", &self.log_table)
                .param("traces", &self.trace_table)
                .param("metrics", &self.metric_table)
                .param("bills", bill_candidates.clone()),
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

        let read = |name: &str| -> Option<Table> {
            let cols: Vec<Column> = columns
                .iter()
                .filter(|c| c.table == name)
                .map(|c| Column {
                    name: c.name.clone(),
                    ty: c.ty.clone(),
                    kind: ColumnKind::classify(&c.ty),
                })
                .collect();
            (!cols.is_empty()).then(|| Table { name: name.to_owned(), columns: cols })
        };
        let table = |name: &str| -> Result<Table> {
            read(name).ok_or_else(|| {
                Error::Internal(format!(
                    "ClickHouse 里没有表 {}.{name}（或者没有读 system.columns 的权限）",
                    self.database
                ))
            })
        };
        let logs = table(&self.log_table)?;
        let traces = table(&self.trace_table)?;
        // 指标表可选：不存在、缺列、属性列不是 JSON，都只是「没有指标页」，不影响另外两张表
        let (metrics, metrics_note) = match read(&self.metric_table) {
            None => (
                None,
                Some(format!(
                    "{}.{} 不存在（没部署 metricpipe 的话本来就没有这张表）",
                    self.database, self.metric_table
                )),
            ),
            Some(t) => match metric_table_problem(&t, &self.database) {
                Some(note) => (None, Some(note)),
                None => (Some(t), None),
            },
        };

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

        // 账单表（goscan）：三张各自可选，一张都没有就是没部署 goscan
        let (bills, bills_note) = self.load_bills(&columns).await;

        Ok(Schema {
            logs,
            traces,
            metrics,
            metrics_note,
            bills,
            bills_note,
            server_version: server.version,
            server_timezone: server.timezone,
            loaded_at: Instant::now(),
        })
    }

    /// 读三张账单表。任何一张出问题都只是「这张不能查」，不影响别的表——所以这里不返回
    /// `Result`，问题都写进 note 里给 `/api/meta`。
    async fn load_bills(&self, columns: &[ColumnRow]) -> (Option<BillTables>, Option<String>) {
        let read = |name: &str| -> Option<Table> {
            let cols: Vec<Column> = columns
                .iter()
                .filter(|c| c.table == name)
                .map(|c| Column {
                    name: c.name.clone(),
                    ty: c.ty.clone(),
                    kind: ColumnKind::classify(&c.ty),
                })
                .collect();
            (!cols.is_empty()).then(|| Table { name: name.to_owned(), columns: cols })
        };

        let specs: [(&str, &str, &[&str]); 3] = [
            ("火山引擎账单表", self.bill_tables[0].as_str(), VOLCENGINE_BILL_COLUMNS),
            ("阿里云月度账单表", self.bill_tables[1].as_str(), ALICLOUD_BILL_COLUMNS),
            ("阿里云日度账单表", self.bill_tables[2].as_str(), ALICLOUD_BILL_COLUMNS),
        ];
        let mut found: Vec<Option<Table>> = Vec::with_capacity(3);
        let mut notes: Vec<String> = Vec::new();
        for (slot, (label, configured, required)) in specs.into_iter().enumerate() {
            // 按候选顺序挑第一个真在库里的：同名的优先（goscan 现在的口径），然后是
            // `_distributed`（改名之前建的那批），最后是火山那张表的老名字。
            // **不看 `_local`**：那只是一个分片的数据，查出来的金额只有三分之一
            let table = bill_table_candidates(slot, configured).into_iter().find_map(|n| read(&n));
            match table {
                None => {
                    notes.push(format!("{label} {}.{configured} 不存在", self.database));
                    found.push(None);
                }
                Some(t) => {
                    let missing = t.missing(required);
                    if missing.is_empty() {
                        found.push(Some(t));
                    } else {
                        notes.push(format!(
                            "{label} {}.{} 缺列: {}",
                            self.database,
                            t.name,
                            missing.join(", ")
                        ));
                        found.push(None);
                    }
                }
            }
        }
        if found.iter().all(Option::is_none) {
            return (
                None,
                Some(format!("{}（未部署 goscan 时本就没有这几张表）", notes.join("；"))),
            );
        }

        let keys = self.sorting_keys(found.iter().flatten().map(|t| t.name.as_str())).await;
        let mut bills = BillTables::default();
        for (slot, table) in found.into_iter().enumerate() {
            let Some(table) = table else { continue };
            let dedupe = keys
                .iter()
                .find(|(name, _)| *name == table.name)
                .map(|(_, key)| key.clone())
                .unwrap_or_else(|| FALLBACK_DEDUPE[slot].iter().map(|s| (*s).to_owned()).collect());
            // 去重键里的列必须真的在表上，不然拼出来的 SQL 会报 UNKNOWN_IDENTIFIER
            let dedupe: Vec<String> = if dedupe.iter().all(|c| table.has(c)) {
                dedupe
            } else {
                tracing::warn!(
                    table = %table.name,
                    sorting_key = %dedupe.join(", "),
                    "账单表的排序键里有表上没有的列，按 goscan 当前 DDL 的去重键查"
                );
                FALLBACK_DEDUPE[slot].iter().map(|s| (*s).to_owned()).collect()
            };
            let bill = BillTable { table, dedupe };
            match slot {
                0 => bills.volcengine = Some(bill),
                1 => bills.alicloud_monthly = Some(bill),
                _ => bills.alicloud_daily = Some(bill),
            }
        }
        (Some(bills), (!notes.is_empty()).then(|| notes.join("；")))
    }

    /// 每张账单表的排序键（= `ReplacingMergeTree` 的去重键）。
    ///
    /// Distributed 表自己没有排序键，要问它底下的 `_local`；读不到就回空，调用方退回静态定义。
    /// 这一趟只在真有账单表时才发，没部署 goscan 的环境一条多余的查询都不会多。
    async fn sorting_keys<'a>(
        &self,
        tables: impl Iterator<Item = &'a str>,
    ) -> Vec<(String, Vec<String>)> {
        let names: Vec<String> = tables
            .flat_map(|n| match n.strip_suffix(DISTRIBUTED_SUFFIX) {
                Some(base) => vec![n.to_owned(), format!("{base}{LOCAL_SUFFIX}")],
                None => vec![n.to_owned()],
            })
            .collect();
        if names.is_empty() {
            return Vec::new();
        }
        let rows = self
            .client
            .rows::<SortingKeyRow>(
                Query::new(
                    "SELECT table, sorting_key FROM system.tables \
                     WHERE database = {db:String} AND table IN {tables:Array(String)}",
                )
                .param("db", &self.database)
                .param("tables", names),
            )
            .await;
        let rows = match rows {
            Ok(r) => r.rows,
            Err(e) => {
                tracing::warn!(error = %e, "读不到账单表的排序键，按 goscan 当前 DDL 的去重键查");
                return Vec::new();
            }
        };
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        for row in &rows {
            let Some(key) = parse_sorting_key(&row.sorting_key) else { continue };
            // `X_local` 的排序键就是 `X_distributed` 的去重键
            let target = match row.table.strip_suffix(LOCAL_SUFFIX) {
                Some(base) => format!("{base}{DISTRIBUTED_SUFFIX}"),
                None => row.table.clone(),
            };
            if !out.iter().any(|(n, _)| *n == target) {
                out.push((target, key));
            }
        }
        out
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
                        metric_columns = s.metrics.as_ref().map_or(0, |t| t.columns.len()),
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

/// 火山账单表的老名字。goscan 把它改成了 `volcengine_bill`，但线上是「先按新口径重建表、
/// 后改配置」，两个名字会并存一段时间；`--volcengine-bill-table` 保持默认时两个都认。
pub const LEGACY_VOLCENGINE_BILL_TABLE: &str = "volcengine_bill_details";

/// goscan 早期版本在集群上把 Distributed 表建成 `X_distributed`（本地表是 `X_local`）。
/// 后来改成了和 logpipe / tracepipe / metricpipe 一样的口径——Distributed 表就叫 `X`——
/// 但改名之前建的表还在库里，所以按基础名找不到时再试一次这个后缀。
const DISTRIBUTED_SUFFIX: &str = "_distributed";
const LOCAL_SUFFIX: &str = "_local";

/// `system.tables.sorting_key` 解析成列名。
///
/// 只认「逗号分隔的纯列名」——账单表的排序键就长这样。带函数调用的（`toDate(x)` 之类）
/// 直接放弃：那种键没法当 `GROUP BY` 的去重键用，退回静态定义更安全。
fn parse_sorting_key(raw: &str) -> Option<Vec<String>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let cols: Vec<String> = raw.split(',').map(|c| c.trim().to_owned()).collect();
    let plain = |c: &String| {
        !c.is_empty()
            && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            && !c.chars().next().is_some_and(|ch| ch.is_ascii_digit())
    };
    cols.iter().all(plain).then_some(cols)
}

/// 一张账单表在库里可能叫什么，按优先级排。
///
/// goscan 这几张表的名字动过两次：集群上的 `_distributed` 后缀取消了（现在和 logpipe 一样，
/// Distributed 表就叫基础名），火山那张从 `volcengine_bill_details` 改成了 `volcengine_bill`。
/// 线上是「先重建表、后改配置」，所以同一时刻可能有好几个名字并存——**挑一个能查的，别让
/// 每个部署都为此配一行环境变量**。`_local` 永远不在候选里：那只是一个分片的数据。
fn bill_table_candidates(slot: usize, configured: &str) -> Vec<String> {
    let mut names = vec![configured.to_owned(), format!("{configured}{DISTRIBUTED_SUFFIX}")];
    // 名字是默认的 `volcengine_bill` 时才兜老名字；配成别的名字就按配的那个来
    if slot == 0 && configured == crate::config::DEFAULT_VOLCENGINE_BILL_TABLE {
        names.push(LEGACY_VOLCENGINE_BILL_TABLE.to_owned());
        names.push(format!("{LEGACY_VOLCENGINE_BILL_TABLE}{DISTRIBUTED_SUFFIX}"));
    }
    names
}

/// 读不到排序键时用的去重键，和 goscan `pkg/ddl` 里三张表的 ORDER BY 一致。
/// 顺序是「火山 / 阿里云月度 / 阿里云日度」，和 [`SchemaCache::bill_tables`] 对应。
const FALLBACK_DEDUPE: [&[&str]; 3] = [
    &[
        "BillPeriod",
        "ExpenseDate",
        "InstanceNo",
        "ExpenseBeginTime",
        "Product",
        "ElementCode",
        "PayableAmount",
    ],
    &[
        "billing_cycle",
        "product_code",
        "instance_id",
        "bill_account_id",
        "subscription_type",
        "payment_amount",
    ],
    &[
        "billing_date",
        "product_code",
        "instance_id",
        "bill_account_id",
        "subscription_type",
        "payment_amount",
    ],
];

/// 火山引擎账单表里 opdash 会用到的列。列名是火山 API 原样的 PascalCase，
/// 金额那几列是 `String`（goscan 刻意保留原值），查询时转 Float64。
pub const VOLCENGINE_BILL_COLUMNS: &[&str] = &[
    "BillPeriod",
    "ExpenseDate",
    "ExpenseBeginTime",
    "InstanceNo",
    "InstanceName",
    "Product",
    "ProductZh",
    "Element",
    "ElementCode",
    "Region",
    "RegionCode",
    "Zone",
    "ZoneCode",
    "OwnerID",
    "OwnerUserName",
    "OwnerCustomerName",
    "Project",
    "ProjectDisplayName",
    "BillingMode",
    "Currency",
    "Count",
    "Unit",
    "PayableAmount",
    "PaidAmount",
    "OriginalBillAmount",
];

/// 阿里云账单表（月度 / 日度共用一套列）里 opdash 会用到的列。
pub const ALICLOUD_BILL_COLUMNS: &[&str] = &[
    "billing_cycle",
    "billing_date",
    "product_code",
    "product_name",
    "product_type",
    "product_detail",
    "instance_id",
    "instance_name",
    "bill_account_id",
    "bill_account_name",
    "subscription_type",
    "region",
    "zone",
    "resource_group",
    "cost_unit",
    "currency",
    "usage",
    "usage_unit",
    "pretax_amount",
    "payment_amount",
    "pretax_gross_amount",
];

/// 四个属性列及其是否为数组（events / links 是 `Array(JSON)`）。
pub const ATTRIBUTE_COLUMNS: &[(&str, bool)] = &[
    ("resource_attributes", false),
    ("span_attributes", false),
    ("events.attributes", true),
    ("links.attributes", true),
];

/// 指标表不能用的原因，`None` = 能用。缺列 / 属性列不是 JSON 都只是禁用指标页。
fn metric_table_problem(table: &Table, database: &str) -> Option<String> {
    let missing = table.missing(METRIC_FIXED_COLUMNS);
    if !missing.is_empty() {
        return Some(format!(
            "{database}.{} 缺列: {}（这些是 metricpipe 固定会写的列），指标页已停用",
            table.name,
            missing.join(", ")
        ));
    }
    for name in ["resource_attributes", "attributes"] {
        let col = table.column(name)?;
        if col.kind != ColumnKind::Json {
            return Some(format!(
                "{database}.{} 的 {name} 列是 {}，opdash 只支持 JSON 属性列（ClickHouse 25.3+），指标页已停用",
                table.name, col.ty
            ));
        }
    }
    None
}

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

/// metricpipe 固定写的列（`MetricEvent` 的字段，exemplars / quantiles 按 Nested 平铺）。
/// 五种指标类型共用一张表，用不上的列留默认值，所以每一列都是「一定在」的。
pub const METRIC_FIXED_COLUMNS: &[&str] = &[
    "timestamp",
    "start_timestamp",
    "metric_name",
    "metric_type",
    "metric_unit",
    "metric_description",
    "service_name",
    "scope_name",
    "scope_version",
    "resource_attributes",
    "attributes",
    "value",
    "temporality",
    "is_monotonic",
    "count",
    "sum",
    "min",
    "max",
    "bucket_counts",
    "explicit_bounds",
    "quantiles.quantile",
    "quantiles.value",
    "exemplars.timestamp",
    "exemplars.value",
    "exemplars.trace_id",
    "exemplars.span_id",
    "flags",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_table_must_be_complete_and_json() {
        let cols = |extra: &[(&str, &str)]| -> Table {
            let mut columns: Vec<Column> = METRIC_FIXED_COLUMNS
                .iter()
                .map(|n| Column {
                    name: (*n).to_owned(),
                    ty: "String".into(),
                    kind: ColumnKind::String,
                })
                .collect();
            for (name, ty) in extra {
                if let Some(c) = columns.iter_mut().find(|c| &c.name == name) {
                    c.ty = (*ty).to_owned();
                    c.kind = ColumnKind::classify(ty);
                }
            }
            Table { name: "otel_metric".into(), columns }
        };
        let json = [("resource_attributes", "JSON"), ("attributes", "JSON")];
        assert_eq!(metric_table_problem(&cols(&json), "logs"), None);
        // 属性列还是老的 Map：停用而不是报错
        let stale = cols(&[("resource_attributes", "JSON"), ("attributes", "Map(String, String)")]);
        assert!(metric_table_problem(&stale, "logs").unwrap().contains("attributes"));
        let mut short = cols(&json);
        short.columns.retain(|c| c.name != "explicit_bounds");
        assert!(metric_table_problem(&short, "logs").unwrap().contains("explicit_bounds"));
    }

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
