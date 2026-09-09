import { useMemo, useState } from 'react'
import { useNavigate } from 'react-router'
import { AlertTriangleIcon } from 'lucide-react'
import { useServices } from '@/api/queries'
import type { ServiceStat } from '@/api/types'
import { StatsLine } from '@/components/StatsLine'
import { Card, EmptyState, ErrorBox, Spinner } from '@/components/ui'
import { formatDurationMs, formatNumber } from '@/lib/time'
import { useTimeRange } from '@/lib/url-state'
import { cn } from '@/lib/utils'

type SortKey = keyof Pick<ServiceStat, 'service' | 'requests' | 'rps' | 'errors' | 'error_rate' | 'p50_ms' | 'p95_ms' | 'p99_ms' | 'max_ms'>

const COLUMNS: { key: SortKey; label: string; align?: 'right'; title?: string }[] = [
  { key: 'service', label: '服务' },
  { key: 'requests', label: '请求数', align: 'right', title: 'Server + Consumer span 数' },
  { key: 'rps', label: 'QPS', align: 'right', title: '按整个时间范围平均' },
  { key: 'errors', label: '错误', align: 'right' },
  { key: 'error_rate', label: '错误率', align: 'right' },
  { key: 'p50_ms', label: 'P50', align: 'right' },
  { key: 'p95_ms', label: 'P95', align: 'right' },
  { key: 'p99_ms', label: 'P99', align: 'right' },
  { key: 'max_ms', label: '最大', align: 'right' },
]

export function ErrorRate({ rate }: { rate: number }) {
  const pct = rate * 100
  const tone = pct >= 5 ? 'text-danger' : pct >= 1 ? 'text-warn' : 'text-muted-fg'
  return (
    <span className={cn('inline-flex items-center justify-end gap-1 tabular-nums', tone)}>
      {pct >= 1 && <AlertTriangleIcon className="size-3.5" aria-label={pct >= 5 ? '错误率高' : '错误率偏高'} />}
      {pct === 0 ? '0%' : pct < 0.1 ? '<0.1%' : `${pct.toFixed(pct < 10 ? 1 : 0)}%`}
    </span>
  )
}

export function ServicesPage() {
  const { range } = useTimeRange()
  const navigate = useNavigate()
  const q = useServices({ from: range.fromMs, to: range.toMs })
  const [sort, setSort] = useState<{ key: SortKey; desc: boolean }>({ key: 'requests', desc: true })
  const rows = useMemo(() => {
    const list = [...(q.data?.services ?? [])]
    list.sort((a, b) => {
      const va = a[sort.key]
      const vb = b[sort.key]
      const c = typeof va === 'string' && typeof vb === 'string' ? va.localeCompare(vb) : Number(va) - Number(vb)
      return sort.desc ? -c : c
    })
    return list
  }, [q.data, sort])

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <header className="flex flex-wrap items-center gap-x-4 gap-y-1 border-b border-border bg-card px-4 py-3">
        <h1 className="text-base font-semibold">服务</h1>
        <span className="text-xs text-muted-fg">只统计入口 span（Server / Consumer）；点一行看接口和趋势。</span>
        {q.isFetching && <Spinner className="size-4" />}
        <StatsLine stats={q.data?.stats} className="ml-auto text-2xs text-muted-fg" />
      </header>
      <div className="min-h-0 flex-1 overflow-auto p-4">
        <Card className="overflow-hidden">
          {q.isError && <ErrorBox error={q.error} onRetry={() => q.refetch()} />}
          {q.isPending && (
            <div className="flex justify-center py-16">
              <Spinner />
            </div>
          )}
          {q.data && !rows.length && <EmptyState title="这个时间范围内没有入口 span" hint="tracepipe 是不是还没接上？或者试试放宽时间范围。" />}
          {rows.length > 0 && (
            <table className={cn('w-full table-fixed border-collapse text-xs', q.isFetching && 'opacity-70')}>
              <thead className="text-2xs text-muted-fg">
                <tr className="border-b border-border bg-muted/40">
                  {COLUMNS.map((c) => (
                    <th
                      key={c.key}
                      title={c.title}
                      className={cn(
                        'cursor-pointer px-4 py-2.5 font-medium select-none hover:text-fg',
                        c.align === 'right' ? 'w-28 text-right' : 'text-left',
                        sort.key === c.key && 'text-accent',
                      )}
                      onClick={() => setSort((s) => ({ key: c.key, desc: s.key === c.key ? !s.desc : c.key !== 'service' }))}
                    >
                      {c.label}
                      {sort.key === c.key && (sort.desc ? ' ▾' : ' ▴')}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {rows.map((s) => (
                  <tr key={s.service} className="row-hover cursor-pointer border-b border-border/60 last:border-b-0" onClick={() => navigate(`/services/${encodeURIComponent(s.service)}`)}>
                    <td className="truncate px-4 py-2.5 font-medium" title={s.service}>
                      {s.service || '(空)'}
                    </td>
                    <td className="px-4 py-2.5 text-right tabular-nums">{formatNumber(s.requests)}</td>
                    <td className="px-4 py-2.5 text-right text-muted-fg tabular-nums">{s.rps < 10 ? s.rps.toFixed(2) : Math.round(s.rps)}</td>
                    <td className="px-4 py-2.5 text-right tabular-nums">{s.errors ? formatNumber(s.errors) : <span className="text-muted-fg">0</span>}</td>
                    <td className="px-4 py-2.5 text-right">
                      <ErrorRate rate={s.error_rate} />
                    </td>
                    <td className="px-4 py-2.5 text-right tabular-nums">{formatDurationMs(s.p50_ms)}</td>
                    <td className="px-4 py-2.5 text-right tabular-nums">{formatDurationMs(s.p95_ms)}</td>
                    <td className="px-4 py-2.5 text-right font-medium tabular-nums">{formatDurationMs(s.p99_ms)}</td>
                    <td className="px-4 py-2.5 text-right text-muted-fg tabular-nums">{formatDurationMs(s.max_ms)}</td>
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
