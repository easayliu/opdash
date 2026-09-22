//! MCP 工具：每个工具 = 一段「模型给的参数 → `/api/*` 查询串」的翻译 + 一段「API 的 JSON →
//! 给模型看的形状」的整理。
//!
//! 整理的原则：模型的上下文是按 token 计费的，页面上要的东西模型多半不要——
//!
//! * 时间戳一律转成配置时区的本地时间字符串，模型不用自己算毫秒；
//! * `stats`（读了多少行）、sparkline、直方图的空桶这类页面装饰不给；
//! * 空字符串的字段直接省掉（一行日志里 trace_id / span_id 常常是空的）；
//! * 长文本（message、异常堆栈）按参数截断，并注明原长；
//! * 大列表有默认上限，比页面的默认值小（日志 50 行、链路 20 条、错误 30 组）。
//!
//! 参数校验大部分不在这里做：翻译成查询串之后由 API 自己校验，错误文本原样交给模型。这里只管
//! 类型（字符串 / 数组 / 布尔）和时间写法。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::OnceLock;

use chrono_tz::Tz;
use serde_json::{Map, Value, json};

use super::{Mcp, fmt_duration, fmt_time, parse_range, parse_time};

pub enum ToolError {
    /// 没有这个工具
    Unknown,
    /// 参数不对 / 查询失败：文本给模型看，它能据此改参数重试
    Failed(String),
    /// 我们自己的问题
    Internal(String),
}

type R<T> = Result<T, String>;

// ---------------------------------------------------------------------------------------------
// 工具目录
// ---------------------------------------------------------------------------------------------

fn prop(ty: &str, desc: &str) -> Value {
    json!({ "type": ty, "description": desc })
}

fn string(desc: &str) -> Value {
    prop("string", desc)
}

fn boolean(desc: &str) -> Value {
    prop("boolean", desc)
}

fn integer(desc: &str) -> Value {
    prop("integer", desc)
}

fn number(desc: &str) -> Value {
    prop("number", desc)
}

fn strings(desc: &str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": desc })
}

fn enumeration(desc: &str, values: &[&str]) -> Value {
    json!({ "type": "string", "enum": values, "description": desc })
}

fn schema(props: Vec<(&str, Value)>, required: &[&str]) -> Value {
    let properties: Map<String, Value> =
        props.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// from / to / range 三个时间参数，几乎每个工具都有。
fn time_props(default_range: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("from", string("开始时间；不给 = to - range")),
        ("to", string("结束时间，默认现在")),
        ("range", string(&format!("时间跨度 15m / 1h / 7d，没给 from 时用，默认 {default_range}"))),
    ]
}

/// 日志筛选条件，search_logs / log_histogram / log_facets 共用。
fn log_filter_props() -> Vec<(&'static str, Value)> {
    vec![
        (
            "q",
            string(
                "关键字。空格分隔 = AND，`a OR b`，`-word` 排除，引号括短语；默认子串、不分大小写",
            ),
        ),
        ("regex", boolean("q 按 RE2 正则解释")),
        ("level", strings("级别，如 [\"ERROR\"]")),
        (
            "filters",
            json!({
                "type": "object",
                "description": "按维度列筛，键是列名、值一个或多个（多个是 OR），如 {\"service_name\": \"order\", \"pod\": [\"a\", \"b\"]}。列名见 get_meta 的 logs.dimensions",
                "additionalProperties": { "type": ["string", "array"], "items": { "type": "string" } },
            }),
        ),
        ("logger", string("精确匹配")),
        ("thread", string("精确匹配")),
        ("host", string("精确匹配")),
    ]
}

/// 账期和筛选参数，三个费用工具共用。账单没有「最近 1 小时」这种说法，时间参数是账期。
fn bill_props() -> Vec<(&'static str, Value)> {
    vec![
        ("from", string("起始账期 YYYY-MM，不给就从 to 往前数 months 个")),
        ("to", string("结束账期 YYYY-MM，默认当前账期")),
        ("months", integer("看最近几个账期，默认 6，上限 36")),
        (
            "amount",
            enumeration(
                "金额口径，默认 payable（应付）；paid = 现金；original = 原价",
                &["payable", "paid", "original"],
            ),
        ),
        ("provider", strings("只看 volcengine / alicloud，不给 = 都要")),
        (
            "filters",
            json!({
                "type": "object",
                "description": "按维度筛，键是 product / item / region / zone / account / instance / project / subscription / currency，值一个或多个（OR）",
                "additionalProperties": { "type": ["string", "array"], "items": { "type": "string" } },
            }),
        ),
        ("q", string("模糊搜产品 / 计费项 / 实例名")),
    ]
}

/// 工具目录。`inputSchema` 是 JSON Schema，客户端和模型都靠它知道能传什么。
///
/// `metrics_enabled = false`（部署没配指标表）时不列指标类工具，`bills_enabled = false`
/// （没部署 goscan）时不列费用类工具：列出来模型也只会换来一个「未启用」，白占上下文。`initialize` 里 `listChanged` 仍然是 `false`——改变工具列表要发
/// `notifications/tools/list_changed`，无状态端点没有服务端到客户端的流发不出去；指标表是部署时
/// 定的，跑着跑着变的情况不存在。
pub fn list(metrics_enabled: bool, bills_enabled: bool) -> Vec<Value> {
    // 全是只读查询：标上 annotations，客户端（Claude Code / Cursor 这类）就不必每次调用都问人一遍
    let tool = |name: &str, description: &str, input: Value| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": input,
            // 只留 readOnlyHint：另外几个 hint 在只读工具上没有意义，而目录是按轮计费的
            "annotations": { "readOnlyHint": true },
        })
    };
    let mut tools = vec![
        tool(
            "get_meta",
            "opdash 的基本信息：版本、时区、当前时间、三张表各有哪些可筛的维度列、指标表是否启用、各项上限。开工前调一次，之后 filters 里的列名照这里的写。",
            schema(vec![], &[]),
        ),
        tool(
            "list_services",
            "时间范围内有 span 上报的服务名列表（按 span 数排序）。不知道服务叫什么时先调它。",
            schema(
                [time_props("1h"), vec![("limit", integer("默认 200"))]].concat(),
                &[],
            ),
        ),
        tool(
            "service_overview",
            "每个服务的请求量 / 错误率 / P50-P99，以及和对比时段比的变化；health 是 red / yellow / ok 的结论（阈值见 instructions）。回答「现在谁不对」。只看入口 span（Server / Consumer）。",
            schema(
                [
                    time_props("1h"),
                    vec![
                        ("compare", enumeration("对比时段，默认 day（昨天同时段，避开早晚高峰）", &["day", "week", "prev"])),
                        ("only_unhealthy", boolean("只要 health 不是 ok 的")),
                        ("limit", integer("默认 50，不健康的排前面")),
                    ],
                ]
                .concat(),
                &[],
            ),
        ),
        tool(
            "service_operations",
            "一个服务（也可以一次给多个，最多 24 个，那时每行带 service）的接口表：每个 span_name 的量 / 错误率 / P50-P99 和对比变化；消失的标 gone、新增的标 new。回答「哪个接口不对」。",
            schema(
                [
                    vec![
                        ("service", json!({
                            "type": ["string", "array"],
                            "items": { "type": "string" },
                            "description": "服务名（service_name）；一次最多 24 个，多个时每行带 service",
                        })),
                        ("kind", enumeration("默认 entry（对外的接口）；client = 它调下游的", &["entry", "client"])),
                        ("compare", enumeration("对比时段，默认 day", &["day", "week", "prev", "none"])),
                        ("sort", enumeration("默认 requests；*_change 是相对对比时段涨得最多的", &["requests", "errors", "p95", "p95_change", "errors_change"])),
                        ("limit", integer("默认 50")),
                    ],
                    time_props("1h"),
                ]
                .concat(),
                &["service"],
            ),
        ),
        tool(
            "service_timeseries",
            "一个服务（可指定某个接口）的量 / 错误 / P50-P99 曲线，每个点带对比时段同一格的值。回答「从什么时候开始变慢 / 报错」。",
            schema(
                [
                    vec![
                        ("service", string("服务名")),
                        ("span_name", string("只看这个接口")),
                        ("compare", enumeration("对比时段，默认 day", &["day", "week", "prev", "none"])),
                    ],
                    time_props("1h"),
                ]
                .concat(),
                &["service"],
            ),
        ),
        tool(
            "error_groups",
            "出错的 span 按「同一种报错」归堆（异常类 + 消息 + 服务 + 接口），带次数 / 影响多少条链路 / 首末时间 / 一条样本链路（sample_trace 拿去 get_trace）。回答「在报什么错」。",
            schema(
                [
                    vec![
                        ("service", string("只看这个服务")),
                        ("span_name", string("只看这个接口")),
                        ("kind", enumeration("默认 entry（和总览同口径）", &["entry", "client", "all"])),
                        ("limit", integer("默认 30，按次数排")),
                    ],
                    time_props("1h"),
                ]
                .concat(),
                &[],
            ),
        ),
        tool(
            "search_traces",
            "检索链路：按服务、接口、span 类型、只看错误、耗时区间、span / resource 属性筛，按时间或耗时排。返回每条 trace 的摘要。不选服务时范围最多 6 小时。",
            schema(
                [
                    vec![
                        ("service", string("服务名")),
                        ("span_name", string("接口 / span 名")),
                        ("kind", strings("Server / Client / Producer / Consumer / Internal；sort=duration 时默认只看入口")),
                        ("error_only", boolean("只看出错的")),
                        ("min_ms", number("耗时下限（毫秒）")),
                        ("max_ms", number("耗时上限（毫秒）")),
                        ("attr", strings("span 属性，每项 key=value；只写 key = 有这个属性")),
                        ("rattr", strings("resource 属性，写法同 attr")),
                        ("filters", json!({ "type": "object", "description": "span 表的维度列筛选，同 search_logs", "additionalProperties": { "type": ["string", "array"], "items": { "type": "string" } } })),
                        ("sort", enumeration("默认 time（最新在前）", &["time", "duration"])),
                        ("limit", integer("默认 20，上限 100")),
                    ],
                    time_props("1h"),
                ]
                .concat(),
                &[],
            ),
        ),
        tool(
            "list_attrs",
            "列属性名，或某个属性名的取值分布（采样近似）。写 search_traces 的 attr / query_metric 的 by 之前先用它，别猜。on=span 要给 service，on=metric 要给 metric。",
            schema(
                [
                    vec![
                        ("on", enumeration("默认 span", &["span", "metric"])),
                        ("scope", enumeration("默认 attributes（数据点 / span 自己的）；resource = 上报方的", &["attributes", "resource"])),
                        ("service", string("on=span 必填")),
                        ("metric", string("on=metric 必填")),
                        ("key", string("给了就列这个键的取值，不给就列键名")),
                        ("limit", integer("列键默认 200，列取值默认 50")),
                    ],
                    time_props("1h"),
                ]
                .concat(),
                &[],
            ),
        ),
        tool(
            "get_trace",
            "一条链路的全部 span：depth（层级）、offset_ms（相对开始）、duration_ms、status；span 太多时保留全部出错的和最慢的。include_logs 顺带取这条 trace 的日志。",
            schema(
                vec![
                    ("trace_id", string("32 位 hex")),
                    ("at", string("这条链路大概什么时候（从别的结果里拿）；带上能少扫很多，不带扫全部分区")),
                    ("max_spans", integer("默认 200")),
                    ("include_logs", boolean("顺带取这条 trace 的日志")),
                    ("log_limit", integer("include_logs 时几条，默认 100")),
                ],
                &["trace_id"],
            ),
        ),
        tool(
            "get_span",
            "一个 span 的全部属性、resource 属性、events（异常堆栈在 exception 事件里）和 links。get_trace 里看到哪个 span 出错，用它看到底错在哪。",
            schema(
                vec![
                    ("trace_id", string("32 位 hex")),
                    ("span_id", string("16 位 hex")),
                    ("at", string("大概的时间，带上更快")),
                    ("max_chars", integer("单个属性值保留多少字符，默认 4000")),
                ],
                &["trace_id", "span_id"],
            ),
        ),
        tool(
            "search_logs",
            "检索日志：关键字 / 正则、级别、维度列、logger、线程、trace_id / span_id。默认最新在前。**任何情况下都尽量给时间范围**：span_id 没有索引、trace_id 的 bloom filter 只剪掉九成七，按 id 查不给范围要扫满 30 天（实测 38 GB / 9~18 s），知道大概时刻就给个 range。message 超长会截断并注明原长。",
            schema(
                [
                    log_filter_props(),
                    vec![
                        ("trace_id", string("32 位 hex")),
                        ("span_id", string("16 位 hex")),
                        ("order", enumeration("默认 desc（最新在前）", &["desc", "asc"])),
                        ("limit", integer("默认 50，上限 200")),
                        ("offset", integer("翻页偏移")),
                        ("max_message_chars", integer("单条 message 保留多少字符，默认 2000")),
                        ("count", boolean("顺带数一共多少条（默认不数）。没有关键字时它要多扫一遍整个时间范围，只想知道量用 log_histogram 更划算；只在第一页有效")),
                    ],
                    time_props("1h"),
                ]
                .concat(),
                &[],
            ),
        ),
        tool(
            "log_histogram",
            "日志条数随时间的分布，按级别分开。筛选条件同 search_logs。回答「错误几点开始的、量有多大」，比翻日志便宜得多。",
            schema([log_filter_props(), time_props("1h")].concat(), &[]),
        ),
        tool(
            "log_facets",
            "几个维度列各自最常见的取值和条数（近似计数）。筛选条件同 search_logs。回答「报错集中在哪个 pod」。",
            schema(
                [
                    vec![
                        ("fields", strings("要统计的列，如 [\"pod\", \"level\"]；可用列见 get_meta，level / logger / host 也行")),
                        ("limit", integer("每列几个取值，默认 20")),
                    ],
                    log_filter_props(),
                    time_props("1h"),
                ]
                .concat(),
                &["fields"],
            ),
        ),
        tool(
            "log_context",
            "某条日志前后的原文（同一台主机、同一个文件），时间正序。host / file / time 从 search_logs 的结果行里原样拿。",
            schema(
                vec![
                    ("host", string("日志行里的 host")),
                    ("file", string("日志行里的 file")),
                    ("time", string("日志行里的 time，原样传")),
                    ("before", integer("往前几行，默认 30")),
                    ("after", integer("往后几行，默认 30")),
                    ("max_message_chars", integer("默认 4000")),
                ],
                &["host", "file", "time"],
            ),
        ),
        tool(
            "list_metrics",
            "指标目录：指标名、类型、单位、哪些服务在报。只扫最近 6 小时，很便宜。问 JVM / GC / 内存 / CPU / 线程 / 连接池 / 消息积压这类，先用它按 match 找名字，找到了再 query_metric。",
            schema(
                [
                    vec![
                        ("service", strings("只看这些服务报的")),
                        ("match", string("指标名包含这个子串，如 jvm.memory")),
                        ("limit", integer("默认 200")),
                    ],
                    time_props("1h"),
                ]
                .concat(),
                &[],
            ),
        ),
        tool(
            "query_metric",
            "查一个上报指标（名字带点：jvm.* / process.* / http.server.*）的曲线。服务的请求量 / 错误率 / P95 不在这张表，那些用 service_*。agg / field 不给就按指标类型自动挑（直方图的 value 列是空的，别拿它聚合）。by 分组：固定列直接写，数据点属性写属性名，resource 属性加 res: 前缀。",
            schema(
                [
                    vec![
                        ("metric", string("指标名，如 http.server.request.duration")),
                        ("agg", enumeration("不给按类型自动挑", &["avg", "sum", "min", "max", "last", "count", "rate", "increase", "mean", "quantile"])),
                        ("field", enumeration("取哪一列；不给按类型自动挑", &["value", "count", "sum", "min", "max"])),
                        ("by", strings("分组维度，如 [\"service_name\", \"http.route\"]")),
                        ("q", strings("agg=quantile 的分位数，默认 [\"0.95\"]，最多 5 个")),
                        ("service", strings("只看这些服务")),
                        ("attr", strings("数据点属性，key=value")),
                        ("rattr", strings("resource 属性，key=value")),
                        ("step", integer("桶宽（秒），不给自动挑")),
                        ("limit", integer("最多几条时间线，默认 10")),
                    ],
                    time_props("1h"),
                ]
                .concat(),
                &["metric"],
            ),
        ),
        tool(
            "metric_exemplars",
            "指标点上挂的 trace id：尖峰直接换成一条链路，拿去 get_trace。query_metric 看到 P99 尖峰之后用它，不用再按时间去撞。只有上报了 exemplar 的指标（一般是直方图）有。",
            schema(
                [
                    vec![
                        ("metric", string("指标名")),
                        ("service", strings("只看这些服务")),
                        ("attr", strings("数据点属性，key=value")),
                        ("rattr", strings("resource 属性，key=value")),
                        ("limit", integer("默认 20，上限 500")),
                    ],
                    time_props("1h"),
                ]
                .concat(),
                &["metric"],
            ),
        ),
        tool(
            "metric_events",
            "进程重启 / pod 新启动的时刻：restart = 累积 counter 掉回去了，start = 这个 pod 在窗口里第一次出现。metric 给一个累积 counter，JVM 服务用 jvm.cpu.time。",
            schema(
                [
                    vec![
                        ("metric", string("一个累积 counter，如 jvm.cpu.time")),
                        ("service", strings("只看这些服务")),
                        ("field", enumeration("默认 value；直方图用 count", &["value", "count", "sum"])),
                    ],
                    time_props("1h"),
                ]
                .concat(),
                &["metric"],
            ),
        ),
        tool(
            "cost_summary",
            "云账单按账期的花费，分云给。回答「这几个月花了多少」「这个月比上个月涨了吗」。granularity=day 改成按天。",
            schema(
                [
                    bill_props(),
                    vec![(
                        "granularity",
                        enumeration("默认 month（按账期）；day = 按天", &["month", "day"]),
                    )],
                ]
                .concat(),
                &[],
            ),
        ),
        tool(
            "cost_breakdown",
            "账单按某个维度排行：钱花在哪个产品 / 地域 / 账号 / 实例上。回答「谁最贵」「多出来的钱是哪个产品」。两朵云合在一起排，每行带各云的分摊。",
            schema(
                [
                    vec![(
                        "by",
                        enumeration(
                            "按哪个维度排，默认 product",
                            &[
                                "product",
                                "item",
                                "region",
                                "zone",
                                "account",
                                "instance",
                                "project",
                                "subscription",
                                "currency",
                            ],
                        ),
                    )],
                    bill_props(),
                    vec![("limit", integer("默认 20，上限 200"))],
                ]
                .concat(),
                &[],
            ),
        ),
        tool(
            "cost_detail",
            "账单明细，一行一个计费项（产品 / 实例 / 地域 / 用量 / 金额）。排行看出哪个贵之后用它看具体在计什么费。一页只出一朵云的。",
            schema(
                [
                    vec![
                        ("provider", enumeration("哪朵云，默认挑一张有的表", &["volcengine", "alicloud"])),
                        ("granularity", enumeration("阿里云专用：monthly（默认）/ daily", &["monthly", "daily"])),
                    ],
                    bill_props(),
                    vec![
                        ("limit", integer("默认 30，上限 1000")),
                        ("offset", integer("翻页")),
                    ],
                ]
                .concat(),
                &[],
            ),
        ),
    ];
    if !metrics_enabled {
        tools.retain(|t| !METRIC_TOOLS.contains(&t["name"].as_str().unwrap_or("")));
    }
    if !bills_enabled {
        tools.retain(|t| !BILL_TOOLS.contains(&t["name"].as_str().unwrap_or("")));
    }
    // 描述前面统一标一句数据来源，和 instructions 里的术语表对齐
    for t in &mut tools {
        let source = source_of(t["name"].as_str().unwrap_or(""));
        let desc = t["description"].as_str().unwrap_or("").to_owned();
        t["description"] = json!(format!("[{source}] {desc}"));
    }
    tools
}

