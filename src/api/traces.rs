//! `/api/traces/*`：链路检索、详情、下拉取值、属性键值。

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{AppState, params::Params};
use crate::clickhouse::Stats;
use crate::error::{Error, Result};
use crate::query::traces::{
    AttrFilter, CANDIDATE_OVERFETCH, CLIENT_KINDS, CandidateRow, Candidates, ENTRY_KINDS,
    ErrorGroupRow, HeatmapRow, KeyRow, LocatedSpan, PROBE_WINDOWS_MS, SUMMARY_WIDEN_MS, Span,
    SpanEvent, SpanLink, SpanRow, SummaryRow, TraceFilter, TraceQueries, TraceSort, TraceSummary,
    ValueRow, candidate_range, dedup_by_trace, detail_forward_probes, detail_probe_hit,
    detail_probes, normalize_kind, normalize_span_id, normalize_trace_id,
};
use crate::query::{Bucket, TimeRange, parse_tz};
use crate::schema::{Schema, TRACE_FIXED_COLUMNS};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/traces/search", get(search))
        .route("/api/traces/heatmap", get(heatmap))
        .route("/api/errors", get(errors))
        .route("/api/traces/values", get(values))
        .route("/api/traces/attr_keys", get(attr_keys))
        .route("/api/traces/attr_values", get(attr_values))
        .route("/api/traces/{trace_id}", get(detail))
        .route("/api/traces/{trace_id}/spans/{span_id}", get(span_attrs))
}

const CONTROL_KEYS: &[&str] = &[
    "from",
    "to",
    "service",
    "span_name",
    "kind",
    "error_only",
    "min_ms",
    "max_ms",
    "attr",
    "rattr",
    "trace_id",
    "sort",
    "limit",
    "field",
    "scope",
    "key",
];

fn range(state: &AppState, p: &Params) -> Result<TimeRange> {
    TimeRange::new(p.get_i64("from")?, p.get_i64("to")?, state.now_ms(), state.config.max_range)
}

/// 动态列（静态 fields）筛选：不认识的键直接 400。
fn dims(schema: &Schema, p: &Params) -> Result<Vec<(String, Vec<String>)>> {
    let dim_columns: Vec<&str> = schema
        .traces
        .extra_string_columns(TRACE_FIXED_COLUMNS)
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    let mut dims = Vec::new();
    for key in p.keys() {
        if CONTROL_KEYS.contains(&key) {
            continue;
        }
        if !dim_columns.contains(&key) {
            return Err(Error::bad_request(format!(
                "不认识的筛选列 {key:?}；span 表可用的筛选列: {}",
                if dim_columns.is_empty() {
                    "（无）".to_owned()
                } else {
                    dim_columns.join(", ")
                }
            )));
        }
        let values = p.get_list(key);
        if !values.is_empty() {
            dims.push((key.to_owned(), values));
        }
    }
    Ok(dims)
}

/// `entry_by_default`：没选 kind 时是否只看入口 span（Server / Consumer）。
fn build_filter(
    state: &AppState,
    schema: &Schema,
    p: &Params,
    entry_by_default: bool,
) -> Result<TraceFilter> {
    let ms_to_ns = |v: Option<f64>| v.map(|ms| (ms.max(0.0) * 1_000_000.0) as u64);
    let mut kinds: Vec<String> =
        p.get_list("kind").iter().map(|k| normalize_kind(k)).collect::<Result<_>>()?;
    // 「最慢的请求」和热力图默认看入口 span：不限 kind 的话排在前面的会是慢 SQL、慢下游调用，
    // 而 Java 开发说的慢请求是 Server span
    if entry_by_default && kinds.is_empty() {
        kinds = ENTRY_KINDS.iter().map(|k| (*k).to_owned()).collect();
    }
    Ok(TraceFilter {
        range: Some(range(state, p)?),
        service: p.get("service").map(str::to_owned),
        span_name: p.get("span_name").map(str::to_owned),
        kinds,
        error_only: p.get_bool("error_only")?.unwrap_or(false),
        min_duration_ns: ms_to_ns(p.get_f64("min_ms")?),
        max_duration_ns: ms_to_ns(p.get_f64("max_ms")?),
        attrs: p.get_all("attr").into_iter().map(AttrFilter::parse).collect::<Result<_>>()?,
        resource_attrs: p
            .get_all("rattr")
            .into_iter()
            .map(AttrFilter::parse)
            .collect::<Result<_>>()?,
        dims: dims(schema, p)?,
    })
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub traces: Vec<TraceSummary>,
    pub limit: u32,
    pub sort: TraceSort,
    pub stats: Stats,
}

