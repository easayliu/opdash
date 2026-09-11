//! `/api/metrics*`：指标目录、标签下拉、时间序列、exemplar。
//!
//! 接口不做 PromQL，参数是结构化的：`metric` + `agg` + `field` + `by` + 过滤。查询语义都在
//! [`crate::query::metrics`] 里，这里只负责解析参数、把行拼成前端要的「共用一条时间轴 + 每条
//! 时间线一个数组」的形状。
//!
//! 指标表是可选的：没部署 metricpipe 时这几个接口一律回 400 并说明原因，`/api/meta` 里
//! `metrics` 为 null，前端据此不显示指标页。

use std::collections::BTreeMap;

use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;

use super::{AppState, params::Params};
use crate::clickhouse::Stats;
use crate::error::{Error, Result};
use crate::query::metrics::{
    Agg, CatalogRow, EventRow, ExemplarRow, Field, GroupKey, HistogramRow, MAX_SERIES,
    MetricFilter, MetricQueries, NameCountRow, SeriesRow, quantile_from_histogram,
};
use crate::query::traces::AttrFilter;
use crate::query::{Bucket, TimeRange, parse_tz};
use crate::schema::Table;

/// 目录页扫的时间窗上限。目录要读整段范围里的 `metric_name` / `service_name`，31 天的窗口
/// 扫下来太贵，而「有哪些指标」看最近几小时就够——真有只在凌晨报一次的指标，把时间范围
/// 挪过去就看得见。
const CATALOG_MAX_RANGE_MS: i64 = 6 * 3_600_000;
/// 目录最多返回多少个指标。
const CATALOG_LIMIT: u32 = 2_000;
/// 自动步长时最多几个点。指标是采样数据，点太密反而全是空桶。
const MAX_BUCKETS: i64 = 60;
/// 手动步长时的桶数上限。
const MAX_MANUAL_BUCKETS: i64 = 1_000;
const MAX_EXEMPLARS: u32 = 500;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/metrics", get(catalog))
        .route("/api/metrics/labels", get(labels))
        .route("/api/metrics/label_values", get(label_values))
        .route("/api/metrics/query", get(query))
        .route("/api/metrics/exemplars", get(exemplars))
        .route("/api/metrics/events", get(events))
}

/// pod 启动的判定边距：首个样本比窗口起点晚这么多才算「新起的」，窗口一开始就在的老 pod
/// 首个样本会贴着起点。两分钟 > 任何合理的上报周期。
const START_MARGIN_MS: i64 = 2 * 60_000;
const MAX_EVENTS: u32 = 200;

/// 指标表在不在。不在就把原因原样告诉前端（表不存在 / 缺列 / 属性列不是 JSON）。
async fn metrics_table(state: &AppState) -> Result<std::sync::Arc<crate::schema::Schema>> {
    let schema = state.schema.get().await?;
    if schema.metrics.is_none() {
        let note = schema.metrics_note.clone().unwrap_or_else(|| "指标表不可用".to_owned());
        return Err(Error::bad_request(format!("指标页未启用：{note}")));
    }
    Ok(schema)
}

fn queries<'a>(state: &'a AppState, table: &'a Table) -> MetricQueries<'a> {
    MetricQueries { database: &state.config.database, table }
}

fn range(state: &AppState, p: &Params) -> Result<TimeRange> {
    TimeRange::new(p.get_i64("from")?, p.get_i64("to")?, state.now_ms(), state.config.max_range)
}

/// 时间桶：给了 `step`（秒）就用给的，否则按范围自动挑。
fn bucket(state: &AppState, range: &TimeRange, p: &Params) -> Result<Bucket> {
    let tz = parse_tz(&state.config.timezone)?;
    let Some(step) = p.get_u32("step")? else {
        return Ok(Bucket::choose(range, tz, MAX_BUCKETS));
    };
    if step == 0 {
        return Err(Error::bad_request("step 至少 1 秒"));
    }
    let width_ms = step as i64 * 1000;
    if range.span_ms() / width_ms > MAX_MANUAL_BUCKETS {
        return Err(Error::bad_request(format!(
            "步长 {step}s 在这个时间范围上要画 {} 个点，最多 {MAX_MANUAL_BUCKETS} 个：把步长调大或把范围缩小",
            range.span_ms() / width_ms
        )));
    }
    Ok(Bucket::with_width(range, tz, width_ms))
}

