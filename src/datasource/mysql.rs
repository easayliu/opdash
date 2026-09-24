//! MySQL 数据源（MariaDB、TiDB 这类兼容协议的也走这里）。
//!
//! 每次操作从连接池拿一条连接，先把会话设好——执行超时、`sql_select_limit`、默认库——再开一个
//! `START TRANSACTION READ ONLY`，所有语句都在这个事务里跑，结束时 `ROLLBACK`。只读事务里写语句
//! 会被 MySQL 以 1792 拒绝，所以即便账号有写权限、语句校验又漏了，也写不进去。
//!
//! 连接池是懒建的：`Pool::new` 要在 tokio 运行时里调，而且配了十个库、一次都没查的话，没必要
//! 一启动就去连十个库。

use std::sync::OnceLock;

use mysql_async::consts::ColumnType;
use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, OptsBuilder, Params, Pool, PoolConstraints, PoolOpts};
use serde_json::{Value, json};

use super::sql::{self, Dialect, quote_ident};
use super::{Resolved, SlowOpts, SlowSort, Source, Table, bytes_to_json};
use crate::error::{Error, Result};

/// 每个数据源最多同时几条连接。排障是人在问、模型在查，并发很低；给少了只会排队，
/// 给多了是在业务库上占连接数。
const MAX_CONNECTIONS: usize = 4;

pub struct Mysql {
    opts: Opts,
    pool: OnceLock<Pool>,
    /// 报错里用的地址，不带密码
    address: String,
}

impl Mysql {
    pub fn new(r: &Resolved) -> std::result::Result<Self, String> {
        let opts = Opts::from_url(&r.url).map_err(|e| format!("MySQL 连接串无法解析: {e}"))?;
        let address = format!("{}:{}", opts.ip_or_hostname(), opts.tcp_port());
        let mut b = OptsBuilder::from_opts(opts)
            .pool_opts(
                PoolOpts::default()
                    .with_constraints(
                        PoolConstraints::new(0, MAX_CONNECTIONS).expect("0 <= 4 且 4 > 0"),
                    )
                    .with_inactive_connection_ttl(std::time::Duration::from_secs(60)),
            )
            // 空闲连接不能一直占着业务库的连接数；wait_timeout 让服务端也会回收
            .wait_timeout(Some(300));
        if let Some(u) = &r.user {
            b = b.user(Some(u));
        }
        if let Some(p) = &r.password {
            b = b.pass(Some(p));
        }
        if let Some(db) = &r.database {
            b = b.db_name(Some(db));
        }
        Ok(Self { opts: b.into(), pool: OnceLock::new(), address })
    }

    fn pool(&self) -> &Pool {
        self.pool.get_or_init(|| Pool::new(self.opts.clone()))
    }

    /// 拿一条连接，把会话设好、开只读事务。
    async fn session(&self, src: &Source, database: Option<&str>, limit: u32) -> Result<Session> {
        let conn = tokio::time::timeout(super::CONNECT_TIMEOUT, self.pool().get_conn())
            .await
            .map_err(|_| Error::Source {
                status: 502,
                message: format!(
                    "连不上 MySQL 数据源 {}（{}）：{} 秒内没有建立连接",
                    src.name,
                    self.address,
                    super::CONNECT_TIMEOUT.as_secs()
                ),
            })?
            .map_err(|e| self.err(src, e))?;
        let mut s = Session { conn };
        let ms = src.timeout.as_millis();
        // max_execution_time 是 MySQL 5.7.8+ 的，MariaDB 叫 max_statement_time（单位秒）；
        // 两个都设不上也不要紧，外面还有一层超时
        if s.conn.query_drop(format!("SET SESSION max_execution_time = {ms}")).await.is_err() {
            let secs = src.timeout.as_secs().max(1);
            let _ = s.conn.query_drop(format!("SET SESSION max_statement_time = {secs}")).await;
        }
        // 没写 LIMIT 的 SELECT 最多回这么多行；多要一行用来判断有没有截断
        s.conn
            .query_drop(format!("SET SESSION sql_select_limit = {}", u64::from(limit) + 1))
            .await
            .map_err(|e| self.err(src, e))?;
        // 连接是池里复用的，上一次 USE 过别的库；每次都显式切到这次要的库
        if let Some(db) = database.or(src.database.as_deref()) {
            s.conn
                .query_drop(format!("USE {}", quote_ident(db)))
                .await
                .map_err(|e| self.err(src, e))?;
        }
        s.conn.query_drop("START TRANSACTION READ ONLY").await.map_err(|e| self.err(src, e))?;
        Ok(s)
    }