async fn search(State(state): State<AppState>, p: Params) -> Result<Json<SearchResponse>> {
    let schema = state.schema.get().await?;
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let sort = TraceSort::parse(p.get("sort"))?;
    let limit = p.get_limit("limit", 50, state.config.max_rows.min(500))?;

    // 直接给了 trace id：跳过检索，只查这一条的摘要（不限时间，走 bloom filter）
    let (ids, narrow, full, mut stats) = if let Some(raw) = p.get("trace_id") {
        (vec![normalize_trace_id(raw)?], None, None, Stats::default())
    } else {
        let filter = build_filter(&state, &schema, &p, sort == TraceSort::Duration)?;
        // 先按**用户给的**范围校验。探测会把窗口切成 1 分钟的小段去查（见 candidates），
        // 而校验是在拼 SQL 时做的，只能看见切过的那一段——「不选服务不能查超过 6 小时」这条
        // 护栏就这么被绕过去了：同一个请求，探测凑够了返回 200、没凑够回退整窗才 400
        filter.validate()?;
        let (rows, stats) = candidates(&state, &queries, &filter, sort, limit).await?;
        // 摘要先按候选自己的时间跨度查，不是按整个搜索窗，见 TraceQueries::summaries
        let narrow = candidate_range(&rows);
        (rows.into_iter().map(|r| r.trace_id).collect::<Vec<_>>(), narrow, filter.range, stats)
    };
    if ids.is_empty() {
        return Ok(Json(SearchResponse { traces: Vec::new(), limit, sort, stats }));
    }
    let (rows, summary_stats) = summaries(&state, &queries, &ids, narrow, full).await?;
    stats.absorb(&summary_stats);
    // 保持候选查询的顺序（按时间 / 按耗时）
    let mut by_id: std::collections::HashMap<String, TraceSummary> =
        rows.into_iter().map(|r| (r.trace_id.clone(), TraceSummary::from(r))).collect();
    let traces: Vec<TraceSummary> = ids.iter().filter_map(|id| by_id.remove(id)).collect();
    // 候选查询刚看到的 trace 在聚合查询里找不到：正常不该发生（同一张表、时间范围还放宽了 10 分钟），
    // 真出现多半是分片 / 副本一时不一致，把 id 记下来好对着库查
    if traces.len() < ids.len() {
        let missing: Vec<&str> = ids
            .iter()
            .filter(|id| !traces.iter().any(|t| &t.trace_id == *id))
            .map(String::as_str)
            .collect();
        tracing::warn!(
            candidates = ids.len(),
            aggregated = traces.len(),
            ?missing,
            "trace summaries missing for some candidate ids"
        );
    }
    Ok(Json(SearchResponse { traces, limit, sort, stats }))
}

/// 候选 trace。按时间排时先从时间窗尾部探一小段，凑够 `limit` 条就收工——`timestamp` 不是
/// 排序键前缀，扫整个窗口是这条查询的全部成本，见 [`PROBE_WINDOWS_MS`]。
///
/// 提前停不会漏：探测窗之外的 trace，它所有匹配 span 都比窗口起点旧，排名必然在这 `limit` 条
/// 之后。同一毫秒内的并列本来就无序（现在的实现自己跑两遍也会换一条），探不探测都一样。
async fn candidates(
    state: &AppState,
    queries: &TraceQueries<'_>,
    filter: &TraceFilter,
    sort: TraceSort,
    limit: u32,
) -> Result<(Vec<CandidateRow>, Stats)> {
    let mut stats = Stats::default();
    match sort {
        // 最新在前：先探窗口尾部的一小段
        TraceSort::Time => {
            if let Some(full) = filter.range {
                for window in PROBE_WINDOWS_MS {
                    // 搜索窗本身就比探测窗窄，直接查整窗，别白搭一次往返
                    if full.span_ms() <= *window {
                        break;
                    }
                    let mut probe = filter.clone();
                    probe.range =
                        Some(TimeRange { from_ms: full.to_ms - window, to_ms: full.to_ms });
                    let hit = state
                        .client
                        .rows::<CandidateRow>(queries.candidates(
                            &probe,
                            sort,
                            Candidates::PerTrace(limit),
                        )?)
                        .await?;
                    stats.absorb(&hit.stats);
                    if hit.rows.len() as u32 >= limit {
                        return Ok((hit.rows, stats));
                    }
                }
            }
        }
        // 最慢在前：探测用不了（最慢的那条可能在窗口任何位置），改成多取几倍的行、
        // 自己去重，换 ClickHouse 的惰性物化，见 Candidates
        TraceSort::Duration => {
            let over = limit.saturating_mul(CANDIDATE_OVERFETCH);
            let wide = state
                .client
                .rows::<CandidateRow>(queries.candidates(filter, sort, Candidates::Rows(over))?)
                .await?;
            stats.absorb(&wide.stats);
            let truncated = wide.rows.len() as u32 >= over;
            let rows = dedup_by_trace(wide.rows, limit);
            // 够了就收工；没取满 `over` 行说明匹配的 span 本来就这么多，去重结果已经是全部
            if rows.len() as u32 >= limit || !truncated {
                return Ok((rows, stats));
            }
            // 多取的行里不同的 trace 不够（一条链路占满了最慢的那几百个 span），退回库里去重
        }
    }
    let all = state
        .client
        .rows::<CandidateRow>(queries.candidates(filter, sort, Candidates::PerTrace(limit))?)
        .await?;
    stats.absorb(&all.stats);
    Ok((all.rows, stats))
}

