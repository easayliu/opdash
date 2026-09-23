import { describe, expect, it, vi } from 'vitest'
import type { BillAllocationResponse } from '@/api/types'

// 不真的写文件：截下交给 write-excel-file 的工作表，验结构与数字
const written: { sheets: { sheet: string; data: unknown[][] }[]; file: string }[] = []
vi.mock('write-excel-file/browser', () => ({
  default: (sheets: { sheet: string; data: unknown[][] }[]) => ({
    toFile: async (file: string) => void written.push({ sheets, file }),
  }),
}))

const line = (name: string, daily: number) => ({
  name, amount: 0, postpaid: 0, amortized: 0, daily, share: 0, amortized_by_period: {}, items: [],
  by_provider: { alicloud: { daily, amortized_by_period: {} } },
})
const DATA = {
  from: '2026-08', to: '2026-09', amount: 'payable', configured: true, window_days: null, days: 2,
  days_by_provider: { alicloud: 2 }, granularity: 'daily', prepaid: false, total: 400, postpaid: 400, amortized: 0,
  amortized_by_period: {}, daily: 30, lines: [line('甲线', 10), line('乙线', 20)],
  unmatched: { ...line('未归属', 0), by_provider: {} }, unmatched_into: null, products: [{ product: 'ECS', rule: null, amount: 400, daily: 30, share: 1, prepaid: false }],
  points: [{ t: '2026-09-20', total: 10, by_line: {} }],
  monthly: [
    { provider: 'alicloud', kind: 'postpaid', line: '甲线', by_period: { '2026-08': 100.004, '2026-09': 50 } },
    { provider: 'alicloud', kind: 'postpaid', line: '乙线', by_period: { '2026-09': 249.996 } },
  ],
  coverage: null, stats: {},
} as unknown as BillAllocationResponse

describe('exportAllocXlsx', () => {
  it('四个工作表，按月拆分的合计与页面同一份计算，金额舍入到分', async () => {
    const { exportAllocXlsx } = await import('./allocXlsx')
    await exportAllocXlsx({ data: DATA, order: ['甲线', '乙线'], nights: 31, estimate: '2026-10', amountLabel: '应付', windowLabel: '所选账期' })
    const { sheets, file } = written[0]
    expect(file).toBe('费用拆分_2026-08_2026-09.xlsx')
    expect(sheets.map((s) => s.sheet)).toEqual(['按月拆分', '按云厂商与付费方式汇总', '按产品', '口径说明'])

    const split = sheets[0].data
    const value = (c: unknown) => (c as { value?: unknown } | undefined)?.value
    const total = split.find((r) => value(r[0]) === '合计')!
    // 列：业务线、8 月、9 月、小计、占比、日均、预估
    expect(total.slice(1).map(value)).toEqual([100, 300, 400, 1, 30, 930])
    // 甲线 8 月 100.004 → 100.00
    const jia = split.find((r) => value(r[0]) === '甲线')!
    expect(value(jia[1])).toBe(100)
  })
})
