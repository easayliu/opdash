import { describe, expect, it } from 'vitest'
import type { TableMeta } from '@/api/types'
import { rawColumns } from '@/components/BillDetailTable'
import { OTHER_SECTION, fieldDoc } from './billFields'

describe('账单原始字段的说明', () => {
  it('说明与分组照 goscan', () => {
    expect(fieldDoc('alicloud', 'nick_name')).toMatchObject({ section: '订单信息', label: '用户昵称' })
    expect(fieldDoc('volcengine', 'ExpenseDate')).toMatchObject({ section: '时间字段', label: '消费日期' })
    // 拿不准的只列出、不作说明
    expect(fieldDoc('volcengine', 'CreditCarriedAmount').label).toBe('')
    // goscan 日后加的新列照常可选
    expect(fieldDoc('alicloud', 'brand_new')).toMatchObject({ section: OTHER_SECTION, label: '' })
  })

  it('原始字段按分组排，阿里云的 nick_name 默认显示', () => {
    const meta: TableMeta = {
      table: 'alicloud_bill_daily',
      dimensions: [],
      columns: [
        { name: 'brand_new', type: 'String', kind: 'string' },
        { name: 'nick_name', type: 'String', kind: 'string' },
        { name: 'pretax_amount', type: 'Decimal(20, 8)', kind: 'float' },
        { name: 'instance_id', type: 'String', kind: 'string' },
      ],
    }
    const cols = rawColumns({ meta, provider: 'alicloud' })
    expect(cols.map((c) => c.raw)).toEqual(['instance_id', 'pretax_amount', 'nick_name', 'brand_new'])
    const nick = cols.find((c) => c.raw === 'nick_name')
    expect(nick).toMatchObject({ key: 'raw:nick_name', label: '用户昵称', shown: true })
    // 没有说明的以字段名作列名；金额按数值排
    expect(cols.find((c) => c.raw === 'brand_new')?.label).toBe('brand_new')
    expect(cols.find((c) => c.raw === 'pretax_amount')?.numeric).toBe(true)
  })
})
