import { useMemo, useState } from 'react'
import { Link, useParams } from 'react-router'
import { useMeta, useOperations, useTimeseries } from '@/api/queries'
import type { OperationStat } from '@/api/types'
import { LineChart, Legend } from '@/components/charts/LineChart'
import { StackedBars } from '@/components/charts/StackedBars'
import { StatsLine } from '@/components/StatsLine'
import { Button, Card, EmptyState, ErrorBox, Spinner } from '@/components/ui'
import { ErrorRate } from '@/pages/ServicesPage'
import { formatDurationMs, formatNumber } from '@/lib/time'
import { useTimeRange, useUrlState } from '@/lib/url-state'
import { cn } from '@/lib/utils'

// 三条线两两都要分得开（会交叉）：用参考配色前三档 aqua / blue / orange，全对校验通过
const LATENCY_SERIES = [
  { key: 'p50_ms', label: 'P50', color: 'var(--chart-3)' },
  { key: 'p95_ms', label: 'P95', color: 'var(--chart-1)' },
  { key: 'p99_ms', label: 'P99', color: 'var(--chart-2)' },
]
const TRAFFIC_SERIES = [
  { key: 'ok', label: '成功', color: 'var(--chart-1)' },
  { key: 'errors', label: '错误', color: 'var(--level-error)' },
]

type OpSort = keyof Pick<OperationStat, 'span_name' | 'requests' | 'errors' | 'error_rate' | 'p50_ms' | 'p95_ms' | 'p99_ms' | 'max_ms'>

