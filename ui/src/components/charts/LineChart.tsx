import { useMemo, useState, type PointerEvent } from 'react'
import { ChartTooltip } from './Tooltip'
import { MAX_SPOKEN_SERIES, andMore, chartSummary } from './describe'
import { useWidth } from './useWidth'
import { bucketDomain, niceMax, niceRange, niceTicks, placeTicks, tickBudget, tickUnit, timeTicks } from './axis'
import { formatTick, formatTs } from '@/lib/time'
import { cn } from '@/lib/utils'

export interface LineSeries {
  key: string
  label: string
  color: string
  /** 画成虚线。对比时段的那条线用它：同一个颜色体系里，虚线一眼就读成「这不是现在」 */
  dashed?: boolean
}

export interface LinePoint {
  t_ms: number
  values: Record<string, number>
}

/** 图上钉一个点：指标的 exemplar（点开跳 trace）。 */
/** 图上标的事件（进程重启 / pod 启动 / 发布）：一条虚线竖线 */
export interface ChartEvent {
  t_ms: number
  label: string
}

export interface ChartMarker {
  t_ms: number
  value: number
  title?: string
  onClick?: () => void
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
  /** 线下面填一层淡色。Cloudflare 控制台的分析图都是填充的，单条线时填上更好读；
   *  线一多互相盖，就别填了 */
  area?: boolean
  /** 拖一段时间 → 缩小范围。和日志页直方图同一个交互 */
  onBrush?: (fromMs: number, toMs: number) => void
  /** 标在图上的事件（重启 / 发布），画成虚线竖线 */
  events?: ChartEvent[]
  /** 点某个点：给出这个桶的时刻、离得最近的那条线，以及点在图上的位置（弹层定位用） */
  onPointClick?: (at: { tMs: number; seriesKey?: string; x: number; y: number }) => void
  /** 别的图上鼠标停在哪个时刻：画一条同位置的竖线，不弹气泡（气泡只属于鼠标真正在的那张图） */
  syncTs?: number | null
  /** 自己被 hover 到哪个时刻，交给上层广播给同一块看板的其它图 */
  onHoverTs?: (tMs: number | null) => void
  /** 缺的桶直接连过去，不断线。指标是采样数据，上报周期比桶宽长时到处是空桶，断了就什么都看不见 */
  connectGaps?: boolean
  /** 每个真实数据点画一个小圆点：点稀疏时只有线段是看不见的（单点线段画不出东西） */
  dots?: boolean
  markers?: ChartMarker[]
  /** 读屏念的那句话里，这张图叫什么。不给就是「折线图」 */
  label?: string
  /** 纵轴不从 0 起，按数据的上下限取整刻度（见 niceRange）。默认从 0 起 */
  fitY?: boolean
  /** 悬停气泡的标题。默认是桶的时刻；按天的图想写「9/23 周三」、或要注明对比的是哪一天时给 */
  hoverTitle?: (tMs: number) => string
}

// 右边留够半个刻度标签的宽度：最后一格是 `23:10` 这种居中标签，只留 8px 会被切掉半个字
const M = { left: 52, right: 24, top: 8, bottom: 22 }

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v))

