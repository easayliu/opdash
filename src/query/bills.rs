//! 账单表（goscan 写的三张表）的 SQL。
//!
//! goscan 是四兄弟里唯一「去云厂商那儿拉回来」的：它按账期把火山引擎、阿里云的账单批量写进
//! 同一个库，所以这里的查询和日志 / 链路 / 指标那三套完全不同——**没有时间戳流，只有账期**
//! （`YYYY-MM`），时间范围是「哪几个月」，不是 unix 毫秒。
//!
//! ## 三张表，两种列名口径
//!
//! 火山的列名是 API 原样的 PascalCase（`BillPeriod` / `PayableAmount`），阿里云是 snake_case
//! （`billing_cycle` / `pretax_amount`），goscan 明确没有硬统一成一套「通用列」。所以跨云的
//! 统一维度在这里做：[`Dimension`] 和 [`Amount`] 各给一朵云一个表达式，SQL 里 `AS` 成同一个
//! 别名，上层就只看得到统一后的形状。
//!
//! **火山的金额是 `String`**（API 原样返回，避免精度和空值问题），所以每个金额表达式都要
//! `toFloat64OrZero`；阿里云的是 `Float64`，直接用。
//!
//! ## 为什么查询里还要自己去重
//!
//! 三张表都是 `ReplacingMergeTree`，本意是「同一个账期重复拉不会翻倍」。但线上这三张表是按
//! 集群模式建的，Distributed 的分片键是 **`rand()`**（2026-09-22 查 `system.tables` 确认）：
//! 重复拉的那一份会随机落到**另一个分片**上，而 `ReplacingMergeTree` 的去重只发生在分片内的
//! merge 里，`FINAL` 同理——跨分片的重复谁都收不掉。goscan 的日调度每天把当月重拉一遍，
//! 一个月下来同一行最多能有三份。
//!
//! 所以默认（[`Dedupe::Group`]）在查询里按**建表时的排序键**再去重一次：排序键就是
//! `ReplacingMergeTree` 的去重键，按它 `GROUP BY` 得到的结果和引擎自己去重后的完全一致，
//! 而且跨分片也对。代价是要把这几个月的数据读出来聚一次——账单一个月几万到几十万行，
//! 比日志表小四个数量级，这个代价可以忽略。
//!
//! 去重键从 `system.tables.sorting_key` 读（见 [`crate::schema::BillTable`]），读不到才退回
//! 这里的静态定义：改了 goscan 的 DDL 而没改 opdash 时，去重键跟着表走，不会悄悄少算钱。
//!
//! 根治办法在 README「账单表的分片键该改」一节：把 Distributed 的分片键换成按去重键哈希、
//! 排序键里别放金额列，那时把 `--bill-dedupe` 调成 `final` 就行。

use serde::{Deserialize, Serialize};

use super::{Bindings, quote_ident};
use crate::alloc::{Alloc, ColumnOp, Matcher, Prepaid};
use crate::clickhouse::{Query, num};
use crate::error::{Error, Result};
use crate::schema::{BillTable, ColumnKind};

/// 一次最多查多少个账期。账单按月存，36 个月已经是「看三年趋势」，再多只是把 SQL 拉长。
pub const MAX_PERIODS: usize = 36;

/// 默认看最近几个账期。
pub const DEFAULT_PERIODS: usize = 6;

/// 明细页一次最多返回多少行。
pub const MAX_DETAIL_ROWS: u32 = 1_000;

/// goscan 写入时间，也是三张表 `ReplacingMergeTree` 的版本列。去重时据它分辨「重拉留下的
/// 旧版本」与「同一次同步里并列的几行」。
const VERSION_COLUMN: &str = "updated_at";

/// 同一次同步写入的行，彼此相隔不会超过这么久。线上 2026-06 至 08 月，同键的并列行最多相隔
/// 2.2 秒；而同一账期的两次同步至少相隔数分钟（一次同步本身就要这么久，goscan 也不允许并发）。
const SYNC_WINDOW_SECS: u32 = 60;

/// 服务期那两列（只有阿里云的账单有）。预付费按它摊到各月。
const SERVICE_PERIOD: &str = "service_period";
const SERVICE_PERIOD_UNIT: &str = "service_period_unit";

/// 服务期换算成月数，口径与手工台账（update_alifee_xlsx）一致：一年 12 个月，按天记的
/// ÷ 30 取整（365 天正好 12 个月，升降配那种「剩余 447 天」是 15 个月），按秒记的
/// ÷ 86400 ÷ 30 取整——域名续费就是按秒记的（29511729 秒 ≈ 11 个月）。算不出来的按一个月计，
/// 至少落在购买当月。
fn months_expr() -> String {
    let (unit, period) = (SERVICE_PERIOD_UNIT, SERVICE_PERIOD);
    format!(
        "toUInt32(greatest(1, round(multiIf(
           {unit} IN ('年', 'Year', 'year', 'years'), toFloat64OrZero({period}) * 12,
           {unit} IN ('月', 'Month', 'month', 'months'), toFloat64OrZero({period}),
           {unit} IN ('天', '日', 'Day', 'day', 'days'), toFloat64OrZero({period}) / 30,
           {unit} IN ('小时', 'Hour', 'hour', 'hours'), toFloat64OrZero({period}) / 24 / 30,
           {unit} IN ('秒', 'Second', 'second', 'seconds'), toFloat64OrZero({period}) / 86400 / 30,
           1))))"
    )
}

/// 账单查询怎么去重，见模块文档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Dedupe {
    /// 按排序键 `GROUP BY`（默认）
    Group,
    /// 给表加 `FINAL`
    Final,
    /// 不去重
    Off,
}

/// 哪朵云。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Volcengine,
    Alicloud,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Volcengine => "volcengine",
            Provider::Alicloud => "alicloud",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::Volcengine => "火山引擎",
            Provider::Alicloud => "阿里云",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim() {
            "volcengine" | "volc" => Ok(Provider::Volcengine),
            "alicloud" | "aliyun" | "ali" => Ok(Provider::Alicloud),
            other => Err(Error::bad_request(format!(
                "provider 只能是 volcengine 或 alicloud，不是 {other:?}"
            ))),
        }
    }
}

/// 哪张表。阿里云有月度和日度两张，内容是同一批账单的两种粒度，**绝不能加在一起**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Volcengine,
    AlicloudMonthly,
    AlicloudDaily,
}

impl Kind {
    pub fn provider(self) -> Provider {
        match self {
            Kind::Volcengine => Provider::Volcengine,
            Kind::AlicloudMonthly | Kind::AlicloudDaily => Provider::Alicloud,
        }
    }

    /// 这张表能不能按天看。火山的明细自带 `ExpenseDate`，阿里云要用日度那张表。
    pub fn has_days(self) -> bool {
        !matches!(self, Kind::AlicloudMonthly)
    }

