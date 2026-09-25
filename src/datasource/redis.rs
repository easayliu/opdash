//! Redis 数据源。
//!
//! Redis 没有只读事务，只读只能靠命令白名单：[`READ_COMMANDS`] 之外的一律拒绝，带子命令的
//! （`OBJECT` / `MEMORY` / `SLOWLOG` / `XINFO` / `CLIENT`）连子命令一起查。`KEYS` 不在白名单里：
//! 它在大库上会把 Redis 阻塞几秒，列键请用 SCAN（`db_tables`）。
//!
//! 一次取整个集合的命令（`HGETALL` / `SMEMBERS` / `LRANGE 0 -1` …）先看一眼集合有多大，
//! 超过 [`MAX_COLLECTION`] 就拒绝并建议改用 `*SCAN` 或缩小区间——一个百万成员的 hash 取回来，
//! 既拖慢 Redis，也会把结果撑到模型根本读不完。
//!
//! 每次操作单独建一条连接：排障时的调用频率很低，不值得维护一个会断、会过期的长连接。

use redis::aio::MultiplexedConnection;
use redis::{AsyncConnectionConfig, Client, ConnectionInfo, IntoConnectionInfo, Value as RValue};
use serde_json::{Value, json};

use super::{Ctx, Resolved, SlowOpts, Source, Table, bytes_to_json, clip};
use crate::error::{Error, Result};

/// 一次最多取回多少个集合成员。
const MAX_COLLECTION: i64 = 1000;
/// 列键时 SCAN 最多翻几轮（每轮 COUNT 1000）。大库里一个少见的前缀可能翻完也凑不够 limit，
/// 不能为了凑数把整个库扫一遍。
const MAX_SCAN_ROUNDS: usize = 50;
/// 单个字符串值给模型看多少字符。
const MAX_VALUE_CHARS: usize = 4000;

/// 放行的只读命令。值是「需要检查子命令」时允许的子命令，空表示不看子命令。
const READ_COMMANDS: &[(&str, &[&str])] = &[
    ("GET", &[]),
    ("MGET", &[]),
    ("STRLEN", &[]),
    ("GETRANGE", &[]),
    ("EXISTS", &[]),
    ("TYPE", &[]),
    ("TTL", &[]),
    ("PTTL", &[]),
    ("EXPIRETIME", &[]),
    ("PEXPIRETIME", &[]),
    ("OBJECT", &["ENCODING", "FREQ", "IDLETIME", "REFCOUNT"]),
    ("MEMORY", &["USAGE", "STATS"]),
    ("HGET", &[]),
    ("HMGET", &[]),
    ("HGETALL", &[]),
    ("HKEYS", &[]),
    ("HVALS", &[]),
    ("HLEN", &[]),
    ("HEXISTS", &[]),
    ("HSTRLEN", &[]),
    ("HSCAN", &[]),
    ("LRANGE", &[]),
    ("LLEN", &[]),
    ("LINDEX", &[]),
    ("LPOS", &[]),
    ("SMEMBERS", &[]),
    ("SCARD", &[]),
    ("SISMEMBER", &[]),
    ("SMISMEMBER", &[]),
    ("SSCAN", &[]),
    ("ZRANGE", &[]),
    ("ZREVRANGE", &[]),
    ("ZRANGEBYSCORE", &[]),
    ("ZREVRANGEBYSCORE", &[]),
    ("ZRANGEBYLEX", &[]),
    ("ZSCORE", &[]),
    ("ZMSCORE", &[]),
    ("ZCARD", &[]),
    ("ZCOUNT", &[]),
    ("ZRANK", &[]),
    ("ZREVRANK", &[]),
    ("ZSCAN", &[]),
    ("XRANGE", &[]),
    ("XREVRANGE", &[]),
    ("XLEN", &[]),
    ("XINFO", &["STREAM", "GROUPS", "CONSUMERS"]),
    ("XPENDING", &[]),
    ("BITCOUNT", &[]),
    ("GETBIT", &[]),
    ("PFCOUNT", &[]),
    ("SCAN", &[]),
    ("DBSIZE", &[]),
    ("INFO", &[]),
    ("SLOWLOG", &["GET", "LEN"]),
    ("CLIENT", &["LIST", "INFO"]),
];

