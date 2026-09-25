//! ClickHouse 数据源：业务自己的 ClickHouse（不是 opdash 读日志 / 链路的那一套，虽然也可以配成同一个）。
//!
//! 复用 [`crate::clickhouse::Client`]：HTTP 接口、`readonly=2`、`max_execution_time` 这些都是现成的。
//! 结果用 `JSONCompact` 取，列顺序和 SQL 里写的一致（`JSONEachRow` 解成对象会丢顺序）。

use serde_json::{Value, json};

use super::sql::{self, Dialect};
use super::{Ctx, QueryOpts, Resolved, SlowOpts, SlowSort, Source, Table};
use crate::clickhouse::{Client, ClientOptions, Query};
use crate::error::{Error, Result, ch_code, trim_ch_message};

/// 单次查询结果最多多少字节。`result_overflow_mode=break` 按块截断，行数上限只能粗略地管住
/// 行数，宽表的一块也可能很大，字节再兜一层。
const MAX_RESULT_BYTES: u64 = 32 << 20;

pub struct Ch {
    client: Client,
    cluster: Option<String>,
}

impl Ch {
    pub fn new(r: &Resolved) -> std::result::Result<Self, String> {
        let client = Client::new(ClientOptions {
            endpoint: r.url.clone(),
            user: r.user.clone().unwrap_or_else(|| "default".to_owned()),
            password: r.password.clone().unwrap_or_default(),
            timeout: r.timeout,
            max_read_bytes: 0,
            max_read_rows: 0,
            max_concurrent: 4,
        })
        .map_err(|e| e.to_string())?;
        if let Some(c) = &r.cluster
            && !crate::config::is_plain_identifier(c)
        {
            return Err(format!("cluster 只能包含字母、数字、下划线：{c:?}"));
        }
        Ok(Self { client, cluster: r.cluster.clone() })
    }

    /// ClickHouse 的错误换成数据源错误：超时 504、连不上 502，其余（语法错、没权限、表不存在）400。
    fn err(src: &Source, e: Error) -> Error {
        match e {
            Error::ClickHouse { code, message } => {
                let status = match code {
                    ch_code::TIMEOUT_EXCEEDED | ch_code::TOO_SLOW | ch_code::SOCKET_TIMEOUT => 504,
                    ch_code::AUTHENTICATION_FAILED | ch_code::REQUIRED_PASSWORD => 502,
                    _ => 400,
                };
                let hint = match status {
                    504 => "。建议：添加命中排序键的 WHERE 条件、缩小时间范围或添加 LIMIT",
                    _ if code == ch_code::READONLY => "。数据源只允许只读查询",
                    _ => "",
                };
                Error::Source {
                    status,
                    message: format!("ClickHouse 错误 {code}：{}{hint}", trim_ch_message(&message)),
                }
            }
            Error::Unavailable(m) => Error::Source {
                status: 502,
                message: format!("ClickHouse 数据源 {} 不可用：{m}", src.name),
            },
            other => other,
        }
    }

    /// 执行一条查询，按 `JSONCompact` 解成表。
    async fn fetch(
        &self,
        src: &Source,
        query: Query,
        database: Option<&str>,
        limit: u32,
    ) -> Result<Table> {
        let mut query = query
            .setting("max_result_rows", u64::from(limit) + 1)
            .setting("max_result_bytes", MAX_RESULT_BYTES)
            .setting("result_overflow_mode", "break");
        if let Some(db) = database.or(src.database.as_deref()) {
            // HTTP 接口的 database 参数：没写库名的表从这个库找
            query = query.setting("database", db);
        }
        let resp =
            self.client.send(&query, Some("JSONCompact")).await.map_err(|e| Self::err(src, e))?;
        let body = resp.bytes().await.map_err(|e| Self::err(src, e.into()))?;
        let v: Value = serde_json::from_slice(&body)
            .map_err(|e| Error::internal(format!("ClickHouse 的 JSONCompact 结果解析失败：{e}")))?;
        let columns: Vec<String> = v["meta"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|m| m["name"].as_str().unwrap_or("").to_owned())
            .collect();
        let mut rows: Vec<Vec<Value>> = v["data"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|r| r.as_array().cloned().unwrap_or_default())
            .collect();
        let truncated = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        Ok(Table { columns, rows, truncated })
    }

