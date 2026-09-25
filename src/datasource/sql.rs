//! 只读 SQL 的门：模型写来的 SQL 先过这里，再交给 MySQL / ClickHouse。
//!
//! **这一层不是唯一的保险**。真正兜底的是库那一侧：MySQL 每条都在 `START TRANSACTION READ ONLY`
//! 里跑、跑完 `ROLLBACK`，ClickHouse 每条都带 `readonly=2`；部署文档也要求配只读账号。这里做的是
//! 在语句到达库之前把明显不对的拦下来，并且给模型一句它看得懂的理由——库回的
//! `ERROR 1792 (25006): Cannot execute statement in a READ ONLY transaction` 模型也能懂，但先拦下来
//! 连一次往返都省了，也不会在只读账号没配好的部署上真的写进去。
//!
//! 做法是一个够用的词法切分，不是 SQL 解析器：去掉注释和引号里的内容之后，
//!
//! * 只能有一条语句（结尾的 `;` 可以有）；
//! * 第一个词必须是 `SELECT` / `WITH` / `SHOW` / `DESC` / `DESCRIBE` / `EXPLAIN`；
//! * 任何位置都不能出现写入类关键字（`WITH … DELETE`、`EXPLAIN ANALYZE UPDATE` 这类靠它拦）；
//! * 不能调用会越出这个库的函数（ClickHouse 的 `url()` / `file()` / `remote()`，MySQL 的
//!   `LOAD_FILE()`）或拿锁的函数（`GET_LOCK()`）；
//! * 不能有 MySQL 的可执行注释 `/*! … */`——那里面的内容 MySQL 会照样执行。

/// 允许的语句开头。
const LEADING: &[&str] = &["SELECT", "WITH", "SHOW", "DESC", "DESCRIBE", "EXPLAIN"];

/// 出现在任何位置都拒绝的词。只收保留字：它们不带引号就当不了列名，不会误伤正常查询。
/// `REPLACE` 例外，它也是字符串函数，后面紧跟 `(` 时放行。
const WRITE_WORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "REPLACE", "CREATE", "DROP", "ALTER", "TRUNCATE", "RENAME",
    "GRANT", "REVOKE", "LOCK", "UNLOCK", "CALL", "LOAD", "OPTIMIZE", "OUTFILE", "DUMPFILE",
];

/// 以函数形式出现（后面紧跟 `(`）就拒绝的名字，不分大小写。前一半是 ClickHouse 的表函数，
/// 会让库去读别处的数据（`url()` 还能当跳板打内网）；后一半是 MySQL 的读文件与拿锁函数。
const FORBIDDEN_FUNCTIONS: &[&str] = &[
    "url",
    "urlcluster",
    "file",
    "filecluster",
    "s3",
    "s3cluster",
    "gcs",
    "hdfs",
    "hdfscluster",
    "azureblobstorage",
    "azureblobstoragecluster",
    "remote",
    "remotesecure",
    "mysql",
    "postgresql",
    "sqlite",
    "mongodb",
    "redis",
    "jdbc",
    "odbc",
    "executable",
    "input",
    "load_file",
    "get_lock",
    "release_lock",
    "release_all_locks",
];

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// 不带引号的词：关键字、标识符、函数名
    Word(String),
    /// 引号里的东西（字符串、`"标识符"`、`` `标识符` ``），原样带引号
    Quoted(String),
    Number(String),
    /// JDBC 的占位符 `?`
    Placeholder,
    Punct(char),
}