/// 这些 trace 的摘要。先按候选自己的时间跨度查——便宜得多——再把**窗口里没找到根 span**
/// 的那几条按完整搜索窗补一次。
///
/// 为什么要补：收窄窗口会切掉跑得久的链路（MQ 消费那种一跑几十分钟的）。线上实测采样 10.7 万条
/// 链路，跨度超过 10 分钟的只有 0.02%，但它们 span 多，被「最新的 50 条」抽中的概率也高，
/// 一页里能有 0~4 条。`root_count = 0` 正好认得出来：完整的链路一定有一个 `parent_span_id = ''`
/// 的根 span，找不到就说明前面被切了。
///
/// 补捞不贵：`trace_id` 的 bloom filter 是 2.5% 误报，50 个 id 一起 OR 有 72% 的块活下来，
/// 换成几条 id 就回到个位数百分比。四个时间窗上实测，**摘要结果与只查完整窗口逐字段一致
/// （0/50 有出入），扫描量少 2.4~3.5 倍**；最坏的一个窗口（50 条里 16 条本来就没有根 span）
/// 是打平，不会更差。
async fn summaries(
    state: &AppState,
    queries: &TraceQueries<'_>,
    ids: &[String],
    narrow: Option<TimeRange>,
    full: Option<TimeRange>,
) -> Result<(Vec<SummaryRow>, Stats)> {
    // 搜索窗本来就没比候选跨度宽多少（放宽 10 分钟之后更是如此）就别收窄了，省得白补一次
    let narrow = narrow.filter(|n| match full {
        Some(f) => n.widen(SUMMARY_WIDEN_MS).span_ms() * 2 <= f.widen(SUMMARY_WIDEN_MS).span_ms(),
        None => false,
    });
    let mut stats = Stats::default();
    let first = state
        .client
        .rows::<SummaryRow>(queries.summaries(ids, narrow.as_ref().or(full.as_ref()))?)
        .await?;
    stats.absorb(&first.stats);
    let mut rows = first.rows;
    let (Some(_), Some(full)) = (narrow, full) else { return Ok((rows, stats)) };
    let cut: Vec<String> =
        rows.iter().filter(|r| r.root_count == 0).map(|r| r.trace_id.clone()).collect();
    if cut.is_empty() {
        return Ok((rows, stats));
    }
    let again = state.client.rows::<SummaryRow>(queries.summaries(&cut, Some(&full))?).await?;
    stats.absorb(&again.stats);
    let mut fixed: std::collections::HashMap<String, SummaryRow> =
        again.rows.into_iter().map(|r| (r.trace_id.clone(), r)).collect();
    for row in &mut rows {
        if let Some(whole) = fixed.remove(&row.trace_id) {
            *row = whole;
        }
    }
    Ok((rows, stats))
}

/// 热力图每个数量级分几档。4 档 = 1 / 1.8 / 3.2 / 5.6 倍的边界。
const HEATMAP_BINS_PER_DECADE: u32 = 4;

#[derive(Serialize)]
pub struct HeatmapCell {
    pub t_ms: i64,
    /// 对数耗时档序号：耗时 ms 落在 `[10^(lvl/bins), 10^((lvl+1)/bins))`；最底一档（1µs）含更短的
    pub lvl: i32,
    pub count: u64,
    pub errors: u64,
}

#[derive(Serialize)]
pub struct HeatmapResponse {
    pub from_ms: i64,
    pub to_ms: i64,
    pub width_ms: i64,
    pub bins_per_decade: u32,
    /// 实际参与统计的 span kind；没选时默认入口 span
    pub kinds: Vec<String>,
    /// 只有非空格子
    pub cells: Vec<HeatmapCell>,
    pub total: u64,
    pub max_count: u64,
    pub stats: Stats,
}

