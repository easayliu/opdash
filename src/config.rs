//! 命令行 / 环境变量配置。每个 flag 都有对应的 `OPDASH_*` 环境变量，容器里直接用 env 配。

use std::net::SocketAddr;
use std::time::Duration;

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "opdash", version, about = "Trace & log query UI over ClickHouse")]
pub struct Config {
    /// 监听地址
    #[arg(long, env = "OPDASH_BIND", default_value = "0.0.0.0:4880")]
    pub bind: SocketAddr,

    /// ClickHouse HTTP 地址（和 logpipe / tracepipe 的 sink.endpoint 一样）
    #[arg(long, env = "OPDASH_CLICKHOUSE_URL", default_value = "http://127.0.0.1:8123")]
    pub clickhouse_url: String,

    #[arg(long, env = "OPDASH_CLICKHOUSE_USER", default_value = "default")]
    pub clickhouse_user: String,

    #[arg(long, env = "OPDASH_CLICKHOUSE_PASSWORD", default_value = "", hide_env_values = true)]
    pub clickhouse_password: String,

    /// 两张表所在的库
    #[arg(long, env = "OPDASH_DATABASE", default_value = "logs")]
    pub database: String,

    /// logpipe 写的日志表
    #[arg(long, env = "OPDASH_LOG_TABLE", default_value = "app_log")]
    pub log_table: String,

    /// tracepipe 写的 span 表
    #[arg(long, env = "OPDASH_TRACE_TABLE", default_value = "otel_trace")]
    pub trace_table: String,

    /// 直方图、按天聚合时对齐用的时区；应和两张表 timestamp 列的时区一致
    #[arg(long, env = "OPDASH_TIMEZONE", default_value = "Asia/Shanghai")]
    pub timezone: String,

    /// 单条查询的最长执行时间（传给 ClickHouse 的 max_execution_time）
    #[arg(long, env = "OPDASH_QUERY_TIMEOUT", default_value = "30s", value_parser = parse_duration)]
    pub query_timeout: Duration,

    /// 允许查询的最大时间跨度；表的 TTL 是 30 天，再大也查不到
    #[arg(long, env = "OPDASH_MAX_RANGE", default_value = "31d", value_parser = parse_duration)]
    pub max_range: Duration,

    /// 单次查询（一页）最多返回多少行日志 / trace
    #[arg(long, env = "OPDASH_MAX_ROWS", default_value_t = 1000)]
    pub max_rows: u32,

    /// 日志分页最多翻到多深；再深让用户缩小范围，ClickHouse 的 OFFSET 是要把前面的都排一遍的
    #[arg(long, env = "OPDASH_MAX_OFFSET", default_value_t = 10_000)]
    pub max_offset: u32,

    /// 导出 CSV / JSONL 最多多少行
    #[arg(long, env = "OPDASH_EXPORT_MAX_ROWS", default_value_t = 50_000)]
    pub export_max_rows: u32,

    /// 一条 trace 最多取多少个 span，超过的截断并在响应里标记
    #[arg(long, env = "OPDASH_MAX_TRACE_SPANS", default_value_t = 5_000)]
    pub max_trace_spans: u32,

    /// 单条查询最多允许 ClickHouse 读多少字节（0 = 不限）。集群上防一条全表扫拖垮所有人，
    /// 超过时 ClickHouse 立刻报错，前端提示缩小范围，比等 30 秒超时体验好
    #[arg(long, env = "OPDASH_MAX_READ_BYTES", default_value_t = 0)]
    pub max_read_bytes: u64,

    /// 单条查询最多允许 ClickHouse 读多少行（0 = 不限），同上
    #[arg(long, env = "OPDASH_MAX_READ_ROWS", default_value_t = 0)]
    pub max_read_rows: u64,

    /// 同时最多几条查询在 ClickHouse 上跑（库是共用的，别让一个页面把它打满）；排队 10 秒没名额回 503
    #[arg(long, env = "OPDASH_MAX_CONCURRENT_QUERIES", default_value_t = 16)]
    pub max_concurrent_queries: usize,

    /// 多久重新读一次 system.columns（新加的 fields 列不用重启就能筛）
    #[arg(long, env = "OPDASH_SCHEMA_REFRESH", default_value = "5m", value_parser = parse_duration)]
    pub schema_refresh: Duration,

    /// 可选的 HTTP Basic 认证，格式 user:password。内网、不配也行
    #[arg(long, env = "OPDASH_BASIC_AUTH", hide_env_values = true, value_parser = parse_basic_auth)]
    pub basic_auth: Option<BasicAuth>,
}

#[derive(Debug, Clone)]
pub struct BasicAuth {
    pub user: String,
    pub password: String,
}

fn parse_basic_auth(raw: &str) -> Result<BasicAuth, String> {
    let (user, password) =
        raw.split_once(':').ok_or_else(|| "格式应为 user:password".to_owned())?;
    if user.is_empty() {
        return Err("用户名不能为空".to_owned());
    }
    Ok(BasicAuth { user: user.to_owned(), password: password.to_owned() })
}

fn parse_duration(raw: &str) -> Result<Duration, String> {
    humantime::parse_duration(raw).map_err(|e| format!("{e}（例：30s / 5m / 7d）"))
}

impl Config {
    /// 校验几个互相有关系的值。clap 自己只管单个参数的格式。
    pub fn validate(&self) -> Result<(), String> {
        if self.max_rows == 0 {
            return Err("--max-rows 不能为 0".into());
        }
        if self.query_timeout.as_secs() == 0 {
            return Err("--query-timeout 至少 1 秒".into());
        }
        for (flag, name) in [
            ("--database", &self.database),
            ("--log-table", &self.log_table),
            ("--trace-table", &self.trace_table),
        ] {
            if !is_plain_identifier(name) {
                return Err(format!("{flag} 只能包含字母、数字、下划线: {name:?}"));
            }
        }
        Ok(())
    }
}

/// 库名表名只放行 `[A-Za-z0-9_]`：它们会被反引号拼进 SQL，别让配置里的一个反引号把语句拆了。
pub fn is_plain_identifier(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_defaults() {
        let cfg = Config::parse_from(["opdash"]);
        assert_eq!(cfg.bind.port(), 4880);
        assert_eq!(cfg.database, "logs");
        assert_eq!(cfg.query_timeout, Duration::from_secs(30));
        assert_eq!(cfg.max_range, Duration::from_secs(31 * 86400));
        assert!(cfg.basic_auth.is_none());
        cfg.validate().unwrap();
    }

    #[test]
    fn parses_basic_auth_and_durations() {
        let cfg =
            Config::parse_from(["opdash", "--basic-auth", "ops:s3cret:x", "--query-timeout", "2m"]);
        let auth = cfg.basic_auth.unwrap();
        assert_eq!(auth.user, "ops");
        assert_eq!(auth.password, "s3cret:x");
        assert_eq!(cfg.query_timeout, Duration::from_secs(120));
    }

    #[test]
    fn rejects_bad_identifiers() {
        let cfg = Config::parse_from(["opdash", "--log-table", "app`log"]);
        assert!(cfg.validate().is_err());
    }
}
