import { describe, expect, it } from 'vitest'
import { bucketDomain, niceRange, placeTicks, tickBudget, tickUnit, timeTicks } from '@/components/charts/axis'

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

describe('timeTicks / tickBudget', () => {
  const from = Date.UTC(2026, 8, 24, 3, 50)
  const hour = from + 3_600_000

  it('宽屏照旧最多 8 个刻度', () => {
    expect(tickBudget(900, 10_000)).toBe(8)
    expect(timeTicks(from, hour, tickBudget(900, 10_000)).length).toBeLessThanOrEqual(8)
  })

  it('手机上的窄图少放几个，免得叠在一起', () => {
    // 390px 的屏幕扣掉边距，画布约 270px：「12:15」这种短刻度放 4 个
    expect(timeTicks(from, hour, tickBudget(270, 60_000))).toHaveLength(4)
    // 「09-24 12:00」这种长刻度放得更少
    expect(tickBudget(270, 3_600_000)).toBeLessThan(tickBudget(270, 60_000))
  })

  it('再窄也至少留两个刻度', () => {
    expect(tickBudget(40, 60_000)).toBe(2)
  })

  it('半年按天画在手机上，刻度拉到八周一个', () => {
    const start = Date.UTC(2026, 3, 1)
    const ticks = timeTicks(start, start + 176 * 86_400_000, tickBudget(290, 86_400_000))
    expect(ticks.length).toBeLessThanOrEqual(5)
  })
})

describe('tickUnit', () => {
  it('刻度落在整分钟上时不写秒', () => {
    expect(tickUnit(10_000, [0, 600_000, 1_200_000])).toBe(60_000)
  })

  it('刻度本身就是秒级时保留秒', () => {
    expect(tickUnit(1_000, [0, 30_000, 60_000])).toBe(1_000)
  })

  it('桶宽已经是分钟以上时按桶宽', () => {
    expect(tickUnit(3_600_000, [0, 21_600_000])).toBe(3_600_000)
  })
})

describe('placeTicks', () => {
  const at = (...xs: number[]) => xs.map((x, i) => ({ t: i, x, text: '09-21' }))

  it('中间的刻度居中', () => {
    expect(placeTicks(at(150), 300)[0].anchor).toBe('middle')
  })

  it('贴右边的刻度朝左对齐，不被裁掉半截', () => {
    expect(placeTicks(at(295), 300)[0].anchor).toBe('end')
  })

  it('贴左边的刻度朝右对齐', () => {
    expect(placeTicks(at(5), 300)[0].anchor).toBe('start')
  })

  it('朝里挪了之后撞上前一个的就不写', () => {
    // 末一个朝左对齐后会压到 270 那个上
    expect(placeTicks(at(100, 270, 298), 300).map((k) => k.x)).toEqual([100, 270])
  })
})