    /// goscan 当前 DDL 里的排序键 = `ReplacingMergeTree` 的去重键。只在
    /// `system.tables.sorting_key` 读不到时用，见模块文档。阿里云这套对应 goscan v0.5 的 DDL：
    /// 加了 `item`（订单 / 后付账单 / 退款 / 调账）和 `line_seq`（同一次拉取里撞键的行按序编号），
    /// 每一行账单从此有唯一的键。
    pub fn fallback_dedupe(self) -> &'static [&'static str] {
        match self {
            Kind::Volcengine => &[
                "BillPeriod",
                "ExpenseDate",
                "InstanceNo",
                "ExpenseBeginTime",
                "Product",
                "ElementCode",
                "BillDetailId",
            ],
            Kind::AlicloudMonthly => &[
                "billing_cycle",
                "product_code",
                "instance_id",
                "bill_account_id",
                "subscription_type",
                "billing_type",
                "item",
                "biz_type",
                "product_detail_code",
                "region",
                "zone",
                "split_item_id",
                "line_seq",
            ],
            Kind::AlicloudDaily => &[
                "billing_date",
                "product_code",
                "instance_id",
                "bill_account_id",
                "subscription_type",
                "billing_type",
                "item",
                "biz_type",
                "product_detail_code",
                "region",
                "zone",
                "split_item_id",
                "line_seq",
            ],
        }
    }

    /// 账期表达式，一律是 `YYYY-MM` 的字符串。
    fn period_expr(self) -> &'static str {
        match self {
            Kind::Volcengine => "BillPeriod",
            Kind::AlicloudMonthly => "billing_cycle",
            // 日度表的 billing_cycle 不在排序键上，按日期现算更稳
            Kind::AlicloudDaily => "formatDateTime(billing_date, '%Y-%m')",
        }
    }

    /// 日期表达式，`YYYY-MM-DD`；月度账单没有日期，回空串。
    ///
    /// 火山的 `ExpenseDate` 是 `String`，用 `toDateOrNull` 而不是 `toDate`：空值 / 脏值只会变成
    /// 空串，不会让整条查询抛异常。
    fn date_expr(self) -> &'static str {
        match self {
            Kind::Volcengine => "ifNull(toString(toDateOrNull(ExpenseDate)), '')",
            Kind::AlicloudMonthly => "''",
            Kind::AlicloudDaily => "toString(billing_date)",
        }
    }

    /// 同上，但仍是 `Date` 类型——要和日期比大小（「最近 N 天」）时用它，
    /// [`Self::date_expr`] 出来的是字符串，减不得。月度表没有日期，回 `None`。
    fn day_expr(self) -> Option<&'static str> {
        match self {
            Kind::Volcengine => Some("toDateOrNull(ExpenseDate)"),
            Kind::AlicloudMonthly => None,
            Kind::AlicloudDaily => Some("billing_date"),
        }
    }
}

/// 看哪一种金额。三朵云的叫法不一样，这里按含义对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Amount {
    /// 应付金额（优惠后、该付多少）
    Payable,
    /// 现金支付金额
    Paid,
    /// 原价（优惠前）
    Original,
}

impl Amount {
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.unwrap_or("payable") {
            "payable" => Ok(Amount::Payable),
            "paid" => Ok(Amount::Paid),
            "original" => Ok(Amount::Original),
            other => Err(Error::bad_request(format!(
                "amount 只能是 payable / paid / original，不是 {other:?}"
            ))),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Amount::Payable => "应付金额",
            Amount::Paid => "现金支付",
            Amount::Original => "原价",
        }
    }

    /// 这一种金额对应哪一列。**转成 Float64 的那一步不在这里**：库里的列类型随 goscan 的版本
    /// 变（见 [`BillQueries::amount_expr`]），得看着表结构选转换函数。
    fn column(self, kind: Kind) -> &'static str {
        match (kind, self) {
            (Kind::Volcengine, Amount::Payable) => "PayableAmount",
            (Kind::Volcengine, Amount::Paid) => "PaidAmount",
            (Kind::Volcengine, Amount::Original) => "OriginalBillAmount",
            (_, Amount::Payable) => "pretax_amount",
            (_, Amount::Paid) => "payment_amount",
            (_, Amount::Original) => "pretax_gross_amount",
        }
    }
}

/// 跨云统一的分组维度。两朵云都有这九个，所以任何一个都能跨云汇总。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Product,
    /// 计费项（同一个产品下的细分，如「按量计费-CPU」）
    Item,
    Region,
    Zone,
    Account,
    Instance,
    Project,
    /// 计费模式 / 订阅类型（包年包月、按量）
    Subscription,
    Currency,
}

/// 能在查询串里用的维度名，也是前端下拉里的那几项。
pub const DIMENSIONS: &[(&str, Dimension, &str)] = &[
    ("product", Dimension::Product, "产品"),
    ("item", Dimension::Item, "计费项"),
    ("region", Dimension::Region, "地域"),
    ("zone", Dimension::Zone, "可用区"),
    ("account", Dimension::Account, "账号"),
    ("instance", Dimension::Instance, "实例"),
    ("project", Dimension::Project, "项目 / 资源组"),
    ("subscription", Dimension::Subscription, "计费模式"),
    ("currency", Dimension::Currency, "币种"),
];

impl Dimension {
    pub fn parse(raw: &str) -> Result<Self> {
        DIMENSIONS.iter().find(|(name, _, _)| *name == raw.trim()).map(|(_, d, _)| *d).ok_or_else(
            || {
                Error::bad_request(format!(
                    "不认识的维度 {raw:?}；可用: {}",
                    DIMENSIONS.iter().map(|(n, _, _)| *n).collect::<Vec<_>>().join(", ")
                ))
            },
        )
    }

    pub fn as_str(self) -> &'static str {
        DIMENSIONS.iter().find(|(_, d, _)| *d == self).map(|(n, _, _)| *n).unwrap_or("product")
    }

    pub fn label(self) -> &'static str {
        DIMENSIONS.iter().find(|(_, d, _)| *d == self).map(|(_, _, l)| *l).unwrap_or("产品")
    }

    /// 取值表达式。两朵云的列名口径不同，而且都可能是空串——空的时候退到编码列，
    /// 不然图上会出现一大块「空白」分类，看不出是什么。
    fn expr(self, kind: Kind) -> &'static str {
        match kind {
            Kind::Volcengine => match self {
                Dimension::Product => "if(ProductZh != '', ProductZh, Product)",
                Dimension::Item => "if(Element != '', Element, ElementCode)",
                Dimension::Region => "if(Region != '', Region, RegionCode)",
                Dimension::Zone => "if(Zone != '', Zone, ZoneCode)",
                Dimension::Account => {
                    "multiIf(OwnerCustomerName != '', OwnerCustomerName, OwnerUserName != '', OwnerUserName, OwnerID)"
                }
                Dimension::Instance => "if(InstanceName != '', InstanceName, InstanceNo)",
                Dimension::Project => "if(ProjectDisplayName != '', ProjectDisplayName, Project)",
                Dimension::Subscription => "BillingMode",
                Dimension::Currency => "Currency",
            },
            _ => match self {
                Dimension::Product => "if(product_name != '', product_name, product_code)",
                Dimension::Item => "if(product_detail != '', product_detail, product_type)",
                Dimension::Region => "region",
                Dimension::Zone => "zone",
                Dimension::Account => {
                    "if(bill_account_name != '', bill_account_name, bill_account_id)"
                }
                Dimension::Instance => "if(instance_name != '', instance_name, instance_id)",
                Dimension::Project => "if(resource_group != '', resource_group, cost_unit)",
                Dimension::Subscription => "subscription_type",
                Dimension::Currency => "currency",
            },
        }
    }
}

/// 账期区间，左右都闭，都是 `YYYY-MM`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodRange {
    pub from: String,
    pub to: String,
}

impl PeriodRange {
    /// 没给就是「截止到 `current` 的最近 [`DEFAULT_PERIODS`] 个账期」。
    pub fn new(from: Option<&str>, to: Option<&str>, current: &str) -> Result<Self> {
        let to = match to {
            Some(raw) => check_period(raw)?,
            None => check_period(current)?,
        };
        let from = match from {
            Some(raw) => check_period(raw)?,
            None => shift_period(&to, -(DEFAULT_PERIODS as i32 - 1))?,
        };
        if from > to {
            return Err(Error::bad_request(format!("起始账期 {from} 晚于结束账期 {to}")));
        }
        let range = Self { from, to };
        if range.len() > MAX_PERIODS {
            return Err(Error::bad_request(format!(
                "单次最多查询 {MAX_PERIODS} 个账期，本次为 {} 个",
                range.len()
            )));
        }
        Ok(range)
    }