/// 切词。注释丢掉；遇到 MySQL 的可执行注释 `/*!` 直接报错，引号没闭合也报错。
pub fn tokenize(sql: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            c if c.is_whitespace() => i += 1,
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '#' => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                if chars.get(i + 2) == Some(&'!') {
                    return Err(
                        "不接受 MySQL 的可执行注释 /*! … */（其中的内容会被执行）".to_owned()
                    );
                }
                let start = i;
                i += 2;
                loop {
                    if i + 1 >= chars.len() {
                        return Err(format!("注释没有闭合（从第 {} 个字符开始）", start + 1));
                    }
                    if chars[i] == '*' && chars[i + 1] == '/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            '\'' | '"' | '`' => {
                let quote = c;
                let start = i;
                i += 1;
                loop {
                    let Some(&ch) = chars.get(i) else {
                        return Err(format!(
                            "引号 {quote} 没有闭合（从第 {} 个字符开始）",
                            start + 1
                        ));
                    };
                    // 反斜杠转义只在字符串里算，反引号标识符里没有这回事
                    if ch == '\\' && quote != '`' {
                        i += 2;
                        continue;
                    }
                    if ch == quote {
                        // 两个引号连写是转义
                        if chars.get(i + 1) == Some(&quote) {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                out.push(Token::Quoted(chars[start..i].iter().collect()));
            }
            '?' => {
                out.push(Token::Placeholder);
                i += 1;
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                    i += 1;
                }
                out.push(Token::Number(chars[start..i].iter().collect()));
            }
            c if c.is_alphanumeric() || c == '_' || c == '$' || c == '@' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
                {
                    i += 1;
                }
                // `@var` 的 @ 单独占了一格，别让循环卡住
                if i == start {
                    i += 1;
                }
                out.push(Token::Word(chars[start..i].iter().collect()));
            }
            c => {
                out.push(Token::Punct(c));
                i += 1;
            }
        }
    }
    Ok(out)
}

/// 两种方言在校验上只差一处：ClickHouse 的结果格式由 opdash 指定，语句里不能再写 `FORMAT`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Mysql,
    ClickHouse,
}

/// 校验一条模型写来的 SQL，通过就返回去掉结尾分号的语句。
pub fn check_read_only(sql: &str, dialect: Dialect) -> Result<String, String> {
    let tokens = tokenize(sql)?;
    // 结尾的分号去掉；中间还有分号就是多条语句
    let mut end = tokens.len();
    while end > 0 && tokens[end - 1] == Token::Punct(';') {
        end -= 1;
    }
    let tokens = &tokens[..end];
    if tokens.is_empty() {
        return Err("SQL 为空".to_owned());
    }
    if tokens.contains(&Token::Punct(';')) {
        return Err("一次只能执行一条语句，请移除中间的分号，分开查询".to_owned());
    }
    let first = tokens.iter().find_map(|t| match t {
        Token::Punct('(') => None,
        Token::Word(w) => Some(Some(w.to_ascii_uppercase())),
        _ => Some(None),
    });
    let first = first.flatten();
    match &first {
        // SHOW / DESCRIBE 本身写不了东西，`SHOW CREATE TABLE` 里的 CREATE 也不是建表
        Some(w) if matches!(w.as_str(), "SHOW" | "DESC" | "DESCRIBE") => {
            return Ok(strip_trailing(sql));
        }
        Some(w) if LEADING.contains(&w.as_str()) => {}
        Some(w) => {
            return Err(format!(
                "只允许只读查询（SELECT / WITH / SHOW / DESCRIBE / EXPLAIN），不接受 {w}"
            ));
        }
        None => return Err("无法识别该语句类型；只允许 SELECT / SHOW / DESCRIBE / EXPLAIN".into()),
    }
    for (i, t) in tokens.iter().enumerate() {
        let Token::Word(w) = t else { continue };
        let upper = w.to_ascii_uppercase();
        let before_dot = matches!(tokens.get(i + 1), Some(Token::Punct('.')));
        let after_dot = i > 0 && matches!(tokens.get(i - 1), Some(Token::Punct('.')));
        let call = matches!(tokens.get(i + 1), Some(Token::Punct('(')));
        // `system.query_log`、`t.update` 里的词是库名 / 列名，不是关键字
        if before_dot || after_dot {
            continue;
        }
        if WRITE_WORDS.contains(&upper.as_str()) && !(upper == "REPLACE" && call) {
            return Err(format!("只读查询里不能出现 {upper}"));
        }
        if call && FORBIDDEN_FUNCTIONS.contains(&w.to_ascii_lowercase().as_str()) {
            return Err(format!("不允许调用 {w}()：它会读取这个库以外的数据或占用锁"));
        }
        if dialect == Dialect::ClickHouse && upper == "FORMAT" && !call {
            return Err("请移除 FORMAT 子句，结果格式由 opdash 指定".to_owned());
        }
    }
    Ok(strip_trailing(sql))
}