pub struct Redis {
    /// 连接信息（含默认库号）。按次指定库号时从它改出一份新的，见 [`Redis::conn`]
    info: ConnectionInfo,
    address: String,
}

impl Redis {
    pub fn new(r: &Resolved) -> std::result::Result<Self, String> {
        let mut info = r
            .url
            .as_str()
            .into_connection_info()
            .map_err(|e| format!("Redis 连接串无法解析：{e}"))?;
        let mut settings = info.redis_settings().clone();
        if let Some(u) = &r.user {
            settings = settings.set_username(u);
        }
        if let Some(p) = &r.password {
            settings = settings.set_password(p);
        }
        if let Some(db) = &r.database {
            let n: i64 =
                db.parse().map_err(|_| format!("Redis 的 database 应是库号，不是 {db:?}"))?;
            settings = settings.set_db(n);
        }
        info = info.set_redis_settings(settings);
        let address = info.addr().to_string();
        Client::open(info.clone()).map_err(|e| format!("Redis 连接串无效：{e}"))?;
        Ok(Self { info, address })
    }

    /// 建一条连接。`db` 给了就连到这个库号，否则用配置里的默认库号。
    ///
    /// 业务上常把不同用途的数据分到不同库号里（线上一个实例用了十几个），`SELECT` 又不在
    /// 只读白名单里——它会改连接状态，而连接是按次新建的，改了也带不到下一条命令——所以
    /// 库号作为参数随每次调用给出。
    async fn conn(&self, src: &Source, db: Option<&str>) -> Result<MultiplexedConnection> {
        let mut info = self.info.clone();
        if let Some(db) = db.map(str::trim).filter(|d| !d.is_empty()) {
            let n: i64 = db.parse().ok().filter(|n| (0..=1024).contains(n)).ok_or_else(|| {
                Error::bad_request(format!("Redis 的 database 应是库号（0、1、12……），不是 {db:?}"))
            })?;
            info = info.clone().set_redis_settings(info.redis_settings().clone().set_db(n));
        }
        let client = Client::open(info).map_err(|e| self.err(src, e))?;
        let cfg = AsyncConnectionConfig::new()
            .set_connection_timeout(Some(super::CONNECT_TIMEOUT))
            .set_response_timeout(Some(src.timeout));
        client
            .get_multiplexed_async_connection_with_config(&cfg)
            .await
            .map_err(|e| self.err(src, e))
    }

    fn err(&self, src: &Source, e: redis::RedisError) -> Error {
        if e.is_timeout() {
            return Error::Source {
                status: 504,
                message: format!(
                    "Redis 数据源 {} 超过 {} 秒没有响应",
                    src.name,
                    src.timeout.as_secs()
                ),
            };
        }
        if e.is_io_error() || e.is_connection_refusal() {
            return Error::Source {
                status: 502,
                message: format!("Redis 数据源 {}（{}）不可用：{e}", src.name, self.address),
            };
        }
        Error::Source { status: 400, message: format!("Redis 错误：{e}") }
    }

    async fn run(
        &self,
        src: &Source,
        conn: &mut MultiplexedConnection,
        args: &[String],
    ) -> Result<RValue> {
        let mut cmd = redis::cmd(&args[0]);
        for a in &args[1..] {
            cmd.arg(a);
        }
        cmd.query_async::<RValue>(conn).await.map_err(|e| self.err(src, e))
    }

