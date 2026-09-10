//! 指标表（metricpipe 的 `otel_metric`）的 SQL。
//!
//! 排序键是 `(service_name, metric_name, toDateTime(timestamp))`，分片键
//! `cityHash64(service_name, metric_name)`：面板查的永远是「某个指标（可能限定服务）最近一段
//! 时间」，正好走前缀；不给服务时靠 `metric_name` 上的 bloom filter 跳 granule，所以指标页
//! 不像链路页那样强制先选服务。
//!
//! 五种指标类型在同一张表里，`metric_type` 区分，用不上的列留默认值。所以取值列要按类型选：
//! Gauge / Sum 看 `value`，Histogram / Summary 看 `count` / `sum` / `bucket_counts`。
//!
//! ## 累积量怎么算速率
//!
//! `temporality = 'Cumulative'` 的 Sum（也就是 counter）存的是进程启动以来的累计值，画出来是
//! 一条上升的锯齿；要的是每秒增量，只能**查询时相减**（当初就是为了不在采集端转 delta 才这么
//! 存的：多副本路由下 delta 化很难做对）。相减必须**按时间线分组**做，而且这里的时间线是
//! `service_name + scope + resource_attributes + attributes` 全部——同一个服务的两个 pod 报的是
//! 两条独立的计数器，混在一起相减会得到一堆负数（然后被当成重启）。表上没有 series_id 列，
//! 只能现算 `cityHash64(...)`，代价是要把两个 JSON 属性列整列读出来；查询已经锁定了一个
//! `metric_name`，读的行数有限，可以接受。
//!
//! 相邻两点的差用 `lagInFrame`：`cur < prev` 视为进程重启（计数器归零），按 Prometheus 的做法
//! 把当前值整个算成增量。`Delta` 的 Sum 不用相减，桶内求和就是增量。两种 temporality 在同一条
//! SQL 里用 `if(temp = 'Cumulative', ...)` 分开，不用先查一次表才知道是哪种。
//!
//! 直方图分位数同理：`bucket_counts` 也是累积的，先按时间线逐桶相减，再把同组的桶计数逐元素
//! 相加（`sumForEach`），最后在 Rust 里从桶计数和 `explicit_bounds` 插值出分位数
//! （见 [`quantile_from_histogram`]）——分位数不能对多条时间线取平均，只能合并原始桶。

use serde::{Deserialize, Serialize};

use super::traces::{AttrFilter, attr_path};
use super::{Bindings, Bucket, TimeRange, quote_ident};
use crate::clickhouse::{Query, num};
use crate::error::{Error, Result};
use crate::schema::Table;

/// `metric_type` 列的五种取值。
pub const METRIC_TYPES: &[&str] = &["Gauge", "Sum", "Histogram", "ExponentialHistogram", "Summary"];

/// 可以直接按名字分组的固定列。不在这个表里的名字一律当数据点属性（`attributes.x`）解释——
/// 标签名撞上这几个列名的情况没见过，真撞了写 `attr:` 前缀。
pub const GROUP_COLUMNS: &[&str] =
    &["service_name", "scope_name", "scope_version", "temporality", "metric_unit"];

/// 属性键 / 值的下拉只看这么多行：`JSONAllPaths` 要读整个属性列，全范围扫太贵。
pub const ATTR_SAMPLE_ROWS: u32 = 20_000;

/// 一次最多返回多少条时间线，超了截断并在响应里标记。
pub const MAX_SERIES: u32 = 100;

/// 分组维度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupKey {
    /// 表上的固定列，如 `service_name`
    Column(String),
    /// 数据点属性 `attributes.<key>`
    Attr(String),
    /// resource 属性 `resource_attributes.<key>`
    Resource(String),
}