    /// 在一个会话里跑 `f`，不论成败都 `ROLLBACK`。
    async fn with_session<T>(
        &self,
        src: &Source,
        database: Option<&str>,
        limit: u32,
        f: impl AsyncFnOnce(&mut Session) -> Result<T>,
    ) -> Result<T> {
        let mut s = self.session(src, database, limit).await?;
        let out = f(&mut s).await;
        // 回滚失败说明连接已经坏了，池子会自己丢掉它
        let _ = s.conn.query_drop("ROLLBACK").await;
        out
    }

    /// mysql_async 的错误 → 给模型看的错误。服务端拒绝（语法错、没权限、表不存在）是 400，
    /// 模型改语句就行；执行超时是 504；连接层的问题是 502。
    fn err(&self, src: &Source, e: mysql_async::Error) -> Error {
        match e {
            mysql_async::Error::Server(se) => {
                let status = match se.code {
                    // 3024 = 超过 max_execution_time；1317 / 1969 = 被中断 / MariaDB 超时
                    3024 | 1317 | 1969 => 504,
                    _ => 400,
                };
                let hint = match se.code {
                    3024 | 1969 => {
                        "。建议：加上走索引的 WHERE 条件或 LIMIT，或先 EXPLAIN 看执行计划"
                    }
                    1142 | 1044 | 1227 | 1045 => "。这个数据源的账号没有该权限",
                    1792 => "。数据源只允许只读查询",
                    _ => "",
                };
                Error::Source {
                    status,
                    message: format!("MySQL 错误 {}: {}{hint}", se.code, se.message),
                }
            }
            other => Error::Source {
                status: 502,
                message: format!("MySQL 数据源 {}（{}）不可用: {other}", src.name, self.address),
            },
        }
    }

    /// 列表分三种情况，差别在要不要读统计值：
    ///
    /// * 没指定库、也没给关键字：只列有哪些库。一个实例上几十个库、上千张表很常见，
    ///   MySQL 8 取 `TABLE_ROWS` / `DATA_LENGTH` 时要给统计过期的表逐张重算（UAT 上一台
    ///   1500 张表的实例 15 秒都没算完），而模型此时要的只是「先挑哪个库」；
    /// * 没指定库、给了关键字：跨库按表名搜，不带统计值；
    /// * 指定了库：这个库的表，带估算行数和大小。
    pub async fn tables(
        &self,
        src: &Source,
        database: Option<&str>,
        pattern: Option<&str>,
        limit: u32,
    ) -> Result<Value> {
        const SYSTEM: &str = "('mysql', 'information_schema', 'performance_schema', 'sys')";
        let db = database.or(src.database.as_deref()).unwrap_or("").to_owned();
        let like = pattern.map(sql::like_contains).unwrap_or_else(|| "%".to_owned());
        let more = u64::from(limit) + 1;
        let (key, stmt, params, note) = if db.is_empty() && pattern.is_none() {
            (
                "databases",
                format!(
                    "SELECT SCHEMA_NAME AS `database` FROM information_schema.SCHEMATA \
                     WHERE SCHEMA_NAME NOT IN {SYSTEM} ORDER BY SCHEMA_NAME LIMIT {more}"
                ),
                Params::Empty,
                "没有指定库，只列出了库名；给 database 列这个库的表，或给 match 跨库按表名搜",
            )
        } else if db.is_empty() {
            (
                "tables",
                format!(
                    "SELECT TABLE_SCHEMA AS `database`, TABLE_NAME AS `table`, TABLE_COMMENT AS `comment` \
                     FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA NOT IN {SYSTEM} AND TABLE_NAME LIKE ? \
                     ORDER BY TABLE_SCHEMA, TABLE_NAME LIMIT {more}"
                ),
                Params::Positional(vec![like.into()]),
                "跨库搜索不带行数和大小；给 database 看这个库的表的估算行数与大小",
            )
        } else {
            (
                "tables",
                format!(
                    "SELECT TABLE_NAME AS `table`, TABLE_TYPE AS `type`, TABLE_ROWS AS approx_rows, \
                     ROUND((DATA_LENGTH + INDEX_LENGTH) / 1048576, 1) AS size_mb, TABLE_COMMENT AS `comment` \
                     FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME LIKE ? \
                     ORDER BY TABLE_NAME LIMIT {more}"
                ),
                Params::Positional(vec![db.clone().into(), like.into()]),
                "approx_rows 取自 information_schema，InnoDB 下是估算值",
            )
        };
        let table = self
            .with_session(src, None, limit, async |s| {
                s.fetch(&stmt, params, limit).await.map_err(|e| self.err(src, e))
            })
            .await?;
        let mut out = json!({ key: table, "note": note });
        if !db.is_empty() {
            out["database"] = json!(db);
        }
        Ok(out)
    }