/// `metric` + 服务 + 属性过滤。
fn filter(range: TimeRange, p: &Params) -> Result<MetricFilter> {
    let mut f = MetricFilter::new(
        range,
        p.get("metric").ok_or_else(|| Error::bad_request("缺少 metric 参数"))?,
    )?;
    f.services = p.get_list("service");
    for raw in p.get_all("attr") {
        f.attrs.push(AttrFilter::parse(raw)?);
    }
    for raw in p.get_all("rattr") {
        f.resource_attrs.push(AttrFilter::parse(raw)?);
    }
    Ok(f)
}

#[derive(Serialize)]
pub struct MetricInfo {
    pub name: String,
    /// `Gauge` / `Sum` / `Histogram` / `ExponentialHistogram` / `Summary`
    #[serde(rename = "type")]
    pub ty: String,
    pub unit: String,
    pub description: String,
    /// `Delta` / `Cumulative` / `Unspecified`
    pub temporality: String,
    /// counter（只增不减）；up-down counter 是 false
    pub monotonic: bool,
    pub services: Vec<String>,
    pub points: u64,
}

#[derive(Serialize)]
pub struct CatalogResponse {
    /// 实际扫的窗口（可能比请求的范围窄，见 `CATALOG_MAX_RANGE_MS`）
    pub from_ms: i64,
    pub to_ms: i64,
    pub metrics: Vec<MetricInfo>,
    pub stats: Stats,
}

async fn catalog(State(state): State<AppState>, p: Params) -> Result<Json<CatalogResponse>> {
    let schema = metrics_table(&state).await?;
    let table = schema.metrics.as_ref().expect("metrics_table 已检查");
    let full = range(&state, &p)?;
    let scan = TimeRange {
        from_ms: full.from_ms.max(full.to_ms - CATALOG_MAX_RANGE_MS),
        to_ms: full.to_ms,
    };
    let services = p.get_list("service");
    let result = state
        .client
        .rows::<CatalogRow>(queries(&state, table).catalog(&scan, &services, CATALOG_LIMIT)?)
        .await?;
    let metrics = result
        .rows
        .into_iter()
        .map(|r| MetricInfo {
            name: r.metric_name,
            ty: r.metric_type,
            unit: r.metric_unit,
            description: r.description,
            temporality: r.temporality,
            monotonic: r.is_monotonic != 0,
            services: r.services,
            points: r.points,
        })
        .collect();
    Ok(Json(CatalogResponse {
        from_ms: scan.from_ms,
        to_ms: scan.to_ms,
        metrics,
        stats: result.stats,
    }))
}

#[derive(Serialize)]
pub struct NamesResponse {
    pub names: Vec<NameCount>,
    pub stats: Stats,
}

#[derive(Serialize)]
pub struct NameCount {
    pub name: String,
    pub count: u64,
}

fn names(rows: Vec<NameCountRow>) -> Vec<NameCount> {
    rows.into_iter().map(|r| NameCount { name: r.name, count: r.count }).collect()
}

/// 某个指标上有哪些标签。`column=resource_attributes` 看 resource 那一列。
async fn labels(State(state): State<AppState>, p: Params) -> Result<Json<NamesResponse>> {
    let schema = metrics_table(&state).await?;
    let table = schema.metrics.as_ref().expect("metrics_table 已检查");
    let f = filter(range(&state, &p)?, &p)?;
    let column = p.get("column").unwrap_or("attributes");
    let limit = p.get_limit("limit", 200, 1000)?;
    let result = state
        .client
        .rows::<NameCountRow>(queries(&state, table).label_keys(&f, column, limit)?)
        .await?;
    Ok(Json(NamesResponse { names: names(result.rows), stats: result.stats }))
}

async fn label_values(State(state): State<AppState>, p: Params) -> Result<Json<NamesResponse>> {
    let schema = metrics_table(&state).await?;
    let table = schema.metrics.as_ref().expect("metrics_table 已检查");
    let f = filter(range(&state, &p)?, &p)?;
    let column = p.get("column").unwrap_or("attributes");
    let key = p.get("key").ok_or_else(|| Error::bad_request("缺少 key 参数"))?;
    let limit = p.get_limit("limit", 200, 1000)?;
    let result = state
        .client
        .rows::<NameCountRow>(queries(&state, table).label_values(&f, column, key, limit)?)
        .await?;
    Ok(Json(NamesResponse { names: names(result.rows), stats: result.stats }))
}

#[derive(Serialize)]
pub struct Label {
    pub key: String,
    pub value: String,
}

