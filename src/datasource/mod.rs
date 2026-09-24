//! 数据源：排障时直连的业务库（`--datasources` 指向的那份 TOML）。
//!
//! 日志、链路、指标回答的是「哪里慢、哪里错」，可很多问题要落到数据上才看得清：这一单的状态
//! 到底是什么、那条 SQL 的执行计划走没走索引、缓存里的值是不是旧的。这一层让 MCP 客户端（在
//! 代码仓库里开着的 Claude Code / Cursor）能对着代码直接查业务库，不必再让人去翻堡垒机。
//!
//! 支持四种库：MySQL（含 MariaDB / TiDB 这类兼容协议）、Redis、Elasticsearch、ClickHouse。
//! 每种库一个子模块，对外是同一组动作——列表、看结构、只读查询、慢查询——见 [`Source`]。
//!
//! ## 只读是怎么保证的
//!
//! 三层，从外到内：
//!
//! 1. **账号**：部署文档要求给每个数据源配只读账号。这是唯一真正可靠的一层，下面两层是在它没配好
//!    时不至于出事；
//! 2. **库的只读模式**：MySQL 每次都在 `START TRANSACTION READ ONLY` 里执行、结束即 `ROLLBACK`，
//!    ClickHouse 每条带 `readonly=2`，Elasticsearch 只开放 `_search` / `_sql` / `_mapping` 这类
//!    读接口（URL 由 opdash 拼，模型碰不到），Redis 只放行白名单里的只读命令；
//! 3. **语句校验**：SQL 先过 [`sql::check_read_only`]，把写语句、多语句、读文件的函数拦在库外。
//!
//! 结果也有上限：每个数据源的 `max_rows`、执行超时，以及 MCP 那一侧的字节预算。
//!
//! ## 为什么是配置文件
//!
//! 连接串里有密码、内网地址，这些都不能进仓库；和 `--bill-alloc` 一样，由部署方以 Secret /
//! 挂载卷送进容器。密码可以写成 `${ENV_NAME}`，读取时从环境变量取，文件本身就可以放进 ConfigMap。

pub mod clickhouse;
pub mod elastic;
pub mod mysql;
pub mod redis;
pub mod sql;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Error, Result};

/// 单次查询默认最多返回多少行；可在每个数据源上用 `max_rows` 改。
pub const DEFAULT_MAX_ROWS: u32 = 500;
/// `max_rows` 的上限。再多模型也读不完，只会把 MCP 的字节预算撑爆。
const MAX_ROWS_CAP: u32 = 10_000;
/// 单次操作的默认超时。
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_TIMEOUT: Duration = Duration::from_secs(300);
/// 建连接的超时。连不上的库不该让模型干等一整个查询超时。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Mysql,
    Redis,
    #[serde(alias = "es")]
    Elasticsearch,
    Clickhouse,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Mysql => "mysql",
            Kind::Redis => "redis",
            Kind::Elasticsearch => "elasticsearch",
            Kind::Clickhouse => "clickhouse",
        }
    }

    /// 链路埋点里的 `db.system` / `db.system.name` → 哪种数据源。认不出的（postgresql、mongodb）
    /// 返回 `None`：opdash 连不了它们，调用照样列出来，只是标不出数据源。
    pub fn of_db_system(system: &str) -> Option<Kind> {
        match system.trim().to_ascii_lowercase().as_str() {
            "mysql" | "mariadb" | "tidb" | "other_sql" => Some(Kind::Mysql),
            "redis" => Some(Kind::Redis),
            "elasticsearch" | "opensearch" => Some(Kind::Elasticsearch),
            "clickhouse" => Some(Kind::Clickhouse),
            _ => None,
        }
    }
}

