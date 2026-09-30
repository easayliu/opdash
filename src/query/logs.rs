//! 日志表（logpipe 的 `app_log`）的 SQL。
//!
//! 这里的 SQL 是按排序键 `(service_name, timestamp, level, trace_id)` 写的（服务打头，见 docs/queries.md
//! 「排序键」一节）：锁定一个服务时按 `ORDER BY timestamp, level, trace_id LIMIT n` 只读这个服务的
//! 那一段；不锁服务时要把范围内各服务的排序列读出来排一遍，排序列很窄，实测墙钟没变差。
//! 排序键以外的列参与排序会退化，见 [`order_by`]。
//!
//! 线上 2026-09-30 换成了这个键（此前是 `(timestamp, level, trace_id)`）。
//!
//! 索引（2026-09-30 起，都是 text 倒排索引）：`trace_id` 上 `idx_trace_id_text`（`array` 分词器，
//! 全表不带时间范围按 trace_id 查只读几千行）；`message` 上 `idx_message_text`（建在
//! `lower(message)` 上，`hasToken` / `hasAllTokens` 和不带服务、不带 trace_id 时的 ASCII 子串 LIKE
//! 用得上，见 [`token_needles`]、[`LogFilter::substring_mode`]）。**`span_id` 上没有任何索引**；
//! 中文关键字仍是逐行扫时间范围内（这个服务）的数据。所以时间范围仍是第一道闸，按 span_id 查尤其如此。

use serde::{Deserialize, Serialize};

use super::{Bindings, Bucket, TimeRange, quote_ident};
use crate::clickhouse::{Query, num};
use crate::error::{Error, Result};
use crate::schema::{ColumnKind, LOG_FIXED_COLUMNS, Table};

/// 关键字语法（Kibana / Datadog 那套）：
///
/// * 空格分隔的词全部要命中（隐含 AND）；`a OR b` 任一命中；`-词` / `NOT 词` 排除；
/// * `"带 空格"` 当一个整体；`( )` 分组；优先级 NOT > AND > OR；
/// * `AND` / `OR` / `NOT` 全大写才算操作符，小写的 `or` 是普通词；要搜大写的就用引号 `"OR"`；
/// * 括号只在词的边界算语法：`(a OR b)` 是分组，`getUser(id)` 这种词内配对的括号照字面搜。
///
/// 解析不会报错：悬空的 `OR`、多余的括号、空引号都忽略掉，尽量按用户的意思来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Term(String),
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

/// 切到整词模式的最短词长。`hasToken` 走 `idx_message_text`（text 倒排索引，建在 `lower(message)` 上；
/// 2026-09-30 之前是 `tokenbf_v1`），只有整词能命中；短词按子串搜更符合直觉（搜 `health` 要能匹配 `healthcheck`），所以只有够长的
/// 标识符才切过去——span id 16 位、trace id / msgId 32 位都覆盖得到。
const TOKEN_MIN_LEN: usize = 16;

/// 一个词里最多发几个 `hasToken`，多了只是把 WHERE 拉长。留最长的几个——越长越稀疏。
const MAX_TOKENS_PER_TERM: usize = 4;

/// 这个词能不能走 token 索引；能就给出要发 `hasToken` 的 needle（小写，索引建在 `lower(message)` 上）。
///
/// ClickHouse 的 tokenizer 按「非字母数字的 ASCII 字符」切词（下划线也算），`hasToken` 的 needle
/// 只能是切好的纯字母数字 token，带分隔符会直接抛异常而不是返回空。所以这里先把词按同样的规则
/// 切开，再只挑 **够长的（≥ [`TOKEN_MIN_LEN`]）** token 当 needle：
///
/// * 纯 id（`AC10…`）：一个 needle，`hasToken` 单独就是完整语义；
/// * `msgId:AC10…`、`traceId=…` 这种键加 id：id 那个 token 当 needle 让索引跳 granule，调用方
///   再 AND 上子串条件保证前面的键也对得上；
/// * `RESULT_CHANGE`、`im_enter_direct_msg`：切出来全是短词，**不发**。线上量过（2026-09-18，
///   1 小时窗、3 分片）：`change` / `result` 各命中 359 / 364 个 granule，一个都跳不掉，而多出来的
///   两个 `hasToken` 让墙钟多 10% ~ 40%（1320 → 1950 ms、1517 → 1655 ms）。常见词无论怎么组合都
///   进不了索引，见 docs/queries.md「跳数索引的上限」。
///
/// 只认全 ASCII 的词：中文字节在 tokenizer 里也算分隔符，但没必要在这条路上证明它。
fn token_needles(term: &str) -> Option<Vec<String>> {
    if !term.is_ascii() {
        return None;
    }
    let mut ids: Vec<String> = term
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= TOKEN_MIN_LEN)
        .map(|t| t.to_ascii_lowercase())
        .collect();
    ids.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    ids.dedup();
    ids.truncate(MAX_TOKENS_PER_TERM);
    (!ids.is_empty()).then_some(ids)
}

/// 单个纯字母数字的词（没有分隔符）：`hasToken` 自己就是完整语义，不用再 AND 子串条件。
fn is_single_token(term: &str) -> bool {
    !term.is_empty() && term.chars().all(|c| c.is_ascii_alphanumeric())
}

/// 子串搜索改走 text 索引的最短词长，和 ClickHouse 的 `text_index_like_min_pattern_length`
/// 默认值一致：更短的模式它自己也不查索引，写成 LIKE 只会比 `position` 多算一遍 `lower`。
const LIKE_MIN_LEN: usize = 4;

/// 这个词的子串搜索能不能交给 `idx_message_text`（`text(tokenizer = 'splitByNonAlpha')`，建在
/// `lower(message)` 上）。索引只认 `lower(message) LIKE '%…%'`，`positionCaseInsensitiveUTF8`
/// 用不上它。只认纯 ASCII：LIKE 的词典扫描只取模式里的**字母数字**部分（见 ClickHouse 设置
/// `text_index_like_min_pattern_length` 的说明），中文不算，`EXPLAIN` 实测中文短语、`EP推送-`
/// 一个 granule 都剪不掉，LIKE 反而比 position 慢一点（一天一个分片 17 s → 20 s）。中文在
/// `splitByNonAlpha` 下并没有被当成分隔符，`检查主播对话` 是词典里的一个 token，只是这条路用不上。
///
/// 只在 [`SubstringMode::Like`] 下用。
fn uses_text_index(term: &str) -> bool {
    term.is_ascii() && term.len() >= LIKE_MIN_LEN
}

/// 不带服务时按整词（`hasToken`）查的纯数字最短位数：手机号 11 位、订单号 / 时间戳 10 位以上都覆盖。
///
/// 这种词写 LIKE 只能把词典整个扫一遍，耗时跟着天数涨：线上 2026-09-30 一个分片，一天 3.5~9 s，
/// 7 天 120 s 超时；`hasToken` 按词直接查，7 天 0.19 s。代价是只命中完整的 token，嵌在更长 token
/// 里的会漏：抽样 8 个真实数字 6 个命中数一致，秒级时间戳 `1790645335` 嵌在毫秒时间戳里，
/// LIKE 225 条、`hasToken` 60 条。
const DIGITS_TOKEN_MIN_LEN: usize = 8;

fn is_digits_token(term: &str) -> bool {
    term.len() >= DIGITS_TOKEN_MIN_LEN && term.bytes().all(|c| c.is_ascii_digit())
}

/// 中间 token 最短几个字符：`to` / `id` 这种太常见，倒排表太长，拿来当前置条件不划算。
const INNER_TOKEN_MIN_LEN: usize = 3;

