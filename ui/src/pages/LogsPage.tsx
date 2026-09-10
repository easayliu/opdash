import { useCallback, useEffect, useMemo, useState } from 'react'
import { Link } from 'react-router'
import { DownloadIcon, PauseIcon, PlayIcon, TerminalIcon } from 'lucide-react'
import { apiUrl, type Params } from '@/api/client'
import { useLogHistogram, useLogSearch, useMeta } from '@/api/queries'
import { TAIL_MAX_ROWS, useLogTail } from '@/api/tail'
import type { LogRow } from '@/api/types'
import { StackedBars } from '@/components/charts/StackedBars'
import { ContextDrawer } from '@/components/ContextDrawer'
import { LogFilters, type LogFilterState } from '@/components/LogFilters'
import { LogStream } from '@/components/LogStream'
import { LogTable } from '@/components/LogTable'
import { StatsLine } from '@/components/StatsLine'
import { Button, EmptyState, ErrorBox, Select, Spinner } from '@/components/ui'
import { levelColor } from '@/lib/colors'
import { cn } from '@/lib/utils'
import { formatNumber } from '@/lib/time'
import { metricsHref, serviceHref, tracesHref } from '@/lib/links'
import { splitList, useTimeRange, useUrlState } from '@/lib/url-state'
import { useIsMobile } from '@/lib/media'
import { positiveTerms } from '@/lib/query-syntax'

const PAGE_SIZES = [100, 200, 500, 1000]

