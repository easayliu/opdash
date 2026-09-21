import { describe, expect, it } from 'vitest'
import { render, screen } from '@testing-library/react'
import { StackedBars } from './StackedBars'

/** 图的左边留给纵轴刻度（`M.left`）：柱子画进这一段就会压住「0」那个标签 */
const AXIS_GUTTER = 48

const SERIES = [{ key: 'INFO', label: 'INFO', color: '#2a78d6' }]
const WIDTH = 60_000
const FROM = 1_700_000_130_000 // 16:05:30 那种不对齐整分的起点
const TO = FROM + 10 * WIDTH

/** 后端按整分的网格分桶，所以首桶落在 16:05:00——比查询起点早半格 */
const BUCKETS = Array.from({ length: 11 }, (_, i) => ({
  t_ms: FROM - 30_000 + i * WIDTH,
  values: { INFO: 1000 + i },
}))

/** 每根柱子的左边缘（path 的 `M x,y` 里那个 x） */
function barLefts(): number[] {
  return [...document.querySelectorAll('path[d^="M"]')]
    .map((p) => Number.parseFloat(p.getAttribute('d')!.slice(1).split(',')[0]))
    .filter((x) => Number.isFinite(x))
}

describe('StackedBars', () => {
  it('首桶早于查询起点时，柱子也不会画到纵轴刻度上', () => {
    render(<StackedBars fromMs={FROM} toMs={TO} widthMs={WIDTH} buckets={BUCKETS} series={SERIES} />)
    const lefts = barLefts()
    expect(lefts.length).toBe(BUCKETS.length)
    expect(Math.min(...lefts)).toBeGreaterThanOrEqual(AXIS_GUTTER)
  })

  it('末桶越过查询终点时，柱子也不会溢出右边', () => {
    render(<StackedBars fromMs={FROM} toMs={FROM + 90_000} widthMs={WIDTH} buckets={BUCKETS.slice(0, 3)} series={SERIES} />)
    // 1024 宽、右边留 8
    expect(Math.max(...barLefts())).toBeLessThanOrEqual(1024 - 8)
  })

  it('读屏拿得到一句概要', () => {
    render(<StackedBars fromMs={FROM} toMs={TO} widthMs={WIDTH} buckets={BUCKETS} series={SERIES} label="日志量直方图" />)
    expect(screen.getByRole('img').getAttribute('aria-label')).toMatch(/^日志量直方图，.*合计/)
  })
})