/// 配置文件里的一段 `[[source]]`。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceConfig {
    name: String,
    kind: Kind,
    url: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password: Option<String>,
    /// Elasticsearch 的 API key（`Authorization: ApiKey …`），与 user / password 二选一
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// 这个库属于哪个环境。一套 opdash 同时配了生产库和测试库时用它区分；不写 = 与
    /// opdash 自己（`--env`）是同一个环境
    #[serde(default)]
    env: Option<String>,
    #[serde(default)]
    services: Vec<String>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    timeout: Option<String>,
    #[serde(default)]
    max_rows: Option<u32>,
    /// ClickHouse 专用：查 `system.query_log` 时用 `clusterAllReplicas` 跨所有副本
    #[serde(default)]
    cluster: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    source: Vec<SourceConfig>,
}

/// 一次操作的上下文：展示时间用的时区、现在几点。
#[derive(Debug, Clone, Copy)]
pub struct Ctx {
    pub tz: Tz,
    pub now_ms: i64,
}

/// 一张结果表。列名和行分开给：几百行对象形式的 JSON 里，列名要重复几百遍。
#[derive(Debug, Default, Serialize)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// 行数到了上限、后面还有没取的
    pub truncated: bool,
}

impl Table {
    pub fn new(columns: &[&str]) -> Self {
        Self { columns: columns.iter().map(|c| (*c).to_owned()).collect(), ..Self::default() }
    }

    /// 某一列的下标。
    pub fn col(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.eq_ignore_ascii_case(name))
    }
}

/// `db_query` 的参数。
#[derive(Debug, Default, Clone)]
pub struct QueryOpts {
    pub database: Option<String>,
    /// Elasticsearch 的索引（DSL 查询必填）
    pub index: Option<String>,
    pub limit: u32,
}

/// 慢查询的参数。
#[derive(Debug, Clone)]
pub struct SlowOpts {
    /// `None` = 数据源配置的库；`Some("*")` = 不按库筛
    pub database: Option<String>,
    pub from_ms: i64,
    pub to_ms: i64,
    pub min_ms: f64,
    pub sort: SlowSort,
    pub limit: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlowSort {
    /// 总耗时：最该优化的（次数 × 单次）
    Total,
    Avg,
    Max,
    Calls,
}

impl SlowSort {
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        Ok(match raw.unwrap_or("total") {
            "total" => SlowSort::Total,
            "avg" => SlowSort::Avg,
            "max" => SlowSort::Max,
            "calls" => SlowSort::Calls,
            other => {
                return Err(Error::bad_request(format!(
                    "sort 只能是 total / avg / max / calls，不是 {other:?}"
                )));
            }
        })
    }
}

enum Backend {
    Mysql(mysql::Mysql),
    Redis(redis::Redis),
    Elastic(elastic::Elastic),
    ClickHouse(clickhouse::Ch),
}

/// 一个配好的数据源。
pub struct Source {
    pub name: String,
    pub kind: Kind,
    pub description: String,
    pub env: Option<String>,
    /// 用这个库的服务（链路里的 service_name），用来把 span 对到数据源上
    pub services: Vec<String>,
    /// 连接地址里的主机和端口，展示和匹配 span 的 `server.address` 用
    pub host: String,
    pub port: Option<u16>,
    /// 链路里 `server.address` 可能出现的其他写法（域名、VIP、`host:port`）
    pub aliases: Vec<String>,
    /// 默认库（MySQL / ClickHouse 的库名，Redis 的库号）
    pub database: Option<String>,
    pub max_rows: u32,
    pub timeout: Duration,
    backend: Backend,
}

impl Source {
    /// 列表里给人 / 模型看的样子。不带账号密码。
    pub fn summary(&self) -> Value {
        let mut v = json!({
            "name": self.name,
            "kind": self.kind.as_str(),
            "address": match self.port {
                Some(p) => format!("{}:{p}", self.host),
                None => self.host.clone(),
            },
            "max_rows": self.max_rows,
        });
        if let Some(env) = &self.env {
            v["env"] = json!(env);
        }
        if !self.description.is_empty() {
            v["description"] = json!(self.description);
        }
        if let Some(db) = &self.database {
            v["database"] = json!(db);
        }
        if !self.services.is_empty() {
            v["services"] = json!(self.services);
        }
        v
    }