/// 每个工具吃的是哪张表。opdash 里「指标」是个重载的词：服务的请求量 / 错误率 / 延迟分位是
/// **span 表**现算的，JVM / GC / CPU / 连接池那些是 metricpipe 上报到**指标表**的。两类工具的
/// 描述里都有「请求量」「P95」「指标」这些字眼，模型光读描述分不出来，问「看指标」时经常走错
/// 一边。来源标在描述最前面，配合 [`crate::mcp::instructions`] 里的术语表消歧。
fn source_of(name: &str) -> &'static str {
    match name {
        "get_meta" => "元信息",
        "search_logs" | "log_histogram" | "log_facets" | "log_context" => "日志表",
        "list_metrics" | "query_metric" | "metric_exemplars" | "metric_events" => "指标表",
        "list_attrs" => "span 表 / 指标表",
        "cost_summary" | "cost_breakdown" | "cost_detail" => "账单表",
        _ => "span 表",
    }
}

/// 指标表没启用时要从目录里拿掉的工具。
const METRIC_TOOLS: &[&str] =
    &["list_metrics", "query_metric", "metric_events", "metric_exemplars"];

/// 账单表（goscan）没启用时要拿掉的工具。
const BILL_TOOLS: &[&str] = &["cost_summary", "cost_breakdown", "cost_detail"];

// ---------------------------------------------------------------------------------------------
// 参数读取
// ---------------------------------------------------------------------------------------------

/// 模型给的参数。类型上宽松一点：数字当字符串用、逗号分隔的字符串当数组用、`"true"` 当布尔用——
/// 模型偶尔会这么写，拒掉只是多一轮往返。
struct Args<'a> {
    map: &'a Map<String, Value>,
    now_ms: i64,
    tz: Tz,
}

/// 查询串里被 API 当控制参数的键，`filters` 里不能用它们当列名（会把真正的参数顶掉）。
const RESERVED_KEYS: &[&str] = &[
    "from",
    "to",
    "q",
    "regex",
    "level",
    "logger",
    "thread",
    "host",
    "trace_id",
    "span_id",
    "order",
    "limit",
    "offset",
    "field",
    "format",
    "ts",
    "file",
    "before",
    "after",
    "count",
    "service",
    "span_name",
    "kind",
    "error_only",
    "min_ms",
    "max_ms",
    "attr",
    "rattr",
    "sort",
    "scope",
    "key",
    "compare",
    "at",
    "span",
    "metric",
    "agg",
    "by",
    "step",
    "column",
];