    pub async fn describe(
        &self,
        src: &Source,
        target: &str,
        database: Option<&str>,
    ) -> Result<Value> {
        let (db, table) = sql::split_table(target).map_err(Error::bad_request)?;
        let db = db.or_else(|| database.map(str::to_owned)).or_else(|| src.database.clone());
        let qualified = match &db {
            Some(d) => format!("{}.{}", quote_ident(d), quote_ident(&table)),
            None => quote_ident(&table),
        };
        let db_param: mysql_async::Value = match &db {
            Some(d) => d.clone().into(),
            None => mysql_async::Value::NULL,
        };
        self.with_session(src, None, 1000, async |s| {
            let create = s
                .fetch(&format!("SHOW CREATE TABLE {qualified}"), Params::Empty, 1)
                .await
                .map_err(|e| self.err(src, e))?;
            let ddl = create.rows.first().and_then(|r| r.get(1)).cloned().unwrap_or(Value::Null);
            let indexes = s
                .fetch(
                    "SELECT INDEX_NAME AS `index`, \
                     GROUP_CONCAT(COLUMN_NAME ORDER BY SEQ_IN_INDEX) AS `columns`, \
                     MIN(NON_UNIQUE) = 0 AS `unique`, MAX(CARDINALITY) AS cardinality \
                     FROM information_schema.STATISTICS \
                     WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_NAME = ? \
                     GROUP BY INDEX_NAME ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME",
                    Params::Positional(vec![db_param.clone(), table.clone().into()]),
                    200,
                )
                .await
                .map_err(|e| self.err(src, e))?;
            let stats = s
                .fetch(
                    "SELECT TABLE_ROWS AS approx_rows, ROUND(DATA_LENGTH / 1048576, 1) AS data_mb, \
                     ROUND(INDEX_LENGTH / 1048576, 1) AS index_mb, UPDATE_TIME AS updated \
                     FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_NAME = ?",
                    Params::Positional(vec![db_param.clone(), table.clone().into()]),
                    1,
                )
                .await
                .map_err(|e| self.err(src, e))?;
            let mut out = json!({ "table": target, "create_table": ddl, "indexes": indexes });
            if let Some(row) = stats.rows.first() {
                for (c, v) in stats.columns.iter().zip(row) {
                    out[c.as_str()] = v.clone();
                }
            }
            Ok(out)
        })
        .await
    }

    pub async fn query(&self, src: &Source, query: &str, opts: &super::QueryOpts) -> Result<Value> {
        let stmt = sql::check_read_only(query, Dialect::Mysql).map_err(Error::bad_request)?;
        tracing::debug!(source = %src.name, sql = %sql::one_line(&stmt), "数据源查询");
        let table = self
            .with_session(src, opts.database.as_deref(), opts.limit, async |s| {
                match s.fetch_text(&stmt, opts.limit).await {
                    Ok(t) => Ok(t),
                    // 超时了就在同一个会话里补一次 EXPLAIN（只生成计划、不执行，毫秒级），把
                    // 「没走索引、扫了多少行」直接交给模型。否则它只知道超时，得自己再猜一轮：
                    // 线上就有过字符串列拿数字比较、索引失效扫全表，改成带引号后 15 秒变 19 毫秒
                    Err(mysql_async::Error::Server(se)) if se.code == 3024 || se.code == 1969 => {
                        let plan = explain_hint(s, &stmt).await;
                        let mut e = self.err(src, mysql_async::Error::Server(se));
                        if let (Some(plan), Error::Source { message, .. }) = (plan, &mut e) {
                            message.push_str(&plan);
                        }
                        Err(e)
                    }
                    Err(e) => Err(self.err(src, e)),
                }
            })
            .await?;
        Ok(json!({ "columns": table.columns, "rows": table.rows, "truncated": table.truncated }))
    }