/// 耗时 × 时间的热力图。和检索用同一套筛选条件，但不取前 N 条，而是在库里按
/// 时间桶 × 对数耗时档聚合，返回的格子数有上限，任意范围都能看到全貌。
/// 一种报错。`kind` / `msg` 里是什么、为什么可能为空，见 [`TraceQueries::error_groups`]。
#[derive(Serialize)]
pub struct ErrorGroup {
    /// 分组身份，前端拿它做 key 和「只看这一种」的筛选
    pub id: String,
    pub service: String,
    pub span_kind: String,
    pub span_name: String,
    pub exception: String,
    pub message: String,
    pub http_status: String,
    pub peer: String,
    pub count: u64,
    pub traces: u64,
    pub first_ms: i64,
    pub last_ms: i64,
    /// 最近一条的样本：点进去就是那条链路，落地自动选中报错的 span
    pub sample_trace: String,
    pub sample_span: String,
}

#[derive(Serialize)]
pub struct ErrorsResponse {
    pub from_ms: i64,
    pub to_ms: i64,
    pub kind: String,
    /// 这段时间内出错的 span 总数（列表被 limit 截断时也是全量）
    pub total: u64,
    pub groups: Vec<ErrorGroup>,
    pub stats: Stats,
}

/// 异常消息截多长；超出的部分只会把同一种错拆成很多组。
const ERROR_MSG_LEN: u32 = 160;
/// 最多返回多少组。线上全站一小时的入口错误是 13 组，200 足够宽裕。
const ERROR_GROUPS_LIMIT: u32 = 200;

/// `/api/errors`：把出错的 span 按「同一种报错」归堆。
///
/// 默认 `kind=entry`（Server / Consumer）——服务总览上那个错误率就是按入口 span 算的，
/// 默认值一致，点进来看到的才是「dash 上那些错误到底是什么」。不限 kind 的话，线上一小时
/// 的列表里 70% 是下游 HTTP 404（4 万条），真正的入口错误会被压到看不见。
async fn errors(State(state): State<AppState>, p: Params) -> Result<Json<ErrorsResponse>> {
    let schema = state.schema.get().await?;
    let range = range(&state, &p)?;
    let kind = p.get("kind").unwrap_or("entry");
    let kinds: &[&str] = match kind {
        "entry" => ENTRY_KINDS,
        "client" => CLIENT_KINDS,
        "all" => &[],
        other => {
            return Err(Error::bad_request(format!(
                "kind 只能是 entry / client / all，不是 {other:?}"
            )));
        }
    };
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let result = state
        .client
        .rows::<ErrorGroupRow>(queries.error_groups(
            &range,
            kinds,
            p.get("service"),
            p.get("span_name"),
            ERROR_MSG_LEN,
            ERROR_GROUPS_LIMIT,
        )?)
        .await?;
    let mut total = 0;
    let groups = result
        .rows
        .into_iter()
        .map(|r| {
            total += r.n;
            ErrorGroup {
                // 分组身份由 ErrorGroupRow 自己给，和 SQL 的 GROUP BY 共用一份列清单；
                // 在这里手拼过，漏了 peer 和 span_kind，导致两组共用一个 id（见 ERROR_GROUP_KEYS）
                id: r.group_id(),
                service: r.service_name,
                span_kind: r.span_kind,
                span_name: r.span_name,
                exception: r.exc_type,
                message: r.exc_msg,
                http_status: r.http_status,
                peer: r.peer,
                count: r.n,
                traces: r.traces,
                first_ms: r.first_ms,
                last_ms: r.last_ms,
                sample_trace: r.sample_trace,
                sample_span: r.sample_span,
            }
        })
        .collect();
    Ok(Json(ErrorsResponse {
        from_ms: range.from_ms,
        to_ms: range.to_ms,
        kind: kind.to_owned(),
        total,
        groups,
        stats: result.stats,
    }))
}

async fn heatmap(State(state): State<AppState>, p: Params) -> Result<Json<HeatmapResponse>> {
    let schema = state.schema.get().await?;
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let filter = build_filter(&state, &schema, &p, true)?;
    let range = filter.range.expect("build_filter 总会填 range");
    let tz = parse_tz(&state.config.timezone)?;
    let bucket = Bucket::choose(&range, tz, 120);
    let result = state
        .client
        .rows::<HeatmapRow>(queries.heatmap(&filter, &bucket, HEATMAP_BINS_PER_DECADE)?)
        .await?;
    let first = bucket.first_index(&range);
    let count = bucket.count(&range).max(0);
    let mut total = 0;
    let mut max_count = 0;
    let mut cells = Vec::with_capacity(result.rows.len());
    for r in result.rows {
        if r.bucket < first || r.bucket >= first + count {
            continue;
        }
        total += r.n;
        max_count = max_count.max(r.n);
        cells.push(HeatmapCell {
            t_ms: bucket.start_ms(r.bucket),
            lvl: r.lvl,
            count: r.n,
            errors: r.errors,
        });
    }
    Ok(Json(HeatmapResponse {
        from_ms: range.from_ms,
        to_ms: range.to_ms,
        width_ms: bucket.width_ms,
        bins_per_decade: HEATMAP_BINS_PER_DECADE,
        kinds: filter.kinds,
        cells,
        total,
        max_count,
        stats: result.stats,
    }))
}