    /// 生效的行数上限：请求给的（0 = 默认 50）和数据源的上限取小。
    pub fn limit(&self, wanted: u32) -> u32 {
        let wanted = if wanted == 0 { 50 } else { wanted };
        wanted.min(self.max_rows).max(1)
    }

    /// 超时包一层：库那一侧的超时（`max_execution_time` 这类）先到、错误信息更干净；
    /// 这里多留几秒兜住建连接、网络卡住这些库管不到的情况。
    async fn timed<T>(&self, fut: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        match tokio::time::timeout(self.timeout + Duration::from_secs(5), fut).await {
            Ok(r) => r,
            Err(_) => Err(Error::Source {
                status: 504,
                message: format!(
                    "数据源 {} 超过 {} 秒没有响应",
                    self.name,
                    (self.timeout + Duration::from_secs(5)).as_secs()
                ),
            }),
        }
    }

    pub async fn tables(
        &self,
        database: Option<&str>,
        pattern: Option<&str>,
        limit: u32,
    ) -> Result<Value> {
        let limit = self.limit(limit);
        self.timed(async {
            match &self.backend {
                Backend::Mysql(b) => b.tables(self, database, pattern, limit).await,
                Backend::Redis(b) => b.tables(self, pattern, limit).await,
                Backend::Elastic(b) => b.tables(self, pattern, limit).await,
                Backend::ClickHouse(b) => b.tables(self, database, pattern, limit).await,
            }
        })
        .await
    }

    pub async fn describe(&self, target: &str, database: Option<&str>) -> Result<Value> {
        let target = target.trim();
        if target.is_empty() {
            return Err(Error::bad_request("target 不能为空：写表名（可带库名）、索引名或键名"));
        }
        self.timed(async {
            match &self.backend {
                Backend::Mysql(b) => b.describe(self, target, database).await,
                Backend::Redis(b) => b.describe(self, target).await,
                Backend::Elastic(b) => b.describe(self, target).await,
                Backend::ClickHouse(b) => b.describe(self, target, database).await,
            }
        })
        .await
    }

    pub async fn query(&self, query: &str, opts: QueryOpts) -> Result<Value> {
        let query = query.trim();
        if query.is_empty() {
            return Err(Error::bad_request("query 不能为空"));
        }
        let opts = QueryOpts { limit: self.limit(opts.limit), ..opts };
        let started = std::time::Instant::now();
        let mut out = self
            .timed(async {
                match &self.backend {
                    Backend::Mysql(b) => b.query(self, query, &opts).await,
                    Backend::Redis(b) => b.query(self, query).await,
                    Backend::Elastic(b) => b.query(self, query, &opts).await,
                    Backend::ClickHouse(b) => b.query(self, query, &opts).await,
                }
            })
            .await?;
        out["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
        Ok(out)
    }

    pub async fn slow(&self, opts: SlowOpts, ctx: Ctx) -> Result<Value> {
        let opts = SlowOpts { limit: self.limit(opts.limit), ..opts };
        self.timed(async {
            match &self.backend {
                Backend::Mysql(b) => b.slow(self, &opts).await,
                Backend::Redis(b) => b.slow(self, &opts, ctx).await,
                Backend::Elastic(b) => b.slow(self, &opts).await,
                Backend::ClickHouse(b) => b.slow(self, &opts, ctx).await,
            }
        })
        .await
    }

    /// 慢查询按哪个库筛：没给就用数据源配置的库，`*` 表示不筛。
    fn slow_database(&self, opts: &SlowOpts) -> Option<String> {
        match opts.database.as_deref() {
            Some("*") => None,
            Some(db) => Some(db.to_owned()),
            None => self.database.clone(),
        }
    }

    /// 这个数据源和一次数据库调用有多像：主机对上 4 分、库名对上 2 分、服务对上 1 分，
    /// 类型对不上直接 0。见 [`Registry::match_call`]。
    fn score(&self, call: &CallTarget<'_>) -> u32 {
        if Kind::of_db_system(call.system) != Some(self.kind) {
            return 0;
        }
        let mut score = 0;
        let host = call.host.trim().to_ascii_lowercase();
        if !host.is_empty() {
            let port_ok = |p: Option<u16>| match (p, call.port) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            };
            let hit = (host == self.host.to_ascii_lowercase() && port_ok(self.port))
                || self.aliases.iter().any(|a| {
                    let (h, p) = split_host_port(a);
                    h.eq_ignore_ascii_case(&host) && port_ok(p)
                });
            if hit {
                score += 4;
            }
        }
        if !call.database.is_empty() && self.database.as_deref() == Some(call.database) {
            score += 2;
        }
        if !call.service.is_empty() && self.services.iter().any(|s| s == call.service) {
            score += 1;
        }
        score
    }
}

