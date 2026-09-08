import type { ReactNode } from 'react'

/** 图表的悬浮读数：值在前、名字在后，系列用一小段色线标识。 */
export function ChartTooltip({
  x,
  y,
  width,
  title,
  rows,
}: {
  x: number
  y: number
  width: number
  title: ReactNode
  rows: { color?: string; label: string; value: string }[]
}) {
  // 靠右时翻到左边，别出界
  const flip = x > width - 180
  return (
    <div
      className="pointer-events-none absolute z-10 min-w-36 rounded-md border border-border bg-card px-2.5 py-1.5 text-xs shadow-md"
      style={{ left: flip ? undefined : x + 12, right: flip ? width - x + 12 : undefined, top: Math.max(0, y - 8) }}
    >
      <div className="mb-1 text-2xs text-muted-fg">{title}</div>
      {rows.map((r) => (
        <div key={r.label} className="flex items-center gap-2 leading-5">
          {r.color && <span className="inline-block h-0.5 w-3 shrink-0 rounded" style={{ background: r.color }} />}
          <span className="font-semibold tabular-nums text-fg">{r.value}</span>
          <span className="truncate text-muted-fg">{r.label}</span>
        </div>
      ))}
    </div>
  )
}
