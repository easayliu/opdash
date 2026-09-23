import { useEffect, useMemo, useRef, useState } from 'react'
import { Link, useParams } from 'react-router'
import { useMeta, useSpanAttrs, useTraceDetail, useTraceLogs } from '@/api/queries'
import type { LogRow, TraceDetailResponse } from '@/api/types'
import { LEVEL_RANK, LogTable, sortLogRows, type ColFilter, type LogSort } from '@/components/LogTable'
import { StatsLine } from '@/components/StatsLine'
import { Badge, Button, Combobox, CopyButton, EmptyState, ErrorBox, Hint, InfoHint, Spinner, buttonClass, linkClass } from '@/components/ui'
import { AnimatePresence } from 'motion/react'
import { SpanPanel, Waterfall, buildTree, rootCauseSpan } from '@/components/Waterfall'
import { ColorAssigner } from '@/lib/colors'
import { WINDOW_AROUND_MS, around, logsHref, metricsHref, serviceHref } from '@/lib/links'
import { formatDuration, formatTs, formatTsMicro } from '@/lib/time'
import { useFromState, useUrlState } from '@/lib/url-state'
import { cn } from '@/lib/utils'
import { usePageTitle } from '@/lib/title'

/** 日志窗口在 span 跨度之外前后各放宽多少：给时钟偏差和写入延迟留余量 */
const LOG_WINDOW_PAD_MS = 5 * 60_000

/**
 * 「已截断」徽标上的解释。两种截断说的不是一回事：
 *
 * * `narrowed`：span 太多，服务端退回了围着 `at` 的窄窗口——图上是**完整的一段时间**，
 *   只是这条 trace 在这段之外还有（多半是被复用的 trace id）；
 * * 否则：第一档窗口就装不下，按时间切了前 N 个。
 */
function truncatedHint(d: TraceDetailResponse, max = 5000): string {
  const span =
    d.window_from_ms != null && d.window_to_ms != null
      ? `${formatTs(d.window_from_ms, { ms: false })} ~ ${formatTs(d.window_to_ms, { ms: false, date: false })}`
      : ''
  const pinned = d.pinned_span ? `；链接指名的 ${d.pinned_span} 不在这一段里，已单独取回来钉在图上` : ''
  return d.narrowed
    ? `这条 trace 的 span 超过 ${max} 个，只显示 ${span} 这一段（从进来的时刻往后尽量长）${pinned}`
    : `超过 ${max} 个 span，按时间只显示最早的这些${pinned}`
}

/** 这一行是否过得了各列的筛选；`except` 那一列不算（给它自己算下拉选项时用） */
function matchDims(r: LogRow, dims: string[], filter: Record<string, string>, except?: string): boolean {
  return dims.every((d) => d === except || !filter[d] || String(r[d] ?? '') === filter[d])
}

