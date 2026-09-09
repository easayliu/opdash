import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { useVirtualizer } from '@tanstack/react-virtual'
import { Link } from 'react-router'
import { ArrowDownIcon, ListTreeIcon } from 'lucide-react'
import type { LogRow } from '@/api/types'
import { ExpandedRow, Highlight, visibleDims } from '@/components/LogTable'
import { Button } from '@/components/ui'
import { levelColor } from '@/lib/colors'
import { useRowKeys } from '@/lib/log-row'
import { formatTs } from '@/lib/time'
import { cn, scrollParent, splitFirstLine } from '@/lib/utils'

export interface LogStreamProps {
  /** 时间升序，最新的一行在最后 */
  rows: LogRow[]
  dims: string[]
  highlight?: string[]
  onContext?: (row: LogRow) => void
  onPivot?: (field: string, value: string) => void
  emptyText?: ReactNode
}

/** 估算的一行高度（px）；真实高度渲染后再量 */
const EST_LINE_H = 20

/** 离底多近算「贴着底」。一行 20px，留一行的余量：手滚一格就脱离，新行不会再把人拽回底部 */
const STICK_SLOP = 24

/**
 * 终端式的日志流：时间正序、新行追加在底部、自动滚到底，像 `kubectl logs -f`。
 * 往上滚就停住不再自动跟（右下角给个「回到底部」），滚回底部又自动接上。
 *
 * 和 [`LogTable`] 的区别只是方向和密度：一行一条、等宽字体、级别用颜色而不是徽章，
 * 点开的详情复用同一个 [`ExpandedRow`]。
 */
export function LogStream({ rows, dims, highlight, onContext, onPivot, emptyText }: LogStreamProps) {
  const hostRef = useRef<HTMLDivElement>(null)
  const scrollEl = useRef<HTMLElement | null>(null)
  const [expanded, setExpanded] = useState<Set<string>>(new Set())
  const [atBottom, setAtBottom] = useState(true)
  // 贴底与否要在滚动回调里立刻读到，state 的值会滞后一帧
  const stick = useRef(true)
  const cols = visibleDims(dims)
  const keys = useRowKeys(rows)

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => (scrollEl.current ??= scrollParent(hostRef.current)),
    estimateSize: () => EST_LINE_H,
    overscan: 12,
    getItemKey: (i) => keys[i],
  })

  const toBottom = useCallback(() => {
    const box = scrollEl.current
    if (box) box.scrollTop = box.scrollHeight
  }, [])

  useEffect(() => {
    const box = (scrollEl.current ??= scrollParent(hostRef.current))
    if (!box) return
    const onScroll = () => {
      const bottom = box.scrollHeight - box.scrollTop - box.clientHeight <= STICK_SLOP
      stick.current = bottom
      setAtBottom((prev) => (prev === bottom ? prev : bottom))
    }
    box.addEventListener('scroll', onScroll, { passive: true })
    return () => box.removeEventListener('scroll', onScroll)
  }, [])

  // 新行到了就滚到底。行高是渲染完才量的，量完总高度会变，所以下一帧再补一次
  useLayoutEffect(() => {
    if (!stick.current) return
    toBottom()
    const id = requestAnimationFrame(toBottom)
    return () => cancelAnimationFrame(id)
  }, [rows.length, expanded, toBottom])

  if (!rows.length) {
    return <div className="px-4 py-12 text-center text-sm text-muted-fg">{emptyText ?? '没有日志'}</div>
  }

  const items = virtualizer.getVirtualItems()
  const padTop = items.length ? items[0].start : 0
  const padBottom = items.length ? virtualizer.getTotalSize() - items[items.length - 1].end : 0
  const toggle = (k: string) =>
    setExpanded((s) => {
      const n = new Set(s)
      if (n.has(k)) n.delete(k)
      else n.add(k)
      return n
    })

  return (
    <div ref={hostRef} className="relative">
      {padTop > 0 && <div style={{ height: padTop }} />}
      {items.map((item) => {
        const r = rows[item.index]
        const key = keys[item.index]
        const open = expanded.has(key)
        const [first, rest] = splitFirstLine(r.message)
        return (
          <div key={key} data-index={item.index} ref={virtualizer.measureElement}>
            <div
              className={cn('row-hover group flex cursor-pointer items-start gap-2 px-3 leading-5 md:px-4', open && 'bg-muted/40')}
              onClick={() => toggle(key)}
            >
              <span className="mono shrink-0 text-2xs text-muted-fg tabular-nums">{formatTs(r.ts_ms, { date: false })}</span>
              <span className="mono w-11 shrink-0 text-2xs uppercase" style={{ color: levelColor(r.level) }} title={r.level}>
                {(r.level || '-').slice(0, 5)}
              </span>
              {cols[0] && (
                <span className="mono w-32 shrink-0 truncate text-2xs text-muted-fg" title={dimValue(r, cols[0])}>
                  {onPivot ? (
                    <button
                      type="button"
                      className="max-w-full truncate hover:text-accent hover:underline"
                      title={`只看 ${cols[0]} = ${dimValue(r, cols[0])}`}
                      onClick={(e) => {
                        e.stopPropagation()
                        onPivot(cols[0], dimValue(r, cols[0]))
                      }}
                    >
                      {dimValue(r, cols[0]) || '-'}
                    </button>
                  ) : (
                    dimValue(r, cols[0]) || '-'
                  )}
                </span>
              )}
              <span className={cn('mono min-w-0 flex-1 text-xs break-all', !open && 'truncate')}>
                <Highlight text={first} terms={highlight} />
                {!open && rest && <span className="ml-1 text-muted-fg">… +{rest.split('\n').length} 行</span>}
              </span>
              {r.trace_id && (
                <Link
                  to={`/traces/${r.trace_id}?at=${r.ts_ms}`}
                  className="mono shrink-0 text-2xs text-accent hover:underline"
                  title={`查看链路 ${r.trace_id}`}
                  onClick={(e) => e.stopPropagation()}
                >
                  {r.trace_id.slice(0, 8)}…
                </Link>
              )}
              {onContext && (
                <button
                  type="button"
                  className="shrink-0 text-muted-fg opacity-0 group-hover:opacity-100 hover:text-fg"
                  title="查看这一行前后的日志（同一容器日志流）"
                  onClick={(e) => {
                    e.stopPropagation()
                    onContext(r)
                  }}
                >
                  <ListTreeIcon className="size-3.5" />
                </button>
              )}
            </div>
            {open && (
              <div className="border-y border-border/60 bg-muted/30 px-3 py-3 md:px-4" onClick={(e) => e.stopPropagation()}>
                <ExpandedRow row={r} dims={dims} highlight={highlight} onPivot={onPivot} />
              </div>
            )}
          </div>
        )
      })}
      {padBottom > 0 && <div style={{ height: padBottom }} />}
      {!atBottom && (
        <div className="sticky bottom-3 z-[1] flex justify-center">
          <Button size="sm" onClick={toBottom} title="回到底部，继续跟着新日志滚">
            <ArrowDownIcon className="size-4" />
            回到底部
          </Button>
        </div>
      )}
    </div>
  )
}

function dimValue(row: LogRow, dim: string): string {
  const v = row[dim]
  return v === null || v === undefined ? '' : String(v)
}
