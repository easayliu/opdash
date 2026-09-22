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
use crate::clickhouse::{Query, num};
use crate::error::{Error, Result};
use crate::schema::BillTable;

/// 一次最多查多少个账期。账单按月存，36 个月已经是「看三年趋势」，再多只是把 SQL 拉长。
pub const MAX_PERIODS: usize = 36;

/// 默认看最近几个账期。
pub const DEFAULT_PERIODS: usize = 6;

/// 明细页一次最多返回多少行。
pub const MAX_DETAIL_ROWS: u32 = 1_000;

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
    /// `system.tables.sorting_key` 读不到时用，见模块文档。
    pub fn fallback_dedupe(self) -> &'static [&'static str] {
        match self {
            Kind::Volcengine => &[
                "BillPeriod",
                "ExpenseDate",
                "InstanceNo",
                "ExpenseBeginTime",
                "Product",
                "ElementCode",
                "PayableAmount",
            ],
            Kind::AlicloudMonthly => &[
                "billing_cycle",
                "product_code",
                "instance_id",
                "bill_account_id",
                "subscription_type",
                "payment_amount",
            ],
            Kind::AlicloudDaily => &[
                "billing_date",
                "product_code",
                "instance_id",
                "bill_account_id",
                "subscription_type",
                "payment_amount",
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

    /// 取金额的表达式。火山那一列是 `String`，必须转。
    fn expr(self, kind: Kind) -> &'static str {
        match (kind, self) {
            (Kind::Volcengine, Amount::Payable) => "toFloat64OrZero(PayableAmount)",
            (Kind::Volcengine, Amount::Paid) => "toFloat64OrZero(PaidAmount)",
            (Kind::Volcengine, Amount::Original) => "toFloat64OrZero(OriginalBillAmount)",
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
        Ok(format!(
            "SELECT {}\n  FROM {}\n  WHERE {where_sql}\n  GROUP BY {keys}",
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
            &[col(self.kind.period_expr(), "period"), col(amount.expr(self.kind), "amount")],
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
            &[col(self.kind.date_expr(), "day"), col(amount.expr(self.kind), "amount")],
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
            &[col(dim.expr(self.kind), "key"), col(amount.expr(self.kind), "amount")],
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
        let inner = self.deduped(&[col(amount.expr(self.kind), "amount")], &where_sql)?;
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
            col(amount.expr(k), "amount"),
            col(Amount::Original.expr(k), "original"),
            col(Amount::Paid.expr(k), "paid"),
        ]
    }
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

    fn table(name: &str, kind: Kind) -> BillTable {
        BillTable {
            table: Table {
                name: name.to_owned(),
                columns: vec![Column {
                    name: "BillPeriod".into(),
                    ty: "String".into(),
                    kind: ColumnKind::String,
                }],
            },
            dedupe: kind.fallback_dedupe().iter().map(|s| (*s).to_owned()).collect(),
        }
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
        assert!(q.sql().contains("any(toFloat64OrZero(PayableAmount)) AS _amount"), "{}", q.sql());
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
        assert!(q.sql().contains("payment_amount AS _amount"), "{}", q.sql());
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
}