/// 词里**两侧都被分隔符切开**的 token（小写），给按服务筛的检索当 `hasAllTokens` 前置条件。
///
/// 只有中间的 token 在日志里一定是完整的 token：`JDC_MAIN_RETRY_CHECK` 两头的 `jdc` / `check`
/// 可能是 `xjdc` / `checks` 的一截，拿它们去 `hasAllTokens` 会漏行；`main` / `retry` 两边都是 `_`，
/// 不会。所以加上这个条件结果和 `position` 严格一致（线上 6 组对照计数全部相同）。分隔符按
/// `splitByNonAlpha` 的规则取 ASCII 非字母数字；中文不是分隔符，带中文的 token 不收。
/// 已经够长、会单独发 `hasToken` 的（≥ [`TOKEN_MIN_LEN`]，见 [`token_needles`]）也不收。
fn inner_tokens(term: &str) -> Vec<String> {
    let parts: Vec<&str> =
        term.split(|c: char| c.is_ascii() && !c.is_ascii_alphanumeric()).collect();
    if parts.len() < 3 {
        return Vec::new();
    }
    let mut toks: Vec<String> = parts[1..parts.len() - 1]
        .iter()
        .filter(|t| (INNER_TOKEN_MIN_LEN..TOKEN_MIN_LEN).contains(&t.len()))
        .filter(|t| t.bytes().all(|c| c.is_ascii_alphanumeric()))
        .map(|t| t.to_ascii_lowercase())
        .collect();
    toks.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    toks.dedup();
    toks.truncate(MAX_TOKENS_PER_TERM);
    toks
}

/// 关键字的子串条件怎么写，按这次检索筛了什么来定，见 [`LogFilter::substring_mode`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubstringMode {
    /// 没按服务、也没按 trace_id 筛：ASCII 词写 `lower(message) LIKE`，长数字写 `hasToken`
    Like,
    /// 按服务筛：`position`，词里有中间 token 时再 AND 一个 `hasAllTokens`
    PositionWithTokens,
    /// 按 trace_id 筛：只写 `position`
    Position,
}

/// `lower(message) LIKE` 的模式：小写（`lower` 只折 ASCII，和 [`uses_text_index`] 只收 ASCII
/// 对得上），`\` / `%` / `_` 转义成字面量，两头加 `%`。
fn like_pattern(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 4);
    out.push('%');
    for c in term.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c.to_ascii_lowercase());
    }
    out.push('%');
    out
}

/// 大小写不敏感的子串条件，写法见 [`SubstringMode`]。
fn substring_sql(term: &str, mode: SubstringMode, b: &mut Bindings) -> String {
    if mode == SubstringMode::Like && uses_text_index(term) {
        return format!("lower(message) LIKE {}", b.bind("String", like_pattern(term)));
    }
    let position = format!("positionCaseInsensitiveUTF8(message, {}) > 0", b.bind("String", term));
    let toks = if mode == SubstringMode::PositionWithTokens { inner_tokens(term) } else { vec![] };
    if toks.is_empty() {
        return position;
    }
    // position 写在前面，理由同 [`Expr::message_sql`] 里的 hasToken
    format!("{position} AND hasAllTokens(lower(message), {})", b.bind("Array(String)", toks))
}

/// 这个正向词是不是按整词（`hasToken`）匹配，和 [`Expr::message_sql`] 的 Term 分支对得上。
fn matched_as_token(term: &str, mode: SubstringMode) -> bool {
    (mode == SubstringMode::Like && is_digits_token(term)) || token_needles(term).is_some()
}