    pub async fn tables(
        &self,
        src: &Source,
        database: Option<&str>,
        pattern: Option<&str>,
        limit: u32,
    ) -> Result<Value> {
        let pattern = pattern.filter(|p| !p.is_empty()).unwrap_or("*");
        let mut conn = self.conn(src, database).await?;
        let mut keys: Vec<String> = Vec::new();
        let mut cursor = "0".to_owned();
        let mut rounds = 0;
        loop {
            let reply = self
                .run(
                    src,
                    &mut conn,
                    &[
                        "SCAN".into(),
                        cursor.clone(),
                        "MATCH".into(),
                        pattern.into(),
                        "COUNT".into(),
                        "1000".into(),
                    ],
                )
                .await?;
            rounds += 1;
            let RValue::Array(parts) = reply else {
                return Err(Error::internal("SCAN 的返回不是数组"));
            };
            cursor = parts.first().map(value_text).unwrap_or_else(|| "0".into());
            if let Some(RValue::Array(batch)) = parts.get(1) {
                keys.extend(batch.iter().map(value_text));
            }
            if cursor == "0" || keys.len() > limit as usize || rounds >= MAX_SCAN_ROUNDS {
                break;
            }
        }
        let truncated = keys.len() > limit as usize;
        keys.truncate(limit as usize);
        // 每个键的类型和 TTL 用一次管道取回来，不是一个键一趟
        let mut pipe = redis::pipe();
        for k in &keys {
            pipe.cmd("TYPE").arg(k).cmd("PTTL").arg(k);
        }
        let meta: Vec<RValue> = if keys.is_empty() {
            Vec::new()
        } else {
            pipe.query_async(&mut conn).await.map_err(|e| self.err(src, e))?
        };
        let mut table = Table::new(&["key", "type", "ttl_s"]);
        for (i, k) in keys.iter().enumerate() {
            let ty = meta.get(i * 2).map(value_text).unwrap_or_default();
            let ttl = match meta.get(i * 2 + 1) {
                Some(RValue::Int(ms)) if *ms >= 0 => json!((*ms as f64 / 100.0).round() / 10.0),
                _ => Value::Null,
            };
            table.rows.push(vec![json!(k), json!(ty), ttl]);
        }
        table.truncated = truncated;
        let dbsize = self.run(src, &mut conn, &["DBSIZE".into()]).await.ok().map(|v| to_json(&v));
        // 各库号有多少键：当前库是空的、数据在别的库号里时，模型看这个就知道该给哪个 database
        let keyspace = match self.run(src, &mut conn, &["INFO".into(), "keyspace".into()]).await {
            Ok(v) => parse_keyspace(&value_text(&v)),
            Err(_) => Table::new(&["database", "keys", "expires"]),
        };
        let current = database
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| self.info.redis_settings().db().to_string());
        let mut out = json!({
            "database": current,
            "pattern": pattern,
            "keys": table,
            "dbsize": dbsize,
            "keyspace": keyspace,
        });
        if cursor != "0" && !truncated {
            out["note"] = json!(format!(
                "SCAN 执行 {rounds} 轮后仍未遍历整个库，仅列出了已扫描到的键；请改用更具体的 match（如 order:123:*）"
            ));
        }
        Ok(out)
    }

    pub async fn describe(&self, src: &Source, key: &str, database: Option<&str>) -> Result<Value> {
        let mut conn = self.conn(src, database).await?;
        let key_s = key.to_owned();
        let ty = value_text(&self.run(src, &mut conn, &["TYPE".into(), key_s.clone()]).await?);
        if ty == "none" {
            return Err(Error::bad_request(format!("键 {key:?} 不存在（或已过期）")));
        }
        let pttl = self.run(src, &mut conn, &["PTTL".into(), key_s.clone()]).await?;
        // 代理 / 云上的 Redis 可能不支持 MEMORY，拿不到就不给
        let memory = self
            .run(src, &mut conn, &["MEMORY".into(), "USAGE".into(), key_s.clone()])
            .await
            .ok()
            .map(|v| to_json(&v));
        let (len_cmd, sample_cmd): (&str, Vec<String>) = match ty.as_str() {
            "string" => {
                ("STRLEN", vec!["GETRANGE".into(), key_s.clone(), "0".into(), "3999".into()])
            }
            "list" => ("LLEN", vec!["LRANGE".into(), key_s.clone(), "0".into(), "19".into()]),
            "hash" => (
                "HLEN",
                vec!["HSCAN".into(), key_s.clone(), "0".into(), "COUNT".into(), "20".into()],
            ),
            "set" => (
                "SCARD",
                vec!["SSCAN".into(), key_s.clone(), "0".into(), "COUNT".into(), "20".into()],
            ),
            "zset" => (
                "ZCARD",
                vec!["ZRANGE".into(), key_s.clone(), "0".into(), "19".into(), "WITHSCORES".into()],
            ),
            "stream" => (
                "XLEN",
                vec![
                    "XREVRANGE".into(),
                    key_s.clone(),
                    "+".into(),
                    "-".into(),
                    "COUNT".into(),
                    "5".into(),
                ],
            ),
            _ => ("", Vec::new()),
        };
        let len = if len_cmd.is_empty() {
            Value::Null
        } else {
            to_json(&self.run(src, &mut conn, &[len_cmd.into(), key_s.clone()]).await?)
        };
        let sample = if sample_cmd.is_empty() {
            Value::Null
        } else {
            let v = self.run(src, &mut conn, &sample_cmd).await?;
            // *SCAN 回的是 [游标, 成员]，只要成员
            match (&v, sample_cmd[0].ends_with("SCAN")) {
                (RValue::Array(parts), true) if parts.len() == 2 => to_json(&parts[1]),
                _ => to_json(&v),
            }
        };
        let ttl = match pttl {
            RValue::Int(ms) if ms >= 0 => json!((ms as f64 / 100.0).round() / 10.0),
            _ => Value::Null,
        };
        Ok(json!({
            "key": key,
            "type": ty,
            "ttl_s": ttl,
            "length": len,
            "memory_bytes": memory,
            "sample": sample,
            "sample_command": sample_cmd.join(" "),
        }))
    }

    pub async fn query(&self, src: &Source, line: &str, database: Option<&str>) -> Result<Value> {
        let args = split_command(line).map_err(Error::bad_request)?;
        check_command(&args).map_err(Error::bad_request)?;
        let mut conn = self.conn(src, database).await?;
        guard_size(self, src, &mut conn, &args).await?;
        tracing::debug!(source = %src.name, command = %clip(line, 300), "数据源查询");
        let v = self.run(src, &mut conn, &args).await?;
        Ok(json!({ "command": args, "result": to_json(&v) }))
    }

    pub async fn slow(&self, src: &Source, opts: &SlowOpts, ctx: Ctx) -> Result<Value> {
        // 慢日志和命令统计是整个实例的，与库号无关
        let mut conn = self.conn(src, None).await?;
        let mut notes: Vec<String> = Vec::new();
        let mut slowlog = Table::new(&["id", "time", "duration_ms", "command", "client"]);
        match self.run(src, &mut conn, &["SLOWLOG".into(), "GET".into(), "128".into()]).await {
            Ok(RValue::Array(entries)) => {
                for e in entries {
                    let RValue::Array(f) = e else { continue };
                    let ts = f.get(1).map(value_i64).unwrap_or(0);
                    let us = f.get(2).map(value_i64).unwrap_or(0);
                    let ms = us as f64 / 1000.0;
                    if ts * 1000 < opts.from_ms || ts * 1000 > opts.to_ms || ms < opts.min_ms {
                        continue;
                    }
                    let command = match f.get(3) {
                        Some(RValue::Array(a)) => {
                            a.iter().map(value_text).collect::<Vec<_>>().join(" ")
                        }
                        _ => String::new(),
                    };
                    slowlog.rows.push(vec![
                        f.first().map(to_json).unwrap_or(Value::Null),
                        json!(crate::mcp::fmt_time(ts * 1000, ctx.tz)),
                        json!(ms),
                        json!(clip(&command, 500)),
                        f.get(4).map(to_json).unwrap_or(Value::Null),
                    ]);
                }
                slowlog.rows.sort_by(|a, b| {
                    b[2].as_f64().unwrap_or(0.0).total_cmp(&a[2].as_f64().unwrap_or(0.0))
                });
                if slowlog.rows.len() > opts.limit as usize {
                    slowlog.rows.truncate(opts.limit as usize);
                    slowlog.truncated = true;
                }
            }
            Ok(_) => {}
            Err(e) => notes.push(format!("SLOWLOG 不可用：{e}")),
        }
        // 各命令的累计耗时：看得出是不是某一类命令（KEYS、大 HGETALL）整体在拖慢
        let mut stats = Table::new(&["command", "calls", "usec_per_call", "total_ms"]);
        match self.run(src, &mut conn, &["INFO".into(), "commandstats".into()]).await {
            Ok(v) => {
                let mut rows = parse_commandstats(&value_text(&v));
                rows.sort_by(|a, b| b.3.total_cmp(&a.3));
                for (cmd, calls, per, total) in rows.into_iter().take(15) {
                    stats.rows.push(vec![json!(cmd), json!(calls), json!(per), json!(total)]);
                }
            }
            Err(e) => notes.push(format!("INFO commandstats 不可用：{e}")),
        }
        notes.push("SLOWLOG 只保留最近若干条（slowlog-max-len），耗时不含网络往返；commandstats 是实例启动以来的累计值".to_owned());
        Ok(json!({ "slowlog": slowlog, "command_stats": stats, "notes": notes }))
    }
}