    /// 区间里有几个账期。
    pub fn len(&self) -> usize {
        match (month_index(&self.from), month_index(&self.to)) {
            (Some(a), Some(b)) if b >= a => (b - a + 1) as usize,
            _ => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 区间里的每一个账期，从早到晚。
    pub fn periods(&self) -> Vec<String> {
        let Some(start) = month_index(&self.from) else { return Vec::new() };
        (0..self.len() as i32).filter_map(|i| period_from_index(start + i)).collect()
    }

    /// 紧挨着的前一段同样长的区间（算环比用）。
    pub fn previous(&self) -> Option<Self> {
        let len = self.len() as i32;
        if len == 0 {
            return None;
        }
        Some(Self {
            from: shift_period(&self.from, -len).ok()?,
            to: shift_period(&self.from, -1).ok()?,
        })
    }
}

/// `YYYY-MM` 校验。账期会直接进 SQL 的参数（不是拼进文本），这里只管格式对不对。
pub fn check_period(raw: &str) -> Result<String> {
    let raw = raw.trim();
    let ok = raw.len() == 7
        && raw.as_bytes()[4] == b'-'
        && raw[..4].bytes().all(|b| b.is_ascii_digit())
        && raw[5..].bytes().all(|b| b.is_ascii_digit());
    if !ok {
        return Err(Error::bad_request(format!("账期应为 YYYY-MM，不是 {raw:?}")));
    }
    let month: u32 = raw[5..].parse().unwrap_or(0);
    if !(1..=12).contains(&month) {
        return Err(Error::bad_request(format!("账期的月份不合法: {raw:?}")));
    }
    Ok(raw.to_owned())
}

/// 从 0 年 1 月数起的月份序号，方便加减。
fn month_index(period: &str) -> Option<i32> {
    let year: i32 = period.get(..4)?.parse().ok()?;
    let month: i32 = period.get(5..)?.parse().ok()?;
    Some(year * 12 + month - 1)
}

fn period_from_index(index: i32) -> Option<String> {
    if index < 0 {
        return None;
    }
    Some(format!("{:04}-{:02}", index / 12, index % 12 + 1))
}

/// 账期加减若干个月。
pub fn shift_period(period: &str, months: i32) -> Result<String> {
    month_index(period)
        .and_then(|i| period_from_index(i + months))
        .ok_or_else(|| Error::bad_request(format!("账期超出范围: {period} {months:+}")))
}

/// 一次账单查询的筛选条件。
#[derive(Debug, Clone)]
pub struct BillFilter {
    pub range: PeriodRange,
    /// 维度等值筛选，同一个维度多个值是 OR
    pub dims: Vec<(Dimension, Vec<String>)>,
    /// 模糊搜：产品 / 实例 / 计费项里任一个包含就算
    pub q: Option<String>,
}

impl BillFilter {
    pub fn new(range: PeriodRange) -> Self {
        Self { range, dims: Vec::new(), q: None }
    }

    /// WHERE 子句。**所有条件都放在去重之前**：重复的行在每一列上都一模一样，先筛后去重
    /// 和先去重后筛结果一样，但少读很多。
    fn where_sql(&self, kind: Kind, b: &mut Bindings) -> String {
        let mut parts = vec![period_where(kind, b, &self.range)];
        for (dim, values) in &self.dims {
            parts.push(format!("{} IN {}", dim.expr(kind), b.bind("Array(String)", values)));
        }
        if let Some(q) = &self.q {
            let needle = b.bind("String", q);
            let cols = [Dimension::Product, Dimension::Item, Dimension::Instance]
                .map(|d| format!("positionCaseInsensitiveUTF8({}, {needle}) > 0", d.expr(kind)));
            parts.push(format!("({})", cols.join(" OR ")));
        }
        parts.join("\n    AND ")
    }
}

/// 账期范围的 WHERE 片段。三张表打头的排序列各不相同，都走主键。
fn period_where(kind: Kind, b: &mut Bindings, range: &PeriodRange) -> String {
    let from = b.bind("String", &range.from);
    let to = b.bind("String", &range.to);
    match kind {
        Kind::Volcengine => format!("BillPeriod >= {from} AND BillPeriod <= {to}"),
        Kind::AlicloudMonthly => format!("billing_cycle >= {from} AND billing_cycle <= {to}"),
        // 日度表按 billing_date（Date，排序键打头）切，右边开区间取到当月最后一天
        Kind::AlicloudDaily => format!(
            "billing_date >= toDate(concat({from}, '-01')) \
             AND billing_date < addMonths(toDate(concat({to}, '-01')), 1)"
        ),
    }
}

/// 一张账单表上的查询。
pub struct BillQueries<'a> {
    pub database: &'a str,
    pub kind: Kind,
    pub table: &'a BillTable,
    pub dedupe: Dedupe,
}

/// 去重子查询里要带出来的一列。`alias` 是给外层用的干净名字（`period` / `amount`……）。
struct Col {
    expr: String,
    alias: &'static str,
}

fn col(expr: impl Into<String>, alias: &'static str) -> Col {
    Col { expr: expr.into(), alias }
}

/// 子查询里这一列叫什么。
///
/// **不能和真实列名同名**：ClickHouse 的分析器在 WHERE 里会先按 SELECT 的别名解析标识符，
/// 于是 `any(instance_id) AS instance_id` 会让 `WHERE ... instance_id ...`（模糊搜里就有）
/// 变成「WHERE 里出现了聚合函数」，直接报 184。账单表的列名没有下划线打头的，加个前缀就躲开了，
/// 外层再 `AS` 回干净的名字。
fn inner(alias: &str) -> String {
    format!("_{alias}")
}

/// 外层的投影：`_period AS period, _amount AS amount`。
fn project(cols: &[Col]) -> String {
    cols.iter().map(|c| format!("{} AS {}", inner(c.alias), c.alias)).collect::<Vec<_>>().join(", ")
}

impl<'a> BillQueries<'a> {
    /// 取金额并转成 Float64 的表达式。
    ///
    /// **转换函数得按库里的真实列类型选**：goscan 2026-09 之前把火山的金额按 API 原样存成
    /// `String`，之后改成了 `Decimal(20, 8)`。两种转换不能混用——对 `String` 用 `toFloat64`
    /// 会报 43（ILLEGAL_TYPE_OF_ARGUMENT），对 `Decimal` 用 `toFloat64OrZero` 同样报错。
    /// 表结构是现成的（每 5 分钟刷一次），照着它选，新旧两种表都能查。
    fn amount_expr(&self, amount: Amount) -> String {
        let column = amount.column(self.kind);
        match self.table.table.column(column).map(|c| c.kind) {
            // 老表：脏值 / 空串当 0，这正是当初存 String 时的查法
            Some(ColumnKind::String) => format!("toFloat64OrZero({column})"),
            _ => format!("toFloat64({column})"),
        }
    }

    fn table_ref(&self) -> String {
        let table = format!("`{}`.`{}`", self.database, self.table.table.name);
        match self.dedupe {
            Dedupe::Final => format!("{table} FINAL"),
            _ => table,
        }
    }