impl Args<'_> {
    fn get(&self, key: &str) -> Option<&Value> {
        self.map.get(key).filter(|v| !v.is_null())
    }

    fn string(&self, key: &str) -> R<Option<String>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.trim().to_owned()).filter(|s| !s.is_empty())),
            Some(Value::Number(n)) => Ok(Some(n.to_string())),
            Some(Value::Bool(b)) => Ok(Some(b.to_string())),
            Some(other) => Err(format!("参数 {key} 应为字符串，不是 {other}")),
        }
    }

    fn required(&self, key: &str) -> R<String> {
        self.string(key)?.ok_or_else(|| format!("缺少参数 {key}"))
    }

    /// 数组，或逗号分隔的字符串。
    fn list(&self, key: &str) -> R<Vec<String>> {
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s.trim().to_owned()),
                    Value::Number(n) => Ok(n.to_string()),
                    other => Err(format!("参数 {key} 的每一项应为字符串，不是 {other}")),
                })
                .filter(|r| !matches!(r, Ok(s) if s.is_empty()))
                .collect(),
            Some(Value::String(s)) => Ok(s
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()),
            Some(Value::Number(n)) => Ok(vec![n.to_string()]),
            Some(other) => Err(format!("参数 {key} 应为字符串数组，不是 {other}")),
        }
    }

    fn boolean(&self, key: &str) -> R<Option<bool>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => Ok(Some(true)),
                "false" | "0" | "no" | "" => Ok(Some(false)),
                _ => Err(format!("参数 {key} 应为 true / false，不是 {s:?}")),
            },
            Some(Value::Number(n)) => Ok(Some(n.as_f64().unwrap_or(0.0) != 0.0)),
            Some(other) => Err(format!("参数 {key} 应为 true / false，不是 {other}")),
        }
    }

    fn f64(&self, key: &str) -> R<Option<f64>> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Number(n)) => Ok(n.as_f64()),
            Some(Value::String(s)) => s
                .trim()
                .parse::<f64>()
                .map(Some)
                .map_err(|_| format!("参数 {key} 应为数字，不是 {s:?}")),
            Some(other) => Err(format!("参数 {key} 应为数字，不是 {other}")),
        }
    }

    fn u32(&self, key: &str) -> R<Option<u32>> {
        match self.f64(key)? {
            None => Ok(None),
            Some(n) if n >= 0.0 && n <= u32::MAX as f64 => Ok(Some(n as u32)),
            Some(n) => Err(format!("参数 {key} 应为非负整数，不是 {n}")),
        }
    }

    /// `[1, max]` 之间，没给用默认值。
    fn limit(&self, key: &str, default: u32, max: u32) -> R<u32> {
        Ok(self.u32(key)?.unwrap_or(default).clamp(1, max))
    }

    fn time(&self, key: &str) -> R<Option<i64>> {
        self.get(key)
            .map(|v| parse_time(v, self.now_ms, self.tz).map_err(|e| format!("参数 {key}: {e}")))
            .transpose()
    }

    /// `(from, to)`。to 默认现在；from 默认 to - range（range 默认 `default_range`）。
    fn window(&self, default_range: &str) -> R<(i64, i64)> {
        let to = self.time("to")?.unwrap_or(self.now_ms);
        let from = match self.time("from")? {
            Some(f) => f,
            None => {
                let range = self.string("range")?;
                to - parse_range(range.as_deref().unwrap_or(default_range))?
            }
        };
        if from >= to {
            return Err(format!(
                "开始时间 {} 必须早于结束时间 {}",
                fmt_time(from, self.tz),
                fmt_time(to, self.tz)
            ));
        }
        Ok((from, to))
    }

    /// 一个时间参数都没给就是 `None`（按 id 查不需要时间范围）。
    fn window_opt(&self, default_range: &str) -> R<Option<(i64, i64)>> {
        if self.get("from").is_none() && self.get("to").is_none() && self.get("range").is_none() {
            return Ok(None);
        }
        self.window(default_range).map(Some)
    }

    /// `filters` 对象 → `(列名, 取值们)`。
    fn filters(&self) -> R<Vec<(String, Vec<String>)>> {
        let Some(v) = self.get("filters") else { return Ok(Vec::new()) };
        let Some(obj) = v.as_object() else {
            return Err(format!("参数 filters 应为对象（列名 → 取值），不是 {v}"));
        };
        let mut out = Vec::new();
        for (key, val) in obj {
            let key = key.trim();
            if key.is_empty()
                || RESERVED_KEYS.contains(&key)
                || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            {
                return Err(format!(
                    "filters 里的列名 {key:?} 不能用；列名见 get_meta 的 dimensions"
                ));
            }
            let values: Vec<String> = match val {
                Value::Array(items) => items
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => Ok(s.clone()),
                        Value::Number(n) => Ok(n.to_string()),
                        other => Err(format!("filters.{key} 的每一项应为字符串，不是 {other}")),
                    })
                    .collect::<R<_>>()?,
                Value::String(s) => vec![s.clone()],
                Value::Number(n) => vec![n.to_string()],
                Value::Null => Vec::new(),
                other => return Err(format!("filters.{key} 应为字符串或数组，不是 {other}")),
            };
            let values: Vec<String> = values.into_iter().filter(|s| !s.trim().is_empty()).collect();
            if !values.is_empty() {
                out.push((key.to_owned(), values));
            }
        }
        Ok(out)
    }
}

/// 查询串拼装。同名键可以重复（`attr=a=1&attr=b=2`），API 那边就是这么收的。
///
/// 攒成 `Vec` 最后再编码：`form_urlencoded::Serializer` 里有个 `&dyn Fn`，不是 `Send`，
/// 跨 `.await` 拿着它整个工具的 future 就进不了 axum 的 handler。
struct Qs(Vec<(String, String)>);

impl Qs {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn push(&mut self, key: &str, value: impl AsRef<str>) -> &mut Self {
        self.0.push((key.to_owned(), value.as_ref().to_owned()));
        self
    }

    fn push_opt(&mut self, key: &str, value: Option<impl AsRef<str>>) -> &mut Self {
        if let Some(v) = value {
            self.push(key, v);
        }
        self
    }

    fn push_all(&mut self, key: &str, values: &[String]) -> &mut Self {
        for v in values {
            self.push(key, v);
        }
        self
    }

    fn push_dims(&mut self, dims: &[(String, Vec<String>)]) -> &mut Self {
        for (k, vs) in dims {
            self.push_all(k, vs);
        }
        self
    }

    fn window(&mut self, (from, to): (i64, i64)) -> &mut Self {
        self.push("from", from.to_string()).push("to", to.to_string())
    }

    fn finish(self) -> String {
        let mut ser = form_urlencoded::Serializer::new(String::new());
        for (k, v) in &self.0 {
            ser.append_pair(k, v);
        }
        ser.finish()
    }
}

// ---------------------------------------------------------------------------------------------
// 调用入口
// ---------------------------------------------------------------------------------------------

/// 单次工具结果的字节上限。`search_logs` 最多能要 200 行 × 20 万字符的 message，真返回回去就是
/// 几十 MB 灌进模型的上下文——API 那边的护栏管的是 ClickHouse 读多少行，管不到这一头。超了就
/// 砍列表、再不行截长文本，并在 notes 里说清楚砍了什么。
///
/// 64 KiB 大约是两万多 token：一次工具调用最多吃掉这么多上下文，正常的一页日志（50 行）远用不到。
const MAX_TOOL_BYTES: usize = 64 << 10;

/// 整份工具目录（含指标工具），校验参数名用。
fn all_tools() -> &'static [Value] {
    static ALL: OnceLock<Vec<Value>> = OnceLock::new();
    ALL.get_or_init(|| list(true, true))
}

/// 模型偶尔会把参数名记错（`service` 写成 `service_name`）。`inputSchema` 里写了
/// `additionalProperties: false`，但客户端基本不校验，写错的参数就被静默忽略——查回来的是全站
/// 的数，看着还挺像回事，这种错最难发现。这里直接拒掉，顺手把认识的参数名列给它。
fn check_arg_names(def: &Value, arguments: &Map<String, Value>) -> R<()> {
    let props = &def["inputSchema"]["properties"];
    let unknown: Vec<&str> =
        arguments.keys().filter(|k| props.get(k.as_str()).is_none()).map(String::as_str).collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let known: Vec<&str> =
        props.as_object().map(|m| m.keys().map(String::as_str).collect()).unwrap_or_default();
    Err(format!(
        "不认识的参数: {}；{} 接受的参数是: {}",
        unknown.join(", "),
        def["name"].as_str().unwrap_or("这个工具"),
        known.join(", "),
    ))
}

fn map_len(m: &Map<String, Value>) -> usize {
    serde_json::to_string(m).map(|s| s.len()).unwrap_or(0)
}

/// 把结果压进字节预算：反复把最长的那个顶层数组砍一半，还超就把长字符串截短。两样都做不到
/// （比如单个 span 的一个属性就几 MB）时原样返回——宁可大一次，也不返回一个看不出被动过手脚的
/// 结果。
fn fit_budget(value: Value, budget: usize) -> Value {
    let Value::Object(mut map) = value else {
        return value;
    };
    if map_len(&map) <= budget {
        return Value::Object(map);
    }
    let original: BTreeMap<String, usize> =
        map.iter().filter_map(|(k, v)| v.as_array().map(|a| (k.clone(), a.len()))).collect();
    let mut kept: BTreeMap<String, usize> = BTreeMap::new();
    loop {
        let len = map_len(&map);
        if len <= budget {
            break;
        }
        let longest = map
            .iter()
            .filter_map(|(k, v)| v.as_array().map(|a| (k.clone(), a.len())))
            .max_by_key(|(_, n)| *n);
        let Some((key, n)) = longest.filter(|(_, n)| *n > 1) else {
            break;
        };
        // 按「超了多少」直接估该留几项，再至少砍掉一半保证每轮都在收敛：一次超大的结果
        // 不值得为它反复序列化十几遍
        let keep = (n * budget / len).min(n / 2).max(1);
        if let Some(Value::Array(items)) = map.get_mut(&key) {
            items.truncate(keep);
        }
        kept.insert(key, keep);
    }
    let mut shortened = false;
    for max_chars in [2000_usize, 200] {
        if map_len(&map) <= budget {
            break;
        }
        if let Value::Object(m) = truncate_strings(&Value::Object(map.clone()), max_chars) {
            map = m;
            shortened = true;
        }
    }
    if kept.is_empty() && !shortened {
        return Value::Object(map);
    }
    let mut what: Vec<String> = kept
        .iter()
        .map(|(k, keep)| format!("{k} 只留了 {keep} 项（共 {}）", original.get(k).unwrap_or(keep)))
        .collect();
    if shortened {
        what.push("长文本被截短".to_owned());
    }
    note(
        &mut map,
        format!(
            "结果超过 {} KB，已截断：{}。想要全部：缩小时间范围、调小 limit / max_message_chars，或者分几次查",
            budget >> 10,
            what.join("，"),
        ),
    );
    Value::Object(map)
}

/// 一次工具调用的结果：给模型的文本，外加一个「一条记录都没有」的标记——那既可能是范围不对，
/// 也可能是选错了工具，日志里单独标出来才看得见这类问题有多少。
pub struct ToolOutput {
    pub text: String,
    pub empty: bool,
}

/// 结果里一条记录都没有：顶层至少有一个列表，而且所有列表都是空的。
fn is_empty_result(v: &Value) -> bool {
    let Some(map) = v.as_object() else {
        return false;
    };
    let mut lists = map.values().filter_map(Value::as_array).peekable();
    lists.peek().is_some() && lists.all(Vec::is_empty)
}

/// 跑一个工具，返回给模型看的文本（紧凑 JSON）。
pub async fn call(
    mcp: &Mcp,
    name: &str,
    arguments: &Map<String, Value>,
) -> Result<ToolOutput, ToolError> {
    let Some(def) = all_tools().iter().find(|t| t["name"] == name) else {
        return Err(ToolError::Unknown);
    };
    check_arg_names(def, arguments).map_err(ToolError::Failed)?;
    let a = Args { map: arguments, now_ms: mcp.state.now_ms(), tz: mcp.tz() };
    let out = match name {
        "get_meta" => get_meta(mcp, &a).await,
        "list_services" => list_services(mcp, &a).await,
        "service_overview" => service_overview(mcp, &a).await,
        "service_operations" => service_operations(mcp, &a).await,
        "service_timeseries" => service_timeseries(mcp, &a).await,
        "error_groups" => error_groups(mcp, &a).await,
        "search_traces" => search_traces(mcp, &a).await,
        "get_trace" => get_trace(mcp, &a).await,
        "get_span" => get_span(mcp, &a).await,
        "search_logs" => search_logs(mcp, &a).await,
        "log_histogram" => log_histogram(mcp, &a).await,
        "log_facets" => log_facets(mcp, &a).await,
        "log_context" => log_context(mcp, &a).await,
        "list_attrs" => list_attrs(mcp, &a).await,
        "list_metrics" => list_metrics(mcp, &a).await,
        "query_metric" => query_metric(mcp, &a).await,
        "metric_exemplars" => metric_exemplars(mcp, &a).await,
        "metric_events" => metric_events(mcp, &a).await,
        "cost_summary" => cost_summary(mcp, &a).await,
        "cost_breakdown" => cost_breakdown(mcp, &a).await,
        "cost_detail" => cost_detail(mcp, &a).await,
        _ => return Err(ToolError::Unknown),
    };
    let value = fit_budget(out.map_err(ToolError::Failed)?, MAX_TOOL_BYTES);
    let empty = is_empty_result(&value);
    let text = serde_json::to_string(&value).map_err(|e| ToolError::Internal(e.to_string()))?;
    Ok(ToolOutput { text, empty })
}

// ---------------------------------------------------------------------------------------------
// 整理结果的小工具
// ---------------------------------------------------------------------------------------------

fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn str_of(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_owned()
}

fn i64_of(v: &Value, key: &str) -> i64 {
    v[key].as_i64().or_else(|| v[key].as_f64().map(|f| f as i64)).unwrap_or(0)
}

fn u64_of(v: &Value, key: &str) -> u64 {
    v[key].as_u64().or_else(|| v[key].as_f64().map(|f| f.max(0.0) as u64)).unwrap_or(0)
}

fn f64_of(v: &Value, key: &str) -> f64 {
    v[key].as_f64().unwrap_or(0.0)
}

/// 保留 `digits` 位小数。
fn round(x: f64, digits: i32) -> f64 {
    let k = 10f64.powi(digits);
    (x * k).round() / k
}

/// 0 ~ 1 → 百分数（两位小数）。
fn pct(rate: f64) -> f64 {
    round(rate * 100.0, 2)
}

/// 相对变化：`(cur - prev) / prev`，百分数；prev 是 0 时给 null。
fn change_pct(cur: f64, prev: f64) -> Value {
    if prev > 0.0 { json!(round((cur - prev) / prev * 100.0, 1)) } else { Value::Null }
}

/// 倍数：`cur / prev`，一位小数；prev 是 0 时给 null。
fn ratio(cur: f64, prev: f64) -> Value {
    if prev > 0.0 { json!(round(cur / prev, 2)) } else { Value::Null }
}

