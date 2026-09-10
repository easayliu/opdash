//! SQL 构造。每个页面一个子模块，这里是共用的：时间范围、直方图分桶、列名白名单。
//!
//! 约定：所有子模块只产出 [`crate::clickhouse::Query`]（SQL 文本 + 绑定参数），不碰网络，
//! 这样单元测试直接断言 SQL 和参数即可。

use std::time::Duration;

use chrono::{TimeZone, Utc};
use chrono_tz::Tz;

use crate::clickhouse::{Query, ToParam};
use crate::error::{Error, Result};

pub mod logs;
pub mod metrics;
pub mod traces;

/// 参数绑定收集器：每 `bind` 一次得到一个 `{pN:Type}` 占位符，最后连同 SQL 拼成 [`Query`]。
///
/// SQL 是按筛选条件动态拼的，参数个数不定，用编号比手写名字省事也不会撞名。
#[derive(Debug, Default)]
pub struct Bindings {
    params: Vec<(String, String)>,
}

impl Bindings {
    pub fn new() -> Self {
        Self::default()
    }

    /// 绑定一个值，返回可以直接写进 SQL 的占位符，如 `{p3:String}`。
    pub fn bind(&mut self, ty: &str, value: impl ToParam) -> String {
        let name = format!("p{}", self.params.len());
        self.params.push((name.clone(), value.to_param()));
        format!("{{{name}:{ty}}}")
    }

    /// `col >= from AND col < to`，参数就地绑定。
    pub fn time_predicate(&mut self, column: &str, range: &TimeRange) -> String {
        let from = self.bind("Int64", range.from_ms);
        let to = self.bind("Int64", range.to_ms);
        format!(
            "{column} >= fromUnixTimestamp64Milli({from}) AND {column} < fromUnixTimestamp64Milli({to})"
        )
    }

    pub fn into_query(self, sql: impl Into<String>) -> Query {
        Query::new(sql).with_params(self.params)
    }

    pub fn params(&self) -> &[(String, String)] {
        &self.params
    }
}

/// 查询的时间范围，左闭右开，unix 毫秒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub from_ms: i64,
    pub to_ms: i64,
}

impl TimeRange {
    /// 校验用户给的范围。缺省：截止到现在、往前一小时。
    pub fn new(
        from: Option<i64>,
        to: Option<i64>,
        now_ms: i64,
        max_range: Duration,
    ) -> Result<Self> {
        let to_ms = to.unwrap_or(now_ms);
        let from_ms = from.unwrap_or(to_ms - 3_600_000);
        if from_ms < 0 || to_ms < 0 {
            return Err(Error::bad_request("时间参数应为 unix 毫秒"));
        }
        if from_ms >= to_ms {
            return Err(Error::bad_request("开始时间必须早于结束时间"));
        }
        let max_ms = max_range.as_millis() as i64;
        if to_ms - from_ms > max_ms {
            return Err(Error::bad_request(format!(
                "时间范围不能超过 {}",
                humantime::format_duration(Duration::from_secs(max_range.as_secs()))
            )));
        }
        Ok(Self { from_ms, to_ms })
    }

    pub fn span_ms(&self) -> i64 {
        self.to_ms - self.from_ms
    }

    /// 前后各放宽 `ms`（trace 聚合时其它 span 可能跨出筛选范围）。
    pub fn widen(&self, ms: i64) -> Self {
        Self { from_ms: (self.from_ms - ms).max(0), to_ms: self.to_ms + ms }
    }

    /// `timestamp >= ... AND timestamp < ...` 片段，参数名固定 `from` / `to`。
    /// `fromUnixTimestamp64Milli` 在参数代入后是常量，ClickHouse 能据此裁剪分区、走主键。
    pub fn predicate(&self, column: &str) -> String {
        format!(
            "{column} >= fromUnixTimestamp64Milli({{from:Int64}}) AND {column} < fromUnixTimestamp64Milli({{to:Int64}})"
        )
    }
}

