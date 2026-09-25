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

    /// metricpipe 写的指标表。这张表可以没有——没部署 metricpipe 时指标页自动隐藏，
    /// 日志和链路照常
    #[arg(long, env = "OPDASH_METRIC_TABLE", default_value = "otel_metric")]
    pub metric_table: String,

    /// goscan 写的火山引擎账单明细表。和指标表一样是可选的：没部署 goscan 时费用页自动隐藏。
    ///
    /// 名字跟 goscan 走：它把这张表从 `volcengine_bill_details` 改成了 `volcengine_bill`，
    /// 同时把集群上的 `_distributed` 后缀取消了——现在和 logpipe / tracepipe / metricpipe 一个
    /// 口径，Distributed 表就叫基础表名，只有底下存数据的本地表带 `_local`。
    ///
    /// **老表仍然认**：配的名字在库里找不到同名表时，会再找一次 `<名字>_distributed`
    /// （改名之前建的那批就长这样）。**不会**退回 `_local`，那只是一个分片的数据。
    /// 这一项是默认的 `volcengine_bill` 时还会再兜一层老名字 `volcengine_bill_details`（见
    /// [`crate::schema::LEGACY_VOLCENGINE_BILL_TABLE`]）——线上是先按新口径重建了表名、
    /// 再慢慢改回配置的，两个名字会并存一段时间，不该要求每个部署都去配一行环境变量。
    #[arg(long, env = "OPDASH_VOLCENGINE_BILL_TABLE", default_value = DEFAULT_VOLCENGINE_BILL_TABLE)]
    pub volcengine_bill_table: String,

    /// goscan 写的阿里云月度账单表，同上可选
    #[arg(long, env = "OPDASH_ALICLOUD_MONTHLY_TABLE", default_value = "alicloud_bill_monthly")]
    pub alicloud_monthly_table: String,

    /// goscan 写的阿里云日度账单表，同上可选
    #[arg(long, env = "OPDASH_ALICLOUD_DAILY_TABLE", default_value = "alicloud_bill_daily")]
    pub alicloud_daily_table: String,

    /// 账单查询怎么去重。三张账单表都是 `ReplacingMergeTree`，而 goscan 建的 Distributed 表用
    /// `rand()` 分片：同一个账期重复拉一次，一模一样的两行会落到不同分片上，`FINAL` 只在分片内
    /// 去重，跨分片的那份它看不见——而 goscan 的日调度每天都会把当月重拉一遍，所以这不是小概率。
    ///
    /// * `group`（默认）：按建表时的排序键加上金额列在查询里 `GROUP BY` 一次，再按 `updated_at`
    ///   只留每个键最近一次同步写入的行。去重键并不唯一（阿里云的尾差调整与正常账单同键），
    ///   金额进分组才不会把并列的两行当成一行；
    /// * `final`：给表加 `FINAL`，即引擎自己的去重——同键只留一行，**会丢掉并列行**，
    ///   在 goscan 让每行账单的键都唯一之前不要用；
    /// * `off`：什么都不做，最快，但重复拉过的账期金额会翻倍。
    ///
    /// 详见 docs/bills.md「查询时去重」。
    #[arg(long, env = "OPDASH_BILL_DEDUPE", default_value = "group")]
    pub bill_dedupe: crate::query::bills::Dedupe,

    /// 成本归属规则文件（TOML），费用页的「分析」视图据此把账单分摊到业务线。
    ///
    /// 不配也能用：分析视图照常给出日均与月度预估，只是少了业务线这一层——「哪台机器属于谁」
    /// 不在账单之中，只能由部署方给出。格式与写法见 `examples/bill-alloc.toml` 与
    /// [`crate::alloc`]。**文件有误时进程直接退出**：归属规则关乎金额，静默降级只会让使用者
    /// 对着一份错账排查。
    #[arg(long, env = "OPDASH_BILL_ALLOC")]
    pub bill_alloc: Option<std::path::PathBuf>,

    /// 数据源配置文件（TOML）：MCP 排障时可直连的业务 MySQL / Redis / Elasticsearch / ClickHouse。
    ///
    /// 配了之后 MCP 多出 `db_*` 一组工具（列表、看结构、只读查询、慢查询），链路里的数据库调用
    /// 也能对到具体的数据源上。**全部只读**：MySQL 在只读事务里执行，ClickHouse 带 `readonly=2`，
    /// Redis 只放行只读命令，Elasticsearch 只开放检索接口；但请务必给每个数据源配只读账号。
    /// 格式见 `examples/datasources.toml` 与 [`crate::datasource`]。**文件有误时进程直接退出**。
    #[arg(long, env = "OPDASH_DATASOURCES")]
    pub datasources: Option<std::path::PathBuf>,

    /// 这套 opdash 属于哪个环境（如 `生产`、`测试`、`prod`）。
    ///
    /// 同一个人往往同时接着几套 opdash 的 MCP（生产一套、测试一套，域名不同），模型只看工具名
    /// 分不出哪套是哪套。配了之后环境名写进 MCP 握手的 `serverInfo.title` 和使用说明的第一句、
    /// `get_meta` 与 `db_sources` 的返回里。不配就只报访问所用的域名。
    #[arg(long, env = "OPDASH_ENV")]
    pub env: Option<String>,

    /// 页面上「API key」对话框里拼接入命令时用的 MCP 服务名（`claude mcp add … <名字>`）。
    ///
    /// 同一台电脑接着几套 opdash 时名字必须不同：命令里先 `remove` 同名的再 `add`，都叫
    /// `opdash` 的话接第二套就把第一套删了。名字还会成为工具名的前缀（`mcp__opdash-prod__…`），
    /// 模型靠它分清是哪套。只能用字母、数字、下划线、短横线。不配时：`--env` 是这些字符就用
    /// `opdash-<env>`（小写），否则（如 `生产`）用 `opdash`。
    #[arg(long, env = "OPDASH_MCP_NAME")]
    pub mcp_name: Option<String>,

    /// goscan 的地址（如 `http://goscan.logging.svc.cluster.local:8080`）。
    ///
    /// 配了之后费用页上多一个「拉取账单」：opdash 把请求转给 goscan 的 `POST /sync`，再按返回的
    /// task id 轮它的 `GET /tasks/{id}` 看结果。**这是 opdash 唯一一处会往外发「会改状态」的请求**
    /// ——账单是 goscan 去云厂商那儿拉的，opdash 自己对 ClickHouse 仍然只读（每条查询都带
    /// `readonly=2`）。不配 = 不显示这个按钮，账单只能等 goscan 自己的 cron。
    ///
    /// goscan 的 HTTP 接口没有认证（它是集群内的服务），所以这一层的门是 opdash 的登录。
    #[arg(long, env = "OPDASH_GOSCAN_URL", value_parser = parse_service_url)]
    pub goscan_url: Option<String>,

    /// 调 goscan 接口的超时。触发同步是「登记一个后台任务就返回」，很快；
    /// 真正的拉取在 goscan 那边跑，靠轮询任务状态看结果，和这个超时无关
    #[arg(long, env = "OPDASH_GOSCAN_TIMEOUT", default_value = "10s", value_parser = parse_duration)]
    pub goscan_timeout: Duration,

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

    /// 列表 / 上下文 / 跟随里每条日志的 `message` 最多取多少个字符，超过的截断并在响应里带上
    /// 原始长度（`message_len`）。**导出不受这个限制**，要全文就导出。
    ///
    /// 线上 `message` 的 p50 是 129 字符、p99 是 9.4 KB，但一小时 844 万条里有 200 条超过 1 MB、
    /// 最大 49 MB——撞上一条，整页就卡死在传输和渲染上（实测一条 41 MB 的日志让链路详情页
    /// 一次要收 53 MB，12.6 秒里 11 秒花在传）。默认 16384 字符，是 p99 的 1.7 倍，正常的堆栈
    /// 和 SQL 一个字都不会少。
    #[arg(long, env = "OPDASH_MAX_MESSAGE_CHARS", default_value_t = 16_384)]
    pub max_message_chars: u32,

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

    /// 日志跟随（SSE）时服务端多久查一次增量。每个跟随连接就是这个频率的一条轻量查询
    #[arg(long, env = "OPDASH_TAIL_INTERVAL", default_value = "1s", value_parser = parse_duration)]
    pub tail_interval: Duration,

    /// 同时最多几条跟随连接（每条都在按 --tail-interval 轮 ClickHouse）；满了回 503
    #[arg(long, env = "OPDASH_MAX_TAIL_STREAMS", default_value_t = 8)]
    pub max_tail_streams: usize,

    /// 多久重新读一次 system.columns（新加的 fields 列不用重启就能筛）
    #[arg(long, env = "OPDASH_SCHEMA_REFRESH", default_value = "5m", value_parser = parse_duration)]
    pub schema_refresh: Duration,

    /// 可选的 HTTP Basic 认证，格式 user:password。内网、不配也行；和 OIDC 可以同时开（脚本 / curl 用）
    #[arg(long, env = "OPDASH_BASIC_AUTH", hide_env_values = true, value_parser = parse_basic_auth)]
    pub basic_auth: Option<BasicAuth>,

    /// OIDC 登录（Keycloak）：realm 的 issuer 地址，如 https://sso.example.com/realms/ops。
    /// 配了就走浏览器跳 Keycloak 登录；`--oidc-client-id` 必须一起配
    #[arg(long, env = "OPDASH_OIDC_ISSUER", value_parser = parse_issuer)]
    pub oidc_issuer: Option<String>,

    /// Keycloak 里给 opdash 建的 client 的 Client ID
    #[arg(long, env = "OPDASH_OIDC_CLIENT_ID")]
    pub oidc_client_id: Option<String>,

    /// client 的密钥（Keycloak 里 Client authentication 打开时有）；public client 不用配，PKCE 照样保护
    #[arg(long, env = "OPDASH_OIDC_CLIENT_SECRET", hide_env_values = true)]
    pub oidc_client_secret: Option<String>,

    /// 授权请求的 scope
    #[arg(long, env = "OPDASH_OIDC_SCOPES", default_value = "openid profile email")]
    pub oidc_scopes: String,

    /// 要求用户带这个角色才放行（realm 角色或本 client 的角色都认）；不配 = 登录了就行
    #[arg(long, env = "OPDASH_OIDC_REQUIRED_ROLE")]
    pub oidc_required_role: Option<String>,

    /// 浏览器访问 opdash 的地址（如 https://opdash.example.com），用来拼 OIDC 回调地址。
    /// 不配则按请求的 Host / X-Forwarded-* 头推；本地 vite 开发时配成 http://localhost:5173
    #[arg(long, env = "OPDASH_PUBLIC_URL", value_parser = parse_public_url)]
    pub public_url: Option<String>,

    /// 登录后会话多久失效（会话是签名 cookie，到期要重新跳一次 Keycloak）
    #[arg(long, env = "OPDASH_SESSION_TTL", default_value = "12h", value_parser = parse_duration)]
    pub session_ttl: Duration,

    /// 会话 cookie 的签名密钥（随便一串长随机字符）。不配则每次启动随机生成：重启后大家都要重新登录，
    /// 多副本部署时必须配同一个
    #[arg(long, env = "OPDASH_SESSION_SECRET", hide_env_values = true)]
    pub session_secret: Option<String>,

    /// 用户自己生成的 API key（给 MCP / 脚本用）最长有效多久；生成时可以选更短的
    #[arg(long, env = "OPDASH_API_KEY_TTL", default_value = "90d", value_parser = parse_duration)]
    pub api_key_ttl: Duration,

    /// API key 存在哪个文件（只存哈希）。容器里把所在目录挂成卷，不然重启就没了；
    /// 多副本要共享同一个文件
    #[arg(long, env = "OPDASH_API_KEY_FILE", default_value = "api-keys.json")]
    pub api_key_file: std::path::PathBuf,

    /// 用户收藏的查询存在哪个文件（按用户区分；没开认证就是大家共用一份）。
    /// 和 API key 文件一样要挂成卷、多副本共享同一个
    #[arg(long, env = "OPDASH_SAVED_QUERY_FILE", default_value = "saved-queries.json")]
    pub saved_query_file: std::path::PathBuf,
}

