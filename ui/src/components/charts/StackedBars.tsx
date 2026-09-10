import { useMemo, useState, type PointerEvent } from 'react'
import { ChartTooltip } from './Tooltip'
import { useWidth } from './useWidth'
import { formatCompact, niceMax, niceTicks, timeTicks } from './axis'
import { formatTick, formatTs } from '@/lib/time'
import { cn } from '@/lib/utils'

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
  /** 别的图上鼠标停在哪个时刻：画一条同位置的竖线（同一块看板的图共用一根十字线） */
  syncTs?: number | null
  onHoverTs?: (tMs: number | null) => void
  /** 数值怎么显示（指标页要带单位）。不给就是纯数字 */
  format?: (v: number) => string
  className?: string
}

const M = { left: 48, right: 8, top: 8, bottom: 22 }

/** 按时间分桶的堆叠柱状图：日志直方图、请求量 / 错误数都用它。 */
export function StackedBars({ fromMs, toMs, widthMs, buckets, series, height = 140, stale, onBrush, syncTs, onHoverTs, format = formatCompact, className }: Props) {
  const [ref, width] = useWidth<HTMLDivElement>()
  const [hover, setHover] = useState<{ x: number; y: number; bucket: BarBucket } | null>(null)
  const [brush, setBrush] = useState<{ x0: number; x1: number } | null>(null)

  const W = Math.max(0, width - M.left - M.right)
  const H = height - M.top - M.bottom
  const span = Math.max(1, toMs - fromMs)
  const xOf = (t: number) => M.left + ((t - fromMs) / span) * W
  const tOf = (x: number) => fromMs + ((x - M.left) / Math.max(1, W)) * span

  const max = useMemo(() => Math.max(0, ...buckets.map((b) => series.reduce((s, k) => s + (b.values[k.key] ?? 0), 0))), [buckets, series])
  const yMax = niceMax(max)
  const yOf = (v: number) => M.top + H - (yMax > 0 ? (v / yMax) * H : 0)
  const slot = (widthMs / span) * W
  const barW = Math.max(1, Math.min(24, slot - 2))

  const onMove = (e: PointerEvent<SVGSVGElement>) => {
    const rect = e.currentTarget.getBoundingClientRect()
    const x = e.clientX - rect.left
    const y = e.clientY - rect.top
    if (brush) setBrush({ ...brush, x1: x })
    const idx = Math.floor((tOf(x) - fromMs) / widthMs)
    const bucket = buckets.find((b) => Math.floor((b.t_ms - fromMs) / widthMs) === idx)
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
  const onUp = () => {
    if (brush && onBrush) {
      const [a, b] = [Math.min(brush.x0, brush.x1), Math.max(brush.x0, brush.x1)]
      if (b - a > 4) onBrush(Math.max(fromMs, tOf(a)), Math.min(toMs, tOf(b)))
    }
    setBrush(null)
  }

  const ticks = useMemo(() => timeTicks(fromMs, toMs), [fromMs, toMs])
  const yTicks = niceTicks(yMax)

  return (
    <div ref={ref} className={cn('relative w-full select-none', stale && 'chart-stale', className)} style={{ height }}>
      {width > 0 && (
        <svg
          width={width}
          height={height}
          className={cn('block touch-pan-y', onBrush && 'cursor-crosshair')}
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
          {brush && (
            <rect
              x={Math.min(brush.x0, brush.x1)}
              y={M.top}
              width={Math.abs(brush.x1 - brush.x0)}
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
          ]}
        />
      )}
    </div>
  )
}
