import { useEffect, useMemo, useState } from 'react'
import { Link, useParams } from 'react-router'
import { CopyIcon } from 'lucide-react'
import { useLogSearch, useMeta, useTraceDetail } from '@/api/queries'
import { LogTable } from '@/components/LogTable'
import { StatsLine } from '@/components/StatsLine'
import { Badge, Button, EmptyState, ErrorBox, Spinner } from '@/components/ui'
import { SpanPanel, Waterfall, buildTree } from '@/components/Waterfall'
import { ColorAssigner } from '@/lib/colors'
import { formatDuration, formatTsMicro } from '@/lib/time'
import { useUrlState } from '@/lib/url-state'
import { copyText } from '@/lib/utils'

export function TraceDetailPage() {
  const { traceId = '' } = useParams<{ traceId: string }>()
  const meta = useMeta()
  const { params, set } = useUrlState()
  const detail = useTraceDetail(traceId)
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
  const root = tree.roots[0]?.span
  const errors = spans.filter((s) => s.status === 'Error').length

  useEffect(() => {
    document.title = root ? `${root.service} ${root.name} · opdash` : 'opdash'
    return () => {
      document.title = 'opdash'
    }
  }, [root])

  const logs = useLogSearch(
    { trace_id: traceId, span_id: logsOnlySpan && selected ? selected : undefined, order: 'asc', limit: 500 },
    showLogs && meta.isSuccess && !!traceId,
  )
  const dims = meta.data?.logs.dimensions ?? []

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-x-4 gap-y-1 border-b border-border bg-card px-3 py-2">
        <div className="min-w-0">
          <div className="flex items-center gap-2 text-sm font-semibold">
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
          <div className="mono flex items-center gap-1 text-2xs text-muted-fg">
            {traceId}
            <button type="button" onClick={() => copyText(traceId)} title="复制 trace id" className="hover:text-fg">
              <CopyIcon className="size-3" />
            </button>
            <button type="button" onClick={() => copyText(window.location.href)} title="复制本页链接" className="ml-2 hover:text-fg">
              复制链接
            </button>
          </div>
        </div>
        {detail.data && spans.length > 0 && (
          <dl className="flex items-center gap-4 text-xs">
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
              <dd className="tabular-nums">{errors > 0 ? <Badge tone="danger">{errors}</Badge> : 0}</dd>
            </div>
          </dl>
        )}
        <div className="ml-auto flex flex-wrap items-center gap-x-3 gap-y-1 text-2xs text-muted-fg">
          {colors.entries().map(([name, color]) => (
            <span key={name} className="inline-flex items-center gap-1">
              <span className="inline-block h-3 w-1 rounded-sm" style={{ background: color }} />
              {name}
            </span>
          ))}
          <StatsLine stats={detail.data?.stats} />
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
            <div className="flex h-8 shrink-0 items-center gap-2 px-3 text-xs">
              <Button variant="ghost" size="xs" onClick={() => setShowLogs((v) => !v)}>
                {showLogs ? '▾' : '▸'} 关联日志
                {logs.data && ` (${logs.data.rows.length}${logs.data.rows.length >= 500 ? '+' : ''})`}
              </Button>
              {showLogs && selected && (
                <Button size="xs" active={logsOnlySpan} onClick={() => set({ span_logs: logsOnlySpan ? null : '1' }, { replace: true })}>
                  只看选中 span 的日志
                </Button>
              )}
              {showLogs && logs.isFetching && <Spinner className="size-3" />}
              <span className="ml-auto flex items-center gap-3 text-2xs text-muted-fg">
                <StatsLine stats={logs.data?.stats} />
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
                    rows={logs.data.rows}
                    dims={dims}
                    compact
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
        {selectedSpan && (
          <SpanPanel
            span={selectedSpan}
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
