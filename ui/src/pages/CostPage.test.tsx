import { describe, expect, it, vi } from 'vitest'
import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { MemoryRouter } from 'react-router'
import { CostPage } from './CostPage'

const META = {
  version: 'test',
  database: 'logs',
  timezone: 'Asia/Shanghai',
  now_ms: Date.now(),
  limits: { max_rows: 1000, max_offset: 10000, export_max_rows: 50000, max_trace_spans: 5000, max_range_ms: 0, query_timeout_ms: 30000 },
  server: { version: 'test', timezone: 'Asia/Shanghai' },
  logs: { table: 'app_log', columns: [], dimensions: [] },
  traces: { table: 'otel_trace', columns: [], dimensions: [] },
  metrics: null,
  bills: {
    providers: ['volcengine', 'alicloud'],
    daily_providers: ['volcengine'],
    volcengine: { table: 'volcengine_bill', columns: [], dimensions: [] },
    alicloud_monthly: { table: 'alicloud_bill_monthly', columns: [], dimensions: [] },
    alicloud_daily: null,
    dedupe: 'group',
    sync: true,
  },
}

const STATS = { read_rows: 1, read_bytes: 2, result_rows: 1, elapsed_ms: 3 }

/** 一份「两朵云、三个账期」的假账单。没有后端，这里验的是页面画不画得出来 */
function stubApi(overrides: Record<string, unknown> = {}) {
  const bodies: Record<string, unknown> = {
    '/api/meta': META,
    '/api/bills/periods': { periods: ['2026-07', '2026-08', '2026-09'], latest: '2026-09', providers: ['volcengine', 'alicloud'], stats: STATS },
    '/api/bills/summary': {
      from: '2026-07',
      to: '2026-09',
      amount: 'payable',
      points: [
        { t: '2026-07', total: 0, by_provider: {} },
        { t: '2026-08', total: 100, by_provider: { volcengine: 100 } },
        { t: '2026-09', total: 250, by_provider: { volcengine: 200, alicloud: 50 } },
      ],
      total: 350,
      by_provider: { volcengine: 300, alicloud: 50 },
      providers: ['volcengine', 'alicloud'],
      stats: STATS,
    },
    '/api/bills/daily': {
      from: '2026-07',
      to: '2026-09',
      amount: 'payable',
      points: [{ t: '2026-09-01', total: 10, by_provider: { volcengine: 10 } }],
      total: 10,
      providers: ['volcengine'],
      stats: STATS,
    },
    '/api/bills/breakdown': {
      by: 'product',
      label: '产品',
      from: '2026-07',
      to: '2026-09',
      amount: 'payable',
      rows: [{ key: '云服务器 ECS', amount: 280, share: 0.8, by_provider: { volcengine: 230, alicloud: 50 } }],
      other: 70,
      total: 350,
      stats: STATS,
    },
    '/api/bills/detail': {
      provider: 'volcengine',
      granularity: 'daily',
      from: '2026-07',
      to: '2026-09',
      amount: 'payable',
      rows: [
        {
          provider: 'volcengine',
          period: '2026-09',
          day: '2026-09-01',
          product: '云服务器',
          item: '按量-CPU',
          instance_id: 'i-abc',
          instance: 'web-1',
          region: '华北2',
          account: '主账号',
          project: '默认',
          subscription: '按量计费',
          usage: '720',
          usage_unit: '小时',
          currency: 'CNY',
          amount: 12.34,
          original: 20,
          paid: 12.34,
        },
      ],
      total: 1,
      limit: 50,
      offset: 0,
      stats: STATS,
    },
    ...overrides,
  }
  vi.stubGlobal('fetch', async (input: RequestInfo | URL) => {
    const url = new URL(String(input), 'http://localhost')
    const body = bodies[url.pathname] ?? {}
    return new Response(JSON.stringify(body), { status: 200, headers: { 'content-type': 'application/json' } })
  })
}

function page(path = '/cost') {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={[path]}>
        <CostPage />
      </MemoryRouter>
    </QueryClientProvider>,
  )
}

describe('费用页', () => {
  it('画出账期趋势、排行和明细', async () => {
    stubApi()
    page()
    // 最后一个账期是「本期」，上一个是对比
    expect(await screen.findByText('2026-09 花费')).toBeInTheDocument()
    expect(await screen.findByText('250.00')).toBeInTheDocument()
    // 环比 (250-100)/100
    expect(await screen.findByText('+150.0%')).toBeInTheDocument()
    // 三个账期的柱子都在轴上（没数据的 7 月也要占一格）
    const chart = await screen.findByRole('img', { name: /按账期的花费/ })
    expect(chart).toBeInTheDocument()
    expect(within(chart).getAllByRole('button')).toHaveLength(3)
    // 排行和「其它」
    expect(await screen.findByText('云服务器 ECS')).toBeInTheDocument()
    expect(await screen.findByText(/未进入排行的其余项合计/)).toBeInTheDocument()
    // 明细
    expect(await screen.findByText('web-1')).toBeInTheDocument()
    expect(await screen.findByText('12.34')).toBeInTheDocument()
  })

  it('点排行里的一项就把它加成筛选条件', async () => {
    stubApi()
    page()
    await userEvent.click(await screen.findByRole('button', { name: /云服务器 ECS/ }))
    // 条件行冒出来，里面是「产品 = 云服务器 ECS」和一个去掉它的按钮
    const chips = (await screen.findByText('筛选')).parentElement!
    expect(within(chips).getByText('产品')).toBeInTheDocument()
    expect(within(chips).getByText('云服务器 ECS')).toBeInTheDocument()
  })

  it('配了 goscan 才显示「拉取账单」', async () => {
    stubApi()
    page()
    expect(await screen.findByRole('button', { name: /拉取账单/ })).toBeInTheDocument()
  })

  it('没配 goscan 就不显示「拉取账单」', async () => {
    stubApi({ '/api/meta': { ...META, bills: { ...META.bills, sync: false } } })
    page()
    await screen.findByText('2026-09 花费')
    expect(screen.queryByRole('button', { name: /拉取账单/ })).not.toBeInTheDocument()
  })

  it('表在但一条账单都没有时，说清楚是同步还没跑', async () => {
    stubApi({ '/api/bills/periods': { periods: [], latest: null, providers: ['volcengine'], stats: STATS } })
    page()
    expect(await screen.findByText('账单表暂无数据')).toBeInTheDocument()
  })

  it('没部署 goscan 就只给一句原因', async () => {
    stubApi({ '/api/meta': { ...META, bills: null, bills_note: 'logs.volcengine_bill 不存在' } })
    page()
    expect(await screen.findByText('费用页未启用')).toBeInTheDocument()
    expect(await screen.findByText(/volcengine_bill 不存在/)).toBeInTheDocument()
  })
})
