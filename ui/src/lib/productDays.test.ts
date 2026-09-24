import { describe, expect, it } from 'vitest'
import type { BillProductDaysResponse } from '@/api/types'
import { compare, defaultEnd, endOptions, trend } from './productDays'

/** 9/1 至 9/24 连续 24 天；ECS 每天 100，最近 7 天涨到 120；对象存储只在最后两天有 */
function fixture(overrides: Partial<BillProductDaysResponse> = {}): BillProductDaysResponse {
  const days = Array.from({ length: 24 }, (_, k) => `2026-09-${String(k + 1).padStart(2, '0')}`)
  return {
    amount: 'payable',
    today: '2026-09-24',
    days,
    last_by_provider: { alicloud: '2026-09-23', volcengine: '2026-09-24' },
    monthly_only: [],
    rows: [
      // 24 日阿里云尚未出账
      { provider: 'alicloud', product: '云服务器 ECS', amounts: days.map((_, k) => (k === 23 ? 0 : k >= 16 ? 120 : 100)) },
      { provider: 'volcengine', product: '对象存储', amounts: days.map((_, k) => (k === 22 ? 50 : k === 23 ? 30 : 0)) },
    ],
    stats: { read_rows: 0, read_bytes: 0, result_rows: 0, elapsed_ms: 0 },
    ...overrides,
  }
}

describe('产品费用对比', () => {
  it('可选的截止日：对比段须完整落在数据之内', () => {
    const pd = fixture()
    expect(endOptions(pd, '1').at(-1)).toBe('2026-09-02')
    expect(endOptions(pd, 'week').at(-1)).toBe('2026-09-08')
    // 近 7 天与前 7 天要 14 天，最早截止到 9/14
    expect(endOptions(pd, '7').at(-1)).toBe('2026-09-14')
    expect(endOptions(pd, '30')).toEqual([])
    expect(endOptions(pd, '1')[0]).toBe('2026-09-24')
  })

  it('默认截止到各云都已出账、且不是今天的最后一天', () => {
    // 火山出到 24 日（今天），阿里云只到 23 日
    expect(defaultEnd(fixture(), '7')).toBe('2026-09-23')
    // 两朵云都出到 24 日，但 24 日是今天
    expect(defaultEnd(fixture({ last_by_provider: { alicloud: '2026-09-24', volcengine: '2026-09-24' } }), '1')).toBe('2026-09-23')
    // 到了 25 日，24 日便是完整的昨天
    expect(defaultEnd(fixture({ today: '2026-09-25', last_by_provider: { alicloud: '2026-09-24', volcengine: '2026-09-24' } }), '1')).toBe('2026-09-24')
    expect(defaultEnd(fixture(), '30')).toBeNull()
  })

  it('与前一日比', () => {
    const c = compare(fixture(), '1', '2026-09-23')
    expect(c.current).toMatchObject({ from: '2026-09-23', to: '2026-09-23' })
    expect(c.previous).toMatchObject({ from: '2026-09-22', to: '2026-09-22' })
    expect(c.rows.map((r) => [r.product, r.current, r.previous, r.delta])).toEqual([
      ['云服务器 ECS', 120, 120, 0],
      ['对象存储', 50, 0, 50],
    ])
    expect(c.pending).toEqual([])
  })

  it('近 7 天与前 7 天：比合计，走势垫上对比段', () => {
    const c = compare(fixture(), '7', '2026-09-23')
    expect(c.current).toMatchObject({ from: '2026-09-17', to: '2026-09-23' })
    expect(c.previous).toMatchObject({ from: '2026-09-10', to: '2026-09-16' })
    expect(c.basis).toBe('total')
    const ecs = c.rows[0]
    expect([ecs.current, ecs.previous, ecs.delta]).toEqual([840, 700, 140])
    expect(c.currentTotal).toBe(890)
  })

  it('与上周同日比', () => {
    const c = compare(fixture(), 'week', '2026-09-23')
    expect(c.previous).toMatchObject({ from: '2026-09-16', to: '2026-09-16' })
    expect(c.rows[0].delta).toBe(20)
  })

  it('某一段缺了几天账单时改按日均比', () => {
    const pd = fixture()
    // 9/12、9/13 两天什么都没同步
    pd.rows = pd.rows.map((r) => ({ ...r, amounts: r.amounts.map((v, k) => (k === 11 || k === 12 ? 0 : v)) }))
    const c = compare(pd, '7', '2026-09-23')
    expect(c.previous.gaps).toEqual(['2026-09-12', '2026-09-13'])
    expect(c.basis).toBe('daily')
    // 本段 840 ÷ 7 天，对比段 500 ÷ 5 天
    expect([c.rows[0].current, c.rows[0].previous]).toEqual([120, 100])
  })

  it('截止到某朵云尚未出账的日子时标出那朵云', () => {
    expect(compare(fixture(), '1', '2026-09-24').pending).toEqual(['alicloud'])
  })

  it('走势：按区间比时对比段按天对齐叠上', () => {
    const pd = fixture()
    const t = trend(pd, '7', '2026-09-23', pd.rows[0].amounts)
    expect(t.points).toHaveLength(7)
    expect(t.points[0]).toEqual({ day: '2026-09-17', value: 120, previousDay: '2026-09-10', previous: 100 })
    expect(t.marks).toEqual([])
  })

  it('走势：单日的比法画 14 天，标出被比较的两天', () => {
    const pd = fixture()
    const t = trend(pd, 'week', '2026-09-23', pd.rows[0].amounts)
    expect(t.points.map((p) => p.day)).toEqual(pd.days.slice(9, 23))
    expect(t.marks).toEqual(['2026-09-16', '2026-09-23'])
    // 数据不足 14 天时从第一天画起
    expect(trend(pd, '1', '2026-09-05', pd.rows[0].amounts).points).toHaveLength(5)
  })
})