    /// 去重后的中间结果：一行一个去重键，列是调用方点名要的那几个表达式。
    ///
    /// `Dedupe::Group` 下按去重键 `GROUP BY`，每个表达式套一层 `any()`——重复的行在每一列上
    /// 都相同，取哪一行都一样，这也正是 `ReplacingMergeTree` 自己的语义。
    fn deduped(&self, cols: &[Col], where_sql: &str) -> Result<String> {
        let select_of = |wrap: bool| -> String {
            cols.iter()
                .map(|c| {
                    let alias = inner(c.alias);
                    if wrap {
                        format!("any({}) AS {alias}", c.expr)
                    } else {
                        format!("{} AS {alias}", c.expr)
                    }
                })
                .collect::<Vec<_>>()
                .join(",\n         ")
        };
        if self.dedupe != Dedupe::Group {
            return Ok(format!(
                "SELECT {}\n  FROM {}\n  WHERE {where_sql}",
                select_of(false),
                self.table_ref()
            ));
        }
        let keys = self
            .table
            .dedupe
            .iter()
            .map(|k| quote_ident(k))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        // 去重键并不唯一：阿里云会把一笔尾差调整（原价 0、应付 -0.005 这种）单独出成一行，
        // 维度与正常账单一模一样，只有金额不同。只按去重键分组再 any()，就会在「409.41」与
        // 「-0.005」之间随手挑一个——2026-08 的百炼因此少算了 34,837.57。所以金额列也进分组：
        // 完全相同的副本合一，金额不同的并列行各自保留、之后相加
        let amounts = [Amount::Payable, Amount::Paid, Amount::Original]
            .into_iter()
            .map(|a| a.column(self.kind))
            .filter(|c| self.table.table.has(c))
            .map(quote_ident)
            .collect::<Result<Vec<_>>>()?;
        let group = if amounts.is_empty() {
            keys.clone()
        } else {
            format!("{keys}, {}", amounts.join(", "))
        };
        let Some(version) = self
            .table
            .table
            .has(VERSION_COLUMN)
            .then(|| quote_ident(VERSION_COLUMN))
            .transpose()?
        else {
            return Ok(format!(
                "SELECT {}\n  FROM {}\n  WHERE {where_sql}\n  GROUP BY {group}",
                select_of(true),
                self.table_ref()
            ));
        };
        // 金额进了分组，重拉留下的旧版本（金额已被云厂商调过）就不会再和新版本合一。于是再按
        // 去重键只留**最近一次同步**写入的那几行：与该键最新的 updated_at 相差 SYNC_WINDOW 以内。
        // 同一次同步里并列的几行相隔不过数秒（线上 6–8 月最多 2.2 秒），两次同步则相隔数小时起，
        // 这个窗口把两者分得很开。这也正是 ReplacingMergeTree(updated_at) 合并后该剩下的样子，
        // 只是引擎会把并列行也吞掉，这里不会
        let passthrough = cols.iter().map(|c| inner(c.alias)).collect::<Vec<_>>().join(", ");
        Ok(format!(
            "SELECT {passthrough}\n  FROM (\n    SELECT {},\n         max({version}) AS __version,\n         max(max({version})) OVER (PARTITION BY {keys}) AS __latest\n      FROM {}\n      WHERE {where_sql}\n      GROUP BY {group}\n  )\n  WHERE __version >= __latest - INTERVAL {SYNC_WINDOW_SECS} SECOND",
            select_of(true),
            self.table_ref()
        ))
    }

    /// 这张表里有哪些账期。只读账期那一列（都是排序键打头），很便宜。
    pub fn periods(&self, range: &PeriodRange) -> Query {
        let mut b = Bindings::new();
        let where_sql = period_where(self.kind, &mut b, range);
        b.into_query(format!(
            "SELECT DISTINCT {} AS period\n  FROM {}\n  WHERE {where_sql}\n  ORDER BY period",
            self.kind.period_expr(),
            self.table_ref()
        ))
    }

    /// 按账期汇总。
    pub fn by_period(&self, filter: &BillFilter, amount: Amount) -> Result<Query> {
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(self.kind, &mut b);
        let inner = self.deduped(
            &[col(self.kind.period_expr(), "period"), col(self.amount_expr(amount), "amount")],
            &where_sql,
        )?;
        Ok(b.into_query(format!(
            "SELECT _period AS period, sum(_amount) AS amount, count() AS rows\nFROM (\n  {inner}\n)\nGROUP BY _period\nORDER BY period"
        )))
    }

    /// 按天汇总。月度表没有日期，调用方别拿它问。
    pub fn by_day(&self, filter: &BillFilter, amount: Amount) -> Result<Query> {
        if !self.kind.has_days() {
            return Err(Error::bad_request("月度账单表没有按天的数据"));
        }
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(self.kind, &mut b);
        let inner = self.deduped(
            &[col(self.kind.date_expr(), "day"), col(self.amount_expr(amount), "amount")],
            &where_sql,
        )?;
        Ok(b.into_query(format!(
            "SELECT _day AS day, sum(_amount) AS amount\nFROM (\n  {inner}\n)\nWHERE _day != ''\nGROUP BY _day\nORDER BY day"
        )))
    }

    /// 按某个维度排行。`limit` 之外的用不着单独查：上层把各云的结果合起来之后自己算「其它」。
    pub fn breakdown(
        &self,
        filter: &BillFilter,
        amount: Amount,
        dim: Dimension,
        limit: u32,
    ) -> Result<Query> {
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(self.kind, &mut b);
        let inner = self.deduped(
            &[col(dim.expr(self.kind), "key"), col(self.amount_expr(amount), "amount")],
            &where_sql,
        )?;
        let limit = b.bind("UInt32", limit);
        Ok(b.into_query(format!(
            "SELECT _key AS key, sum(_amount) AS amount\nFROM (\n  {inner}\n)\nGROUP BY _key\nORDER BY amount DESC, key\nLIMIT {limit}"
        )))
    }

    /// 排行之外的「其它」和总额：和 [`Self::breakdown`] 同一段数据，单独一条查询避免把
    /// 上万个分类都传回来。
    pub fn total(&self, filter: &BillFilter, amount: Amount) -> Result<Query> {
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(self.kind, &mut b);
        let inner = self.deduped(&[col(self.amount_expr(amount), "amount")], &where_sql)?;
        Ok(b.into_query(format!(
            "SELECT sum(_amount) AS amount, count() AS rows\nFROM (\n  {inner}\n)"
        )))
    }

    /// 明细列表。列是跨云统一过的，见 [`DetailRow`]。
    pub fn detail(
        &self,
        filter: &BillFilter,
        amount: Amount,
        limit: u32,
        offset: u32,
    ) -> Result<Query> {
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(self.kind, &mut b);
        let cols = self.detail_cols(amount);
        let inner = self.deduped(&cols, &where_sql)?;
        let limit = b.bind("UInt32", limit);
        let offset = b.bind("UInt32", offset);
        Ok(b.into_query(format!(
            "SELECT {}\nFROM (\n  {inner}\n)\nORDER BY amount DESC, period DESC, product\nLIMIT {limit} OFFSET {offset}",
            project(&cols)
        )))
    }

    /// 明细一共多少行（去重之后）。
    pub fn detail_count(&self, filter: &BillFilter) -> Result<Query> {
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(self.kind, &mut b);
        let inner = self.deduped(&[col("1", "one")], &where_sql)?;
        Ok(b.into_query(format!("SELECT count() AS rows\nFROM (\n  {inner}\n)")))
    }

    /// 导出：和明细同样的列，不分页。
    pub fn export(&self, filter: &BillFilter, amount: Amount, limit: u32) -> Result<Query> {
        self.detail(filter, amount, limit, 0)
    }

    // -----------------------------------------------------------------------------------------
    // 成本归属（`--bill-alloc`，见 [`crate::alloc`]）
    // -----------------------------------------------------------------------------------------

    /// 这条匹配条件在这张表上用不用得上。用不上的有两种：限定了另一朵云，或者要匹配的原始列
    /// 在这张表里根本不存在（`intranet_ip` 只有阿里云有）。
    ///
    /// **用不上即视为不命中，而非视为无此条件**：若将缺列的条件当作恒真，一条「内网地址属于
    /// 某几台机器」的规则会把另一朵云的全部费用一并计入那条业务线。
    fn matcher_applies(&self, m: &Matcher) -> bool {
        if m.provider.is_some_and(|p| p != self.kind.provider()) {
            return false;
        }
        m.columns.iter().all(|c| self.table.table.has(&c.column))
    }

