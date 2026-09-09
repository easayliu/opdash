//! `/api/logs/tail`：日志跟随的服务端推送（SSE）。
//!
//! ClickHouse 不会主动推（MergeTree 上没有能用的 WATCH），所以还是轮询——只是把轮询从每个浏览器
//! 挪到服务端：页面开一条长连接，服务端拿着游标每 `--tail-interval` 查一次增量，只把新行发下去。
//! 相比前端每 5 秒重查一遍整个窗口，延迟从 5 秒降到 1 秒，每次查的还只是游标往后那一小段。

use std::collections::{HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderValue},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use futures_util::stream::{self, Stream, StreamExt};
use serde::Serialize;
use tokio::sync::OwnedSemaphorePermit;

use super::{AppState, logs::build_filter, params::Params};
use crate::clickhouse::Stats;
use crate::error::{Error, Result};
use crate::query::TimeRange;
use crate::query::logs::{LogFilter, LogQueries, LogRow, Order};
use crate::schema::Schema;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/logs/tail", get(tail))
}

/// 回看多久兜住晚到的行：logpipe 是攒批写的，一行的时间戳可能比它落库的时刻早几十秒，
/// 只顺着游标往后查会把这些行漏掉。
const LOOKBACK_MS: i64 = 60_000;

/// 多久回看一次。回看查的是已经过去的窗口，不必每轮都做——每 10 秒一次，多出来的查询可以忽略；
/// 更要紧的是回看不能顶替正常那一轮：一个窗口里的行数超过 `limit` 时，从窗口头部取前 `limit` 行
/// 拿到的全是推过的，游标就再也走不动了。
const LOOKBACK_EVERY: Duration = Duration::from_secs(10);

/// 断线重连时最多补多久的历史。断太久就别补了，从最新一屏重新开始——补上万行既慢，
/// 也不是跟随想看的东西（要看历史用翻页）。
const RESUME_MAX_GAP_MS: i64 = 5 * 60_000;

/// 一轮里最多连着追几批。库里积压很多时不睡够间隔就接着查，把积压吐完；但也不能一直追，
/// 不然写入速度长期高于 `limit / interval` 的话这条连接就在满速刷库了。
const MAX_CATCH_UP: u32 = 10;

/// 连着这么多轮查询失败就断开。EventSource 会自己重连（并带上 Last-Event-ID 接着游标续），
/// 比在这里死等强。
const MAX_ERRORS: u32 = 5;

/// 指纹表的上限，防止跟一整天把内存吃掉。超了丢最老的，代价是极老的重复行可能再推一次。
const MAX_SEEN: usize = 20_000;

/// 保活注释的间隔：中间隔着 nginx / k8s ingress 时，闲着不发东西的连接会被掐掉。
const KEEP_ALIVE: Duration = Duration::from_secs(15);

#[derive(Serialize)]
struct Hello {
    /// 从这一刻的日志开始跟（unix 毫秒）
    cursor_ms: i64,
    interval_ms: u64,
    lookback_ms: i64,
    limit: u32,
    /// 断线重连时接着上次的游标续上了（而不是从最新一屏重来）
    resumed: bool,
}

#[derive(Serialize)]
struct RowsEvent {
    rows: Vec<LogRow>,
    /// 这一批查询在 ClickHouse 上的开销，页面拿去显示
    stats: Stats,
}

#[derive(Serialize)]
struct ErrorEvent {
    error: String,
    /// 和错误响应 JSON 里的 `kind` 同一套分类
    kind: &'static str,
    /// 断开了（前端不用再等这条连接），还是下一轮接着试
    fatal: bool,
}

/// 一行的身份指纹，用的字段和前端 `rowKey` 一样。存 hash 不存整串：跟一小时也就几百 KB。
/// 同一毫秒、同一线程打出一模一样内容的两行会撞，撞了就是少推一行——比重复刷屏好。
fn fingerprint(r: &LogRow) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    r.ts_ms.hash(&mut h);
    r.host.hash(&mut h);
    r.file.hash(&mut h);
    r.thread.hash(&mut h);
    r.logger.hash(&mut h);
    r.message.hash(&mut h);
    h.finish()
}

/// 一次查询的结果：没推过的行（时间升序）、这批一共回了多少行、里面最大的时间戳。
struct Fetched {
    fresh: Vec<LogRow>,
    stats: Stats,
    returned: usize,
    max_ts: Option<i64>,
}

