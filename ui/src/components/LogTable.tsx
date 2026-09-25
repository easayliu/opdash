import { useEffect, useId, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { useVirtualizer, type Virtualizer } from '@tanstack/react-virtual'
import { Link } from 'react-router'
import { ChevronDownIcon, ChevronRightIcon, ChevronsUpDownIcon, ListTreeIcon, XIcon } from 'lucide-react'
import type { LogRow } from '@/api/types'
import { Badge, Button, Combobox, CopyButton, Hint, levelTone, linkClass, type ComboOption } from '@/components/ui'
import { around, logsHref } from '@/lib/links'
import { messageTruncated, rowKey, truncationNote, useRowKeys } from '@/lib/log-row'
import { useIsMobile } from '@/lib/media'
import { scrollBehavior } from '@/lib/motion'
import { useFrom } from '@/lib/url-state'
import { formatTs } from '@/lib/time'
import { cn, scrollParent, splitFirstLine } from '@/lib/utils'

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
  /**
   * 某几列的表头上带一个下拉筛选（按列名给，`level` 也算一列）。rows 已经是筛过的——筛选本身由外面做，
   * 表只负责画个入口：链路详情里一条 trace 的日志都在手里，按服务再查一趟库是白扫。
   */
  colFilters?: Record<string, ColFilter>
}

/**
 * 表头的一个下拉筛选。单选时 `value` 是一个值（'' 是不筛）；`multiple` 时是一组值（空数组是不筛），
 * 同一列选中的几个值之间是「或」
 */
export type ColFilter = (
  | { multiple?: false; value: string; onChange: (v: string) => void }
  | { multiple: true; value: string[]; onChange: (v: string[]) => void }
) & {
  /** 可选的值；`note` 一般放条数 */
  options: ComboOption[]
  /** 候选值要现查时给：点开下拉那一刻调用 */
  onOpen?: () => void
  loading?: boolean
}

export interface LogSort {
  /** 'ts_ms' / 'level' / 'logger' / 某个维度列名 */
  key: string
  dir: 'asc' | 'desc'
}

/** 级别按严重程度排，认不出的排最后 */
export const LEVEL_RANK: Record<string, number> = { FATAL: 0, ERROR: 1, WARN: 2, WARNING: 2, INFO: 3, DEBUG: 4, TRACE: 5 }

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
    if (idx >= 0) virtualizer.scrollToIndex(idx, { align: 'start', behavior: scrollBehavior() })
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

/**
 * 当前列的排序状态，给读屏用。
 *
 * `aria-sort` 要挂在 `th` 上，不是挂在里面那个按钮上。可排序但没在排的列显式写 `none`，读屏
 * 才知道这列点了能排；不可排序的列不写这个属性。
 */
export function ariaSort(col: string, sort?: LogSort, onSort?: (key: string) => void): 'ascending' | 'descending' | 'none' | undefined {
  if (!onSort) return undefined
  if (sort?.key !== col) return 'none'
  return sort.dir === 'asc' ? 'ascending' : 'descending'
}

export function SortHeader({ label, col, sort, onSort }: { label: string; col: string; sort?: LogSort; onSort?: (key: string) => void }) {
  if (!onSort) return <>{label}</>
  const active = sort?.key === col
  return (
    <button type="button" onClick={() => onSort(col)} className={cn('inline-flex items-center gap-0.5 hover:text-fg', active && 'text-fg')}>
      {label}
      {active ? <span aria-hidden>{sort?.dir === 'asc' ? '▲' : '▼'}</span> : <ChevronsUpDownIcon className="size-3 opacity-50" />}
    </button>
  )
}

/**
 * 表头里的下拉筛选：和筛选栏同一套带搜索的 Combobox，只是触发器缩成一个漏斗图标加当前值，
 * 看起来是表头的一部分。菜单 fixed 定位，不会被表格的滚动容器裁掉。
 */
