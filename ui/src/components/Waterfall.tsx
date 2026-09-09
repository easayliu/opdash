import { Fragment, useEffect, useMemo, useRef, useState } from 'react'
import { Link } from 'react-router'
import { AlertTriangleIcon, ChevronDownIcon, ChevronRightIcon, CopyIcon } from 'lucide-react'
import type { AttrValue, Span } from '@/api/types'
import { Badge, Button } from '@/components/ui'
import type { ColorAssigner } from '@/lib/colors'
import { formatDuration, formatTsMicro } from '@/lib/time'
import { cn, copyText } from '@/lib/utils'

export interface SpanNode {
  span: Span
  children: SpanNode[]
  depth: number
  /** parent_span_id 指向了一个不在结果里的 span */
  orphan: boolean
}

export interface SpanTree {
  roots: SpanNode[]
  startUs: number
  endUs: number
}

/** 按 parent_span_id 建树。根 = parent 为空的；parent 找不到的挂到顶层并标记。 */
export function buildTree(spans: Span[]): SpanTree {
  const byId = new Map<string, SpanNode>()
  for (const s of spans) byId.set(s.span_id, { span: s, children: [], depth: 0, orphan: false })
  const roots: SpanNode[] = []
  for (const node of byId.values()) {
    const pid = node.span.parent_span_id
    const parent = pid ? byId.get(pid) : undefined
    if (parent && parent !== node) parent.children.push(node)
    else {
      node.orphan = !!pid
      roots.push(node)
    }
  }
  const byStart = (a: SpanNode, b: SpanNode) => a.span.start_us - b.span.start_us
  roots.sort(byStart)
  const walk = (n: SpanNode, depth: number) => {
    n.depth = depth
    n.children.sort(byStart)
    for (const c of n.children) walk(c, depth + 1)
  }
  for (const r of roots) walk(r, 0)
  let startUs = Infinity
  let endUs = -Infinity
  for (const s of spans) {
    startUs = Math.min(startUs, s.start_us)
    endUs = Math.max(endUs, s.start_us + s.duration_ns / 1000)
  }
  if (!Number.isFinite(startUs)) startUs = endUs = 0
  return { roots, startUs, endUs: Math.max(endUs, startUs + 1) }
}

function kindTone(kind: string): 'muted' | 'info' | 'accent' | 'ok' {
  switch (kind) {
    case 'Server':
      return 'accent'
    case 'Consumer':
      return 'ok'
    case 'Client':
    case 'Producer':
      return 'info'
    default:
      return 'muted'
  }
}

/** 从属性里拼一句人看得懂的副标题：HTTP 方法 + 路径 / 状态码，SQL 语句，消息 topic。 */
export function spanSubtitle(s: Span): string {
  const a = s.attributes
  const str = (k: string) => (a[k] === undefined || a[k] === null ? '' : String(a[k]))
  if (str('http.request.method') || str('http.method')) {
    const status = str('http.response.status_code') || str('http.status_code')
    const path = str('url.path') || str('http.target') || str('url.full') || str('http.url') || str('http.route')
    return [str('http.request.method') || str('http.method'), path, status && `→ ${status}`].filter(Boolean).join(' ')
  }
  if (str('db.system')) return `${str('db.system')} ${str('db.statement') || str('db.operation')}`.trim()
  if (str('messaging.system')) return `${str('messaging.system')} ${str('messaging.destination.name')} ${str('messaging.operation')}`.trim()
  if (str('rpc.system')) return `${str('rpc.system')} ${str('rpc.service')}/${str('rpc.method')}`
  if (str('code.namespace')) return `${str('code.namespace')}.${str('code.function')}`
  return ''
}

/** 时间轴上正在看的窗口（µs 绝对时间）。 */
export interface TimeWindow {
  startUs: number
  endUs: number
}

/**
 * 根请求的窗口：根 span 本身 + 在它结束前就开始的同步子孙。
 * 消息消费、定时补偿这类在根返回之后才跑的 span 不算进来——
 * 它们常常晚几分钟甚至半小时，按全跨度画的话根请求里的几十个 span 会全部挤成一条线。
 */
export function rootWindow(tree: SpanTree): TimeWindow | null {
  const root = tree.roots[0]
  if (!root) return null
  const start = root.span.start_us
  const rootEnd = start + root.span.duration_ns / 1000
  let end = rootEnd
  const walk = (n: SpanNode) => {
    for (const c of n.children) {
      if (c.span.start_us > rootEnd) continue
      end = Math.max(end, c.span.start_us + c.span.duration_ns / 1000)
      walk(c)
    }
  }
  walk(root)
  return { startUs: start, endUs: Math.max(end, start + 1) }
}