#[derive(Serialize)]
pub struct SeriesOut {
    /// 分组维度取值，顺序和请求的 `by` 一致；`agg=quantile` 时末尾多一个 `quantile`
    pub labels: Vec<Label>,
    /// `k=v, k2=v2`；没有分组维度时是指标名
    pub name: String,
    /// 和 `t_ms` 等长，null = 这个桶没数据
    pub values: Vec<Option<f64>>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub avg: Option<f64>,
    pub last: Option<f64>,
}

#[derive(Serialize)]
pub struct QueryResponse {
    pub metric: String,
    pub agg: Agg,
    pub field: Field,
    pub by: Vec<String>,
    pub from_ms: i64,
    pub to_ms: i64,
    pub width_ms: i64,
    /// 每个桶的起点，所有时间线共用
    pub t_ms: Vec<i64>,
    pub series: Vec<SeriesOut>,
    /// 时间线超过 `limit` 条，只返回了最大的那些
    pub truncated: bool,
    pub stats: Stats,
}

/// 分组维度的取值 → 每个桶的值。按分组键收集，保持「一条时间线一行」。
struct Collector {
    by: Vec<String>,
    count: usize,
    first: i64,
    order: Vec<Vec<String>>,
    values: BTreeMap<Vec<String>, Vec<Option<f64>>>,
}

impl Collector {
    fn new(by: &[GroupKey], bucket: &Bucket, range: &TimeRange) -> Self {
        Self {
            by: by.iter().map(GroupKey::label).collect(),
            count: bucket.count(range).max(0) as usize,
            first: bucket.first_index(range),
            order: Vec::new(),
            values: BTreeMap::new(),
        }
    }

    /// 一行 = 一条时间线在一个桶上的取值。桶序号落在范围外的丢掉（自动步长下不会有）。
    fn put(&mut self, keys: Vec<String>, bucket: i64, value: Option<f64>) {
        let idx = bucket - self.first;
        if idx < 0 || idx as usize >= self.count {
            return;
        }
        let slot = self.values.entry(keys.clone()).or_insert_with(|| {
            self.order.push(keys);
            vec![None; self.count]
        });
        slot[idx as usize] = value;
    }

    fn len(&self) -> usize {
        self.order.len()
    }