/// 直方图分桶：宽度从阶梯里挑，原点对齐到 `tz` 的本地零点。
///
/// 不用 `toStartOfInterval(ts, INTERVAL n SECOND)`：它按 UTC 的秒数取整，6 小时一桶时边界落在
/// 北京时间 02 / 08 / 14 / 20 点，一天一桶时边界是早上 8 点。这里改成
/// `intDiv(toUnixTimestamp64Milli(ts) - origin, width)`，原点取范围起点那天的本地零点，
/// 桶宽只要能整除一天（阶梯里的都能），边界就都落在本地的整点 / 零点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub width_ms: i64,
    pub origin_ms: i64,
}

const SECOND: i64 = 1_000;
const MINUTE: i64 = 60 * SECOND;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

/// 桶宽阶梯。都能整除一天，7 天那档整除一周。
const LADDER: &[i64] = &[
    SECOND,
    2 * SECOND,
    5 * SECOND,
    10 * SECOND,
    15 * SECOND,
    30 * SECOND,
    MINUTE,
    2 * MINUTE,
    5 * MINUTE,
    10 * MINUTE,
    15 * MINUTE,
    30 * MINUTE,
    HOUR,
    2 * HOUR,
    3 * HOUR,
    6 * HOUR,
    12 * HOUR,
    DAY,
    2 * DAY,
    7 * DAY,
];

impl Bucket {
    /// 选最小的、让桶数不超过 `max_buckets` 的宽度。
    pub fn choose(range: &TimeRange, tz: Tz, max_buckets: i64) -> Self {
        let span = range.span_ms().max(1);
        let width_ms = LADDER
            .iter()
            .copied()
            .find(|w| span / w <= max_buckets)
            .unwrap_or(*LADDER.last().expect("ladder non-empty"));
        Self { width_ms, origin_ms: local_midnight_ms(range.from_ms, tz) }
    }

    /// 指定桶宽（指标页让用户自己选步长）。宽度对齐到本地零点的规则不变；
    /// 宽度整除不了一天时（比如 45s）边界只对齐到范围起点那天的零点，够用。
    pub fn with_width(range: &TimeRange, tz: Tz, width_ms: i64) -> Self {
        Self { width_ms: width_ms.max(1), origin_ms: local_midnight_ms(range.from_ms, tz) }
    }

    /// 桶序号表达式（整数），参数名固定 `bucket_origin` / `bucket_width`。
    pub fn index_expr(&self, column: &str) -> String {
        format!(
            "intDiv(toUnixTimestamp64Milli({column}) - {{bucket_origin:Int64}}, {{bucket_width:Int64}})"
        )
    }

    /// 序号 → 桶起点毫秒。
    pub fn start_ms(&self, index: i64) -> i64 {
        self.origin_ms + index * self.width_ms
    }

    /// 范围内桶的数量（含首尾不完整的）。
    pub fn count(&self, range: &TimeRange) -> i64 {
        let first = (range.from_ms - self.origin_ms).div_euclid(self.width_ms);
        let last = (range.to_ms - 1 - self.origin_ms).div_euclid(self.width_ms);
        last - first + 1
    }

    pub fn first_index(&self, range: &TimeRange) -> i64 {
        (range.from_ms - self.origin_ms).div_euclid(self.width_ms)
    }
}

/// `ts_ms` 所在本地日（按 `tz`）的零点，unix 毫秒。
pub fn local_midnight_ms(ts_ms: i64, tz: Tz) -> i64 {
    let utc = Utc.timestamp_millis_opt(ts_ms).single().unwrap_or_else(Utc::now);
    let local = utc.with_timezone(&tz);
    let date = local.date_naive();
    // 个别时区某天零点不存在（夏令时跳过），取最早可用的时刻
    match tz.from_local_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight")).earliest() {
        Some(midnight) => midnight.timestamp_millis(),
        None => local.timestamp_millis() - (local.timestamp_millis().rem_euclid(DAY)),
    }
}

/// 列名进 SQL 前的最后一道关：只接受白名单里查出来的名字，这里再兜一次不含反引号 / 反斜杠 /
/// 控制字符，然后反引号引用。
pub fn quote_ident(name: &str) -> Result<String> {
    if name.is_empty()
        || name.len() > 128
        || name.chars().any(|c| c == '`' || c == '\\' || c.is_control())
    {
        return Err(Error::bad_request(format!("非法列名: {name:?}")));
    }
    Ok(format!("`{name}`"))
}