/// 火山账单表的默认名字。goscan 把它从 `volcengine_bill_details` 改成了这个。
pub const DEFAULT_VOLCENGINE_BILL_TABLE: &str = "volcengine_bill";

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

/// issuer 必须和 Keycloak 签在 token 里的 `iss` 一字不差，末尾的斜杠去掉，避免比对时差一个字符。
fn parse_issuer(raw: &str) -> Result<String, String> {
    let s = raw.trim().trim_end_matches('/');
    if !(s.starts_with("https://") || s.starts_with("http://")) {
        return Err(
            "应是 http(s):// 开头的 realm 地址，如 https://sso.example.com/realms/ops".to_owned()
        );
    }
    Ok(s.to_owned())
}

/// 集群内服务的地址（goscan）。和 `--public-url` 同一套校验，只是报错里举的例子不同。
fn parse_service_url(raw: &str) -> Result<String, String> {
    let s = raw.trim().trim_end_matches('/');
    if !(s.starts_with("https://") || s.starts_with("http://")) {
        return Err("应是 http(s):// 开头的地址，如 http://goscan.logging.svc.cluster.local:8080"
            .to_owned());
    }
    Ok(s.to_owned())
}

fn parse_public_url(raw: &str) -> Result<String, String> {
    let s = raw.trim().trim_end_matches('/');
    if !(s.starts_with("https://") || s.starts_with("http://")) {
        return Err("应是 http(s):// 开头的地址，如 https://opdash.example.com".to_owned());
    }
    Ok(s.to_owned())
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
        if self.max_message_chars < 256 {
            return Err("--max-message-chars 不能小于 256，否则无法容纳一行完整的堆栈".into());
        }
        if self.query_timeout.as_secs() == 0 {
            return Err("--query-timeout 至少 1 秒".into());
        }
        if self.oidc_issuer.is_some() && self.oidc_client_id.is_none() {
            return Err("配置 --oidc-issuer 时必须同时配置 --oidc-client-id".into());
        }
        if self.tail_interval < Duration::from_millis(200) {
            return Err("--tail-interval 不能小于 200ms，否则会对数据库造成过大压力".into());
        }
        if self.tail_interval > Duration::from_secs(60) {
            return Err("--tail-interval 不能大于 60s，否则将失去实时跟随的意义".into());
        }
        if self.max_tail_streams == 0 {
            return Err("--max-tail-streams 至少 1".into());
        }
        if self.session_ttl.as_secs() < 60 {
            return Err("--session-ttl 至少 1 分钟".into());
        }
        if let Some(env) = &self.env
            && (env.trim().is_empty() || env.chars().count() > 32)
        {
            return Err("--env 应是 1~32 个字符的环境名，如 生产 / 测试 / prod".into());
        }
        if let Some(n) = &self.mcp_name
            && !is_mcp_name(n)
        {
            return Err(format!("--mcp-name 只能包含字母、数字、下划线、短横线，长度 1~64: {n:?}"));
        }
        if self.api_key_ttl.as_secs() < 3600 {
            return Err("--api-key-ttl 至少 1 小时".into());
        }
        for (flag, name) in [
            ("--database", &self.database),
            ("--log-table", &self.log_table),
            ("--trace-table", &self.trace_table),
            ("--metric-table", &self.metric_table),
            ("--volcengine-bill-table", &self.volcengine_bill_table),
            ("--alicloud-monthly-table", &self.alicloud_monthly_table),
            ("--alicloud-daily-table", &self.alicloud_daily_table),
        ] {
            if !is_plain_identifier(name) {
                return Err(format!("{flag} 只能包含字母、数字、下划线：{name:?}"));
            }
        }
        Ok(())
    }
}