impl GroupKey {
    /// `service_name` → 列；`res:k8s.pod.name` → resource 属性；其余 → 数据点属性。
    /// `attr:` 前缀可以强制按属性解释。
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if let Some(key) = raw.strip_prefix("res:") {
            return Ok(GroupKey::Resource(check_key(key)?));
        }
        if let Some(key) = raw.strip_prefix("attr:") {
            return Ok(GroupKey::Attr(check_key(key)?));
        }
        if GROUP_COLUMNS.contains(&raw) {
            return Ok(GroupKey::Column(raw.to_owned()));
        }
        Ok(GroupKey::Attr(check_key(raw)?))
    }

    /// 回给前端的名字，和 `parse` 吃的是同一种写法。
    pub fn label(&self) -> String {
        match self {
            GroupKey::Column(c) => c.clone(),
            GroupKey::Attr(k) => k.clone(),
            GroupKey::Resource(k) => format!("res:{k}"),
        }
    }

    /// 取值表达式，统一转成字符串（属性里同一个 key 可能有整数也有字符串）。
    fn expr(&self) -> Result<String> {
        Ok(match self {
            GroupKey::Column(c) => format!("toString({})", quote_ident(c)?),
            GroupKey::Attr(k) => format!("toString({})", attr_path("attributes", k)?),
            GroupKey::Resource(k) => format!("toString({})", attr_path("resource_attributes", k)?),
        })
    }
}

/// 分组维度里的属性名。真正拼进 SQL 的那道关在 [`attr_path`]，这里提前挡一遍，
/// 好让「不合法的分组维度」在解析参数时就报出来，而不是等到拼 SQL。
fn check_key(key: &str) -> Result<String> {
    let key = key.trim();
    if key.is_empty()
        || key.len() > 256
        || key.chars().any(|c| c == '`' || c == '\\' || c.is_control())
    {
        return Err(Error::bad_request(format!("属性名不合法: {key:?}")));
    }
    Ok(key.to_owned())
}

/// 取哪一列的值。Gauge / Sum 只有 `value`；Histogram / Summary 的是 `count` / `sum`（
/// `min` / `max` 只有 Histogram 有，而且没上报时是 0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Value,
    Count,
    Sum,
    Min,
    Max,
}

impl Field {
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.unwrap_or("value") {
            "value" => Ok(Field::Value),
            "count" => Ok(Field::Count),
            "sum" => Ok(Field::Sum),
            "min" => Ok(Field::Min),
            "max" => Ok(Field::Max),
            other => Err(Error::bad_request(format!(
                "field 只能是 value / count / sum / min / max，不是 {other:?}"
            ))),
        }
    }

    /// 一律转成 Float64：`count` 是 UInt64，和别的列混在一个表达式里会报类型不匹配。
    fn expr(self) -> &'static str {
        match self {
            Field::Value => "toFloat64(value)",
            Field::Count => "toFloat64(count)",
            Field::Sum => "toFloat64(sum)",
            Field::Min => "toFloat64(min)",
            Field::Max => "toFloat64(max)",
        }
    }
}

/// 桶内怎么聚。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Agg {
    Avg,
    Sum,
    Min,
    Max,
    /// 桶内最后一个点的值（gauge 看当前水位用）
    Last,
    /// 数据点条数
    Count,
    /// 每秒增量（counter）
    Rate,
    /// 桶内增量（counter）
    Increase,
    /// 增量的 sum ÷ 增量的 count：直方图的平均值
    Mean,
    /// 从直方图桶里插值出的分位数，走 [`MetricQueries::histogram`]
    Quantile,
}

impl Agg {
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        match raw.unwrap_or("avg") {
            "avg" => Ok(Agg::Avg),
            "sum" => Ok(Agg::Sum),
            "min" => Ok(Agg::Min),
            "max" => Ok(Agg::Max),
            "last" => Ok(Agg::Last),
            "count" => Ok(Agg::Count),
            "rate" => Ok(Agg::Rate),
            "increase" => Ok(Agg::Increase),
            "mean" => Ok(Agg::Mean),
            "quantile" | "p50" | "p90" | "p95" | "p99" => Ok(Agg::Quantile),
            other => Err(Error::bad_request(format!(
                "agg 只能是 avg / sum / min / max / last / count / rate / increase / mean / quantile，不是 {other:?}"
            ))),
        }
    }

    /// 要不要按时间线相减（也就是要不要算 `series`，那是整个查询里最贵的一步）。
    pub fn needs_series(self) -> bool {
        matches!(self, Agg::Rate | Agg::Increase | Agg::Mean | Agg::Quantile)
    }
}

/// 一次指标查询的筛选条件。
#[derive(Debug, Clone)]
pub struct MetricFilter {
    pub range: TimeRange,
    pub metric: String,
    /// 空 = 不限
    pub services: Vec<String>,
    pub attrs: Vec<AttrFilter>,
    pub resource_attrs: Vec<AttrFilter>,
}