/// 一次数据库调用（链路里的一个 Client span）指向哪里，从 span 属性里取。
#[derive(Debug, Default)]
pub struct CallTarget<'a> {
    pub system: &'a str,
    pub host: &'a str,
    pub port: Option<u16>,
    pub database: &'a str,
    pub service: &'a str,
}

/// 全部数据源。没配 `--datasources` 时是空的，`db_*` 工具不出现在目录里。
#[derive(Default)]
pub struct Registry {
    sources: Vec<Arc<Source>>,
}

impl Registry {
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    pub fn all(&self) -> &[Arc<Source>] {
        &self.sources
    }

    pub fn get(&self, name: &str) -> Result<Arc<Source>> {
        if self.sources.is_empty() {
            return Err(Error::Source {
                status: 404,
                message: "这个 opdash 没有配置数据源（--datasources），无法直连业务库".to_owned(),
            });
        }
        self.sources.iter().find(|s| s.name == name).cloned().ok_or_else(|| Error::Source {
            status: 404,
            message: format!(
                "没有叫 {name:?} 的数据源；可用的数据源：{}",
                self.sources.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
            ),
        })
    }

    /// 一次数据库调用最可能落在哪个数据源上。只看类型一致的；主机、库名、服务名至少对上一样
    /// 才算，全都对不上宁可不标——标错了模型会拿着别的库去查，比标不出来更糟。
    pub fn match_call(&self, call: &CallTarget<'_>) -> Option<&Arc<Source>> {
        let mut best: Option<(&Arc<Source>, u32)> = None;
        for s in &self.sources {
            let score = s.score(call);
            if score > 0 && best.is_none_or(|(_, b)| score > b) {
                best = Some((s, score));
            }
        }
        best.map(|(s, _)| s)
    }

    /// 读配置文件。有任何一处不对就报错，进程带着原因退出：数据源是拿去排障的，
    /// 少一个库静默不见，比起不来更难发现。
    pub fn load(path: &Path) -> std::result::Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读取数据源配置 {} 失败: {e}", path.display()))?;
        Self::parse(&text, &|name| std::env::var(name).ok())
            .map_err(|e| format!("数据源配置 {} 有误: {e}", path.display()))
    }

    /// 解析配置文本。`env` 用来展开 `${NAME}`，测试里换成假的。
    pub fn parse(
        text: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> std::result::Result<Self, String> {
        let file: FileConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut sources: Vec<Arc<Source>> = Vec::new();
        for cfg in file.source {
            let name = cfg.name.clone();
            if !is_source_name(&name) {
                return Err(format!(
                    "数据源名称 {name:?} 只能包含字母、数字、下划线和短横线，长度 1~64"
                ));
            }
            if sources.iter().any(|s| s.name == name) {
                return Err(format!("数据源名称 {name:?} 重复"));
            }
            let source = build_source(cfg, env).map_err(|e| format!("数据源 {name}: {e}"))?;
            sources.push(Arc::new(source));
        }
        Ok(Self { sources })
    }
}

fn is_source_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 连接信息里去掉了密码之后剩下的部分，各后端从这里拿。
pub struct Resolved {
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub api_key: Option<String>,
    pub database: Option<String>,
    pub timeout: Duration,
    pub cluster: Option<String>,
}

