//! 导出 CSV 时防公式注入（CSV injection）。
//!
//! 日志的 message、属性是业务服务写的，外部请求能间接控制它（请求参数、User-Agent 原样打进日志）。
//! 以 `=` / `+` / `-` / `@` 开头的单元格在 Excel、WPS 里会被当成公式：`=HYPERLINK("http://…?"&A2)`
//! 能把旁边单元格的内容带出去，老版本 Excel 还能经 DDE 执行命令。按 OWASP 的做法，这类单元格
//! 前面补一个 `'`，表格软件就把它当文本。
//!
//! 导出是 ClickHouse 直接出的 `CSVWithNames`，opdash 只转发字节流。这里在转发时逐字节过一遍，
//! 不整体读进内存。ClickHouse 的 CSV 里字符串（以及日期、数组这类）一律加双引号，数字不加，
//! 所以**只改引号里的字段**：负数金额 `-12.5` 不带引号，不会被改成 `'-12.5`。

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

/// 开头是这些字符的单元格会被表格软件当成公式（Tab、回车开头的也会被一些软件去掉后再判断）。
const FORMULA_LEAD: &[u8] = b"=+-@\t\r";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// 在一个字段的开头
    FieldStart,
    /// 刚过开引号，下一个字节是字段内容的第一个字节
    QuotedFirst,
    /// 在引号里
    Quoted,
    /// 引号里遇到一个 `"`：后面再跟 `"` 是转义，否则字段结束
    QuoteSeen,
    /// 不带引号的字段（数字、`\N`）
    Unquoted,
}

/// 跨分块保留状态的改写器：一个字段可能被网络分块切在任何位置。
#[derive(Debug)]
pub struct FormulaGuard {
    state: State,
}

impl Default for FormulaGuard {
    fn default() -> Self {
        Self { state: State::FieldStart }
    }
}

impl FormulaGuard {
    /// 改写一块。只会插入 `'`，不会删改原有字节；多字节 UTF-8 的字节都 ≥ 0x80，不会误判。
    pub fn feed(&mut self, chunk: &[u8]) -> Bytes {
        let mut out = Vec::with_capacity(chunk.len() + 16);
        for &b in chunk {
            self.state = match self.state {
                State::QuotedFirst if FORMULA_LEAD.contains(&b) => {
                    out.push(b'\'');
                    State::Quoted
                }
                State::QuotedFirst | State::Quoted => {
                    if b == b'"' {
                        State::QuoteSeen
                    } else {
                        State::Quoted
                    }
                }
                State::QuoteSeen if b == b'"' => State::Quoted,
                State::FieldStart if b == b'"' => State::QuotedFirst,
                State::FieldStart | State::QuoteSeen | State::Unquoted => {
                    if b == b',' || b == b'\n' { State::FieldStart } else { State::Unquoted }
                }
            };
            out.push(b);
        }
        Bytes::from(out)
    }
}

/// 把 ClickHouse 回的 CSV 字节流套上 [`FormulaGuard`]。
pub fn guard<S, E>(stream: S) -> impl Stream<Item = Result<Bytes, E>>
where
    S: Stream<Item = Result<Bytes, E>>,
{
    let mut g = FormulaGuard::default();
    stream.map(move |chunk| chunk.map(|c| g.feed(&c)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> String {
        let mut g = FormulaGuard::default();
        let out: Vec<u8> = chunks.iter().flat_map(|c| g.feed(c.as_bytes()).to_vec()).collect();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn prefixes_quoted_cells_that_look_like_formulas() {
        let csv = "\"ts\",\"message\",\"n\"\n\
                   \"2026-01-01\",\"=HYPERLINK(\"\"http://x/\"\")\",-12.5\n\
                   \"2026-01-01\",\"+1\",3\n\
                   \"2026-01-01\",\"@SUM(A1)\",\\N\n\
                   \"2026-01-01\",\"-cmd\",0\n\
                   \"2026-01-01\",\"\tx\",0\n\
                   \"2026-01-01\",\"a=1\",0\n\
                   \"2026-01-01\",\"\",0\n\
                   \"2026-01-01\",\"\"\"=q\"\"\",0\n\
                   \"2026-01-01\",\"中文=1\",0\n";
        let want = "\"ts\",\"message\",\"n\"\n\
                    \"2026-01-01\",\"'=HYPERLINK(\"\"http://x/\"\")\",-12.5\n\
                    \"2026-01-01\",\"'+1\",3\n\
                    \"2026-01-01\",\"'@SUM(A1)\",\\N\n\
                    \"2026-01-01\",\"'-cmd\",0\n\
                    \"2026-01-01\",\"'\tx\",0\n\
                    \"2026-01-01\",\"a=1\",0\n\
                    \"2026-01-01\",\"\",0\n\
                    \"2026-01-01\",\"\"\"=q\"\"\",0\n\
                    \"2026-01-01\",\"中文=1\",0\n";
        assert_eq!(run(&[csv]), want);
    }

    #[test]
    fn keeps_state_across_chunk_boundaries() {
        let csv = "\"a\",\"=1\",-2\n\"x,\"\"y\",\"@z\"\n";
        let want = "\"a\",\"'=1\",-2\n\"x,\"\"y\",\"'@z\"\n";
        // 在每一个位置切开都要得到同样的结果
        for i in 0..=csv.len() {
            assert_eq!(run(&[&csv[..i], &csv[i..]]), want, "切在 {i}");
        }
        // 引号里的逗号、换行、转义引号不会让它错认字段开头
        assert_eq!(run(&["\"a\n=b\",\"c\"\"\n-d\"\n"]), "\"a\n=b\",\"c\"\"\n-d\"\n");
    }
}