/// 一条 trace 的 span 定位在哪一档窗口上查到的。
struct Located {
    rows: Vec<LocatedSpan>,
    /// 几档探测加起来的读量
    stats: Stats,
    /// 这条 trace 的 span 没显示全：要么取满了 `max`，要么退回了更窄的一档窗口
    truncated: bool,
    /// 用的那一档窗口；`None` = 不限时间，扫了全部分区
    window: Option<TimeRange>,
    /// 宽的那一档装不下，退回了窄的一档（见 [`locate_spans`]）
    narrowed: bool,
}

/// 按 [DETAIL_PROBE_WINDOWS](crate::query::traces::DETAIL_PROBE_WINDOWS) 从窄到宽探，命中一档就收工。
///
/// `at` 是这条 trace 大概在什么时候——从列表页点进来是它的开始时间，从日志点进来是那条日志的
/// 时间，顶栏直达时是页面当前时间范围的猜测。**一个都没有就锚在「现在」**往回探
/// （[DETAIL_NOW_WINDOWS](crate::query::traces::DETAIL_NOW_WINDOWS)：手点进来的 trace 几乎都是刚发生的）。不管哪种，最后一档不限时间
/// 的兜底都保证查得全，猜错只是多跑两趟空查询。
///
/// **装不下就退回窄窗口**（2026-09-16 加）。span 数超过 `--max-trace-spans` 时，宽窗口那一档
/// 取回来的是「按时间从早往晚的前 5000 个」——对被复用的 trace id（常驻消费者一直用同一个，
/// 线上那条 `1719ae…8dbc` 挂着 2 小时 13 分、21882 个 span）来说，那是一小时前的另一段，和用户
/// 带着 `at` 点进来想看的那一刻毫无关系，指名的 span 也在被切掉的后半段里。所以改成退回
/// **第一档够用的窗口**（够用 = 至少 `max/10` 个 span，免得 `at` 偏了几分钟时退回一张空图）：
/// 它围着 `at`、在自己这个窗口里是完整的，而且便宜得多。线上同一条 trace 实测：
///
/// | 退回哪一档 | span 数 | 取数 | 关联日志的窗口 |
/// |---|---|---|---|
/// | 不退（最早的 5000 个） | 5000 | 16.4 MB | 18 分钟，且指名的 span 不在图里 |
/// | ±15 分钟 | 4738 | 34.3 MB | 39 分钟 / 1.6 GB |
/// | **±1 分钟（现在）** | **701** | **3.8 MB** | **11 分钟 / 0.4 GB** |
async fn locate_spans(
    state: &AppState,
    queries: &TraceQueries<'_>,
    trace_id: &str,
    max: u32,
    at: Option<i64>,
) -> Result<Located> {
    let mut stats = Stats::default();
    // 装不下时的退路：第一档「够用」的窗口（至少 max/10 个 span）
    let mut narrow: Option<(Vec<LocatedSpan>, Option<TimeRange>)> = None;
    // 探过的最后一档：没探中也没装不下时就用它（原来的行为）
    let mut last: Option<(Vec<LocatedSpan>, Option<TimeRange>)> = None;
    let mut done: Option<(Vec<LocatedSpan>, Option<TimeRange>, bool, bool)> = None;
    for window in detail_probes(at, state.now_ms()) {
        // 最后这档是全表扫（线上 5.1 GB / 39 s）。上一档已经装了半个上限还多，这条 trace
        // 无论如何都放不下，扫回来也只会被截断、再退回窄窗口——那一趟纯亏
        if window.is_none() && last.as_ref().is_some_and(|(r, _)| r.len() * 2 > max as usize) {
            break;
        }
        let r = state
            .client
            .rows::<LocatedSpan>(queries.detail_locate(trace_id, max, window.as_ref())?)
            .await?;
        stats.absorb(&r.stats);
        if r.rows.len() > max as usize {
            // 这一档装不下。先试「从 at 往后尽量长」的几档：trace 是往后跑的，`at` 多半是它的
            // 开头，同样的 span 预算往后延能盖住这条 trace 自己的时间。都装不下再退回围着 at
            // 的窄窗口；连退路都没有（第一档就满了）才按时间切前 max 个
            let forward = match at {
                Some(at) => {
                    forward_fallback(state, queries, trace_id, max, at, window.as_ref(), &mut stats)
                        .await?
                }
                None => None,
            };
            done = Some(match forward.or_else(|| narrow.take()) {
                Some((rows, window)) => (rows, window, true, true),
                None => (r.rows, window, true, false),
            });
            break;
        }
        if window.as_ref().is_some_and(|w| detail_probe_hit(&r.rows, w)) {
            done = Some((r.rows, window, false, false));
            break;
        }
        // 没探中：贴着窗口边，trace 多半还往外延伸，继续往宽里探
        if narrow.is_none() && r.rows.len() * 10 >= max as usize && !r.rows.is_empty() {
            narrow = Some((r.rows.clone(), window));
        }
        if !r.rows.is_empty() {
            last = Some((r.rows, window));
        }
    }
    let (rows, window, truncated, narrowed) = done
        .or_else(|| last.map(|(rows, window)| (rows, window, false, false)))
        .unwrap_or_default();
    Ok(Located { rows, stats, truncated, window, narrowed })
}