/** 多条折线 + 十字线读数（2px 线、所有系列一起读）。 */
export function LineChart({
  fromMs,
  toMs,
  widthMs,
  points,
  series,
  height = 160,
  stale,
  format = (v) => String(v),
  className,
  area,
  events,
  onBrush,
  onPointClick,
  syncTs,
  onHoverTs,
  connectGaps,
  dots,
  markers,
  label,
  fitY,
  hoverTitle,
}: Props) {
  const [ref, width] = useWidth<HTMLDivElement>()
  const [hover, setHover] = useState<{ x: number; y: number; point: LinePoint } | null>(null)
  const [brush, setBrush] = useState<{ x0: number; x1: number } | null>(null)
  const W = Math.max(0, width - M.left - M.right)
  const H = height - M.top - M.bottom
  // 横轴要盖住实际拿到的点，不能只按请求的范围算——首尾两个桶会越界（见 bucketDomain）。
  // 点画在桶的中间，所以 xOf 里还要加半格
  const { x0, x1 } = bucketDomain(fromMs, toMs, points, widthMs)
  const span = Math.max(1, x1 - x0)
  const xOf = (t: number) => M.left + ((t - x0 + widthMs / 2) / span) * W
  const tOf = (x: number) => x0 + ((x - M.left) / Math.max(1, W)) * span
  const y = useMemo(() => {
    const values = points.flatMap((p) => series.map((s) => p.values[s.key]).filter((v): v is number => v !== undefined && Number.isFinite(v)))
    const max = Math.max(0, ...values)
    if (fitY && values.length) return niceRange(Math.min(...values), max)
    return { lo: 0, hi: niceMax(max), ticks: niceTicks(niceMax(max)) }
  }, [points, series, fitY])
  const yOf = (v: number) => M.top + H - (y.hi > y.lo ? ((v - y.lo) / (y.hi - y.lo)) * H : 0)
  const ticks = useMemo(() => timeTicks(x0, x1, tickBudget(W, widthMs)), [x0, x1, W, widthMs])
  const unit = tickUnit(widthMs, ticks)

  /**
   * 读屏念的那句话：什么图、哪一段时间、每条线最新多少、最高多少（见 ./describe）。
   * 「最新」取最后一个有值的点——指标是采样的，末尾常常是空桶。
   */
  const summary = useMemo(() => {
    const told = series
      .map((s) => {
        let last: number | null = null
        let top = -Infinity
        for (const p of points) {
          const v = p.values[s.key]
          if (v === undefined || v === null || !Number.isFinite(v)) continue
          last = v
          if (v > top) top = v
        }
        return last === null ? null : `${s.label} 最新 ${format(last)}，最高 ${format(top)}`
      })
      .filter((x): x is string => x !== null)
    return chartSummary(label ?? '折线图', fromMs, toMs, told.length ? andMore(told.slice(0, MAX_SPOKEN_SERIES), told.length) : '')
  }, [points, series, format, fromMs, toMs, label])

  const nearest = (x: number): LinePoint | null => {
    let best: LinePoint | null = null
    let bestD = Infinity
    for (const p of points) {
      const d = Math.abs(xOf(p.t_ms) - x)
      if (d < bestD) {
        bestD = d
        best = p
      }
    }
    return best
  }

  const onMove = (e: PointerEvent<SVGSVGElement>) => {
    const rect = e.currentTarget.getBoundingClientRect()
    const x = e.clientX - rect.left
    if (brush) setBrush({ ...brush, x1: x })
    const best = nearest(x)
    setHover(best ? { x: xOf(best.t_ms), y: e.clientY - rect.top, point: best } : null)
    onHoverTs?.(best ? best.t_ms : null)
  }
  const onLeave = () => {
    setHover(null)
    setBrush(null)
    onHoverTs?.(null)
  }
  const onDown = (e: PointerEvent<SVGSVGElement>) => {
    if (!onBrush) return
    const rect = e.currentTarget.getBoundingClientRect()
    setBrush({ x0: e.clientX - rect.left, x1: e.clientX - rect.left })
    e.currentTarget.setPointerCapture(e.pointerId)
  }
  const onUp = (e: PointerEvent<SVGSVGElement>) => {
    const dragged = brush && Math.abs(brush.x1 - brush.x0) > 4
    if (brush && onBrush && dragged) {
      const [a, b] = [Math.min(brush.x0, brush.x1), Math.max(brush.x0, brush.x1)]
      onBrush(Math.max(fromMs, tOf(a)), Math.min(toMs, tOf(b)))
    }
    // 没拖动就是点击：定位到最近的点、以及纵向离鼠标最近的那条线
    if (!dragged && onPointClick) {
      const rect = e.currentTarget.getBoundingClientRect()
      const x = e.clientX - rect.left
      const y = e.clientY - rect.top
      const p = nearest(x)
      if (p) {
        let key: string | undefined
        let best = Infinity
        for (const s of series) {
          const v = p.values[s.key]
          if (v === undefined) continue
          const d = Math.abs(yOf(v) - y)
          if (d < best) {
            best = d
            key = s.key
          }
        }
        onPointClick({ tMs: p.t_ms, seriesKey: key, x: xOf(p.t_ms), y })
      }
    }
    setBrush(null)
  }
  // 别的图在这个时刻上：画同位置的竖线（自己有 hover 时以自己为准）
  const syncPoint = syncTs != null && !hover ? points.find((p) => p.t_ms === syncTs) : undefined

  return (
    <div ref={ref} className={cn('relative w-full select-none', stale && 'chart-stale', className)} style={{ height }}>
      {width > 0 && (
        <svg
          width={width}
          height={height}
          role="img"
          aria-label={summary}
          className={cn('block touch-pan-y', (onBrush || onPointClick) && 'cursor-crosshair', brush && 'cursor-col-resize')}
          onPointerMove={onMove}
          onPointerLeave={onLeave}
          onPointerDown={onDown}
          onPointerUp={onUp}
        >
          {y.ticks.map((v) => (
            <g key={v}>
              <line x1={M.left} x2={M.left + W} y1={yOf(v)} y2={yOf(v)} stroke="var(--grid)" strokeWidth={1} />
              <text x={M.left - 6} y={yOf(v) + 3} textAnchor="end" fontSize={11} fill="var(--muted-fg)" className="tabular-nums">
                {format(v)}
              </text>
            </g>
          ))}
          <line x1={M.left} x2={M.left + W} y1={M.top + H} y2={M.top + H} stroke="var(--axis)" strokeWidth={1} />
          {placeTicks(ticks.map((t) => ({ t, x: M.left + ((t - fromMs) / span) * W, text: formatTick(t, unit) })), width).map((k) => (
            <text key={k.t} x={k.x} y={height - 6} textAnchor={k.anchor} fontSize={11} fill="var(--muted-fg)">
              {k.text}
            </text>
          ))}
          {series.map((s) => {
            // 没有数据的桶（请求数为 0）断开，不画成 0；connectGaps 时连过去。
            // 一段 = 一串连续有值的点，线和填充都从同一串坐标生成
            const segs: [number, number][][] = []
            let cur: [number, number][] = []
            for (const p of points) {
              const v = p.values[s.key]
              if (v === undefined || Number.isNaN(v)) {
                if (connectGaps) continue
                if (cur.length) segs.push(cur)
                cur = []
                continue
              }
              cur.push([xOf(p.t_ms), yOf(v)])
            }
            if (cur.length) segs.push(cur)
            const line = (seg: [number, number][]) =>
              seg.map(([x, y], i) => `${i ? 'L' : 'M'}${x.toFixed(1)},${y.toFixed(1)}`).join(' ')
            const baseline = yOf(y.lo).toFixed(1)
            return (
              <g key={s.key}>
                {area &&
                  segs
                    // 一个点围不出面
                    .filter((seg) => seg.length > 1)
                    .map((seg, i) => (
                      <path
                        key={`a${i}`}
                        d={`${line(seg)} L${seg[seg.length - 1][0].toFixed(1)},${baseline} L${seg[0][0].toFixed(1)},${baseline} Z`}
                        fill={s.color}
                        opacity={0.12}
                        stroke="none"
                      />
                    ))}
                {segs.map((seg, i) => (
                  <path
                    key={i}
                    d={line(seg)}
                    fill="none"
                    stroke={s.color}
                    strokeWidth={s.dashed ? 1.5 : 2}
                    strokeDasharray={s.dashed ? '5 4' : undefined}
                    strokeLinejoin="round"
                    strokeLinecap="round"
                  />
                ))}
                {dots && segs.flat().map(([x, y], i) => <circle key={i} cx={x} cy={y} r={2} fill={s.color} />)}
              </g>
            )
          })}
          {events
            ?.filter((e) => e.t_ms >= fromMs && e.t_ms <= toMs)
            .map((e, i) => (
              <g key={`ev${i}`}>
                <line x1={xOf(e.t_ms)} x2={xOf(e.t_ms)} y1={M.top} y2={M.top + H} stroke="var(--warn)" strokeWidth={1} strokeDasharray="3 3" />
                <path d={`M${xOf(e.t_ms) - 4},${M.top} h8 l-4 6 z`} fill="var(--warn)" />
                <title>{e.label}</title>
              </g>
            ))}
          {/* exemplar：值可能远超曲线（p95 线上挂着一次 3 秒的请求），钉在画布内，不让它撑大纵轴 */}
          {markers?.map((m, i) => (
            <circle
              key={i}
              cx={xOf(m.t_ms)}
              cy={Math.min(M.top + H, Math.max(M.top, yOf(m.value)))}
              r={3.5}
              fill="var(--card)"
              stroke="var(--brand)"
              strokeWidth={2}
              className={m.onClick ? 'cursor-pointer' : undefined}
              onClick={m.onClick}
            >
              {m.title && <title>{m.title}</title>}
            </circle>
          ))}
          {syncPoint && (
            <line
              x1={xOf(syncPoint.t_ms)}
              x2={xOf(syncPoint.t_ms)}
              y1={M.top}
              y2={M.top + H}
              stroke="var(--muted-fg)"
              strokeWidth={1}
              opacity={0.35}
            />
          )}
          {brush && Math.abs(brush.x1 - brush.x0) > 2 && (
            <rect
              // 框在画布里：从刻度栏上起手拖的话，选框会盖住纵轴的数字
              x={clamp(Math.min(brush.x0, brush.x1), M.left, M.left + W)}
              y={M.top}
              width={clamp(Math.max(brush.x0, brush.x1), M.left, M.left + W) - clamp(Math.min(brush.x0, brush.x1), M.left, M.left + W)}
              height={H}
              fill="var(--accent)"
              opacity={0.15}
            />
          )}
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
      {hover && !brush && (
        <ChartTooltip
          x={hover.x}
          y={hover.y}
          width={width}
          title={hoverTitle ? hoverTitle(hover.point.t_ms) : formatTs(hover.point.t_ms, { ms: false })}
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
          <span
            className="inline-block h-0.5 w-3 rounded"
            style={s.dashed ? { background: `repeating-linear-gradient(to right, ${s.color} 0 4px, transparent 4px 7px)` } : { background: s.color }}
          />
          {s.label}
        </span>
      ))}
    </div>
  )
}