/// 一次取整个集合的命令先看集合多大，太大就拒绝。
async fn guard_size(
    r: &Redis,
    src: &Source,
    conn: &mut MultiplexedConnection,
    args: &[String],
) -> Result<()> {
    let name = args[0].to_ascii_uppercase();
    let Some(key) = args.get(1) else { return Ok(()) };
    let (len_cmd, span) = match name.as_str() {
        "HGETALL" | "HKEYS" | "HVALS" => ("HLEN", None),
        "SMEMBERS" => ("SCARD", None),
        "LRANGE" => ("LLEN", range_span(args)),
        "ZRANGE" | "ZREVRANGE" => ("ZCARD", range_span(args)),
        _ => return Ok(()),
    };
    let len = value_i64(&r.run(src, conn, &[len_cmd.into(), key.clone()]).await?);
    // 区间写死了、而且不大（LRANGE k 0 99），不用管集合多大
    let wanted = match span {
        Some((start, stop)) if start >= 0 && stop >= 0 => stop - start + 1,
        _ => len,
    };
    if wanted > MAX_COLLECTION {
        let hint = match name.as_str() {
            "HGETALL" | "HKEYS" | "HVALS" => format!("HSCAN {key} 0 COUNT 100"),
            "SMEMBERS" => format!("SSCAN {key} 0 COUNT 100"),
            "LRANGE" => format!("LRANGE {key} 0 99"),
            _ => format!("{name} {key} 0 99"),
        };
        return Err(Error::bad_request(format!(
            "{key} 有 {len} 个成员，{name} 一次最多取 {MAX_COLLECTION} 个；请改用 {hint} 分批查看"
        )));
    }
    Ok(())
}

