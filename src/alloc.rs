//! 成本归属：把账单按规则分摊到业务线（`--bill-alloc` 指向的那份文件）。
//!
//! 云账单只回答「哪个产品花了多少」，而对账时真正要回答的是「哪条业务线花了多少」。两者之间
//! 差着一层归属：一台机器属于谁、一项共用服务按什么比例摊给几条线。这层知识**不在账单里**，
//! 只能由运维给出，因此它是一份外部配置，而非代码中的常量——业务线名称、实例的内网地址均属
//! 部署方的内部信息，不应随仓库分发。示例见 `examples/bill-alloc.toml`。
//!
//! ## 规则怎么生效
//!
//! 每一行账单自上而下匹配 `[[rules]]`，**命中第一条即停**。顺序因此是有意义的：先写「某台机器
//! 专属」这类窄规则，再写「同一产品的其余部分按比例拆分」这类宽规则。若宽的写在前面，窄的永远
//! 不会被命中；更隐蔽的是同一笔费用满足两条规则而被计两次——匹配即停正是为杜绝此事。
//!
//! 命中之后，这笔钱或整笔归于一条业务线（`to`），或按权重摊给若干条（`split`）。权重只论相对
//! 大小，`{ "甲" = 1, "乙" = 3 }` 与 `{ "甲" = 25, "乙" = 75 }` 等价（键含中文时 TOML 要求加引号）。
//!
//! 一条规则由若干条件相与构成：统一维度（`product` / `item` / `region` …，与费用页的排行维度
//! 同名同义）取值命中其一即可；`columns` 则直接匹配库里的原始列，用于账单维度表达不了的归属
//! 依据（内网地址、实例规格、弹性伸缩组的命名习惯）。原始列的列名随云而异，某张表没有这一列时
//! 该规则对这张表整体不生效，而非退化成「恒真」。
//!
//! ## 为什么不在这里读金额
//!
//! 本模块只描述规则、并把「命中第几条规则」翻译成业务线份额，不涉及 SQL 与金额。分类表达式由
//! [`crate::query::bills`] 按各云的列名生成，聚合在 ClickHouse 内完成——账单一个月数万行，
//! 全部取回进程内再分类既慢又无必要。

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::query::bills::{DIMENSIONS, Dimension, Provider};

/// 最多往前回溯多少个月。再往前阿里云的账单接口本来就拉不到。
const MAX_LOOKBACK_MONTHS: u32 = 120;

/// 一份归属配置里最多允许多少条规则。每条规则在 SQL 中都是 `multiIf` 的一个分支，
/// 数百条之后语句将长得难以卒读，也说明归属方式应当换一种建模。
const MAX_RULES: usize = 200;

/// 原始列的匹配方式。
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnOp {
    /// 取值属于给定集合
    AnyOf(Vec<String>),
    /// `LIKE` 模式，`%` 匹配任意长度
    Like(String),
    /// `NOT LIKE`
    NotLike(String),
}

/// 直接按库里的原始列匹配。列名随云而异，因此带列名的规则只对含该列的表生效。
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnMatch {
    pub column: String,
    pub op: ColumnOp,
}

/// 一条账单行要满足的条件，各项之间是「与」。全空表示无条件命中。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Matcher {
    /// 限定某朵云；不限定则两朵都看
    pub provider: Option<Provider>,
    /// 统一维度的等值匹配，同一维度的多个取值之间是「或」
    pub dims: Vec<(Dimension, Vec<String>)>,
    pub columns: Vec<ColumnMatch>,
}

/// 命中之后分给哪条业务线、占多大权重。
#[derive(Debug, Clone, PartialEq)]
pub struct Share {
    /// [`Alloc::lines`] 里的下标
    pub line: usize,
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub name: String,
    pub matcher: Matcher,
    pub shares: Vec<Share>,
}

impl Rule {
    /// 权重之和，用于把金额换算成各条业务线的份额。
    pub fn total_weight(&self) -> f64 {
        self.shares.iter().map(|s| s.weight).sum()
    }