/// 原文去掉结尾的分号和空白：注释里的分号不影响，语句本身原样交给库。
fn strip_trailing(sql: &str) -> String {
    sql.trim_end().trim_end_matches(|c: char| c == ';' || c.is_whitespace()).to_owned()
}

/// 把 JDBC 语句里的 `?` 依次换成参数值，得到一条能直接 `EXPLAIN` 的语句。
///
/// 参数一律按字符串字面量代入：埋点里的参数值本来就是字符串，看不出原本的类型；MySQL 拿
/// `'12'` 比整数列会隐式转换且仍然走索引，反过来拿数字比字符串列才会让索引失效，所以带引号
/// 是不会让执行计划变差的那一边。占位符和参数个数对不上（参数没采全）时返回 `None`。
pub fn fill_placeholders(sql: &str, params: &[String]) -> Option<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len() + params.len() * 8);
    let mut next = params.iter();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' | '"' | '`' => {
                let quote = c;
                out.push(c);
                i += 1;
                while i < chars.len() {
                    let ch = chars[i];
                    out.push(ch);
                    i += 1;
                    if ch == '\\' && quote != '`' {
                        if let Some(&esc) = chars.get(i) {
                            out.push(esc);
                            i += 1;
                        }
                        continue;
                    }
                    if ch == quote {
                        if chars.get(i) == Some(&quote) {
                            out.push(quote);
                            i += 1;
                            continue;
                        }
                        break;
                    }
                }
            }
            '?' => {
                let value = next.next()?;
                out.push('\'');
                for ch in value.chars() {
                    match ch {
                        '\'' => out.push_str("''"),
                        '\\' => out.push_str("\\\\"),
                        ch => out.push(ch),
                    }
                }
                out.push('\'');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    next.next().is_none().then_some(out)
}

/// 标识符加反引号（MySQL 与 ClickHouse 都认），里面的反引号双写。
pub fn quote_ident(name: &str) -> String {
    let mut s = String::with_capacity(name.len() + 2);
    s.push('`');
    for c in name.chars() {
        if c == '`' {
            s.push('`');
        }
        s.push(c);
    }
    s.push('`');
    s
}

/// `库.表` 或 `表` → (库, 表)。库名表名里本身带点的，用反引号括起来写：`` `a.b`.`c` ``。
pub fn split_table(raw: &str) -> Result<(Option<String>, String), String> {
    let raw = raw.trim();
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' if quoted && chars.peek() == Some(&'`') => {
                chars.next();
                cur.push('`');
            }
            '`' => quoted = !quoted,
            '.' if !quoted => parts.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    parts.push(cur);
    if quoted {
        return Err(format!("表名 {raw:?} 的反引号没有闭合"));
    }
    match parts.as_slice() {
        [t] if !t.is_empty() => Ok((None, t.clone())),
        [d, t] if !d.is_empty() && !t.is_empty() => Ok((Some(d.clone()), t.clone())),
        _ => Err(format!("表名应写成 table 或 database.table，不是 {raw:?}")),
    }
}

