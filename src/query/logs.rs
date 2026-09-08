//! 日志表（logpipe 的 `app_log`）的 SQL。
//!
//! 排序键是 `(timestamp, level, trace_id)`：带时间范围的查询 `ORDER BY timestamp DESC LIMIT n`
//! 能按排序键倒着读、读够就停，不用把整段时间的日志都排一遍；按 trace id 查走 `idx_trace_id`
//! bloom filter，不带时间范围也不慢。`message` 没有全文索引，关键字是逐行扫时间范围内的
//! 数据，所以时间范围是所有查询的第一道闸。

use serde::{Deserialize, Serialize};

use super::{Bindings, Bucket, TimeRange, quote_ident};
use crate::clickhouse::{Query, num};
use crate::error::{Error, Result};
use crate::schema::{ColumnKind, LOG_FIXED_COLUMNS, Table};

/// 关键字语法：空格分隔的词全部要命中（AND）；`-词` 排除；`"带 空格"` 当一个整体。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Terms {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

pub fn parse_terms(q: &str) -> Terms {
    let mut terms = Terms::default();
    let mut chars = q.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else { break };
        let negate = first == '-';
        if negate {
            chars.next();
        }
        let mut word = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                word.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                word.push(c);
                chars.next();
            }
        }
        if word.is_empty() {
            continue;
        }
        if negate { &mut terms.exclude } else { &mut terms.include }.push(word);
    }
    terms
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Order {
    Asc,
    Desc,
}

impl Order {
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.map(str::to_ascii_lowercase).as_deref() {
            None | Some("desc") => Ok(Order::Desc),
            Some("asc") => Ok(Order::Asc),
            Some(other) => {
                Err(Error::bad_request(format!("order 只能是 asc 或 desc，不是 {other:?}")))
            }
        }
    }

    fn sql(self) -> &'static str {
        match self {
            Order::Asc => "ASC",
            Order::Desc => "DESC",
        }
    }
}

/// 日志筛选条件。列名都是白名单里的（调用方按 [`Table`] 校验过）。
#[derive(Debug, Default, Clone)]
pub struct LogFilter {
    /// 没有时间范围只允许在给了 trace_id / span_id 时（走 bloom filter）。
    pub range: Option<TimeRange>,
    pub q: String,
    /// `q` 按正则（ClickHouse `match`，RE2）而不是关键字语法
    pub regex: bool,
    pub levels: Vec<String>,
    /// logger 包含（不分大小写）
    pub logger: Option<String>,
    /// thread 包含（不分大小写）
    pub thread: Option<String>,
    pub host: Option<String>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    /// 动态列（k8s 元数据、静态 fields）→ 允许的值集合（IN）
    pub dims: Vec<(String, Vec<String>)>,
}

impl LogFilter {
    /// 有没有按消息内容筛（关键字 / 正则）。这种查询没有索引可用，必须扫完整个时间范围。
    pub fn has_message_predicate(&self) -> bool {
        !self.q.trim().is_empty()
    }

    pub fn validate(&self) -> Result<()> {
        if self.range.is_none() && self.trace_id.is_none() && self.span_id.is_none() {
            return Err(Error::bad_request("需要时间范围（from / to），或者 trace_id / span_id"));
        }
        if self.regex && self.q.len() > 1024 {
            return Err(Error::bad_request("正则太长"));
        }
        Ok(())
    }