    pub async fn slow(&self, src: &Source, opts: &SlowOpts) -> Result<Value> {
        let db = src.slow_database(opts);
        let order = match opts.sort {
            SlowSort::Total => "SUM_TIMER_WAIT",
            SlowSort::Avg => "AVG_TIMER_WAIT",
            SlowSort::Max => "MAX_TIMER_WAIT",
            SlowSort::Calls => "COUNT_STAR",
        };
        // 计时器单位是皮秒：/1e9 是毫秒
        let digest_sql = |sample: bool| {
            format!(
                "SELECT SCHEMA_NAME AS `database`, LEFT(DIGEST_TEXT, 2000) AS `query`,{sample} \
                 COUNT_STAR AS calls, ROUND(AVG_TIMER_WAIT / 1e9, 2) AS avg_ms, \
                 ROUND(MAX_TIMER_WAIT / 1e9, 2) AS max_ms, ROUND(SUM_TIMER_WAIT / 1e9) AS total_ms, \
                 ROUND(SUM_ROWS_EXAMINED / COUNT_STAR) AS avg_rows_examined, \
                 ROUND(SUM_ROWS_SENT / COUNT_STAR) AS avg_rows_sent, \
                 SUM_NO_INDEX_USED AS no_index_used, SUM_CREATED_TMP_DISK_TABLES AS tmp_disk_tables, \
                 SUM_ERRORS AS errors, FIRST_SEEN AS first_seen, LAST_SEEN AS last_seen \
                 FROM performance_schema.events_statements_summary_by_digest \
                 WHERE DIGEST_TEXT IS NOT NULL \
                   AND LAST_SEEN >= FROM_UNIXTIME(? / 1000) AND FIRST_SEEN <= FROM_UNIXTIME(? / 1000) \
                   AND AVG_TIMER_WAIT >= ? * 1e9 AND (? = '' OR SCHEMA_NAME = ?) \
                 ORDER BY {order} DESC LIMIT {limit}",
                sample = if sample { " LEFT(QUERY_SAMPLE_TEXT, 2000) AS sample," } else { "" },
                limit = opts.limit,
            )
        };
        let db_s = db.clone().unwrap_or_default();
        let params = || {
            Params::Positional(vec![
                opts.from_ms.into(),
                opts.to_ms.into(),
                opts.min_ms.into(),
                db_s.clone().into(),
                db_s.clone().into(),
            ])
        };
        self.with_session(src, None, opts.limit, async |s| {
            let mut notes: Vec<String> = Vec::new();
            // QUERY_SAMPLE_TEXT 是 8.0 才有的列，5.7 上报 1054，去掉它再查一次
            let digests = match s.fetch(&digest_sql(true), params(), opts.limit).await {
                Err(mysql_async::Error::Server(se)) if se.code == 1054 => {
                    s.fetch(&digest_sql(false), params(), opts.limit).await
                }
                other => other,
            };
            let mut digest_error = None;
            let digests = match digests {
                Ok(t) if !t.rows.is_empty() => Some(t),
                Ok(_) => None,
                Err(e) => {
                    digest_error = Some(self.err(src, e).to_string());
                    None
                }
            };
            // 没有语句摘要（阿里云 RDS 5.7 默认关着 performance_schema）时退到慢查询日志表：
            // log_output 含 TABLE 时，超过 long_query_time 的语句逐条记在 mysql.slow_log 里
            let mut slow_log = None;
            if digests.is_none() {
                let output = s
                    .fetch_text("SELECT @@global.log_output, @@global.slow_query_log, @@global.long_query_time", 1)
                    .await
                    .ok()
                    .and_then(|t| t.rows.into_iter().next());
                let text = |i: usize| {
                    output.as_ref().and_then(|r| r.get(i)).map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                };
                let to_table = text(0).is_some_and(|o| o.to_ascii_uppercase().contains("TABLE"));
                let enabled = text(1).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("ON"));
                if to_table && enabled {
                    let stmt = format!(
                        "SELECT start_time, db AS `database`, \
                         ROUND(TIME_TO_SEC(query_time) * 1000 + MICROSECOND(query_time) / 1000, 1) AS duration_ms, \
                         ROUND(TIME_TO_SEC(lock_time) * 1000 + MICROSECOND(lock_time) / 1000, 1) AS lock_ms, \
                         rows_sent, rows_examined, user_host, \
                         LEFT(CONVERT(sql_text USING utf8mb4), 2000) AS `query` \
                         FROM mysql.slow_log \
                         WHERE start_time >= FROM_UNIXTIME(? / 1000) AND start_time <= FROM_UNIXTIME(? / 1000) \
                           AND TIME_TO_SEC(query_time) * 1000 + MICROSECOND(query_time) / 1000 >= ? \
                           AND (? = '' OR db = ?) \
                         ORDER BY query_time DESC LIMIT {}",
                        opts.limit
                    );
                    match s.fetch(&stmt, params(), opts.limit).await {
                        Ok(t) => {
                            notes.push(format!(
                                "performance_schema 没有可用的语句摘要，改读慢查询日志 mysql.slow_log：逐条记录耗时超过 long_query_time（当前 {} 秒）的语句，按耗时排序；start_time 为数据库服务器的本地时间",
                                text(2).unwrap_or_default()
                            ));
                            slow_log = Some(t);
                        }
                        Err(e) => notes.push(format!("读取 mysql.slow_log 失败: {}", self.err(src, e))),
                    }
                }
            }
            if digests.is_none() && slow_log.is_none() {
                notes.push(match digest_error {
                    Some(e) => format!(
                        "读取 performance_schema 失败（需要 performance_schema 开启且账号有 SELECT 权限）: {e}"
                    ),
                    None => "语句摘要为空：performance_schema 可能未开启（SHOW VARIABLES LIKE 'performance_schema'），或这段时间里没有达到 min_ms 的语句；慢查询日志也没有写在表里（log_output 不含 TABLE）".to_owned(),
                });
            }
            let running = s
                .fetch(
                    "SELECT ID AS id, USER AS user, DB AS `database`, COMMAND AS command, TIME AS seconds, \
                     STATE AS state, LEFT(INFO, 2000) AS `query` \
                     FROM information_schema.PROCESSLIST \
                     WHERE COMMAND NOT IN ('Sleep', 'Daemon', 'Binlog Dump', 'Binlog Dump GTID') \
                       AND ID <> CONNECTION_ID() AND TIME >= ? \
                     ORDER BY TIME DESC LIMIT 50",
                    Params::Positional(vec![(opts.min_ms / 1000.0).floor().into()]),
                    50,
                )
                .await
                .map_err(|e| self.err(src, e))?;
            if digests.is_some() {
                notes.push("digests 是 performance_schema 自上次重置以来的累计值（按 LAST_SEEN 落在时间范围内筛选），时间为数据库服务器的本地时间".to_owned());
            }
            notes.push("running 需要 PROCESS 权限才能看到其他账号的会话".to_owned());
            let mut out = json!({ "running": running, "notes": notes });
            if let Some(d) = digests {
                out["digests"] = json!(d);
            }
            if let Some(l) = slow_log {
                out["slow_log"] = json!(l);
            }
            Ok(out)
        })
        .await
    }
}