/// 按字符截断，注明原长。
fn truncate(s: &str, max_chars: usize) -> (String, bool) {
    let n = s.chars().count();
    if n <= max_chars {
        return (s.to_owned(), false);
    }
    let head: String = s.chars().take(max_chars).collect();
    (format!("{head}…[共 {n} 字符，已截断]"), true)
}

/// 递归截断一个 JSON 里所有长字符串（span 属性里的堆栈、SQL）。
fn truncate_strings(v: &Value, max_chars: usize) -> Value {
    match v {
        Value::String(s) => Value::String(truncate(s, max_chars).0),
        Value::Array(items) => {
            Value::Array(items.iter().map(|x| truncate_strings(x, max_chars)).collect())
        }
        Value::Object(m) => Value::Object(
            m.iter().map(|(k, x)| (k.clone(), truncate_strings(x, max_chars))).collect(),
        ),
        other => other.clone(),
    }
}

/// 一行日志：`ts_ms` → `time`，空字符串省掉，message 截断。维度列（service_name / pod…）在
/// API 响应里已经是平铺的，原样带过去。
fn shape_log_row(row: &Value, tz: Tz, max_chars: usize) -> Value {
    let mut out = Map::new();
    out.insert("time".into(), json!(fmt_time(i64_of(row, "ts_ms"), tz)));
    let Some(obj) = row.as_object() else { return Value::Object(out) };
    for (k, v) in obj {
        match k.as_str() {
            "ts_ms" | "message_len" => {}
            "message" => {
                let raw = v.as_str().unwrap_or("");
                let (text, cut) = truncate(raw, max_chars);
                out.insert("message".into(), json!(text));
                // API 那边按 --max-message-chars 截过一次的话会给原长，比我们数出来的准
                if let Some(len) = row["message_len"].as_u64() {
                    out.insert("message_len".into(), json!(len));
                } else if cut {
                    out.insert("message_len".into(), json!(raw.chars().count()));
                }
            }
            _ => {
                if v.as_str().is_some_and(str::is_empty) {
                    continue;
                }
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

fn shape_log_rows(rows: &Value, tz: Tz, max_chars: usize) -> Vec<Value> {
    arr(rows).iter().map(|r| shape_log_row(r, tz, max_chars)).collect()
}

fn note(out: &mut Map<String, Value>, text: String) {
    match out.get_mut("notes") {
        Some(Value::Array(items)) => items.push(json!(text)),
        _ => {
            out.insert("notes".into(), json!([text]));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 各个工具
// ---------------------------------------------------------------------------------------------

async fn get_meta(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let meta = mcp.get("/api/meta", "").await?;
    let table = |t: &Value| json!({ "table": t["table"], "dimensions": t["dimensions"] });
    let mut out = json!({
        "version": meta["version"],
        "timezone": meta["timezone"],
        "now": fmt_time(i64_of(&meta, "now_ms"), a.tz),
        "clickhouse_version": meta["server"]["version"],
        "database": meta["database"],
        "logs": table(&meta["logs"]),
        "traces": table(&meta["traces"]),
        "metrics": if meta["metrics"].is_null() { Value::Null } else { json!({ "table": meta["metrics"]["table"] }) },
        "limits": {
            "max_range": fmt_duration(u64_of(&meta["limits"], "max_range_ms") as i64),
            "max_rows_per_page": meta["limits"]["max_rows"],
            "max_trace_spans": meta["limits"]["max_trace_spans"],
            "query_timeout_ms": meta["limits"]["query_timeout_ms"],
        },
    });
    if let Some(n) = meta["metrics_note"].as_str() {
        out["metrics_note"] = json!(n);
    }
    Ok(out)
}

async fn list_services(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let (from, to) = a.window("1h")?;
    let limit = a.limit("limit", 200, 2000)?;
    let mut qs = Qs::new();
    qs.window((from, to)).push("field", "service").push("limit", limit.to_string());
    let body = mcp.get("/api/traces/values", &qs.finish()).await?;
    let services: Vec<Value> = arr(&body["values"])
        .iter()
        .map(|v| json!({ "service": v["value"], "span_count": v["count"] }))
        .collect();
    Ok(json!({
        "from": fmt_time(from, a.tz),
        "to": fmt_time(to, a.tz),
        "count": services.len(),
        "services": services,
    }))
}

/// 和 `ui/src/lib/health.ts` 同一套阈值。
const MIN_SAMPLES: u64 = 300;

fn health(
    requests: u64,
    errors: u64,
    error_rate: f64,
    p95: f64,
    prev: Option<&Value>,
) -> &'static str {
    if error_rate >= 0.05 {
        return "red";
    }
    let mut level = if error_rate >= 0.01 { 1 } else { 0 };
    if let Some(p) = prev {
        let prev_requests = u64_of(p, "requests");
        let prev_p95 = f64_of(p, "p95_ms");
        if requests >= MIN_SAMPLES && prev_requests >= MIN_SAMPLES && p95 >= 200.0 && prev_p95 > 0.0
        {
            let r = p95 / prev_p95;
            if r >= 3.0 {
                return "red";
            }
            if r >= 1.5 {
                level = level.max(1);
            }
        }
        if u64_of(p, "errors") == 0 && errors > 0 {
            level = level.max(1);
        }
    }
    if level == 1 { "yellow" } else { "ok" }
}

fn health_rank(h: &str) -> u8 {
    match h {
        "red" => 0,
        "yellow" => 1,
        _ => 2,
    }
}

async fn service_overview(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let window = a.window("1h")?;
    let compare = a.string("compare")?.unwrap_or_else(|| "day".to_owned());
    let only_unhealthy = a.boolean("only_unhealthy")?.unwrap_or(false);
    let limit = a.limit("limit", 50, 1000)? as usize;
    let mut qs = Qs::new();
    qs.window(window).push("compare", &compare);
    let body = mcp.get("/api/services", &qs.finish()).await?;

    let mut rows: Vec<(u8, u64, Value)> = arr(&body["services"])
        .iter()
        .map(|s| {
            let requests = u64_of(s, "requests");
            let errors = u64_of(s, "errors");
            let error_rate = f64_of(s, "error_rate");
            let p95 = f64_of(s, "p95_ms");
            let prev = s.get("prev").filter(|p| !p.is_null());
            let h = health(requests, errors, error_rate, p95, prev);
            let mut row = json!({
                "service": s["service"],
                "health": h,
                "requests": requests,
                "errors": errors,
                "error_rate_pct": pct(error_rate),
                "rps": round(f64_of(s, "rps"), 2),
                "p50_ms": round(f64_of(s, "p50_ms"), 1),
                "p95_ms": round(p95, 1),
                "p99_ms": round(f64_of(s, "p99_ms"), 1),
            });
            if let Some(p) = prev {
                row["vs_prev"] = json!({
                    "requests": u64_of(p, "requests"),
                    "requests_change_pct": change_pct(requests as f64, u64_of(p, "requests") as f64),
                    "errors": u64_of(p, "errors"),
                    "error_rate_pct": pct(f64_of(p, "error_rate")),
                    "p95_ms": round(f64_of(p, "p95_ms"), 1),
                    "p95_ratio": ratio(p95, f64_of(p, "p95_ms")),
                });
            } else {
                row["vs_prev"] = Value::Null;
            }
            (health_rank(h), requests, row)
        })
        .filter(|(rank, _, _)| !only_unhealthy || *rank < 2)
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    let total = rows.len();
    let unhealthy = rows.iter().filter(|(rank, _, _)| *rank < 2).count();
    let services: Vec<Value> = rows.into_iter().take(limit).map(|(_, _, v)| v).collect();
    let mut out = json!({
        "from": fmt_time(window.0, a.tz),
        "to": fmt_time(window.1, a.tz),
        "compare": body["compare"],
        "prev_from": fmt_time(i64_of(&body, "prev_from_ms"), a.tz),
        "prev_to": fmt_time(i64_of(&body, "prev_to_ms"), a.tz),
        "service_count": total,
        "unhealthy_count": unhealthy,
        "services": services,
    });
    if total > limit {
        note(
            out.as_object_mut().expect("json 对象"),
            format!(
                "共 {total} 个服务，只返回了前 {limit} 个；可以加 limit 或 only_unhealthy=true"
            ),
        );
    }
    if total == 0 {
        note(
            out.as_object_mut().expect("json 对象"),
            "这段时间没有入口 span。如果你要找的是 JVM / GC / 堆内存 / CPU 这类上报指标，             它们不在这张表里，用 list_metrics 找名字、query_metric 查曲线"
                .to_owned(),
        );
    }
    Ok(out)
}

fn shape_operation(op: &Value, compare_on: bool) -> (Value, f64, f64) {
    let requests = u64_of(op, "requests");
    let errors = u64_of(op, "errors");
    let p95 = f64_of(op, "p95_ms");
    let prev = op.get("prev").filter(|p| !p.is_null());
    let mut row = json!({
        "span_name": op["span_name"],
        "kind": op["kind"],
        "requests": requests,
        "errors": errors,
        "error_rate_pct": pct(f64_of(op, "error_rate")),
        "rps": round(f64_of(op, "rps"), 2),
        "p50_ms": round(f64_of(op, "p50_ms"), 1),
        "p95_ms": round(p95, 1),
        "p99_ms": round(f64_of(op, "p99_ms"), 1),
    });
    if op.get("service").is_some() {
        row["service"] = op["service"].clone();
    }
    let (mut p95_change, mut errors_change) = (0.0, 0.0);
    match prev {
        Some(p) => {
            let prev_requests = u64_of(p, "requests");
            let prev_errors = u64_of(p, "errors");
            let prev_p95 = f64_of(p, "p95_ms");
            p95_change = if requests >= 100 && prev_requests >= 100 {
                (p95 - prev_p95).max(0.0) * requests as f64
            } else {
                0.0
            };
            errors_change = errors as f64 - prev_errors as f64;
            row["vs_prev"] = json!({
                "requests": prev_requests,
                "requests_change_pct": change_pct(requests as f64, prev_requests as f64),
                "errors": prev_errors,
                "error_rate_pct": pct(f64_of(p, "error_rate")),
                "p95_ms": round(prev_p95, 1),
                "p95_ratio": ratio(p95, prev_p95),
            });
            if requests == 0 {
                row["status"] = json!("gone");
            }
        }
        None if compare_on => {
            row["status"] = json!("new");
        }
        None => {}
    }
    (row, p95_change, errors_change)
}

async fn service_operations(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let services = a.list("service")?;
    if services.is_empty() {
        return Err("缺少参数 service".to_owned());
    }
    let window = a.window("1h")?;
    let compare = a.string("compare")?.unwrap_or_else(|| "day".to_owned());
    let sort = a.string("sort")?.unwrap_or_else(|| "requests".to_owned());
    let limit = a.limit("limit", 50, 2000)? as usize;
    let mut qs = Qs::new();
    qs.window(window).push("compare", &compare).push_opt("kind", a.string("kind")?);
    // 多个服务走 /api/services/operations（一次查完），单个还是走原来的路径
    let (path, query) = if let [only] = services.as_slice() {
        (format!("/api/services/{}/operations", encode_segment(only)), qs.finish())
    } else {
        qs.push_all("service", &services);
        ("/api/services/operations".to_owned(), qs.finish())
    };
    let body = mcp.get(&path, &query).await?;
    let compare_on = body["compare"].as_str() != Some("none");
    let mut rows: Vec<(Value, f64, f64)> =
        arr(&body["operations"]).iter().map(|op| shape_operation(op, compare_on)).collect();
    let key = |r: &(Value, f64, f64)| -> f64 {
        match sort.as_str() {
            "errors" => u64_of(&r.0, "errors") as f64,
            "p95" => f64_of(&r.0, "p95_ms"),
            "p95_change" => r.1,
            "errors_change" => r.2,
            "requests" => u64_of(&r.0, "requests") as f64,
            _ => f64::NAN,
        }
    };
    if key(&(Value::Null, 0.0, 0.0)).is_nan()
        && !["requests", "errors", "p95", "p95_change", "errors_change"].contains(&sort.as_str())
    {
        return Err(format!(
            "sort 只能是 requests / errors / p95 / p95_change / errors_change，不是 {sort:?}"
        ));
    }
    rows.sort_by(|x, y| key(y).total_cmp(&key(x)));
    let total = rows.len();
    let operations: Vec<Value> = rows.into_iter().take(limit).map(|(v, _, _)| v).collect();
    let mut out = json!({
        "service": body["service"],
        "kind": body["kind"],
        "from": fmt_time(window.0, a.tz),
        "to": fmt_time(window.1, a.tz),
        "compare": body["compare"],
        "sorted_by": sort,
        "operation_count": total,
        "operations": operations,
    });
    if let Some(p) = body["prev_from_ms"].as_i64() {
        out["prev_from"] = json!(fmt_time(p, a.tz));
        out["prev_to"] = json!(fmt_time(i64_of(&body, "prev_to_ms"), a.tz));
    }
    if total > limit {
        note(
            out.as_object_mut().expect("json 对象"),
            format!("共 {total} 个接口，只返回了前 {limit} 个"),
        );
    }
    // 服务端也封了顶（每个服务 200 个接口），这一批是量最大的那些，不是全部
    if body["truncated"].as_bool() == Some(true) {
        note(
            out.as_object_mut().expect("json 对象"),
            "有服务的接口数超过了服务端上限，只统计了量最大的那些；接口名里拼了 SQL / id 的服务会这样".to_owned(),
        );
    }
    Ok(out)
}

async fn service_timeseries(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let service = a.required("service")?;
    let window = a.window("1h")?;
    let mut qs = Qs::new();
    qs.window(window)
        .push("compare", a.string("compare")?.unwrap_or_else(|| "day".to_owned()))
        .push_opt("span_name", a.string("span_name")?);
    let path = format!("/api/services/{}/timeseries", encode_segment(&service));
    let body = mcp.get(&path, &qs.finish()).await?;
    let points: Vec<Value> = arr(&body["points"])
        .iter()
        .map(|p| {
            let mut row = json!({
                "time": fmt_time(i64_of(p, "t_ms"), a.tz),
                "requests": u64_of(p, "requests"),
                "errors": u64_of(p, "errors"),
                "p50_ms": round(f64_of(p, "p50_ms"), 1),
                "p95_ms": round(f64_of(p, "p95_ms"), 1),
                "p99_ms": round(f64_of(p, "p99_ms"), 1),
            });
            if let Some(prev) = p.get("prev").filter(|v| !v.is_null()) {
                row["prev_requests"] = json!(u64_of(prev, "requests"));
                row["prev_errors"] = json!(u64_of(prev, "errors"));
                row["prev_p95_ms"] = json!(round(f64_of(prev, "p95_ms"), 1));
            }
            row
        })
        .collect();
    let mut out = json!({
        "service": body["service"],
        "from": fmt_time(window.0, a.tz),
        "to": fmt_time(window.1, a.tz),
        "step": fmt_duration(u64_of(&body, "width_ms") as i64),
        "compare": body["compare"],
        "points": points,
    });
    if let Some(n) = body["span_name"].as_str() {
        out["span_name"] = json!(n);
    }
    Ok(out)
}

async fn error_groups(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let window = a.window("1h")?;
    let limit = a.limit("limit", 30, 200)? as usize;
    let mut qs = Qs::new();
    qs.window(window)
        .push_opt("service", a.string("service")?)
        .push_opt("span_name", a.string("span_name")?)
        .push_opt("kind", a.string("kind")?);
    let body = mcp.get("/api/errors", &qs.finish()).await?;
    let all = arr(&body["groups"]);
    let groups: Vec<Value> = all
        .iter()
        .take(limit)
        .map(|g| {
            let mut row = Map::new();
            for key in
                ["service", "span_name", "span_kind", "exception", "message", "http_status", "peer"]
            {
                let v = str_of(g, key);
                if !v.is_empty() {
                    row.insert(key.into(), json!(v));
                }
            }
            row.insert("count".into(), g["count"].clone());
            row.insert("traces".into(), g["traces"].clone());
            row.insert("first".into(), json!(fmt_time(i64_of(g, "first_ms"), a.tz)));
            row.insert("last".into(), json!(fmt_time(i64_of(g, "last_ms"), a.tz)));
            row.insert("sample_trace".into(), g["sample_trace"].clone());
            row.insert("sample_span".into(), g["sample_span"].clone());
            // 样本链路的时刻，get_trace 的 at 直接用
            row.insert("sample_at".into(), json!(fmt_time(i64_of(g, "last_ms"), a.tz)));
            Value::Object(row)
        })
        .collect();
    let mut out = json!({
        "from": fmt_time(window.0, a.tz),
        "to": fmt_time(window.1, a.tz),
        "kind": body["kind"],
        "total_error_spans": body["total"],
        "group_count": all.len(),
        "groups": groups,
    });
    if all.len() > limit {
        note(
            out.as_object_mut().expect("json 对象"),
            format!("共 {} 种报错，只返回了次数最多的 {limit} 种", all.len()),
        );
    }
    Ok(out)
}

fn shape_trace_summary(t: &Value, tz: Tz) -> Value {
    let mut row = json!({
        "trace_id": t["trace_id"],
        "start": fmt_time(i64_of(t, "start_us") / 1000, tz),
        "duration_ms": round(f64_of(t, "duration_ns") / 1e6, 2),
        "span_count": t["span_count"],
        "error_count": t["error_count"],
        "root_service": t["root_service"],
        "root_name": t["root_name"],
        "services": t["services"],
    });
    let span_ms = f64_of(t, "span_ns") / 1e6;
    if span_ms > f64_of(t, "duration_ns") / 1e6 * 1.5 {
        row["span_ms"] = json!(round(span_ms, 2));
    }
    if t["root_missing"].as_bool() == Some(true) {
        row["root_missing"] = json!(true);
    }
    row
}

async fn search_traces(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let window = a.window("1h")?;
    let limit = a.limit("limit", 20, 100)?;
    let mut qs = Qs::new();
    qs.window(window)
        .push_opt("service", a.string("service")?)
        .push_opt("span_name", a.string("span_name")?)
        .push_all("kind", &a.list("kind")?)
        .push_opt("error_only", a.boolean("error_only")?.map(|b| b.to_string()))
        .push_opt("min_ms", a.f64("min_ms")?.map(|v| v.to_string()))
        .push_opt("max_ms", a.f64("max_ms")?.map(|v| v.to_string()))
        .push_all("attr", &a.list("attr")?)
        .push_all("rattr", &a.list("rattr")?)
        .push_dims(&a.filters()?)
        .push_opt("sort", a.string("sort")?)
        .push("limit", limit.to_string());
    let body = mcp.get("/api/traces/search", &qs.finish()).await?;
    let traces: Vec<Value> =
        arr(&body["traces"]).iter().map(|t| shape_trace_summary(t, a.tz)).collect();
    Ok(json!({
        "from": fmt_time(window.0, a.tz),
        "to": fmt_time(window.1, a.tz),
        "sort": body["sort"],
        "count": traces.len(),
        "traces": traces,
    }))
}

fn is_error_status(s: &str) -> bool {
    s.eq_ignore_ascii_case("error") || s.eq_ignore_ascii_case("status_code_error")
}

/// 把一条 trace 的 span 整理成带层级的列表；超过 `max_spans` 时保留全部出错的 + 最慢的。
fn shape_spans(spans: &[Value], max_spans: usize, tz: Tz) -> (Vec<Value>, Value) {
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| i64_of(&spans[i], "start_us"));
    let by_id: HashMap<&str, usize> =
        spans.iter().enumerate().map(|(i, s)| (s["span_id"].as_str().unwrap_or(""), i)).collect();
    // 父 → 子（按开始时间排好的）
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for &i in &order {
        let parent = spans[i]["parent_span_id"].as_str().unwrap_or("");
        match by_id.get(parent) {
            Some(&p) if !parent.is_empty() && p != i => children.entry(p).or_default().push(i),
            _ => roots.push(i),
        }
    }
    let mut depth = vec![0usize; spans.len()];
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|&r| (r, 0)).collect();
    let mut seen = vec![false; spans.len()];
    while let Some((i, d)) = stack.pop() {
        if seen[i] {
            continue;
        }
        seen[i] = true;
        depth[i] = d;
        if let Some(kids) = children.get(&i) {
            for &k in kids.iter().rev() {
                stack.push((k, d + 1));
            }
        }
    }
    let t0 = order.first().map(|&i| i64_of(&spans[i], "start_us")).unwrap_or(0);
    let t1 = spans
        .iter()
        .map(|s| i64_of(s, "start_us") + (f64_of(s, "duration_ns") / 1000.0) as i64)
        .max()
        .unwrap_or(t0);
    let mut services: BTreeSet<String> = BTreeSet::new();
    let mut errors = 0usize;
    for s in spans {
        services.insert(str_of(s, "service"));
        if is_error_status(&str_of(s, "status")) {
            errors += 1;
        }
    }

    // 挑要返回的：全部出错的 + 最慢的补满
    let keep: Vec<usize> = if spans.len() <= max_spans {
        order.clone()
    } else {
        let mut chosen: BTreeSet<usize> = spans
            .iter()
            .enumerate()
            .filter(|(_, s)| is_error_status(&str_of(s, "status")))
            .map(|(i, _)| i)
            .collect();
        let mut by_duration: Vec<usize> = (0..spans.len()).collect();
        by_duration.sort_by(|&x, &y| {
            f64_of(&spans[y], "duration_ns").total_cmp(&f64_of(&spans[x], "duration_ns"))
        });
        for i in by_duration {
            if chosen.len() >= max_spans {
                break;
            }
            chosen.insert(i);
        }
        order.iter().copied().filter(|i| chosen.contains(i)).collect()
    };
    let rows: Vec<Value> = keep
        .iter()
        .map(|&i| {
            let s = &spans[i];
            let mut row = json!({
                "span_id": s["span_id"],
                "depth": depth[i],
                "service": s["service"],
                "name": s["name"],
                "kind": s["kind"],
                "offset_ms": round((i64_of(s, "start_us") - t0) as f64 / 1000.0, 2),
                "duration_ms": round(f64_of(s, "duration_ns") / 1e6, 2),
            });
            let parent = str_of(s, "parent_span_id");
            if !parent.is_empty() {
                row["parent_span_id"] = json!(parent);
                if !by_id.contains_key(parent.as_str()) {
                    row["parent_missing"] = json!(true);
                }
            }
            let status = str_of(s, "status");
            if is_error_status(&status) {
                row["status"] = json!("ERROR");
                let msg = str_of(s, "status_message");
                if !msg.is_empty() {
                    row["status_message"] = json!(truncate(&msg, 500).0);
                }
            }
            row
        })
        .collect();
    let summary = json!({
        "start": fmt_time(t0 / 1000, tz),
        "end": fmt_time(t1 / 1000, tz),
        "span_ms": round((t1 - t0) as f64 / 1000.0, 2),
        "span_count": spans.len(),
        "error_count": errors,
        "services": services,
        "root_missing": roots.iter().all(|&r| !spans[r]["parent_span_id"].as_str().unwrap_or("").is_empty()) && !spans.is_empty(),
    });
    (rows, summary)
}

/// 路径段里的服务名 / id：`/` `?` `#` `%` 之类要转义，其余照旧。
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b':'
            | b'@'
            | b'!'
            | b'$'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'=' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

async fn get_trace(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let trace_id = a.required("trace_id")?.trim().to_ascii_lowercase();
    let at = a.time("at")?;
    let max_spans = a.limit("max_spans", 200, 5000)? as usize;
    let mut qs = Qs::new();
    qs.push_opt("at", at.map(|v| v.to_string()));
    let body = mcp.get(&format!("/api/traces/{}", encode_segment(&trace_id)), &qs.finish()).await?;
    let spans = arr(&body["spans"]);
    if spans.is_empty() {
        let mut msg = format!("没有找到 trace {trace_id} 的 span");
        if at.is_some() {
            msg.push_str("（at 附近的几档时间窗口和全部分区都查过了）");
        }
        msg.push_str("。可能：id 抄错、还没入库（几秒延迟）、或者已经过了 TTL");
        return Err(msg);
    }
    let (rows, summary) = shape_spans(spans, max_spans, a.tz);
    let mut out = Map::new();
    out.insert("trace_id".into(), json!(trace_id));
    if let Some(s) = summary.as_object() {
        for (k, v) in s {
            out.insert(k.clone(), v.clone());
        }
    }
    let returned = rows.len();
    out.insert("returned_spans".into(), json!(returned));
    out.insert("spans".into(), Value::Array(rows));
    if body["truncated"].as_bool() == Some(true) {
        note(
            &mut out,
            format!(
                "这条 trace 的 span 超过了服务端上限，只取到了一部分{}",
                if body["narrowed"].as_bool() == Some(true) {
                    "（退回了围着 at 的窄时间窗）"
                } else {
                    ""
                }
            ),
        );
    }
    if returned < spans.len() {
        note(
            &mut out,
            format!(
                "共 {} 个 span，只返回了 {returned} 个：全部出错的 + 最慢的。要更多加 max_spans",
                spans.len()
            ),
        );
    }
    if a.boolean("include_logs")?.unwrap_or(false) {
        let log_limit = a.limit("log_limit", 100, 1000)?;
        // 已经知道这条 trace 的时刻，日志按时间范围查（前后各放宽一小时）比只按 trace_id 走 bloom filter 便宜得多
        let t0 = spans.iter().map(|s| i64_of(s, "start_us")).min().unwrap_or(0) / 1000;
        let t1 = spans
            .iter()
            .map(|s| i64_of(s, "start_us") + (f64_of(s, "duration_ns") / 1000.0) as i64)
            .max()
            .unwrap_or(0)
            / 1000;
        let mut qs = Qs::new();
        qs.push("trace_id", &trace_id)
            .window(((t0 - 3_600_000).max(0), t1 + 3_600_000))
            .push("order", "asc")
            .push("count", "0")
            .push("limit", log_limit.to_string());
        match mcp.get("/api/logs/search", &qs.finish()).await {
            Ok(logs) => {
                let rows = shape_log_rows(&logs["rows"], a.tz, 1000);
                if rows.len() as u32 >= log_limit {
                    note(
                        &mut out,
                        format!(
                            "日志只取了前 {log_limit} 条（时间正序），要更多用 search_logs 带 trace_id 翻页"
                        ),
                    );
                }
                out.insert("logs".into(), Value::Array(rows));
            }
            Err(e) => note(&mut out, format!("取日志失败: {e}")),
        }
    }
    Ok(Value::Object(out))
}

async fn get_span(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let trace_id = a.required("trace_id")?.trim().to_ascii_lowercase();
    let span_id = a.required("span_id")?.trim().to_ascii_lowercase();
    let max_chars = a.limit("max_chars", 4000, 200_000)? as usize;
    let mut qs = Qs::new();
    qs.push_opt("at", a.time("at")?.map(|v| v.to_string()));
    let path =
        format!("/api/traces/{}/spans/{}", encode_segment(&trace_id), encode_segment(&span_id));
    let body = mcp.get(&path, &qs.finish()).await?;
    let events: Vec<Value> = arr(&body["events"])
        .iter()
        .map(|e| {
            json!({
                "time": fmt_time(i64_of(e, "ts_us") / 1000, a.tz),
                "name": e["name"],
                "attributes": truncate_strings(&e["attributes"], max_chars),
            })
        })
        .collect();
    let links: Vec<Value> = arr(&body["links"])
        .iter()
        .map(|l| json!({ "trace_id": l["trace_id"], "span_id": l["span_id"], "attributes": truncate_strings(&l["attributes"], max_chars) }))
        .collect();
    Ok(json!({
        "trace_id": trace_id,
        "span_id": span_id,
        "attributes": truncate_strings(&body["attributes"], max_chars),
        "resource": truncate_strings(&body["resource"], max_chars),
        "events": events,
        "links": links,
    }))
}

/// search_logs / log_histogram / log_facets 共用的筛选条件。
fn log_filter_qs(a: &Args<'_>, qs: &mut Qs) -> R<()> {
    qs.push_opt("q", a.string("q")?)
        .push_opt("regex", a.boolean("regex")?.map(|b| b.to_string()))
        .push_all("level", &a.list("level")?)
        .push_opt("logger", a.string("logger")?)
        .push_opt("thread", a.string("thread")?)
        .push_opt("host", a.string("host")?)
        .push_dims(&a.filters()?);
    Ok(())
}

async fn search_logs(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let trace_id = a.string("trace_id")?;
    let span_id = a.string("span_id")?;
    let by_id = trace_id.is_some() || span_id.is_some();
    // 按 id 查可以不带时间范围，但那是兜底不是常态：span_id 上没有索引，不带范围要扫满 30 天
    // （2026-09-21 实测 31.3 G 行 / 37.9 GiB / 8.8 s），工具说明里已经写明让调用方尽量给 range
    let window = if by_id { a.window_opt("1h")? } else { Some(a.window("1h")?) };
    let limit = a.limit("limit", 50, 200)?;
    let max_chars = a.limit("max_message_chars", 2000, 200_000)? as usize;
    // 默认不数总数。`/api/logs/search` 默认会并发一条 `count()`，而**没有关键字**时这条是整个
    // 请求里最贵的一步：行那条按排序键读够 limit 就停，count 那条要扫完整个时间范围（线上一
    // 小时窗 900 多万行）。日志页早就为此传 `count=0`，用直方图各桶之和顶（见 README
    // 「共 N 条不单独跑 count()」），这条路以前漏了。要总数就用 log_histogram，或显式 count=true
    let want_count = a.boolean("count")?.unwrap_or(false);
    let mut qs = Qs::new();
    if let Some(w) = window {
        qs.window(w);
    }
    log_filter_qs(a, &mut qs)?;
    qs.push_opt("trace_id", trace_id.as_deref())
        .push_opt("span_id", span_id.as_deref())
        .push_opt("order", a.string("order")?)
        .push("count", if want_count { "1" } else { "0" })
        .push("limit", limit.to_string())
        .push_opt("offset", a.u32("offset")?.map(|v| v.to_string()));
    let body = mcp.get("/api/logs/search", &qs.finish()).await?;
    let rows = shape_log_rows(&body["rows"], a.tz, max_chars);
    let mut out = Map::new();
    if let Some((from, to)) = window {
        out.insert("from".into(), json!(fmt_time(from, a.tz)));
        out.insert("to".into(), json!(fmt_time(to, a.tz)));
    }
    out.insert("order".into(), body["order"].clone());
    out.insert("offset".into(), body["offset"].clone());
    out.insert("returned".into(), json!(rows.len()));
    if let Some(total) = body["total"].as_u64() {
        out.insert("total".into(), json!(total));
    }
    if let Some(terms) = body["token_terms"].as_array().filter(|t| !t.is_empty()) {
        note(
            &mut out,
            format!(
                "这些词按整词匹配（走了索引），搜它们的一部分搜不到: {}",
                terms.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
            ),
        );
    }
    if rows.len() as u32 >= limit {
        note(
            &mut out,
            format!(
                "满了 {limit} 行，可能还有更多：加 offset 翻页，或缩小时间范围 / 加筛选{}",
                if want_count {
                    ""
                } else {
                    "。要知道一共多少条用 log_histogram（顺带给时间分布），或者 count=true"
                }
            ),
        );
    }
    out.insert("rows".into(), Value::Array(rows));
    Ok(Value::Object(out))
}

async fn log_histogram(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let window = a.window("1h")?;
    let mut qs = Qs::new();
    qs.window(window);
    log_filter_qs(a, &mut qs)?;
    let body = mcp.get("/api/logs/histogram", &qs.finish()).await?;
    let buckets: Vec<Value> = arr(&body["buckets"])
        .iter()
        .map(|b| {
            let mut row = Map::new();
            row.insert("time".into(), json!(fmt_time(i64_of(b, "t_ms"), a.tz)));
            row.insert("total".into(), b["total"].clone());
            if let Some(counts) = b["counts"].as_object() {
                for (level, n) in counts {
                    row.insert(level.clone(), n.clone());
                }
            }
            Value::Object(row)
        })
        .collect();
    Ok(json!({
        "from": fmt_time(window.0, a.tz),
        "to": fmt_time(window.1, a.tz),
        "step": fmt_duration(u64_of(&body, "width_ms") as i64),
        "total": body["total"],
        "levels": body["levels"],
        "buckets": buckets,
    }))
}

async fn log_facets(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let fields = a.list("fields")?;
    if fields.is_empty() {
        return Err("缺少参数 fields（要统计哪些列）".to_owned());
    }
    let window = a.window("1h")?;
    let limit = a.limit("limit", 20, 500)?;
    let mut qs = Qs::new();
    qs.window(window).push_all("field", &fields).push("limit", limit.to_string());
    log_filter_qs(a, &mut qs)?;
    let body = mcp.get("/api/logs/facets", &qs.finish()).await?;
    let mut facets = Map::new();
    for f in arr(&body["facets"]) {
        let values: Vec<Value> = arr(&f["values"])
            .iter()
            .map(|v| json!({ "value": v["value"], "count": v["count"] }))
            .collect();
        facets.insert(str_of(f, "field"), Value::Array(values));
    }
    Ok(json!({
        "from": fmt_time(window.0, a.tz),
        "to": fmt_time(window.1, a.tz),
        "note": "计数是近似的（Space-Saving 算法）",
        "facets": facets,
    }))
}

async fn log_context(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let host = a.required("host")?;
    let file = a.required("file")?;
    let ts = a.time("time")?.ok_or_else(|| "缺少参数 time".to_owned())?;
    let max_chars = a.limit("max_message_chars", 4000, 200_000)? as usize;
    let mut qs = Qs::new();
    qs.push("host", &host)
        .push("file", &file)
        .push("ts", ts.to_string())
        .push("before", a.limit("before", 30, 500)?.to_string())
        .push("after", a.limit("after", 30, 500)?.to_string());
    let body = mcp.get("/api/logs/context", &qs.finish()).await?;
    Ok(json!({
        "host": host,
        "file": file,
        "anchor": fmt_time(ts, a.tz),
        "window": fmt_duration(u64_of(&body, "window_ms") as i64),
        "before": shape_log_rows(&body["before"], a.tz, max_chars),
        "after": shape_log_rows(&body["after"], a.tz, max_chars),
    }))
}

/// span / 指标上有哪些属性名、某个属性名有哪些取值。两边的端点形状不一样（span 那边回
/// `keys` / `values`，指标那边回 `names`），统一成 `items: [{name, count}]` 再给模型。
async fn list_attrs(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let on = a.string("on")?.unwrap_or_else(|| "span".to_owned());
    let scope = a.string("scope")?.unwrap_or_else(|| "attributes".to_owned());
    let resource = match scope.as_str() {
        "attributes" | "attribute" | "span" => false,
        "resource" | "resource_attributes" => true,
        other => return Err(format!("scope 只能是 attributes 或 resource，不是 {other:?}")),
    };
    let key = a.string("key")?;
    let window = a.window("1h")?;
    let limit = a.limit("limit", if key.is_some() { 50 } else { 200 }, 1000)?;
    let mut qs = Qs::new();
    qs.window(window).push("limit", limit.to_string()).push_opt("key", key.clone());
    let path = match on.as_str() {
        "span" => {
            qs.push("scope", if resource { "resource" } else { "span" })
                .push_opt("service", a.string("service")?);
            if key.is_some() { "/api/traces/attr_values" } else { "/api/traces/attr_keys" }
        }
        "metric" => {
            qs.push("column", if resource { "resource_attributes" } else { "attributes" })
                .push_opt("metric", a.string("metric")?)
                .push_all("service", &a.list("service")?);
            if key.is_some() { "/api/metrics/label_values" } else { "/api/metrics/labels" }
        }
        other => return Err(format!("on 只能是 span 或 metric，不是 {other:?}")),
    };
    let body = mcp.get(path, &qs.finish()).await?;
    // keys（span 属性名）/ values（span 属性值）/ names（指标两种都叫这个）
    let rows = [&body["keys"], &body["values"], &body["names"]]
        .into_iter()
        .find(|v| v.is_array())
        .unwrap_or(&Value::Null);
    let items: Vec<Value> = arr(rows)
        .iter()
        .map(|r| {
            let name = [r.get("key"), r.get("value"), r.get("name")]
                .into_iter()
                .flatten()
                .find(|v| v.is_string())
                .cloned()
                .unwrap_or(Value::Null);
            json!({ "name": name, "count": r["count"] })
        })
        .collect();
    let mut out = json!({
        "on": on,
        "scope": if resource { "resource" } else { "attributes" },
        "from": fmt_time(window.0, a.tz),
        "to": fmt_time(window.1, a.tz),
        "count": items.len(),
        "items": items,
        "sampled": "count 是在最近若干行上采样统计的，看相对大小，别当准确条数",
    });
    if let Some(k) = key {
        out["key"] = json!(k);
    }
    Ok(out)
}

async fn list_metrics(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let window = a.window("1h")?;
    let needle = a.string("match")?.map(|s| s.to_ascii_lowercase());
    let limit = a.limit("limit", 200, 5000)? as usize;
    let mut qs = Qs::new();
    qs.window(window).push_all("service", &a.list("service")?);
    let body = mcp.get("/api/metrics", &qs.finish()).await?;
    let all: Vec<&Value> = arr(&body["metrics"])
        .iter()
        .filter(|m| {
            needle.as_ref().is_none_or(|n| str_of(m, "name").to_ascii_lowercase().contains(n))
        })
        .collect();
    let metrics: Vec<Value> = all
        .iter()
        .take(limit)
        .map(|m| {
            let mut row = json!({ "name": m["name"], "type": m["type"], "points": m["points"] });
            for key in ["unit", "description", "temporality"] {
                let v = str_of(m, key);
                if !v.is_empty() && v != "Unspecified" {
                    row[key] = json!(v);
                }
            }
            if m["monotonic"].as_bool() == Some(true) {
                row["counter"] = json!(true);
            }
            let services = arr(&m["services"]);
            if services.len() > 8 {
                row["services"] = json!(services.iter().take(8).cloned().collect::<Vec<_>>());
                row["service_count"] = json!(services.len());
            } else {
                row["services"] = m["services"].clone();
            }
            row
        })
        .collect();
    let mut out = json!({
        "scanned_from": fmt_time(i64_of(&body, "from_ms"), a.tz),
        "scanned_to": fmt_time(i64_of(&body, "to_ms"), a.tz),
        "count": all.len(),
        "metrics": metrics,
    });
    if all.len() > limit {
        note(
            out.as_object_mut().expect("json 对象"),
            format!("共 {} 个指标，只返回了 {limit} 个；用 match 缩小", all.len()),
        );
    }
    Ok(out)
}

async fn query_metric(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let metric = a.required("metric")?;
    let window = a.window("1h")?;
    let limit = a.limit("limit", 10, 200)?;
    let mut qs = Qs::new();
    qs.window(window)
        .push("metric", &metric)
        .push_opt("agg", a.string("agg")?)
        .push_opt("field", a.string("field")?)
        .push_all("by", &a.list("by")?)
        .push_all("q", &a.list("q")?)
        .push_all("service", &a.list("service")?)
        .push_all("attr", &a.list("attr")?)
        .push_all("rattr", &a.list("rattr")?)
        .push_opt("step", a.u32("step")?.map(|v| v.to_string()))
        .push("limit", limit.to_string());
    let body = mcp.get("/api/metrics/query", &qs.finish()).await?;
    let times: Vec<String> =
        arr(&body["t_ms"]).iter().map(|t| fmt_time(t.as_i64().unwrap_or(0), a.tz)).collect();
    let series: Vec<Value> = arr(&body["series"])
        .iter()
        .map(|s| {
            let labels: BTreeMap<String, Value> =
                arr(&s["labels"]).iter().map(|l| (str_of(l, "key"), l["value"].clone())).collect();
            let points: Vec<Value> = arr(&s["values"])
                .iter()
                .zip(&times)
                .filter(|(v, _)| !v.is_null())
                .map(|(v, t)| json!([t, round(v.as_f64().unwrap_or(0.0), 4)]))
                .collect();
            let stat = |k: &str| s[k].as_f64().map(|v| round(v, 4));
            json!({
                "name": s["name"],
                "labels": labels,
                "min": stat("min"),
                "max": stat("max"),
                "avg": stat("avg"),
                "last": stat("last"),
                "points": points,
            })
        })
        .collect();
    let mut out = json!({
        "metric": body["metric"],
        // agg / field 可能是服务端按类型挑的，原样回给模型，它才知道自己看的是什么
        "metric_type": body["metric_type"],
        "agg": body["agg"],
        "field": body["field"],
        "by": body["by"],
        "from": fmt_time(window.0, a.tz),
        "to": fmt_time(window.1, a.tz),
        "step": fmt_duration(u64_of(&body, "width_ms") as i64),
        "series_count": series.len(),
        "series": series,
        "points_note": "points 是 [时间, 值]，没数据的桶省掉了",
    });
    if body["truncated"].as_bool() == Some(true) {
        note(
            out.as_object_mut().expect("json 对象"),
            format!("时间线超过 {limit} 条，只返回了量最大的那些；加 limit 或加过滤"),
        );
    }
    if let Some(n) = body["note"].as_str() {
        note(out.as_object_mut().expect("json 对象"), n.to_owned());
    }
    Ok(out)
}

/// 指标点上挂的 trace id：从「这个指标现在很高」直接跳到「是这几条链路慢」。
async fn metric_exemplars(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let metric = a.required("metric")?;
    let window = a.window("1h")?;
    let limit = a.limit("limit", 20, 500)?;
    let mut qs = Qs::new();
    qs.window(window)
        .push("metric", &metric)
        .push_all("service", &a.list("service")?)
        .push_all("attr", &a.list("attr")?)
        .push_all("rattr", &a.list("rattr")?)
        .push("limit", limit.to_string());
    let body = mcp.get("/api/metrics/exemplars", &qs.finish()).await?;
    // 端点已经按值从大到小排了：最慢 / 最大的那些请求排在前面
    let exemplars: Vec<Value> = arr(&body["exemplars"])
        .iter()
        .take(limit as usize)
        .map(|e| {
            json!({
                "time": fmt_time(i64_of(e, "t_ms"), a.tz),
                "value": round(f64_of(e, "value"), 4),
                "service": e["service"],
                "trace_id": e["trace_id"],
                "span_id": e["span_id"],
            })
        })
        .collect();
    let mut out = json!({
        "metric": body["metric"],
        "from": fmt_time(window.0, a.tz),
        "to": fmt_time(window.1, a.tz),
        "count": exemplars.len(),
        "exemplars": exemplars,
        "next": "拿 trace_id 调 get_trace（把这里的 time 当 at 传过去会快很多）",
    });
    if arr(&body["exemplars"]).is_empty() {
        note(
            out.as_object_mut().expect("json 对象"),
            "这个指标在这段时间没有 exemplar：埋点没开 exemplar，或者这个指标本来就不挂 trace（一般只有直方图有）。改用 query_metric 看曲线、再按时间去 search_traces".to_owned(),
        );
    }
    Ok(out)
}

async fn metric_events(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let metric = a.required("metric")?;
    let window = a.window("1h")?;
    let mut qs = Qs::new();
    qs.window(window)
        .push("metric", &metric)
        .push_all("service", &a.list("service")?)
        .push_opt("field", a.string("field")?);
    let body = mcp.get("/api/metrics/events", &qs.finish()).await?;
    let events: Vec<Value> = arr(&body["events"])
        .iter()
        .map(|e| json!({ "time": fmt_time(i64_of(e, "t_ms"), a.tz), "kind": e["kind"], "pod": e["pod"], "service": e["service"] }))
        .collect();
    Ok(json!({
        "metric": body["metric"],
        "from": fmt_time(window.0, a.tz),
        "to": fmt_time(window.1, a.tz),
        "count": events.len(),
        "events": events,
        "legend": { "restart": "累积 counter 掉回去了：进程原地重启", "start": "这个 pod 在窗口里第一次出现：发布 / 扩容 / 重建" },
    }))
}

// ---------------------------------------------------------------------------------------------
// 云账单（goscan）
// ---------------------------------------------------------------------------------------------

/// 当前账期（按 `--timezone`）。账单工具的时间参数是账期，不走 [`Args::window`] 那一套。
fn current_period(a: &Args<'_>) -> String {
    chrono::DateTime::from_timestamp_millis(a.now_ms)
        .unwrap_or_else(chrono::Utc::now)
        .with_timezone(&a.tz)
        .format("%Y-%m")
        .to_string()
}

/// `(from, to)` 两个账期。没给 from 就从 to 往前数 `months` 个（含 to 自己）。
fn bill_range(a: &Args<'_>) -> R<(String, String)> {
    let to = match a.string("to")? {
        Some(v) => crate::query::bills::check_period(&v).map_err(|e| e.user_message())?,
        None => current_period(a),
    };
    let from = match a.string("from")? {
        Some(v) => crate::query::bills::check_period(&v).map_err(|e| e.user_message())?,
        None => {
            let months =
                a.limit("months", crate::api::bills::DEFAULT_RANGE_MONTHS as u32, 36)? as i32;
            crate::query::bills::shift_period(&to, -(months - 1)).map_err(|e| e.user_message())?
        }
    };
    Ok((from, to))
}

/// 账期 + 金额口径 + 云 + 维度筛选，三个费用工具共用的查询串。
fn bill_qs(a: &Args<'_>) -> R<Qs> {
    let (from, to) = bill_range(a)?;
    let mut qs = Qs::new();
    qs.push("from", &from)
        .push("to", &to)
        .push_opt("amount", a.string("amount")?)
        .push_all("provider", &a.list("provider")?)
        .push_dims(&a.filters()?)
        .push_opt("q", a.string("q")?);
    Ok(qs)
}

/// 各云的分摊平铺进同一个对象里：`{"volcengine": 1.2, "alicloud": 3.4}` 比嵌一层
/// `by_provider` 省 token，模型读起来也直接。
fn flatten_providers(target: &mut Map<String, Value>, by_provider: &Value) {
    let Some(obj) = by_provider.as_object() else { return };
    for (provider, amount) in obj {
        if amount.as_f64().unwrap_or(0.0) != 0.0 {
            target.insert(provider.clone(), json!(round(amount.as_f64().unwrap_or(0.0), 4)));
        }
    }
}

/// 花了多少钱：按账期（默认）或者按天。
async fn cost_summary(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let by_day = matches!(a.string("granularity")?.as_deref(), Some("day"));
    let qs = bill_qs(a)?;
    let path = if by_day { "/api/bills/daily" } else { "/api/bills/summary" };
    let body = mcp.get(path, &qs.finish()).await?;
    let points: Vec<Value> = arr(&body["points"])
        .iter()
        .filter(|p| !by_day || f64_of(p, "total") != 0.0)
        .map(|p| {
            let mut row = Map::new();
            row.insert("t".into(), p["t"].clone());
            row.insert("total".into(), json!(round(f64_of(p, "total"), 2)));
            flatten_providers(&mut row, &p["by_provider"]);
            Value::Object(row)
        })
        .collect();
    let mut by_provider = Map::new();
    flatten_providers(&mut by_provider, &body["by_provider"]);
    let mut out = json!({
        "from": body["from"],
        "to": body["to"],
        "amount": body["amount"],
        "granularity": if by_day { "day" } else { "month" },
        "total": round(f64_of(&body, "total"), 2),
        "points": points,
    });
    if !by_provider.is_empty() {
        out["by_provider"] = Value::Object(by_provider);
    }
    if let Some(providers) = body["providers"].as_array()
        && providers.len() == 1
    {
        note(
            out.as_object_mut().expect("json 对象"),
            format!(
                "这段数字只有 {} 一朵云：另一朵云在这个部署里没有账单表（或者没同步日度账单）",
                providers[0].as_str().unwrap_or("")
            ),
        );
    }
    Ok(out)
}

/// 钱花在哪：按产品 / 地域 / 账号 / 实例排行。
async fn cost_breakdown(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let limit = a.limit("limit", 20, 200)?;
    let mut qs = bill_qs(a)?;
    qs.push_opt("by", a.string("by")?).push("limit", limit.to_string());
    let body = mcp.get("/api/bills/breakdown", &qs.finish()).await?;
    let rows: Vec<Value> = arr(&body["rows"])
        .iter()
        .map(|r| {
            let mut row = Map::new();
            row.insert("key".into(), r["key"].clone());
            row.insert("amount".into(), json!(round(f64_of(r, "amount"), 2)));
            row.insert("share_pct".into(), json!(pct(f64_of(r, "share"))));
            flatten_providers(&mut row, &r["by_provider"]);
            Value::Object(row)
        })
        .collect();
    Ok(json!({
        "by": body["by"],
        "from": body["from"],
        "to": body["to"],
        "amount": body["amount"],
        "total": round(f64_of(&body, "total"), 2),
        "other": round(f64_of(&body, "other"), 2),
        "rows": rows,
        "other_note": "other = 总额减去上面这些，也就是没进排行的那些加起来",
    }))
}

/// 明细：一行一个计费项。空字段直接省掉，别拿一堆空串占模型的上下文。
async fn cost_detail(mcp: &Mcp, a: &Args<'_>) -> R<Value> {
    let limit = a.limit("limit", 30, crate::query::bills::MAX_DETAIL_ROWS)?;
    let mut qs = bill_qs(a)?;
    qs.push_opt("provider", a.string("provider")?)
        .push_opt("granularity", a.string("granularity")?)
        .push("limit", limit.to_string())
        .push_opt("offset", a.u32("offset")?.map(|v| v.to_string()));
    let body = mcp.get("/api/bills/detail", &qs.finish()).await?;
    let rows: Vec<Value> = arr(&body["rows"])
        .iter()
        .map(|r| {
            let mut row = Map::new();
            for key in [
                "period",
                "day",
                "product",
                "item",
                "instance",
                "instance_id",
                "region",
                "account",
                "project",
                "subscription",
                "usage",
                "usage_unit",
                "currency",
            ] {
                let v = str_of(r, key);
                if !v.is_empty() {
                    row.insert(key.into(), json!(v));
                }
            }
            row.insert("amount".into(), json!(round(f64_of(r, "amount"), 4)));
            // 打过折的才把原价带上：原价和应付一样时多一列没意义
            let original = round(f64_of(r, "original"), 4);
            if original != round(f64_of(r, "amount"), 4) && original != 0.0 {
                row.insert("original".into(), json!(original));
            }
            Value::Object(row)
        })
        .collect();
    Ok(json!({
        "provider": body["provider"],
        "granularity": body["granularity"],
        "from": body["from"],
        "to": body["to"],
        "amount": body["amount"],
        "total_rows": body["total"],
        "count": rows.len(),
        "rows": rows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    const NOW: i64 = 1_767_196_800_000;

    fn a(map: &Map<String, Value>) -> Args<'_> {
        Args { map, now_ms: NOW, tz: chrono_tz::Asia::Shanghai }
    }

    #[test]
    fn every_tool_has_an_object_schema() {
        let tools = list(true, true);
        assert!(tools.len() >= 18);
        let mut names = BTreeSet::new();
        for t in &tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{t}");
            assert!(t["description"].as_str().unwrap().len() > 20);
            assert!(names.insert(t["name"].as_str().unwrap().to_owned()), "重名: {}", t["name"]);
            // required 里的每个都得在 properties 里
            for r in arr(&t["inputSchema"]["required"]) {
                assert!(t["inputSchema"]["properties"].get(r.as_str().unwrap()).is_some(), "{t}");
            }
            // 只读标注：客户端靠它决定要不要拦一下问人。只留这一个，别的 hint 在只读工具上
            // 没有意义，而目录是每轮都要重发的
            assert_eq!(t["annotations"], json!({ "readOnlyHint": true }), "{t}");
            // 描述开头统一标数据来源，和 instructions 的术语表对上
            assert!(t["description"].as_str().unwrap().starts_with('['), "{t}");
        }
    }

    /// 工具目录在客户端那边是**每一轮**都要重发的（它在系统提示里），所以它的大小是按轮计费的。
    /// 这个上限是拿来挡「描述随手越写越长」的：真要加，先想想能不能把哪段挪进 instructions
    /// （那个只在握手时发一次）。
    ///
    /// 上限从 16k 提到 19k 是因为接了第四种数据源（goscan 的云账单，三个 cost_* 工具，约 3.4k
    /// 字符）——**这是给新数据源的额度，不是给形容词的**：没有账单表的部署一个 cost_* 都不会列。
    #[test]
    fn the_tool_catalog_stays_within_its_token_budget() {
        let json = serde_json::to_string(&list(true, true)).unwrap();
        let chars = json.chars().count();
        assert!(chars < 19_000, "工具目录 {chars} 字符，超预算了（约 {} token）", chars / 2);
        // 单个工具别写成小作文
        for t in list(true, true) {
            let n = t["description"].as_str().unwrap().chars().count();
            assert!(n < 500, "{} 的描述 {n} 字符，太长了", t["name"]);
        }
    }

    #[test]
    fn metric_and_bill_tools_disappear_without_their_tables() {
        let names = |metrics, bills| {
            list(metrics, bills)
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_owned())
                .collect::<BTreeSet<_>>()
        };
        let (with, without) = (names(true, true), names(false, true));
        assert!(with.contains("query_metric") && with.contains("metric_exemplars"));
        assert!(!without.contains("query_metric"), "{without:?}");
        assert_eq!(
            with.difference(&without).cloned().collect::<BTreeSet<_>>(),
            METRIC_TOOLS.iter().map(|s| (*s).to_owned()).collect::<BTreeSet<_>>(),
        );
        // 没部署 goscan 的环境同理：费用工具整组消失，别的一个不少
        let no_bills = names(true, false);
        assert!(with.contains("cost_summary") && with.contains("cost_detail"));
        assert_eq!(
            with.difference(&no_bills).cloned().collect::<BTreeSet<_>>(),
            BILL_TOOLS.iter().map(|s| (*s).to_owned()).collect::<BTreeSet<_>>(),
        );
        // 目录里的每个工具都得有对应的分支，否则调用时会莫名其妙地「没有这个工具」
        assert!(with.contains("list_attrs"));
    }

    #[test]
    fn unknown_argument_names_are_rejected_not_ignored() {
        let def = all_tools().iter().find(|t| t["name"] == "service_operations").unwrap();
        let ok = args(json!({ "service": "order", "range": "15m" }));
        assert!(check_arg_names(def, &ok).is_ok());
        let typo = args(json!({ "service_name": "order" }));
        let err = check_arg_names(def, &typo).unwrap_err();
        assert!(err.contains("service_name"), "{err}");
        assert!(err.contains("service"), "把认识的参数名列出来: {err}");
    }

    #[test]
    fn oversized_results_get_trimmed_and_say_so() {
        let row = |i: usize| json!({ "message": "x".repeat(2000), "i": i });
        let big = json!({ "count": 200, "logs": (0..200).map(row).collect::<Vec<_>>() });
        let out = fit_budget(big, 64 << 10);
        let logs = out["logs"].as_array().unwrap();
        assert!(logs.len() < 200 && !logs.is_empty(), "砍了一半又一半: {}", logs.len());
        assert!(serde_json::to_string(&out).unwrap().len() <= (64 << 10) + 512);
        let note = out["notes"][0].as_str().unwrap();
        assert!(note.contains("logs") && note.contains("200"), "{note}");

        // 没超预算的原样返回，不带 notes
        let small = json!({ "logs": [row(1)] });
        assert!(fit_budget(small.clone(), 64 << 10).get("notes").is_none());

        // 砍光数组也压不下去时截长文本
        let one = json!({ "attributes": { "stack": "y".repeat(200_000) } });
        let out = fit_budget(one, 8 << 10);
        assert!(serde_json::to_string(&out).unwrap().len() <= 8 << 10);
        assert!(out["notes"][0].as_str().unwrap().contains("长文本"));
    }

    #[test]
    fn window_defaults_and_relative_forms() {
        let m = args(json!({}));
        assert_eq!(a(&m).window("1h").unwrap(), (NOW - 3_600_000, NOW));
        let m = args(json!({ "range": "15m" }));
        assert_eq!(a(&m).window("1h").unwrap(), (NOW - 900_000, NOW));
        let m = args(json!({ "from": "now-2h", "to": "now-1h" }));
        assert_eq!(a(&m).window("1h").unwrap(), (NOW - 7_200_000, NOW - 3_600_000));
        let m = args(json!({ "from": "now", "to": "now-1h" }));
        assert!(a(&m).window("1h").is_err());
        let m = args(json!({}));
        assert_eq!(a(&m).window_opt("1h").unwrap(), None);
    }

    #[test]
    fn lists_and_filters_are_lenient_about_shape() {
        let m = args(json!({
            "level": "ERROR, WARN", "kind": ["Server", 1],
            "filters": { "pod": ["a", "b"], "service_name": "x", "namespace": null },
            "regex": "true", "limit": "7",
        }));
        let a = a(&m);
        assert_eq!(a.list("level").unwrap(), ["ERROR", "WARN"]);
        assert_eq!(a.list("kind").unwrap(), ["Server", "1"]);
        assert_eq!(
            a.filters().unwrap(),
            vec![
                ("pod".to_owned(), vec!["a".to_owned(), "b".to_owned()]),
                ("service_name".to_owned(), vec!["x".to_owned()])
            ]
        );
        assert_eq!(a.boolean("regex").unwrap(), Some(true));
        assert_eq!(a.limit("limit", 50, 5).unwrap(), 5);
        let bad = args(json!({ "filters": { "limit": "9" } }));
        assert!(
            super::Args { map: &bad, now_ms: NOW, tz: chrono_tz::UTC }.filters().is_err(),
            "控制参数不能当列名"
        );
    }

    #[test]
    fn query_string_repeats_keys() {
        let mut qs = Qs::new();
        qs.window((1, 2))
            .push_all("attr", &["a=1".to_owned(), "b=中".to_owned()])
            .push_opt("x", None::<&str>);
        assert_eq!(qs.finish(), "from=1&to=2&attr=a%3D1&attr=b%3D%E4%B8%AD");
        assert_eq!(encode_segment("GET /x?y"), "GET%20%2Fx%3Fy");
    }

    #[test]
    fn spans_get_depth_and_the_worst_survive_truncation() {
        let span = |id: &str, parent: &str, start: i64, dur_ms: f64, status: &str| {
            json!({ "span_id": id, "parent_span_id": parent, "service": "svc", "name": id, "kind": "Server",
                    "start_us": start, "duration_ns": dur_ms * 1e6, "status": status, "status_message": "" })
        };
        let spans = vec![
            span("root", "", 1_000_000, 100.0, "Unset"),
            span("child", "root", 1_010_000, 50.0, "Unset"),
            span("grandchild", "child", 1_020_000, 5.0, "Error"),
            span("orphan", "nope", 1_030_000, 1.0, "Unset"),
        ];
        let (rows, summary) = shape_spans(&spans, 10, chrono_tz::Asia::Shanghai);
        let depth: Vec<(String, u64)> =
            rows.iter().map(|r| (str_of(r, "span_id"), r["depth"].as_u64().unwrap())).collect();
        assert_eq!(
            depth,
            [
                ("root".to_owned(), 0),
                ("child".to_owned(), 1),
                ("grandchild".to_owned(), 2),
                ("orphan".to_owned(), 0)
            ]
        );
        assert_eq!(rows[3]["parent_missing"], true);
        assert_eq!(rows[2]["status"], "ERROR");
        assert!(rows[0].get("status").is_none(), "没出错的不带 status");
        assert_eq!(rows[1]["offset_ms"], 10.0);
        assert_eq!(summary["span_count"], 4);
        assert_eq!(summary["error_count"], 1);
        assert_eq!(summary["root_missing"], false);

        // 只留 2 个：出错的那个一定在，另一个是最慢的 root
        let (rows, _) = shape_spans(&spans, 2, chrono_tz::Asia::Shanghai);
        let ids: Vec<String> = rows.iter().map(|r| str_of(r, "span_id")).collect();
        assert_eq!(ids, ["root", "grandchild"]);
    }

    #[test]
    fn log_rows_drop_empties_and_truncate() {
        let row = json!({ "ts_ms": NOW, "level": "ERROR", "trace_id": "", "span_id": "abc", "thread": "main",
                          "logger": "L", "message": "0123456789", "file": "/a.log", "host": "h", "pod": "p-1" });
        let out = shape_log_row(&row, chrono_tz::Asia::Shanghai, 4);
        assert_eq!(out["time"], "2026-01-01T00:00:00.000+08:00");
        assert!(out.get("trace_id").is_none());
        assert_eq!(out["span_id"], "abc");
        assert_eq!(out["pod"], "p-1");
        assert_eq!(out["message"], "0123…[共 10 字符，已截断]");
        assert_eq!(out["message_len"], 10);
        assert!(out.get("ts_ms").is_none());
    }

    #[test]
    fn health_follows_the_ui_thresholds() {
        let prev = json!({ "requests": 1000, "errors": 0, "p95_ms": 100.0 });
        assert_eq!(health(1000, 60, 0.06, 100.0, Some(&prev)), "red");
        assert_eq!(health(1000, 15, 0.015, 100.0, Some(&prev)), "yellow");
        assert_eq!(health(1000, 0, 0.0, 350.0, Some(&prev)), "red", "P95 涨到 3.5 倍");
        assert_eq!(health(1000, 0, 0.0, 180.0, Some(&prev)), "ok", "P95 没到 200ms 不算");
        assert_eq!(
            health(
                1000,
                0,
                0.0,
                160.0,
                Some(&json!({ "requests": 1000, "errors": 0, "p95_ms": 100.0 }))
            ),
            "ok"
        );
        assert_eq!(health(1000, 0, 0.0, 250.0, Some(&prev)), "yellow", "2.5 倍是黄");
        assert_eq!(health(50, 0, 0.0, 900.0, Some(&prev)), "ok", "样本不够不比 P95");
        assert_eq!(health(1000, 1, 0.001, 100.0, Some(&prev)), "yellow", "从零错误变成有错误");
        assert_eq!(health(1000, 0, 0.0, 900.0, None), "ok", "没有对比时段只看错误率");
    }
}
