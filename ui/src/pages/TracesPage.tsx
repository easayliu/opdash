import { useCallback, useMemo } from 'react'
import { Link, useNavigate } from 'react-router'
import { useMeta, useTraceSearch } from '@/api/queries'
import type { Params } from '@/api/client'
import { Scatter } from '@/components/charts/Scatter'
import { StatsLine } from '@/components/StatsLine'
import { TraceFilters, type TraceFilterState } from '@/components/TraceFilters'
import { Badge, EmptyState, ErrorBox, Spinner } from '@/components/ui'
import { formatDuration, formatTsMicro } from '@/lib/time'
import { splitList, useTimeRange, useUrlState } from '@/lib/url-state'

export function TracesPage() {
  const meta = useMeta()
  const navigate = useNavigate()
  const { params, set, setParams } = useUrlState()
  const { range } = useTimeRange()

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

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <TraceFilters state={filter} rangeParams={{ from: range.fromMs, to: range.toMs }} onChange={setFilter} />
      <section className="border-b border-border bg-card px-4 pt-3 pb-2">
        <Scatter
          fromMs={range.fromMs}
          toMs={range.toMs}
          points={traces.map((t) => ({ id: t.trace_id, t_ms: t.start_us / 1000, value: t.duration_ns / 1e6, error: t.error_count > 0, label: `${t.root_service} ${t.root_name}` }))}
          height={170}
          stale={search.isFetching}
          onClick={(id) => navigate(`/traces/${id}`)}
        />
        <div className="mt-1 flex items-center justify-between text-2xs text-muted-fg">
          <span>
            纵轴为请求耗时（根 span，对数刻度）；<span className="inline-block size-2.5 rounded-full align-middle" style={{ background: 'var(--level-error)' }} /> 有错误的链路。点一个点打开详情。
          </span>
          <StatsLine stats={search.data?.stats} />
        </div>
      </section>
      <div className="flex items-center gap-3 border-b border-border bg-card px-4 py-2 text-xs text-muted-fg">
        {search.data && (
          <span className="font-medium text-fg">
            {traces.length} 条链路{traces.length >= limit && `（只取前 ${limit} 条，${filter.sort === 'duration' ? '最慢在前' : '最新在前'}）`}
          </span>
        )}
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
        {traces.length > 0 && (
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
                <tr key={t.trace_id} className="row-hover cursor-pointer border-b border-border/60" onClick={() => navigate(`/traces/${t.trace_id}`)}>
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
                    <Link to={`/traces/${t.trace_id}`} className="text-accent hover:underline" onClick={(e) => e.stopPropagation()}>
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