    /// 和 MySQL 一样分三种情况：没指定库也没给关键字时只列库（带表数），给了关键字跨库搜，
    /// 指定了库列这个库的表。账号的默认库往往是空的 `default`，业务表都在别的库里，
    /// 默认只看 `currentDatabase()` 会得到一个误导人的空列表。
    pub async fn tables(
        &self,
        src: &Source,
        database: Option<&str>,
        pattern: Option<&str>,
        limit: u32,
    ) -> Result<Value> {
        const SYSTEM: &str = "('system', 'INFORMATION_SCHEMA', 'information_schema')";
        let db = database.or(src.database.as_deref()).unwrap_or("").to_owned();
        let like = pattern.map(sql::like_contains).unwrap_or_else(|| "%".to_owned());
        let (key, q, note) = if db.is_empty() && pattern.is_none() {
            (
                "databases",
                Query::new(format!(
                    "SELECT database, count() AS tables, sum(total_rows) AS total_rows,\n  \
                     formatReadableSize(sum(total_bytes)) AS size\n\
                     FROM system.tables\nWHERE database NOT IN {SYSTEM} AND NOT is_temporary\n\
                     GROUP BY database\nORDER BY database\nLIMIT {{limit:UInt32}}"
                )),
                Some(
                    "未指定库，仅列出了库名；指定 database 可列出该库的表，指定 match 可跨库按表名搜索",
                ),
            )
        } else if db.is_empty() {
            (
                "tables",
                Query::new(format!(
                    "SELECT database, name AS table, engine, total_rows, formatReadableSize(total_bytes) AS size, comment\n\
                     FROM system.tables\n\
                     WHERE database NOT IN {SYSTEM} AND name ILIKE {{like:String}} AND NOT is_temporary\n\
                     ORDER BY database, name\nLIMIT {{limit:UInt32}}"
                ))
                .param("like", &like),
                None,
            )
        } else {
            (
                "tables",
                Query::new(
                    "SELECT name AS table, engine, total_rows, formatReadableSize(total_bytes) AS size, comment\n\
                     FROM system.tables\n\
                     WHERE database = {db:String} AND name ILIKE {like:String} AND NOT is_temporary\n\
                     ORDER BY name\nLIMIT {limit:UInt32}",
                )
                .param("db", &db)
                .param("like", &like),
                None,
            )
        };
        let table = self.fetch(src, q.param("limit", limit + 1), None, limit).await?;
        let mut out = json!({ key: table });
        if let Some(n) = note {
            out["note"] = json!(n);
        }
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
            Some(d) => format!("{}.{}", sql::quote_ident(d), sql::quote_ident(&table)),
            None => sql::quote_ident(&table),
        };
        let create =
            self.fetch(src, Query::new(format!("SHOW CREATE TABLE {qualified}")), None, 1).await?;
        let stats = self
            .fetch(
                src,
                Query::new(
                    "SELECT total_rows, formatReadableSize(total_bytes) AS size, engine, sorting_key, partition_key\n\
                     FROM system.tables\n\
                     WHERE database = if({db:String} = '', currentDatabase(), {db:String}) AND name = {t:String}",
                )
                .param("db", db.as_deref().unwrap_or(""))
                .param("t", &table),
                None,
                1,
            )
            .await?;
        let mut out = json!({
            "table": target,
            "create_table": create.rows.first().and_then(|r| r.first()).cloned().unwrap_or(Value::Null),
        });
        if let Some(row) = stats.rows.first() {
            for (c, v) in stats.columns.iter().zip(row) {
                out[c.as_str()] = v.clone();
            }
        }
        Ok(out)
    }

    pub async fn query(&self, src: &Source, query: &str, opts: &QueryOpts) -> Result<Value> {
        let stmt = sql::check_read_only(query, Dialect::ClickHouse).map_err(Error::bad_request)?;
        let table = self.fetch(src, Query::new(stmt), opts.database.as_deref(), opts.limit).await?;
        Ok(json!({ "columns": table.columns, "rows": table.rows, "truncated": table.truncated }))
    }

    pub async fn slow(&self, src: &Source, opts: &SlowOpts, ctx: Ctx) -> Result<Value> {
        let db = src.slow_database(opts);
        // 集群上 query_log 是每个节点各记各的，发起查询的那台才有记录；配了 cluster 就跨所有副本读
        let from = match &self.cluster {
            Some(c) => format!("clusterAllReplicas('{c}', system.query_log)"),
            None => "system.query_log".to_owned(),
        };
        let order = match opts.sort {
            SlowSort::Total => "total_ms",
            SlowSort::Avg => "avg_ms",
            SlowSort::Max => "max_ms",
            SlowSort::Calls => "calls",
        };
        let q = Query::new(format!(
            "SELECT any(substring(query, 1, 2000)) AS sample, count() AS calls,\n  \
             countIf(type != 'QueryFinish') AS errors,\n  \
             round(avg(query_duration_ms)) AS avg_ms, quantile(0.95)(query_duration_ms) AS p95_ms,\n  \
             max(query_duration_ms) AS max_ms, sum(query_duration_ms) AS total_ms,\n  \
             sum(read_rows) AS read_rows, formatReadableSize(sum(read_bytes)) AS read_bytes,\n  \
             formatReadableSize(max(memory_usage)) AS max_memory, any(user) AS user,\n  \
             formatDateTime(max(event_time), '%Y-%m-%d %H:%i:%S', {{tz:String}}) AS last_seen\n\
             FROM {from}\n\
             WHERE event_date >= toDate(fromUnixTimestamp64Milli({{from_ms:Int64}}))\n  \
               AND event_time >= fromUnixTimestamp64Milli({{from_ms:Int64}})\n  \
               AND event_time <= fromUnixTimestamp64Milli({{to_ms:Int64}})\n  \
               AND type IN ('QueryFinish', 'ExceptionWhileProcessing')\n  \
               AND query_kind = 'Select' AND query_duration_ms >= {{min_ms:Float64}}\n  \
               AND ({{db:String}} = '' OR has(databases, {{db:String}}))\n\
             GROUP BY normalized_query_hash\n\
             ORDER BY {order} DESC\nLIMIT {{limit:UInt32}}"
        ))
        .param("tz", ctx.tz.name())
        .param("from_ms", opts.from_ms)
        .param("to_ms", opts.to_ms)
        .param("min_ms", opts.min_ms)
        .param("db", db.as_deref().unwrap_or(""))
        .param("limit", opts.limit);
        let table = self.fetch(src, q, None, opts.limit).await?;
        let mut notes = vec![
            "按 normalized_query_hash 归并（同一条语句换了参数算一种），sample 是其中一条原文；只统计 SELECT",
        ];
        if self.cluster.is_none() {
            notes.push("未配置 cluster：集群部署时，仅读取到了处理本次请求的单个节点的 query_log");
        }
        Ok(json!({ "queries": table, "notes": notes }))
    }
}
