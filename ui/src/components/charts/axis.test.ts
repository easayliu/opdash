import { describe, expect, it } from 'vitest'
import { bucketDomain, niceRange } from '@/components/charts/axis'

const W = 60_000

describe('bucketDomain', () => {
  /**
   * 后端按固定网格分桶，首桶可能比查询起点早整整一格。按 [from, to] 去定位的话，
   * 第一根柱子会画到坐标轴左边的刻度栏里，压住「0」那个标签。
   */
  it('把越界的首桶圈进来', () => {
    // 查 16:05:30 起，桶对齐到整分，于是首桶是 16:05:00
    const from = 1_700_000_130_000
    const to = from + 3_600_000
    const { x0, x1 } = bucketDomain(from, to, [{ t_ms: from - 30_000 }, { t_ms: from + 30_000 }], W)
    expect(x0).toBe(from - 30_000)
    expect(x1).toBeGreaterThanOrEqual(to)
  })

  it('把越界的末桶圈进来', () => {
    const from = 1_700_000_000_000
    const to = from + 90_000
    const { x1 } = bucketDomain(from, to, [{ t_ms: from }, { t_ms: from + 60_000 }], W)
    // 末桶覆盖 [+60s, +120s)，超过了 to，轴得跟着延到 +120s
    expect(x1).toBe(from + 120_000)
  })

  it('桶都在范围内时就是原来的范围', () => {
    const from = 1_700_000_000_000
    const to = from + 120_000
    expect(bucketDomain(from, to, [{ t_ms: from }, { t_ms: from + 60_000 }], W)).toEqual({ x0: from, x1: to })
  })

  it('一个桶都没有时不改范围', () => {
    expect(bucketDomain(1, 2, [], W)).toEqual({ x0: 1, x1: 2 })
  })

  it('入参无序也认得出最早和最晚（热力图的格子不保证顺序）', () => {
    const from = 1_700_000_000_000
    const to = from + 180_000
    const cells = [{ t_ms: from + 120_000 }, { t_ms: from - 60_000 }, { t_ms: from + 60_000 }]
    expect(bucketDomain(from, to, cells, W)).toEqual({ x0: from - 60_000, x1: to })
  })
})

describe('niceRange', () => {
  it('上下取到整刻度，把起伏撑满', () => {
    // 每天 1,000 上下：从 0 起画只看得到一条平线
    expect(niceRange(980, 1130)).toEqual({ lo: 950, hi: 1150, ticks: [950, 1000, 1050, 1100, 1150] })
  })

  it('全部相等时退回从 0 起', () => {
    expect(niceRange(100, 100).lo).toBe(0)
  })
})
