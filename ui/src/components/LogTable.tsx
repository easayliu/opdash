import { Fragment, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react'
import { useVirtualizer, type Virtualizer } from '@tanstack/react-virtual'
import { Link } from 'react-router'
import { ChevronDownIcon, ChevronRightIcon, ChevronsUpDownIcon, CopyIcon, ListTreeIcon } from 'lucide-react'
import type { LogRow } from '@/api/types'
import { Badge, Button, levelTone } from '@/components/ui'
import { useIsMobile } from '@/lib/media'
import { formatTs } from '@/lib/time'
import { cn, copyText, splitFirstLine } from '@/lib/utils'

export interface LogTableProps {
  rows: LogRow[]
  /** 显示哪些动态列（service_name / pod …） */
  dims: string[]
  /** 要高亮的关键字 */
  highlight?: string[]
  /** 锚点行（上下文视图里高亮） */
  anchorKey?: string
  /** 高亮这个 span 打的日志（链路详情里点了某个 span） */
  selectedSpanId?: string | null
  onContext?: (row: LogRow) => void
  /** 点某个值 → 加为筛选条件 */
  onPivot?: (field: string, value: string) => void
  compact?: boolean
  emptyText?: ReactNode
  /** 给了就显示可点的表头（客户端排序，rows 已按它排好） */
  sort?: LogSort
  onSort?: (key: string) => void
}

export interface LogSort {
  /** 'ts_ms' / 'level' / 'logger' / 某个维度列名 */
  key: string
  dir: 'asc' | 'desc'
}

/** 级别按严重程度排，认不出的排最后 */
const LEVEL_RANK: Record<string, number> = { FATAL: 0, ERROR: 1, WARN: 2, WARNING: 2, INFO: 3, DEBUG: 4, TRACE: 5 }

/** 按 sort 排序（稳定，同值按时间再排一次），不改原数组。 */
export function sortLogRows(rows: LogRow[], sort: LogSort): LogRow[] {
  const dir = sort.dir === 'asc' ? 1 : -1
  const cmp = (a: LogRow, b: LogRow): number => {
    if (sort.key === 'ts_ms') return a.ts_ms - b.ts_ms
    if (sort.key === 'level') return (LEVEL_RANK[a.level.toUpperCase()] ?? 9) - (LEVEL_RANK[b.level.toUpperCase()] ?? 9)
    return dimValue(a, sort.key).localeCompare(dimValue(b, sort.key))
  }
  return [...rows].sort((a, b) => dir * cmp(a, b) || a.ts_ms - b.ts_ms)
}

/** 最近的纵向滚动祖先：表格自己不滚，滚的是页面 / 抽屉里那层 overflow-auto */
function scrollParent(el: HTMLElement | null): HTMLElement | null {
  for (let box = el?.parentElement ?? null; box; box = box.parentElement) {
    if (/(auto|scroll)/.test(getComputedStyle(box).overflowY)) return box
  }
  return null
}

/** 表格 / 卡片列表底部估算的一行高度（px）；真实高度渲染后再量 */
const EST_ROW_H = 34
const EST_CARD_H = 76

/**
 * 只渲染视口里的行。行高不固定（消息两行截断、展开后更高），渲染后按 `data-index` 实测。
 * 表头是 sticky 的，滚到某一行时要让出它的高度。
 */
function useRowVirtualizer(rows: LogRow[], keys: string[], hostRef: React.RefObject<HTMLElement | null>, estimate: number, stickyPx: number) {
  const scrollEl = useRef<HTMLElement | null>(null)
  const [margin, setMargin] = useState(0)
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => (scrollEl.current ??= scrollParent(hostRef.current)),
    estimateSize: () => estimate,
    overscan: 8,
    scrollMargin: margin,
    scrollPaddingStart: stickyPx,
    getItemKey: (i) => keys[i],
  })
  // 表格前面可能还有别的东西（报错框、空态），量一下它在滚动容器里的起点
  useLayoutEffect(() => {
    const host = hostRef.current
    const box = scrollEl.current ?? scrollParent(host)
    if (!host || !box) return
    scrollEl.current = box
    const m = Math.max(0, Math.round(host.getBoundingClientRect().top - box.getBoundingClientRect().top + box.scrollTop))
    setMargin((prev) => (prev === m ? prev : m))
  })
  return virtualizer
}

