import { useEffect, useMemo, useState } from 'react'
import { Link, useParams } from 'react-router'
import { CopyIcon } from 'lucide-react'
import { useMeta, useSpanAttrs, useTraceDetail, useTraceLogs } from '@/api/queries'
import { LogTable, sortLogRows, type LogSort } from '@/components/LogTable'
import { StatsLine } from '@/components/StatsLine'
import { Badge, Button, EmptyState, ErrorBox, Spinner } from '@/components/ui'
import { SpanPanel, Waterfall, buildTree } from '@/components/Waterfall'
import { ColorAssigner } from '@/lib/colors'
import { around, logsHref, metricsHref, serviceHref } from '@/lib/links'
import { formatDuration, formatTsMicro } from '@/lib/time'
import { useUrlState } from '@/lib/url-state'
import { copyText } from '@/lib/utils'

export function TraceDetailPage() {
  const { traceId = '' } = useParams<{ traceId: string }>()
  const meta = useMeta()
  const { params, set } = useUrlState()
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

  useEffect(() => {
    document.title = root ? `${root.service} ${root.name} · opdash` : 'opdash'
    return () => {
      document.title = 'opdash'
    }
  }, [root])

  // 一条 trace 的日志全拉下来，排序在浏览器里做（点表头）。
  // 知道开始时间就圈到前 1 小时、后 24 小时：日志表按 trace id 找同样靠 bloom filter，不限时间要扫全部分区
  const at = Number(params.get('at')) || undefined
  const logs = useTraceLogs(
    {
      trace_id: traceId,
      span_id: logsOnlySpan && selected ? selected : undefined,
      from: at && at - 3_600_000,
      to: at && at + 24 * 3_600_000,
    },
    meta.data?.limits,
    showLogs && !!traceId,
  )
  const [sort, setSort] = useState<LogSort>({ key: 'ts_ms', dir: 'asc' })
  const onSort = (key: string) => setSort((s) => ({ key, dir: s.key === key && s.dir === 'asc' ? 'desc' : 'asc' }))
  const sortedLogs = useMemo(() => (logs.data ? sortLogRows(logs.data.rows, sort) : []), [logs.data, sort])
  const selectedLogCount = useMemo(() => (selected ? sortedLogs.filter((r) => r.span_id === selected).length : 0), [sortedLogs, selected])
  const dims = meta.data?.logs.dimensions ?? []

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-x-6 gap-y-1.5 border-b border-border bg-card px-3 py-2.5 md:px-4 md:py-3">
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
                    可能还没入库（采集有几秒延迟）、被采样掉了，或者已经超过 30 天。可以看看有没有<Link to={`/logs?trace_id=${traceId}`} className="text-accent hover:underline">这个 trace id 的日志</Link>。
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
                {logs.data && ` (${logs.data.rows.length})`}
              </Button>
              {showLogs && selected && !logsOnlySpan && logs.data && (
                <span className="hidden text-2xs text-muted-fg md:inline">选中 span 的 {selectedLogCount} 条已高亮</span>
              )}
              {logs.data?.truncated && (
                <Badge tone="warn" title={`翻页深度到了上限 ${meta.data?.limits.max_offset ?? ''}，库里共 ${logs.data.total ?? '?'} 条，只拉了前面这些`}>
                  未拉全
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
                <Link to={`/logs?trace_id=${traceId}`} className="text-accent hover:underline">
                  在日志页打开
                </Link>
              </span>
            </div>
            {showLogs && (
              <div className="min-h-0 flex-1 overflow-auto border-t border-border/60">
                {logs.isError && <ErrorBox error={logs.error} />}
                {logs.data && (
                  <LogTable
                    rows={sortedLogs}
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