    /// 命中本规则的 `amount` 分给各条业务线之后的结果。
    pub fn split(&self, amount: f64) -> Vec<(usize, f64)> {
        let total = self.total_weight();
        if total <= 0.0 {
            return Vec::new();
        }
        self.shares.iter().map(|s| (s.line, amount * s.weight / total)).collect()
    }
}

/// 预付费（包年包月）怎么算。
///
/// 这类账单在**购买当月一次性出账**，直接按账单发生月计入，那个月会凭空鼓起一大块，日均和
/// 月度预估随之失真。所以它走另一条路：按账单行自己的服务期摊到各月，一台包年的机器于是在
/// 十二个月里各计十二分之一。摊销的起点是购买账期，终点由 `service_period` /
/// `service_period_unit` 决定。
///
/// 升降配只收退差价，其 `service_period` 记的是「剩余天数」而非整期，金额也远小于整机价；
/// 按同一套摊法处理即可——钱是真实发生的，摊的期限也正是它该覆盖的那段。
#[derive(Debug, Clone, PartialEq)]
pub struct Prepaid {
    /// 哪些账单行算预付费
    pub matcher: Matcher,
    /// 往前回溯多少个月找购买记录：一台三年前买的三年期机器还在服务期内，它的摊销仍要计入
    /// 本月。**只管找多远，不截断服务期**——五年期的机器照样摊五年
    pub lookback_months: u32,
}

/// `lookback_months` 不写时的默认值。阿里云账单接口只保留 18 个月，往前回溯 36 个月
/// 已足够覆盖库里所有能查到的购买记录。
pub const DEFAULT_LOOKBACK_MONTHS: u32 = 36;

/// 一份归属配置。`Default` 是「没配规则」：一切都算未归属，分析视图只剩按产品的日均。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Alloc {
    /// 业务线，顺序即页面上的顺序
    pub lines: Vec<String>,
    /// 未命中任何规则的费用归到哪条线；`None` 表示单独列作「未归属」
    pub unmatched: Option<usize>,
    /// 只统计命中其中任一条的账单行；为空表示全部都统计
    pub include: Vec<Matcher>,
    pub rules: Vec<Rule>,
    /// 预付费按服务期摊销；没配这一段就只统计后付费
    pub prepaid: Option<Prepaid>,
}

impl Alloc {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读取归属规则 {} 失败：{e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{} 有误：{e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let file: File = toml::from_str(text).map_err(|e| e.to_string())?;
        file.into_alloc()
    }

    /// 业务线的下标。
    fn index_of(&self, name: &str) -> Option<usize> {
        self.lines.iter().position(|l| l == name)
    }
}

// -------------------------------------------------------------------------------------------
// 文件中的形状。字段名即 TOML 的键名，多写一个键即报错——归属规则关乎金额，
// 拼错的键若被静默忽略，错的便是账目，而不只是少一条提示
// -------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    lines: Vec<String>,
    unmatched: Option<String>,
    #[serde(default)]
    include: Vec<Entry>,
    #[serde(default)]
    rules: Vec<Entry>,
    prepaid: Option<Entry>,
}

/// `[[include]]` 与 `[[rules]]` 共用一种形状：条件部分完全一致，只是前者不带去向。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: Option<String>,
    provider: Option<String>,
    product: Option<Vec<String>>,
    item: Option<Vec<String>>,
    region: Option<Vec<String>>,
    zone: Option<Vec<String>>,
    account: Option<Vec<String>>,
    instance: Option<Vec<String>>,
    project: Option<Vec<String>>,
    subscription: Option<Vec<String>>,
    currency: Option<Vec<String>>,
    #[serde(default)]
    columns: Vec<ColumnEntry>,
    to: Option<String>,
    split: Option<BTreeMap<String, f64>>,
    /// 只有 `[prepaid]` 用得上
    lookback_months: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ColumnEntry {
    name: String,
    any_of: Option<Vec<String>>,
    like: Option<String>,
    not_like: Option<String>,
}