    /// 匹配条件的 SQL，各项相与；条件全空时是恒真。用不上时回 `None`。
    fn matcher_sql(&self, m: &Matcher, b: &mut Bindings) -> Option<String> {
        if !self.matcher_applies(m) {
            return None;
        }
        let mut parts = Vec::new();
        for (dim, values) in &m.dims {
            parts.push(format!("{} IN {}", dim.expr(self.kind), b.bind("Array(String)", values)));
        }
        for c in &m.columns {
            let column = self.table.table.column(&c.column)?;
            let ident = quote_ident(&column.name).ok()?;
            // 非字符串列（如 Decimal 的用量、Date 的日期）先转为字符串再比较，以免类型不合报错
            let expr = if column.kind == ColumnKind::String {
                ident
            } else {
                format!("toString({ident})")
            };
            parts.push(match &c.op {
                ColumnOp::AnyOf(values) => {
                    format!("{expr} IN {}", b.bind("Array(String)", values))
                }
                ColumnOp::Like(pattern) => format!("{expr} LIKE {}", b.bind("String", pattern)),
                ColumnOp::NotLike(pattern) => {
                    format!("{expr} NOT LIKE {}", b.bind("String", pattern))
                }
            });
        }
        Some(if parts.is_empty() { "1".to_owned() } else { format!("({})", parts.join(" AND ")) })
    }

    /// 分类表达式：命中第几条规则就是几，一条都没命中是 -1。
    ///
    /// 用 `multiIf` 而非若干个 `if` 相加，语义正是「自上而下、命中即停」；下标写死为常量，
    /// 用不上的规则整条略去也不会让后面的规则错位。
    fn classify_expr(&self, alloc: &Alloc, b: &mut Bindings) -> String {
        let branches: Vec<String> = alloc
            .rules
            .iter()
            .enumerate()
            .filter_map(|(i, rule)| {
                self.matcher_sql(&rule.matcher, b).map(|cond| format!("{cond}, {i}"))
            })
            .collect();
        if branches.is_empty() {
            // 一条规则都用不上（未配规则，或规则尽属另一朵云）：整张表均计作未归属
            return "toInt32(-1)".to_owned();
        }
        format!("toInt32(multiIf({}, -1))", branches.join(", "))
    }

    /// 归属查询的 WHERE：页面上的筛选 + 配置里的 `[[include]]` + 「只看最近 N 天」。
    fn alloc_where(
        &self,
        filter: &BillFilter,
        alloc: &Alloc,
        window_days: Option<u32>,
        b: &mut Bindings,
    ) -> Result<String> {
        let mut sql = filter.where_sql(self.kind, b);
        if !alloc.include.is_empty() {
            let parts: Vec<String> =
                alloc.include.iter().filter_map(|m| self.matcher_sql(m, b)).collect();
            // 每一条 include 都用不上这张表，即这张表里没有要统计的行
            let joined = if parts.is_empty() { "0".to_owned() } else { parts.join(" OR ") };
            sql = format!("{sql}\n    AND ({joined})");
        }
        // 预付费另走摊销那条路（[`Self::alloc_prepaid`]），此处排除，免得同一笔钱计两次。
        // **只在摊得动的表上排除**：火山那张没有服务期列，摊不了，那就仍按出账当月计入，
        // 否则这部分钱两头都不落，凭空少掉
        if let Some(prepaid) = &alloc.prepaid
            && self.amortizable()
            && let Some(cond) = self.matcher_sql(&prepaid.matcher, b)
        {
            sql = format!("{sql}\n    AND NOT {cond}");
        }
        if let Some(days) = window_days {
            let day = self.kind.day_expr().ok_or_else(|| {
                Error::bad_request("月度账单表没有按天的数据，无法按最近几日统计")
            })?;
            // 「最近 N 天」以**该表最后一日有账单的日期**为基准，而非以今日为基准：账单滞后
            // 一两日出具，自今日倒推会平白少算几天。两朵云的同步进度不同，各依各的基准。
            // 子查询另行绑定一次账期，占位符编号顺次排下，与外层互不干扰
            let inner_where = period_where(self.kind, b, &filter.range);
            let table = format!("`{}`.`{}`", self.database, self.table.table.name);
            let back = b.bind("UInt32", days.saturating_sub(1));
            sql = format!(
                "{sql}\n    AND {day} >= (SELECT max({day}) FROM {table} WHERE {inner_where}) - {back}"
            );
        }
        Ok(sql)
    }

    /// 按「第几条规则 + 产品」汇总。一条规则可能横跨多个产品（除 `[[include]]` 外并无限制），
    /// 分产品列出方能与账单本身对得上，也正是费用表中「业务线 → 产品 → 金额」那几行。
    pub fn alloc_by_product(
        &self,
        filter: &BillFilter,
        amount: Amount,
        alloc: &Alloc,
        window_days: Option<u32>,
    ) -> Result<Query> {
        let mut b = Bindings::new();
        let where_sql = self.alloc_where(filter, alloc, window_days, &mut b)?;
        let classify = self.classify_expr(alloc, &mut b);
        let cols = [
            col(classify, "rule"),
            col(Dimension::Product.expr(self.kind), "product"),
            col(self.amount_expr(amount), "amount"),
        ];
        let inner = self.deduped(&cols, &where_sql)?;
        Ok(b.into_query(format!(
            "SELECT _rule AS rule, _product AS product, sum(_amount) AS amount\nFROM (\n  {inner}\n)\nGROUP BY _rule, _product\nORDER BY amount DESC"
        )))
    }

    /// 这张表摊不摊得了预付费：得有服务期那两列，才知道一笔购买该摊到哪几个月。
    /// 阿里云的两张表都有，火山那张没有。
    fn amortizable(&self) -> bool {
        self.table.table.has(SERVICE_PERIOD) && self.table.table.has(SERVICE_PERIOD_UNIT)
    }

    /// 预付费的购买记录，按「第几条规则 + 产品 + 购买账期 + 服务期月数」汇总。
    ///
    /// 摊到各月的那一步在进程里做（见 `api::bills`）：一行购买要变成十几行月度摊销，在 SQL 里
    /// 展开既难读又难验；而购买记录本身行数很少（包年包月一个月不过几十上百行），取回来再摊
    /// 是笔划算的买卖。
    ///
    /// `range` 是**往前放宽过的**账期区间：三年前买的机器若还在服务期内，它的摊销仍要计入本月，
    /// 所以不能只查用户选的那几个月。摊不动的表回 `None`。
    pub fn alloc_prepaid(
        &self,
        filter: &BillFilter,
        amount: Amount,
        alloc: &Alloc,
        prepaid: &Prepaid,
        range: &PeriodRange,
    ) -> Result<Option<Query>> {
        if !self.amortizable() {
            return Ok(None);
        }
        let mut widened = filter.clone();
        widened.range = range.clone();
        let mut b = Bindings::new();
        let mut where_sql = widened.where_sql(self.kind, &mut b);
        let Some(cond) = self.matcher_sql(&prepaid.matcher, &mut b) else {
            return Ok(None);
        };
        where_sql = format!("{where_sql}\n    AND {cond}");
        let classify = self.classify_expr(alloc, &mut b);
        let cols = [
            col(classify, "rule"),
            col(Dimension::Product.expr(self.kind), "product"),
            col(self.kind.period_expr(), "period"),
            col(months_expr(), "months"),
            col(self.amount_expr(amount), "amount"),
        ];
        let inner = self.deduped(&cols, &where_sql)?;
        Ok(Some(b.into_query(format!(
            "SELECT _rule AS rule, _product AS product, _period AS period, _months AS months, sum(_amount) AS amount\nFROM (\n  {inner}\n)\nGROUP BY _rule, _product, _period, _months\nORDER BY period, amount DESC"
        ))))
    }

