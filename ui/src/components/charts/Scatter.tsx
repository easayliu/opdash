import { useMemo, useState, type PointerEvent } from 'react'
import { ChartTooltip } from './Tooltip'
import { useWidth } from './useWidth'
import { timeTicks } from './axis'
import { formatDurationMs, formatTick, formatTs } from '@/lib/time'
import { cn } from '@/lib/utils'

export interface ScatterPoint {
  id: string
  t_ms: number
  /** 毫秒 */
  value: number
  error: boolean
  label: string
}

interface Props {
  fromMs: number
  toMs: number
  points: ScatterPoint[]
  height?: number
  stale?: boolean
  onClick?: (id: string) => void
  selected?: string | null
  className?: string
}

const M = { left: 52, right: 8, top: 8, bottom: 22 }

/** 耗时 × 时间的散点图（对数纵轴），一眼看出离群点。点的命中区比点本身大得多。 */
export function Scatter({ fromMs, toMs, points, height = 160, stale, onClick, selected, className }: Props) {
  const [ref, width] = useWidth<HTMLDivElement>()
  const [hover, setHover] = useState<{ x: number; y: number; p: ScatterPoint } | null>(null)
  const W = Math.max(0, width - M.left - M.right)
  const H = height - M.top - M.bottom
  const span = Math.max(1, toMs - fromMs)
  const xOf = (t: number) => M.left + ((t - fromMs) / span) * W
  const { lo, hi } = useMemo(() => {
    const vals = points.map((p) => Math.max(0.01, p.value))
    const lo = Math.min(0.1, ...vals)
    const hi = Math.max(1, ...vals)
    return { lo: Math.pow(10, Math.floor(Math.log10(lo))), hi: Math.pow(10, Math.ceil(Math.log10(hi))) }
  }, [points])
  const yOf = (v: number) => M.top + H - ((Math.log10(Math.max(v, lo)) - Math.log10(lo)) / Math.max(1e-9, Math.log10(hi) - Math.log10(lo))) * H
  // 对数刻度每 10 倍一档；高度不够时隔档抽稀，别让标签叠在一起
  const allTicks: number[] = []
  for (let v = lo; v <= hi; v *= 10) allTicks.push(v)
  const every = Math.max(1, Math.ceil(allTicks.length / Math.max(1, Math.floor(H / 18))))
  const yTicks = allTicks.filter((_, i) => i % every === 0)
  const ticks = useMemo(() => timeTicks(fromMs, toMs), [fromMs, toMs])

  const onMove = (e: PointerEvent<SVGSVGElement>) => {
    const rect = e.currentTarget.getBoundingClientRect()
    const x = e.clientX - rect.left
    const y = e.clientY - rect.top
    let best: ScatterPoint | null = null
    let bestD = 14 * 14
    for (const p of points) {
      const dx = xOf(p.t_ms) - x
      const dy = yOf(p.value) - y
      const d = dx * dx + dy * dy
      if (d < bestD) {
        bestD = d
        best = p
      }
    }
    setHover(best ? { x: xOf(best.t_ms), y: yOf(best.value), p: best } : null)
  }

  return (
    <div ref={ref} className={cn('relative w-full select-none', stale && 'chart-stale', className)} style={{ height }}>
      {width > 0 && (
        <svg
          width={width}
          height={height}
          className={cn('block', hover && onClick && 'cursor-pointer')}
          onPointerMove={onMove}
          onPointerLeave={() => setHover(null)}
          onClick={() => hover && onClick?.(hover.p.id)}
        >
          {yTicks.map((v) => (
            <g key={v}>
              <line x1={M.left} x2={M.left + W} y1={yOf(v)} y2={yOf(v)} stroke="var(--grid)" strokeWidth={1} />
              <text x={M.left - 6} y={yOf(v) + 3} textAnchor="end" fontSize={11} fill="var(--muted-fg)">
                {formatDurationMs(v)}
              </text>
            </g>
          ))}
          <line x1={M.left} x2={M.left + W} y1={M.top + H} y2={M.top + H} stroke="var(--axis)" strokeWidth={1} />
          {ticks.map((t) => (
            <text key={t} x={xOf(t)} y={height - 6} textAnchor="middle" fontSize={11} fill="var(--muted-fg)">
              {formatTick(t, span / 8)}
            </text>
          ))}
          {points.map((p) => (
            <circle
              key={p.id}
              cx={xOf(p.t_ms)}
              cy={yOf(p.value)}
              r={selected === p.id || hover?.p === p ? 6 : 4}
              fill={p.error ? 'var(--level-error)' : 'var(--chart-1)'}
              stroke="var(--card)"
              strokeWidth={2}
            />
          ))}
        </svg>
      )}
      {hover && (
        <ChartTooltip
          x={hover.x}
          y={hover.y}
          width={width}
          title={formatTs(hover.p.t_ms)}
          rows={[
            { value: formatDurationMs(hover.p.value), label: hover.p.label },
            ...(hover.p.error ? [{ value: '有错误', label: '状态', color: 'var(--level-error)' }] : []),
          ]}
        />
      )}
    </div>
  )
}
