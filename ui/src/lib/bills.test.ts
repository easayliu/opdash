import { describe, expect, it } from 'vitest'
import { changeRatio, dayTick, daysInMonth, formatChange, formatMoneyShort, periodSpan, periodTick, periodsBetween, shiftPeriod } from './bills'

describe('账期算术', () => {
  it('跨年加减', () => {
    expect(shiftPeriod('2026-01', -1)).toBe('2025-12')
    expect(shiftPeriod('2026-12', 1)).toBe('2027-01')
    expect(shiftPeriod('2026-09', -5)).toBe('2026-04')
    // 不合法的原样返回，页面不会因为一个坏参数白屏
    expect(shiftPeriod('2026/09', -1)).toBe('2026/09')
    expect(shiftPeriod('2026-13', 1)).toBe('2026-13')
  })

  it('区间长度和展开', () => {
    expect(periodSpan('2026-04', '2026-09')).toBe(6)
    expect(periodSpan('2026-09', '2026-09')).toBe(1)
    // 反的区间不是「负几个月」，是「没有」
    expect(periodSpan('2026-09', '2026-04')).toBe(0)
    expect(periodsBetween('2025-11', '2026-02')).toEqual(['2025-11', '2025-12', '2026-01', '2026-02'])
    expect(periodsBetween('2026-09', '2026-04')).toEqual([])
  })

  it('坐标轴标签：一月带年份当分界', () => {
    expect(periodTick('2026-09')).toBe('9月')
    expect(periodTick('2026-01')).toBe('26年1月')
    expect(dayTick('2026-09-01')).toBe('9/1')
  })
})

describe('金额和环比', () => {
  it('上万折成万，小额保留两位', () => {
    expect(formatMoneyShort(1234.5)).toBe('1,234.50')
    expect(formatMoneyShort(12345)).toBe('1.2万')
    expect(formatMoneyShort(123456789)).toBe('1.2亿')
  })

  it('上期是 0 时没有环比，不是 ∞', () => {
    expect(changeRatio(100, 80)).toBeCloseTo(0.25)
    expect(changeRatio(100, 0)).toBeNull()
    expect(formatChange(0.25)).toBe('+25.0%')
    expect(formatChange(-0.04)).toBe('-4.0%')
    expect(formatChange(null)).toBe('—')
  })
})

describe('账期的天数', () => {
  it('按自然月给出天数，闰年二月也对', () => {
    expect(daysInMonth('2026-10')).toBe(31)
    expect(daysInMonth('2026-09')).toBe(30)
    expect(daysInMonth('2026-02')).toBe(28)
    expect(daysInMonth('2024-02')).toBe(29)
    // 格式不对时给一个不会让预估爆掉的默认值
    expect(daysInMonth('2026')).toBe(30)
  })
})