/// `LRANGE key start stop` 的 (start, stop)。
fn range_span(args: &[String]) -> Option<(i64, i64)> {
    Some((args.get(2)?.parse().ok()?, args.get(3)?.parse().ok()?))
}

/// 一行命令切成参数：空白分隔，双引号里支持 `\"` `\\` `\n` `\t`，单引号里原样。
/// 和 redis-cli 的写法一致，模型照着文档写就能用。
pub fn split_command(line: &str) -> std::result::Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut chars = line.trim().chars().peekable();
    while chars.peek().is_some() {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        let mut cur = String::new();
        match first {
            '"' => {
                chars.next();
                loop {
                    match chars.next() {
                        None => return Err("双引号没有闭合".to_owned()),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => cur.push('\n'),
                            Some('t') => cur.push('\t'),
                            Some('r') => cur.push('\r'),
                            Some(c) => cur.push(c),
                            None => return Err("双引号没有闭合".to_owned()),
                        },
                        Some(c) => cur.push(c),
                    }
                }
            }
            '\'' => {
                chars.next();
                loop {
                    match chars.next() {
                        None => return Err("单引号没有闭合".to_owned()),
                        Some('\'') => break,
                        Some(c) => cur.push(c),
                    }
                }
            }
            _ => {
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() {
                        break;
                    }
                    cur.push(c);
                    chars.next();
                }
            }
        }
        args.push(cur);
    }
    if args.is_empty() {
        return Err("命令为空".to_owned());
    }
    Ok(args)
}

