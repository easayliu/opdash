import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { DownloadIcon, PauseIcon, PlayIcon } from 'lucide-react'
import { apiUrl, type Params } from '@/api/client'
import { useLogHistogram, useLogSearch, useMeta } from '@/api/queries'
import type { LogRow } from '@/api/types'
import { StackedBars } from '@/components/charts/StackedBars'
import { ContextDrawer } from '@/components/ContextDrawer'
import { LogFilters, type LogFilterState } from '@/components/LogFilters'
import { LogTable, rowKey } from '@/components/LogTable'
import { StatsLine } from '@/components/StatsLine'
import { Button, EmptyState, ErrorBox, Select, Spinner } from '@/components/ui'
import { levelColor } from '@/lib/colors'
import { formatNumber } from '@/lib/time'
import { splitList, useTimeRange, useUrlState } from '@/lib/url-state'

const PAGE_SIZES = [100, 200, 500, 1000]
const FOLLOW_INTERVAL_MS = 5000
const FOLLOW_MAX_ROWS = 2000

function parseTerms(q: string): string[] {
  // 只取要高亮的正向词（和后端 parse_terms 一致的简化版）
  const out: string[] = []
  const re = /-?"([^"]*)"|(\S+)/g
  let m: RegExpExecArray | null
  while ((m = re.exec(q))) {
    const raw = m[0]
    if (raw.startsWith('-')) continue
    const word = m[1] ?? m[2]
    if (word) out.push(word)
  }
  return out
}

interface PagerProps {
  offset: number
  limit: number
  /** 当前页已经是最后一页 */
  atEnd: boolean
  /** 还有下一页，但撞上了后端的翻页上限 */
  hitCap: boolean
  maxOffset: number
  onPage: (offset: number) => void
}

function Pager({ offset, limit, atEnd, hitCap, maxOffset, onPage }: PagerProps) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <Button size="sm" disabled={offset === 0} onClick={() => onPage(Math.max(0, offset - limit))}>
        上一页
      </Button>
      <Button
        size="sm"
        disabled={atEnd || hitCap}
        title={hitCap ? `最多翻到第 ${formatNumber(maxOffset)} 条，请缩小范围` : atEnd ? '已经是最后一页' : undefined}
        onClick={() => onPage(offset + limit)}
      >
        下一页
      </Button>
    </span>
  )
}