fn build_source(
    cfg: SourceConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<Source, String> {
    let expand = |field: &str, v: Option<String>| -> std::result::Result<Option<String>, String> {
        v.map(|v| expand_env(&v, env).map_err(|e| format!("{field}: {e}"))).transpose()
    };
    let url = expand_env(&cfg.url, env).map_err(|e| format!("url: {e}"))?;
    let user = expand("user", cfg.user)?.filter(|s| !s.is_empty());
    let password = expand("password", cfg.password)?.filter(|s| !s.is_empty());
    let api_key = expand("api_key", cfg.api_key)?.filter(|s| !s.is_empty());
    let timeout = match cfg.timeout.as_deref() {
        Some(raw) => humantime::parse_duration(raw)
            .map_err(|e| format!("timeout {raw:?} 写法不对（例：15s / 1m）: {e}"))?,
        None => DEFAULT_TIMEOUT,
    };
    if timeout < Duration::from_secs(1) || timeout > MAX_TIMEOUT {
        return Err("timeout 应在 1 秒到 5 分钟之间".to_owned());
    }
    let max_rows = cfg.max_rows.unwrap_or(DEFAULT_MAX_ROWS);
    if max_rows == 0 || max_rows > MAX_ROWS_CAP {
        return Err(format!("max_rows 应在 1 到 {MAX_ROWS_CAP} 之间"));
    }
    if api_key.is_some() && cfg.kind != Kind::Elasticsearch {
        return Err("api_key 只用于 Elasticsearch".to_owned());
    }
    if cfg.cluster.is_some() && cfg.kind != Kind::Clickhouse {
        return Err("cluster 只用于 ClickHouse".to_owned());
    }
    let parsed = url::Url::parse(&url).map_err(|e| format!("url 无法解析: {e}"))?;
    let scheme_ok = match cfg.kind {
        Kind::Mysql => parsed.scheme() == "mysql",
        Kind::Redis => parsed.scheme() == "redis",
        Kind::Elasticsearch | Kind::Clickhouse => matches!(parsed.scheme(), "http" | "https"),
    };
    if !scheme_ok {
        return Err(match cfg.kind {
            Kind::Mysql => "MySQL 的 url 应以 mysql:// 开头".to_owned(),
            Kind::Redis => {
                "Redis 的 url 应以 redis:// 开头（暂不支持 rediss:// 与集群模式）".to_owned()
            }
            _ => "url 应以 http:// 或 https:// 开头".to_owned(),
        });
    }
    let host = parsed.host_str().unwrap_or("").to_owned();
    if host.is_empty() {
        return Err("url 里缺少主机名".to_owned());
    }
    let port = parsed.port_or_known_default().or(match cfg.kind {
        Kind::Mysql => Some(3306),
        Kind::Redis => Some(6379),
        _ => None,
    });
    // 默认库：配置里写了就用，否则取 URL 路径（mysql://h/db、redis://h/0）
    let path_db = parsed.path().trim_matches('/').to_owned();
    let database = cfg.database.clone().filter(|s| !s.is_empty()).or_else(|| match cfg.kind {
        Kind::Mysql | Kind::Redis if !path_db.is_empty() => Some(path_db.clone()),
        _ => None,
    });
    let resolved = Resolved {
        url: url.clone(),
        user,
        password,
        api_key,
        database: database.clone(),
        timeout,
        cluster: cfg.cluster.clone(),
    };
    let backend = match cfg.kind {
        Kind::Mysql => Backend::Mysql(mysql::Mysql::new(&resolved)?),
        Kind::Redis => Backend::Redis(redis::Redis::new(&resolved)?),
        Kind::Elasticsearch => Backend::Elastic(elastic::Elastic::new(&resolved)?),
        Kind::Clickhouse => Backend::ClickHouse(clickhouse::Ch::new(&resolved)?),
    };
    Ok(Source {
        name: cfg.name,
        kind: cfg.kind,
        description: cfg.description.unwrap_or_default(),
        env: cfg.env.map(|e| e.trim().to_owned()).filter(|e| !e.is_empty()),
        services: cfg.services,
        host,
        port,
        aliases: cfg.aliases,
        database,
        max_rows,
        timeout,
        backend,
    })
}

/// `${NAME}` 换成环境变量的值；变量没设置就报错，不留空——空密码连上去的报错只会说「认证失败」，
/// 看不出是漏配了环境变量。
fn expand_env(
    raw: &str,
    env: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or_else(|| "${ 没有对应的 }".to_owned())?;
        let name = &after[..end];
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("环境变量名 {name:?} 不合法"));
        }
        let value = env(name).ok_or_else(|| format!("环境变量 {name} 没有设置"))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `host:port` → (host, port)；没有端口或端口不是数字就只有 host。IPv6 的 `[::1]:3306` 也认。