    /// 按「有值的部分的绝对值之和」从大到小排：图例第一行就是最主要的那条线。
    fn finish(mut self, metric: &str, limit: usize) -> (Vec<SeriesOut>, bool) {
        let mut out: Vec<SeriesOut> = self
            .order
            .drain(..)
            .map(|keys| {
                let values = self.values.remove(&keys).unwrap_or_default();
                let labels: Vec<Label> = self
                    .by
                    .iter()
                    .zip(keys.iter())
                    .map(|(k, v)| Label { key: k.clone(), value: v.clone() })
                    .collect();
                let name = match labels.is_empty() {
                    true => metric.to_owned(),
                    false => labels
                        .iter()
                        .map(|l| {
                            format!("{}={}", l.key, if l.value.is_empty() { "-" } else { &l.value })
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                };
                let present: Vec<f64> = values.iter().flatten().copied().collect();
                let sum: f64 = present.iter().sum();
                SeriesOut {
                    min: present.iter().copied().reduce(f64::min),
                    max: present.iter().copied().reduce(f64::max),
                    avg: (!present.is_empty()).then(|| sum / present.len() as f64),
                    last: values.iter().rev().flatten().next().copied(),
                    labels,
                    name,
                    values,
                }
            })
            .collect();
        out.sort_by(|a, b| {
            let weight =
                |s: &SeriesOut| -> f64 { s.values.iter().flatten().map(|v| v.abs()).sum::<f64>() };
            weight(b).total_cmp(&weight(a)).then_with(|| a.name.cmp(&b.name))
        });
        let truncated = out.len() > limit;
        out.truncate(limit);
        (out, truncated)
    }
}

async fn query(State(state): State<AppState>, p: Params) -> Result<Json<QueryResponse>> {
    let schema = metrics_table(&state).await?;
    let table = schema.metrics.as_ref().expect("metrics_table 已检查");
    let range = range(&state, &p)?;
    let bucket = bucket(&state, &range, &p)?;
    let f = filter(range, &p)?;
    let agg = Agg::parse(p.get("agg"))?;
    let field = Field::parse(p.get("field"))?;
    let by: Vec<GroupKey> =
        p.get_list("by").iter().map(|k| GroupKey::parse(k)).collect::<Result<_>>()?;
    let limit = p.get_limit("limit", 20, MAX_SERIES)?;
    let q = queries(&state, table);
    let mut collector = Collector::new(&by, &bucket, &range);

    // 多要一条时间线，好知道是不是被截断了
    let stats = if agg == Agg::Quantile {
        let quantiles = quantiles(&p)?;
        let result =
            state.client.rows::<HistogramRow>(q.histogram(&f, &by, &bucket, limit + 1)?).await?;
        // 一个分组 × 一个分位数 = 一条线，分位数作为最后一个标签
        collector.by.push("quantile".to_owned());
        for row in result.rows {
            for (label, quantile) in &quantiles {
                let mut keys = row.keys.clone();
                keys.push(label.clone());
                let v = quantile_from_histogram(&row.counts, &row.bounds, *quantile);
                collector.put(keys, row.bucket, v);
            }
        }
        result.stats
    } else {
        let result = state
            .client
            .rows::<SeriesRow>(q.series(&f, agg, field, &by, &bucket, limit + 1)?)
            .await?;
        for row in result.rows {
            collector.put(row.keys, row.bucket, row.v);
        }
        result.stats
    };

    let first = bucket.first_index(&range);
    let t_ms: Vec<i64> = (0..collector.count as i64).map(|i| bucket.start_ms(first + i)).collect();
    let over = collector.len() > limit as usize;
    let (series, cut) = collector.finish(&f.metric, limit as usize);
    Ok(Json(QueryResponse {
        metric: f.metric,
        agg,
        field,
        by: by.iter().map(GroupKey::label).collect(),
        from_ms: range.from_ms,
        to_ms: range.to_ms,
        width_ms: bucket.width_ms,
        t_ms,
        series,
        truncated: over || cut,
        stats,
    }))
}

/// `q=0.5,0.95` → `[("p50", 0.5), ("p95", 0.95)]`。标签用 p50 这种写法，图例好读。
fn quantiles(p: &Params) -> Result<Vec<(String, f64)>> {
    let raw = p.get_list("q");
    let raw = if raw.is_empty() { vec!["0.95".to_owned()] } else { raw };
    if raw.len() > 5 {
        return Err(Error::bad_request("一次最多 5 个分位数"));
    }
    raw.iter()
        .map(|s| {
            let v: f64 = s
                .parse()
                .map_err(|_| Error::bad_request(format!("q 应为 0 ~ 1 的小数，不是 {s:?}")))?;
            if !(0.0..=1.0).contains(&v) {
                return Err(Error::bad_request(format!("q 应在 0 和 1 之间，不是 {v}")));
            }
            let label = match v * 100.0 {
                p if (p - p.round()).abs() < 1e-9 => format!("p{}", p.round() as i64),
                p => format!("p{p}"),
            };
            Ok((label, v))
        })
        .collect()
}

#[derive(Serialize)]
pub struct Exemplar {
    pub t_ms: i64,
    pub value: f64,
    pub trace_id: String,
    pub span_id: String,
    pub service: String,
}

#[derive(Serialize)]
pub struct ExemplarsResponse {
    pub metric: String,
    pub exemplars: Vec<Exemplar>,
    pub stats: Stats,
}

/// 指标上挂的 trace id：图上一个尖峰，点开就是那次请求。
async fn exemplars(State(state): State<AppState>, p: Params) -> Result<Json<ExemplarsResponse>> {
    let schema = metrics_table(&state).await?;
    let table = schema.metrics.as_ref().expect("metrics_table 已检查");
    let f = filter(range(&state, &p)?, &p)?;
    let limit = p.get_limit("limit", 100, MAX_EXEMPLARS)?;
    let result =
        state.client.rows::<ExemplarRow>(queries(&state, table).exemplars(&f, limit)?).await?;
    let exemplars = result
        .rows
        .into_iter()
        .map(|r| Exemplar {
            t_ms: r.t_ms,
            value: r.value.unwrap_or(0.0),
            trace_id: r.trace_id,
            span_id: r.span_id,
            service: r.service_name,
        })
        .collect();
    Ok(Json(ExemplarsResponse { metric: f.metric, exemplars, stats: result.stats }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantile_labels_are_readable() {
        let p = Params::parse(Some("q=0.5,0.95,0.999"));
        let q = quantiles(&p).unwrap();
        assert_eq!(q[0], ("p50".to_owned(), 0.5));
        assert_eq!(q[1], ("p95".to_owned(), 0.95));
        assert_eq!(q[2].0, "p99.9");
        // 没给就是 p95
        assert_eq!(quantiles(&Params::default()).unwrap(), [("p95".to_owned(), 0.95)]);
        assert!(quantiles(&Params::parse(Some("q=95"))).is_err());
        assert!(quantiles(&Params::parse(Some("q=abc"))).is_err());
    }

    #[test]
    fn collector_lays_points_on_the_shared_axis() {
        let range = TimeRange { from_ms: 1_000_000, to_ms: 1_600_000 };
        let bucket = Bucket { width_ms: 200_000, origin_ms: 0 };
        let by = [GroupKey::parse("service_name").unwrap()];
        let mut c = Collector::new(&by, &bucket, &range);
        let first = bucket.first_index(&range); // 5
        c.put(vec!["a".into()], first, Some(1.0));
        c.put(vec!["a".into()], first + 2, Some(3.0));
        c.put(vec!["b".into()], first + 1, Some(10.0));
        // 范围外的桶丢掉
        c.put(vec!["b".into()], first + 99, Some(99.0));
        assert_eq!(c.len(), 2);
        let (series, truncated) = c.finish("m", 10);
        assert!(!truncated);
        // 按量排序：b 的总量更大
        assert_eq!(series[0].name, "service_name=b");
        assert_eq!(series[0].values, [None, Some(10.0), None]);
        assert_eq!(series[1].values, [Some(1.0), None, Some(3.0)]);
        assert_eq!(series[1].last, Some(3.0));
        assert_eq!(series[1].avg, Some(2.0));
        assert_eq!(series[1].max, Some(3.0));
    }

    #[test]
    fn collector_truncates_to_the_biggest_series() {
        let range = TimeRange { from_ms: 0, to_ms: 200_000 };
        let bucket = Bucket { width_ms: 200_000, origin_ms: 0 };
        let mut c = Collector::new(&[GroupKey::parse("service_name").unwrap()], &bucket, &range);
        for (name, v) in [("a", 1.0), ("b", 5.0), ("c", 3.0)] {
            c.put(vec![name.into()], 0, Some(v));
        }
        let (series, truncated) = c.finish("m", 2);
        assert!(truncated);
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].name, "service_name=b");
        assert_eq!(series[1].name, "service_name=c");
    }
}

#[derive(Serialize)]
pub struct Event {
    pub t_ms: i64,
    /// `restart` = 累积 counter 掉回去了（原地重启）；`start` = 这个 pod 在窗口里第一次出现（新起 / 发布）
    pub kind: &'static str,
    pub pod: String,
    /// 不带 `service` 参数查全站时靠这个分到各个服务上
    pub service: String,
}

#[derive(Serialize)]
pub struct EventsResponse {
    pub metric: String,
    pub events: Vec<Event>,
    pub stats: Stats,
}

/// 进程重启 / pod 启动的时刻，标在这个服务所有图上；不带 `service` 就是全站的，服务总览页用。「14:02 延迟尖峰」和「14:01 重启」
/// 一眼对上，省掉排查里最常见的一步。`metric` 要给一个累积 counter（前端从目录里挑，
/// `jvm.cpu.time` 最合适）；`field` 默认 `value`，直方图给 `count`。
async fn events(State(state): State<AppState>, p: Params) -> Result<Json<EventsResponse>> {
    let schema = metrics_table(&state).await?;
    let table = schema.metrics.as_ref().expect("metrics_table 已检查");
    let f = filter(range(&state, &p)?, &p)?;
    let field = Field::parse(p.get("field"))?;
    let q = queries(&state, table);
    let (restarts, starts) = tokio::try_join!(
        state.client.rows::<EventRow>(q.restarts(&f, field, MAX_EVENTS)?),
        state.client.rows::<EventRow>(q.pod_starts(&f, START_MARGIN_MS, MAX_EVENTS)?),
    )?;
    let mut stats = restarts.stats;
    stats.absorb(&starts.stats);
    let mut events: Vec<Event> = restarts
        .rows
        .into_iter()
        .map(|r| Event { t_ms: r.t_ms, kind: "restart", pod: r.pod, service: r.service_name })
        .chain(starts.rows.into_iter().map(|r| Event {
            t_ms: r.t_ms,
            kind: "start",
            pod: r.pod,
            service: r.service_name,
        }))
        .collect();
    events.sort_by_key(|e| e.t_ms);
    Ok(Json(EventsResponse { metric: f.metric, events, stats }))
}