/// 宽窗口装不下时的退路：从 `at` 往后延着试几档，取第一档装得下的。
///
/// 只试比 `over`（已经装不下的那一档）更窄的窗口，见
/// [DETAIL_FORWARD_WINDOWS](crate::query::traces::DETAIL_FORWARD_WINDOWS)。最多多跑三趟带窗口的定位查询，
/// 而且只在「这条 trace 大得装不下」这条已经很贵的路径上才发生。
async fn forward_fallback(
    state: &AppState,
    queries: &TraceQueries<'_>,
    trace_id: &str,
    max: u32,
    at: i64,
    over: Option<&TimeRange>,
    stats: &mut Stats,
) -> Result<Option<(Vec<LocatedSpan>, Option<TimeRange>)>> {
    for window in detail_forward_probes(at, over) {
        let r = state
            .client
            .rows::<LocatedSpan>(queries.detail_locate(trace_id, max, Some(&window))?)
            .await?;
        stats.absorb(&r.stats);
        if !r.rows.is_empty() && r.rows.len() <= max as usize {
            return Ok(Some((r.rows, Some(window))));
        }
    }
    Ok(None)
}

/// 单独定位 URL 上 `span=` 指名的那一个 span，窗口同样按 [DETAIL_PROBE_WINDOWS](crate::query::traces::DETAIL_PROBE_WINDOWS)
/// （没有 `at` 时按 [DETAIL_NOW_WINDOWS](crate::query::traces::DETAIL_NOW_WINDOWS)）从窄往宽探。
///
/// `deep`：要不要走最后那档不限时间的兜底。主查询被截断（这条 trace 的 span 可能散在探到的
/// 窗口之外），或者它本来就没带时间条件时才走；否则窗口里已经是这条 trace 的全部 span，
/// 再为一个找不到的 id 扫全部分区（线上 789 MB / 7 s）多半只是 id 抄错了。
async fn locate_one(
    state: &AppState,
    queries: &TraceQueries<'_>,
    trace_id: &str,
    span_id: &str,
    at: Option<i64>,
    deep: bool,
) -> Result<(Option<LocatedSpan>, Stats)> {
    let mut stats = Stats::default();
    for window in detail_probes(at, state.now_ms()) {
        if window.is_none() && !deep {
            continue;
        }
        let r = state
            .client
            .rows::<LocatedSpan>(queries.detail_locate_span(trace_id, span_id, window.as_ref())?)
            .await?;
        stats.absorb(&r.stats);
        if let Some(found) = r.rows.into_iter().next() {
            return Ok((Some(found), stats));
        }
    }
    Ok((None, stats))
}

#[derive(Serialize)]
pub struct SpanAttrsResponse {
    pub trace_id: String,
    pub span_id: String,
    pub attributes: BTreeMap<String, Value>,
    pub resource: BTreeMap<String, Value>,
    pub events: Vec<SpanEvent>,
    pub links: Vec<SpanLink>,
    pub stats: Stats,
}

#[derive(Serialize)]
pub struct DetailResponse {
    pub trace_id: String,
    pub spans: Vec<Span>,
    /// span 数超过了 `--max-trace-spans`，只返回了前面这些
    pub truncated: bool,
    /// 按 `at` 附近的时间窗口查的；false = 不限时间，扫了全部分区
    pub windowed: bool,
    /// 实际查的时间窗（unix 毫秒）。`truncated` 时前端拿它说清楚「只显示了哪一段」
    pub window_from_ms: Option<i64>,
    pub window_to_ms: Option<i64>,
    /// span 太多，宽的那一档装不下，退回了围着 `at` 的窄窗口（见 [`locate_spans`]）
    pub narrowed: bool,
    /// `span=` 指名的那个 span 不在上面这批里（trace 被截断了），单独捞回来钉在 `spans` 末尾。
    /// 它的父 span 多半不在图里，瀑布图上会挂成一条「父缺失」
    pub pinned_span: Option<String>,
    /// span 的属性 / events / links 没在这里返回，点开某个 span 时按
    /// `/api/traces/{trace_id}/spans/{span_id}` 单独取（那四个 JSON 列是详情查询的全部成本）
    pub attributes_lazy: bool,
    pub stats: Stats,
}