/**
 * 默认窗口：全跨度（最早 span 开始到最晚 span 结束），打开就能看到消息消费这类异步 span 在哪。
 * 根请求里的同步 span 被挤成一条线时，点表头的「根请求」切到 `rootWindow`。
 */
/** 行不在（或被 `stickyPx` 高的表头挡住）滚动容器的可视区里时，把它滚到中间；已经看得见就不动。 */
function scrollRowIntoView(el: HTMLElement, stickyPx: number) {
  // 找真正在纵向滚动的祖先：瀑布图自己那层 overflow-auto 只管横向，高度没限制，得再往上找
  let box: HTMLElement | null = el.parentElement
  while (box && !(/(auto|scroll)/.test(getComputedStyle(box).overflowY) && box.scrollHeight > box.clientHeight + 1)) {
    box = box.parentElement
  }
  if (!box) return
  const r = el.getBoundingClientRect()
  const b = box.getBoundingClientRect()
  if (r.top >= b.top + stickyPx && r.bottom <= b.bottom) return
  const target = box.scrollTop + (r.top - b.top) - (b.height - r.height) / 2
  box.scrollTo({ top: Math.max(0, target), behavior: 'smooth' })
}

export function defaultWindow(tree: SpanTree): TimeWindow {
  return { startUs: tree.startUs, endUs: tree.endUs }
}

interface Props {
  tree: SpanTree
  colors: ColorAssigner
  selected: string | null
  onSelect: (spanId: string | null) => void
}

const ROW_H = 30
const LEFT_W = 400
/** 条至少画这么宽，不然 1ms 的 span 在 30s 的轴上根本看不见 */
const MIN_BAR_PCT = 0.15