struct Session {
    conn: Conn,
}

/// 超时语句的执行计划摘要，拼在错误信息后面。只对 SELECT / WITH 做（EXPLAIN 本身超时、
/// SHOW 没有计划）；EXPLAIN 也失败就什么都不给，不能让补充信息盖掉原来的错误。
async fn explain_hint(s: &mut Session, stmt: &str) -> Option<String> {
    let head = stmt.trim_start().get(..6)?.to_ascii_uppercase();
    if !(head.starts_with("SELECT") || head.starts_with("WITH")) {
        return None;
    }
    let plan = s.fetch_text(&format!("EXPLAIN {stmt}"), 20).await.ok()?;
    let col = |row: &[Value], name: &str| -> String {
        match plan.col(name).and_then(|i| row.get(i)) {
            Some(Value::Null) | None => String::new(),
            Some(Value::String(v)) => v.clone(),
            Some(v) => v.to_string(),
        }
    };
    let mut lines = Vec::new();
    // 整表扫描：type=ALL 是扫全表，type=index 是把整棵索引从头扫到尾（覆盖索引时常见），
    // 行数一样多。线上那次字符串主键拿数字比较，计划就是 type=index、rows≈400 万，而且
    // MySQL 把 possible_keys 置空了——类型不一致时它认为索引根本用不上，所以不能靠
    // 「有可用索引却没用」来认
    let mut full_scan = false;
    for row in &plan.rows {
        let (table, ty, key) = (col(row, "table"), col(row, "type"), col(row, "key"));
        if ty == "ALL" || ty == "index" {
            full_scan = true;
        }
        lines.push(format!(
            "{table}：type={ty}，key={}，rows≈{}{}",
            if key.is_empty() { "无" } else { &key },
            col(row, "rows"),
            match col(row, "Extra") {
                x if x.is_empty() => String::new(),
                x => format!("，{x}"),
            }
        ));
    }
    let total = lines.len();
    lines.truncate(8);
    let mut out = format!("\n执行计划（EXPLAIN）：{}", lines.join("；"));
    if total > 8 {
        out.push_str(&format!("；……共 {total} 步"));
    }
    if full_scan {
        out.push_str(
            "\ntype=ALL / index 是整表扫描。常见原因：没有命中索引的条件；比较值与列类型不一致\
             （字符串列拿数字比较，如 varchar 的 id 写成 IN (123) 而不是 IN ('123')）；对索引列套了函数。\
             先 db_describe 看列类型",
        );
    }
    Some(out)
}