impl Config {
    /// 生效的 MCP 服务名，见 [`Config::mcp_name`] 字段。
    pub fn mcp_server_name(&self) -> String {
        if let Some(n) = &self.mcp_name {
            return n.clone();
        }
        match self.env.as_deref().map(|e| e.trim().to_ascii_lowercase().replace(' ', "-")) {
            Some(e) if is_mcp_name(&e) => format!("opdash-{e}"),
            _ => "opdash".to_owned(),
        }
    }
}

fn is_mcp_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
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
        assert_eq!(cfg.metric_table, "otel_metric");
        assert_eq!(cfg.volcengine_bill_table, "volcengine_bill");
        assert_eq!(cfg.bill_dedupe, crate::query::bills::Dedupe::Group);
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
    fn oidc_needs_client_id_and_trims_issuer() {
        let cfg =
            Config::parse_from(["opdash", "--oidc-issuer", "https://sso.example.com/realms/ops/"]);
        assert_eq!(cfg.oidc_issuer.as_deref(), Some("https://sso.example.com/realms/ops"));
        assert!(cfg.validate().is_err());
        let cfg = Config::parse_from([
            "opdash",
            "--oidc-issuer",
            "https://sso.example.com/realms/ops",
            "--oidc-client-id",
            "opdash",
        ]);
        cfg.validate().unwrap();
        assert!(Config::try_parse_from(["opdash", "--oidc-issuer", "sso.example.com"]).is_err());
    }

    #[test]
    fn mcp_name_follows_env_unless_given() {
        let name = |args: &[&str]| {
            let cfg = Config::parse_from([&["opdash"], args].concat());
            cfg.validate().map(|_| cfg.mcp_server_name())
        };
        assert_eq!(name(&[]).unwrap(), "opdash");
        assert_eq!(name(&["--env", "UAT"]).unwrap(), "opdash-uat");
        assert_eq!(name(&["--env", "生产"]).unwrap(), "opdash", "中文环境名拼不进命令");
        assert_eq!(name(&["--env", "生产", "--mcp-name", "opdash-prod"]).unwrap(), "opdash-prod");
        assert!(name(&["--mcp-name", "opdash prod"]).is_err());
    }

    #[test]
    fn rejects_bad_identifiers() {
        let cfg = Config::parse_from(["opdash", "--log-table", "app`log"]);
        assert!(cfg.validate().is_err());
    }
}