/// 命令在不在白名单里。
pub fn check_command(args: &[String]) -> std::result::Result<(), String> {
    let name = args[0].to_ascii_uppercase();
    if name == "KEYS" {
        return Err("不支持 KEYS（大库上会阻塞 Redis）；如需列出键，请使用 db_tables（按 SCAN 分批获取）并指定 match，如 order:*".to_owned());
    }
    let Some((_, subs)) = READ_COMMANDS.iter().find(|(n, _)| *n == name) else {
        return Err(format!(
            "只允许只读命令，不接受 {name}。可用：{}",
            READ_COMMANDS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(" ")
        ));
    };
    if !subs.is_empty() {
        let sub = args.get(1).map(|s| s.to_ascii_uppercase()).unwrap_or_default();
        if !subs.contains(&sub.as_str()) {
            return Err(format!("{name} 只允许这些子命令：{}", subs.join(" / ")));
        }
    }
    Ok(())
}

/// `INFO keyspace` 的 `db12:keys=178599,expires=50847,avg_ttl=…` → 每个库号一行。
fn parse_keyspace(text: &str) -> Table {
    let mut t = Table::new(&["database", "keys", "expires"]);
    for line in text.lines() {
        let Some((db, rest)) = line.trim().strip_prefix("db").and_then(|l| l.split_once(':'))
        else {
            continue;
        };
        let field = |k: &str| {
            rest.split(',')
                .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='))
                .and_then(|v| v.parse::<u64>().ok())
        };
        if let Ok(n) = db.parse::<u32>() {
            t.rows.push(vec![json!(n), json!(field("keys")), json!(field("expires"))]);
        }
    }
    t
}

/// `INFO commandstats` 的 `cmdstat_get:calls=10,usec=35,usec_per_call=3.50,…` →
/// (命令, 次数, 每次微秒, 总毫秒)。
fn parse_commandstats(text: &str) -> Vec<(String, u64, f64, f64)> {
    text.lines()
        .filter_map(|line| {
            let (name, rest) = line.trim().strip_prefix("cmdstat_")?.split_once(':')?;
            let field = |k: &str| {
                rest.split(',')
                    .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='))
                    .map(str::to_owned)
            };
            let calls: u64 = field("calls")?.parse().ok()?;
            let usec: f64 = field("usec")?.parse().ok()?;
            let per: f64 = field("usec_per_call").and_then(|v| v.parse().ok()).unwrap_or(0.0);
            Some((name.to_owned(), calls, per, (usec / 1000.0).round()))
        })
        .collect()
}

fn value_text(v: &RValue) -> String {
    match v {
        RValue::BulkString(b) => String::from_utf8_lossy(b).into_owned(),
        RValue::SimpleString(s) => s.clone(),
        RValue::Int(i) => i.to_string(),
        RValue::Okay => "OK".to_owned(),
        RValue::Double(f) => f.to_string(),
        RValue::VerbatimString { text, .. } => text.clone(),
        other => format!("{other:?}"),
    }
}

fn value_i64(v: &RValue) -> i64 {
    match v {
        RValue::Int(i) => *i,
        other => value_text(other).parse().unwrap_or(0),
    }
}

