import { useMemo, useState, type PointerEvent } from 'react'
import { ChartTooltip } from './Tooltip'
import { MAX_SPOKEN_SERIES, andMore, chartSummary } from './describe'
import { useWidth } from './useWidth'
import { bucketDomain, formatCompact, niceMax, niceTicks, timeTicks } from './axis'
import { formatTick, formatTs } from '@/lib/time'
import { cn } from '@/lib/utils'

/** 图上标的事件（进程重启 / pod 启动 / 发布）：一条虚线竖线 */
export interface ChartEvent {
  t_ms: number
  label: string
}

export interface BarSeries {
  key: string
  label: string
  color: string
}

export interface BarBucket {
  t_ms: number
  values: Record<string, number>
}

interface Props {
  fromMs: number
  toMs: number
  widthMs: number
  buckets: BarBucket[]
  series: BarSeries[]
  height?: number
  stale?: boolean
  /** 拖一段时间 → 缩小范围 */
  onBrush?: (fromMs: number, toMs: number) => void
  /** 标在图上的事件（重启 / 发布），画成虚线竖线 */
  events?: ChartEvent[]
  /** 点某根柱：给出这个桶的时刻、点中的那一段（堆叠里的哪条线）和位置 */
  onPointClick?: (at: { tMs: number; seriesKey?: string; x: number; y: number }) => void
  /** 别的图上鼠标停在哪个时刻：画一条同位置的竖线（同一块看板的图共用一根十字线） */
  syncTs?: number | null
  onHoverTs?: (tMs: number | null) => void
  /** 数值怎么显示（指标页要带单位）。不给就是纯数字 */
  format?: (v: number) => string
  /** 对比时段同一格的量，画成垫在柱子后面的浅灰影子——形状一比就知道是「这个时段本来就这样」
   *  还是「今天不一样」（和总览卡上的迷你趋势同一套读法）。值从 `values[ghost.key]` 取，
   *  不参与堆叠，但会算进纵轴最大值，不然影子会撑出画布 */
  ghost?: { key: string; label: string }
  /** 读屏念的那句话里，这张图叫什么。不给就是「堆叠柱状图」 */
  label?: string
  className?: string
}

const M = { left: 48, right: 8, top: 8, bottom: 22 }

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v))