/// OR 组合成一个 `multiSearchAny` 时给出它的词；`None` 表示拆成 OR 链，每一项各走各的
/// [`Expr::message_sql`]。
///
/// 全是普通词的 OR 用 multiSearchAny 一趟扫完，比 N 个 position 快；ClickHouse 限制一次最多
/// 256 个 needle，超了退回 OR 链。
///
/// 例外：不带服务、每个词都能走 text 索引时拆成 OR 链，每个词各写各的 LIKE / hasToken。
/// multiSearchAny 用不上索引（建在 `lower(message)` 上的索引认不出 CaseInsensitive 版本，改成
/// `multiSearchAny(lower(message), …)` 实测也剪不掉）：线上 2026-09-30 一个分片一天，
/// `listEbpAccount OR ebp/advertiser/list` multiSearchAny 88.8 GiB / 92 s，LIKE OR LIKE
/// 3.0 GiB / 3.7 s。有一个词走不了索引，整个 OR 都得逐行扫，那还是 multiSearchAny 扫得快。
fn or_as_multi_search(parts: &[Expr], mode: SubstringMode) -> Option<Vec<String>> {
    let terms: Vec<String> = parts
        .iter()
        .map(|p| match p {
            Expr::Term(t) => Some(t.clone()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let indexed = mode == SubstringMode::Like && terms.iter().all(|t| uses_text_index(t));
    (terms.len() <= 256 && !indexed).then_some(terms)
}

impl Expr {
    fn and(mut parts: Vec<Expr>) -> Option<Expr> {
        let mut flat = Vec::with_capacity(parts.len());
        for p in parts.drain(..) {
            match p {
                Expr::And(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        match flat.len() {
            0 => None,
            1 => flat.pop(),
            _ => Some(Expr::And(flat)),
        }
    }

    fn or(mut parts: Vec<Expr>) -> Option<Expr> {
        let mut flat = Vec::with_capacity(parts.len());
        for p in parts.drain(..) {
            match p {
                Expr::Or(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        match flat.len() {
            0 => None,
            1 => flat.pop(),
            _ => Some(Expr::Or(flat)),
        }
    }

    /// 要高亮的正向词：不在 NOT 下面的所有词。
    pub fn positive_terms(&self) -> Vec<String> {
        fn walk(e: &Expr, negated: bool, out: &mut Vec<String>) {
            match e {
                Expr::Term(t) => {
                    if !negated {
                        out.push(t.clone());
                    }
                }
                Expr::Not(inner) => walk(inner, !negated, out),
                Expr::And(parts) | Expr::Or(parts) => {
                    parts.iter().for_each(|p| walk(p, negated, out))
                }
            }
        }
        let mut out = Vec::new();
        walk(self, false, &mut out);
        out
    }

    /// 在 `message` 上的谓词。返回的 SQL 可以直接 AND 到别的条件上（OR 一定带括号）。
    fn message_sql(&self, mode: SubstringMode, b: &mut Bindings) -> String {
        match self {
            // 不带服务时长数字按整词查，见 [`DIGITS_TOKEN_MIN_LEN`]
            Expr::Term(t) if mode == SubstringMode::Like && is_digits_token(t) => {
                format!("hasToken(lower(message), {})", b.bind("String", t))
            }
            // 够长的整词走 text 索引直接按行号取（线上 7 天不带服务按 19 位 id 搜 236 GiB / 286 s
            // → 0 GiB / 2.7 s）；其余是子串匹配。见 [`token_needles`]
            Expr::Term(t) => {
                match token_needles(t) {
                    Some(toks) if is_single_token(t) => {
                        format!("hasToken(lower(message), {})", b.bind("String", &toks[0]))
                    }
                    Some(toks) => {
                        // 子串条件写在前面：`and` 是短路求值，索引跳不掉的 granule 里先算它，
                        // 稀有词基本不会走到后面的 hasToken；索引分析看的是整个 WHERE，顺序无关
                        let mut parts = vec![substring_sql(t, mode, b)];
                        parts.extend(toks.iter().map(|tok| {
                            format!("hasToken(lower(message), {})", b.bind("String", tok))
                        }));
                        parts.join(" AND ")
                    }
                    None => substring_sql(t, mode, b),
                }
            }
            // 排除词一律保持子串语义：整词比子串窄，取反之后就变宽了，会漏掉本该排除的行；
            // 而且否定条件本来就用不上索引（它只能证明「可能有」）
            Expr::Not(inner) => match &**inner {
                Expr::Term(t) => {
                    format!("positionCaseInsensitiveUTF8(message, {}) = 0", b.bind("String", t))
                }
                Expr::And(_) => format!("NOT ({})", inner.message_sql(mode, b)),
                other => format!("NOT {}", other.message_sql(mode, b)),
            },
            Expr::And(parts) => {
                parts.iter().map(|p| p.message_sql(mode, b)).collect::<Vec<_>>().join(" AND ")
            }
            Expr::Or(parts) => match or_as_multi_search(parts, mode) {
                Some(terms) => format!(
                    "multiSearchAnyCaseInsensitiveUTF8(message, {})",
                    b.bind("Array(String)", terms)
                ),
                None => {
                    let inner = parts
                        .iter()
                        .map(|p| match p {
                            Expr::And(_) => format!("({})", p.message_sql(mode, b)),
                            _ => p.message_sql(mode, b),
                        })
                        .collect::<Vec<_>>()
                        .join(" OR ");
                    format!("({inner})")
                }
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Phrase(String),
    And,
    Or,
    Not,
    LParen,
    RParen,
}

fn tokenize(q: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = q.chars().peekable();
    'outer: loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        // 词前面的前缀：`(` 开组，`-` 取反（后面得紧跟内容，孤零零的 `-` 忽略）
        loop {
            match chars.peek() {
                Some('(') => {
                    chars.next();
                    tokens.push(Token::LParen);
                }
                Some('-') => {
                    let mut ahead = chars.clone();
                    ahead.next();
                    match ahead.peek() {
                        Some(c) if !c.is_whitespace() && *c != ')' => {
                            chars.next();
                            tokens.push(Token::Not);
                        }
                        _ => {
                            chars.next();
                            continue 'outer;
                        }
                    }
                }
                _ => break,
            }
        }
        match chars.peek() {
            None => break,
            Some(')') => {
                chars.next();
                tokens.push(Token::RParen);
            }
            Some('"') => {
                chars.next();
                let mut phrase = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(e @ ('"' | '\\')) => phrase.push(e),
                            Some(e) => {
                                phrase.push('\\');
                                phrase.push(e);
                            }
                            None => phrase.push('\\'),
                        },
                        c => phrase.push(c),
                    }
                }
                if !phrase.is_empty() {
                    tokens.push(Token::Phrase(phrase));
                }
            }
            Some(_) => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() {
                        break;
                    }
                    word.push(c);
                    chars.next();
                }
                // 词尾多出来的 `)` 是关组：`(a OR foo)` → foo + `)`；`getUser(id)` 配对了就不动
                let mut closers = 0;
                while word.ends_with(')') {
                    let opens = word.matches('(').count();
                    let closes = word.matches(')').count();
                    if closes <= opens {
                        break;
                    }
                    word.pop();
                    closers += 1;
                }
                match word.as_str() {
                    "" => {}
                    "AND" => tokens.push(Token::And),
                    "OR" => tokens.push(Token::Or),
                    "NOT" => tokens.push(Token::Not),
                    _ => tokens.push(Token::Word(word)),
                }
                tokens.extend(std::iter::repeat_n(Token::RParen, closers));
            }
        }
    }
    tokens
}

/// 括号和 NOT 最多嵌套几层。解析是递归下降，每层吃掉几百字节的栈：不设上限的话，`q` 里
/// 放五千个 `(` 就能把 2 MiB 的 worker 线程栈压爆，而 Rust 栈溢出是整个进程 abort，不是
/// 这一个请求失败。32 层手写的查询永远用不到。
const MAX_DEPTH: usize = 32;

/// 关键字语法的 `q` 最长多少字节。词越多拼出来的 SQL 越长，贴一大段报错进来也该先截一截。
const MAX_QUERY_LEN: usize = 4096;

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    depth: usize,
    /// 碰到过超过 [`MAX_DEPTH`] 的嵌套：超出的那层当放错位置的操作符跳过，[`LogFilter::validate`]
    /// 据此回 400
    too_deep: bool,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    /// or := and ("OR" and)*
    fn parse_or(&mut self) -> Option<Expr> {
        let mut parts = Vec::new();
        parts.extend(self.parse_and());
        while self.peek() == Some(&Token::Or) {
            self.bump();
            parts.extend(self.parse_and());
        }
        Expr::or(parts)
    }

    /// and := unary (["AND"] unary)*
    fn parse_and(&mut self) -> Option<Expr> {
        let mut parts = Vec::new();
        loop {
            match self.peek() {
                None | Some(Token::Or) | Some(Token::RParen) => break,
                Some(Token::And) => {
                    self.bump();
                }
                Some(_) => parts.extend(self.parse_unary()),
            }
        }
        Expr::and(parts)
    }

    /// unary := ("NOT" | "-") unary | "(" or ")" | word | phrase
    fn parse_unary(&mut self) -> Option<Expr> {
        let token = self.bump()?;
        if !matches!(token, Token::Not | Token::LParen) {
            return self.parse_leaf(token);
        }
        if self.depth >= MAX_DEPTH {
            self.too_deep = true;
            return None;
        }
        self.depth += 1;
        let expr = self.parse_leaf(token);
        self.depth -= 1;
        expr
    }

    fn parse_leaf(&mut self, token: Token) -> Option<Expr> {
        match token {
            Token::Not => self.parse_unary().map(|e| match e {
                Expr::Not(inner) => *inner,
                other => Expr::Not(Box::new(other)),
            }),
            Token::LParen => {
                let inner = self.parse_or();
                if self.peek() == Some(&Token::RParen) {
                    self.bump();
                }
                inner
            }
            Token::Word(w) | Token::Phrase(w) => Some(Expr::Term(w)),
            // 放错位置的操作符 / 括号：跳过
            Token::And | Token::Or | Token::RParen => None,
        }
    }
}

/// 把关键字串解析成表达式；只有空白 / 只有操作符时是 `None`。
pub fn parse_query(q: &str) -> Option<Expr> {
    parse(q).0
}

/// 解析，顺带告诉调用方有没有嵌套超限。
fn parse(q: &str) -> (Option<Expr>, bool) {
    let mut p = Parser { tokens: tokenize(q), pos: 0, depth: 0, too_deep: false };
    let mut parts = Vec::new();
    while p.peek().is_some() {
        let before = p.pos;
        if p.peek() == Some(&Token::RParen) {
            p.bump();
            continue;
        }
        parts.extend(p.parse_or());
        if p.pos == before {
            p.bump();
        }
    }
    (Expr::and(parts), p.too_deep)
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
    /// 没有时间范围只允许在给了 trace_id / span_id 时，而且**这条路很贵**：`span_id` 上没有
    /// 任何索引（`trace_id` 2026-09-30 起有 text 索引，不带时间范围也只读几千行）。2026-09-21 线上实测（关掉 query
    /// condition cache）span 点查扫 31.3 G 行 / 37.9 GiB / 8.8 s、trace 点查 1.03 G 行 /
    /// 5.4 GB / 14 s，而同一个 span 加上一小时窗口只要 8.3 M 行 / 158 MB / 0.18 s。
    /// 调用方手上只要有个大概时刻就该带上范围，不带是「实在不知道它是什么时候的」的兜底。
    pub range: Option<TimeRange>,
    pub q: String,
    /// `q` 按正则（ClickHouse `match`，RE2）而不是关键字语法（见 [`parse_query`]）
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
    /// 关键字的子串条件怎么写（[`SubstringMode`]）：LIKE 走 text 索引**只在既没按服务、也没按
    /// trace_id 筛的时候**；按服务筛时写 position，再用中间 token 的 `hasAllTokens` 让索引先剪行。
    ///
    /// 排序键服务打头，按服务筛本身就把读量压到这个服务的那一段；这时再走 text 索引，LIKE 的
    /// 词典扫描要把整个 part 的词典过一遍，常见词还要把大片行号物化出来，反而更慢。线上
    /// 2026-09-30 同一天、各跑两次取热缓存：
    ///
    /// | 场景 | position | LIKE 走索引 |
    /// |---|---|---|
    /// | dyopen，常见词 `msgid` | 0.81 s | 9.23 s |
    /// | dyopen，稀有词 `WX_RECOGNIZE_SHADOW` | 0.70 s | 0.68 s |
    /// | ai-crm，稀有词 `hit_area` | 0.89 s | 1.21 s |
    /// | 不带服务，11 位手机号 | 266 GiB / 36 s | 0 GiB / 5.5 s |
    /// | 不带服务，`timeout` | 266 GiB / 33 s | 0.36 GiB / 4.9 s |
    ///
    /// 不带服务时 position 一天就要读 266 GiB，在 100 GiB 的读量护栏下根本跑不完。
    /// pod / container 这类筛选不在排序键里、省不下读量，照「没按服务筛」算。
    ///
    /// 带 trace_id 时 `idx_trace_id_text` 已经把范围压到这条链路的几万行，position 逐行扫这点
    /// 数据很快；再走 LIKE，词典扫描的开销和行数无关，照样要过一遍。线上 2026-09-30 一条
    /// 5 分钟窗口的 `trace_id` + `replaceKeyWords`，热缓存 position 5.5 万行 / 1.9 s，
    /// LIKE 1.4 万行 / 13.6 s。
    ///
    /// 按服务筛时的 `hasAllTokens` 是按词直接查，不扫词典，常见词也不会变慢（见 [`inner_tokens`]）。
    /// 线上 2026-09-30 一个分片热缓存：afanti-crawler-cmd-worker 14 小时 `JDC_MAIN_RETRY_CHECK`，
    /// position 29.6 GiB / 6.1 s → 7.3 GiB / 1.8 s（中间 token 只有 `main` / `retry`）；dyopen 一天
    /// `WX_RECOGNIZE_SHADOW`（`recognize`
    /// 很常见）0.60 s → 0.52 s，hjk 一天 `job/account/detail` 0.23 s → 0.21 s。不带服务时不加：
    /// LIKE 自己就用了中间 token，读的行数和加了一样。
    fn substring_mode(&self) -> SubstringMode {
        if self.trace_id.is_some() {
            SubstringMode::Position
        } else if self
            .dims
            .iter()
            .any(|(column, values)| column == "service_name" && !values.is_empty())
        {
            SubstringMode::PositionWithTokens
        } else {
            SubstringMode::Like
        }
    }

    /// 有没有按消息内容筛（关键字 / 正则）。这种查询没有索引可用，必须扫完整个时间范围。
    pub fn has_message_predicate(&self) -> bool {
        if self.regex { !self.q.trim().is_empty() } else { parse_query(&self.q).is_some() }
    }

    pub fn validate(&self) -> Result<()> {
        if self.range.is_none() && self.trace_id.is_none() && self.span_id.is_none() {
            return Err(Error::bad_request("需要时间范围（from / to），或提供 trace_id / span_id"));
        }
        if self.regex && self.q.len() > 1024 {
            return Err(Error::bad_request("正则表达式过长"));
        }
        if !self.regex {
            if self.q.len() > MAX_QUERY_LEN {
                return Err(Error::bad_request(format!("关键字过长：上限为 {MAX_QUERY_LEN} 字节")));
            }
            if parse(&self.q).1 {
                return Err(Error::bad_request(format!(
                    "括号与 NOT 的嵌套层数过多：上限为 {MAX_DEPTH} 层"
                )));
            }
        }
        Ok(())
    }

    /// 哪些词被当成整词（而不是子串）匹配了——走了 token 索引的那些。语义比子串窄，页面要提示。
    /// 分支必须和 [`Expr::message_sql`] 一致：NOT 下面的不进索引，合成 multiSearchAny 的 OR 组也不进；
    /// 拆成 OR 链的组里每一项照常判断。
    pub fn token_terms(&self) -> Vec<String> {
        fn walk(e: &Expr, mode: SubstringMode, out: &mut Vec<String>) {
            match e {
                Expr::Term(t) => {
                    if matched_as_token(t, mode) && !out.contains(t) {
                        out.push(t.clone());
                    }
                }
                Expr::And(parts) => parts.iter().for_each(|p| walk(p, mode, out)),
                Expr::Or(parts) if or_as_multi_search(parts, mode).is_none() => {
                    parts.iter().for_each(|p| walk(p, mode, out))
                }
                Expr::Not(_) | Expr::Or(_) => {}
            }
        }
        if self.regex {
            return Vec::new();
        }
        let mut out = Vec::new();
        if let Some(expr) = parse_query(&self.q) {
            walk(&expr, self.substring_mode(), &mut out);
        }
        out
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
                let mode = self.substring_mode();
                match parse_query(&self.q) {
                    // 顶层 AND 拆成多个子句，和别的条件一起平铺，SQL 好读
                    Some(Expr::And(parts)) => {
                        clauses.extend(parts.iter().map(|p| p.message_sql(mode, b)));
                    }
                    Some(expr) => clauses.push(expr.message_sql(mode, b)),
                    None => {}
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
    /// `message` 被截断前有多少字符。等于 `message` 的长度就是没截断；导出那条路不带这个字段
    #[serde(default, deserialize_with = "num::de_opt")]
    pub message_len: Option<u64>,
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

/// 检索 / 上下文 / 导出共用同一套排序，三处看到的顺序才一致。
///
/// 排的就是表排序键去掉服务那一截 `(timestamp, level, trace_id)`——锁定单个服务时这正是该服务数据段
/// 的物理顺序，ClickHouse 才能纯按顺序读、读够 LIMIT 就停。以前后面还跟着 host / file / thread / logger / message 让同毫秒的行有确定顺序，
/// 但这些列不在排序键里，执行计划上会多出 `PartialSorting` + `FinishSorting`：线上实测一小时窗口
/// 取 200 行，3.03 GB / 4240 ms vs 现在的 0.14 GB / 166 ms，带关键字时 9.19 GB / 20.8 s vs 6.00 GB / 4.7 s。
///
/// 代价是同一 `(timestamp, level, trace_id)` 的行之间先后不保证（线上 10 分钟里 38% 的行落在这种
/// 并列组里，最大一组 233 行），页边界正好切在一组中间时翻页可能重复或漏几行。要精确得换 keyset
/// 翻页（游标带这个元组，不用 OFFSET），那也顺带解决深 OFFSET 的翻页上限。
fn order_by(order: Order) -> String {
    let d = order.sql();
    format!("timestamp {d}, level {d}, trace_id {d}")
}

/// 只有低基数的字符串列才值得做 facet；`message` / `file` / id 这些做了也没法看。
pub fn facetable(table: &Table, field: &str) -> bool {
    const NEVER: &[&str] = &["message", "file", "trace_id", "span_id", "timestamp"];
    !NEVER.contains(&field) && table.column(field).is_some_and(|c| c.kind == ColumnKind::String)
}

/// 这一列值不值得取回前端。JSON / Map / Array / 其它复杂类型一律不取：日志页显示不了，
/// 而且 JSON 列是按 granule 整块读的，白读一遍。
fn displayable(kind: ColumnKind) -> bool {
    matches!(kind, ColumnKind::String | ColumnKind::Int | ColumnKind::Float | ColumnKind::DateTime)
}

pub struct LogQueries<'a> {
    pub database: &'a str,
    pub table: &'a Table,
    /// 每条日志的 `message` 最多取多少字符（[`crate::config::Config::max_message_chars`]）。
    /// 只作用于列表 / 上下文 / 跟随，导出不截。
    pub max_message_chars: u32,
}

impl LogQueries<'_> {
    fn table_ref(&self) -> String {
        format!("`{}`.`{}`", self.database, self.table.name)
    }

    /// 固定列带别名，其余**能显示的**列原样。
    ///
    /// 只带上字符串 / 数字 / 时间列——也就是 `/api/meta` 里当筛选维度给前端的那些。日志表在
    /// 线上物理带着整套 span 列（`resource_attributes JSON`、`events.attributes Array(JSON)`
    /// ……，logpipe 不往里写，全是默认值），页面上也没有地方显示它们，`SELECT *` 式地带上
    /// 只是白读、白传：线上一小时窗口取 200 行实测 0.129 GB → 0.098 GB。
    fn select_columns(&self, cap: Option<u32>) -> Result<String> {
        // 截断在 SQL 里做，不是拿回来再截：线上一条 41 MB 的日志，ClickHouse 只用了 1.7 秒，
        // 12.6 秒里另外 11 秒全花在 ClickHouse → opdash 这一程的传输上。在 Rust 里截省不掉它。
        //
        // 单位是**字符**不是字节（`substringUTF8` / `lengthUTF8`）：按字节切会把多字节字符劈成
        // 半个，ClickHouse 对非法 UTF-8 的行为是未定义的，吐出来的 JSON 可能直接解析不了。
        //
        // 原始长度必须写成 `` `表名`.message `` ——直接写 `lengthUTF8(message)` 会解析成上面那个
        // 截断后的别名 `message`，量出来永远等于 cap（和 [`Self::export_columns`] 里 `time`
        // 别名踩的是同一个坑）。
        let message = match cap {
            Some(n) => format!(
                "substringUTF8(message, 1, {n}) AS message, lengthUTF8({tbl}.message) AS message_len",
                tbl = quote_ident(&self.table.name)?,
            ),
            None => "message".to_owned(),
        };
        let mut cols: Vec<String> = vec![
            "toUnixTimestamp64Milli(timestamp) AS ts_ms".into(),
            "level".into(),
            "trace_id".into(),
            "span_id".into(),
            "thread".into(),
            "logger".into(),
            message,
            "file".into(),
            "host".into(),
        ];
        for c in &self.table.columns {
            if !LOG_FIXED_COLUMNS.contains(&c.name.as_str()) && displayable(c.kind) {
                cols.push(quote_ident(&c.name)?);
            }
        }
        Ok(cols.join(", "))
    }

    fn export_columns(&self) -> Result<String> {
        // 导出给人看：时间按列的时区格式化成 `2026-09-08 16:52:15.123`。别名不能叫 timestamp：
        // ClickHouse 里 WHERE 会优先解析成这个别名（String），时间范围比较就报类型错
        Ok(self.select_columns(None)?.replacen(
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
        // 按排序键读、读够 LIMIT 就停，见 [`order_by`]
        let sql = format!(
            "SELECT {cols}\nFROM {from}\nWHERE {where_sql}\nORDER BY {order_by}\nLIMIT {limit} OFFSET {offset}",
            cols = self.select_columns(Some(self.max_message_chars))?,
            from = self.table_ref(),
            order_by = order_by(order),
        );
        // SELECT 里的截断别名叫 `message`，和列同名。ClickHouse 默认让 WHERE 里的 `message`
        // 也指向这个别名，于是关键字条件全变成对截断后文本求值：
        //
        // * `hasToken(lower(message), …)` 变成 `lower(substringUTF8(message, 1, n))`，和
        //   `idx_message_tokens` 的表达式 `lower(message)` 对不上，**索引一个 granule 都不剪**。
        //   线上一条按 19 位 id 搜 81 分钟的查询 1305 万行 / 19.6 GiB → 14 万行 / 1.6 GiB，结果不变；
        // * 子串搜索只在每条日志的前 n 个字符里找，长日志后半截搜不到。
        //
        // 这个设置让 WHERE / ORDER BY 里的名字优先解析成列。`message_len` 那里写表名前缀是同一个坑。
        Ok(b.into_query(sql).setting("prefer_column_name_to_alias", 1))
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

    /// 几列各自出现最多的值（给筛选下拉用），**一条查询出全部**。每个 `field` 都要通过
    /// [`facetable`]。
    ///
    /// 日志页顶上一排下拉，以前是一个维度一条 `GROUP BY`：光是打开页面就发 4 条（服务 /
    /// 命名空间 / pod / 容器），点「更多筛选」再发 10 条，14 条的 `WHERE` 一模一样、只差
    /// 分组的那一列。线上 1 小时窗实测，**每条都要扫 916 万行**，4 条合起来 3666 万行 /
    /// 591.6 MB，只为了填几个下拉框。
    ///
    /// `approx_top_k` 把它们并成一次扫描：**14 个维度一条查询 911 万行 / 439.6 MB / 0.48 秒**,
    /// 比原来光打开页面那 4 条还便宜。代价是计数从精确变成 Space-Saving 近似——线上这些
    /// 维度实测返回的误差项都是 0（基数没超过算法容量），而且这个数字在下拉里只是个参考量级。
    ///
    /// 计数用 `toFloat64` 转一下：UInt64 在 JSON 里是带引号的字符串，而元组里的字段没法单独
    /// 挂 `num::de`。行数远不到 2^53，浮点存得下。
    pub fn facets(&self, filter: &LogFilter, fields: &[&str], limit: u32) -> Result<Query> {
        filter.validate()?;
        if fields.is_empty() {
            return Err(Error::bad_request("缺少参数 field"));
        }
        let mut cols = Vec::with_capacity(fields.len());
        for field in fields {
            if !facetable(self.table, field) {
                return Err(Error::bad_request(format!("列 {field:?} 不支持统计取值")));
            }
            let col = quote_ident(field)?;
            // approx_top_k 的个数必须是字面量常量，进不了参数绑定；limit 是 u32，拼进去是安全的
            cols.push(format!(
                "arrayMap(t -> (t.1, toFloat64(t.2)), approx_top_k({limit})({col})) AS {col}"
            ));
        }
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        let sql = format!(
            "SELECT {}\nFROM {from}\nWHERE {where_sql}",
            cols.join(",\n  "),
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
            cols = self.select_columns(Some(self.max_message_chars))?,
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

    fn term(s: &str) -> Expr {
        Expr::Term(s.into())
    }

    fn not(e: Expr) -> Expr {
        Expr::Not(Box::new(e))
    }

    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        // 没有深度上限时五千个 `(` 就能让进程 abort；测试线程也只有 2 MiB 栈
        for q in ["(".repeat(100_000) + "a", "-".repeat(100_000) + "a", "NOT ".repeat(50_000) + "a"]
        {
            let (_, too_deep) = parse(&q);
            assert!(too_deep);
        }
        let nested = "(".repeat(MAX_DEPTH) + "a" + &")".repeat(MAX_DEPTH);
        assert_eq!(parse(&nested), (Some(term("a")), false));
    }

    #[test]
    fn rejects_overlong_or_overnested_keywords() {
        let filter = |q: String| LogFilter { q, trace_id: Some("ab".into()), ..Default::default() };
        let msg = |f: LogFilter| f.validate().unwrap_err().to_string();
        assert!(msg(filter("(".repeat(MAX_DEPTH + 1) + "a")).contains("嵌套"));
        assert!(msg(filter("a ".repeat(MAX_QUERY_LEN))).contains("过长"));
        filter("(a OR b) NOT (c d)".into()).validate().unwrap();
        // 正则不走这套语法，括号再多也不是嵌套
        LogFilter { regex: true, ..filter("(".repeat(100) + ")".repeat(100).as_str()) }
            .validate()
            .unwrap();
    }

    #[test]
    fn parses_terms() {
        assert_eq!(
            parse_query(r#"支付 失败 -超时 "order id" -"not this" - "#),
            Some(Expr::And(vec![
                term("支付"),
                term("失败"),
                not(term("超时")),
                term("order id"),
                not(term("not this")),
            ]))
        );
        assert_eq!(parse_query("   "), None);
        assert_eq!(parse_query("\"unterminated"), Some(term("unterminated")));
        assert_eq!(parse_query("a\tb\nc"), Some(Expr::And(vec![term("a"), term("b"), term("c")])));
        // 引号里 \" 是转义；引号外的反斜杠照字面（搜 Windows 路径 / 正则片段时不用双写）
        assert_eq!(
            parse_query(r#"say \"hi\" "a \"b\" c""#),
            Some(Expr::And(vec![term("say"), term(r#"\"hi\""#), term("a \"b\" c"),]))
        );
    }

    #[test]
    fn parses_boolean_operators() {
        assert_eq!(
            parse_query("AC1062 OR Preparing:"),
            Some(Expr::Or(vec![term("AC1062"), term("Preparing:")]))
        );
        // AND 比 OR 优先
        assert_eq!(
            parse_query("a b OR c AND d"),
            Some(Expr::Or(vec![
                Expr::And(vec![term("a"), term("b")]),
                Expr::And(vec![term("c"), term("d")]),
            ]))
        );
        // 括号分组 + NOT 作用于整组；-( 也行
        assert_eq!(
            parse_query("(a OR b) NOT (c d)"),
            Some(Expr::And(vec![
                Expr::Or(vec![term("a"), term("b")]),
                not(Expr::And(vec![term("c"), term("d")])),
            ]))
        );
        assert_eq!(parse_query("x -(a OR b)"), parse_query("x NOT (a OR b)"));
        // 小写 or 是普通词；引号里的 OR 也是普通词
        assert_eq!(parse_query("a or b"), Some(Expr::And(vec![term("a"), term("or"), term("b")])));
        assert_eq!(
            parse_query(r#"a "OR" b"#),
            Some(Expr::And(vec![term("a"), term("OR"), term("b")]))
        );
        // 双重否定抵消
        assert_eq!(parse_query("NOT -a"), Some(term("a")));
    }

    #[test]
    fn parens_inside_words_are_literal() {
        assert_eq!(
            parse_query("getUser(id) timeout"),
            Some(Expr::And(vec![term("getUser(id)"), term("timeout")]))
        );
        assert_eq!(
            parse_query("(getUser(id) OR foo)"),
            Some(Expr::Or(vec![term("getUser(id)"), term("foo")]))
        );
        assert_eq!(
            parse_query("Exception( -x"),
            Some(Expr::And(vec![term("Exception("), not(term("x"))]))
        );
        assert_eq!(parse_query("(a OR b))"), parse_query("a OR b"));
    }

    #[test]
    fn tolerates_dangling_syntax() {
        assert_eq!(parse_query("OR"), None);
        assert_eq!(parse_query("AND OR NOT ( ) -"), None);
        assert_eq!(parse_query("OR a OR"), Some(term("a")));
        assert_eq!(parse_query("a OR OR b"), Some(Expr::Or(vec![term("a"), term("b")])));
        assert_eq!(parse_query(") a ( b"), Some(Expr::And(vec![term("a"), term("b")])));
        assert_eq!(parse_query(r#""" a"#), Some(term("a")));
        assert_eq!(parse_query("- a"), Some(term("a")));
    }

    #[test]
    fn positive_terms_skip_negated() {
        let e = parse_query(r#"a -b (c OR -d) NOT (e f) "g h""#).unwrap();
        assert_eq!(e.positive_terms(), ["a", "c", "g h"]);
    }

    #[test]
    fn boolean_sql() {
        let table = table();
        let q = LogQueries { database: "logs", table: &table, max_message_chars: 16_384 };
        let sql_of = |s: &str| {
            let filter = LogFilter { range: Some(range()), q: s.into(), ..Default::default() };
            q.search(&filter, Order::Desc, 10, 0).unwrap().sql().to_owned()
        };
        // 全是词的 OR 走 multiSearchAny
        let sql = sql_of("a OR b OR c");
        assert!(
            sql.contains("AND multiSearchAnyCaseInsensitiveUTF8(message, {p2:Array(String)})"),
            "{sql}"
        );
        // 混合的 OR 带括号，里面的 AND 也带括号
        let sql = sql_of("x (a b OR -c)");
        assert!(
            sql.contains(
                "AND positionCaseInsensitiveUTF8(message, {p2:String}) > 0\n  AND ((positionCaseInsensitiveUTF8(message, {p3:String}) > 0 AND positionCaseInsensitiveUTF8(message, {p4:String}) > 0) OR positionCaseInsensitiveUTF8(message, {p5:String}) = 0)"
            ),
            "{sql}"
        );
        // NOT 整组
        let sql = sql_of("NOT (a OR b)");
        assert!(
            sql.contains("AND NOT multiSearchAnyCaseInsensitiveUTF8(message, {p2:Array(String)})"),
            "{sql}"
        );
        let sql = sql_of("NOT (a b)");
        assert!(sql.contains("AND NOT (positionCaseInsensitiveUTF8(message, {p2:String}) > 0 AND positionCaseInsensitiveUTF8(message, {p3:String}) > 0)"), "{sql}");
        // 只有操作符：**WHERE 里**没有 message 条件。只看 WHERE——SELECT 里的
        // `substringUTF8(message, …)` 是截断，不是过滤条件
        let sql = sql_of("OR AND");
        let where_sql = sql.split("\nWHERE ").nth(1).unwrap_or("");
        assert!(!where_sql.contains("message"), "{sql}");
    }

    /// 列表要截 `message`、要带回原始长度，导出不能截。
    ///
    /// 原始长度必须是 `` `app_log`.message `` 而不是 `message`：后者会解析成上面那个截断后的
    /// 别名，量出来永远等于 cap。这条线上踩过（返回 16384 而不是真实的 41149053），
    /// 单测钉住写法。
    #[test]
    fn list_truncates_message_but_export_does_not() {
        let table = table();
        let q = LogQueries { database: "logs", table: &table, max_message_chars: 4096 };
        let filter = LogFilter { range: Some(range()), ..Default::default() };

        let query = q.search(&filter, Order::Desc, 10, 0).unwrap();
        // WHERE 里的 message 必须解析成列，不能是截断后的别名，否则 token 索引失效、
        // 长日志的后半截搜不到（2026-09-30 线上踩过）
        assert!(
            query.settings().contains(&("prefer_column_name_to_alias", "1".to_owned())),
            "{:?}",
            query.settings()
        );
        let search = query.sql().to_owned();
        assert!(search.contains("substringUTF8(message, 1, 4096) AS message"), "{search}");
        assert!(search.contains("lengthUTF8(`app_log`.message) AS message_len"), "{search}");

        let context = q.context("h", "f", 0, true, 5).unwrap().sql().to_owned();
        assert!(context.contains("substringUTF8(message, 1, 4096) AS message"), "{context}");

        // 导出是拿全文的那条路，截了就没意义了
        let export = q.export(&filter, Order::Desc, 10).unwrap().sql().to_owned();
        assert!(export.contains(", message,"), "{export}");
        assert!(!export.contains("substringUTF8"), "{export}");
        assert!(!export.contains("message_len"), "{export}");
    }

    /// ASCII 子串走 text 索引：LIKE 模式小写、转义通配符；短词和中文保持 position。
    #[test]
    fn ascii_substrings_use_the_text_index() {
        let table = table();
        let q = LogQueries { database: "logs", table: &table, max_message_chars: 16_384 };
        let query_of = |s: &str| {
            let filter = LogFilter { range: Some(range()), q: s.into(), ..Default::default() };
            q.search(&filter, Order::Desc, 10, 0).unwrap()
        };

        let query = query_of("WX_RECOGNIZE_SHADOW");
        assert!(query.sql().contains("AND lower(message) LIKE {p2:String}"), "{}", query.sql());
        // 参数按 TSV escaped 格式上 HTTP（`tsv_escape`），LIKE 里的一个 `\` 在这里是两个
        assert_eq!(query.params()[2].1, r"%wx\\_recognize\\_shadow%");

        // 通配符和反斜杠都当字面量
        assert_eq!(like_pattern(r"50%_a\b"), r"%50\%\_a\\b%");

        // 3 个字符以下、中文：ClickHouse 不查索引 / 分词进不去，保持 position
        for s in ["abc", "超时了"] {
            let sql = query_of(s).sql().to_owned();
            assert!(!sql.contains("LIKE"), "{s}: {sql}");
            assert!(
                sql.contains("positionCaseInsensitiveUTF8(message, {p2:String}) > 0"),
                "{s}: {sql}"
            );
        }

        // 按服务筛时不走 text 索引：服务那一段本来就小，LIKE 的词典扫描反而更慢
        let filter = LogFilter {
            range: Some(range()),
            q: "WX_RECOGNIZE_SHADOW".into(),
            dims: vec![("service_name".into(), vec!["dyopen".into()])],
            ..Default::default()
        };
        let sql = q.search(&filter, Order::Desc, 10, 0).unwrap().sql().to_owned();
        assert!(!sql.contains("LIKE"), "{sql}");
        assert!(sql.contains("positionCaseInsensitiveUTF8(message"), "{sql}");
        // 按 pod 筛省不下读量，照样走索引
        let filter = LogFilter {
            range: Some(range()),
            q: "WX_RECOGNIZE_SHADOW".into(),
            dims: vec![("pod".into(), vec!["a".into()])],
            ..Default::default()
        };
        let sql = q.search(&filter, Order::Desc, 10, 0).unwrap().sql().to_owned();
        assert!(sql.contains("lower(message) LIKE"), "{sql}");
        // 按 trace_id 筛时也不走：trace_id 索引已经把范围压得很小
        let filter = LogFilter {
            range: Some(range()),
            q: "WX_RECOGNIZE_SHADOW".into(),
            trace_id: Some("62ef683b4721e46391a6eeffc9cef3c9".into()),
            ..Default::default()
        };
        let sql = q.search(&filter, Order::Desc, 10, 0).unwrap().sql().to_owned();
        assert!(!sql.contains("LIKE"), "{sql}");
        assert!(sql.contains("positionCaseInsensitiveUTF8(message"), "{sql}");

        // 排除词的写法不变；都能走索引的 OR 拆成 LIKE 链（见 substring_sql_follows_the_filter）
        let sql = query_of("-timeout").sql().to_owned();
        assert!(sql.contains("positionCaseInsensitiveUTF8(message, {p2:String}) = 0"), "{sql}");
        let sql = query_of("timeout OR refused").sql().to_owned();
        assert!(
            sql.contains(
                "AND (lower(message) LIKE {p2:String} OR lower(message) LIKE {p3:String})"
            ),
            "{sql}"
        );
    }

    #[test]
    fn inner_tokens_skip_the_edges() {
        assert_eq!(inner_tokens("JDC_MAIN_RETRY_CHECK"), ["retry", "main"]);
        assert_eq!(inner_tokens("job/account/detail"), ["account"]);
        // 两头是分隔符时，挨着它的 token 也是完整的
        assert_eq!(inner_tokens("/api/v1/user/"), ["user", "api"]);
        // 没有中间 token、太短、带中文、够长走 hasToken 的都不收
        for s in ["hit_area", "WX", "a_to_b", "解析_内容_x", "x_ac1062c800014a070cf02cd72b78e8ed_y"]
        {
            assert!(inner_tokens(s).is_empty(), "{s}");
        }
        // 中文不是分隔符：`推送x` 是一个 token，两边的 `_` 照样切
        assert_eq!(inner_tokens("a_推送x_abc_b"), ["abc"]);
    }

    #[test]
    fn substring_sql_follows_the_filter() {
        let table = table();
        let q = LogQueries { database: "logs", table: &table, max_message_chars: 16_384 };
        let service = || vec![("service_name".to_owned(), vec!["dyopen".to_owned()])];
        let sql_of = |text: &str, dims: Vec<(String, Vec<String>)>, trace_id: Option<&str>| {
            let filter = LogFilter {
                range: Some(range()),
                q: text.into(),
                dims,
                trace_id: trace_id.map(Into::into),
                ..Default::default()
            };
            q.search(&filter, Order::Desc, 10, 0).unwrap()
        };

        // 按服务筛：position 在前，中间 token 的 hasAllTokens 在后
        let query = sql_of("JDC_MAIN_RETRY_CHECK", service(), None);
        assert!(
            query.sql().contains(
                "positionCaseInsensitiveUTF8(message, {p3:String}) > 0 AND hasAllTokens(lower(message), {p4:Array(String)})"
            ),
            "{}",
            query.sql()
        );
        assert!(query.params()[4].1.contains("retry"), "{:?}", query.params());
        // 没有中间 token 就只有 position
        let sql = sql_of("hit_area", service(), None).sql().to_owned();
        assert!(!sql.contains("hasAllTokens"), "{sql}");
        // 按 trace_id 筛：只有 position
        let sql = sql_of("JDC_MAIN_RETRY_CHECK", vec![], Some("ab")).sql().to_owned();
        assert!(!sql.contains("hasAllTokens") && !sql.contains("LIKE"), "{sql}");

        // 不带服务：长数字按整词查，短数字、按服务筛时仍是子串
        let query = sql_of("13625825825", vec![], None);
        assert!(
            query.sql().contains("AND hasToken(lower(message), {p2:String})"),
            "{}",
            query.sql()
        );
        assert_eq!(query.params()[2].1, "13625825825");
        let sql = sql_of("1362582", vec![], None).sql().to_owned();
        assert!(sql.contains("lower(message) LIKE"), "{sql}");
        let sql = sql_of("13625825825", service(), None).sql().to_owned();
        assert!(sql.contains("positionCaseInsensitiveUTF8(message"), "{sql}");
        // 整词匹配要报给页面，按服务筛时不报
        let filter = |text: &str, dims| LogFilter { q: text.into(), dims, ..Default::default() };
        assert_eq!(filter("13625825825 超时", vec![]).token_terms(), ["13625825825"]);
        assert!(filter("13625825825", service()).token_terms().is_empty());
        assert_eq!(filter("timeout OR 13625825825", vec![]).token_terms(), ["13625825825"]);
        assert!(filter("超时 OR 13625825825", vec![]).token_terms().is_empty());
        // 排除词不受影响
        let sql = sql_of("-13625825825", vec![], None).sql().to_owned();
        assert!(sql.contains("positionCaseInsensitiveUTF8(message, {p2:String}) = 0"), "{sql}");

        // 不带服务、每个词都能走索引的 OR 拆成 LIKE / hasToken 链
        let sql = sql_of("listEbpAccount OR ebp/advertiser/list OR 13625825825", vec![], None)
            .sql()
            .to_owned();
        assert!(
            sql.contains(
                "AND (lower(message) LIKE {p2:String} OR lower(message) LIKE {p3:String} OR hasToken(lower(message), {p4:String}))"
            ),
            "{sql}"
        );
        // 有一个词走不了索引，或者按服务筛，还是 multiSearchAny
        for (text, dims) in
            [("listEbpAccount OR 超时", vec![]), ("listEbpAccount OR timeout", service())]
        {
            let sql = sql_of(text, dims, None).sql().to_owned();
            assert!(sql.contains("multiSearchAnyCaseInsensitiveUTF8(message"), "{text}: {sql}");
        }
    }

    #[test]
    fn long_alphanumeric_terms_use_the_token_index() {
        let table = table();
        let q = LogQueries { database: "logs", table: &table, max_message_chars: 16_384 };
        let sql_of = |query: &str| {
            let filter = LogFilter { range: Some(range()), q: query.into(), ..Default::default() };
            q.search(&filter, Order::Desc, 10, 0).unwrap().sql().to_owned()
        };

        // 32 位 msgId：整词，走索引，needle 转小写
        let sql = sql_of("AC1062C800014A070CF02CD72B78E8ED");
        assert!(sql.contains("AND hasToken(lower(message), {p2:String})"), "{sql}");
        let filter = LogFilter {
            range: Some(range()),
            q: "AC1062C800014A070CF02CD72B78E8ED".into(),
            ..Default::default()
        };
        let query = q.search(&filter, Order::Desc, 10, 0).unwrap();
        assert_eq!(query.params()[2].1, "ac1062c800014a070cf02cd72b78e8ed");
        assert_eq!(filter.token_terms(), vec!["AC1062C800014A070CF02CD72B78E8ED"]);

        // 短词、切出来全是短词的复合词、中文：都退回子串（hasToken 遇到分隔符会抛异常，
        // needle 只能是切好的 token；而常见词不管怎么组合都跳不掉 granule，见 token_needles）
        for q_str in [
            "health",
            "msgId:",
            "RESULT_CHANGE",
            "im_enter_direct_msg",
            "WX_RECOGNIZE_SHADOW",
            "直播间主动触达",
            "CHANGE|私信手机号为空",
            "订单AC1062C800014A070CF02CD72B78E8ED",
        ] {
            let sql = sql_of(q_str);
            assert!(!sql.contains("hasToken"), "{q_str} 不该走索引: {sql}");
            // 纯 ASCII 的子串交给 text 索引（lower(message) LIKE），中文仍是 position
            if q_str.is_ascii() {
                assert!(sql.contains("lower(message) LIKE {p2:String}"), "{q_str}: {sql}");
            } else {
                assert!(sql.contains("positionCaseInsensitiveUTF8(message"), "{q_str}: {sql}");
            }
        }

        // 键加 id：id 那个 token 进索引跳 granule，子串条件保留在最前面保证键也对得上
        let filter = LogFilter {
            range: Some(range()),
            q: "msgId:AC1062C800014A070CF02CD72B78E8ED".into(),
            ..Default::default()
        };
        let query = q.search(&filter, Order::Desc, 10, 0).unwrap();
        let sql = query.sql();
        assert!(
            sql.contains(
                "AND lower(message) LIKE {p2:String} AND hasToken(lower(message), {p3:String})"
            ),
            "{sql}"
        );
        assert_eq!(query.params()[2].1, "%msgid:ac1062c800014a070cf02cd72b78e8ed%");
        assert_eq!(query.params()[3].1, "ac1062c800014a070cf02cd72b78e8ed");
        assert_eq!(filter.token_terms(), vec!["msgId:AC1062C800014A070CF02CD72B78E8ED"]);
        // 两个 id 各一个 hasToken；复合词的排除仍是子串
        let sql = sql_of("AC1062C800014A070CF02CD72B78E8ED/0123456789abcdef0123456789abcdef");
        assert_eq!(sql.matches("hasToken").count(), 2, "{sql}");
        let sql = sql_of("-msgId:AC1062C800014A070CF02CD72B78E8ED");
        assert!(!sql.contains("hasToken"), "{sql}");

        // 排除词保持子串语义：整词取反会变宽，会漏掉本该排除的行
        let sql = sql_of("-AC1062C800014A070CF02CD72B78E8ED");
        assert!(!sql.contains("hasToken"), "{sql}");
        assert!(sql.contains("positionCaseInsensitiveUTF8(message, {p2:String}) = 0"), "{sql}");

        // 顶层 AND 里的每个词各自判断
        let filter = LogFilter {
            range: Some(range()),
            q: "AC1062C800014A070CF02CD72B78E8ED 超时".into(),
            ..Default::default()
        };
        let sql = q.search(&filter, Order::Desc, 10, 0).unwrap().sql().to_owned();
        assert!(sql.contains("hasToken(lower(message)"), "{sql}");
        assert!(sql.contains("positionCaseInsensitiveUTF8"), "{sql}");
        assert_eq!(filter.token_terms(), vec!["AC1062C800014A070CF02CD72B78E8ED"]);

        // 正则模式不动，也不报整词
        let filter = LogFilter {
            range: Some(range()),
            q: "AC1062C800014A070CF02CD72B78E8ED".into(),
            regex: true,
            ..Default::default()
        };
        assert!(filter.token_terms().is_empty());
        assert!(q.search(&filter, Order::Desc, 10, 0).unwrap().sql().contains("match(message"));
    }

    #[test]
    fn select_skips_columns_the_page_cannot_show() {
        // 线上的日志表物理上带着整套 span 列（logpipe 不写，全是默认值）
        let mut t = table();
        for (name, ty) in [
            ("span_attributes", "JSON"),
            ("events.attributes", "Array(JSON)"),
            ("labels", "Map(String, String)"),
            ("duration_ns", "UInt64"),
        ] {
            t.columns.push(Column {
                name: name.to_owned(),
                ty: ty.to_owned(),
                kind: ColumnKind::classify(ty),
            });
        }
        let q = LogQueries { database: "logs", table: &t, max_message_chars: 16_384 };
        let filter = LogFilter { range: Some(range()), ..Default::default() };
        let sql = q.search(&filter, Order::Desc, 10, 0).unwrap();
        for skipped in ["span_attributes", "events.attributes", "labels"] {
            assert!(!sql.sql().contains(skipped), "{skipped} 不该取: {}", sql.sql());
        }
        // 数字列还是取的：静态 fields 里可能有
        assert!(sql.sql().contains("`duration_ns`"), "{}", sql.sql());
    }

    #[test]
    fn search_binds_everything() {
        let table = table();
        let q = LogQueries { database: "logs", table: &table, max_message_chars: 16_384 };
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
        assert!(sql.ends_with("ORDER BY timestamp DESC, level DESC, trace_id DESC\nLIMIT {p8:UInt32} OFFSET {p9:UInt32}"), "{sql}");
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
        let q = LogQueries { database: "logs", table: &table, max_message_chars: 16_384 };
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
        let q = LogQueries { database: "logs", table: &table, max_message_chars: 16_384 };
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
        let q = LogQueries { database: "logs", table: &table, max_message_chars: 16_384 };
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

        // 几个维度一条查询，一个维度一列，列名就是维度名（前端按它对号入座）
        let f = q.facets(&filter, &["pod", "host"], 50).unwrap();
        assert!(
            f.sql().contains(
                "SELECT arrayMap(t -> (t.1, toFloat64(t.2)), approx_top_k(50)(`pod`)) AS `pod`"
            ),
            "{}",
            f.sql()
        );
        assert!(f.sql().contains("approx_top_k(50)(`host`)) AS `host`"), "{}", f.sql());
        // 一个不能筛的列就整条拒绝，不能悄悄少给一个下拉
        assert!(q.facets(&filter, &["pod", "message"], 50).is_err());
        assert!(q.facets(&filter, &["replica"], 50).is_err(), "数字列不做 facet");
        assert!(q.facets(&filter, &["nope"], 50).is_err());
        assert!(q.facets(&filter, &[], 50).is_err(), "一个维度都不给");

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
        assert!(c.sql().contains("ORDER BY timestamp DESC, level DESC, trace_id DESC"));
        assert_eq!(c.params()[3].1, CONTEXT_WINDOW_MS.to_string());
        let c = q.context("node-1", "/var/log/x.log", 1_500, false, 50).unwrap();
        assert!(
            c.sql().contains("timestamp > fromUnixTimestamp64Milli({p2:Int64})"),
            "{}",
            c.sql()
        );
        assert!(c.sql().contains("ORDER BY timestamp ASC, level ASC, trace_id ASC"));

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