/// 解析 `--timezone` 这类 IANA 时区名。
pub fn parse_tz(name: &str) -> Result<Tz> {
    name.parse::<Tz>().map_err(|_| Error::bad_request(format!("不认识的时区: {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_788_857_535_000; // 2026-09-08 16:52:15 +08:00

    #[test]
    fn time_range_defaults_and_validation() {
        let r = TimeRange::new(None, None, NOW, Duration::from_secs(86400)).unwrap();
        assert_eq!(r.to_ms, NOW);
        assert_eq!(r.from_ms, NOW - 3_600_000);
        assert!(TimeRange::new(Some(NOW), Some(NOW), NOW, Duration::from_secs(86400)).is_err());
        assert!(
            TimeRange::new(Some(NOW - 2 * 86_400_000), Some(NOW), NOW, Duration::from_secs(86400))
                .is_err()
        );
        assert!(TimeRange::new(Some(-1), Some(NOW), NOW, Duration::from_secs(86400)).is_err());
        let w = r.widen(60_000);
        assert_eq!(w.from_ms, r.from_ms - 60_000);
        assert_eq!(w.to_ms, r.to_ms + 60_000);
        assert!(r.predicate("timestamp").contains("fromUnixTimestamp64Milli({from:Int64})"));
    }

    #[test]
    fn bucket_aligns_to_local_midnight() {
        let tz: Tz = "Asia/Shanghai".parse().unwrap();
        // 2026-09-08 01:00 +08:00
        let from = 1_788_800_400_000;
        let range = TimeRange { from_ms: from, to_ms: from + 6 * HOUR };
        let b = Bucket::choose(&range, tz, 120);
        assert_eq!(b.width_ms, 5 * MINUTE); // 6h / 120 = 3min → 阶梯上取 5min
        // 原点是 2026-09-08 00:00 +08:00 = 2026-09-07 16:00 UTC
        assert_eq!(b.origin_ms, 1_788_796_800_000);
        assert_eq!(b.count(&range), 72);
        assert_eq!(b.start_ms(b.first_index(&range)), from);

        let day_range = TimeRange { from_ms: from, to_ms: from + 30 * DAY };
        let b = Bucket::choose(&day_range, tz, 120);
        assert_eq!(b.width_ms, 6 * HOUR);
        assert_eq!(b.start_ms(0), 1_788_796_800_000);
    }

    #[test]
    fn bucket_never_exceeds_target() {
        let tz: Tz = "Asia/Shanghai".parse().unwrap();
        for span in [1, SECOND, 90 * SECOND, HOUR, 25 * HOUR, 31 * DAY] {
            let range = TimeRange { from_ms: NOW - span, to_ms: NOW };
            let b = Bucket::choose(&range, tz, 120);
            assert!(b.count(&range) <= 121, "span {span} → {} buckets", b.count(&range));
        }
    }

    #[test]
    fn bindings_number_placeholders() {
        let mut b = Bindings::new();
        let a = b.bind("String", "x");
        let range = TimeRange { from_ms: 1, to_ms: 2 };
        let t = b.time_predicate("timestamp", &range);
        assert_eq!(a, "{p0:String}");
        assert_eq!(
            t,
            "timestamp >= fromUnixTimestamp64Milli({p1:Int64}) AND timestamp < fromUnixTimestamp64Milli({p2:Int64})"
        );
        let q = b.into_query("SELECT 1");
        assert_eq!(q.params().len(), 3);
        assert_eq!(q.params()[1], ("p1".to_owned(), "1".to_owned()));
    }

    #[test]
    fn quote_ident_rejects_injection() {
        assert_eq!(quote_ident("pod").unwrap(), "`pod`");
        assert_eq!(quote_ident("events.name").unwrap(), "`events.name`");
        assert!(quote_ident("a`b").is_err());
        assert!(quote_ident("a\\b").is_err());
        assert!(quote_ident("").is_err());
    }
}