export function ServiceDetailPage() {
  const { name = '' } = useParams<{ name: string }>()
  const service = decodeURIComponent(name)
  const meta = useMeta()
  const { range } = useTimeRange()
  const { params, set } = useUrlState()
  const kind = params.get('kind') === 'client' ? 'client' : 'entry'
  const op = params.get('op') ?? ''
  const rangeParams = { from: range.fromMs, to: range.toMs }
  const ops = useOperations(service, { ...rangeParams, kind })
  const ts = useTimeseries(service, { ...rangeParams, span_name: op || undefined })
  const [sort, setSort] = useState<{ key: OpSort; desc: boolean }>({ key: 'requests', desc: true })
  const rows = useMemo(() => {
    const list = [...(ops.data?.operations ?? [])]
    list.sort((a, b) => {
      const va = a[sort.key]
      const vb = b[sort.key]
      const c = typeof va === 'string' && typeof vb === 'string' ? va.localeCompare(vb) : Number(va) - Number(vb)
      return sort.desc ? -c : c
    })
    return list
  }, [ops.data, sort])
  const logDim = meta.data?.logs.dimensions.includes('service_name') ? 'service_name' : 'container'
  // 没有请求的桶不画（断线），画成 0 会把延迟曲线拉到地板上
  const points = (ts.data?.points ?? []).map((p) => {
    const values: Record<string, number> = p.requests > 0 ? { p50_ms: p.p50_ms, p95_ms: p.p95_ms, p99_ms: p.p99_ms } : {}
    return { t_ms: p.t_ms, values }
  })
  const traffic = (ts.data?.points ?? []).map((p) => ({ t_ms: p.t_ms, values: { ok: p.requests - p.errors, errors: p.errors } }))
  const cols: { key: OpSort; label: string; right?: boolean }[] = [
    { key: 'span_name', label: kind === 'entry' ? '接口 / 操作' : '下游调用' },
    { key: 'requests', label: '次数', right: true },
    { key: 'errors', label: '错误', right: true },
    { key: 'error_rate', label: '错误率', right: true },
    { key: 'p50_ms', label: 'P50', right: true },
    { key: 'p95_ms', label: 'P95', right: true },
    { key: 'p99_ms', label: 'P99', right: true },
    { key: 'max_ms', label: '最大', right: true },
  ]

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-3 border-b border-border bg-card px-4 py-3">
        <Link to="/services" className="text-sm text-muted-fg hover:text-fg">
          ← 服务
        </Link>
        <h1 className="text-base font-semibold">{service}</h1>
        {op && (
          <span className="inline-flex items-center gap-1.5 rounded-md bg-accent-soft px-2.5 py-1 text-xs text-accent">
            {op}
            <button type="button" onClick={() => set({ op: null })} title="看整个服务">
              ✕
            </button>
          </span>
        )}
        <span className="ml-auto flex items-center gap-2">
          <Link to={`/traces?service=${encodeURIComponent(service)}${op ? `&span_name=${encodeURIComponent(op)}` : ''}&kind=Server,Consumer&sort=duration`}>
            <Button size="sm">最慢的链路</Button>
          </Link>
          <Link to={`/traces?service=${encodeURIComponent(service)}${op ? `&span_name=${encodeURIComponent(op)}` : ''}&error_only=1`}>
            <Button size="sm">出错的链路</Button>
          </Link>
          <Link to={`/logs?${logDim}=${encodeURIComponent(service)}&level=ERROR,WARN`}>
            <Button size="sm">错误日志</Button>
          </Link>
        </span>
      </header>
      <div className="min-h-0 flex-1 overflow-auto p-4">
        <div className="grid gap-4 lg:grid-cols-2">
          <Card title={`请求量与错误${op ? `：${op}` : ''}`} extra={<StatsLine stats={ts.data?.stats} />}>
            <div className="px-3 pt-3 pb-1">
              {ts.isError ? (
                <ErrorBox error={ts.error} />
              ) : (
                <>
                  <Legend series={TRAFFIC_SERIES} className="px-1" />
                  <StackedBars fromMs={ts.data?.from_ms ?? range.fromMs} toMs={ts.data?.to_ms ?? range.toMs} widthMs={ts.data?.width_ms ?? 60_000} buckets={traffic} series={TRAFFIC_SERIES} height={190} stale={ts.isFetching} />
                </>
              )}
            </div>
          </Card>
          <Card title="延迟分位（毫秒）">
            <div className="px-3 pt-3 pb-1">
              {ts.isError ? (
                <ErrorBox error={ts.error} />
              ) : (
                <>
                  <Legend series={LATENCY_SERIES} className="px-1" />
                  <LineChart fromMs={ts.data?.from_ms ?? range.fromMs} toMs={ts.data?.to_ms ?? range.toMs} widthMs={ts.data?.width_ms ?? 60_000} points={points} series={LATENCY_SERIES} height={190} stale={ts.isFetching} format={(v) => formatDurationMs(v)} />
                </>
              )}
            </div>
          </Card>
        </div>
        <Card
          className="mt-4 overflow-hidden"
          title={
            <span className="flex items-center gap-1">
              {(['entry', 'client'] as const).map((k) => (
                <button key={k} type="button" onClick={() => set({ kind: k === 'entry' ? null : k, op: null })} className={cn('rounded-sm px-2.5 py-1', kind === k ? 'bg-accent-soft text-accent' : 'text-muted-fg hover:text-fg')}>
                  {k === 'entry' ? '入口接口' : '下游调用'}
                </button>
              ))}
              {ops.isFetching && <Spinner className="size-3.5" />}
            </span>
          }
          extra={<StatsLine stats={ops.data?.stats} />}
        >
          {ops.isError && <ErrorBox error={ops.error} onRetry={() => ops.refetch()} />}
          {ops.isPending && (
            <div className="flex justify-center py-10">
              <Spinner />
            </div>
          )}
          {ops.data && !rows.length && <EmptyState title={kind === 'entry' ? '没有入口 span' : '没有对外调用的 span'} />}
          {rows.length > 0 && (
            <table className="w-full table-fixed border-collapse text-xs">
              <thead className="text-2xs text-muted-fg">
                <tr className="border-b border-border bg-muted/40">
                  {cols.map((c) => (
                    <th
                      key={c.key}
                      className={cn('cursor-pointer px-4 py-2.5 font-medium select-none hover:text-fg', c.right ? 'w-28 text-right' : 'text-left', sort.key === c.key && 'text-accent')}
                      onClick={() => setSort((s) => ({ key: c.key, desc: s.key === c.key ? !s.desc : c.key !== 'span_name' }))}
                    >
                      {c.label}
                      {sort.key === c.key && (sort.desc ? ' ▾' : ' ▴')}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {rows.map((o) => (
                  <tr
                    key={`${o.kind}:${o.span_name}`}
                    className={cn('row-hover cursor-pointer border-b border-border/60 last:border-b-0', op === o.span_name && 'row-selected')}
                    onClick={() => set({ op: op === o.span_name ? null : o.span_name })}
                    title="点击只看这个操作的趋势"
                  >
                    <td className="truncate px-4 py-2.5 font-medium" title={o.span_name}>
                      {o.span_name} <span className="text-2xs font-normal text-muted-fg">{o.kind}</span>
                    </td>
                    <td className="px-4 py-2.5 text-right tabular-nums">{formatNumber(o.requests)}</td>
                    <td className="px-4 py-2.5 text-right tabular-nums">{o.errors ? formatNumber(o.errors) : <span className="text-muted-fg">0</span>}</td>
                    <td className="px-4 py-2.5 text-right">
                      <ErrorRate rate={o.error_rate} />
                    </td>
                    <td className="px-4 py-2.5 text-right tabular-nums">{formatDurationMs(o.p50_ms)}</td>
                    <td className="px-4 py-2.5 text-right tabular-nums">{formatDurationMs(o.p95_ms)}</td>
                    <td className="px-4 py-2.5 text-right font-medium tabular-nums">{formatDurationMs(o.p99_ms)}</td>
                    <td className="px-4 py-2.5 text-right text-muted-fg tabular-nums">{formatDurationMs(o.max_ms)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </Card>
      </div>
    </div>
  )
}