impl File {
    fn into_alloc(self) -> Result<Alloc, String> {
        if self.lines.is_empty() {
            return Err("lines 至少需要一条业务线".into());
        }
        for (i, line) in self.lines.iter().enumerate() {
            if line.trim().is_empty() {
                return Err(format!("lines 第 {} 项为空", i + 1));
            }
            if self.lines[..i].contains(line) {
                return Err(format!("业务线 {line:?} 重复"));
            }
        }
        if self.rules.len() > MAX_RULES {
            return Err(format!("规则最多 {MAX_RULES} 条，本文件有 {} 条", self.rules.len()));
        }
        let mut alloc = Alloc {
            lines: self.lines,
            unmatched: None,
            include: Vec::new(),
            rules: Vec::new(),
            prepaid: None,
        };
        if let Some(name) = &self.unmatched {
            alloc.unmatched = Some(
                alloc
                    .index_of(name)
                    .ok_or_else(|| format!("unmatched = {name:?} 不在 lines 中"))?,
            );
        }
        for (i, entry) in self.include.into_iter().enumerate() {
            let at = format!("include 第 {} 条", i + 1);
            if entry.to.is_some() || entry.split.is_some() {
                return Err(format!("{at}仅用于筛选账单行，不能指定 to / split"));
            }
            if entry.lookback_months.is_some() {
                return Err(format!("{at}不能指定 lookback_months，该项仅用于 [prepaid]"));
            }
            alloc.include.push(entry.matcher(&at)?);
        }
        if let Some(entry) = self.prepaid {
            let at = "[prepaid]";
            if entry.to.is_some() || entry.split.is_some() {
                return Err(format!(
                    "{at}仅用于界定哪些账单行属于预付费，费用去向由 [[rules]] 决定，不能指定 to / split"
                ));
            }
            let lookback_months = entry.lookback_months.unwrap_or(DEFAULT_LOOKBACK_MONTHS);
            if !(1..=MAX_LOOKBACK_MONTHS).contains(&lookback_months) {
                return Err(format!(
                    "{at} 的 lookback_months 应在 1 与 {MAX_LOOKBACK_MONTHS} 之间"
                ));
            }
            let matcher = entry.matcher(at)?;
            if matcher == Matcher::default() {
                return Err(format!(
                    "{at}未给出任何条件，这会使全部账单都被视为预付费；通常应写为 subscription = [\"Subscription\", \"包年包月\"]"
                ));
            }
            alloc.prepaid = Some(Prepaid { matcher, lookback_months });
        }
        for (i, entry) in self.rules.into_iter().enumerate() {
            if entry.lookback_months.is_some() {
                return Err(format!(
                    "规则第 {} 条不能指定 lookback_months，该项仅用于 [prepaid]",
                    i + 1
                ));
            }
            let name = entry
                .name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| format!("规则 {}", i + 1));
            let at = format!("规则「{name}」");
            let shares = entry.shares(&alloc, &at)?;
            alloc.rules.push(Rule { name, matcher: entry.matcher(&at)?, shares });
        }
        Ok(alloc)
    }
}

impl Entry {
    fn matcher(&self, at: &str) -> Result<Matcher, String> {
        let provider = match &self.provider {
            Some(raw) => Some(
                Provider::parse(raw)
                    .map_err(|_| format!("{at}的 provider 只能是 volcengine 或 alicloud"))?,
            ),
            None => None,
        };
        let named = [
            ("product", &self.product),
            ("item", &self.item),
            ("region", &self.region),
            ("zone", &self.zone),
            ("account", &self.account),
            ("instance", &self.instance),
            ("project", &self.project),
            ("subscription", &self.subscription),
            ("currency", &self.currency),
        ];
        let mut dims = Vec::new();
        for (key, values) in named {
            let Some(values) = values else { continue };
            if values.is_empty() {
                return Err(format!("{at}的 {key} 是空列表，不会命中任何账单行"));
            }
            // DIMENSIONS 是维度的唯一出处，这里的键名与费用页排行下拉里的完全一致
            let dim = DIMENSIONS
                .iter()
                .find(|(n, _, _)| *n == key)
                .map(|(_, d, _)| *d)
                .ok_or_else(|| format!("{at}用了未知维度 {key}"))?;
            dims.push((dim, values.clone()));
        }
        let mut columns = Vec::new();
        for c in &self.columns {
            let ops = [
                c.any_of.clone().map(ColumnOp::AnyOf),
                c.like.clone().map(ColumnOp::Like),
                c.not_like.clone().map(ColumnOp::NotLike),
            ];
            let mut ops = ops.into_iter().flatten();
            let op = ops.next().ok_or_else(|| {
                format!("{at}的 columns.{} 须指定 any_of / like / not_like 之一", c.name)
            })?;
            if ops.next().is_some() {
                return Err(format!(
                    "{at}的 columns.{} 同时指定了多种匹配方式，每列只能指定一种",
                    c.name
                ));
            }
            if let ColumnOp::AnyOf(values) = &op
                && values.is_empty()
            {
                return Err(format!("{at}的 columns.{} any_of 是空列表", c.name));
            }
            if c.name.trim().is_empty() {
                return Err(format!("{at}的 columns 缺少列名"));
            }
            columns.push(ColumnMatch { column: c.name.clone(), op });
        }
        Ok(Matcher { provider, dims, columns })
    }