export function LogsPage() {
  const meta = useMeta()
  const { params, set } = useUrlState()
  const { range, setRange } = useTimeRange()
  const dims = meta.data?.logs.dimensions ?? []

  // URL → 筛选状态
  const filter: LogFilterState = useMemo(() => {
    const dimValues: Record<string, string[]> = {}
    for (const d of dims) {
      const v = splitList(params.get(d))
      if (v.length) dimValues[d] = v
    }
    return {
      q: params.get('q') ?? '',
      regex: params.get('regex') === '1',
      levels: splitList(params.get('level')),
      logger: params.get('logger') ?? '',
      thread: params.get('thread') ?? '',
      trace_id: params.get('trace_id') ?? '',
      span_id: params.get('span_id') ?? '',
      dims: dimValues,
    }
  }, [params, dims])
  const order = (params.get('order') === 'asc' ? 'asc' : 'desc') as 'asc' | 'desc'
  const limit = Number(params.get('limit')) || 200
  const offset = Number(params.get('offset')) || 0
  const follow = params.get('follow') === '1' && !!range.relative && order === 'desc'

  const setFilter = useCallback(
    (next: LogFilterState) => {
      const patch: Record<string, string | null> = {
        q: next.q || null,
        regex: next.regex ? '1' : null,
        level: next.levels.join(',') || null,
        logger: next.logger || null,
        thread: next.thread || null,
        trace_id: next.trace_id || null,
        span_id: next.span_id || null,
        offset: null,
      }
      for (const d of dims) patch[d] = next.dims[d]?.join(',') || null
      set(patch)
    },
    [dims, set],
  )

  const byId = !!(filter.trace_id || filter.span_id)
  // 按 id 查时不带时间范围（bloom filter 直接命中，时间范围只会把 3 小时前的 trace 挡在外面）
  const baseParams: Params = useMemo(
    () => ({
      ...(byId ? {} : { from: range.fromMs, to: range.toMs }),
      q: filter.q,
      regex: filter.regex ? 1 : undefined,
      level: filter.levels.join(','),
      logger: filter.logger,
      thread: filter.thread,
      trace_id: filter.trace_id,
      span_id: filter.span_id,
      ...Object.fromEntries(Object.entries(filter.dims).map(([k, v]) => [k, v.join(',')])),
    }),
    [byId, range.fromMs, range.toMs, filter],
  )
  const search = useLogSearch({ ...baseParams, order: byId ? 'asc' : order, limit, offset }, !follow && meta.isSuccess)
  const histogram = useLogHistogram(baseParams, !byId && meta.isSuccess)

  // ---- 跟随模式：每 5 秒拉一次，回看 60 秒兜住晚到的行，按行键去重 ----
  const [live, setLive] = useState<LogRow[]>([])
  const seen = useRef<Set<string>>(new Set())
  const lastMax = useRef<number>(0)
  useEffect(() => {
    if (!follow) {
      setLive([])
      seen.current = new Set()
      lastMax.current = 0
      return
    }
    let stopped = false
    const tick = async () => {
      const now = Date.now()
      const from = lastMax.current ? lastMax.current - 60_000 : range.fromMs
      try {
        const res = await fetch(`/api/logs/search${new URLSearchParams(Object.entries({ ...baseParams, from, to: now, order: 'desc', limit: 500, count: 0 }).filter(([, v]) => v !== undefined && v !== '').map(([k, v]) => [k, String(v)])).toString().replace(/^/, '?')}`)
        if (!res.ok || stopped) return
        const data = (await res.json()) as { rows: LogRow[] }
        const fresh = data.rows.filter((r) => !seen.current.has(rowKey(r)))
        if (fresh.length) {
          for (const r of fresh) {
            seen.current.add(rowKey(r))
            lastMax.current = Math.max(lastMax.current, r.ts_ms)
          }
          setLive((prev) => [...fresh, ...prev].sort((a, b) => b.ts_ms - a.ts_ms).slice(0, FOLLOW_MAX_ROWS))
        }
      } catch {
        // 网络抖动下一轮再试
      }
    }
    tick()
    const id = setInterval(tick, FOLLOW_INTERVAL_MS)
    return () => {
      stopped = true
      clearInterval(id)
    }
  }, [follow, baseParams, range.fromMs])

  const [contextRow, setContextRow] = useState<LogRow | null>(null)
  useEffect(() => {
    if (!contextRow) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setContextRow(null)
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [contextRow])

  const highlight = filter.regex ? [] : parseTerms(filter.q)
  const rows = follow ? live : (search.data?.rows ?? [])
  const total = search.data?.total
  const exportParams = { ...baseParams, order }
  const maxOffset = meta.data?.limits.max_offset ?? Infinity
  const pager: PagerProps = {
    offset,
    limit,
    // 拿回来的不满一页就是最后一页；有 total 的话再用 total 兜一下
    atEnd: rows.length < limit || (total !== undefined && offset + rows.length >= total),
    hitCap: rows.length >= limit && offset + limit > maxOffset,
    maxOffset,
    onPage: (next) => set({ offset: next || null }),
  }

  const onPivot = (field: string, value: string) => {
    if (dims.includes(field)) setFilter({ ...filter, dims: { ...filter.dims, [field]: [value] } })
    else if (field === 'level') setFilter({ ...filter, levels: [value] })
    else if (field === 'thread') setFilter({ ...filter, thread: value })
    else if (field === 'host') set({ host: value })
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <LogFilters state={filter} dims={dims} rangeParams={{ from: range.fromMs, to: range.toMs }} onChange={setFilter} />
      {!byId && (
        <section className="border-b border-border bg-card px-4 pt-3 pb-2">
          {histogram.isError ? (
            <ErrorBox error={histogram.error} />
          ) : (
            <StackedBars
              fromMs={histogram.data?.from_ms ?? range.fromMs}
              toMs={histogram.data?.to_ms ?? range.toMs}
              widthMs={histogram.data?.width_ms ?? 60_000}
              buckets={(histogram.data?.buckets ?? []).map((b) => ({ t_ms: b.t_ms, values: b.counts }))}
              series={(histogram.data?.levels ?? []).map((l) => ({ key: l, label: l, color: levelColor(l) }))}
              height={150}
              stale={histogram.isFetching}
              onBrush={(f, t) => setRange({ fromMs: f, toMs: t, relative: null })}
            />
          )}
          <div className="mt-1 flex items-center justify-between text-2xs text-muted-fg">
            <span className="flex flex-wrap gap-x-4">
              {(histogram.data?.levels ?? []).map((l) => (
                <span key={l} className="inline-flex items-center gap-1">
                  <span className="inline-block size-2.5 rounded-sm" style={{ background: levelColor(l) }} />
                  {l}
                </span>
              ))}
              {histogram.data && <span>拖选一段可缩小时间范围</span>}
            </span>
            <StatsLine stats={histogram.data?.stats} />
          </div>
        </section>
      )}
      <div className="flex flex-wrap items-center gap-3 border-b border-border bg-card px-4 py-2 text-xs">
        <span className="font-medium text-fg">
          {follow
            ? `跟随中 · 已收 ${formatNumber(rows.length)} 行`
            : total !== undefined
              ? `共 ${formatNumber(total)} 条${total > limit ? `，显示第 ${formatNumber(offset + 1)} ~ ${formatNumber(Math.min(offset + limit, total))} 条` : ''}`
              : search.data
                ? `显示 ${formatNumber(rows.length)} 条`
                : ''}
        </span>
        {search.isFetching && <Spinner className="size-4" />}
        <StatsLine stats={search.data?.stats} />
        <div className="ml-auto flex items-center gap-2">
          {!byId && (
            <Button
              size="sm"
              active={follow}
              disabled={!range.relative}
              title={range.relative ? '每 5 秒拉一次新日志（回看 60 秒兜住晚到的行）' : '只有相对时间范围（最近 N 分钟）才能跟随'}
              onClick={() => set({ follow: follow ? null : '1', order: null, offset: null })}
            >
              {follow ? <PauseIcon className="size-4" /> : <PlayIcon className="size-4" />}
              {follow ? '停止跟随' : '跟随'}
            </Button>
          )}
          {!byId && !follow && (
            <Select value={order} onChange={(e) => set({ order: e.target.value === 'asc' ? 'asc' : null, offset: null })} className="h-8 text-xs">
              <option value="desc">最新在前</option>
              <option value="asc">最早在前</option>
            </Select>
          )}
          {!follow && (
            <Select value={String(limit)} onChange={(e) => set({ limit: e.target.value === '200' ? null : e.target.value, offset: null })} className="h-8 text-xs">
              {PAGE_SIZES.map((n) => (
                <option key={n} value={n}>
                  每页 {n}
                </option>
              ))}
            </Select>
          )}
          {!follow && !byId && <Pager {...pager} />}
          <a href={apiUrl('/logs/export', { ...exportParams, format: 'csv' })} className="inline-flex" download title={`导出 CSV（最多 ${meta.data?.limits.export_max_rows ?? 50000} 行）`}>
            <Button size="sm">
              <DownloadIcon className="size-4" />
              CSV
            </Button>
          </a>
          <a href={apiUrl('/logs/export', { ...exportParams, format: 'jsonl' })} className="inline-flex" download title="导出 JSON Lines">
            <Button size="sm">JSONL</Button>
          </a>
        </div>
      </div>
      <div className="min-h-0 flex-1 overflow-auto bg-card">
        {search.isError && <ErrorBox error={search.error} onRetry={() => search.refetch()} />}
        {!follow && search.isPending && !search.isError && (
          <div className="flex justify-center py-16">
            <Spinner />
          </div>
        )}
        {(follow || search.data) && (
          <LogTable
            rows={rows}
            dims={dims}
            highlight={highlight}
            onContext={setContextRow}
            onPivot={onPivot}
            emptyText={
              <EmptyState
                title={follow ? '等待新日志…' : '这个范围内没有匹配的日志'}
                hint={
                  byId
                    ? '这个 trace / span 没有对应的日志：可能是采集延迟（等几秒再刷新），或者这个服务没有打 TID。'
                    : '试试放宽时间范围、去掉一个筛选条件，或者检查关键字是否写在了 message 里（logger / thread 有单独的框）。'
                }
              />
            }
          />
        )}
        {rows.length > 0 && (
          <footer className="flex flex-wrap items-center justify-between gap-2 border-t border-border px-4 py-3 text-xs text-muted-fg">
            {follow ? (
              <span>
                跟随中，每 {FOLLOW_INTERVAL_MS / 1000} 秒拉一次新日志；最多保留 {formatNumber(FOLLOW_MAX_ROWS)} 行，更早的会被丢掉
              </span>
            ) : byId ? (
              <span>{rows.length >= limit ? `只显示了前 ${formatNumber(limit)} 条，可以把「每页」调大` : `共 ${formatNumber(rows.length)} 条，已经到底了`}</span>
            ) : (
              <>
                <span>
                  {pager.atEnd ? '已经到底了 · ' : ''}
                  {total !== undefined
                    ? `第 ${formatNumber(offset + 1)} ~ ${formatNumber(offset + rows.length)} 条，共 ${formatNumber(total)} 条`
                    : `第 ${formatNumber(offset + 1)} ~ ${formatNumber(offset + rows.length)} 条`}
                  {pager.hitCap && '；已到翻页上限，再往后请缩小时间范围或加筛选'}
                </span>
                <Pager {...pager} />
              </>
            )}
          </footer>
        )}
      </div>
      {contextRow && <ContextDrawer row={contextRow} dims={dims} onClose={() => setContextRow(null)} />}
    </div>
  )
}
