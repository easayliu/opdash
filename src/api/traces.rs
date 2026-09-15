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
    ValueRow, candidate_range, dedup_by_trace, normalize_kind, normalize_span_id,
    normalize_trace_id,
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

/// 详情按时间窗口裁剪时，开始时间往前放多少、往后放多少：往前给时钟偏差留余量，
/// 往后要装下根返回后才跑的异步 span（消息消费、定时补偿可能晚半小时以上）。
const DETAIL_WINDOW_BEFORE_MS: i64 = 3_600_000;
const DETAIL_WINDOW_AFTER_MS: i64 = 24 * 3_600_000;

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
    /// 按 `at` 附近的时间窗口查的（前 1 小时、后 24 小时）；不带 `at` 是全表按 bloom filter 找
    pub windowed: bool,
    /// span 的属性 / events / links 没在这里返回，点开某个 span 时按
    /// `/api/traces/{trace_id}/spans/{span_id}` 单独取（那四个 JSON 列是详情查询的全部成本）
    pub attributes_lazy: bool,
    pub stats: Stats,
}

/// `at`（unix 毫秒，可选）：trace 的开始时间。列表页 / 日志页跳过来时都知道，带上就能裁剪分区。
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
    let window = p.get_i64("at")?.map(|at| TimeRange {
        from_ms: (at - DETAIL_WINDOW_BEFORE_MS).max(0),
        to_ms: at + DETAIL_WINDOW_AFTER_MS,
    });
    let located = state
        .client
        .rows::<LocatedSpan>(queries.detail_locate(&trace_id, max, window.as_ref())?)
        .await?;
    let mut stats = located.stats;
    let truncated = located.rows.len() > max as usize;
    let located: Vec<LocatedSpan> = located.rows.into_iter().take(max as usize).collect();
    let spans: Vec<Span> = if located.is_empty() {
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
    Ok(Json(DetailResponse {
        trace_id,
        spans,
        truncated,
        windowed: window.is_some(),
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
    let window = p.get_i64("at")?.map(|at| TimeRange {
        from_ms: (at - DETAIL_WINDOW_BEFORE_MS).max(0),
        to_ms: at + DETAIL_WINDOW_AFTER_MS,
    });
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
    let span =
        match hint {
            Some(span) => span,
            None => {
                let located = state
                    .client
                    .rows::<LocatedSpan>(queries.detail_locate(
                        &trace_id,
                        state.config.max_trace_spans,
                        window.as_ref(),
                    )?)
                    .await?;
                stats = located.stats;
                located.rows.into_iter().find(|s| s.span_id == span_id).ok_or_else(|| {
                    Error::bad_request(format!("这条 trace 里没有 span {span_id}"))
                })?
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