impl MetricFilter {
    pub fn new(range: TimeRange, metric: &str) -> Result<Self> {
        let metric = metric.trim();
        if metric.is_empty() || metric.len() > 256 {
            return Err(Error::bad_request("必须指定一个指标名"));
        }
        Ok(Self {
            range,
            metric: metric.to_owned(),
            services: Vec::new(),
            attrs: Vec::new(),
            resource_attrs: Vec::new(),
        })
    }

    fn where_sql(&self, b: &mut Bindings) -> Result<String> {
        let mut parts = vec![
            b.time_predicate("timestamp", &self.range),
            format!("metric_name = {}", b.bind("String", &self.metric)),
        ];
        if !self.services.is_empty() {
            parts.push(format!("service_name IN {}", b.bind("Array(String)", &self.services)));
        }
        for f in &self.attrs {
            parts.push(f.sql("attributes", b)?);
        }
        for f in &self.resource_attrs {
            parts.push(f.sql("resource_attributes", b)?);
        }
        Ok(parts.join("\n    AND "))
    }
}

/// 一条时间线在某个桶上的取值。`v` 为空表示这个桶没数据（累积量的第一个桶没有前值）。
#[derive(Debug, Deserialize)]
pub struct SeriesRow {
    #[serde(deserialize_with = "num::de")]
    pub bucket: i64,
    /// 各分组维度的取值，顺序和请求里的 `by` 一致
    pub keys: Vec<String>,
    #[serde(default, deserialize_with = "num::de_opt")]
    pub v: Option<f64>,
}

/// 直方图：某个桶上合并后的桶计数和上界。
#[derive(Debug, Deserialize)]
pub struct HistogramRow {
    #[serde(deserialize_with = "num::de")]
    pub bucket: i64,
    pub keys: Vec<String>,
    #[serde(deserialize_with = "num::de_vec")]
    pub counts: Vec<f64>,
    #[serde(deserialize_with = "num::de_vec")]
    pub bounds: Vec<f64>,
}

#[derive(Debug, Deserialize)]
pub struct CatalogRow {
    pub metric_name: String,
    pub metric_type: String,
    pub metric_unit: String,
    pub description: String,
    pub temporality: String,
    pub services: Vec<String>,
    #[serde(deserialize_with = "num::de")]
    pub is_monotonic: u8,
    #[serde(deserialize_with = "num::de")]
    pub points: u64,
}

#[derive(Debug, Deserialize)]
pub struct NameCountRow {
    pub name: String,
    #[serde(deserialize_with = "num::de")]
    pub count: u64,
}

#[derive(Debug, Deserialize)]
pub struct ExemplarRow {
    #[serde(deserialize_with = "num::de")]
    pub t_ms: i64,
    #[serde(default, deserialize_with = "num::de_opt")]
    pub value: Option<f64>,
    pub trace_id: String,
    pub span_id: String,
    pub service_name: String,
}

pub struct MetricQueries<'a> {
    pub database: &'a str,
    pub table: &'a Table,
}