/** 跟随时表头那行的状态字 */
const TAIL_LABEL = { connecting: '连接中…', live: '跟随中', reconnecting: '重连中…', failed: '跟随已断开' } as const

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
  const isMobile = useIsMobile()
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
  // 终端模式：时间正序追加在底部、自动滚到底。只在跟随时有意义（不跟随就是普通翻页）
  const terminal = follow && params.get('term') === '1'

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
  // 直方图各桶之和就是总条数，有直方图时让检索别再跑一条扫同样数据的 count()
  const search = useLogSearch(
    { ...baseParams, order: byId ? 'asc' : order, limit, offset, count: byId ? undefined : 0 },
    !follow && meta.isSuccess,
  )
  /**
   * 带关键字时**等检索回来再发直方图**，两条不再并发。
   *
   * 两条查询的 WHERE 一模一样，而 `message` 没有索引，要扫完整个时间范围（线上一小时约
   * 10 GB 未压缩——整张表 83% 的体积就是这一列）。并发发出去就是同一段数据扫两遍；错开之后
   * 第二条命中 ClickHouse 26.x 的 **query condition cache**（`use_query_condition_cache`
   * 服务端默认开）：线上实测第一条 39.8 GB / 3.5 s，紧接着同条件的第二条 **0 GB / 18 ms**。
   *
   * 顺序是「检索在前」：列表是人盯着的那一块，不能为了直方图让它变慢。而且关键字命中多的
   * 时候检索读够 200 行就停、本来就快，这种情况下直方图仍要自己扫——两种情况加起来，
   * 集群总共大约只扫一遍。检索失败（比如读量超限）也放行，不然一个错误连累两块都空着。
   */
  const histogram = useLogHistogram(baseParams, !byId && meta.isSuccess && (!filter.q || search.isFetched))

  // 跟随：一条 SSE 长连接，服务端按游标推增量（见 src/api/tail.rs）
  const tail = useLogTail(baseParams, follow)

  const [contextRow, setContextRow] = useState<LogRow | null>(null)
  useEffect(() => {
    if (!contextRow) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setContextRow(null)
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [contextRow])

  const highlight = useMemo(() => (filter.regex ? [] : positiveTerms(filter.q)), [filter.regex, filter.q])
  const rows = follow ? tail.rows : (search.data?.rows ?? [])
  // 表格是最新在前，终端是最新在后
  const streamRows = useMemo(() => (terminal ? [...rows].reverse() : rows), [terminal, rows])
  // 检索还没回来时表里是上一次的结果（keepPreviousData）。直方图比检索快，这时候把新的总数
  // 摆在旧行上面就成了「共 4 条」配着一屏不相干的日志，所以这一段整体标成待更新。
  const stale = !follow && search.isPlaceholderData
  // 按 id 查没有直方图，总数还是检索自己带回来的
  const total = byId ? search.data?.total : histogram.data?.total
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

  // 日志表上「服务」这一维叫什么（老表没有 service_name 就退回 container）
  const serviceDim = dims.includes('service_name') ? 'service_name' : 'container'

  const onPivot = (field: string, value: string) => {
    if (dims.includes(field)) setFilter({ ...filter, dims: { ...filter.dims, [field]: [value] } })
    else if (field === 'level') setFilter({ ...filter, levels: [value] })
    else if (field === 'thread') setFilter({ ...filter, thread: value })
    else if (field === 'host') set({ host: value })
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <LogFilters state={filter} dims={dims} rangeParams={{ from: range.fromMs, to: range.toMs }} onChange={setFilter} />
      <CrossLinks service={filter.dims[serviceDim]?.[0]} win={{ fromMs: range.fromMs, toMs: range.toMs }} hasMetrics={!!meta.data?.metrics} />
      {!byId && (
        <section className="border-b border-border bg-card px-3 pt-2 pb-1.5 md:px-4 md:pt-3 md:pb-2">
          {histogram.isError ? (
            <ErrorBox error={histogram.error} />
          ) : (
            <StackedBars
              fromMs={histogram.data?.from_ms ?? range.fromMs}
              toMs={histogram.data?.to_ms ?? range.toMs}
              widthMs={histogram.data?.width_ms ?? 60_000}
              buckets={(histogram.data?.buckets ?? []).map((b) => ({ t_ms: b.t_ms, values: b.counts }))}
              series={(histogram.data?.levels ?? []).map((l) => ({ key: l, label: l, color: levelColor(l) }))}
              height={isMobile ? 96 : 150}
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
              {histogram.data && <span className="hidden sm:inline">拖选一段可缩小时间范围</span>}
            </span>
            <StatsLine stats={histogram.data?.stats} className="hidden text-2xs text-muted-fg sm:inline" />
          </div>
        </section>
      )}
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-border bg-card px-3 py-2 text-xs md:px-4">
        <span className="font-medium text-fg">
          {stale
            ? '查询中…'
            : follow
              ? `${TAIL_LABEL[tail.status]} · 已收 ${formatNumber(rows.length)} 行`
              : total !== undefined
                ? `共 ${formatNumber(total)} 条${total > limit ? `，显示第 ${formatNumber(offset + 1)} ~ ${formatNumber(Math.min(offset + limit, total))} 条` : ''}`
                : search.data
                  ? `显示 ${formatNumber(rows.length)} 条`
                  : ''}
        </span>
        {(search.isFetching || (follow && tail.status !== 'live')) && <Spinner className="size-4" />}
        {/* 整词模式是后端按词长自动切的，不提示的话「搜 id 的前半截搜不到」会很费解 */}
        {!stale && !!search.data?.token_terms?.length && (
          <span
            className="text-2xs text-muted-fg"
            title={`${search.data.token_terms.join('、')}：够长的标识符按整词匹配，走 message 上的 token 索引，快很多。要搜片段请用正则模式（.*）。`}
          >
            按整词匹配 · 已走索引
          </span>
        )}
        <StatsLine stats={follow ? tail.stats : search.data?.stats} className={cn('hidden text-2xs text-muted-fg sm:inline', stale && 'opacity-50')} />
        <div className="ml-auto flex items-center gap-2">
          {!byId && (
            <Button
              size="sm"
              active={follow}
              disabled={!range.relative}
              title={range.relative ? '新日志实时推过来（服务端每秒查一次增量，并回看一分钟兜住晚到的行）' : '只有相对时间范围（最近 N 分钟）才能跟随'}
              onClick={() => set({ follow: follow ? null : '1', order: null, offset: null })}
            >
              {follow ? <PauseIcon className="size-4" /> : <PlayIcon className="size-4" />}
              <span className="hidden sm:inline">{follow ? '停止跟随' : '跟随'}</span>
            </Button>
          )}
          {follow && (
            <Button
              size="sm"
              active={terminal}
              title="终端模式：新日志正序追加在底部、自动滚到底；往上滚就停住"
              onClick={() => set({ term: terminal ? null : '1' })}
            >
              <TerminalIcon className="size-4" />
              <span className="hidden sm:inline">终端</span>
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
          {!follow && !byId && !isMobile && <Pager {...pager} />}
          {/* 导出在手机上没什么用，也省出一行 */}
          <a href={apiUrl('/logs/export', { ...exportParams, format: 'csv' })} className="hidden md:inline-flex" download title={`导出 CSV（最多 ${meta.data?.limits.export_max_rows ?? 50000} 行）`}>
            <Button size="sm">
              <DownloadIcon className="size-4" />
              CSV
            </Button>
          </a>
          <a href={apiUrl('/logs/export', { ...exportParams, format: 'jsonl' })} className="hidden md:inline-flex" download title="导出 JSON Lines">
            <Button size="sm">JSONL</Button>
          </a>
        </div>
      </div>
      <div className={cn('min-h-0 flex-1 overflow-auto bg-card', stale && 'opacity-40 transition-opacity')}>
        {search.isError && <ErrorBox error={search.error} onRetry={() => search.refetch()} />}
        {follow && tail.error && <ErrorBox error={{ message: tail.error }} />}
        {!follow && search.isPending && !search.isError && (
          <div className="flex justify-center py-16">
            <Spinner />
          </div>
        )}
        {(follow || search.data) &&
          (terminal ? (
            <LogStream
              rows={streamRows}
              dims={dims}
              highlight={highlight}
              onContext={setContextRow}
              onPivot={onPivot}
              emptyText={<EmptyState title="等待新日志…" hint="新的行会一条条打在下面。往上滚可以停住不跟，滚回底部又接上。" />}
            />
          ) : (
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
          ))}
        {rows.length > 0 && (
          <footer className="flex flex-wrap items-center justify-between gap-2 border-t border-border px-3 py-3 text-xs text-muted-fg md:px-4">
            {follow ? (
              <span>
                新日志由服务端推送（每 {(tail.hello?.interval_ms ?? 1000) / 1000} 秒查一次增量，回看 {(tail.hello?.lookback_ms ?? 60_000) / 1000} 秒兜住晚到的行）；
                {terminal ? '正序打在下面，往上滚可以停住；' : ''}最多保留 {formatNumber(TAIL_MAX_ROWS)} 行，更早的会被丢掉
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

/**
 * 筛到某一个服务时，给出跳到另外两个信号的入口——同一个服务、同一段时间。
 * 没筛服务就不显示：跳过去也不知道该看哪个服务的指标。
 */
function CrossLinks({ service, win, hasMetrics }: { service?: string; win: { fromMs: number; toMs: number }; hasMetrics: boolean }) {
  if (!service) return null
  return (
    <div className="flex flex-wrap items-center gap-2 border-b border-border bg-card px-3 py-1.5 md:px-4">
      <span className="text-2xs text-muted-fg">{service} 这段时间的</span>
      {hasMetrics && (
        <Link to={metricsHref(service, win)}>
          <Button size="xs">指标看板</Button>
        </Link>
      )}
      <Link to={tracesHref({ service, sort: 'duration', kinds: 'Server,Consumer' }, win)}>
        <Button size="xs">最慢的链路</Button>
      </Link>
      <Link to={tracesHref({ service, errorOnly: true }, win)}>
        <Button size="xs">出错的链路</Button>
      </Link>
      <Link to={serviceHref(service, win)}>
        <Button size="xs">服务概览</Button>
      </Link>
    </div>
  )
}
