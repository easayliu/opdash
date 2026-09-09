import { useMemo, useState, type PointerEvent } from 'react'
import { ChartTooltip } from './Tooltip'
import { useWidth } from './useWidth'
import { timeTicks } from './axis'
import { formatDurationMs, formatNumber, formatTick, formatTs } from '@/lib/time'
import { cn } from '@/lib/utils'

/** 一格：时间桶起点 × 对数耗时档。只有非空的格子。 */
export interface HeatCell {
  t_ms: number
  lvl: number
  count: number
  errors: number
}

export interface HeatCellRange {
  fromMs: number
  toMs: number
  /** 耗时区间，毫秒 */
  minMs: number
  maxMs: number
}

interface Props {
  fromMs: number
  toMs: number
  widthMs: number
  binsPerDecade: number
  cells: HeatCell[]
  height?: number
  stale?: boolean
  /** 拖一段时间 → 缩小范围 */
  onBrush?: (fromMs: number, toMs: number) => void
  /** 点一格 → 只看这个时间桶里、这一档耗时的链路 */
  onCellClick?: (range: HeatCellRange) => void
  className?: string
}

const M = { left: 52, right: 8, top: 8, bottom: 22 }
/** 格子之间留的底色缝，让密集区域仍能数出桶 */
const GAP = 1
/** 至少画这么多个数量级，数据都挤在一档时图不至于只剩几条大横带 */
const MIN_DECADES = 3

/** 档序号 → 这一档耗时的下界（毫秒） */
function lvlMs(lvl: number, bins: number): number {
  return Math.pow(10, lvl / bins)
}

/**
 * 耗时 × 时间的热力图（对数纵轴）。散点图的替代：在库里按时间桶 × 对数耗时档聚合过，
 * 格子数有上限，任意时间范围都能一次画出全貌，不像散点只能画检索出来的前 N 条。
 * 颜色深浅 = 数量（对数标定），偏红 = 错误占比。
 */