impl Session {
    /// 预处理语句（二进制协议），给我们自己写的、要绑参数的 SQL 用。
    async fn fetch(
        &mut self,
        stmt: &str,
        params: Params,
        limit: u32,
    ) -> std::result::Result<Table, mysql_async::Error> {
        let mut result = self.conn.exec_iter(stmt, params).await?;
        collect(&mut result, limit).await
    }

    /// 文本协议，给模型写的 SQL 用：`SHOW` / `EXPLAIN` 这些预处理语句不一定支持。
    async fn fetch_text(
        &mut self,
        stmt: &str,
        limit: u32,
    ) -> std::result::Result<Table, mysql_async::Error> {
        let mut result = self.conn.query_iter(stmt).await?;
        collect(&mut result, limit).await
    }
}

async fn collect<P: mysql_async::prelude::Protocol>(
    result: &mut mysql_async::QueryResult<'_, '_, P>,
    limit: u32,
) -> std::result::Result<Table, mysql_async::Error> {
    let cols: Vec<(String, ColumnType)> =
        result.columns_ref().iter().map(|c| (c.name_str().into_owned(), c.column_type())).collect();
    let mut table =
        Table { columns: cols.iter().map(|(n, _)| n.clone()).collect(), ..Table::default() };
    while let Some(row) = result.next().await? {
        if table.rows.len() >= limit as usize {
            table.truncated = true;
            break;
        }
        let values = row.unwrap();
        table.rows.push(
            values
                .into_iter()
                .enumerate()
                .map(|(i, v)| to_json(v, cols.get(i).map(|c| c.1)))
                .collect(),
        );
    }
    // 截断时剩下的行 mysql_async 会在下一次用这条连接前读掉；显式读掉，免得 ROLLBACK 排在后面
    while result.next().await?.is_some() {}
    Ok(table)
}