/// `at`（unix 毫秒，可选）：trace 的开始时间。列表页 / 日志页跳过来时都知道，带上就能裁剪分区。
/// `span`（可选）：页面要选中的那个 span，超过上限被截断时保证它也在返回里，见 [`locate_one`]。
///
/// 两次往返：先按 trace id 只读轻列定位 span（bloom filter 的误报块读起来便宜），再按排序键前缀
/// 走主键取全部列。原因见 [`TraceQueries::detail_locate`]。
async fn detail(
    State(state): State<AppState>,
    Path(trace_id): Path<String>,
    p: Params,
) -> Result<Json<DetailResponse>> {
    let schema = state.schema.get().await?;
    let trace_id = normalize_trace_id(&trace_id)?;
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let max = state.config.max_trace_spans;
    let at = p.get_i64("at")?;
    // 写错的 span id 不拦成 400：它只是「页面想选中哪个」的提示，瀑布图本身照样该画出来
    let wanted = p.get("span").and_then(|s| normalize_span_id(s).ok());
    let probe = locate_spans(&state, &queries, &trace_id, max, at).await?;
    let mut stats = probe.stats;
    let truncated = probe.truncated;
    let narrowed = probe.narrowed;
    let window = probe.window;
    let located: Vec<LocatedSpan> = probe.rows.into_iter().take(max as usize).collect();
    // 页面指名要看的那个 span 不在这批里（截断切掉了，或退回窄窗口时落在窗口外）：
    // 单独定位一次，下面取完瀑布图再把它钉进去
    let pinned = match &wanted {
        Some(id) if !located.iter().any(|s| &s.span_id == id) => {
            let deep = truncated || window.is_none();
            // 没有 at（顶栏直达、点了「查全部时间」）就拿定位到的最后一个 span 当中心点：
            // 被 LIMIT 切掉的都在它后面，几档窄窗口多半就能捞着，省掉那趟全表扫
            let center = at.or_else(|| located.iter().map(|s| s.ts_ms).max());
            let (found, s) = locate_one(&state, &queries, &trace_id, id, center, deep).await?;
            stats.absorb(&s);
            found
        }
        _ => None,
    };
    let mut spans: Vec<Span> = if located.is_empty() {
        Vec::new()
    } else {
        // 瀑布图不要属性列：那四列是这一步的全部成本，点开某个 span 时再单独取
        let full =
            state.client.rows::<SpanRow>(queries.detail_fetch(&trace_id, &located, false)?).await?;
        stats.absorb(&full.stats);
        // 第二步比第一步少了行：正常不该发生（同一张表、条件是第一步的超集、排序相同），
        // 真出现多半是分片 / 副本一时不一致，记下来好对着库查
        if full.rows.len() < located.len() {
            tracing::warn!(
                trace_id,
                located = located.len(),
                fetched = full.rows.len(),
                "trace detail fetched fewer spans than located"
            );
        }
        full.rows.into_iter().map(Span::from).collect()
    };
    // 钉住的那一个按排序键前缀单独取（一行，0.01 GB 量级），排在末尾，瀑布图自己按时间排
    let mut pinned_span = None;
    if let Some(span) = pinned {
        let one = state.client.rows::<SpanRow>(queries.detail_span(&trace_id, &span)?).await?;
        stats.absorb(&one.stats);
        if let Some(row) = one.rows.into_iter().next() {
            pinned_span = Some(span.span_id);
            spans.push(Span::from(row));
        }
    }
    Ok(Json(DetailResponse {
        trace_id,
        spans,
        truncated,
        windowed: window.is_some(),
        window_from_ms: window.as_ref().map(|w| w.from_ms),
        window_to_ms: window.as_ref().map(|w| w.to_ms),
        narrowed,
        pinned_span,
        attributes_lazy: true,
        stats,
    }))
}