/// 一条跟随连接的状态。
struct Tail {
    state: AppState,
    /// 建连时的表结构快照：跟随期间列变了不影响这条流，重连时自然拿到新的
    schema: Arc<Schema>,
    filter: LogFilter,
    limit: u32,
    interval: Duration,
    /// 窗口起点，首轮从这里往后找最新的一屏
    from_ms: i64,
    /// 已经推到哪一毫秒（含）。下一轮从这里往后查，同毫秒的重复行靠指纹去掉
    cursor_ms: i64,
    seen: HashSet<u64>,
    /// 和 `seen` 同步的淘汰队列。回看拿到的行时间戳更小，所以队列不是严格有序的，
    /// 按队头近似淘汰就够——真正兜底的是 `MAX_SEEN`
    seen_order: VecDeque<(i64, u64)>,
    /// 首轮（取最新一屏）还没做
    priming: bool,
    last_lookback: Instant,
    /// 上一轮拿满了一批，说明还有积压，下一轮不睡
    catch_up: u32,
    errors: u32,
    /// 已经发过致命错误，这条流到此为止
    done: bool,
    /// 握着名额，连接断了自动还回去
    _permit: OwnedSemaphorePermit,
}

impl Tail {
    /// 查一段窗口 `[from_ms, to_ms)`，过掉推过的行。
    async fn fetch(&mut self, from_ms: i64, to_ms: i64, order: Order) -> Result<Fetched> {
        let mut filter = self.filter.clone();
        filter.range = Some(TimeRange { from_ms: from_ms.max(0), to_ms });
        let queries =
            LogQueries { database: &self.state.config.database, table: &self.schema.logs };
        let query = queries.search(&filter, order, self.limit, 0)?;
        let res = self.state.client.rows::<LogRow>(query).await?;
        let returned = res.rows.len();
        let max_ts = res.rows.iter().map(|r| r.ts_ms).max();
        let mut fresh: Vec<LogRow> = Vec::new();
        for row in res.rows {
            if self.mark(&row) {
                fresh.push(row);
            }
        }
        // 倒序查（首轮 / 回看）拿到的是最新的一批，推下去按时间升序，页面上就是一条条往下打
        fresh.sort_by_key(|r| r.ts_ms);
        Ok(Fetched { fresh, stats: res.stats, returned, max_ts })
    }

    /// 记下这一行；返回 true 表示以前没推过。
    fn mark(&mut self, r: &LogRow) -> bool {
        let fp = fingerprint(r);
        if !self.seen.insert(fp) {
            return false;
        }
        self.seen_order.push_back((r.ts_ms, fp));
        true
    }

    /// 丢掉回看窗口以外的指纹：那些行不会再出现在后面任何一次查询里。
    fn prune(&mut self) {
        let floor = self.cursor_ms - LOOKBACK_MS;
        while let Some(&(ts, fp)) = self.seen_order.front() {
            if ts >= floor && self.seen_order.len() <= MAX_SEEN {
                break;
            }
            self.seen.remove(&fp);
            self.seen_order.pop_front();
        }
    }

    /// 走一轮，返回这一轮要推的行（可能是空的）。
    async fn step(&mut self) -> Result<Option<RowsEvent>> {
        let now = self.state.now_ms();
        if self.priming {
            // 首轮给窗口里最新的一屏，而不是从窗口头部补历史：跟随要的是「从现在往下滚」
            let f = self.fetch(self.from_ms, now.max(self.from_ms + 1), Order::Desc).await?;
            self.priming = false;
            // 首轮本来就取的是最新一屏，没有积压要追，下一轮按正常节奏来
            self.catch_up = 0;
            self.cursor_ms = f.max_ts.unwrap_or(self.from_ms).max(self.cursor_ms);
            self.prune();
            return Ok(self.event(f.fresh, f.stats));
        }

        let f = self.fetch(self.cursor_ms, now.max(self.cursor_ms + 1), Order::Asc).await?;
        let full = f.returned as u32 >= self.limit;
        match f.max_ts {
            Some(ts) if ts > self.cursor_ms => self.cursor_ms = ts,
            // 同一毫秒里的行比一批还多：再从这一毫秒查还是这批，游标得硬往前挪一格，
            // 代价是那一毫秒剩下的行跟随看不到（翻页能看到）
            Some(ts) if full => {
                tracing::warn!(ts, limit = self.limit, "同一毫秒的日志超过一批，跟随跳过剩下的");
                self.cursor_ms = ts + 1;
            }
            _ => {}
        }
        self.catch_up = if full { self.catch_up + 1 } else { 0 };

        let mut rows = f.fresh;
        let mut stats = f.stats;
        // 积压还没吐完时先别回看，把游标推上去要紧
        if !full && self.last_lookback.elapsed() >= LOOKBACK_EVERY {
            self.last_lookback = Instant::now();
            let from = self.cursor_ms - LOOKBACK_MS;
            if from < self.cursor_ms && self.cursor_ms > 0 {
                let late = self.fetch(from, self.cursor_ms, Order::Desc).await?;
                stats.absorb(&late.stats);
                if !late.fresh.is_empty() {
                    tracing::debug!(count = late.fresh.len(), "跟随补到晚落库的行");
                    rows.extend(late.fresh);
                    rows.sort_by_key(|r| r.ts_ms);
                }
            }
        }
        self.prune();
        Ok(self.event(rows, stats))
    }