    /// 按「第几条规则 + 哪一天（月度表则为哪个账期）」汇总，用以绘制业务线的趋势，并据以
    /// 统计共有几天的账单。
    pub fn alloc_by_bucket(
        &self,
        filter: &BillFilter,
        amount: Amount,
        alloc: &Alloc,
        window_days: Option<u32>,
    ) -> Result<Query> {
        let mut b = Bindings::new();
        let where_sql = self.alloc_where(filter, alloc, window_days, &mut b)?;
        let classify = self.classify_expr(alloc, &mut b);
        let bucket =
            if self.kind.has_days() { self.kind.date_expr() } else { self.kind.period_expr() };
        let cols =
            [col(classify, "rule"), col(bucket, "bucket"), col(self.amount_expr(amount), "amount")];
        let inner = self.deduped(&cols, &where_sql)?;
        Ok(b.into_query(format!(
            "SELECT _rule AS rule, _bucket AS bucket, sum(_amount) AS amount\nFROM (\n  {inner}\n)\nWHERE _bucket != ''\nGROUP BY _rule, _bucket\nORDER BY bucket"
        )))
    }

    /// 明细行的列，两朵云都 `AS` 成同一套别名。
    fn detail_cols(&self, amount: Amount) -> Vec<Col> {
        let k = self.kind;
        let (usage, usage_unit, instance_id) = match k {
            Kind::Volcengine => ("Count", "Unit", "InstanceNo"),
            _ => ("usage", "usage_unit", "instance_id"),
        };
        vec![
            col(k.period_expr(), "period"),
            col(k.date_expr(), "day"),
            col(Dimension::Product.expr(k), "product"),
            col(Dimension::Item.expr(k), "item"),
            col(instance_id, "instance_id"),
            col(Dimension::Instance.expr(k), "instance"),
            col(Dimension::Region.expr(k), "region"),
            col(Dimension::Account.expr(k), "account"),
            col(Dimension::Project.expr(k), "project"),
            col(Dimension::Subscription.expr(k), "subscription"),
            col(format!("toString({usage})"), "usage"),
            col(usage_unit, "usage_unit"),
            col(Dimension::Currency.expr(k), "currency"),
            col(self.amount_expr(amount), "amount"),
            col(self.amount_expr(Amount::Original), "original"),
            col(self.amount_expr(Amount::Paid), "paid"),
        ]
    }
}

/// 库里有哪些账期。[`BillQueries::periods`] 只 `SELECT DISTINCT` 一列，**不能拿
/// [`BucketRow`] 来解**——那个结构要求 `amount`，账期列表里没有这一列，一旦真有数据就会
/// 报「missing field `amount`」。空表时一行都没有，正好把这个错藏过去，所以单测里要有一条
/// 带行的回放（见 tests/api_bills.rs）。
#[derive(Debug, Deserialize)]
pub struct PeriodRow {
    pub period: String,
}

/// 某个账期（或某一天）的金额。
#[derive(Debug, Deserialize)]
pub struct BucketRow {
    #[serde(alias = "day")]
    pub period: String,
    #[serde(deserialize_with = "num::de")]
    pub amount: f64,
}

/// 排行里的一项。
#[derive(Debug, Deserialize)]
pub struct KeyRow {
    pub key: String,
    #[serde(deserialize_with = "num::de")]
    pub amount: f64,
}

/// 预付费的一笔购买（已按规则、产品、购买账期、服务期聚合）。
#[derive(Debug, Deserialize)]
pub struct AllocPrepaidRow {
    pub rule: i32,
    pub product: String,
    /// 购买账期，摊销从这个月起算
    pub period: String,
    /// 服务期折成几个月
    #[serde(deserialize_with = "num::de")]
    pub months: u32,
    #[serde(deserialize_with = "num::de")]
    pub amount: f64,
}

/// 成本归属汇总的一行。`rule` 是命中了第几条规则（[`Alloc::rules`] 的下标），-1 表示
/// 一条都没命中；`key` 按查询不同，是产品名、某一天或某个账期。
#[derive(Debug, Deserialize)]
pub struct AllocRow {
    pub rule: i32,
    #[serde(rename = "product", alias = "bucket")]
    pub key: String,
    #[serde(deserialize_with = "num::de")]
    pub amount: f64,
}

/// 总额。
#[derive(Debug, Deserialize)]
pub struct TotalRow {
    #[serde(default, deserialize_with = "num::de_opt")]
    pub amount: Option<f64>,
    #[serde(default, deserialize_with = "num::de_opt")]
    pub rows: Option<u64>,
}

/// 一行明细，两朵云统一后的形状。
#[derive(Debug, Serialize, Deserialize)]
pub struct DetailRow {
    /// 哪朵云。库里没有这一列，是 API 层按表填的
    #[serde(default)]
    pub provider: String,
    pub period: String,
    /// 月度账单没有日期，是空串
    #[serde(default)]
    pub day: String,
    pub product: String,
    pub item: String,
    pub instance_id: String,
    pub instance: String,
    pub region: String,
    pub account: String,
    pub project: String,
    pub subscription: String,
    pub usage: String,
    pub usage_unit: String,
    pub currency: String,
    #[serde(deserialize_with = "num::de")]
    pub amount: f64,
    #[serde(deserialize_with = "num::de")]
    pub original: f64,
    #[serde(deserialize_with = "num::de")]
    pub paid: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{BillTable, Column, ColumnKind, Table};

    /// 一张够用的假表：去重键那几列 + 三种金额列。`money_type` 是金额列在库里的类型——
    /// goscan 2026-09 之前是 `String`，之后是 `Decimal(20, 8)`，opdash 两种都要能查。
    fn table_with(name: &str, kind: Kind, money_type: &str) -> BillTable {
        let money = [
            Amount::Payable.column(kind),
            Amount::Paid.column(kind),
            Amount::Original.column(kind),
        ];
        let mut columns: Vec<Column> = kind
            .fallback_dedupe()
            .iter()
            .map(|n| Column {
                name: (*n).to_owned(),
                ty: "String".into(),
                kind: ColumnKind::String,
            })
            .collect();
        columns.extend(money.iter().map(|n| Column {
            name: (*n).to_owned(),
            ty: money_type.to_owned(),
            kind: ColumnKind::classify(money_type),
        }));
        BillTable {
            table: Table { name: name.to_owned(), columns },
            dedupe: kind.fallback_dedupe().iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn table(name: &str, kind: Kind) -> BillTable {
        table_with(name, kind, "Decimal(20, 8)")
    }

    fn queries<'a>(t: &'a BillTable, kind: Kind, dedupe: Dedupe) -> BillQueries<'a> {
        BillQueries { database: "logs", kind, table: t, dedupe }
    }

    #[test]
    fn periods_are_validated_and_defaulted() {
        let r = PeriodRange::new(None, None, "2026-09").unwrap();
        assert_eq!(r.from, "2026-04");
        assert_eq!(r.to, "2026-09");
        assert_eq!(r.len(), 6);
        assert_eq!(r.periods().first().unwrap(), "2026-04");
        assert_eq!(r.periods().last().unwrap(), "2026-09");
        let prev = r.previous().unwrap();
        assert_eq!((prev.from.as_str(), prev.to.as_str()), ("2025-10", "2026-03"));

        assert_eq!(shift_period("2026-01", -1).unwrap(), "2025-12");
        assert_eq!(shift_period("2026-12", 1).unwrap(), "2027-01");
        assert!(check_period("2026-13").is_err());
        assert!(check_period("2026-1").is_err());
        assert!(check_period("2026/01").is_err());
        assert!(PeriodRange::new(Some("2026-09"), Some("2026-04"), "2026-09").is_err());
        assert!(PeriodRange::new(Some("2000-01"), Some("2026-09"), "2026-09").is_err());
    }

