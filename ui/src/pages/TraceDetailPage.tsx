import { useEffect, useMemo, useRef, useState } from 'react'
import { Link, useParams } from 'react-router'
import { CopyIcon } from 'lucide-react'
import { useMeta, useSpanAttrs, useTraceDetail, useTraceLogs } from '@/api/queries'
import { LogTable, sortLogRows, type LogSort } from '@/components/LogTable'
import { StatsLine } from '@/components/StatsLine'
import { Badge, Button, EmptyState, ErrorBox, Spinner } from '@/components/ui'
import { SpanPanel, Waterfall, buildTree, rootCauseSpan } from '@/components/Waterfall'
import { ColorAssigner } from '@/lib/colors'
import { around, logsHref, metricsHref, serviceHref } from '@/lib/links'
import { formatDuration, formatTsMicro } from '@/lib/time'
import { useFromState, useUrlState } from '@/lib/url-state'
import { copyText } from '@/lib/utils'

/** 日志窗口在 span 跨度之外前后各放宽多少：给时钟偏差和写入延迟留余量 */
const LOG_WINDOW_PAD_MS = 5 * 60_000

export function TraceDetailPage() {
  const { traceId = '' } = useParams<{ traceId: string }>()
  const meta = useMeta()
  const { params, set } = useUrlState()
  // 从哪个列表点进来的。直接粘 URL 进来的没有来处，就不显示返回——见 useFromState
  const from = useFromState()
  const detail = useTraceDetail(traceId, params.get('at'))
  const selected = params.get('span')
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

  useEffect(() => {
    document.title = root ? `${root.service} ${root.name} · opdash` : 'opdash'
    return () => {
      document.title = 'opdash'
    }
  }, [root])

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
    // 连 at 都没有就不带时间条件，让后端靠 bloom filter 扫全部分区
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
  const shownLogs = useMemo(
    () => (logsOnlySpan && selected ? sortedLogs.filter((r) => r.span_id === selected) : sortedLogs),
    [sortedLogs, logsOnlySpan, selected],
  )
  // 去日志页也把窗口带上：日志页按 id 查默认不裁时间，在这个规模的集群上会直接撞超时
  const logsRangeQuery =
    logWindow?.from && logWindow.to ? `&from=${logWindow.from}&to=${logWindow.to}` : ''
  const dims = meta.data?.logs.dimensions ?? []

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-x-6 gap-y-1.5 border-b border-border bg-card px-3 py-2.5 md:px-4 md:py-3">
        {from && (
          <Link to={from.href} className="shrink-0 text-sm text-muted-fg hover:text-fg" title={`回到${from.label}`}>
            ← {from.label}
          </Link>
        )}
        <div className="min-w-0 max-w-full">
          <div className="flex min-w-0 items-center gap-2 text-base font-semibold">
            {root ? (
              <>
                <span className="text-muted-fg">{root.service}</span>
                <span className="truncate">{root.name}</span>
                {tree.roots.length > 1 && <Badge tone="muted">{tree.roots.length} 个顶层 span</Badge>}
              </>
            ) : (
              '链路详情'
            )}
          </div>
          <div className="mono mt-0.5 flex min-w-0 items-center gap-1.5 text-2xs text-muted-fg">
            <span className="truncate">{traceId}</span>
            <button type="button" onClick={() => copyText(traceId)} title="复制 trace id" className="hover:text-fg">
              <CopyIcon className="size-3.5" />
            </button>
            <button type="button" onClick={() => copyText(window.location.href)} title="复制本页链接" className="ml-2 hover:text-fg">
              复制链接
            </button>
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
                  <Badge tone="warn" className="ml-1" title={`超过 ${meta.data?.limits.max_trace_spans ?? 5000} 个 span，只显示前面这些`}>
                    已截断
                  </Badge>
                )}
              </dd>
            </div>
            <div>
              <dt className="text-2xs text-muted-fg">错误</dt>
              <dd className="tabular-nums">
                {errors > 0 ? (
                  <button type="button" onClick={jumpToError} title={errors > 1 ? '点击跳到下一个出错的 span' : '点击跳到出错的 span'} className="cursor-pointer">
                    <Badge tone="danger">{errors}</Badge>
                  </button>
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
              <Link to={metricsHref(root.service, around(tree.startUs / 1000))} title={`${root.service} 在这前后半小时的指标`}>
                <Button size="xs">服务指标</Button>
              </Link>
            )}
            <Link to={serviceHref(root.service, around(tree.startUs / 1000))}>
              <Button size="xs">服务概览</Button>
            </Link>
            <Link to={logsHref({ traceId })} title="这条链路的全部日志">
              <Button size="xs">全部日志</Button>
            </Link>
          </div>
        )}
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-2xs text-muted-fg md:ml-auto md:gap-x-4">
          {colors.entries().map(([name, color]) => (
            <span key={name} className="inline-flex items-center gap-1">
              <span className="inline-block h-3.5 w-1 rounded-sm" style={{ background: color }} />
              {name}
            </span>
          ))}
          <StatsLine stats={detail.data?.stats} className="hidden text-2xs text-muted-fg sm:inline" />
          {detail.data?.windowed && (
            <button
              type="button"
              className="text-accent hover:underline"
              title="只查了开始时间前 1 小时到后 24 小时的 span；怀疑漏了就查全部时间（慢）"
              onClick={() => set({ at: null }, { replace: true })}
            >
              查全部时间
            </button>
          )}
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
                    可能还没入库（采集有几秒延迟）、被采样掉了，或者已经超过 30 天。可以看看有没有<Link to={`/logs?trace_id=${traceId}${logsRangeQuery}`} className="text-accent hover:underline">这个 trace id 的日志</Link>。
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
                <Badge
                  tone="warn"
                  title={`库里带这个 trace id 的日志有 ${logs.data.total ?? '?'} 条，只取了最早的 ${logs.data.rows.length} 条。这么多多半是 trace id 被复用了（常驻消费者一直用同一个 id），剩下的多半跟这次请求无关——要全看去日志页`}
                >
                  只取了前 {logs.data.rows.length} 条
                </Badge>
              )}
              {showLogs && selected && (
                <Button size="xs" active={logsOnlySpan} onClick={() => set({ span_logs: logsOnlySpan ? null : '1' }, { replace: true })}>
                  只看选中 span<span className="hidden md:inline"> 的日志</span>
                </Button>
              )}
              {showLogs && logs.isFetching && <Spinner className="size-3.5" />}
              <span className="ml-auto flex items-center gap-3 text-2xs text-muted-fg">
                <StatsLine stats={logs.data?.stats} className="hidden text-2xs text-muted-fg md:inline" />
                <Link to={`/logs?trace_id=${traceId}${logsRangeQuery}`} className="text-accent hover:underline">
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
                    emptyText={
                      <span>
                        没有带这个 trace id 的日志。{selected && logsOnlySpan ? '试试取消「只看选中 span」。' : '日志里要打 [TID:…] 才能关联；Go / nginx 这类不打 TID 的服务这里看不到。'}
                      </span>
                    }
                  />
                )}
              </div>
            )}
          </section>
        </div>
        {selectedSpanFull && (
          <SpanPanel
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
      </div>
    </div>
  )
}