    fn shares(&self, alloc: &Alloc, at: &str) -> Result<Vec<Share>, String> {
        let line = |name: &str| {
            alloc.index_of(name).ok_or_else(|| format!("{at}归属的 {name:?} 不在 lines 中"))
        };
        match (&self.to, &self.split) {
            (Some(_), Some(_)) => Err(format!("{at}同时指定了 to 和 split，只能二选一")),
            (None, None) => Err(format!("{at}未指定费用归属，须给出 to 或 split")),
            (Some(to), None) => Ok(vec![Share { line: line(to)?, weight: 1.0 }]),
            (None, Some(split)) => {
                if split.is_empty() {
                    return Err(format!("{at}的 split 为空"));
                }
                let mut shares = Vec::new();
                for (name, weight) in split {
                    if !weight.is_finite() || *weight <= 0.0 {
                        return Err(format!("{at}给 {name:?} 的权重是 {weight}，应为正数"));
                    }
                    shares.push(Share { line: line(name)?, weight: *weight });
                }
                // 按业务线在 lines 里的顺序排，页面上各行的次序才是配置里写的那个次序
                // （split 是张表，TOML 解出来按键名排序，与业务线的顺序无关）
                shares.sort_by_key(|s| s.line);
                Ok(shares)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
lines = ["甲线", "乙线", "公共"]
unmatched = "公共"

[[include]]
subscription = ["PayAsYouGo", "按量计费"]

[[rules]]
name = "甲线专用机器"
product = ["云服务器 ECS"]
to = "甲线"
columns = [{ name = "intranet_ip", any_of = ["10.0.0.1", "10.0.0.2"] }]

[[rules]]
name = "ECS 其余部分按机器数拆"
product = ["云服务器 ECS"]
split = { "甲线" = 1, "乙线" = 3 }

[prepaid]
subscription = ["Subscription", "包年包月"]
lookback_months = 24
"#;

    #[test]
    fn parses_lines_rules_and_shares() {
        let alloc = Alloc::parse(SAMPLE).unwrap();
        assert_eq!(alloc.lines, ["甲线", "乙线", "公共"]);
        assert_eq!(alloc.unmatched, Some(2));
        assert_eq!(alloc.include.len(), 1);
        assert_eq!(alloc.include[0].dims[0].0, Dimension::Subscription);
        assert_eq!(alloc.rules.len(), 2);

        let first = &alloc.rules[0];
        assert_eq!(first.name, "甲线专用机器");
        assert_eq!(first.matcher.columns[0].column, "intranet_ip");
        assert_eq!(
            first.matcher.columns[0].op,
            ColumnOp::AnyOf(vec!["10.0.0.1".into(), "10.0.0.2".into()])
        );
        assert_eq!(first.split(100.0), vec![(0, 100.0)]);

        // 权重只论相对大小：1 : 3 即 25% 与 75%
        let second = &alloc.rules[1];
        assert_eq!(second.split(100.0), vec![(0, 25.0), (1, 75.0)]);

        // 预付费单独一段，只说哪些行算预付费，去向仍由上面的规则决定
        let prepaid = alloc.prepaid.as_ref().unwrap();
        assert_eq!(prepaid.lookback_months, 24);
        assert_eq!(prepaid.matcher.dims[0].0, Dimension::Subscription);
    }

    /// 归属关乎金额，配置中的错误应当场报出，而非静默算出一份错账。
    #[test]
    fn rejects_configuration_mistakes() {
        let cases = [
            (r#"lines = []"#, "至少需要一条"),
            (r#"lines = ["甲", "甲"]"#, "重复"),
            ("lines = [\"甲\"]\nunmatched = \"乙\"", "不在 lines 中"),
            ("lines = [\"甲\"]\n[[rules]]\nname = \"x\"\nproduct = [\"ECS\"]", "未指定费用归属"),
            (
                "lines = [\"甲\"]\n[[rules]]\nname = \"x\"\nto = \"甲\"\nsplit = { \"甲\" = 1 }",
                "只能二选一",
            ),
            (
                "lines = [\"甲\", \"乙\"]\n[[rules]]\nname = \"x\"\nsplit = { \"甲\" = 1, \"丙\" = 2 }",
                "不在 lines 中",
            ),
            ("lines = [\"甲\"]\n[[rules]]\nname = \"x\"\nsplit = { \"甲\" = 0 }", "应为正数"),
            (
                "lines = [\"甲\"]\n[[rules]]\nname = \"x\"\nto = \"甲\"\ncolumns = [{ name = \"ip\" }]",
                "any_of / like / not_like",
            ),
            (
                "lines = [\"甲\"]\n[[rules]]\nname = \"x\"\nto = \"甲\"\ncolumns = [{ name = \"ip\", like = \"a%\", not_like = \"b%\" }]",
                "只能指定一种",
            ),
            // 键名拼错不能悄悄放过：products 不是 product
            (
                "lines = [\"甲\"]\n[[rules]]\nname = \"x\"\nto = \"甲\"\nproducts = [\"ECS\"]",
                "unknown field",
            ),
            ("lines = [\"甲\"]\n[[include]]\nto = \"甲\"", "不能指定 to / split"),
            (
                "lines = [\"甲\"]\n[[rules]]\nname = \"x\"\nto = \"甲\"\nprovider = \"aws\"",
                "只能是 volcengine",
            ),
            // 预付费那一段不给条件，等于把全部账单都当成预付费
            ("lines = [\"甲\"]\n[prepaid]\n", "未给出任何条件"),
            (
                "lines = [\"甲\"]\n[prepaid]\nsubscription = [\"Subscription\"]\nto = \"甲\"",
                "不能指定 to / split",
            ),
            (
                "lines = [\"甲\"]\n[prepaid]\nsubscription = [\"Subscription\"]\nlookback_months = 0",
                "lookback_months",
            ),
            (
                "lines = [\"甲\"]\n[[rules]]\nname = \"x\"\nto = \"甲\"\nlookback_months = 12",
                "仅用于 [prepaid]",
            ),
        ];
        for (text, needle) in cases {
            let err = Alloc::parse(text).expect_err(text);
            assert!(err.contains(needle), "{text} => {err}");
        }
    }

    /// 仓库中的示例文件必须能够加载——它是使用者据以起步的样板。
    #[test]
    fn the_shipped_example_parses() {
        let alloc =
            Alloc::parse(include_str!("../examples/bill-alloc.toml")).expect("示例文件应当有效");
        assert_eq!(alloc.lines.len(), 4);
        assert_eq!(alloc.unmatched, Some(3));
        assert!(alloc.rules.len() >= 6);
    }

    /// 未命名的规则也应有一个可供显示的名称。
    #[test]
    fn rules_fall_back_to_a_generated_name() {
        let alloc =
            Alloc::parse("lines = [\"甲\"]\n[[rules]]\nto = \"甲\"\nproduct = [\"ECS\"]").unwrap();
        assert_eq!(alloc.rules[0].name, "规则 1");
    }
}