export function Waterfall({ tree, colors, selected, onSelect }: Props) {
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set())
  const [zoom, setZoom] = useState<TimeWindow | null>(null)
  const [drag, setDrag] = useState<{ x0: number; x1: number } | null>(null)
  useEffect(() => setZoom(null), [tree])

  // 外部选中一个 span（比如点顶部的错误数跳过来）：把它折叠着的祖先展开，行渲染出来后再滚到视野里
  const parentOf = useMemo(() => {
    const m = new Map<string, string>()
    const walk = (n: SpanNode) => {
      for (const c of n.children) {
        m.set(c.span.span_id, n.span.span_id)
        walk(c)
      }
    }
    tree.roots.forEach(walk)
    return m
  }, [tree])
  const listRef = useRef<HTMLDivElement>(null)
  const pendingScroll = useRef<string | null>(null)
  useEffect(() => {
    if (!selected) return
    pendingScroll.current = selected
    const hidden: string[] = []
    for (let p = parentOf.get(selected); p; p = parentOf.get(p)) if (collapsed.has(p)) hidden.push(p)
    if (hidden.length) {
      setCollapsed((s) => {
        const n = new Set(s)
        hidden.forEach((id) => n.delete(id))
        return n
      })
    }
    // collapsed 只在这里读一次，展开之后由下面那个 effect 接着滚
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected, parentOf])
  useEffect(() => {
    const id = pendingScroll.current
    if (!id) return
    const el = listRef.current?.querySelector<HTMLElement>(`[data-span-id="${id}"]`)
    if (!el) return
    pendingScroll.current = null
    scrollRowIntoView(el, 32)
  })

  const rows = useMemo(() => {
    const out: SpanNode[] = []
    const walk = (n: SpanNode) => {
      out.push(n)
      if (!collapsed.has(n.span.span_id)) n.children.forEach(walk)
    }
    tree.roots.forEach(walk)
    return out
  }, [tree, collapsed])

  const full = useMemo<TimeWindow>(() => ({ startUs: tree.startUs, endUs: tree.endUs }), [tree])
  const root = useMemo(() => rootWindow(tree), [tree])
  const view = zoom ?? defaultWindow(tree)
  const viewLen = Math.max(1, view.endUs - view.startUs)
  const fullLen = Math.max(1, full.endUs - full.startUs)
  const isFull = view.startUs <= full.startUs && view.endUs >= full.endUs
  const isRoot = !!root && view.startUs === root.startUs && view.endUs === root.endUs
  const rootIsPartial = !!root && root.endUs - root.startUs < fullLen * 0.999
  const x = (us: number) => ((us - view.startUs) / viewLen) * 100
  const offsetLabel = (us: number) => `+${formatDuration((us - tree.startUs) * 1000)}`
  const ticks = [0, 0.25, 0.5, 0.75, 1]

  const outside = useMemo(() => {
    let before = 0
    let after = 0
    for (const n of rows) {
      const s = n.span.start_us
      const e = s + n.span.duration_ns / 1000
      if (e < view.startUs) before++
      else if (s > view.endUs) after++
    }
    return { before, after }
  }, [rows, view])

  // 在时间轴表头上拖一段来放大；双击还原
  const onPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return
    const rect = e.currentTarget.getBoundingClientRect()
    const px = e.clientX - rect.left
    setDrag({ x0: px, x1: px })
    e.currentTarget.setPointerCapture(e.pointerId)
  }
  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    if (!drag) return
    const rect = e.currentTarget.getBoundingClientRect()
    setDrag({ x0: drag.x0, x1: Math.max(0, Math.min(rect.width, e.clientX - rect.left)) })
  }
  const onPointerUp = (e: React.PointerEvent<HTMLDivElement>) => {
    if (!drag) return
    const rect = e.currentTarget.getBoundingClientRect()
    const a = Math.min(drag.x0, drag.x1)
    const b = Math.max(drag.x0, drag.x1)
    setDrag(null)
    if (b - a < 4 || rect.width <= 0) return
    const startUs = view.startUs + (a / rect.width) * viewLen
    const endUs = view.startUs + (b / rect.width) * viewLen
    if (endUs - startUs < 1) return
    setZoom({ startUs, endUs })
  }

  return (
    <div ref={listRef} className="relative min-w-0 overflow-auto">
      <div className="sticky top-0 z-[1] flex h-8 border-b border-border bg-card text-2xs text-muted-fg" style={{ minWidth: LEFT_W + 400 }}>
        <div className="flex shrink-0 items-center gap-1 px-3" style={{ width: LEFT_W }}>
          <span className="mr-auto">服务 / 操作</span>
          {rootIsPartial && (
            <Button size="xs" variant="ghost" active={isRoot} onClick={() => setZoom(root)} title="只看根请求的时间窗口（不含返回之后才跑的异步 span）">
              根请求 {formatDuration((root!.endUs - root!.startUs) * 1000)}
            </Button>
          )}
          {(rootIsPartial || !isFull) && (
            <Button size="xs" variant="ghost" active={isFull} onClick={() => setZoom(full)} title="最早 span 开始到最晚 span 结束">
              全部 {formatDuration(fullLen * 1000)}
            </Button>
          )}
        </div>
        <div
          className="relative flex-1 cursor-col-resize select-none"
          title="拖动选择范围放大；双击还原"
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={() => setDrag(null)}
          onDoubleClick={() => setZoom(null)}
        >
          {ticks.map((t) => (
            <span
              key={t}
              className={cn('absolute top-0 leading-8 tabular-nums', t === 0 ? 'pl-1.5' : t === 1 ? 'pr-1.5' : '-translate-x-1/2')}
              // 两端的刻度给「窗口外 N 个」的角标让位
              style={t === 1 ? { right: outside.after > 0 ? '3.5rem' : 0 } : { left: t === 0 && outside.before > 0 ? '3.5rem' : `${t * 100}%` }}
            >
              {offsetLabel(view.startUs + viewLen * t)}
            </span>
          ))}
          {outside.before > 0 && (
            <span className="absolute top-0 left-0 rounded-br bg-warn-soft px-1.5 leading-[1.125rem] text-warn" title={`${outside.before} 个 span 在窗口之前`}>
              ◂ {outside.before}
            </span>
          )}
          {outside.after > 0 && (
            <span className="absolute top-0 right-0 rounded-bl bg-warn-soft px-1.5 leading-[1.125rem] text-warn" title={`${outside.after} 个 span 在窗口之后（异步）`}>
              {outside.after} ▸
            </span>
          )}
        </div>
      </div>
      {drag && Math.abs(drag.x1 - drag.x0) >= 4 && (
        <div
          className="pointer-events-none absolute inset-y-0 z-[2] border-x border-accent bg-accent/15"
          style={{ left: LEFT_W + Math.min(drag.x0, drag.x1), width: Math.abs(drag.x1 - drag.x0) }}
        />
      )}
      {rows.map((n) => {
        const s = n.span
        const isSel = selected === s.span_id
        const isErr = s.status === 'Error'
        const color = colors.color(s.service)
        const hasKids = n.children.length > 0
        const startUs = s.start_us
        const endUs = startUs + s.duration_ns / 1000
        const x0 = x(startUs)
        const x1 = x(endUs)
        const before = x1 < 0
        const after = x0 > 100
        const left = Math.max(0, x0)
        const right = Math.min(100, Math.max(x1, left + MIN_BAR_PCT))
        const width = right - left
        const dur = formatDuration(s.duration_ns)
        // 耗时标签：条后面放得下就放后面，不然放前面，再不然压在条上
        const labelAfter = right < 84
        const labelBefore = !labelAfter && left > 16
        return (
          <div
            key={s.span_id}
            data-span-id={s.span_id}
            className={cn('row-hover flex cursor-pointer border-b border-border/50', isSel && 'row-selected')}
            style={{ height: ROW_H, minWidth: LEFT_W + 400 }}
            onClick={() => onSelect(isSel ? null : s.span_id)}
          >
            <div className="flex shrink-0 items-center gap-1.5 overflow-hidden pr-3" style={{ width: LEFT_W, paddingLeft: 8 + n.depth * 16 }}>
              <button
                type="button"
                className={cn('shrink-0 text-muted-fg', !hasKids && 'invisible')}
                onClick={(e) => {
                  e.stopPropagation()
                  setCollapsed((c) => {
                    const next = new Set(c)
                    if (next.has(s.span_id)) next.delete(s.span_id)
                    else next.add(s.span_id)
                    return next
                  })
                }}
                title={collapsed.has(s.span_id) ? '展开' : '折叠'}
              >
                {collapsed.has(s.span_id) ? <ChevronRightIcon className="size-4" /> : <ChevronDownIcon className="size-4" />}
              </button>
              <span className="inline-block h-4 w-1 shrink-0 rounded-sm" style={{ background: color }} />
              <span className="truncate text-xs">
                <span className="text-muted-fg">{s.service}</span> <span className="font-medium">{s.name}</span>
              </span>
              {isErr && <AlertTriangleIcon className="size-4 shrink-0 text-danger" aria-label="错误" />}
              {n.orphan && (
                <Badge tone="warn" title={`父 span ${s.parent_span_id} 不在结果里（采样或未入库）`}>
                  父缺失
                </Badge>
              )}
            </div>
            <div className="relative flex-1 overflow-hidden">
              {ticks.slice(1, -1).map((t) => (
                <span key={t} className="absolute inset-y-0 border-l border-border/60" style={{ left: `${t * 100}%` }} />
              ))}
              {before || after ? (
                <span
                  className={cn('absolute top-0 text-2xs leading-[30px] whitespace-nowrap text-muted-fg tabular-nums', before ? 'left-1.5' : 'right-1.5')}
                  title={`${s.service} ${s.name} ${dur}，开始于 ${offsetLabel(startUs)}，在当前窗口之${before ? '前' : '后'}`}
                >
                  {before ? `◂ ${offsetLabel(startUs)} · ${dur}` : `${offsetLabel(startUs)} · ${dur} ▸`}
                </span>
              ) : (
                <>
                  <div
                    className={cn('absolute top-[7px] h-4 rounded-sm', isErr && 'ring-1 ring-danger', x0 < 0 && 'rounded-l-none', x1 > 100 && 'rounded-r-none')}
                    style={{ left: `${left}%`, width: `${width}%`, background: color, opacity: isErr ? 0.9 : 0.75 }}
                    title={`${s.service} ${s.name} ${dur}，开始于 ${offsetLabel(startUs)}`}
                  />
                  <span
                    className={cn(
                      'absolute top-0 text-2xs leading-[30px] whitespace-nowrap tabular-nums',
                      labelAfter || labelBefore ? 'text-muted-fg' : 'text-fg',
                    )}
                    style={labelAfter ? { left: `${right}%`, marginLeft: 4 } : labelBefore ? { right: `${100 - left}%`, marginRight: 4 } : { left: `${left}%`, marginLeft: 4 }}
                  >
                    {dur}
                  </span>
                </>
              )}
            </div>
          </div>
        )
      })}
    </div>
  )
}