/// Redis 的返回 → JSON。字符串值超长截断；看着像 JSON 的字符串原样保留成字符串（模型读得懂，
/// 解析了反而丢掉「这就是存在 Redis 里的原文」这层意思）。
pub fn to_json(v: &RValue) -> Value {
    match v {
        RValue::Nil => Value::Null,
        RValue::Int(i) => json!(i),
        RValue::BulkString(b) => match bytes_to_json(b) {
            Value::String(s) => Value::String(clip(&s, MAX_VALUE_CHARS)),
            other => other,
        },
        RValue::Array(items) | RValue::Set(items) => {
            Value::Array(items.iter().map(to_json).collect())
        }
        RValue::SimpleString(s) => json!(s),
        RValue::Okay => json!("OK"),
        RValue::Map(pairs) => {
            Value::Array(pairs.iter().flat_map(|(k, v)| [to_json(k), to_json(v)]).collect())
        }
        RValue::Attribute { data, .. } => to_json(data),
        RValue::Double(f) => json!(f),
        RValue::Boolean(b) => json!(b),
        RValue::VerbatimString { text, .. } => json!(clip(text, MAX_VALUE_CHARS)),
        RValue::BigNumber(n) => json!(format!("{n:?}")),
        RValue::Push { data, .. } => Value::Array(data.iter().map(to_json).collect()),
        RValue::ServerError(e) => json!(format!("ERR {e}")),
        other => json!(format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn splits_like_redis_cli() {
        assert_eq!(split_command("GET user:1").unwrap(), args(&["GET", "user:1"]));
        assert_eq!(
            split_command(r#"HGET "user info" 'a b' "x\"y""#).unwrap(),
            args(&["HGET", "user info", "a b", "x\"y"])
        );
        assert_eq!(split_command("  GET   k  ").unwrap(), args(&["GET", "k"]));
        assert!(split_command("GET \"k").is_err());
        assert!(split_command("   ").is_err());
    }

    #[test]
    fn only_read_commands_pass() {
        check_command(&args(&["get", "k"])).unwrap();
        check_command(&args(&["HGETALL", "k"])).unwrap();
        check_command(&args(&["memory", "usage", "k"])).unwrap();
        check_command(&args(&["SLOWLOG", "GET", "10"])).unwrap();
        assert!(check_command(&args(&["SET", "k", "v"])).unwrap_err().contains("SET"));
        assert!(check_command(&args(&["DEL", "k"])).is_err());
        assert!(check_command(&args(&["FLUSHALL"])).is_err());
        assert!(check_command(&args(&["CONFIG", "GET", "*"])).is_err());
        assert!(check_command(&args(&["EVAL", "return 1", "0"])).is_err());
        assert!(check_command(&args(&["KEYS", "*"])).unwrap_err().contains("SCAN"));
        assert!(check_command(&args(&["SLOWLOG", "RESET"])).is_err());
        assert!(check_command(&args(&["MEMORY", "PURGE"])).is_err());
        assert!(check_command(&args(&["CLIENT", "KILL", "x"])).is_err());
        assert!(check_command(&args(&["OBJECT"])).is_err());
    }

    #[test]
    fn parses_keyspace() {
        let t = parse_keyspace(
            "# Keyspace\r\ndb0:keys=3,expires=1,avg_ttl=5\r\ndb12:keys=178599,expires=50847,avg_ttl=49828244\r\n",
        );
        assert_eq!(
            t.rows,
            vec![vec![json!(0), json!(3), json!(1)], vec![json!(12), json!(178599), json!(50847)]]
        );
    }

    #[test]
    fn parses_commandstats() {
        let text = "# Commandstats\r\ncmdstat_get:calls=10,usec=35000,usec_per_call=3500.00,rejected_calls=0\r\ncmdstat_hgetall:calls=2,usec=100,usec_per_call=50.00\r\n";
        let rows = parse_commandstats(text);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("get".to_owned(), 10, 3500.0, 35.0));
    }

    #[test]
    fn values_become_json() {
        let v = RValue::Array(vec![
            RValue::BulkString(b"a".to_vec()),
            RValue::Int(3),
            RValue::Nil,
            RValue::BulkString(vec![0xff]),
        ]);
        assert_eq!(to_json(&v), json!(["a", 3, null, "0xff"]));
        let long = RValue::BulkString(vec![b'x'; MAX_VALUE_CHARS + 10]);
        assert!(to_json(&long).as_str().unwrap().contains("共"));
        assert_eq!(range_span(&args(&["LRANGE", "k", "0", "-1"])), Some((0, -1)));
    }
}