/** 按时间分桶的堆叠柱状图：日志直方图、请求量 / 错误数都用它。 */
export function StackedBars({ fromMs, toMs, widthMs, buckets, series, height = 140, stale, onBrush, onPointClick, events, syncTs, onHoverTs, format = formatCompact, ghost, label, className }: Props) {
  const [ref, width] = useWidth<HTMLDivElement>()
  const [hover, setHover] = useState<{ x: number; y: number; bucket: BarBucket } | null>(null)
  const [brush, setBrush] = useState<{ x0: number; x1: number } | null>(null)

  const W = Math.max(0, width - M.left - M.right)
  const H = height - M.top - M.bottom
  // 横轴要盖住实际拿到的桶，不能只按请求的范围算——首尾两个桶会越界（见 bucketDomain）
  const { x0, x1 } = bucketDomain(fromMs, toMs, buckets, widthMs)
  const span = Math.max(1, x1 - x0)
  const xOf = (t: number) => M.left + ((t - x0) / span) * W
  const tOf = (x: number) => x0 + ((x - M.left) / Math.max(1, W)) * span

  const ghostKey = ghost?.key
  const max = useMemo(
    () =>
      Math.max(
        0,
        ...buckets.map((b) => Math.max(series.reduce((s, k) => s + (b.values[k.key] ?? 0), 0), ghostKey ? (b.values[ghostKey] ?? 0) : 0)),
      ),
    [buckets, series, ghostKey],
  )
  const yMax = niceMax(max)
  const yOf = (v: number) => M.top + H - (yMax > 0 ? (v / yMax) * H : 0)
  const slot = (widthMs / span) * W
  const barW = Math.max(1, Math.min(24, slot - 2))

  const onMove = (e: PointerEvent<SVGSVGElement>) => {
    const rect = e.currentTarget.getBoundingClientRect()
    const x = e.clientX - rect.left
    const y = e.clientY - rect.top
    if (brush) setBrush({ ...brush, x1: x })
    const idx = Math.floor((tOf(x) - x0) / widthMs)
    const bucket = buckets.find((b) => Math.floor((b.t_ms - x0) / widthMs) === idx)
    setHover(bucket ? { x, y, bucket } : null)
    onHoverTs?.(bucket ? bucket.t_ms : null)
  }
  const onDown = (e: PointerEvent<SVGSVGElement>) => {
    if (!onBrush) return
    const rect = e.currentTarget.getBoundingClientRect()
    const x = e.clientX - rect.left
    setBrush({ x0: x, x1: x })
    e.currentTarget.setPointerCapture(e.pointerId)
  }
  const onUp = (e: PointerEvent<SVGSVGElement>) => {
    const dragged = brush && Math.abs(brush.x1 - brush.x0) > 4
    if (brush && onBrush && dragged) {
      const [a, b] = [Math.min(brush.x0, brush.x1), Math.max(brush.x0, brush.x1)]
      onBrush(Math.max(fromMs, tOf(a)), Math.min(toMs, tOf(b)))
    }
    // 没拖动就是点击：落在堆叠的哪一段上，那一段就是用户想说的那条线
    if (!dragged && onPointClick && hover) {
      const rect = e.currentTarget.getBoundingClientRect()
      const y = e.clientY - rect.top
      let acc = 0
      let key: string | undefined
      for (const s of series) {
        const v = hover.bucket.values[s.key] ?? 0
        if (v <= 0) continue
        const top = yOf(acc + v)
        const bottom = yOf(acc)
        if (y >= top && y <= bottom) key = s.key
        acc += v
      }
      onPointClick({ tMs: hover.bucket.t_ms, seriesKey: key, x: xOf(hover.bucket.t_ms) + slot / 2, y })
    }
    setBrush(null)
  }

  const ticks = useMemo(() => timeTicks(x0, x1), [x0, x1])
  const yTicks = niceTicks(yMax)

  // 读屏念的那句话：什么图、哪一段时间、合计多少、各系列各多少（见 ./describe）
  const summary = useMemo(() => {
    const totals = series
      .map((s) => ({ label: s.label, v: buckets.reduce((n, b) => n + (b.values[s.key] ?? 0), 0) }))
      .filter((t) => t.v > 0)
    if (!totals.length) return chartSummary(label ?? '堆叠柱状图', fromMs, toMs, '')
    const all = totals.reduce((n, t) => n + t.v, 0)
    const parts = totals.slice(0, MAX_SPOKEN_SERIES).map((t) => `${t.label} ${format(t.v)}`)
    return chartSummary(label ?? '堆叠柱状图', fromMs, toMs, `合计 ${format(all)}；${andMore(parts, totals.length)}`)
  }, [buckets, series, format, fromMs, toMs, label])

  return (
    <div ref={ref} className={cn('relative w-full select-none', stale && 'chart-stale', className)} style={{ height }}>
      {width > 0 && (
        <svg
          width={width}
          height={height}
          role="img"
          aria-label={summary}
          className={cn('block touch-pan-y', (onBrush || onPointClick) && 'cursor-crosshair')}
          onPointerMove={onMove}
          onPointerLeave={() => {
            setHover(null)
            if (brush) setBrush(null)
            onHoverTs?.(null)
          }}
          onPointerDown={onDown}
          onPointerUp={onUp}
        >
          {yTicks.map((v) => (
            <g key={v}>
              <line x1={M.left} x2={M.left + W} y1={yOf(v)} y2={yOf(v)} stroke="var(--grid)" strokeWidth={1} />
              <text x={M.left - 6} y={yOf(v) + 3} textAnchor="end" fontSize={11} fill="var(--muted-fg)" className="tabular-nums">
                {format(v)}
              </text>
            </g>
          ))}
          <line x1={M.left} x2={M.left + W} y1={M.top + H} y2={M.top + H} stroke="var(--axis)" strokeWidth={1} />
          {ticks.map((t) => (
            <text key={t} x={xOf(t)} y={height - 6} textAnchor="middle" fontSize={11} fill="var(--muted-fg)">
              {formatTick(t, widthMs)}
            </text>
          ))}
          {ghost &&
            buckets.map((b) => {
              const v = b.values[ghost.key] ?? 0
              if (v <= 0) return null
              // 比主柱宽一点，像垫在后面的影子；再宽就糊成一片
              const w = Math.min(slot, barW + 4)
              return (
                <rect
                  key={`ghost${b.t_ms}`}
                  x={xOf(b.t_ms) + slot / 2 - w / 2}
                  y={yOf(v)}
                  width={w}
                  height={Math.max(1, M.top + H - yOf(v))}
                  fill="var(--muted-fg)"
                  opacity={0.18}
                />
              )
            })}
          {buckets.map((b) => {
            let acc = 0
            const cx = xOf(b.t_ms) + slot / 2
            const x = cx - barW / 2
            const isHover = hover?.bucket === b
            return (
              <g key={b.t_ms} opacity={isHover ? 0.85 : 1}>
                {series.map((s, i) => {
                  const v = b.values[s.key] ?? 0
                  if (v <= 0) return null
                  const y1 = yOf(acc)
                  acc += v
                  const y0 = yOf(acc)
                  // 段与段之间留 2px 的底色缝，最顶上的段圆角封顶
                  const h = Math.max(1, y1 - y0 - (i > 0 ? 2 : 0))
                  const isTop = series.slice(i + 1).every((k) => !(b.values[k.key] > 0))
                  const r = isTop ? Math.min(4, barW / 2, h / 2) : 0
                  return (
                    <path
                      key={s.key}
                      d={`M${x},${y1} V${y0 + r} Q${x},${y0} ${x + r},${y0} H${x + barW - r} Q${x + barW},${y0} ${x + barW},${y0 + r} V${y1} Z`}
                      fill={s.color}
                    />
                  )
                })}
              </g>
            )
          })}
          {syncTs != null && !hover && (
            <line
              x1={xOf(syncTs) + slot / 2}
              x2={xOf(syncTs) + slot / 2}
              y1={M.top}
              y2={M.top + H}
              stroke="var(--muted-fg)"
              strokeWidth={1}
              opacity={0.35}
            />
          )}
          {events
            ?.filter((e) => e.t_ms >= fromMs && e.t_ms <= toMs)
            .map((e, i) => (
              <g key={`ev${i}`}>
                <line x1={xOf(e.t_ms)} x2={xOf(e.t_ms)} y1={M.top} y2={M.top + H} stroke="var(--warn)" strokeWidth={1} strokeDasharray="3 3" />
                <path d={`M${xOf(e.t_ms) - 4},${M.top} h8 l-4 6 z`} fill="var(--warn)" />
                <title>{e.label}</title>
              </g>
            ))}
          {brush && (
            <rect
              // 框在画布里：从刻度栏上起手拖的话，选框会盖住纵轴的数字
              x={clamp(Math.min(brush.x0, brush.x1), M.left, M.left + W)}
              y={M.top}
              width={clamp(Math.max(brush.x0, brush.x1), M.left, M.left + W) - clamp(Math.min(brush.x0, brush.x1), M.left, M.left + W)}
              height={H}
              fill="var(--accent)"
              opacity={0.15}
              stroke="var(--accent)"
            />
          )}
        </svg>
      )}
      {hover && !brush && (
        <ChartTooltip
          x={hover.x}
          y={hover.y}
          width={width}
          title={`${formatTs(hover.bucket.t_ms, { ms: false })} 起 ${widthMs >= 60_000 ? `${Math.round(widthMs / 60_000)} 分钟` : `${Math.round(widthMs / 1000)} 秒`}`}
          rows={[
            { label: '合计', value: format(series.reduce((s, k) => s + (hover.bucket.values[k.key] ?? 0), 0)) },
            ...series
              .filter((s) => (hover.bucket.values[s.key] ?? 0) > 0)
              .map((s) => ({ color: s.color, label: s.label, value: format(hover.bucket.values[s.key] ?? 0) })),
            ...(ghost && (hover.bucket.values[ghost.key] ?? 0) > 0
              ? [{ label: ghost.label, value: format(hover.bucket.values[ghost.key] ?? 0) }]
              : []),
          ]}
        />
      )}
    </div>
  )
}