    /// WHERE 后面的部分。
    fn where_sql(&self, b: &mut Bindings) -> Result<String> {
        let mut clauses = Vec::new();
        if let Some(range) = &self.range {
            clauses.push(b.time_predicate("timestamp", range));
        }
        if let Some(id) = &self.trace_id {
            clauses.push(format!("trace_id = {}", b.bind("String", id)));
        }
        if let Some(id) = &self.span_id {
            clauses.push(format!("span_id = {}", b.bind("String", id)));
        }
        if !self.levels.is_empty() {
            clauses.push(format!("level IN {}", b.bind("Array(String)", &self.levels)));
        }
        if let Some(logger) = &self.logger {
            clauses.push(format!(
                "positionCaseInsensitiveUTF8(logger, {}) > 0",
                b.bind("String", logger)
            ));
        }
        if let Some(thread) = &self.thread {
            clauses.push(format!(
                "positionCaseInsensitiveUTF8(thread, {}) > 0",
                b.bind("String", thread)
            ));
        }
        if let Some(host) = &self.host {
            clauses.push(format!("host = {}", b.bind("String", host)));
        }
        for (column, values) in &self.dims {
            clauses.push(format!(
                "{} IN {}",
                quote_ident(column)?,
                b.bind("Array(String)", values)
            ));
        }
        if !self.q.trim().is_empty() {
            if self.regex {
                clauses.push(format!("match(message, {})", b.bind("String", self.q.trim())));
            } else {
                let terms = parse_terms(&self.q);
                for term in &terms.include {
                    clauses.push(format!(
                        "positionCaseInsensitiveUTF8(message, {}) > 0",
                        b.bind("String", term)
                    ));
                }
                for term in &terms.exclude {
                    clauses.push(format!(
                        "positionCaseInsensitiveUTF8(message, {}) = 0",
                        b.bind("String", term)
                    ));
                }
            }
        }
        Ok(if clauses.is_empty() { "1".to_owned() } else { clauses.join("\n  AND ") })
    }
}

/// 一行日志。固定列之外的（`namespace` / `pod` / `cluster` / `env`……）平铺在 `extra` 里，
/// 序列化时也平铺，前端拿到的就是一层 JSON。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRow {
    #[serde(deserialize_with = "num::de")]
    pub ts_ms: i64,
    pub level: String,
    pub trace_id: String,
    pub span_id: String,
    pub thread: String,
    pub logger: String,
    pub message: String,
    pub file: String,
    pub host: String,
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct CountRow {
    #[serde(deserialize_with = "num::de")]
    pub count: u64,
}

#[derive(Debug, Deserialize)]
pub struct HistogramRow {
    #[serde(deserialize_with = "num::de")]
    pub bucket: i64,
    pub level: String,
    #[serde(deserialize_with = "num::de")]
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FacetRow {
    pub value: String,
    #[serde(deserialize_with = "num::de")]
    pub count: u64,
}

/// 上下文查询往前 / 往后最多找多远。
pub const CONTEXT_WINDOW_MS: i64 = 3_600_000;

/// 检索 / 上下文 / 导出共用同一套排序键，三处看到的顺序才一致。
fn order_by(order: Order) -> String {
    let d = order.sql();
    format!("timestamp {d}, host {d}, file {d}, thread {d}, logger {d}, message {d}")
}

/// 只有低基数的字符串列才值得做 facet；`message` / `file` / id 这些做了也没法看。
pub fn facetable(table: &Table, field: &str) -> bool {
    const NEVER: &[&str] = &["message", "file", "trace_id", "span_id", "timestamp"];
    !NEVER.contains(&field) && table.column(field).is_some_and(|c| c.kind == ColumnKind::String)
}

pub struct LogQueries<'a> {
    pub database: &'a str,
    pub table: &'a Table,
}