/// MySQL 的值 → JSON。文本协议下所有值都是字节串，按列类型把整数、浮点数还原成数字；
/// DECIMAL 保留字符串，免得金额在 f64 里丢精度。
fn to_json(v: mysql_async::Value, ty: Option<ColumnType>) -> Value {
    use ColumnType::*;
    use mysql_async::Value as V;
    match v {
        V::NULL => Value::Null,
        V::Int(i) => json!(i),
        V::UInt(u) => json!(u),
        V::Float(f) => json!(f),
        V::Double(f) => json!(f),
        V::Date(y, mo, d, h, mi, s, us) => {
            let date = format!("{y:04}-{mo:02}-{d:02}");
            if ty == Some(MYSQL_TYPE_DATE) {
                json!(date)
            } else if us > 0 {
                json!(format!("{date} {h:02}:{mi:02}:{s:02}.{us:06}"))
            } else {
                json!(format!("{date} {h:02}:{mi:02}:{s:02}"))
            }
        }
        V::Time(neg, days, h, mi, s, us) => {
            let hours = u64::from(days) * 24 + u64::from(h);
            let sign = if neg { "-" } else { "" };
            if us > 0 {
                json!(format!("{sign}{hours:02}:{mi:02}:{s:02}.{us:06}"))
            } else {
                json!(format!("{sign}{hours:02}:{mi:02}:{s:02}"))
            }
        }
        V::Bytes(b) => {
            let numeric = |s: &str| -> Option<Value> {
                match ty? {
                    MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG
                    | MYSQL_TYPE_INT24 | MYSQL_TYPE_YEAR => s
                        .parse::<i64>()
                        .map(|i| json!(i))
                        .or_else(|_| s.parse::<u64>().map(|u| json!(u)))
                        .ok(),
                    MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
                        s.parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| json!(f))
                    }
                    _ => None,
                }
            };
            match std::str::from_utf8(&b) {
                Ok(s) => numeric(s).unwrap_or_else(|| Value::String(s.to_owned())),
                Err(_) => bytes_to_json(&b),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::Value as V;

    #[test]
    fn values_become_json_by_column_type() {
        assert_eq!(to_json(V::Bytes(b"42".to_vec()), Some(ColumnType::MYSQL_TYPE_LONG)), json!(42));
        assert_eq!(
            to_json(
                V::Bytes(b"18446744073709551615".to_vec()),
                Some(ColumnType::MYSQL_TYPE_LONGLONG)
            ),
            json!(u64::MAX)
        );
        assert_eq!(
            to_json(V::Bytes(b"12.50".to_vec()), Some(ColumnType::MYSQL_TYPE_NEWDECIMAL)),
            json!("12.50"),
            "DECIMAL 保留字符串"
        );
        assert_eq!(
            to_json(V::Bytes(b"1.5".to_vec()), Some(ColumnType::MYSQL_TYPE_DOUBLE)),
            json!(1.5)
        );
        assert_eq!(
            to_json(V::Bytes(b"abc".to_vec()), Some(ColumnType::MYSQL_TYPE_VAR_STRING)),
            json!("abc")
        );
        assert_eq!(to_json(V::NULL, None), Value::Null);
        assert_eq!(
            to_json(V::Date(2026, 9, 24, 10, 5, 3, 0), Some(ColumnType::MYSQL_TYPE_DATETIME)),
            json!("2026-09-24 10:05:03")
        );
        assert_eq!(
            to_json(V::Date(2026, 9, 24, 0, 0, 0, 0), Some(ColumnType::MYSQL_TYPE_DATE)),
            json!("2026-09-24")
        );
        assert_eq!(to_json(V::Time(true, 1, 2, 3, 4, 0), None), json!("-26:03:04"));
        assert!(to_json(V::Bytes(vec![0xff, 0xfe]), None).as_str().unwrap().starts_with("0xfffe"));
    }

    #[test]
    fn connection_string_overrides() {
        let r = Resolved {
            url: "mysql://reader@10.0.0.5:3307/shop".into(),
            user: None,
            password: Some("p:@/x".into()),
            api_key: None,
            database: Some("shop".into()),
            timeout: std::time::Duration::from_secs(5),
            cluster: None,
        };
        let m = Mysql::new(&r).unwrap();
        assert_eq!(m.address, "10.0.0.5:3307");
        assert_eq!(m.opts.pass(), Some("p:@/x"), "密码里的特殊字符不用 URL 转义");
        assert_eq!(m.opts.user(), Some("reader"));
        assert_eq!(m.opts.db_name(), Some("shop"));
    }
}
