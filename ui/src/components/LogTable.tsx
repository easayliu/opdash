import { Fragment, useState, type ReactNode } from 'react'
import { Link } from 'react-router'
import { ChevronDownIcon, ChevronRightIcon, CopyIcon, ListTreeIcon } from 'lucide-react'
import type { LogRow } from '@/api/types'
import { Badge, Button, levelTone } from '@/components/ui'
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
  onContext?: (row: LogRow) => void
  /** 点某个值 → 加为筛选条件 */
  onPivot?: (field: string, value: string) => void
  compact?: boolean
  emptyText?: ReactNode
}

/** 一行日志的唯一键：和后端排序键一致，跟随模式去重也用它。 */
export function rowKey(r: LogRow): string {
  return `${r.ts_ms}|${r.host}|${r.file}|${r.thread}|${r.logger}|${r.message}`
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

export function LogTable({ rows, dims, highlight, anchorKey, onContext, onPivot, compact, emptyText }: LogTableProps) {
  const [expanded, setExpanded] = useState<Set<string>>(new Set())
  const cols = visibleDims(dims)
  const toggle = (k: string) =>
    setExpanded((s) => {
      const n = new Set(s)
      if (n.has(k)) n.delete(k)
      else n.add(k)
      return n
    })

  if (!rows.length) {
    return <div className="px-3 py-10 text-center text-xs text-muted-fg">{emptyText ?? '没有日志'}</div>
  }
  return (
    <table className="w-full table-fixed border-collapse text-xs">
      <thead className="sticky top-0 z-[1] bg-card text-2xs text-muted-fg">
        <tr className="border-b border-border">
          <th className="w-5" />
          <th className="w-[9.5rem] px-1 py-1.5 text-left font-medium">时间</th>
          <th className="w-14 px-1 py-1.5 text-left font-medium">级别</th>
          {cols.map((c) => (
            <th key={c} className={cn('px-1 py-1.5 text-left font-medium', c === 'pod' ? 'w-52' : 'w-36')}>
              {c}
            </th>
          ))}
          {!compact && <th className="w-44 px-1 py-1.5 text-left font-medium">logger</th>}
          <th className="px-1 py-1.5 text-left font-medium">message</th>
          <th className="w-24 px-1 py-1.5 text-left font-medium">trace</th>
          {onContext && <th className="w-8" />}
        </tr>
      </thead>
      <tbody>
        {rows.map((r) => {
          const key = rowKey(r)
          const open = expanded.has(key)
          const [first, rest] = splitFirstLine(r.message)
          const isAnchor = anchorKey === key
          return (
            <Fragment key={key}>
              <tr
                className={cn('row-hover cursor-pointer border-b border-border/60 align-top', isAnchor && 'row-selected', open && 'bg-muted/40')}
                onClick={() => toggle(key)}
              >
                <td className="py-1 pl-1 text-muted-fg">
                  {open ? <ChevronDownIcon className="size-3.5" /> : <ChevronRightIcon className="size-3.5" />}
                </td>
                <td className="mono px-1 py-1 whitespace-nowrap text-muted-fg tabular-nums">{formatTs(r.ts_ms)}</td>
                <td className="px-1 py-1">
                  <Badge tone={levelTone(r.level)}>{r.level || '-'}</Badge>
                </td>
                {cols.map((c) => (
                  <td key={c} className="truncate px-1 py-1 text-muted-fg" title={dimValue(r, c)}>
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
                  <td className="mono truncate px-1 py-1 text-muted-fg" title={r.logger}>
                    {r.logger}
                  </td>
                )}
                <td className="px-1 py-1">
                  <div className={cn('mono break-all', !open && 'line-clamp-2')}>
                    <Highlight text={first} terms={highlight} />
                    {!open && rest && <span className="ml-1 text-muted-fg">… +{rest.split('\n').length} 行</span>}
                  </div>
                </td>
                <td className="mono px-1 py-1 text-2xs">
                  {r.trace_id ? (
                    <Link
                      to={`/traces/${r.trace_id}`}
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
                  <td className="px-1 py-0.5">
                    <Button
                      variant="ghost"
                      size="xs"
                      className="px-1"
                      title="查看这一行前后的日志（同一容器日志流）"
                      onClick={(e) => {
                        e.stopPropagation()
                        onContext(r)
                      }}
                    >
                      <ListTreeIcon className="size-3.5" />
                    </Button>
                  </td>
                )}
              </tr>
              {open && (
                <tr className="border-b border-border/60 bg-muted/30">
                  <td />
                  <td colSpan={cols.length + (compact ? 4 : 5) + (onContext ? 1 : 0)} className="px-1 py-2">
                    <ExpandedRow row={r} dims={dims} highlight={highlight} onPivot={onPivot} />
                  </td>
                </tr>
              )}
            </Fragment>
          )
        })}
      </tbody>
    </table>
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
    <div className="space-y-2">
      <pre className="mono max-h-[28rem] overflow-auto rounded-md border border-border bg-card p-2 text-xs leading-5 whitespace-pre-wrap break-all">
        <Highlight text={row.message} terms={highlight} />
      </pre>
      <div className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-2xs">
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
                  <Link to={`/traces/${v}`} className="text-accent hover:underline">
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
                  <CopyIcon className="size-3" />
                </button>
              </span>
            </Fragment>
          ))}
      </div>
    </div>
  )
}
