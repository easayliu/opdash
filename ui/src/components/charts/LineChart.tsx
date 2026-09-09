import { useMemo, useState, type PointerEvent } from 'react'
import { ChartTooltip } from './Tooltip'
import { useWidth } from './useWidth'
import { niceMax, niceTicks, timeTicks } from './axis'
import { formatTick, formatTs } from '@/lib/time'
import { cn } from '@/lib/utils'

export interface LineSeries {
  key: string
  label: string
  color: string
}

export interface LinePoint {
  t_ms: number
  values: Record<string, number>
}

interface Props {
  fromMs: number
  toMs: number
  widthMs: number
  points: LinePoint[]
  series: LineSeries[]
  height?: number
  stale?: boolean
  format?: (v: number) => string
  className?: string
}

const M = { left: 52, right: 8, top: 8, bottom: 22 }

/** 多条折线 + 十字线读数（2px 线、所有系列一起读）。 */
export function LineChart({ fromMs, toMs, widthMs, points, series, height = 160, stale, format = (v) => String(v), className }: Props) {
  const [ref, width] = useWidth<HTMLDivElement>()
  const [hover, setHover] = useState<{ x: number; y: number; point: LinePoint } | null>(null)
  const W = Math.max(0, width - M.left - M.right)
  const H = height - M.top - M.bottom
  const span = Math.max(1, toMs - fromMs)
  const xOf = (t: number) => M.left + ((t - fromMs + widthMs / 2) / span) * W
  const max = useMemo(() => Math.max(0, ...points.flatMap((p) => series.map((s) => p.values[s.key] ?? 0))), [points, series])
  const yMax = niceMax(max)
  const yOf = (v: number) => M.top + H - (yMax > 0 ? (v / yMax) * H : 0)
  const ticks = useMemo(() => timeTicks(fromMs, toMs), [fromMs, toMs])

  const onMove = (e: PointerEvent<SVGSVGElement>) => {
    const rect = e.currentTarget.getBoundingClientRect()
    const x = e.clientX - rect.left
    let best: LinePoint | null = null
    let bestD = Infinity
    for (const p of points) {
      const d = Math.abs(xOf(p.t_ms) - x)
      if (d < bestD) {
        bestD = d
        best = p
      }
    }
    setHover(best ? { x: xOf(best.t_ms), y: e.clientY - rect.top, point: best } : null)
  }

  return (
    <div ref={ref} className={cn('relative w-full select-none', stale && 'chart-stale', className)} style={{ height }}>
      {width > 0 && (
        <svg width={width} height={height} className="block" onPointerMove={onMove} onPointerLeave={() => setHover(null)}>
          {niceTicks(yMax).map((v) => (
            <g key={v}>
              <line x1={M.left} x2={M.left + W} y1={yOf(v)} y2={yOf(v)} stroke="var(--grid)" strokeWidth={1} />
              <text x={M.left - 6} y={yOf(v) + 3} textAnchor="end" fontSize={11} fill="var(--muted-fg)" className="tabular-nums">
                {format(v)}
              </text>
            </g>
          ))}
          <line x1={M.left} x2={M.left + W} y1={M.top + H} y2={M.top + H} stroke="var(--axis)" strokeWidth={1} />
          {ticks.map((t) => (
            <text key={t} x={M.left + ((t - fromMs) / span) * W} y={height - 6} textAnchor="middle" fontSize={11} fill="var(--muted-fg)">
              {formatTick(t, widthMs)}
            </text>
          ))}
          {series.map((s) => {
            // 没有数据的桶（请求数为 0）断开，不画成 0
            const segs: string[] = []
            let cur: string[] = []
            for (const p of points) {
              const v = p.values[s.key]
              if (v === undefined || Number.isNaN(v)) {
                if (cur.length) segs.push(cur.join(' '))
                cur = []
                continue
              }
              cur.push(`${cur.length ? 'L' : 'M'}${xOf(p.t_ms).toFixed(1)},${yOf(v).toFixed(1)}`)
            }
            if (cur.length) segs.push(cur.join(' '))
            return (
              <g key={s.key}>
                {segs.map((d, i) => (
                  <path key={i} d={d} fill="none" stroke={s.color} strokeWidth={2} strokeLinejoin="round" strokeLinecap="round" />
                ))}
              </g>
            )
          })}
          {hover && (
            <g>
              <line x1={hover.x} x2={hover.x} y1={M.top} y2={M.top + H} stroke="var(--muted-fg)" strokeWidth={1} opacity={0.6} />
              {series.map((s) => {
                const v = hover.point.values[s.key]
                if (v === undefined) return null
                return <circle key={s.key} cx={hover.x} cy={yOf(v)} r={4} fill={s.color} stroke="var(--card)" strokeWidth={2} />
              })}
            </g>
          )}
        </svg>
      )}
      {hover && (
        <ChartTooltip
          x={hover.x}
          y={hover.y}
          width={width}
          title={formatTs(hover.point.t_ms, { ms: false })}
          rows={series.map((s) => ({ color: s.color, label: s.label, value: hover.point.values[s.key] === undefined ? '-' : format(hover.point.values[s.key]) }))}
        />
      )}
    </div>
  )
}

/** 图例：系列 ≥ 2 时一定要有。 */
export function Legend({ series, className }: { series: LineSeries[]; className?: string }) {
  if (series.length < 2) return null
  return (
    <div className={cn('flex flex-wrap items-center gap-x-3 gap-y-1 text-2xs text-muted-fg', className)}>
      {series.map((s) => (
        <span key={s.key} className="inline-flex items-center gap-1.5">
          <span className="inline-block h-0.5 w-3 rounded" style={{ background: s.color }} />
          {s.label}
        </span>
      ))}
    </div>
  )
}