    #[test]
    fn dedupes_by_the_sorting_key() {
        let t = table("volcengine_bill", Kind::Volcengine);
        let q = queries(&t, Kind::Volcengine, Dedupe::Group)
            .by_period(
                &BillFilter::new(PeriodRange::new(None, None, "2026-09").unwrap()),
                Amount::Payable,
            )
            .unwrap();
        // 去重发生在子查询里，按建表时的排序键分组
        assert!(
            q.sql().contains("GROUP BY `BillPeriod`, `ExpenseDate`, `InstanceNo`"),
            "{}",
            q.sql()
        );
        assert!(q.sql().contains("any(toFloat64(PayableAmount)) AS _amount"), "{}", q.sql());
        assert!(q.sql().contains("sum(_amount) AS amount"), "{}", q.sql());
        // 账期是参数，不是拼进去的
        assert_eq!(q.params()[0].1, "2026-04");
        assert_eq!(q.params()[1].1, "2026-09");
        assert!(!q.sql().contains("2026-04"), "{}", q.sql());
    }

    #[test]
    fn final_and_off_skip_the_group_by() {
        let t = table("alicloud_bill_monthly", Kind::AlicloudMonthly);
        let filter = BillFilter::new(PeriodRange::new(None, None, "2026-09").unwrap());
        let q = queries(&t, Kind::AlicloudMonthly, Dedupe::Final)
            .by_period(&filter, Amount::Paid)
            .unwrap();
        assert!(q.sql().contains("`logs`.`alicloud_bill_monthly` FINAL"), "{}", q.sql());
        assert!(!q.sql().contains("any("), "{}", q.sql());
        let q = queries(&t, Kind::AlicloudMonthly, Dedupe::Off)
            .by_period(&filter, Amount::Paid)
            .unwrap();
        assert!(!q.sql().contains("FINAL"), "{}", q.sql());
        assert!(q.sql().contains("toFloat64(payment_amount) AS _amount"), "{}", q.sql());
    }

    /// 金额列的转换函数按库里的真实类型选：老表存的是 String，新表是 Decimal，
    /// 两种转换混用都会被 ClickHouse 以 43（ILLEGAL_TYPE_OF_ARGUMENT）拒掉。
    #[test]
    fn amount_conversion_follows_the_column_type() {
        let filter = BillFilter::new(PeriodRange::new(None, None, "2026-09").unwrap());
        let legacy = table_with("volcengine_bill", Kind::Volcengine, "String");
        let q = queries(&legacy, Kind::Volcengine, Dedupe::Group)
            .by_period(&filter, Amount::Payable)
            .unwrap();
        assert!(q.sql().contains("toFloat64OrZero(PayableAmount)"), "{}", q.sql());

        let current = table_with("volcengine_bill", Kind::Volcengine, "Decimal(20, 8)");
        let q = queries(&current, Kind::Volcengine, Dedupe::Group)
            .by_period(&filter, Amount::Payable)
            .unwrap();
        assert!(q.sql().contains("toFloat64(PayableAmount)"), "{}", q.sql());
        assert!(!q.sql().contains("toFloat64OrZero"), "{}", q.sql());
    }

    #[test]
    fn daily_table_slices_by_date_not_by_string() {
        let t = table("alicloud_bill_daily", Kind::AlicloudDaily);
        let q = queries(&t, Kind::AlicloudDaily, Dedupe::Group)
            .by_day(
                &BillFilter::new(
                    PeriodRange::new(Some("2026-08"), Some("2026-09"), "2026-09").unwrap(),
                ),
                Amount::Payable,
            )
            .unwrap();
        assert!(q.sql().contains("billing_date >= toDate(concat("), "{}", q.sql());
        assert!(q.sql().contains("addMonths(toDate(concat("), "{}", q.sql());
        // 月度表没有日期，问了要报错
        let m = table("alicloud_bill_monthly", Kind::AlicloudMonthly);
        assert!(
            queries(&m, Kind::AlicloudMonthly, Dedupe::Group)
                .by_day(
                    &BillFilter::new(PeriodRange::new(None, None, "2026-09").unwrap()),
                    Amount::Payable
                )
                .is_err()
        );
    }

    #[test]
    fn filters_bind_every_value() {
        let t = table("volcengine_bill", Kind::Volcengine);
        let mut filter = BillFilter::new(PeriodRange::new(None, None, "2026-09").unwrap());
        filter.dims.push((Dimension::Product, vec!["云服务器".into()]));
        filter.q = Some("i-abc".into());
        let q = queries(&t, Kind::Volcengine, Dedupe::Group)
            .breakdown(&filter, Amount::Payable, Dimension::Region, 20)
            .unwrap();
        assert!(
            q.sql().contains("if(ProductZh != '', ProductZh, Product) IN {p2:Array(String)}"),
            "{}",
            q.sql()
        );
        assert!(q.sql().contains("positionCaseInsensitiveUTF8"), "{}", q.sql());
        assert_eq!(q.params()[2].1, "['云服务器']");
        assert_eq!(q.params()[3].1, "i-abc");
        assert!(!q.sql().contains("云服务器"), "{}", q.sql());
    }

    #[test]
    fn detail_has_the_same_shape_for_both_clouds() {
        let volc = table("volcengine_bill", Kind::Volcengine);
        let ali = table("alicloud_bill_daily", Kind::AlicloudDaily);
        let filter = BillFilter::new(PeriodRange::new(None, None, "2026-09").unwrap());
        let a = queries(&volc, Kind::Volcengine, Dedupe::Group)
            .detail(&filter, Amount::Payable, 100, 0)
            .unwrap();
        let b = queries(&ali, Kind::AlicloudDaily, Dedupe::Group)
            .detail(&filter, Amount::Payable, 100, 0)
            .unwrap();
        for alias in ["period", "day", "product", "instance_id", "amount", "original", "paid"] {
            let projected = format!("_{alias} AS {alias}");
            assert!(a.sql().contains(&projected), "火山缺 {alias}: {}", a.sql());
            assert!(b.sql().contains(&projected), "阿里云缺 {alias}: {}", b.sql());
        }
        assert!(a.sql().contains("toString(Count)) AS _usage"), "{}", a.sql());
        assert!(b.sql().contains("toString(usage)) AS _usage"), "{}", b.sql());
        // 子查询里的别名一律带下划线前缀：别名和真实列名撞上时，ClickHouse 会把 WHERE 里的
        // 那个列名解析成聚合结果，模糊搜直接报 184
        assert!(!b.sql().contains("any(instance_id) AS instance_id"), "{}", b.sql());
        assert!(b.sql().contains("any(instance_id) AS _instance_id"), "{}", b.sql());
    }

    /// 去重键并不唯一：阿里云把尾差调整单独出成一行，维度与正常账单一模一样，只差金额。
    /// 金额因此也进分组，并列的几行各自保留；再按去重键只留最近一次同步写入的那几行，
    /// 重拉留下的旧版本（金额可能已被调过）不会与新版本并存。
    #[test]
    fn dedupe_keeps_parallel_lines_and_drops_stale_versions() {
        let mut t = table_with("alicloud_bill_daily", Kind::AlicloudDaily, "Float64");
        t.table.columns.push(Column {
            name: "updated_at".into(),
            ty: "DateTime64(3)".into(),
            kind: ColumnKind::DateTime,
        });
        let filter = BillFilter::new(PeriodRange::new(None, None, "2026-09").unwrap());
        let q = queries(&t, Kind::AlicloudDaily, Dedupe::Group)
            .by_period(&filter, Amount::Payable)
            .unwrap();
        let sql = q.sql();
        assert!(
            sql.contains("`line_seq`, `pretax_amount`, `payment_amount`, `pretax_gross_amount`"),
            "金额列要进分组: {sql}"
        );
        assert!(sql.contains("max(max(`updated_at`)) OVER (PARTITION BY `billing_date`"), "{sql}");
        assert!(sql.contains("WHERE __version >= __latest - INTERVAL 60 SECOND"), "{sql}");

        // 没有版本列的表（老版本的 goscan）退而求其次：只按去重键 + 金额分组
        let bare = table_with("alicloud_bill_daily", Kind::AlicloudDaily, "Float64");
        let q = queries(&bare, Kind::AlicloudDaily, Dedupe::Group)
            .by_period(&filter, Amount::Payable)
            .unwrap();
        assert!(q.sql().contains("`pretax_amount`"), "{}", q.sql());
        assert!(!q.sql().contains("__latest"), "{}", q.sql());
    }