export function HeaderFilter({ col, filter, label = col }: { col: string; filter: ColFilter; /** 提示文字里怎么称呼这一列，默认即列名 */ label?: string }) {
  const { options, onOpen, loading } = filter
  const picked = filter.multiple ? filter.value : filter.value ? [filter.value] : []
  const name = label === col ? ` ${col} ` : label
  const common = {
    variant: 'inline' as const,
    options,
    onOpenChange: onOpen ? (o: boolean) => o && onOpen() : undefined,
    loading,
    placeholder: `全部${label === col ? ` ${col}` : label}`,
    searchPlaceholder: `搜索${label === col ? ` ${col}` : label}…`,
    className: 'min-w-0 flex-1',
    title: picked.length
      ? `只看${name}= ${picked.join('、')}，${filter.multiple ? '点击增减' : '点击切换'}`
      : `按${name}筛选`,
  }
  const clear = () => (filter.multiple ? filter.onChange([]) : filter.onChange(''))
  return (
    <span className="flex min-w-0 flex-1 items-center gap-0.5">
      {filter.multiple ? (
        <Combobox {...common} multiple value={filter.value} onChange={filter.onChange} />
      ) : (
        <Combobox {...common} value={filter.value} onChange={filter.onChange} />
      )}
      {picked.length > 0 && (
        <Hint text="取消筛选" asChild>
          <button type="button" onClick={clear} className="shrink-0 text-muted-fg hover:text-fg">
            <XIcon className="size-3" />
          </button>
        </Hint>
      )}
    </span>
  )
}

/**
 * 展开 / 收起一条日志的那个箭头。
 *
 * 整行可点是给鼠标的方便，但它**不能是唯一的入口**：`<tr>` / `<li>` 不可聚焦，键盘和读屏原来
 * 就打不开详情——而详情里才有 message 全文、属性表、trace / span 的跳转，等于半个页面够不着。
 * 所以箭头本身是个真按钮，按 ARIA 的 Disclosure 那套报 `aria-expanded`，并用 `aria-controls`
 * 指向展开出来的那一块。行上的 onClick 照旧，点按钮时别让它再冒上去翻一次。
 */