pub fn split_host_port(s: &str) -> (&str, Option<u16>) {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('[')
        && let Some((h, tail)) = rest.split_once(']')
    {
        return (h, tail.strip_prefix(':').and_then(|p| p.parse().ok()));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => match p.parse() {
            Ok(port) => (h, Some(port)),
            Err(_) => (s, None),
        },
        _ => (s, None),
    }
}

/// 给模型看的文本截断：超过 `max` 个字符只留前面，并注明原长。
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…（共 {} 字符）", s.chars().count())
}

/// 二进制值（MySQL 的 BLOB、Redis 的序列化对象）不是 UTF-8 时的展示：十六进制前 64 字节 + 总长。
pub fn bytes_to_json(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => Value::String(s.to_owned()),
        Err(_) => {
            let hex: String = b.iter().take(64).map(|x| format!("{x:02x}")).collect();
            Value::String(if b.len() > 64 {
                format!("0x{hex}…（二进制，共 {} 字节）", b.len())
            } else {
                format!("0x{hex}")
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "DB_PASS" => Some("p@ss".into()),
            _ => None,
        }
    }

    const SAMPLE: &str = r#"
[[source]]
name = "order-db"
kind = "mysql"
url = "mysql://reader@10.0.0.5:3306/shop"
password = "${DB_PASS}"
description = "订单库（只读从库）"
env = "测试"
services = ["order-service"]
aliases = ["rm-demo.mysql.example.com"]

[[source]]
name = "order-cache"
kind = "redis"
url = "redis://10.0.0.6:6379/2"
services = ["order-service"]

[[source]]
name = "search"
kind = "es"
url = "http://10.0.0.7:9200"
max_rows = 100
timeout = "30s"

[[source]]
name = "biz-ck"
kind = "clickhouse"
url = "http://10.0.0.8:8123"
database = "biz"
cluster = "default"
"#;

    #[test]
    fn parses_a_full_config() {
        let reg = Registry::parse(SAMPLE, &env).unwrap();
        assert_eq!(reg.all().len(), 4);
        let db = reg.get("order-db").unwrap();
        assert_eq!(db.kind, Kind::Mysql);
        assert_eq!(db.database.as_deref(), Some("shop"), "库名取自 URL 路径");
        assert_eq!(db.port, Some(3306));
        assert_eq!(db.max_rows, DEFAULT_MAX_ROWS);
        let summary = db.summary().to_string();
        assert!(!summary.contains("p@ss"), "列表里不能带密码: {summary}");
        assert!(summary.contains("10.0.0.5:3306"), "{summary}");
        assert_eq!(db.summary()["env"], "测试");
        let cache = reg.get("order-cache").unwrap();
        assert_eq!(cache.database.as_deref(), Some("2"), "Redis 的库号");
        let es = reg.get("search").unwrap();
        assert_eq!(es.kind, Kind::Elasticsearch, "es 是 elasticsearch 的简写");
        assert_eq!(es.port, Some(9200));
        assert_eq!(es.limit(1000), 100, "请求的行数不能超过数据源的上限");
        assert_eq!(es.limit(0), 50);
        assert_eq!(es.timeout, Duration::from_secs(30));
        assert!(reg.get("nope").err().unwrap().to_string().contains("order-db"));
    }

    #[test]
    fn rejects_bad_configs() {
        let bad = |text: &str| Registry::parse(text, &env).err().unwrap_or_default();
        assert!(
            bad("[[source]]\nname = \"a\"\nkind = \"mysql\"\nurl = \"mysql://h/${NOPE}\"")
                .contains("NOPE")
        );
        assert!(
            bad("[[source]]\nname = \"a b\"\nkind = \"mysql\"\nurl = \"mysql://h/d\"")
                .contains("名称")
        );
        assert!(
            bad("[[source]]\nname = \"a\"\nkind = \"mysql\"\nurl = \"http://h/d\"")
                .contains("mysql://")
        );
        assert!(
            bad("[[source]]\nname = \"a\"\nkind = \"redis\"\nurl = \"rediss://h\"")
                .contains("rediss")
        );
        assert!(bad("[[source]]\nname = \"a\"\nkind = \"pg\"\nurl = \"x\"").contains("kind"));
        assert!(
            bad("[[source]]\nname = \"a\"\nkind = \"mysql\"\nurl = \"mysql://h\"\npasswd = \"x\"")
                .contains("passwd")
        );
        let dup = "[[source]]\nname = \"a\"\nkind = \"redis\"\nurl = \"redis://h\"\n";
        assert!(bad(&format!("{dup}{dup}")).contains("重复"));
        assert!(Registry::parse("", &env).unwrap().is_empty());
    }

    #[test]
    fn matches_calls_by_host_database_and_service() {
        let reg = Registry::parse(SAMPLE, &env).unwrap();
        let name = |call: CallTarget<'_>| reg.match_call(&call).map(|s| s.name.clone());
        // 主机写成了配置里的别名
        assert_eq!(
            name(CallTarget {
                system: "mysql",
                host: "RM-DEMO.mysql.example.com",
                port: Some(3306),
                ..Default::default()
            }),
            Some("order-db".into())
        );
        // 只有服务名对得上也算
        assert_eq!(
            name(CallTarget { system: "redis", service: "order-service", ..Default::default() }),
            Some("order-cache".into())
        );
        // 类型不对，服务名对上也不算
        assert_eq!(
            name(CallTarget {
                system: "postgresql",
                service: "order-service",
                ..Default::default()
            }),
            None
        );
        // 什么都对不上
        assert_eq!(
            name(CallTarget { system: "mysql", host: "10.9.9.9", ..Default::default() }),
            None
        );
        // 端口不同就不是同一个库
        assert_eq!(
            name(CallTarget {
                system: "mysql",
                host: "10.0.0.5",
                port: Some(3307),
                ..Default::default()
            }),
            None
        );
    }

    /// 仓库里的示例配置得能直接用：字段名改了而示例没跟上，照抄的人会在启动时撞上报错。
    #[test]
    fn the_example_file_parses() {
        let text = include_str!("../../examples/datasources.toml");
        let reg = Registry::parse(text, &|_| Some("x".to_owned())).unwrap();
        let kinds: Vec<Kind> = reg.all().iter().map(|s| s.kind).collect();
        assert_eq!(kinds, [Kind::Mysql, Kind::Redis, Kind::Elasticsearch, Kind::Clickhouse]);
    }

    #[test]
    fn expands_env_and_splits_hosts() {
        assert_eq!(expand_env("a${DB_PASS}b", &env).unwrap(), "ap@ssb");
        assert_eq!(expand_env("plain", &env).unwrap(), "plain");
        assert!(expand_env("${", &env).is_err());
        assert_eq!(split_host_port("h:3306"), ("h", Some(3306)));
        assert_eq!(split_host_port("h"), ("h", None));
        assert_eq!(split_host_port("[::1]:6379"), ("::1", Some(6379)));
        assert_eq!(clip("abcdef", 3), "abc…（共 6 字符）");
        assert_eq!(bytes_to_json(&[0xff, 0x00]), json!("0xff00"));
    }
}