impl LogQueries<'_> {
    fn table_ref(&self) -> String {
        format!("`{}`.`{}`", self.database, self.table.name)
    }

    /// 固定列带别名，其余列原样。
    fn select_columns(&self) -> Result<String> {
        let mut cols: Vec<String> = vec![
            "toUnixTimestamp64Milli(timestamp) AS ts_ms".into(),
            "level".into(),
            "trace_id".into(),
            "span_id".into(),
            "thread".into(),
            "logger".into(),
            "message".into(),
            "file".into(),
            "host".into(),
        ];
        for c in &self.table.columns {
            if !LOG_FIXED_COLUMNS.contains(&c.name.as_str()) {
                cols.push(quote_ident(&c.name)?);
            }
        }
        Ok(cols.join(", "))
    }

    fn export_columns(&self) -> Result<String> {
        // 导出给人看：时间按列的时区格式化成 `2026-09-08 16:52:15.123`。别名不能叫 timestamp：
        // ClickHouse 里 WHERE 会优先解析成这个别名（String），时间范围比较就报类型错
        Ok(self.select_columns()?.replacen(
            "toUnixTimestamp64Milli(timestamp) AS ts_ms",
            "toString(timestamp) AS time",
            1,
        ))
    }

    pub fn search(
        &self,
        filter: &LogFilter,
        order: Order,
        limit: u32,
        offset: u32,
    ) -> Result<Query> {
        filter.validate()?;
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        let limit = b.bind("UInt32", limit);
        let offset = b.bind("UInt32", offset);
        // 排序键前缀是 timestamp，ClickHouse 会按排序键顺序读、读够 LIMIT 就停；后面几列只是
        // 让同一毫秒内的行有确定顺序（ClickHouse 的并行排序不保证稳定），翻页时不会串。
        // 同一线程同一毫秒打两行同样的内容才会仍然分不出先后，那种情况顺序本来就无所谓。
        let sql = format!(
            "SELECT {cols}\nFROM {from}\nWHERE {where_sql}\nORDER BY {order_by}\nLIMIT {limit} OFFSET {offset}",
            cols = self.select_columns()?,
            from = self.table_ref(),
            order_by = order_by(order),
        );
        Ok(b.into_query(sql))
    }

    pub fn count(&self, filter: &LogFilter) -> Result<Query> {
        filter.validate()?;
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        Ok(b.into_query(format!(
            "SELECT count() AS count\nFROM {}\nWHERE {where_sql}",
            self.table_ref()
        )))
    }

    pub fn histogram(&self, filter: &LogFilter, bucket: &Bucket) -> Result<Query> {
        filter.validate()?;
        if filter.range.is_none() {
            return Err(Error::bad_request("直方图需要时间范围"));
        }
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        let origin = b.bind("Int64", bucket.origin_ms);
        let width = b.bind("Int64", bucket.width_ms);
        let sql = format!(
            "SELECT intDiv(toUnixTimestamp64Milli(timestamp) - {origin}, {width}) AS bucket, level, count() AS count\nFROM {from}\nWHERE {where_sql}\nGROUP BY bucket, level\nORDER BY bucket",
            from = self.table_ref(),
        );
        Ok(b.into_query(sql))
    }

    /// 某一列出现最多的值（给筛选下拉用）。`field` 必须通过 [`facetable`]。
    pub fn facets(&self, filter: &LogFilter, field: &str, limit: u32) -> Result<Query> {
        filter.validate()?;
        if !facetable(self.table, field) {
            return Err(Error::bad_request(format!("列 {field:?} 不支持统计取值")));
        }
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        let limit = b.bind("UInt32", limit);
        let sql = format!(
            "SELECT {col} AS value, count() AS count\nFROM {from}\nWHERE {where_sql}\nGROUP BY value\nORDER BY count DESC, value\nLIMIT {limit}",
            col = quote_ident(field)?,
            from = self.table_ref(),
        );
        Ok(b.into_query(sql))
    }

    /// 同一个日志流（host + file）里某一时刻之前 / 之后的 n 行。`before` 含 `ts_ms` 本身那一毫秒。
    /// 只在前后 [`CONTEXT_WINDOW_MS`] 内找：`file` 不在排序键里，不限时间的话一个安静的（或已经
    /// 轮转掉的）文件会一路往前扫到 30 天前；限定范围还能裁剪分区。
    pub fn context(
        &self,
        host: &str,
        file: &str,
        ts_ms: i64,
        before: bool,
        n: u32,
    ) -> Result<Query> {
        let mut b = Bindings::new();
        let host = b.bind("String", host);
        let file = b.bind("String", file);
        let ts = b.bind("Int64", ts_ms);
        let window = b.bind("Int64", CONTEXT_WINDOW_MS);
        let n = b.bind("UInt32", n);
        let (cmp, bound, order) = if before {
            ("<=", format!("timestamp >= fromUnixTimestamp64Milli({ts} - {window})"), Order::Desc)
        } else {
            (">", format!("timestamp < fromUnixTimestamp64Milli({ts} + {window})"), Order::Asc)
        };
        let sql = format!(
            "SELECT {cols}\nFROM {from}\nWHERE host = {host} AND file = {file}\n  AND timestamp {cmp} fromUnixTimestamp64Milli({ts})\n  AND {bound}\nORDER BY {order_by}\nLIMIT {n}",
            cols = self.select_columns()?,
            from = self.table_ref(),
            order_by = order_by(order),
        );
        Ok(b.into_query(sql))
    }

    /// 导出：和 search 一样的条件，不分页，时间戳格式化成文本。
    pub fn export(&self, filter: &LogFilter, order: Order, limit: u32) -> Result<Query> {
        filter.validate()?;
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        let limit = b.bind("UInt32", limit);
        let sql = format!(
            "SELECT {cols}\nFROM {from}\nWHERE {where_sql}\nORDER BY {order_by}\nLIMIT {limit}",
            cols = self.export_columns()?,
            from = self.table_ref(),
            order_by = order_by(order),
        );
        Ok(b.into_query(sql))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Column;

    fn table() -> Table {
        let cols = [
            ("timestamp", "DateTime64(3, 'Asia/Shanghai')"),
            ("level", "LowCardinality(String)"),
            ("trace_id", "String"),
            ("span_id", "String"),
            ("thread", "String"),
            ("logger", "String"),
            ("message", "String"),
            ("file", "String"),
            ("host", "LowCardinality(String)"),
            ("namespace", "LowCardinality(String)"),
            ("pod", "String"),
            ("replica", "Int64"),
        ];
        Table {
            name: "app_log".into(),
            columns: cols
                .iter()
                .map(|(n, t)| Column {
                    name: (*n).into(),
                    ty: (*t).into(),
                    kind: ColumnKind::classify(t),
                })
                .collect(),
        }
    }

    fn range() -> TimeRange {
        TimeRange { from_ms: 1_000, to_ms: 2_000 }
    }

    #[test]
    fn parses_terms() {
        let t = parse_terms(r#"支付 失败 -超时 "order id" -"not this" - "#);
        assert_eq!(t.include, ["支付", "失败", "order id"]);
        assert_eq!(t.exclude, ["超时", "not this"]);
        assert_eq!(parse_terms("   "), Terms::default());
        assert_eq!(parse_terms("\"unterminated").include, ["unterminated"]);
        assert_eq!(parse_terms("a\tb\nc").include, ["a", "b", "c"]);
    }

    #[test]
    fn search_binds_everything() {
        let table = table();
        let q = LogQueries { database: "logs", table: &table };
        let filter = LogFilter {
            range: Some(range()),
            q: "支付 -超时".into(),
            levels: vec!["ERROR".into(), "WARN".into()],
            logger: Some("OrderService".into()),
            host: Some("node-1".into()),
            dims: vec![("pod".into(), vec!["a".into(), "b".into()])],
            ..Default::default()
        };
        let query = q.search(&filter, Order::Desc, 200, 400).unwrap();
        let sql = query.sql();
        assert!(
            sql.starts_with("SELECT toUnixTimestamp64Milli(timestamp) AS ts_ms, level, trace_id"),
            "{sql}"
        );
        assert!(sql.contains(", `namespace`, `pod`, `replica`\nFROM `logs`.`app_log`"), "{sql}");
        assert!(sql.contains("timestamp >= fromUnixTimestamp64Milli({p0:Int64})"), "{sql}");
        assert!(sql.contains("level IN {p2:Array(String)}"), "{sql}");
        assert!(sql.contains("positionCaseInsensitiveUTF8(logger, {p3:String}) > 0"), "{sql}");
        assert!(sql.contains("host = {p4:String}"), "{sql}");
        assert!(sql.contains("`pod` IN {p5:Array(String)}"), "{sql}");
        assert!(sql.contains("positionCaseInsensitiveUTF8(message, {p6:String}) > 0"), "{sql}");
        assert!(sql.contains("positionCaseInsensitiveUTF8(message, {p7:String}) = 0"), "{sql}");
        assert!(sql.ends_with("ORDER BY timestamp DESC, host DESC, file DESC, thread DESC, logger DESC, message DESC\nLIMIT {p8:UInt32} OFFSET {p9:UInt32}"), "{sql}");
        let params = query.params();
        assert_eq!(params[2], ("p2".to_owned(), "['ERROR','WARN']".to_owned()));
        assert_eq!(params[5], ("p5".to_owned(), "['a','b']".to_owned()));
        assert_eq!(params[6], ("p6".to_owned(), "支付".to_owned()));
        assert_eq!(params[7], ("p7".to_owned(), "超时".to_owned()));
        assert_eq!(params[8], ("p8".to_owned(), "200".to_owned()));
        // 用户输入从不进 SQL 文本
        assert!(!sql.contains("支付") && !sql.contains("node-1") && !sql.contains("OrderService"));
    }

    #[test]
    fn regex_mode_uses_match() {
        let table = table();
        let q = LogQueries { database: "logs", table: &table };
        let filter = LogFilter {
            range: Some(range()),
            q: "order.*failed".into(),
            regex: true,
            ..Default::default()
        };
        let query = q.search(&filter, Order::Asc, 10, 0).unwrap();
        assert!(query.sql().contains("match(message, {p2:String})"), "{}", query.sql());
        assert!(query.sql().contains("ORDER BY timestamp ASC"));
    }

    #[test]
    fn trace_id_alone_needs_no_range() {
        let table = table();
        let q = LogQueries { database: "logs", table: &table };
        let filter = LogFilter { trace_id: Some("abc".into()), ..Default::default() };
        let query = q.search(&filter, Order::Asc, 10, 0).unwrap();
        assert!(query.sql().contains("WHERE trace_id = {p0:String}\n"), "{}", query.sql());
        assert!(
            q.search(&LogFilter::default(), Order::Asc, 10, 0).is_err(),
            "没有范围也没有 id 应拒绝"
        );
        assert!(
            q.histogram(&filter, &Bucket { width_ms: 1000, origin_ms: 0 }).is_err(),
            "直方图必须有范围"
        );
    }

    #[test]
    fn histogram_facets_context_export() {
        let table = table();
        let q = LogQueries { database: "logs", table: &table };
        let filter = LogFilter { range: Some(range()), ..Default::default() };
        let h = q.histogram(&filter, &Bucket { width_ms: 60_000, origin_ms: 0 }).unwrap();
        assert!(
            h.sql().contains(
                "intDiv(toUnixTimestamp64Milli(timestamp) - {p2:Int64}, {p3:Int64}) AS bucket"
            ),
            "{}",
            h.sql()
        );
        assert!(h.sql().ends_with("GROUP BY bucket, level\nORDER BY bucket"));

        let f = q.facets(&filter, "pod", 50).unwrap();
        assert!(f.sql().contains("SELECT `pod` AS value, count() AS count"), "{}", f.sql());
        assert!(q.facets(&filter, "message", 50).is_err());
        assert!(q.facets(&filter, "replica", 50).is_err(), "数字列不做 facet");
        assert!(q.facets(&filter, "nope", 50).is_err());

        let c = q.context("node-1", "/var/log/x.log", 1_500, true, 50).unwrap();
        assert!(
            c.sql().contains("timestamp <= fromUnixTimestamp64Milli({p2:Int64})"),
            "{}",
            c.sql()
        );
        assert!(
            c.sql().contains("timestamp >= fromUnixTimestamp64Milli({p2:Int64} - {p3:Int64})"),
            "{}",
            c.sql()
        );
        assert!(c.sql().contains("ORDER BY timestamp DESC, host DESC"));
        assert_eq!(c.params()[3].1, CONTEXT_WINDOW_MS.to_string());
        let c = q.context("node-1", "/var/log/x.log", 1_500, false, 50).unwrap();
        assert!(
            c.sql().contains("timestamp > fromUnixTimestamp64Milli({p2:Int64})"),
            "{}",
            c.sql()
        );
        assert!(c.sql().contains("ORDER BY timestamp ASC, host ASC"));

        let e = q.export(&filter, Order::Desc, 50_000).unwrap();
        assert!(e.sql().starts_with("SELECT toString(timestamp) AS time, level"), "{}", e.sql());
        assert!(!e.sql().contains("OFFSET"));
    }

    #[test]
    fn log_row_flattens_extra_columns() {
        let json = r#"{"ts_ms":1,"level":"INFO","trace_id":"","span_id":"","thread":"t","logger":"l","message":"m","file":"f","host":"h","pod":"p-1","replica":3}"#;
        let row: LogRow = serde_json::from_str(json).unwrap();
        assert_eq!(row.ts_ms, 1);
        assert_eq!(row.extra["pod"], "p-1");
        let quoted: LogRow =
            serde_json::from_str(&json.replace("\"ts_ms\":1", "\"ts_ms\":\"1\"")).unwrap();
        assert_eq!(quoted.ts_ms, 1);
        assert_eq!(row.extra["replica"], 3);
        let back = serde_json::to_value(&row).unwrap();
        assert_eq!(back["pod"], "p-1");
        assert_eq!(back["ts_ms"], 1);
    }
}