export function DisclosureToggle({ open, controls, onToggle, className }: { open: boolean; controls: string; onToggle: () => void; className?: string }) {
  return (
    <button
      type="button"
      aria-expanded={open}
      aria-controls={open ? controls : undefined}
      aria-label={open ? '收起这条日志的详情' : '展开这条日志的详情'}
      onClick={(e) => {
        e.stopPropagation()
        onToggle()
      }}
      className={cn('cursor-pointer rounded-sm text-muted-fg hover:text-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand/60', className)}
    >
      {open ? <ChevronDownIcon className="size-4" /> : <ChevronRightIcon className="size-4" />}
    </button>
  )
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

export function LogTable({ rows, dims, highlight, anchorKey, selectedSpanId, onContext, onPivot, compact, emptyText, sort, onSort, colFilters }: LogTableProps) {
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
      colFilters={colFilters}
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

function LogRows({ rows, dims, cols, highlight, anchorKey, selectedSpanId, onContext, onPivot, compact, sort, onSort, colFilters, expanded, toggle }: RowsProps & Pick<LogTableProps, 'compact' | 'sort' | 'onSort' | 'colFilters'>) {
  const from = useFrom()
  const uid = useId()
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
    /*
     * `aria-rowcount` 报的是**全部**行数，不是 DOM 里这二十行。
     *
     * 虚拟列表只渲染视口里的那一屏，读屏照 DOM 数就会说「表格，共 22 行」——而实际上有一万两千
     * 条，人会以为已经到底了。ARIA 给这种「行不全在 DOM 里」的表准备的就是这一对属性：表上报
     * 总数，每行报自己是第几行（从 1 开始，表头占掉 1）。
     */
    <table ref={tableRef} aria-rowcount={rows.length + 1} className="w-full table-fixed border-collapse text-xs">
      <thead className="sticky top-0 z-[1] bg-card text-2xs text-muted-fg shadow-[inset_0_-1px_0_var(--border)]">
        <tr aria-rowindex={1}>
          <th className="w-7" />
          <th aria-sort={ariaSort('ts_ms', sort, onSort)} className="w-[12.5rem] px-1.5 py-2 text-left font-medium">
            <SortHeader label="时间" col="ts_ms" sort={sort} onSort={onSort} />
          </th>
          <th aria-sort={ariaSort('level', sort, onSort)} className={cn('px-1.5 py-2 text-left font-medium', colFilters?.level ? 'w-24' : 'w-16')}>
            {colFilters?.level ? (
              <span className="flex min-w-0 items-center gap-1.5">
                <SortHeader label="级别" col="level" sort={sort} onSort={onSort} />
                <HeaderFilter col="level" filter={colFilters.level} />
              </span>
            ) : (
              <SortHeader label="级别" col="level" sort={sort} onSort={onSort} />
            )}
          </th>
          {cols.map((c) => (
            <th key={c} aria-sort={ariaSort(c, sort, onSort)} className={cn('px-1.5 py-2 text-left font-medium', c === 'pod' ? 'w-56' : 'w-40')}>
              {colFilters?.[c] ? (
                <span className="flex min-w-0 items-center gap-1.5">
                  <SortHeader label={c} col={c} sort={sort} onSort={onSort} />
                  <HeaderFilter col={c} filter={colFilters[c]} />
                </span>
              ) : (
                <SortHeader label={c} col={c} sort={sort} onSort={onSort} />
              )}
            </th>
          ))}
          {!compact && (
            <th aria-sort={ariaSort('logger', sort, onSort)} className="w-48 px-1.5 py-2 text-left font-medium">
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
        <tbody aria-hidden>
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
        // 展开出来那一行的 id，给箭头的 aria-controls 指。rowKey 是内容拼的（带空格），当不了 id
        const detailId = `${uid}-detail-${item.index}`
        // 一条日志一个 tbody：主行加展开行一起量高度
        return (
          <tbody key={key} data-index={item.index} ref={virtualizer.measureElement}>
            <tr
              // 表头是第 1 行，所以数据行从 2 起
              aria-rowindex={item.index + 2}
              className={cn('row-hover cursor-pointer border-b border-border/60 align-top', isAnchor && 'row-selected', open && 'bg-muted/40')}
              data-selected={isAnchor ? '1' : undefined}
              onClick={() => toggle(key)}
            >
              <td className="py-1.5 pl-2">
                <DisclosureToggle open={open} controls={detailId} onToggle={() => toggle(key)} />
              </td>
              <td className="mono px-1.5 py-1.5 whitespace-nowrap text-muted-fg tabular-nums">{formatTs(r.ts_ms)}</td>
              <td className="px-1.5 py-1.5">
                <Badge tone={levelTone(r.level)}>{r.level || '-'}</Badge>
              </td>
              {cols.map((c) => (
                <td key={c} className="truncate px-1.5 py-1.5 text-muted-fg" title={dimValue(r, c)}>
                  {onPivot ? (
                    <Hint text={`只看 ${c} = ${dimValue(r, c)}`} asChild>
                      <button
                        type="button"
                        className="max-w-full truncate hover:text-accent hover:underline"
                        onClick={(e) => {
                          e.stopPropagation()
                          onPivot(c, dimValue(r, c))
                        }}
                      >
                        {dimValue(r, c) || '-'}
                      </button>
                    </Hint>
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
                  {/* 展开时也标：标记要贴在文本断掉的那一点上，不然人得滚过一万六千字
                      才在下面的详情里看到「已截断」 */}
                  {messageTruncated(r) && (
                    <Hint text={truncationNote(r)}>
                      <span className="ml-1 text-warn">
                        · 已截断
                      </span>
                    </Hint>
                  )}
                </div>
              </td>
              <td className="mono px-1.5 py-1.5 text-2xs">
                {r.trace_id ? (
                  <Hint text={`查看链路 ${r.trace_id}`} asChild>
                    <Link
                      to={`/traces/${r.trace_id}?at=${r.ts_ms}`} state={from}
                      className={linkClass}
                      onClick={(e) => e.stopPropagation()}
                    >
                      {r.trace_id.slice(0, 8)}…
                    </Link>
                  </Hint>
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
              <tr id={detailId} className="border-b border-border/60 bg-muted/30">
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
        <tbody aria-hidden>
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
  const from = useFrom()
  const uid = useId()
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
        const detailId = `${uid}-detail-${item.index}`
        return (
          // 整行可点只是给鼠标的方便，键盘走这一行开头那个 DisclosureToggle
          // eslint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/no-noninteractive-element-interactions
          <li
            key={key}
            data-index={item.index}
            ref={virtualizer.measureElement}
            // 同理：DOM 里只有视口那几条，这一对属性才说得出「第 1203 条，共 12345 条」
            aria-setsize={rows.length}
            aria-posinset={item.index + 1}
            data-selected={isAnchor ? '1' : undefined}
            className={cn('border-b border-border/60 px-3 py-2', isAnchor && 'row-selected', open && 'bg-muted/40')}
            onClick={() => toggle(key)}
          >
            <div className="flex items-center gap-2 text-2xs text-muted-fg">
              <DisclosureToggle open={open} controls={detailId} onToggle={() => toggle(key)} className="-ml-1 shrink-0" />
              <span className="mono tabular-nums">{formatTs(r.ts_ms, { date: false })}</span>
              <Badge tone={levelTone(r.level)}>{r.level || '-'}</Badge>
              {primary && <span className="min-w-0 flex-1 truncate">{dimValue(r, primary) || '-'}</span>}
              {r.trace_id && (
                <Hint text={`查看链路 ${r.trace_id}`} asChild>
                  <Link
                    to={`/traces/${r.trace_id}?at=${r.ts_ms}`} state={from}
                    className="mono shrink-0 text-accent"
                    onClick={(e) => e.stopPropagation()}
                  >
                    {r.trace_id.slice(0, 8)}…
                  </Link>
                </Hint>
              )}
              {onContext && (
                <Hint text="查看这一行前后的日志" asChild>
                  <button
                    type="button"
                    className="-my-1 -mr-1 shrink-0 p-1 text-muted-fg"
                    onClick={(e) => {
                      e.stopPropagation()
                      onContext(r)
                    }}
                  >
                    <ListTreeIcon className="size-4" />
                  </button>
                </Hint>
              )}
            </div>
            {open ? (
              // 详情在可点的 li 里面，点它不该顺带把整条收起来
              // eslint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/no-static-element-interactions
              <div id={detailId} className="mt-2" onClick={(e) => e.stopPropagation()}>
                <ExpandedRow row={r} dims={dims} highlight={highlight} onPivot={onPivot} />
              </div>
            ) : (
              <div className="mono mt-1 line-clamp-3 break-all leading-5">
                <Highlight text={first} terms={highlight} />
                {rest && <span className="ml-1 text-muted-fg">… +{rest.split('\n').length} 行</span>}
                {messageTruncated(r) && (
                  <Hint text={truncationNote(r)}>
                    <span className="ml-1 text-warn">
                      · 已截断
                    </span>
                  </Hint>
                )}
              </div>
            )}
          </li>
        )
      })}
      {padBottom > 0 && <li aria-hidden style={{ height: padBottom }} />}
    </ul>
  )
}

export function ExpandedRow({ row, dims, highlight, onPivot }: { row: LogRow; dims: string[]; highlight?: string[]; onPivot?: (f: string, v: string) => void }) {
  const from = useFrom()
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
      {messageTruncated(row) && (
        <div className="rounded-md border border-warn/40 bg-warn-soft px-3 py-2 text-2xs text-warn">
          {truncationNote(row)}。完整内容过大时会使页面失去响应（线上曾出现单条 41 MB 的日志）；如需全文，请使用日志页的「导出」，导出内容不截断。
        </div>
      )}
      <div>
        <div className="mb-1.5 flex items-center gap-1.5 text-2xs text-muted-fg">
          <span className="font-medium">message</span>
          <CopyButton text={row.message} title="复制整条日志正文" size="xs" />
          {messageTruncated(row) && <span className="text-warn">（复制的内容同样经过截断）</span>}
        </div>
        <pre className="mono max-h-[28rem] overflow-auto rounded-md border border-border bg-card p-3 text-xs leading-5 whitespace-pre-wrap break-all">
          <Highlight text={row.message} terms={highlight} />
        </pre>
      </div>
      <div className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-xs">
        {fields
          .filter(([, v]) => v)
          .map(([k, v]) => (
            // 跟 span 属性表同一种东西：图标扫到这一行才浮现，不然十来行糊出一列图标。
            // 外面这层 display:contents 不生成盒子，键值照旧是 grid 的两个格子，只是 hover 有了着落
            <div key={k} className="group contents">
              <span className="text-muted-fg">{k}</span>
              <span className="mono flex min-w-0 items-center gap-1 break-all">
                {onPivot && !['file', 'trace_id', 'span_id', 'logger'].includes(k) ? (
                  <Hint text={`只看 ${k} = ${v}`} asChild>
                    <button type="button" className="text-left hover:text-accent hover:underline" onClick={() => onPivot(k, v)}>
                      {v}
                    </button>
                  </Hint>
                ) : k === 'trace_id' ? (
                  <Link to={`/traces/${v}?at=${row.ts_ms}`} state={from} className={linkClass}>
                    {v}
                  </Link>
                ) : k === 'span_id' ? (
                  // 带上这条日志前后的时间窗：日志页按 id 查也要裁时间（见 logsHref），
                  // 光给一个 span id 会落到它 1 小时的默认范围上，翻旧日志时就点空了
                  <Hint text="这个 span 的全部日志" asChild>
                    <Link to={logsHref({ spanId: v }, around(row.ts_ms))} className={linkClass}>
                      {v}
                    </Link>
                  </Hint>
                ) : (
                  v
                )}
                <CopyButton text={v} title={`复制 ${k}`} reveal />
              </span>
            </div>
          ))}
      </div>
    </div>
  )
}