    /// 含内网地址匹配的归属配置：阿里云的表有 `intranet_ip`，火山引擎的没有。
    fn alloc() -> crate::alloc::Alloc {
        crate::alloc::Alloc::parse(
            r#"
lines = ["甲线", "乙线", "公共"]
unmatched = "公共"

[[include]]
subscription = ["PayAsYouGo", "按量计费"]

[[rules]]
name = "甲线专用机器"
product = ["云服务器 ECS"]
to = "甲线"
columns = [{ name = "intranet_ip", any_of = ["10.0.0.1"] }]

[[rules]]
name = "ECS 其余部分"
product = ["云服务器 ECS"]
split = { "甲线" = 1, "乙线" = 3 }

[prepaid]
subscription = ["Subscription", "包年包月"]
"#,
        )
        .unwrap()
    }

    /// 归属的分类在库内完成：一条 `multiIf` 自上而下、命中即停，外层按它分组。
    #[test]
    fn allocation_classifies_in_sql() {
        let t = table_with("alicloud_bill_daily", Kind::AlicloudDaily, "Float64");
        // 线上的表有内网地址这一列，去重键那套假列中没有，此处补上
        let mut t = t;
        t.table.columns.push(Column {
            name: "intranet_ip".into(),
            ty: "String".into(),
            kind: ColumnKind::String,
        });
        let filter = BillFilter::new(PeriodRange::new(None, None, "2026-09").unwrap());
        let q = queries(&t, Kind::AlicloudDaily, Dedupe::Group)
            .alloc_by_product(&filter, Amount::Payable, &alloc(), None)
            .unwrap();
        let sql = q.sql();
        assert!(sql.contains("multiIf("), "{sql}");
        assert!(sql.contains("`intranet_ip` IN {"), "{sql}");
        assert!(sql.contains(", 0, "), "第一条规则的下标: {sql}");
        assert!(sql.contains(", 1, -1)"), "第二条规则与未命中: {sql}");
        assert!(sql.contains("GROUP BY _rule, _product"), "{sql}");
        // include 变成 WHERE 的一部分，不是事后在进程里筛
        assert!(sql.contains("subscription_type IN {"), "{sql}");
        // 取值一律绑参数
        assert!(!sql.contains("10.0.0.1"), "{sql}");
        assert!(q.params().iter().any(|(_, v)| v.contains("10.0.0.1")));
    }

    /// 要匹配的列在这张表上不存在时，整条规则对该表不生效——**不可当作恒真**，
    /// 否则「某几台机器归甲线」会把另一朵云的全部费用一并计入甲线。
    #[test]
    fn rules_that_need_a_missing_column_never_match() {
        let t = table("volcengine_bill", Kind::Volcengine);
        let filter = BillFilter::new(PeriodRange::new(None, None, "2026-09").unwrap());
        let q = queries(&t, Kind::Volcengine, Dedupe::Group)
            .alloc_by_product(&filter, Amount::Payable, &alloc(), None)
            .unwrap();
        let sql = q.sql();
        assert!(!sql.contains("intranet_ip"), "{sql}");
        // 第一条规则整条略去，第二条依旧在，下标不随之挪动
        assert!(sql.contains(", 1, -1)"), "{sql}");
        assert!(!sql.contains(", 0, "), "{sql}");
    }

    /// 预付费走摊销那条路：单独一条查询按购买账期和服务期取数，同时把这些行从
    /// 「按发生月计入」的那条路里排除，免得同一笔钱计两次。
    #[test]
    fn prepaid_rows_leave_the_as_billed_stream() {
        let mut t = table_with("alicloud_bill_daily", Kind::AlicloudDaily, "Float64");
        for name in ["intranet_ip", "service_period", "service_period_unit"] {
            t.table.columns.push(Column {
                name: name.into(),
                ty: "String".into(),
                kind: ColumnKind::String,
            });
        }
        let alloc = alloc();
        let prepaid = alloc.prepaid.clone().unwrap();
        let filter = BillFilter::new(PeriodRange::new(None, None, "2026-09").unwrap());
        let q = queries(&t, Kind::AlicloudDaily, Dedupe::Group);

        let as_billed = q.alloc_by_product(&filter, Amount::Payable, &alloc, None).unwrap();
        assert!(as_billed.sql().contains("AND NOT (subscription_type IN {"), "{}", as_billed.sql());

        let range = PeriodRange { from: "2024-10".into(), to: "2026-09".into() };
        let amortize = q
            .alloc_prepaid(&filter, Amount::Payable, &alloc, &prepaid, &range)
            .unwrap()
            .expect("阿里云的表摊得动");
        let sql = amortize.sql();
        assert!(sql.contains("GROUP BY _rule, _product, _period, _months"), "{sql}");
        // 服务期折成月数：一年 12 个月，按天记的除以平均月长
        assert!(sql.contains("service_period_unit IN ('年'"), "{sql}");
        assert!(sql.contains("/ 86400 / 30"), "按秒记的服务期（域名）也要换算: {sql}");
        // 取数的区间是往前放宽过的，三年前买的机器也找得到
        assert!(amortize.params().iter().any(|(_, v)| v == "2024-10"), "{:?}", amortize.params());
    }

    /// 火山那张表没有服务期列，摊不动：不生成摊销查询，也**不能**把它的包年包月行从
    /// 按发生月那条路里排除，否则这部分钱两头都不落。
    #[test]
    fn a_table_without_service_period_keeps_prepaid_as_billed() {
        let t = table("volcengine_bill", Kind::Volcengine);
        let alloc = alloc();
        let prepaid = alloc.prepaid.clone().unwrap();
        let filter = BillFilter::new(PeriodRange::new(None, None, "2026-09").unwrap());
        let q = queries(&t, Kind::Volcengine, Dedupe::Group);

        let range = PeriodRange { from: "2024-10".into(), to: "2026-09".into() };
        assert!(
            q.alloc_prepaid(&filter, Amount::Payable, &alloc, &prepaid, &range).unwrap().is_none()
        );
        let as_billed = q.alloc_by_product(&filter, Amount::Payable, &alloc, None).unwrap();
        assert!(!as_billed.sql().contains("AND NOT"), "{}", as_billed.sql());
    }

    /// 「最近 N 天」以表中最后一日有账单的日期为准，而非以今日为准。
    #[test]
    fn window_counts_back_from_the_latest_billed_day() {
        let t = table("alicloud_bill_daily", Kind::AlicloudDaily);
        let filter = BillFilter::new(PeriodRange::new(None, None, "2026-09").unwrap());
        let q = queries(&t, Kind::AlicloudDaily, Dedupe::Group)
            .alloc_by_bucket(&filter, Amount::Payable, &crate::alloc::Alloc::default(), Some(7))
            .unwrap();
        let sql = q.sql();
        assert!(
            sql.contains("SELECT max(billing_date) FROM `logs`.`alicloud_bill_daily`"),
            "{sql}"
        );
        // 含最后一日在内共 7 天，故往回数 6 日
        assert_eq!(q.params().last().unwrap().1, "6");
        assert!(sql.contains("GROUP BY _rule, _bucket"), "{sql}");
        // 一条规则也没有：整张表均为未归属
        assert!(sql.contains("toInt32(-1)"), "{sql}");

        // 月度账单没有日期，此参数对它没有意义
        let m = table("alicloud_bill_monthly", Kind::AlicloudMonthly);
        assert!(
            queries(&m, Kind::AlicloudMonthly, Dedupe::Group)
                .alloc_by_bucket(&filter, Amount::Payable, &crate::alloc::Alloc::default(), Some(7))
                .is_err()
        );
    }
}
