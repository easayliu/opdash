import { useCallback, useMemo } from 'react'
import { Link, useNavigate } from 'react-router'
import { useMeta, useTraceHeatmap, useTraceSearch } from '@/api/queries'
import type { Params } from '@/api/client'
import type { TraceSummary } from '@/api/types'
import { Heatmap, type HeatCellRange } from '@/components/charts/Heatmap'
import { StatsLine } from '@/components/StatsLine'
import { TraceFilters, type TraceFilterState } from '@/components/TraceFilters'
import { Badge, Button, EmptyState, ErrorBox, Spinner } from '@/components/ui'
import { formatDuration, formatTsMicro, writeRange } from '@/lib/time'
import { logsHref, metricsHref, serviceHref } from '@/lib/links'
import { splitList, useTimeRange, useUrlState } from '@/lib/url-state'
import { useIsMobile } from '@/lib/media'

/** 详情页带上开始时间，服务端只查附近分区 */
function traceHref(t: TraceSummary): string {
  return `/traces/${t.trace_id}?at=${Math.floor(t.start_us / 1000)}`
}

export function TracesPage() {
  const meta = useMeta()
  const navigate = useNavigate()
  const { params, set, setParams } = useUrlState()
  const { range, setRange } = useTimeRange()
  const isMobile = useIsMobile()

  const filter: TraceFilterState = useMemo(
    () => ({
      service: params.get('service') ?? '',
      span_name: params.get('span_name') ?? '',
      kinds: splitList(params.get('kind')),
      error_only: params.get('error_only') === '1',
      min_ms: params.get('min_ms') ?? '',
      max_ms: params.get('max_ms') ?? '',
      attrs: params.getAll('attr'),
      sort: params.get('sort') === 'duration' ? 'duration' : 'time',
    }),
    [params],
  )
  const setFilter = useCallback(
    (next: TraceFilterState) => {
      setParams((prev) => {
        const p = new URLSearchParams(prev)
        const put = (k: string, v: string | null) => (v ? p.set(k, v) : p.delete(k))
        put('service', next.service)
        put('span_name', next.span_name)
        put('kind', next.kinds.join(','))
        put('error_only', next.error_only ? '1' : null)
        put('min_ms', next.min_ms)
        put('max_ms', next.max_ms)
        put('sort', next.sort === 'duration' ? 'duration' : null)
        p.delete('attr')
        for (const a of next.attrs) p.append('attr', a)
        return p
      })
    },
    [setParams],
  )
  const limit = Number(params.get('limit')) || 50
  const searchParams: Params = useMemo(
    () => ({
      from: range.fromMs,
      to: range.toMs,
      service: filter.service,
      span_name: filter.span_name,
      kind: filter.kinds.join(','),
      error_only: filter.error_only ? 1 : undefined,
      min_ms: filter.min_ms,
      max_ms: filter.max_ms,
      attr: filter.attrs,
      sort: filter.sort,
      limit,
    }),
    [range.fromMs, range.toMs, filter, limit],
  )
  const search = useTraceSearch(searchParams, meta.isSuccess)
  const traces = search.data?.traces ?? []
  // 热力图和检索同一套条件，但不分页也不排序：在库里聚合，画的是范围内的全部
  const heatParams: Params = useMemo(() => {
    const { limit: _limit, sort: _sort, ...rest } = searchParams
    return rest
  }, [searchParams])
  const heat = useTraceHeatmap(heatParams, meta.isSuccess)
  // 点一格：时间缩到这个桶，耗时限定到这一档，列表就是这格里的链路
  const onCellClick = useCallback(
    (cell: HeatCellRange) => {
      setParams((prev) => {
        const p = new URLSearchParams(prev)
        writeRange(p, { fromMs: cell.fromMs, toMs: cell.toMs, relative: null })
        p.set('min_ms', String(Number(cell.minMs.toPrecision(4))))
        p.set('max_ms', String(Number(cell.maxMs.toPrecision(4))))
        return p
      })
    },
    [setParams],
  )
  const heatSubject = filter.kinds.length ? '匹配的 span' : '入口 span（Server / Consumer）'

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <TraceFilters state={filter} rangeParams={{ from: range.fromMs, to: range.toMs }} onChange={setFilter} />
      {filter.service && (
        <div className="flex flex-wrap items-center gap-2 border-b border-border bg-card px-3 py-1.5 md:px-4">
          <span className="text-2xs text-muted-fg">{filter.service} 这段时间的</span>
          {meta.data?.metrics && (
            <Link to={metricsHref(filter.service, { fromMs: range.fromMs, toMs: range.toMs })}>
              <Button size="xs">指标看板</Button>
            </Link>
          )}
          <Link
            to={logsHref(
              { dim: meta.data?.logs.dimensions.includes('service_name') ? 'service_name' : 'container', service: filter.service, levels: 'ERROR,WARN' },
              { fromMs: range.fromMs, toMs: range.toMs },
            )}
          >
            <Button size="xs">错误日志</Button>
          </Link>
          <Link to={serviceHref(filter.service, { fromMs: range.fromMs, toMs: range.toMs })}>
            <Button size="xs">服务概览</Button>
          </Link>
        </div>
      )}
      <section className="border-b border-border bg-card px-3 pt-2 pb-1.5 md:px-4 md:pt-3 md:pb-2">
        {heat.isError && <ErrorBox error={heat.error} onRetry={() => heat.refetch()} />}
        <Heatmap
          fromMs={range.fromMs}
          toMs={range.toMs}
          widthMs={heat.data?.width_ms ?? Math.max(1, (range.toMs - range.fromMs) / 120)}
          binsPerDecade={heat.data?.bins_per_decade ?? 4}
          cells={heat.data?.cells ?? []}
          height={isMobile ? 130 : 200}
          stale={heat.isFetching}
          onBrush={(fromMs, toMs) => setRange({ fromMs, toMs, relative: null })}
          onCellClick={onCellClick}
        />
        <div className="mt-1 flex items-center justify-between gap-2 text-2xs text-muted-fg">
          <span>
            <span className="hidden md:inline">纵轴为耗时（对数刻度），每格是一个时间桶里落在这一档的{heatSubject}数：</span>
            <span className="mx-0.5 inline-block size-2.5 rounded-sm align-middle" style={{ background: 'var(--chart-1)' }} /> 越深越多，
            <span className="mx-0.5 inline-block size-2.5 rounded-sm align-middle" style={{ background: 'var(--level-error)' }} /> 偏红表示错误占比高
            {heat.data && `；共 ${heat.data.total.toLocaleString('zh-CN')} 个`}
            <span className="hidden md:inline">。拖一段缩小时间，点一格只看这一档。</span>
          </span>
          <StatsLine stats={heat.data?.stats} className="hidden text-2xs text-muted-fg sm:inline" />
        </div>
      </section>
      <div className="flex items-center gap-3 border-b border-border bg-card px-3 py-2 text-xs text-muted-fg md:px-4">
        {search.data && (
          <span className="font-medium text-fg">
            {traces.length} 条链路{traces.length >= limit && `（只取前 ${limit} 条，${filter.sort === 'duration' ? '最慢在前' : '最新在前'}）`}
          </span>
        )}
        {search.data && <StatsLine stats={search.data.stats} className="hidden text-2xs text-muted-fg sm:inline" />}
        {search.isFetching && <Spinner className="size-4" />}
        <span className="ml-auto flex items-center gap-2">
          每页
          {[50, 100, 200].map((n) => (
            <button key={n} type="button" onClick={() => set({ limit: n === 50 ? null : n })} className={n === limit ? 'font-semibold text-accent' : 'hover:text-fg'}>
              {n}
            </button>
          ))}
        </span>
      </div>
      <div className="min-h-0 flex-1 overflow-auto bg-card">
        {search.isError && <ErrorBox error={search.error} onRetry={() => search.refetch()} />}
        {search.isPending && !search.isError && (
          <div className="flex justify-center py-16">
            <Spinner />
          </div>
        )}
        {search.data && !traces.length && (
          <EmptyState
            title="没有匹配的链路"
            hint={
              filter.service
                ? '试试放宽时间范围、去掉耗时 / 属性条件；如果刚发生，采集有几秒延迟。'
                : '不选服务时只能查最近 6 小时内的链路；选一个服务可以查更长的范围。'
            }
          />
        )}
        {traces.length > 0 && isMobile && (
          <ul className="text-xs">
            {traces.map((t) => (
              <li key={t.trace_id} className="row-hover cursor-pointer border-b border-border/60 px-3 py-2" onClick={() => navigate(traceHref(t))}>
                <div className="flex items-center gap-2">
                  <span className="mono text-2xs text-muted-fg tabular-nums">{formatTsMicro(t.start_us, { date: false }).slice(0, 12)}</span>
                  <span className="min-w-0 flex-1 truncate">
                    <span className="text-muted-fg">{t.root_service}</span> <span className="font-medium">{t.root_name}</span>
                  </span>
                  <span className="shrink-0 font-semibold tabular-nums">{formatDuration(t.duration_ns)}</span>
                </div>
                <div className="mt-1 flex items-center gap-2 text-2xs text-muted-fg">
                  {t.error_count > 0 && <Badge tone="danger">{t.error_count} 错误</Badge>}
                  {t.root_missing && (
                    <Badge tone="warn" title="没找到根 span，显示的是最早的那个 span">
                      根缺失
                    </Badge>
                  )}
                  <span className="tabular-nums">{t.span_count} span</span>
                  <span className="min-w-0 flex-1 truncate" title={t.services.join(', ')}>
                    {t.services.join(', ')}
                  </span>
                  <span className="mono shrink-0 text-accent">{t.trace_id.slice(0, 8)}…</span>
                </div>
              </li>
            ))}
          </ul>
        )}
        {traces.length > 0 && !isMobile && (
          <table className="w-full table-fixed border-collapse text-xs">
            <thead className="sticky top-0 bg-card text-2xs text-muted-fg shadow-[inset_0_-1px_0_var(--border)]">
              <tr>
                <th className="w-52 px-3 py-2 text-left font-medium">开始时间</th>
                <th className="px-3 py-2 text-left font-medium">入口（根 span）</th>
                <th className="w-28 px-3 py-2 text-right font-medium">请求耗时</th>
                <th className="w-28 px-3 py-2 text-right font-medium" title="最早 span 开始到最晚 span 结束（含异步消费）">
                  总跨度
                </th>
                <th className="w-20 px-3 py-2 text-right font-medium">span</th>
                <th className="w-20 px-3 py-2 text-right font-medium">错误</th>
                <th className="w-80 px-3 py-2 text-left font-medium">涉及服务</th>
                <th className="w-36 px-3 py-2 text-left font-medium">trace id</th>
              </tr>
            </thead>
            <tbody>
              {traces.map((t) => (
                <tr key={t.trace_id} className="row-hover cursor-pointer border-b border-border/60" onClick={() => navigate(traceHref(t))}>
                  <td className="mono px-3 py-2 whitespace-nowrap text-muted-fg tabular-nums">{formatTsMicro(t.start_us).slice(0, 23)}</td>
                  <td className="truncate px-3 py-2">
                    <span className="text-muted-fg">{t.root_service}</span> <span className="font-medium">{t.root_name}</span>
                    {t.root_missing && (
                      <Badge tone="warn" className="ml-1" title="没找到根 span，显示的是最早的那个 span">
                        根缺失
                      </Badge>
                    )}
                  </td>
                  <td className="px-3 py-2 text-right font-semibold tabular-nums">{formatDuration(t.duration_ns)}</td>
                  <td className="px-3 py-2 text-right text-muted-fg tabular-nums">{formatDuration(t.span_ns)}</td>
                  <td className="px-3 py-2 text-right tabular-nums">{t.span_count}</td>
                  <td className="px-3 py-2 text-right tabular-nums">{t.error_count > 0 ? <Badge tone="danger">{t.error_count}</Badge> : <span className="text-muted-fg">0</span>}</td>
                  <td className="truncate px-3 py-2 text-muted-fg" title={t.services.join(', ')}>
                    {t.services.join(', ')}
                  </td>
                  <td className="mono px-3 py-2 text-2xs">
                    <Link to={traceHref(t)} className="text-accent hover:underline" onClick={(e) => e.stopPropagation()}>
                      {t.trace_id.slice(0, 12)}…
                    </Link>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  )
}
