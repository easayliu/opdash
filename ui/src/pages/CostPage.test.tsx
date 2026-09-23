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
    allocation: { lines: ['甲线', '乙线', '公共'], rules: 2 },
  },
}

const STATS = { read_rows: 1, read_bytes: 2, result_rows: 1, elapsed_ms: 3 }

/** 一份分摊结果：甲线由两条规则构成，还有一笔没命中规则 */
const ALLOCATION = {
  from: '2026-07',
  to: '2026-09',
  amount: 'payable',
  configured: true,
  window_days: 7,
  days: 2,
  days_by_provider: { volcengine: 2, alicloud: 2 },
  granularity: 'daily',
  prepaid: true,
  total: 1000,
  postpaid: 800,
  amortized: 200,
  amortized_by_period: { '2026-09': 200, '2026-10': 200, '2026-11': 200 },
  daily: 400,
  lines: [
    {
      name: '甲线',
      amount: 600,
      postpaid: 400,
      amortized: 200,
      daily: 200,
      share: 0.6,
      amortized_by_period: { '2026-09': 200, '2026-10': 200 },
      items: [
        { product: '云服务器 ECS', rule: '甲线专用机器', amount: 300, daily: 150, share: 0.3, prepaid: false },
        { product: '云服务器 ECS', rule: 'ECS 其余部分', amount: 100, daily: 50, share: 0.1, prepaid: false },
        { product: '云服务器 ECS', rule: '包年包月机器', amount: 200, daily: null, share: 0.2, prepaid: true },
      ],
    },
    { name: '乙线', amount: 300, postpaid: 300, amortized: 0, daily: 150, share: 0.3, amortized_by_period: {}, items: [] },
    { name: '公共', amount: 100, postpaid: 100, amortized: 0, daily: 50, share: 0.1, amortized_by_period: {}, items: [] },
  ],
  unmatched: {
    name: '未归属',
    amount: 100,
    postpaid: 100,
    amortized: 0,
    daily: 50,
    share: 0.1,
    amortized_by_period: {},
    items: [{ product: '对象存储', rule: null, amount: 100, daily: 50, share: 0.1, prepaid: false }],
  },
  products: [{ product: '云服务器 ECS', rule: null, amount: 700, daily: 350, share: 0.7, prepaid: false }],
  points: [
    { t: '2026-09-20', total: 550, by_line: { 甲线: 250, 乙线: 300 } },
    { t: '2026-09-21', total: 250, by_line: { 甲线: 150, 公共: 100 } },
  ],
  coverage: null,
  stats: STATS,
}

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

  it('下拉一律用筛选栏的自绘下拉，不再弹系统原生菜单', async () => {
    stubApi()
    const { container } = page()
    await screen.findByText('2026-09 花费')
    expect(container.querySelector('select')).toBeNull()

    // 云筛选：第一项是「全部云」，选中某一朵云后触发器描成强调色，表示筛选生效
    await userEvent.click(screen.getByRole('button', { name: '按云筛选' }))
    const options = screen.getAllByRole('option').map((o) => o.textContent)
    expect(options).toEqual(['全部云', '火山引擎', '阿里云'])
    await userEvent.click(screen.getByRole('option', { name: '阿里云' }))
    const trigger = screen.getByRole('button', { name: '按云筛选' })
    expect(trigger).toHaveTextContent('阿里云')
    expect(trigger.className).toContain('border-accent')

    // 账期这类永远有值的下拉不亮：它不是「正在生效的筛选」
    expect(screen.getByRole('button', { name: '起始账期' }).className).not.toContain('border-accent')
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

  it('分析视图把账单摊到业务线，并按日均给出月度预估', async () => {
    stubApi({ '/api/bills/allocation': ALLOCATION })
    page('/cost?view=analysis&days=7&est=2026-10')
    // 日均按「有账单的两天」求得，而不是按自然月的天数
    expect(await screen.findByText('日均')).toBeInTheDocument()
    // 日均那张卡片与甲线那一行都是 400.00，此处只确认它确实出现
    expect((await screen.findAllByText('400.00')).length).toBeGreaterThan(0)
    // 10 月 31 天：后付费 400 × 31，再加该月摊过来的预付费 200
    expect(await screen.findByText('2026-10 预估')).toBeInTheDocument()
    expect(await screen.findByText('12,600.00')).toBeInTheDocument()
    // 合计分两段列出，预付费摊销单独标明
    expect(await screen.findByText(/后付费 800.00 · 预付费摊销 200.00/)).toBeInTheDocument()
    // 三条业务线各占一行（图例上还有一份同名的），未归属单列
    expect((await screen.findAllByText('甲线')).length).toBeGreaterThan(0)
    expect(await screen.findByText('未归属')).toBeInTheDocument()

    // 展开甲线，看得到它由哪两条规则构成
    await userEvent.click((await screen.findAllByRole('button', { name: /甲线/ }))[0])
    expect(await screen.findByText('甲线专用机器')).toBeInTheDocument()
    expect(await screen.findByText('ECS 其余部分')).toBeInTheDocument()
    // 预付费摊来的那一行标着「摊销」，且不给日均
    expect(await screen.findByText('摊销')).toBeInTheDocument()
  })

  it('日度账单不完整时如实提示，并给出补拉的办法', async () => {
    stubApi({
      '/api/bills/allocation': {
        ...ALLOCATION,
        days_by_provider: { alicloud: 1 },
        coverage: { provider: 'alicloud', daily: 7618.53, monthly: 152958.18 },
      },
    })
    page('/cost?view=analysis')
    expect(await screen.findByRole('alert')).toHaveTextContent('阿里云的日度账单不完整')
    // 覆盖比例：7618.53 / 152958.18
    expect(screen.getByRole('alert')).toHaveTextContent('5.0%')
    // 配了 goscan 就指向「拉取账单」
    expect(screen.getByRole('alert')).toHaveTextContent('拉取账单')
  })

  it('没配归属规则时只给按产品的日均，并说明怎么配', async () => {
    stubApi({
      '/api/meta': { ...META, bills: { ...META.bills, allocation: null } },
      '/api/bills/allocation': { ...ALLOCATION, configured: false, lines: [] },
    })
    page('/cost?view=analysis')
    expect(await screen.findByText('尚未配置成本归属规则')).toBeInTheDocument()
    expect((await screen.findAllByText(/bill-alloc/)).length).toBeGreaterThan(0)
    // 按产品那张表照常在
    expect(await screen.findByText('云服务器 ECS')).toBeInTheDocument()
  })

  it('没部署 goscan 就只给一句原因', async () => {
    stubApi({ '/api/meta': { ...META, bills: null, bills_note: 'logs.volcengine_bill 不存在' } })
    page()
    expect(await screen.findByText('费用页未启用')).toBeInTheDocument()
    expect(await screen.findByText(/volcengine_bill 不存在/)).toBeInTheDocument()
  })
})