/// `LIKE` 模式里的 `%` / `_` / `\` 转义掉，用户给的是子串，不是模式。
pub fn like_contains(needle: &str) -> String {
    let mut s = String::from("%");
    for c in needle.chars() {
        if matches!(c, '%' | '_' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('%');
    s
}

/// 调试日志里一行 SQL：换行压成空格，太长截断。
pub fn one_line(sql: &str) -> String {
    let mut s = String::new();
    for (n, word) in sql.split_whitespace().enumerate() {
        if n > 0 {
            s.push(' ');
        }
        if s.len() > 300 {
            s.push('…');
            break;
        }
        s.push_str(word);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(sql: &str) -> String {
        check_read_only(sql, Dialect::Mysql).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    fn rejected(sql: &str) -> String {
        check_read_only(sql, Dialect::Mysql).expect_err(sql)
    }

    #[test]
    fn plain_reads_pass() {
        assert_eq!(ok("select * from t_order where id = 1;"), "select * from t_order where id = 1");
        ok("SELECT REPLACE(name, 'a', 'b') FROM t");
        ok("WITH x AS (SELECT 1) SELECT * FROM x");
        ok("(SELECT 1) UNION (SELECT 2)");
        ok("show create table t_order");
        ok("SHOW INDEX FROM t_order");
        ok("desc t_order");
        ok("EXPLAIN FORMAT=JSON SELECT * FROM t WHERE a = 'x'");
        ok("EXPLAIN ANALYZE SELECT 1");
        // 关键字只出现在字符串、注释、反引号标识符里，或者是限定名的一部分
        ok("SELECT 'delete from t; drop table x' AS s, `update` FROM t -- drop\n WHERE 1");
        ok("SELECT t.update, t.delete FROM t /* insert */");
        ok("SELECT update_time, deleted FROM t");
        ok("SELECT CAST(x AS CHAR CHARACTER SET utf8mb4) FROM t");
    }

    #[test]
    fn writes_and_tricks_are_rejected() {
        assert!(rejected("delete from t").contains("DELETE"));
        assert!(rejected("UPDATE t SET a = 1").contains("UPDATE"));
        rejected("insert into t values (1)");
        rejected("REPLACE INTO t VALUES (1)");
        rejected("set global read_only = 0");
        rejected("WITH x AS (SELECT 1) DELETE FROM t");
        rejected("EXPLAIN ANALYZE UPDATE t SET a = 1");
        rejected("SELECT * FROM t FOR UPDATE");
        rejected("SELECT * FROM t LOCK IN SHARE MODE");
        rejected("SELECT * FROM t INTO OUTFILE '/tmp/x'");
        rejected("SELECT LOAD_FILE('/etc/passwd')");
        rejected("SELECT GET_LOCK('x', 10)");
        assert!(rejected("SELECT 1; DROP TABLE t").contains("一条语句"));
        assert!(rejected("SELECT /*! 1; DELETE FROM t */").contains("可执行注释"));
        assert!(rejected("SELECT 'abc").contains("没有闭合"));
        rejected("");
        rejected(" ; ");
        rejected("CALL p()");
    }

    #[test]
    fn clickhouse_specifics() {
        let ch = |s: &str| check_read_only(s, Dialect::ClickHouse);
        ch("SELECT * FROM system.query_log LIMIT 1").unwrap();
        ch("SELECT * FROM clusterAllReplicas('log', system.query_log)").unwrap();
        ch("SELECT formatReadableSize(1)").unwrap();
        assert!(ch("SELECT * FROM url('http://10.0.0.1/', CSV)").is_err());
        assert!(ch("SELECT * FROM s3('x')").is_err());
        assert!(ch("SELECT * FROM remote('h', db.t)").is_err());
        assert!(ch("SELECT 1 FORMAT CSV").unwrap_err().contains("FORMAT"));
        assert!(ch("ALTER TABLE t DELETE WHERE 1").is_err());
        assert!(ch("SYSTEM STOP MERGES").is_err());
        assert!(ch("OPTIMIZE TABLE t FINAL").is_err());
    }

    #[test]
    fn fills_jdbc_placeholders_as_strings() {
        let p = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            fill_placeholders("select * from t where id = ? and name = ?", &p(&["12", "O'Neil"]))
                .unwrap(),
            "select * from t where id = '12' and name = 'O''Neil'"
        );
        // 引号里的问号不是占位符
        assert_eq!(
            fill_placeholders("select '?' from t where a = ?", &p(&["x"])).unwrap(),
            "select '?' from t where a = 'x'"
        );
        assert_eq!(fill_placeholders("select 1", &[]).unwrap(), "select 1");
        assert!(fill_placeholders("select ? , ?", &p(&["1"])).is_none(), "参数不够");
        assert!(fill_placeholders("select ?", &p(&["1", "2"])).is_none(), "参数多了");
        assert_eq!(fill_placeholders("select ?", &p(&["a\\b"])).unwrap(), "select 'a\\\\b'");
    }

    #[test]
    fn splits_and_quotes_table_names() {
        assert_eq!(split_table("t_order").unwrap(), (None, "t_order".to_owned()));
        assert_eq!(split_table("shop.t_order").unwrap(), (Some("shop".into()), "t_order".into()));
        assert_eq!(split_table("`a.b`.`c`").unwrap(), (Some("a.b".into()), "c".into()));
        assert!(split_table("a.b.c").is_err());
        assert!(split_table("").is_err());
        assert_eq!(quote_ident("a`b"), "`a``b`");
        assert_eq!(like_contains("user_%"), "%user\\_\\%%");
    }
}