    fn event(&self, rows: Vec<LogRow>, stats: Stats) -> Option<RowsEvent> {
        (!rows.is_empty()).then_some(RowsEvent { rows, stats })
    }
}

/// 拿下一条要发的事件；`None` = 结束这条流。没有新行时不发东西，靠 keep-alive 撑着连接。
async fn next_event(t: &mut Tail) -> Option<Event> {
    if t.done {
        return None;
    }
    loop {
        if t.catch_up == 0 || t.catch_up > MAX_CATCH_UP {
            t.catch_up = 0;
            tokio::time::sleep(t.interval).await;
        }
        match t.step().await {
            Ok(None) => continue,
            Ok(Some(batch)) => {
                t.errors = 0;
                // id 就是游标：EventSource 重连时会把它放进 Last-Event-ID，接着这里续
                let event = Event::default().event("rows").id(t.cursor_ms.to_string());
                match event.json_data(&batch) {
                    Ok(e) => return Some(e),
                    Err(e) => {
                        tracing::error!(error = %e, "跟随事件序列化失败");
                        return None;
                    }
                }
            }
            Err(e) => {
                t.errors += 1;
                // 出错就回到正常节奏，别在追积压的状态下连着重试
                t.catch_up = 0;
                // 参数类的错误重试多少次都一样（比如正则写错），直接断；库超时 / 抖动就再等一轮
                let fatal = e.kind() == "bad_request" || t.errors >= MAX_ERRORS;
                tracing::warn!(error = %e, fatal, "跟随查询失败");
                t.done = fatal;
                let body = ErrorEvent { error: e.user_message(), kind: e.kind(), fatal };
                // 事件名不能叫 error：浏览器的 EventSource 把连接自身的故障也派发成 error，
                // 两者混在同一个监听器里分不清
                return Event::default().event("query_error").json_data(&body).ok();
            }
        }
    }
}

async fn tail(State(state): State<AppState>, headers: HeaderMap, p: Params) -> Result<Response> {
    let schema = state.schema.get().await?;
    let filter = build_filter(&state, &schema, &p)?;
    let Some(range) = filter.range else {
        return Err(Error::bad_request("跟随需要时间范围（from / to）"));
    };
    let limit = p.get_limit("limit", 500, state.config.max_rows)?;
    // 名额是连接级的，握到断开为止，所以不排队：满了直接回 503，让人知道现在跟不了
    let permit = Arc::clone(&state.tails)
        .try_acquire_owned()
        .map_err(|_| Error::TooManyTails(state.config.max_tail_streams))?;

    // 断线重连：浏览器把上一条事件的 id（游标毫秒）放在 Last-Event-ID 里带回来，接着那儿续
    let now = state.now_ms();
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|ts| *ts >= range.from_ms && now - *ts <= RESUME_MAX_GAP_MS);

    let interval = state.config.tail_interval;
    let mut tail = Tail {
        state,
        schema,
        filter,
        limit,
        interval,
        from_ms: range.from_ms,
        cursor_ms: resume.unwrap_or(range.from_ms),
        seen: HashSet::new(),
        seen_order: VecDeque::new(),
        priming: resume.is_none(),
        last_lookback: Instant::now(),
        catch_up: 0,
        errors: 0,
        done: false,
        _permit: permit,
    };
    // 首轮不等间隔，连上就给一屏
    tail.catch_up = 1;

    let hello = Event::default()
        .event("hello")
        .json_data(Hello {
            cursor_ms: tail.cursor_ms,
            interval_ms: interval.as_millis() as u64,
            lookback_ms: LOOKBACK_MS,
            limit,
            resumed: resume.is_some(),
        })
        .map_err(Error::internal)?;

    let events = stream::once(async move { hello })
        .chain(stream::unfold(tail, |mut t| async move {
            let event = next_event(&mut t).await?;
            Some((event, t))
        }))
        .map(Ok::<Event, std::convert::Infallible>);

    Ok(sse_response(events))
}

fn sse_response<S>(events: S) -> Response
where
    S: Stream<Item = std::result::Result<Event, std::convert::Infallible>> + Send + 'static,
{
    let mut response =
        Sse::new(events).keep_alive(KeepAlive::new().interval(KEEP_ALIVE)).into_response();
    // nginx 默认会把上游响应攒起来再发，SSE 得明确关掉，不然事件卡在代理里
    response.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}