function fmtValue(v: AttrValue): string {
  if (v === null || v === undefined) return ''
  if (typeof v === 'object') return JSON.stringify(v)
  return String(v)
}

function KV({ entries }: { entries: [string, AttrValue][] }) {
  if (!entries.length) return <div className="px-4 py-4 text-xs text-muted-fg">（无）</div>
  return (
    <div className="grid grid-cols-[minmax(8rem,auto)_1fr] gap-x-4 gap-y-1 px-4 py-3 text-xs">
      {entries.map(([k, v]) => (
        <Fragment key={k}>
          <span className="mono truncate text-muted-fg" title={k}>
            {k}
          </span>
          <span className="mono min-w-0 break-all">{fmtValue(v)}</span>
        </Fragment>
      ))}
    </div>
  )
}

/** 右侧的 span 详情面板。 */
export function SpanPanel({ span, traceStartUs, onClose, onShowLogs }: { span: Span; traceStartUs: number; onClose: () => void; onShowLogs: () => void }) {
  const [tab, setTab] = useState<'attrs' | 'resource' | 'events' | 'links'>('attrs')
  const attrs = Object.entries(span.attributes)
  const resource = Object.entries(span.resource)
  const subtitle = spanSubtitle(span)
  return (
    <aside className="flex min-h-0 w-[30rem] shrink-0 flex-col border-l border-border bg-card">
      <header className="border-b border-border px-4 py-3">
        <div className="flex items-start justify-between gap-2">
          <div className="min-w-0">
            <div className="truncate text-base font-semibold" title={span.name}>
              {span.name}
            </div>
            <div className="truncate text-xs text-muted-fg" title={subtitle}>
              {span.service}
              {subtitle && ` · ${subtitle}`}
            </div>
          </div>
          <Button variant="ghost" size="xs" onClick={onClose} title="关闭">
            ✕
          </Button>
        </div>
        <div className="mt-2.5 flex flex-wrap items-center gap-2 text-xs">
          <Badge tone={kindTone(span.kind)}>{span.kind}</Badge>
          {span.status === 'Error' ? <Badge tone="danger">Error{span.status_message ? `: ${span.status_message}` : ''}</Badge> : span.status === 'Ok' ? <Badge tone="ok">Ok</Badge> : null}
          <span className="text-muted-fg">耗时</span>
          <span className="font-semibold tabular-nums">{formatDuration(span.duration_ns)}</span>
          <span className="text-muted-fg">开始</span>
          <span className="tabular-nums">{formatTsMicro(span.start_us)}</span>
          <span className="text-muted-fg">（+{formatDuration((span.start_us - traceStartUs) * 1000)}）</span>
        </div>
        <div className="mono mt-1.5 flex items-center gap-1.5 text-2xs text-muted-fg">
          span {span.span_id}
          <button type="button" onClick={() => copyText(span.span_id)} title="复制 span id" className="hover:text-fg">
            <CopyIcon className="size-3" />
          </button>
          <Button size="xs" variant="ghost" className="ml-auto" onClick={onShowLogs}>
            只看这个 span 的日志
          </Button>
        </div>
      </header>
      <div className="flex border-b border-border text-xs">
        {(
          [
            ['attrs', `属性 ${attrs.length}`],
            ['resource', `资源 ${resource.length}`],
            ['events', `事件 ${span.events.length}`],
            ['links', `链接 ${span.links.length}`],
          ] as const
        ).map(([k, label]) => (
          <button
            key={k}
            type="button"
            onClick={() => setTab(k)}
            className={cn('px-4 py-2 font-medium text-muted-fg hover:text-fg', tab === k && 'border-b-2 border-accent text-accent')}
          >
            {label}
          </button>
        ))}
      </div>
      <div className="min-h-0 flex-1 overflow-auto">
        {tab === 'attrs' && <KV entries={attrs} />}
        {tab === 'resource' && <KV entries={resource} />}
        {tab === 'events' &&
          (span.events.length ? (
            span.events.map((e, i) => {
              const stack = e.attributes['exception.stacktrace']
              const rest = Object.entries(e.attributes).filter(([k]) => k !== 'exception.stacktrace')
              return (
                <div key={i} className="border-b border-border/60">
                  <div className="flex items-center gap-2 px-4 pt-3 text-sm">
                    <span className="font-medium">{e.name}</span>
                    <span className="text-2xs text-muted-fg tabular-nums">+{formatDuration((e.ts_us - span.start_us) * 1000)}</span>
                  </div>
                  <KV entries={rest} />
                  {stack !== undefined && (
                    <pre className="mono mx-4 mb-3 max-h-80 overflow-auto rounded-md border border-border bg-muted/40 p-3 text-2xs leading-[1.125rem] whitespace-pre-wrap">{fmtValue(stack)}</pre>
                  )}
                </div>
              )
            })
          ) : (
            <div className="px-4 py-4 text-xs text-muted-fg">（无事件）</div>
          ))}
        {tab === 'links' &&
          (span.links.length ? (
            span.links.map((l, i) => (
              <div key={i} className="border-b border-border/60 px-4 py-3 text-xs">
                <Link to={`/traces/${l.trace_id}?span=${l.span_id}`} className="mono text-accent hover:underline">
                  {l.trace_id} / {l.span_id}
                </Link>
                <KV entries={Object.entries(l.attributes)} />
              </div>
            ))
          ) : (
            <div className="px-4 py-4 text-xs text-muted-fg">（无链接）</div>
          ))}
      </div>
    </aside>
  )
}