impl MetricQueries<'_> {
    fn table_ref(&self) -> String {
        format!("`{}`.`{}`", self.database, self.table.name)
    }

    /// 分片键是 `cityHash64(service_name, metric_name)`：两个都定死时只打一个分片。
    fn finish(b: Bindings, sql: String) -> Query {
        b.into_query(sql).setting("optimize_skip_unused_shards", 1)
    }

    /// 时间线标识。见模块头：必须带上 resource 属性，不然同一服务的多个 pod 会被当成一条。
    fn series_expr() -> &'static str {
        "cityHash64(service_name, scope_name, toString(resource_attributes), toString(attributes))"
    }

    /// `[toString(a), toString(b)] AS keys`；没有分组维度时是空数组，这样结果行的形状不变。
    fn keys_expr(by: &[GroupKey]) -> Result<String> {
        if by.is_empty() {
            return Ok("emptyArrayString() AS keys".to_owned());
        }
        let parts: Vec<String> = by.iter().map(GroupKey::expr).collect::<Result<_>>()?;
        Ok(format!("[{}] AS keys", parts.join(", ")))
    }

    /// 有哪些指标。`metric_description` 只取一个代表值——同名指标不同服务的描述理应一样。
    /// `services` 取到 100 个：页面要靠它列出「有指标的服务」，截断了就会少几个服务。
    pub fn catalog(&self, range: &TimeRange, services: &[String], limit: u32) -> Result<Query> {
        let mut b = Bindings::new();
        let mut where_sql = b.time_predicate("timestamp", range);
        if !services.is_empty() {
            where_sql.push_str(&format!(
                "\n  AND service_name IN {}",
                b.bind("Array(String)", services)
            ));
        }
        let limit = b.bind("UInt32", limit);
        let sql = format!(
            "SELECT metric_name, metric_type, metric_unit,\n  \
             any(metric_description) AS description, any(temporality) AS temporality,\n  \
             arraySort(groupUniqArray(100)(service_name)) AS services,\n  \
             max(is_monotonic) AS is_monotonic, count() AS points\n\
             FROM {from}\nWHERE {where_sql}\n\
             GROUP BY metric_name, metric_type, metric_unit\n\
             ORDER BY metric_name, metric_type\nLIMIT {limit}",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 某个指标上有哪些标签名。`column` 是 `attributes` 或 `resource_attributes`。
    pub fn label_keys(&self, filter: &MetricFilter, column: &str, limit: u32) -> Result<Query> {
        let column = attr_column(column)?;
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        let sample = b.bind("UInt32", ATTR_SAMPLE_ROWS);
        let limit = b.bind("UInt32", limit);
        // LIMIT 限在源行上，不是展开之后的行上（和链路页那边同一个坑）
        let sql = format!(
            "SELECT name, count() AS count\nFROM (\n  \
             SELECT arrayJoin(JSONAllPaths({column})) AS name\n  FROM (\n    \
             SELECT {column}\n    FROM {from}\n    WHERE {where_sql}\n    LIMIT {sample}\n  )\n)\n\
             GROUP BY name\nORDER BY count DESC, name\nLIMIT {limit}",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 某个标签常见的取值。
    pub fn label_values(
        &self,
        filter: &MetricFilter,
        column: &str,
        key: &str,
        limit: u32,
    ) -> Result<Query> {
        let path = attr_path(attr_column(column)?, key)?;
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        let sample = b.bind("UInt32", ATTR_SAMPLE_ROWS);
        let limit = b.bind("UInt32", limit);
        let sql = format!(
            "SELECT name, count() AS count\nFROM (\n  \
             SELECT toString({path}) AS name\n  FROM {from}\n  WHERE {where_sql}\n  LIMIT {sample}\n)\n\
             WHERE name != ''\nGROUP BY name\nORDER BY count DESC, name\nLIMIT {limit}",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }

    /// 时间序列。`limit` 是时间线条数上限，多要一条用来判断有没有被截断。
    pub fn series(
        &self,
        filter: &MetricFilter,
        agg: Agg,
        field: Field,
        by: &[GroupKey],
        bucket: &Bucket,
        limit: u32,
    ) -> Result<Query> {
        if agg == Agg::Quantile {
            return Err(Error::bad_request("分位数走 histogram 查询"));
        }
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        let keys = Self::keys_expr(by)?;
        let origin = b.bind("Int64", bucket.origin_ms);
        let width = b.bind("Int64", bucket.width_ms);
        let bucket_expr =
            format!("intDiv(toUnixTimestamp64Milli(timestamp) - {origin}, {width}) AS bucket");
        let f = field.expr();
        let from = self.table_ref();

        let base = if agg.needs_series() {
            let secs = b.bind("Float64", bucket.width_ms as f64 / 1000.0);
            // 一条时间线在一个桶里的：最后一个累计值 cur、桶内求和 dsum（Delta 用）、
            // 最后一个点的时刻 t（算真实间隔用，桶里没点的时候不至于把速率摊平）
            let inner = format!(
                "SELECT {bucket_expr}, {keys}, {series} AS series,\n      \
                 argMax({f}, timestamp) AS cur, sum({f}) AS dsum,\n      \
                 max(toUnixTimestamp64Milli(timestamp)) AS t, any(temporality) AS temp{extra}\n    \
                 FROM {from}\n    WHERE {where_sql}\n    GROUP BY bucket, keys, series",
                series = Self::series_expr(),
                extra = match agg {
                    // 平均值要 sum 和 count 各自的增量，多带一列
                    Agg::Mean =>
                        ",\n      argMax(toFloat64(count), timestamp) AS cur_n, sum(toFloat64(count)) AS dsum_n",
                    _ => "",
                },
            );
            let lagged = format!(
                "SELECT bucket, keys, cur, dsum, t, temp,\n      \
                 lagInFrame(cur) OVER w AS prev, lagInFrame(t) OVER w AS prev_t{extra}\n    \
                 FROM (\n    {inner}\n    )\n    \
                 WINDOW w AS (PARTITION BY series, keys ORDER BY bucket ASC ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)",
                extra = match agg {
                    Agg::Mean => ",\n      cur_n, dsum_n, lagInFrame(cur_n) OVER w AS prev_n",
                    _ => "",
                },
            );
            // 累积量：cur < prev 视为进程重启，整个 cur 算成增量；第一个桶没有前值，记 null
            let delta = |cur: &str, prev: &str, dsum: &str| {
                format!(
                    "if(temp = 'Cumulative', if(prev_t = 0, NULL, if({cur} >= {prev}, {cur} - {prev}, {cur})), {dsum})"
                )
            };
            let inc = delta("cur", "prev", "dsum");
            match agg {
                // 速率按两个点的真实间隔算，不按桶宽：上报周期比桶宽长的时候（60s 上报、
                // 30s 一桶）才不会被摊平成一半
                Agg::Rate => format!(
                    "SELECT bucket, keys,\n    \
                     sum(if(temp = 'Cumulative',\n      \
                     if(prev_t = 0 OR t <= prev_t, NULL, if(cur >= prev, cur - prev, cur) * 1000 / (t - prev_t)),\n      \
                     dsum / {secs})) AS v\n  \
                     FROM (\n  {lagged}\n  )\n  GROUP BY bucket, keys"
                ),
                Agg::Increase => format!(
                    "SELECT bucket, keys, sum({inc}) AS v\n  FROM (\n  {lagged}\n  )\n  GROUP BY bucket, keys",
                ),
                Agg::Mean => format!(
                    "SELECT bucket, keys, sum({inc}) / nullIf(sum({inc_n}), 0) AS v\n  \
                     FROM (\n  {lagged}\n  )\n  GROUP BY bucket, keys",
                    inc_n = delta("cur_n", "prev_n", "dsum_n"),
                ),
                _ => unreachable!("needs_series 只有 rate / increase / mean / quantile"),
            }
        } else {
            let expr = match agg {
                Agg::Avg => format!("avg({f})"),
                Agg::Sum => format!("sum({f})"),
                Agg::Min => format!("min({f})"),
                Agg::Max => format!("max({f})"),
                Agg::Last => format!("argMax({f}, timestamp)"),
                Agg::Count => "toFloat64(count())".to_owned(),
                _ => unreachable!("needs_series 的分支在上面"),
            };
            format!(
                "SELECT {bucket_expr}, {keys}, {expr} AS v\n  FROM {from}\n  WHERE {where_sql}\n  GROUP BY bucket, keys"
            )
        };
        let sql = Self::top_series(&base, "abs(v)", "bucket, keys, v", &mut b, limit);
        Ok(Self::finish(b, sql))
    }

    /// 直方图：按时间线相减后合并桶计数，分位数在 Rust 里插值。
    pub fn histogram(
        &self,
        filter: &MetricFilter,
        by: &[GroupKey],
        bucket: &Bucket,
        limit: u32,
    ) -> Result<Query> {
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        let keys = Self::keys_expr(by)?;
        let origin = b.bind("Int64", bucket.origin_ms);
        let width = b.bind("Int64", bucket.width_ms);
        // 桶边界不一样的两条时间线合不到一起，所以 bounds 也进分组键：真出现了就是两组，
        // 而不是悄悄算错
        let inner = format!(
            "SELECT intDiv(toUnixTimestamp64Milli(timestamp) - {origin}, {width}) AS bucket,\n        \
             {keys}, {series} AS series, explicit_bounds AS bounds,\n        \
             argMax(bucket_counts, timestamp) AS cur, sumForEach(bucket_counts) AS dsum,\n        \
             any(temporality) AS temp\n      \
             FROM {from}\n      WHERE {where_sql}\n        AND notEmpty(bucket_counts)\n      \
             GROUP BY bucket, keys, series, bounds",
            series = Self::series_expr(),
            from = self.table_ref(),
        );
        // 累积量逐元素相减；第一个桶（前值是空数组）整条记 0，不能记 null——桶计数是数组，
        // 后面还要 sumForEach 相加。
        // `toUInt64(c - p)` 要套在**分支里面**：26.x 的 `UInt64 - UInt64` 出来是 Int64，
        // 和另一个分支的 UInt64 拼不出公共类型，`if` 会给一个 `Variant(Int64, UInt64)`，
        // 外层 sumForEach 直接报 43（线上撞过：Illegal type Variant(Array(UInt64),
        // Array(Variant(Int64, UInt64)))）。套在 if 外面也不行，toUInt64 不吃 Variant
        let base = format!(
            "SELECT bucket, keys, sumForEach(inc) AS counts, any(bounds) AS bounds\n  FROM (\n    \
             SELECT bucket, keys, bounds,\n      \
             if(temp = 'Cumulative',\n        \
             if(length(prev) = length(cur), arrayMap((c, p) -> if(c >= p, toUInt64(c - p), c), cur, prev), arrayMap(x -> toUInt64(0), cur)),\n        \
             dsum) AS inc\n    FROM (\n      \
             SELECT bucket, keys, bounds, cur, dsum, temp, lagInFrame(cur) OVER w AS prev\n      FROM (\n      {inner}\n      )\n      \
             WINDOW w AS (PARTITION BY series, keys, bounds ORDER BY bucket ASC ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)\n    )\n  )\n  \
             GROUP BY bucket, keys",
        );
        let sql = Self::top_series(
            &base,
            "arraySum(counts)",
            "bucket, keys, counts, bounds",
            &mut b,
            limit,
        );
        Ok(Self::finish(b, sql))
    }

    /// 只留「总量」最大的前 `limit` 条时间线。`dense_rank` 是在聚合之后的几千行上跑的，不贵；
    /// 换成两次往返（先查 top N 再查点）反而多一遍扫表。
    fn top_series(base: &str, weight: &str, cols: &str, b: &mut Bindings, limit: u32) -> String {
        let limit = b.bind("UInt32", limit);
        format!(
            "SELECT {cols}\nFROM (\n  SELECT {cols}, dense_rank() OVER (ORDER BY total DESC, keys ASC) AS rk\n  \
             FROM (\n  SELECT {cols}, sum({weight}) OVER (PARTITION BY keys) AS total\n  FROM (\n  {base}\n  )\n  )\n)\n\
             WHERE rk <= {limit}\nORDER BY keys, bucket"
        )
    }

    /// exemplar：指标上挂的 trace id，用来从一个尖峰直接跳到那次请求。按值从大到小取，
    /// 慢的那几次总是排在前面。
    pub fn exemplars(&self, filter: &MetricFilter, limit: u32) -> Result<Query> {
        let mut b = Bindings::new();
        let where_sql = filter.where_sql(&mut b)?;
        let limit = b.bind("UInt32", limit);
        let sql = format!(
            "SELECT toUnixTimestamp64Milli(`exemplars.timestamp`) AS t_ms, `exemplars.value` AS value,\n  \
             `exemplars.trace_id` AS trace_id, `exemplars.span_id` AS span_id, service_name\n\
             FROM (\n  \
             SELECT service_name, `exemplars.timestamp`, `exemplars.value`, `exemplars.trace_id`, `exemplars.span_id`\n  \
             FROM {from}\n  WHERE {where_sql}\n    AND notEmpty(`exemplars.trace_id`)\n)\n\
             ARRAY JOIN `exemplars.timestamp`, `exemplars.value`, `exemplars.trace_id`, `exemplars.span_id`\n\
             WHERE trace_id != ''\nORDER BY value DESC, t_ms DESC\nLIMIT {limit}",
            from = self.table_ref(),
        );
        Ok(Self::finish(b, sql))
    }
}

fn attr_column(name: &str) -> Result<&'static str> {
    match name {
        "attributes" => Ok("attributes"),
        "resource_attributes" => Ok("resource_attributes"),
        other => Err(Error::bad_request(format!(
            "属性列只能是 attributes 或 resource_attributes，不是 {other:?}"
        ))),
    }
}

/// 从直方图的桶计数里插值出分位数。
///
/// `bounds` 是 n 个上界，`counts` 是 n+1 个计数（最后一个是最大上界之外的）。落在某个桶里时
/// 在桶内线性插值——桶内的分布是未知的，这是 Prometheus 的 `histogram_quantile` 用的同一套
/// 近似。落在最后一个（无上界的）桶里只能返回最大上界，宁可低估也不编一个数出来。
pub fn quantile_from_histogram(counts: &[f64], bounds: &[f64], q: f64) -> Option<f64> {
    let total: f64 = counts.iter().sum();
    if total <= 0.0 || bounds.is_empty() {
        return None;
    }
    let target = total * q.clamp(0.0, 1.0);
    let mut seen = 0.0;
    for (i, c) in counts.iter().enumerate() {
        if seen + c < target {
            seen += c;
            continue;
        }
        // 溢出桶：只知道「比最大上界还大」
        let Some(upper) = bounds.get(i) else {
            return bounds.last().copied();
        };
        let lower = if i == 0 { 0.0_f64.min(*upper) } else { bounds[i - 1] };
        if *c <= 0.0 {
            return Some(*upper);
        }
        let frac = ((target - seen) / c).clamp(0.0, 1.0);
        return Some(lower + (upper - lower) * frac);
    }
    bounds.last().copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Column, ColumnKind, METRIC_FIXED_COLUMNS};

    fn table() -> Table {
        Table {
            name: "otel_metric".into(),
            columns: METRIC_FIXED_COLUMNS
                .iter()
                .map(|n| Column {
                    name: (*n).to_owned(),
                    ty: "String".into(),
                    kind: ColumnKind::String,
                })
                .collect(),
        }
    }

    fn queries(t: &Table) -> MetricQueries<'_> {
        MetricQueries { database: "logs", table: t }
    }

    fn filter() -> MetricFilter {
        MetricFilter::new(
            TimeRange { from_ms: 1_788_000_000_000, to_ms: 1_788_003_600_000 },
            "http.server.request.duration",
        )
        .unwrap()
    }

    fn bucket() -> Bucket {
        Bucket { width_ms: 60_000, origin_ms: 1_787_961_600_000 }
    }

    #[test]
    fn group_keys_parse_by_shape() {
        assert_eq!(
            GroupKey::parse("service_name").unwrap(),
            GroupKey::Column("service_name".into())
        );
        assert_eq!(GroupKey::parse("http.route").unwrap(), GroupKey::Attr("http.route".into()));
        assert_eq!(
            GroupKey::parse("res:k8s.pod.name").unwrap(),
            GroupKey::Resource("k8s.pod.name".into())
        );
        // 标签名撞上列名时可以强制按属性解释
        assert_eq!(
            GroupKey::parse("attr:service_name").unwrap(),
            GroupKey::Attr("service_name".into())
        );
        assert_eq!(GroupKey::parse("res:k8s.pod.name").unwrap().label(), "res:k8s.pod.name");
        assert!(GroupKey::parse("a`b").is_err());
    }

    #[test]
    fn raw_agg_does_not_compute_series() {
        let t = table();
        let q = queries(&t);
        let sql = q
            .series(
                &filter(),
                Agg::Avg,
                Field::Value,
                &[GroupKey::parse("service_name").unwrap()],
                &bucket(),
                21,
            )
            .unwrap();
        assert!(sql.sql().contains("avg(toFloat64(value))"), "{}", sql.sql());
        // 最贵的那一步只在算增量时才有
        assert!(!sql.sql().contains("cityHash64"), "{}", sql.sql());
        assert!(sql.sql().contains("[toString(`service_name`)] AS keys"), "{}", sql.sql());
        assert!(sql.sql().contains("dense_rank()"));
    }

    #[test]
    fn rate_diffs_per_series_and_handles_resets() {
        let t = table();
        let q = queries(&t);
        let sql = q.series(&filter(), Agg::Rate, Field::Value, &[], &bucket(), 21).unwrap();
        let text = sql.sql();
        // 时间线必须带上 resource 属性：同一服务的两个 pod 是两条计数器
        assert!(text.contains("toString(resource_attributes)"), "{text}");
        assert!(text.contains("PARTITION BY series, keys ORDER BY bucket"), "{text}");
        // 计数器归零
        assert!(text.contains("cur - prev, cur"), "{text}");
        // Delta 的走桶内求和，不相减
        assert!(text.contains("dsum / {p"), "{text}");
        assert!(text.contains("emptyArrayString() AS keys"), "{text}");
    }

    #[test]
    fn attr_filters_and_service_are_bound_not_interpolated() {
        let t = table();
        let q = queries(&t);
        let mut f = filter();
        f.services = vec!["checkout".into()];
        f.attrs = vec![AttrFilter::parse("http.route=/orders").unwrap()];
        f.resource_attrs = vec![AttrFilter::parse("k8s.namespace.name=prod").unwrap()];
        let sql = q.series(&f, Agg::Avg, Field::Value, &[], &bucket(), 21).unwrap();
        let text = sql.sql();
        assert!(text.contains("service_name IN {p"), "{text}");
        assert!(text.contains("toString(attributes.`http.route`) = {p"), "{text}");
        assert!(text.contains("toString(resource_attributes.`k8s.namespace.name`) = {p"), "{text}");
        assert!(sql.params().iter().any(|(_, v)| v == "/orders"));
        // 注入尝试进不了 SQL 文本
        let mut bad = filter();
        bad.attrs = vec![AttrFilter::parse("a`b=1").unwrap()];
        assert!(q.series(&bad, Agg::Avg, Field::Value, &[], &bucket(), 21).is_err());
    }

    #[test]
    fn histogram_merges_buckets_per_series() {
        let t = table();
        let q = queries(&t);
        let sql = q
            .histogram(&filter(), &[GroupKey::parse("http.route").unwrap()], &bucket(), 21)
            .unwrap();
        let text = sql.sql();
        assert!(text.contains("sumForEach(inc) AS counts"), "{text}");
        // 显式 toUInt64：不然 26.x 的 UInt64 减法出 Int64，if 拼成 Variant，sumForEach 报 43
        assert!(
            text.contains("arrayMap((c, p) -> if(c >= p, toUInt64(c - p), c), cur, prev)"),
            "{text}"
        );
        // 桶边界不同的时间线不能合并，进分组键
        assert!(text.contains("GROUP BY bucket, keys, series, bounds"), "{text}");
    }

    #[test]
    #[ignore = "手工看 SQL 用"]
    fn dump_sql() {
        let t = table();
        let q = queries(&t);
        let by = [GroupKey::parse("http.route").unwrap(), GroupKey::parse("service_name").unwrap()];
        println!(
            "--- rate ---\n{}",
            q.series(&filter(), Agg::Rate, Field::Value, &by, &bucket(), 21).unwrap().sql()
        );
        println!(
            "--- mean ---\n{}",
            q.series(&filter(), Agg::Mean, Field::Sum, &by, &bucket(), 21).unwrap().sql()
        );
        println!(
            "--- avg ---\n{}",
            q.series(&filter(), Agg::Avg, Field::Value, &[], &bucket(), 21).unwrap().sql()
        );
        println!("--- hist ---\n{}", q.histogram(&filter(), &by, &bucket(), 21).unwrap().sql());
        println!("--- catalog ---\n{}", q.catalog(&filter().range, &[], 500).unwrap().sql());
        println!("--- keys ---\n{}", q.label_keys(&filter(), "attributes", 100).unwrap().sql());
        println!("--- exemplars ---\n{}", q.exemplars(&filter(), 50).unwrap().sql());
    }

    #[test]
    fn quantile_interpolates_inside_the_bucket() {
        let bounds = [10.0, 20.0, 50.0];
        let counts = [10.0, 10.0, 10.0, 0.0];
        // 中位数落在第二个桶正中间
        assert_eq!(quantile_from_histogram(&counts, &bounds, 0.5), Some(15.0));
        assert_eq!(quantile_from_histogram(&counts, &bounds, 0.0), Some(0.0));
        // 全落在溢出桶里：只能给最大上界
        assert_eq!(quantile_from_histogram(&[0.0, 0.0, 0.0, 5.0], &bounds, 0.9), Some(50.0));
        assert_eq!(quantile_from_histogram(&[], &bounds, 0.5), None);
        assert_eq!(quantile_from_histogram(&[1.0], &[], 0.5), None);
    }

    #[test]
    fn exemplars_filter_before_the_array_join() {
        let t = table();
        let q = queries(&t);
        let text = q.exemplars(&filter(), 50).unwrap().sql().to_owned();
        let inner = text.find("notEmpty(`exemplars.trace_id`)").unwrap();
        let join = text.find("ARRAY JOIN").unwrap();
        assert!(inner < join, "先在源行上挡掉没有 exemplar 的行，再展开:\n{text}");
    }
}