/** 选中的 span 变了：把它的第一条日志滚到表头下面。上下文视图里的锚点行第一次出现时滚到中间。 */
function useScrollToMarked(
  virtualizer: Virtualizer<HTMLElement, Element>,
  rows: LogRow[],
  selectedSpanId: string | null | undefined,
  anchorKey: string | undefined,
) {
  useEffect(() => {
    if (!selectedSpanId) return
    const idx = rows.findIndex((r) => r.span_id === selectedSpanId)
    if (idx >= 0) virtualizer.scrollToIndex(idx, { align: 'start', behavior: 'smooth' })
    // rows 换了（重新排序）也要跟着滚，virtualizer 本身稳定
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedSpanId, rows])
  const anchored = useRef<string | null>(null)
  useEffect(() => {
    if (!anchorKey || anchored.current === anchorKey) return
    const idx = rows.findIndex((r) => rowKey(r) === anchorKey)
    if (idx < 0) return
    anchored.current = anchorKey
    virtualizer.scrollToIndex(idx, { align: 'center' })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [anchorKey, rows])
}

function SortHeader({ label, col, sort, onSort }: { label: string; col: string; sort?: LogSort; onSort?: (key: string) => void }) {
  if (!onSort) return <>{label}</>
  const active = sort?.key === col
  return (
    <button type="button" onClick={() => onSort(col)} className={cn('inline-flex items-center gap-0.5 hover:text-fg', active && 'text-fg')} title="点击排序">
      {label}
      {active ? <span aria-hidden>{sort?.dir === 'asc' ? '▲' : '▼'}</span> : <ChevronsUpDownIcon className="size-3 opacity-50" />}
    </button>
  )
}

/** 一行日志的身份：内容拼出来的，跟随模式按它去重，上下文视图按它认锚点行。 */
export function rowKey(r: LogRow): string {
  return `${r.ts_ms}|${r.host}|${r.file}|${r.thread}|${r.logger}|${r.message}`
}

/**
 * 渲染用的 key。同一毫秒、同一线程打出一模一样内容的行是真会有的，[`rowKey`] 会撞。
 * 撞了的话 React 的 key 和虚拟列表按 key 存的
 * 高度都会串——同一行渲染好几遍、行序错乱。所以重复的加个序号，唯一的行还是保持内容 key，
 * 翻页 / 跟随时不会无谓重挂。
 */
function useRowKeys(rows: LogRow[]): string[] {
  return useMemo(() => {
    const seen = new Map<string, number>()
    return rows.map((r) => {
      const base = rowKey(r)
      const n = seen.get(base) ?? 0
      seen.set(base, n + 1)
      return n === 0 ? base : `${base}#${n}`
    })
  }, [rows])
}

/** 把命中的关键字用 <mark> 包起来（不分大小写，纯文本，不走 innerHTML）。 */
export function Highlight({ text, terms }: { text: string; terms?: string[] }) {
  const keys = (terms ?? []).filter(Boolean)
  if (!keys.length) return <>{text}</>
  const lower = text.toLowerCase()
  const parts: ReactNode[] = []
  let i = 0
  while (i < text.length) {
    let bestIdx = -1
    let bestLen = 0
    for (const k of keys) {
      const idx = lower.indexOf(k.toLowerCase(), i)
      if (idx >= 0 && (bestIdx < 0 || idx < bestIdx)) {
        bestIdx = idx
        bestLen = k.length
      }
    }
    if (bestIdx < 0) {
      parts.push(text.slice(i))
      break
    }
    if (bestIdx > i) parts.push(text.slice(i, bestIdx))
    parts.push(<mark key={`${bestIdx}`}>{text.slice(bestIdx, bestIdx + bestLen)}</mark>)
    i = bestIdx + bestLen
  }
  return <>{parts}</>
}

function dimValue(row: LogRow, dim: string): string {
  const v = row[dim]
  return v === null || v === undefined ? '' : String(v)
}

/** 表里优先展示的维度列：有 service_name 就不再重复显示 container */
export function visibleDims(dims: string[]): string[] {
  const prefer = ['service_name', 'pod', 'namespace']
  const out = prefer.filter((d) => dims.includes(d))
  if (!dims.includes('service_name') && dims.includes('container')) out.push('container')
  return out
}

export function LogTable({ rows, dims, highlight, anchorKey, selectedSpanId, onContext, onPivot, compact, emptyText, sort, onSort }: LogTableProps) {
  const [expanded, setExpanded] = useState<Set<string>>(new Set())
  const isMobile = useIsMobile()
  const cols = visibleDims(dims)
  const toggle = (k: string) =>
    setExpanded((s) => {
      const n = new Set(s)
      if (n.has(k)) n.delete(k)
      else n.add(k)
      return n
    })

  if (!rows.length) {
    return <div className="px-4 py-12 text-center text-sm text-muted-fg">{emptyText ?? '没有日志'}</div>
  }
  if (isMobile) {
    return (
      <LogCards
        rows={rows}
        dims={dims}
        cols={cols}
        highlight={highlight}
        anchorKey={anchorKey}
        selectedSpanId={selectedSpanId}
        onContext={onContext}
        onPivot={onPivot}
        expanded={expanded}
        toggle={toggle}
      />
    )
  }
  return (
    <LogRows
      rows={rows}
      dims={dims}
      cols={cols}
      highlight={highlight}
      anchorKey={anchorKey}
      selectedSpanId={selectedSpanId}
      onContext={onContext}
      onPivot={onPivot}
      compact={compact}
      sort={sort}
      onSort={onSort}
      expanded={expanded}
      toggle={toggle}
    />
  )
}

type RowsProps = Pick<LogTableProps, 'rows' | 'dims' | 'highlight' | 'anchorKey' | 'selectedSpanId' | 'onContext' | 'onPivot'> & {
  cols: string[]
  expanded: Set<string>
  toggle: (k: string) => void
}

/** sticky 表头的高度，滚到某行时让出来 */
const THEAD_H = 30

function LogRows({ rows, dims, cols, highlight, anchorKey, selectedSpanId, onContext, onPivot, compact, sort, onSort, expanded, toggle }: RowsProps & Pick<LogTableProps, 'compact' | 'sort' | 'onSort'>) {
  const tableRef = useRef<HTMLTableElement>(null)
  const keys = useRowKeys(rows)
  const virtualizer = useRowVirtualizer(rows, keys, tableRef, EST_ROW_H, THEAD_H)
  useScrollToMarked(virtualizer, rows, selectedSpanId, anchorKey)
  const items = virtualizer.getVirtualItems()
  const margin = virtualizer.options.scrollMargin
  const padTop = items.length ? items[0].start - margin : 0
  const padBottom = items.length ? virtualizer.getTotalSize() - (items[items.length - 1].end - margin) : 0
  const span = cols.length + (compact ? 4 : 5) + (onContext ? 1 : 0)
  return (
    <table ref={tableRef} className="w-full table-fixed border-collapse text-xs">
      <thead className="sticky top-0 z-[1] bg-card text-2xs text-muted-fg shadow-[inset_0_-1px_0_var(--border)]">
        <tr>
          <th className="w-7" />
          <th className="w-[12.5rem] px-1.5 py-2 text-left font-medium">
            <SortHeader label="时间" col="ts_ms" sort={sort} onSort={onSort} />
          </th>
          <th className="w-16 px-1.5 py-2 text-left font-medium">
            <SortHeader label="级别" col="level" sort={sort} onSort={onSort} />
          </th>
          {cols.map((c) => (
            <th key={c} className={cn('px-1.5 py-2 text-left font-medium', c === 'pod' ? 'w-56' : 'w-40')}>
              <SortHeader label={c} col={c} sort={sort} onSort={onSort} />
            </th>
          ))}
          {!compact && (
            <th className="w-48 px-1.5 py-2 text-left font-medium">
              <SortHeader label="logger" col="logger" sort={sort} onSort={onSort} />
            </th>
          )}
          <th className="px-1.5 py-2 text-left font-medium">message</th>
          <th className="w-28 px-1.5 py-2 text-left font-medium">trace</th>
          {onContext && <th className="w-10" />}
        </tr>
      </thead>
      {/* 视口外的行用一段空白顶着，滚动条长度和全量渲染时一样 */}
      {padTop > 0 && (
        <tbody>
          <tr style={{ height: padTop }}>
            <td colSpan={span} className="p-0" />
          </tr>
        </tbody>
      )}
      {items.map((item) => {
        const r = rows[item.index]
        const key = keys[item.index]
        const open = expanded.has(key)
        const [first, rest] = splitFirstLine(r.message)
        const isAnchor = anchorKey === key || (!!selectedSpanId && r.span_id === selectedSpanId)
        // 一条日志一个 tbody：主行加展开行一起量高度
        return (
          <tbody key={key} data-index={item.index} ref={virtualizer.measureElement}>
            <tr
              className={cn('row-hover cursor-pointer border-b border-border/60 align-top', isAnchor && 'row-selected', open && 'bg-muted/40')}
              data-selected={isAnchor ? '1' : undefined}
              onClick={() => toggle(key)}
            >
              <td className="py-1.5 pl-2 text-muted-fg">
                {open ? <ChevronDownIcon className="size-4" /> : <ChevronRightIcon className="size-4" />}
              </td>
              <td className="mono px-1.5 py-1.5 whitespace-nowrap text-muted-fg tabular-nums">{formatTs(r.ts_ms)}</td>
              <td className="px-1.5 py-1.5">
                <Badge tone={levelTone(r.level)}>{r.level || '-'}</Badge>
              </td>
              {cols.map((c) => (
                <td key={c} className="truncate px-1.5 py-1.5 text-muted-fg" title={dimValue(r, c)}>
                  {onPivot ? (
                    <button
                      type="button"
                      className="max-w-full truncate hover:text-accent hover:underline"
                      title={`只看 ${c} = ${dimValue(r, c)}`}
                      onClick={(e) => {
                        e.stopPropagation()
                        onPivot(c, dimValue(r, c))
                      }}
                    >
                      {dimValue(r, c) || '-'}
                    </button>
                  ) : (
                    dimValue(r, c) || '-'
                  )}
                </td>
              ))}
              {!compact && (
                <td className="mono truncate px-1.5 py-1.5 text-muted-fg" title={r.logger}>
                  {r.logger}
                </td>
              )}
              <td className="px-1.5 py-1.5">
                <div className={cn('mono break-all leading-5', !open && 'line-clamp-2')}>
                  <Highlight text={first} terms={highlight} />
                  {!open && rest && <span className="ml-1 text-muted-fg">… +{rest.split('\n').length} 行</span>}
                </div>
              </td>
              <td className="mono px-1.5 py-1.5 text-2xs">
                {r.trace_id ? (
                  <Link
                    to={`/traces/${r.trace_id}?at=${r.ts_ms}`}
                    className="text-accent hover:underline"
                    title={`查看链路 ${r.trace_id}`}
                    onClick={(e) => e.stopPropagation()}
                  >
                    {r.trace_id.slice(0, 8)}…
                  </Link>
                ) : (
                  <span className="text-muted-fg">-</span>
                )}
              </td>
              {onContext && (
                <td className="px-1 py-1">
                  <Button
                    variant="ghost"
                    size="xs"
                    className="px-1.5"
                    title="查看这一行前后的日志（同一容器日志流）"
                    onClick={(e) => {
                      e.stopPropagation()
                      onContext(r)
                    }}
                  >
                    <ListTreeIcon className="size-4" />
                  </Button>
                </td>
              )}
            </tr>
            {open && (
              <tr className="border-b border-border/60 bg-muted/30">
                <td />
                <td colSpan={span} className="px-2 py-3">
                  <ExpandedRow row={r} dims={dims} highlight={highlight} onPivot={onPivot} />
                </td>
              </tr>
            )}
          </tbody>
        )
      })}
      {padBottom > 0 && (
        <tbody>
          <tr style={{ height: padBottom }}>
            <td colSpan={span} className="p-0" />
          </tr>
        </tbody>
      )}
    </table>
  )
}

/** 手机上的日志列表：一条一张卡，点开看全文和字段。列太多的表格在窄屏上只能横滚，不如卡片。 */
function LogCards({ rows, dims, cols, highlight, anchorKey, selectedSpanId, onContext, onPivot, expanded, toggle }: RowsProps) {
  const listRef = useRef<HTMLUListElement>(null)
  const keys = useRowKeys(rows)
  const virtualizer = useRowVirtualizer(rows, keys, listRef, EST_CARD_H, 0)
  useScrollToMarked(virtualizer, rows, selectedSpanId, anchorKey)
  const items = virtualizer.getVirtualItems()
  const margin = virtualizer.options.scrollMargin
  const padTop = items.length ? items[0].start - margin : 0
  const padBottom = items.length ? virtualizer.getTotalSize() - (items[items.length - 1].end - margin) : 0
  // 卡片上只放第一个维度（一般是 service_name），其余的点开再看
  const primary = cols[0]
  return (
    <ul ref={listRef} className="text-xs">
      {padTop > 0 && <li aria-hidden style={{ height: padTop }} />}
      {items.map((item) => {
        const r = rows[item.index]
        const key = keys[item.index]
        const open = expanded.has(key)
        const [first, rest] = splitFirstLine(r.message)
        const isAnchor = anchorKey === key || (!!selectedSpanId && r.span_id === selectedSpanId)
        return (
          <li
            key={key}
            data-index={item.index}
            ref={virtualizer.measureElement}
            data-selected={isAnchor ? '1' : undefined}
            className={cn('border-b border-border/60 px-3 py-2', isAnchor && 'row-selected', open && 'bg-muted/40')}
            onClick={() => toggle(key)}
          >
            <div className="flex items-center gap-2 text-2xs text-muted-fg">
              <span className="mono tabular-nums">{formatTs(r.ts_ms, { date: false })}</span>
              <Badge tone={levelTone(r.level)}>{r.level || '-'}</Badge>
              {primary && <span className="min-w-0 flex-1 truncate">{dimValue(r, primary) || '-'}</span>}
              {r.trace_id && (
                <Link
                  to={`/traces/${r.trace_id}?at=${r.ts_ms}`}
                  className="mono shrink-0 text-accent"
                  title={`查看链路 ${r.trace_id}`}
                  onClick={(e) => e.stopPropagation()}
                >
                  {r.trace_id.slice(0, 8)}…
                </Link>
              )}
              {onContext && (
                <button
                  type="button"
                  className="-my-1 -mr-1 shrink-0 p-1 text-muted-fg"
                  title="查看这一行前后的日志"
                  onClick={(e) => {
                    e.stopPropagation()
                    onContext(r)
                  }}
                >
                  <ListTreeIcon className="size-4" />
                </button>
              )}
            </div>
            {open ? (
              <div className="mt-2" onClick={(e) => e.stopPropagation()}>
                <ExpandedRow row={r} dims={dims} highlight={highlight} onPivot={onPivot} />
              </div>
            ) : (
              <div className="mono mt-1 line-clamp-3 break-all leading-5">
                <Highlight text={first} terms={highlight} />
                {rest && <span className="ml-1 text-muted-fg">… +{rest.split('\n').length} 行</span>}
              </div>
            )}
          </li>
        )
      })}
      {padBottom > 0 && <li aria-hidden style={{ height: padBottom }} />}
    </ul>
  )
}

function ExpandedRow({ row, dims, highlight, onPivot }: { row: LogRow; dims: string[]; highlight?: string[]; onPivot?: (f: string, v: string) => void }) {
  const fields: [string, string][] = [
    ['level', row.level],
    ['logger', row.logger],
    ['thread', row.thread],
    ...dims.map((d): [string, string] => [d, dimValue(row, d)]),
    ['host', row.host],
    ['file', row.file],
    ['trace_id', row.trace_id],
    ['span_id', row.span_id],
  ]
  return (
    <div className="space-y-3">
      <pre className="mono max-h-[28rem] overflow-auto rounded-md border border-border bg-card p-3 text-xs leading-5 whitespace-pre-wrap break-all">
        <Highlight text={row.message} terms={highlight} />
      </pre>
      <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-xs">
        {fields
          .filter(([, v]) => v)
          .map(([k, v]) => (
            <Fragment key={k}>
              <span className="text-muted-fg">{k}</span>
              <span className="mono flex min-w-0 items-center gap-1 break-all">
                {onPivot && !['file', 'trace_id', 'span_id', 'logger'].includes(k) ? (
                  <button type="button" className="text-left hover:text-accent hover:underline" onClick={() => onPivot(k, v)} title={`只看 ${k} = ${v}`}>
                    {v}
                  </button>
                ) : k === 'trace_id' ? (
                  <Link to={`/traces/${v}?at=${row.ts_ms}`} className="text-accent hover:underline">
                    {v}
                  </Link>
                ) : k === 'span_id' ? (
                  <Link to={`/logs?span_id=${v}`} className="text-accent hover:underline" title="这个 span 的全部日志">
                    {v}
                  </Link>
                ) : (
                  v
                )}
                <button type="button" className="text-muted-fg hover:text-fg" title="复制" onClick={() => copyText(v)}>
                  <CopyIcon className="size-3.5" />
                </button>
              </span>
            </Fragment>
          ))}
      </div>
    </div>
  )
}