/// 一个 span 的属性 / events / links。瀑布图那一趟故意不取这几列，用户点开哪个 span 才查哪个。
///
/// 要圈到这一个 span，得知道它的 `service_name` / `span_name` / 毫秒时间戳——它们是排序键
/// 前缀。页面上这三个值就在手里（详情响应里给过），带上就直接查；不带就自己先跑一遍定位
/// 查询。差别不小：定位要按 trace id 扫 bloom filter（线上一条 25 小时窗口 0.11 GB），
/// 带上前缀之后这一趟只剩 0.001 GB 量级。
async fn span_attrs(
    State(state): State<AppState>,
    Path((trace_id, span_id)): Path<(String, String)>,
    p: Params,
) -> Result<Json<SpanAttrsResponse>> {
    let schema = state.schema.get().await?;
    let trace_id = normalize_trace_id(&trace_id)?;
    let span_id = normalize_span_id(&span_id)?;
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let hint = match (p.get("service"), p.get("name"), p.get_i64("ts")?) {
        (Some(service), Some(name), Some(ts_ms)) => Some(LocatedSpan {
            span_id: span_id.clone(),
            service_name: service.to_owned(),
            span_name: name.to_owned(),
            ts_ms,
        }),
        _ => None,
    };
    let mut stats = Stats::default();
    let span = match hint {
        Some(span) => span,
        // 只找这一个 span：定位整条 trace 再从里面挑，既多读几千行，超过
        // `--max-trace-spans` 时还会把要找的那个截掉（见 locate_one）
        None => {
            let (found, s) =
                locate_one(&state, &queries, &trace_id, &span_id, p.get_i64("at")?, true).await?;
            stats = s;
            found.ok_or_else(|| Error::bad_request(format!("这条 trace 里没有 span {span_id}")))?
        }
    };
    let full = state.client.rows::<SpanRow>(queries.detail_span(&trace_id, &span)?).await?;
    stats.absorb(&full.stats);
    let span = full
        .rows
        .into_iter()
        .next()
        .map(Span::from)
        .ok_or_else(|| Error::bad_request("span 已经不在库里了（可能刚过 TTL）"))?;
    Ok(Json(SpanAttrsResponse {
        trace_id,
        span_id,
        attributes: span.attributes,
        resource: span.resource,
        events: span.events,
        links: span.links,
        stats,
    }))
}

#[derive(Serialize)]
pub struct ValuesResponse {
    pub field: String,
    pub values: Vec<ValueRow>,
    pub stats: Stats,
}

/// `field=service`：时间范围内的服务；`field=span_name&service=x[&kind=entry|client|all]`：某服务的操作。
async fn values(State(state): State<AppState>, p: Params) -> Result<Json<ValuesResponse>> {
    let schema = state.schema.get().await?;
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let range = range(&state, &p)?;
    let limit = p.get_limit("limit", 200, 2000)?;
    let field = p.get("field").unwrap_or("service");
    let query = match field {
        "service" | "service_name" => queries.services(&range, limit)?,
        "span_name" => {
            let service = p
                .get("service")
                .ok_or_else(|| Error::bad_request("field=span_name 需要 service"))?;
            let kinds: &[&str] = match p.get("kind").unwrap_or("all") {
                "entry" => ENTRY_KINDS,
                "client" => CLIENT_KINDS,
                "all" => &[],
                other => &[Box::leak(normalize_kind(other)?.into_boxed_str())],
            };
            queries.span_names(&range, service, kinds, limit)?
        }
        other => {
            return Err(Error::bad_request(format!(
                "field 只能是 service 或 span_name，不是 {other:?}"
            )));
        }
    };
    let result = state.client.rows::<ValueRow>(query).await?;
    Ok(Json(ValuesResponse { field: field.to_owned(), values: result.rows, stats: result.stats }))
}

#[derive(Serialize)]
pub struct KeysResponse {
    pub scope: String,
    pub keys: Vec<KeyRow>,
    pub stats: Stats,
}

async fn attr_keys(State(state): State<AppState>, p: Params) -> Result<Json<KeysResponse>> {
    let schema = state.schema.get().await?;
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let range = range(&state, &p)?;
    let scope = p.get("scope").unwrap_or("span");
    let limit = p.get_limit("limit", 200, 1000)?;
    let result = state
        .client
        .rows::<KeyRow>(queries.attr_keys(&range, p.get("service"), scope, limit)?)
        .await?;
    Ok(Json(KeysResponse { scope: scope.to_owned(), keys: result.rows, stats: result.stats }))
}

async fn attr_values(State(state): State<AppState>, p: Params) -> Result<Json<ValuesResponse>> {
    let schema = state.schema.get().await?;
    let queries = TraceQueries { database: &state.config.database, table: &schema.traces };
    let range = range(&state, &p)?;
    let scope = p.get("scope").unwrap_or("span");
    let key = p.get("key").ok_or_else(|| Error::bad_request("缺少参数 key"))?;
    let limit = p.get_limit("limit", 50, 500)?;
    let result = state
        .client
        .rows::<ValueRow>(queries.attr_values(&range, p.get("service"), scope, key, limit)?)
        .await?;
    Ok(Json(ValuesResponse { field: key.to_owned(), values: result.rows, stats: result.stats }))
}