export function Heatmap({ fromMs, toMs, widthMs, binsPerDecade, cells, height = 170, stale, onBrush, onCellClick, className }: Props) {
  const [ref, width] = useWidth<HTMLDivElement>()
  const [hover, setHover] = useState<{ x: number; y: number; cell: HeatCell } | null>(null)
  const [brush, setBrush] = useState<{ x0: number; x1: number } | null>(null)

  const W = Math.max(0, width - M.left - M.right)
  const H = height - M.top - M.bottom
  const span = Math.max(1, toMs - fromMs)
  const bins = Math.max(1, binsPerDecade)
  const bucketMs = Math.max(1, widthMs)
  const xOf = (t: number) => M.left + ((t - fromMs) / span) * W
  const tOf = (x: number) => fromMs + ((x - M.left) / Math.max(1, W)) * span

  // 纵轴取到整数量级，刻度落在 10 倍边界上；没数据时给个 0.1ms ~ 10s 的默认范围。
  // 桶的起点对齐到服务端的原点（本地零点），不一定对齐 fromMs，相位从任一格推出来。
  const { lvlLo, lvlHi, maxCount, byKey, phase } = useMemo(() => {
    let lo = Infinity
    let hi = -Infinity
    let max = 0
    const map = new Map<string, HeatCell>()
    for (const c of cells) {
      lo = Math.min(lo, c.lvl)
      hi = Math.max(hi, c.lvl)
      max = Math.max(max, c.count)
      map.set(`${c.t_ms}:${c.lvl}`, c)
    }
    if (!cells.length) {
      lo = -bins
      hi = 4 * bins - 1
    }
    const lvlLo = Math.floor(lo / bins) * bins
    let lvlHi = (Math.floor(hi / bins) + 1) * bins
    if (lvlHi - lvlLo < MIN_DECADES * bins) lvlHi = lvlLo + MIN_DECADES * bins
    const first = cells[0]?.t_ms ?? fromMs
    const phase = ((first % bucketMs) + bucketMs) % bucketMs
    return { lvlLo, lvlHi, maxCount: max, byKey: map, phase }
  }, [cells, bins, bucketMs, fromMs])
  const rows = lvlHi - lvlLo
  const rowH = H / Math.max(1, rows)
  /** 档的下边缘 y */
  const yOf = (lvl: number) => M.top + H - (lvl - lvlLo) * rowH
  const bucketStart = (t: number) => t - ((((t - phase) % bucketMs) + bucketMs) % bucketMs)

  // 数量是重尾的：几个格子里挤着大部分请求，线性标定会让其它格子全都发白，用对数
  const alphaOf = (count: number) => 0.15 + 0.85 * (Math.log1p(count) / Math.max(1e-9, Math.log1p(maxCount)))
  const fillOf = (c: HeatCell) => {
    const pct = c.count > 0 ? Math.round((100 * c.errors) / c.count) : 0
    return pct > 0 ? `color-mix(in srgb, var(--level-error) ${pct}%, var(--chart-1))` : 'var(--chart-1)'
  }

  const cellAt = (x: number, y: number): HeatCell | null => {
    if (x < M.left || x > M.left + W || y < M.top || y > M.top + H) return null
    const t = bucketStart(tOf(x))
    const lvl = lvlLo + Math.floor((M.top + H - y) / rowH)
    return byKey.get(`${t}:${lvl}`) ?? null
  }

  const onMove = (e: PointerEvent<SVGSVGElement>) => {
    const rect = e.currentTarget.getBoundingClientRect()
    const x = e.clientX - rect.left
    const y = e.clientY - rect.top
    if (brush) setBrush({ ...brush, x1: x })
    const cell = cellAt(x, y)
    setHover(cell ? { x, y, cell } : null)
  }
  const onDown = (e: PointerEvent<SVGSVGElement>) => {
    if (!onBrush && !onCellClick) return
    const rect = e.currentTarget.getBoundingClientRect()
    const x = e.clientX - rect.left
    setBrush({ x0: x, x1: x })
    e.currentTarget.setPointerCapture(e.pointerId)
  }
  const onUp = () => {
    if (brush) {
      const [a, b] = [Math.min(brush.x0, brush.x1), Math.max(brush.x0, brush.x1)]
      if (b - a > 4) {
        if (onBrush) onBrush(Math.max(fromMs, tOf(a)), Math.min(toMs, tOf(b)))
      } else if (hover && onCellClick) {
        // 没拖动就是点了一格
        const c = hover.cell
        onCellClick({
          fromMs: Math.max(fromMs, c.t_ms),
          toMs: Math.min(toMs, c.t_ms + bucketMs),
          minMs: lvlMs(c.lvl, bins),
          maxMs: lvlMs(c.lvl + 1, bins),
        })
      }
    }
    setBrush(null)
  }

  const ticks = useMemo(() => timeTicks(fromMs, toMs), [fromMs, toMs])
  // 每个数量级一条刻度；高度不够时隔档抽稀
  const decades: number[] = []
  for (let lvl = lvlLo; lvl <= lvlHi; lvl += bins) decades.push(lvl)
  const every = Math.max(1, Math.ceil(decades.length / Math.max(1, Math.floor(H / 18))))
  const yTicks = decades.filter((_, i) => i % every === 0)

  return (
    <div ref={ref} className={cn('relative w-full select-none', stale && 'chart-stale', className)} style={{ height }}>
      {width > 0 && (
        <svg
          width={width}
          height={height}
          className={cn('block', hover && onCellClick && 'cursor-pointer', brush && 'cursor-col-resize')}
          onPointerMove={onMove}
          onPointerDown={onDown}
          onPointerUp={onUp}
          onPointerLeave={() => {
            setHover(null)
            if (brush) setBrush(null)
          }}
        >
          {yTicks.map((lvl) => (
            <g key={lvl}>
              <line x1={M.left} x2={M.left + W} y1={yOf(lvl)} y2={yOf(lvl)} stroke="var(--grid)" strokeWidth={1} />
              <text x={M.left - 6} y={yOf(lvl) + 3} textAnchor="end" fontSize={11} fill="var(--muted-fg)">
                {formatDurationMs(lvlMs(lvl, bins))}
              </text>
            </g>
          ))}
          {cells.map((c) => {
            if (c.lvl < lvlLo || c.lvl >= lvlHi) return null
            // 首尾两个桶可能伸出范围外，裁到坐标区里
            const x0 = Math.max(M.left, xOf(c.t_ms))
            const x1 = Math.min(M.left + W, xOf(c.t_ms + bucketMs))
            const w = x1 - x0 - GAP
            if (w <= 0) return null
            const isHover = hover?.cell === c
            return (
              <rect
                key={`${c.t_ms}:${c.lvl}`}
                x={x0}
                y={yOf(c.lvl + 1) + GAP / 2}
                width={Math.max(1, w)}
                height={Math.max(1, rowH - GAP)}
                fill={fillOf(c)}
                fillOpacity={isHover ? 1 : alphaOf(c.count)}
                stroke={isHover ? 'var(--fg)' : undefined}
                strokeWidth={isHover ? 1 : 0}
              />
            )
          })}
          <line x1={M.left} x2={M.left + W} y1={M.top + H} y2={M.top + H} stroke="var(--axis)" strokeWidth={1} />
          {ticks.map((t) => (
            <text key={t} x={xOf(t)} y={height - 6} textAnchor="middle" fontSize={11} fill="var(--muted-fg)">
              {formatTick(t, span / 8)}
            </text>
          ))}
          {brush && Math.abs(brush.x1 - brush.x0) > 4 && (
            <rect
              x={Math.min(brush.x0, brush.x1)}
              y={M.top}
              width={Math.abs(brush.x1 - brush.x0)}
              height={H}
              fill="var(--accent)"
              fillOpacity={0.15}
              stroke="var(--accent)"
              strokeWidth={1}
            />
          )}
        </svg>
      )}
      {hover && !(brush && Math.abs(brush.x1 - brush.x0) > 4) && (
        <ChartTooltip
          x={hover.x}
          y={hover.y}
          width={width}
          title={`${formatTs(hover.cell.t_ms, { ms: false })} ~ ${formatTs(hover.cell.t_ms + bucketMs, { ms: false, date: false })}`}
          rows={[
            {
              value: `${formatNumber(hover.cell.count)} 个`,
              label: `${formatDurationMs(lvlMs(hover.cell.lvl, bins))} ~ ${formatDurationMs(lvlMs(hover.cell.lvl + 1, bins))}`,
              color: 'var(--chart-1)',
            },
            ...(hover.cell.errors > 0
              ? [
                  {
                    value: `${formatNumber(hover.cell.errors)} 个（${Math.round((100 * hover.cell.errors) / hover.cell.count)}%）`,
                    label: '错误',
                    color: 'var(--level-error)',
                  },
                ]
              : []),
          ]}
        />
      )}
    </div>
  )
}