export function TraceDetailPage() {
  const { traceId = '' } = useParams<{ traceId: string }>()
  const meta = useMeta()
  const { params, set } = useUrlState()
  // 从哪个列表点进来的。直接粘 URL 进来的没有来处，就不显示返回——见 useFromState
  const from = useFromState()
  const selected = params.get('span')
  /**
   * 进页面时 URL 上指名的那个 span（分享的链接、错误分组给的样本），交给服务端钉进结果里。
   *
   * 超过 `--max-trace-spans` 的 trace 截断是按时间从早往晚切的，指名的那个——尤其是出错的
   * 那个——常常正好在被切掉的后半段：链接打开只剩一张瀑布图，点不中人专程来看的这一条。
   *
   * 记在 ref 里只认第一次：跟着当前选中走的话，点一下瀑布图就换了 query key，整条详情重查一遍。
   */
  const pinned = useRef<{ trace: string; span: string | null } | null>(null)
  if (pinned.current?.trace !== traceId) pinned.current = { trace: traceId, span: selected }
  const detail = useTraceDetail(traceId, params.get('at'), pinned.current.span)
  const logsOnlySpan = params.get('span_logs') === '1'
  const [showLogs, setShowLogs] = useState(params.get('tab') !== 'none')

  const spans = detail.data?.spans ?? []
  const tree = useMemo(() => buildTree(spans), [spans])
  const colors = useMemo(() => {
    const c = new ColorAssigner()
    // 按 span 开始顺序分配：根服务拿第一个颜色
    for (const s of [...spans].sort((a, b) => a.start_us - b.start_us)) c.color(s.service)
    return c
  }, [spans])
  const selectedSpan = spans.find((s) => s.span_id === selected) ?? null
  // 属性 / events / links 是点开这个 span 才查的（详情那一趟不带这四个 JSON 列，见 useSpanAttrs）
  const attrs = useSpanAttrs(
    traceId,
    detail.data?.attributes_lazy ? selected : null,
    selectedSpan
      ? { service: selectedSpan.service, name: selectedSpan.name, ts_ms: Math.floor(selectedSpan.start_us / 1000) }
      : undefined,
    params.get('at'),
  )
  const attrsFor = attrs.data?.span_id === selected ? attrs.data : undefined
  const selectedSpanFull = useMemo(() => {
    if (!selectedSpan) return null
    if (!detail.data?.attributes_lazy) return selectedSpan
    if (!attrsFor) return { ...selectedSpan, attributes: {}, resource: {}, events: [], links: [] }
    return {
      ...selectedSpan,
      attributes: attrsFor.attributes,
      resource: attrsFor.resource,
      events: attrsFor.events,
      links: attrsFor.links,
    }
  }, [selectedSpan, detail.data?.attributes_lazy, attrsFor])
  const root = tree.roots[0]?.span
  usePageTitle(root && `${root.service} ${root.name}`)
  const errorSpans = useMemo(() => spans.filter((s) => s.status === 'Error').sort((a, b) => a.start_us - b.start_us), [spans])
  const errors = errorSpans.length
  // 点顶部的错误数：选中下一个出错的 span（从当前选中的往后数，到头再绕回第一个），瀑布图会滚过去
  const jumpToError = () => {
    if (!errors) return
    const idx = errorSpans.findIndex((s) => s.span_id === selected)
    const next = errorSpans[(idx + 1) % errors]
    set({ span: next.span_id, span_logs: null }, { replace: true })
  }

  /**
   * 出错的链路打开就直接选中抛异常的那个 span，右边面板上就是 exception 的类型和堆栈。
   *
   * 不这么做的话，从「错误日志」「出错的链路」点进来，落地是一张瀑布图，还要自己在几十上百行
   * 里找红条、点开、翻到 events——报错本身明明是来这一趟唯一想看的东西。
   *
   * 只在本条 trace 第一次加载出来时做一次：之后人自己关掉面板、点了别的 span，就不再插手
   * （`picked` 记的是已经替哪条 trace 选过了）。URL 里带了 `span=` 的（别人分享的链接、
   * 从错误分组点过来的样本）也不覆盖。
   */
  const picked = useRef<string | null>(null)
  useEffect(() => {
    if (picked.current === traceId || !spans.length) return
    picked.current = traceId
    if (selected) return
    const cause = rootCauseSpan(tree)
    if (cause) set({ span: cause.span_id }, { replace: true })
  }, [traceId, spans.length, tree, selected, set])

  const at = Number(params.get('at')) || undefined
  /**
   * 日志窗口按这条 trace 的 span 实际跨度圈，等详情回来再查。
   *
   * 日志表按 trace id 找靠的是 bloom filter，读量跟窗口里的**真实数据量**成正比。以前圈的是
   * `at` 前 1 小时、后 24 小时，看今天的 trace 不花钱（+24 小时还没发生），看昨天的就是
   * 32.7 M 行 / 145.5 MB —— 为了拿 40 条日志。按 span 跨度前后各放宽 5 分钟只要 0.76 M 行 /
   * 26.0 MB。
   *
   * 会不会漏：线上比对了 717 条 trace 的日志时间和 span 时间，日志最晚比 span 最晚晚 2 ms
   * （中位），93.3% 的 trace 一条都不漏；漏的那些全是 trace id 被复用的（同一个 id 挂着两小时
   * 的日志），而那些日志本来就不属于用户正在看的这一次请求。
   *
   * 代价是日志要等详情那一趟（以前两条并行）。墙钟基本没变：详情本来就比日志慢。
   */
  const logWindow = useMemo<{ from?: number; to?: number } | undefined>(() => {
    // 还在等详情：这时候查等于用旧办法圈一个大窗
    if (detail.isPending) return undefined
    const found = detail.data?.spans ?? []
    if (found.length) {
      let from = Infinity
      let to = -Infinity
      for (const s of found) {
        from = Math.min(from, s.start_us / 1000)
        to = Math.max(to, (s.start_us + s.duration_ns / 1000) / 1000)
      }
      return { from: Math.floor(from - LOG_WINDOW_PAD_MS), to: Math.ceil(to + LOG_WINDOW_PAD_MS) }
    }
    // span 表里没有这条 trace（采样掉了、过了 TTL，或者只有日志打了 TID）：退回 at 前后一大片；
    // 连 at 都没有才不带时间条件——日志表的 `idx_trace_id` 误判率 2.5%，摊到 30 天只剪掉九成七，
    // 这一趟实测要 10.4 亿行 / 5.4 GB / 14 s，是真没别的线索了才走
    return at ? { from: at - 3_600_000, to: at + 24 * 3_600_000 } : {}
  }, [detail.isPending, detail.data, at])
  // 一条 trace 的日志全拉下来，排序和「只看某个 span」都在浏览器里做
  const logs = useTraceLogs(
    { trace_id: traceId, ...logWindow },
    meta.data?.limits,
    showLogs && !!traceId && !!logWindow,
  )
  const [sort, setSort] = useState<LogSort>({ key: 'ts_ms', dir: 'asc' })
  const onSort = (key: string) => setSort((s) => ({ key, dir: s.key === key && s.dir === 'asc' ? 'desc' : 'asc' }))
  const sortedLogs = useMemo(() => (logs.data ? sortLogRows(logs.data.rows, sort) : []), [logs.data, sort])
  const selectedLogCount = useMemo(() => (selected ? sortedLogs.filter((r) => r.span_id === selected).length : 0), [sortedLogs, selected])
  // 「只看选中 span」在本地筛：整条 trace 的日志已经在手里了，为它再查一趟库是白扫一遍
  const spanLogs = useMemo(
    () => (logsOnlySpan && selected ? sortedLogs.filter((r) => r.span_id === selected) : sortedLogs),
    [sortedLogs, logsOnlySpan, selected],
  )
  const dims = meta.data?.logs.dimensions ?? []
  /**
   * 表头上按级别 / 服务 / pod 筛，也在本地做。一条跨十几个服务的 trace 有上百条日志，人往往只想看
   * 其中一个服务（或者滚动发布时只看新 pod）打了什么、只看 WARN 以上。选中值记在 URL 里，
   * 链接发给同事看到的一样。
   */
  // 日志表上「服务」这一维叫什么（老表没有 service_name 就退回 container）
  const serviceDim = dims.includes('service_name') ? 'service_name' : dims.includes('container') ? 'container' : null
  const filterDims = useMemo(
    () => ['level', serviceDim, dims.includes('pod') ? 'pod' : null].filter((d): d is string => !!d),
    [dims, serviceDim],
  )
  // 每列当前选中的值（'' 是不筛）。params 每次渲染都是新对象，按值拼一个 key 让下面的 memo 稳定
  const filterKey = filterDims.map((d) => params.get(`log_${d}`) ?? '').join('\u0000')
  const dimFilter = useMemo(() => {
    const vals = filterKey.split('\u0000')
    return Object.fromEntries(filterDims.map((d, i) => [d, vals[i] ?? '']))
  }, [filterDims, filterKey])
  const shownLogs = useMemo(() => spanLogs.filter((r) => matchDims(r, filterDims, dimFilter)), [spanLogs, filterDims, dimFilter])
  const colFilters = useMemo(() => {
    const out: Record<string, ColFilter> = {}
    for (const d of filterDims) {
      // 选项和条数按「其他列的筛选都生效」算：选了服务之后 pod 的下拉只剩这个服务的 pod
      const counts = new Map<string, number>()
      for (const r of spanLogs) {
        if (!matchDims(r, filterDims, dimFilter, d)) continue
        const v = String(r[d] ?? '')
        counts.set(v, (counts.get(v) ?? 0) + 1)
      }
      // 级别按严重程度排，其余按条数多的在前
      const rank = (v: string) => (d === 'level' ? (LEVEL_RANK[v.toUpperCase()] ?? 9) : 0)
      const options = [...counts]
        .sort((a, b) => rank(a[0]) - rank(b[0]) || b[1] - a[1] || a[0].localeCompare(b[0]))
        .map(([value, n]) => ({ value, label: value || '（空）', note: `${n} 条` }))
      out[d] = { value: dimFilter[d], options, onChange: (v) => set({ [`log_${d}`]: v || null }, { replace: true }) }
    }
    return out
  }, [spanLogs, filterDims, dimFilter, set])
  const dimFiltered = filterDims.some((d) => dimFilter[d])
  // 页头的服务图例点一下就是按这个服务筛日志（再点取消），比在表头下拉里找快
  const serviceFilter = serviceDim ? dimFilter[serviceDim] : ''
  const toggleServiceFilter = (name: string) => {
    if (!serviceDim) return
    setShowLogs(true)
    set({ [`log_${serviceDim}`]: serviceFilter === name ? null : name }, { replace: true })
  }
  // 去日志页也把窗口带上：日志页按 id 查默认不裁时间，在这个规模的集群上会直接撞超时
  const logsRangeQuery =
    logWindow?.from && logWindow.to ? `&from=${logWindow.from}&to=${logWindow.to}` : ''

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-x-6 gap-y-1.5 border-b border-border bg-card px-3 py-2.5 md:px-4 md:py-3">
        {from && (
          <Link to={from.href} className="shrink-0 text-sm text-muted-fg hover:text-fg">
            ← {from.label}
          </Link>
        )}
        <div className="min-w-0 max-w-full">
          <h1 className="flex min-w-0 items-center gap-2 text-base font-semibold">
            {root ? (
              <>
                <span className="text-muted-fg">{root.service}</span>
                <span className="truncate">{root.name}</span>
                {tree.roots.length > 1 && <Badge tone="muted">{tree.roots.length} 个顶层 span</Badge>}
              </>
            ) : (
              '链路详情'
            )}
          </h1>
          <div className="mono mt-0.5 flex min-w-0 items-center gap-1.5 text-2xs text-muted-fg">
            <span className="truncate">{traceId}</span>
            <CopyButton text={traceId} title="复制 trace id" />
            <CopyButton text={() => window.location.href} title="复制本页链接（带当前选中的 span）" className="ml-2" label="复制链接" />
          </div>
        </div>
        {detail.data && spans.length > 0 && (
          <dl className="flex flex-wrap items-center gap-x-4 gap-y-1 text-sm md:gap-6">
            <div>
              <dt className="text-2xs text-muted-fg">开始</dt>
              <dd className="tabular-nums">{formatTsMicro(tree.startUs)}</dd>
            </div>
            <div>
              <dt className="text-2xs text-muted-fg">请求耗时</dt>
              <dd className="font-semibold tabular-nums">{root ? formatDuration(root.duration_ns) : '-'}</dd>
            </div>
            <div>
              <dt className="text-2xs text-muted-fg">总跨度</dt>
              <dd className="tabular-nums">{formatDuration((tree.endUs - tree.startUs) * 1000)}</dd>
            </div>
            <div>
              <dt className="text-2xs text-muted-fg">span</dt>
              <dd className="tabular-nums">
                {spans.length}
                {detail.data.truncated && (
                  <Hint text={truncatedHint(detail.data, meta.data?.limits.max_trace_spans)} className="ml-1">
                    <Badge tone="warn">{detail.data.narrowed ? '只显示这一段' : '已截断'}</Badge>
                  </Hint>
                )}
              </dd>
            </div>
            <div>
              <dt className="text-2xs text-muted-fg">错误</dt>
              <dd className="tabular-nums">
                {errors > 0 ? (
                  <Hint text={errors > 1 ? '点击跳到下一个出错的 span' : '点击跳到出错的 span'} asChild>
                    <button type="button" onClick={jumpToError} className="cursor-pointer">
                      <Badge tone="danger">{errors}</Badge>
                    </button>
                  </Hint>
                ) : (
                  0
                )}
              </dd>
            </div>
          </dl>
        )}
        {/* 从这条链路跳到别的信号：时间窗以这条 trace 的开始时刻为中心前后放宽，
            「这次为什么慢」常常要看那一刻服务的 GC / 连接池 */}
        {root && (
          <div className="flex flex-wrap items-center gap-2">
            {meta.data?.metrics && (
              <Link to={metricsHref(root.service, around(tree.startUs / 1000))} className={buttonClass({ size: 'xs' })}>
                服务指标
              </Link>
            )}
            <Link to={serviceHref(root.service, around(tree.startUs / 1000))} className={buttonClass({ size: 'xs' })}>
              服务概览
            </Link>
            {/* 时间窗按这条 trace 的实际跨度前后放宽：日志页按 id 查也要裁时间（见 logsHref），
                不带窗口就会撞上它 1 小时的默认值 */}
            <Link
              to={logsHref({ traceId }, { fromMs: tree.startUs / 1000 - WINDOW_AROUND_MS, toMs: tree.endUs / 1000 + WINDOW_AROUND_MS })}
              className={buttonClass({ size: 'xs' })}
            >
              全部日志
            </Link>
          </div>
        )}
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-2xs text-muted-fg md:ml-auto md:gap-x-4">
          {colors.entries().map(([name, color]) => (
            <Hint text={serviceFilter === name ? '取消只看这个服务的日志' : `只看 ${name} 的日志`} asChild>
              <button
                key={name}
                type="button"
                disabled={!serviceDim}
                onClick={() => toggleServiceFilter(name)}
                className={cn(
                  'inline-flex cursor-pointer items-center gap-1 rounded px-1 -mx-1 hover:bg-muted hover:text-fg disabled:cursor-default disabled:hover:bg-transparent disabled:hover:text-muted-fg',
                  serviceFilter === name && 'bg-accent-soft font-medium text-accent hover:text-accent',
                  serviceFilter && serviceFilter !== name && 'opacity-60',
                )}
              >
                <span className="inline-block h-3.5 w-1 rounded-sm" style={{ background: color }} />
                {name}
              </button>
            </Hint>
          ))}
          <StatsLine stats={detail.data?.stats} className="hidden text-2xs text-muted-fg sm:inline" />
          {detail.data?.windowed && (
            <button
              type="button"
              className={linkClass}
              onClick={() => set({ at: null }, { replace: true })}
            >
              查全部时间
            </button>
          )}
          {detail.data?.windowed && <InfoHint text="只查了开始时间前 1 小时到后 24 小时的 span；怀疑有遗漏时可查全部时间（较慢）" />}
        </div>
      </header>
      <div className="flex min-h-0 flex-1">
        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          <div className="min-h-0 flex-1 overflow-auto bg-card">
            {detail.isError && <ErrorBox error={detail.error} onRetry={() => detail.refetch()} />}
            {detail.isPending && (
              <div className="flex justify-center py-16">
                <Spinner />
              </div>
            )}
            {detail.data && !spans.length && (
              <EmptyState
                title="没有这条 trace 的 span"
                hint={
                  <>
                    可能还没入库（采集有几秒延迟）、被采样掉了，或者已经超过 30 天。可以看看有没有<Link to={`/logs?trace_id=${traceId}${logsRangeQuery}`} className={linkClass}>这个 trace id 的日志</Link>。
                  </>
                }
              />
            )}
            {spans.length > 0 && <Waterfall tree={tree} colors={colors} selected={selected} onSelect={(id) => set({ span: id, span_logs: id ? undefined : null }, { replace: true })} />}
          </div>
          <section className="flex min-h-0 flex-col border-t border-border bg-card" style={{ height: showLogs ? '40%' : undefined }}>
            <div className="flex h-10 shrink-0 items-center gap-2 overflow-x-auto px-3 text-xs whitespace-nowrap md:px-4">
              <Button variant="ghost" size="xs" onClick={() => setShowLogs((v) => !v)}>
                {showLogs ? '▾' : '▸'} 关联日志
                {logs.data && ` (${shownLogs.length})`}
              </Button>
              {showLogs && selected && !logsOnlySpan && logs.data && (
                <span className="hidden text-2xs text-muted-fg md:inline">选中 span 的 {selectedLogCount} 条已高亮</span>
              )}
              {logs.data?.truncated && (
                <Hint
                  text={`库里带这个 trace id 的日志有 ${logs.data.total ?? '?'} 条，只取了最早的 ${logs.data.rows.length} 条。这么多多半是 trace id 被复用了（常驻消费者一直用同一个 id），剩下的多半跟这次请求无关——要全看去日志页`}
                >
                  <Badge tone="warn">只取了前 {logs.data.rows.length} 条</Badge>
                </Hint>
              )}
              {showLogs && selected && (
                <Button size="xs" active={logsOnlySpan} onClick={() => set({ span_logs: logsOnlySpan ? null : '1' }, { replace: true })}>
                  只看选中 span<span className="hidden md:inline"> 的日志</span>
                </Button>
              )}
              {showLogs &&
                logs.data &&
                filterDims.map((d) => (
                  // 手机上是卡片列表、没有表头，筛选放到这一行来
                  <Combobox
                    key={d}
                    value={dimFilter[d]}
                    options={colFilters[d].options}
                    onChange={colFilters[d].onChange}
                    placeholder={`全部 ${d}`}
                    searchPlaceholder={`搜索 ${d}…`}
                    className="w-32 shrink-0 md:hidden"
                    title={`按 ${d} 筛选`}
                  />
                ))}
              {showLogs && logs.isFetching && <Spinner className="size-3.5" />}
              <span className="ml-auto flex items-center gap-3 text-2xs text-muted-fg">
                <StatsLine stats={logs.data?.stats} className="hidden text-2xs text-muted-fg md:inline" />
                <Link to={`/logs?trace_id=${traceId}${logsRangeQuery}`} className={linkClass}>
                  在日志页打开
                </Link>
              </span>
            </div>
            {showLogs && (
              <div className="min-h-0 flex-1 overflow-auto border-t border-border/60">
                {logs.isError && <ErrorBox error={logs.error} />}
                {logs.data && (
                  <LogTable
                    rows={shownLogs}
                    dims={dims}
                    compact
                    selectedSpanId={logsOnlySpan ? null : selected}
                    sort={sort}
                    onSort={onSort}
                    colFilters={colFilters}
                    emptyText={
                      <span>
                        {dimFiltered
                          ? '这个筛选下没有日志。'
                          : `没有带这个 trace id 的日志。${selected && logsOnlySpan ? '试试取消「只看选中 span」。' : '日志里要打 [TID:…] 才能关联；Go / nginx 这类不打 TID 的服务这里看不到。'}`}
                        {dimFiltered && (
                          <button type="button" className={linkClass} onClick={() => set(Object.fromEntries(filterDims.map((d) => [`log_${d}`, null])), { replace: true })}>
                            清掉列上的筛选
                          </button>
                        )}
                      </span>
                    }
                  />
                )}
              </div>
            )}
          </section>
        </div>
        {/* 点开 / 关掉 span 详情：面板淡入淡出，瀑布图这边不动 */}
        <AnimatePresence initial={false}>
        {selectedSpanFull && (
          <SpanPanel
            key="span-panel"
            span={selectedSpanFull}
            loading={attrs.isFetching && !attrsFor}
            error={attrs.error}
            metricsLink={
              meta.data?.metrics ? metricsHref(selectedSpanFull.service, around(selectedSpanFull.start_us / 1000)) : undefined
            }
            traceStartUs={tree.startUs}
            onClose={() => set({ span: null, span_logs: null }, { replace: true })}
            onShowLogs={() => {
              setShowLogs(true)
              set({ span_logs: '1' }, { replace: true })
            }}
          />
        )}
        </AnimatePresence>
      </div>
    </div>
  )
}
