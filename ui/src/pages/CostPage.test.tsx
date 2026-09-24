import { describe, expect, it, vi } from 'vitest'
import { render, screen, waitFor, within } from '@testing-library/react'
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
    volcengine: {
      table: 'volcengine_bill',
      columns: [
        { name: 'ExpenseDate', type: 'String', kind: 'string' },
        { name: 'Count', type: 'Decimal(20, 8)', kind: 'float' },
      ],
      dimensions: [],
    },
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
      by_provider: {
        alicloud: { daily: 120, amortized_by_period: { '2026-10': 200 } },
        volcengine: { daily: 80, amortized_by_period: {} },
      },
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
  unmatched_into: null,
  monthly: [
    { provider: 'alicloud', kind: 'postpaid', line: '甲线', by_period: { '2026-09': 250 } },
    { provider: 'volcengine', kind: 'postpaid', line: '甲线', by_period: { '2026-09': 150 } },
    { provider: 'alicloud', kind: 'prepaid', line: '甲线', by_period: { '2026-09': 200 } },
    { provider: 'alicloud', kind: 'postpaid', line: '乙线', by_period: { '2026-08': 100, '2026-09': 200 } },
    { provider: 'volcengine', kind: 'postpaid', line: '公共', by_period: { '2026-09': 100 } },
    { provider: 'alicloud', kind: 'postpaid', line: null, by_period: { '2026-09': 100 } },
  ],
  products: [{ product: '云服务器 ECS', rule: null, amount: 700, daily: 350, share: 0.7, prepaid: false }],
  points: [
    { t: '2026-09-20', total: 550, by_line: { 甲线: 250, 乙线: 300 } },
    { t: '2026-09-21', total: 250, by_line: { 甲线: 150, 公共: 100 } },
  ],
  coverage: null,
  stats: STATS,
}

/** 产品费用对比：今天 9/22，两朵云都出到 9/21 */
const PRODUCT_DAYS = {
  amount: 'payable',
  today: '2026-09-22',
  days: ['2026-09-19', '2026-09-20', '2026-09-21', '2026-09-22'],
  last_by_provider: { alicloud: '2026-09-21', volcengine: '2026-09-21' },
  monthly_only: [],
  rows: [
    { provider: 'alicloud', product: '云服务器 ECS', amounts: [300, 400, 250, 0] },
    { provider: 'volcengine', product: '对象存储', amounts: [0, 0, 100, 0] },
  ],
  stats: STATS,
}

/** 按天钻取：21 日与 20 日 */
const ALLOC_DAY = {
  day: '2026-09-21',
  previous_day: '2026-09-20',
  amount: 'payable',
  configured: true,
  current: 250,
  previous: 550,
  lines: [
    {
      name: '甲线',
      current: 150,
      previous: 250,
      items: [
        { provider: 'alicloud', product: '云服务器 ECS', rule: '甲线专用机器', current: 150, previous: 150 },
        { provider: 'alicloud', product: '云服务器 ECS', rule: 'ECS 其余部分', current: 0, previous: 100 },
      ],
    },
    { name: '乙线', current: 0, previous: 300, items: [{ provider: 'alicloud', product: '云服务器 ECS', rule: 'ECS 其余部分', current: 0, previous: 300 }] },
  ],
  unmatched: { name: '未归属', current: 100, previous: 0, items: [{ provider: 'volcengine', product: '对象存储', rule: null, current: 100, previous: 0 }] },
  unmatched_into: null,
  products: [
    { provider: 'alicloud', product: '云服务器 ECS', rule: null, current: 150, previous: 550 },
    { provider: 'volcengine', product: '对象存储', rule: null, current: 100, previous: 0 },
  ],
  stats: { read_rows: 1, read_bytes: 2, result_rows: 1, elapsed_ms: 3 },
}

/** 页面发出去的请求，按先后记下，断言「带没带某个参数」用 */
const seen: URL[] = []

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
    '/api/bills/product-days': PRODUCT_DAYS,
    '/api/bills/allocation/day': ALLOC_DAY,
    '/api/bills/facets': { facets: { product: ['云服务器', '对象存储'], region: ['华北2'] }, stats: STATS },
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
          extra: { ExpenseDate: '2026-09-01T08:00' },
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
    seen.push(url)
    const hit = bodies[url.pathname] ?? {}
    // 给函数的按请求参数现算，模拟「带了筛选条件，接口回的就变少」
    const body = typeof hit === 'function' ? (hit as (u: URL) => unknown)(url) : hit
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

/** 构成图的图例按钮：列表行也是同名按钮，图例靠 `aria-pressed` 区分 */
const legends = (name: string) =>
  screen.queryAllByRole('button', { name: new RegExp(`^${name}`) }).filter((b) => b.hasAttribute('aria-pressed'))
const findLegend = async (name: string) => {
  await screen.findAllByRole('button', { name: new RegExp(`^${name}`) })
  const [el] = legends(name)
  if (!el) throw new Error(`没有「${name}」的图例`)
  return el
}

describe('费用页', () => {
  it('画出账期趋势、排行和明细', async () => {
    stubApi()
    page()
    // 最后一个账期是「本期」，上一个是对比
    expect(await screen.findByText('2026-09 费用')).toBeInTheDocument()
    expect(await screen.findByText('250.00')).toBeInTheDocument()
    // 环比 (250-100)/100
    expect(await screen.findByText('+150.0%')).toBeInTheDocument()
    // 三个账期的柱子都在轴上（没数据的 7 月也要占一格）
    const chart = await screen.findByRole('img', { name: /按账期的费用/ })
    expect(chart).toBeInTheDocument()
    for (const month of ['7月', '8月', '9月']) expect(within(chart).getByText(month)).toBeInTheDocument()
    // 排行和「其它」
    expect(await screen.findByText('云服务器 ECS')).toBeInTheDocument()
    expect(await screen.findByText(/未进入排行的其余项合计/)).toBeInTheDocument()
    // 明细
    expect(await screen.findByText('web-1')).toBeInTheDocument()
    expect(await screen.findByText('12.34')).toBeInTheDocument()
  })

  it('明细可按列排序、在表头筛选，并能自选显示哪些列', async () => {
    localStorage.clear()
    seen.length = 0
    stubApi()
    page()
    await screen.findByText('web-1')
    const detailCalls = () => seen.filter((u) => u.pathname === '/api/bills/detail')
    // 排序交给服务端：文字列先按 A–Z，再点一次换方向
    await userEvent.click(screen.getByRole('button', { name: '地域' }))
    expect(detailCalls().at(-1)?.searchParams.get('sort')).toBe('region')
    expect(detailCalls().at(-1)?.searchParams.get('order')).toBe('asc')
    await userEvent.click(screen.getByRole('button', { name: '地域' }))
    expect(detailCalls().at(-1)?.searchParams.get('order')).toBe('desc')
    // 导出的顺序与页面一致
    expect(screen.getByRole('link', { name: /导出 CSV/ }).getAttribute('href')).toContain('sort=region')

    // 候选值点开下拉才查
    expect(seen.some((u) => u.pathname === '/api/bills/facets')).toBe(false)
    await userEvent.click(screen.getByRole('button', { name: '按产品筛选' }))
    await userEvent.click(await screen.findByRole('option', { name: '对象存储' }))
    expect(seen.find((u) => u.pathname === '/api/bills/facets')?.searchParams.get('dims')).toContain('product')
    // 表头筛选与点排行是同一套条件，顶上的筛选条同样列出
    const chips = (await screen.findByText('筛选')).parentElement!
    expect(within(chips).getByText('对象存储')).toBeInTheDocument()
    expect(detailCalls().at(-1)?.searchParams.get('product')).toBe('对象存储')

    // 账号默认不显示，勾上即出现，并记在浏览器里
    expect(screen.queryByRole('columnheader', { name: /账号/ })).toBeNull()
    await userEvent.click(screen.getByRole('button', { name: '显示列' }))
    await userEvent.click(await screen.findByRole('checkbox', { name: '账号' }))
    expect(await screen.findByRole('columnheader', { name: /账号/ })).toBeInTheDocument()
    expect(screen.getByText('主账号')).toBeInTheDocument()
    expect(localStorage.getItem('opdash.bills.detail-columns.v2')).toContain('account')

    // 原始字段：列出这张账单表的全部列，以中文说明作列名，按字段名也搜得到；勾了才向接口要，且能按它排序
    await userEvent.type(screen.getByRole('textbox', { name: '搜索字段' }), 'expense')
    expect(screen.queryByRole('checkbox', { name: /Count/ })).toBeNull()
    await userEvent.click(screen.getByRole('checkbox', { name: /消费日期/ }))
    expect(await screen.findByText('2026-09-01T08:00')).toBeInTheDocument()
    expect(detailCalls().at(-1)?.searchParams.get('cols')).toBe('ExpenseDate')
    await userEvent.keyboard('{Escape}')
    await userEvent.click(screen.getByRole('button', { name: '消费日期' }))
    expect(detailCalls().at(-1)?.searchParams.get('sort')).toBe('raw:ExpenseDate')
    localStorage.clear()
  })

  it('下拉一律用筛选栏的自绘下拉，不再弹系统原生菜单', async () => {
    stubApi()
    const { container } = page()
    await screen.findByText('2026-09 费用')
    expect(container.querySelector('select')).toBeNull()

    // 云筛选：第一项是「全部云」，选中某一朵云后触发器描成强调色，表示筛选生效
    await userEvent.click(screen.getByRole('button', { name: '按云厂商筛选' }))
    const options = screen.getAllByRole('option').map((o) => o.textContent)
    expect(options).toEqual(['全部云厂商', '火山引擎', '阿里云'])
    await userEvent.click(screen.getByRole('option', { name: '阿里云' }))
    const trigger = screen.getByRole('button', { name: '按云厂商筛选' })
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

  it('配了 goscan 才显示「同步账单」', async () => {
    stubApi()
    page()
    expect(await screen.findByRole('button', { name: /同步账单/ })).toBeInTheDocument()
  })

  it('没配 goscan 就不显示「同步账单」', async () => {
    stubApi({ '/api/meta': { ...META, bills: { ...META.bills, sync: false } } })
    page()
    await screen.findByText('2026-09 费用')
    expect(screen.queryByRole('button', { name: /同步账单/ })).not.toBeInTheDocument()
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
    // 统计卡片与按月拆分表的表头各一处
    expect((await screen.findAllByText('2026-10 预估')).length).toBe(2)
    // 卡片与拆分表合计行的预估列各一处
    expect((await screen.findAllByText('12,600.00')).length).toBe(2)
    // 合计分两段列出，预付费摊销单独标明
    expect(await screen.findByText(/后付费 800.00 · 预付费摊销 200.00/)).toBeInTheDocument()

    // 按月拆分：业务线 × 月份，末行两云合计。9 月 1,000，8 月 100，小计 1,100
    const split = await screen.findByRole('region', { name: '按月拆分' })
    expect(within(split).getByText('两云合计', { selector: 'th span' })).toBeInTheDocument()
    expect(within(split).getByText('1,000.00')).toBeInTheDocument()
    expect(within(split).getAllByText('1,100.00').length).toBeGreaterThan(0)
    // 未命中规则、也没并入哪条线的钱单列一行
    expect(within(split).getByText('未归属')).toBeInTheDocument()
    expect(within(split).getByText('未命中规则')).toBeInTheDocument()
    // 切到「阿里云 · 后付费」：只剩阿里云的后付费，合计 250 + 300 + 100 = 650
    await userEvent.click(within(split).getByRole('button', { name: '阿里云 · 后付费' }))
    expect(within(split).getAllByText('650.00').length).toBeGreaterThan(0)
    // 分段页签也给预估：甲线在阿里云的日均 120 × 10 月 31 天
    expect(within(split).getAllByText('3,720.00').length).toBeGreaterThan(0)
    // 预付费摊销页签：预估就是摊入 10 月的金额，不按天计
    await userEvent.click(within(split).getByRole('button', { name: '阿里云 · 预付费摊销' }))
    expect(within(split).getAllByText('200.00').length).toBeGreaterThan(0)
    await userEvent.click(within(split).getByRole('button', { name: '两云合计' }))

    // 按云与付费方式汇总：阿里云两种付费方式各一行，再加阿里云小计与两云合计
    const summary = screen.getByRole('region', { name: '按云厂商与付费方式汇总' })
    expect(within(summary).getByText('阿里云 · 预付费摊销')).toBeInTheDocument()
    expect(within(summary).getByText('阿里云小计')).toBeInTheDocument()
    expect(within(summary).getByText('火山引擎 · 后付费')).toBeInTheDocument()

    // 展开甲线，看得到它由哪两条规则构成
    await userEvent.click(within(split).getByRole('button', { name: '甲线' }))
    expect(await screen.findByText('甲线专用机器')).toBeInTheDocument()
    expect(await screen.findByText('ECS 其余部分')).toBeInTheDocument()
    // 预付费摊来的那一行标着「摊销」，且不给日均
    expect(await screen.findByText('预付费摊销')).toBeInTheDocument()
    // 点这一行的其他位置（小计那一格）同样能收起
    await userEvent.click(within(split).getAllByText('600.00')[0])
    expect(screen.queryByText('甲线专用机器')).not.toBeInTheDocument()
  })

  it('产品费用对比默认比各云都已出账的最后一天与前一天，可按变动排序', async () => {
    stubApi({ '/api/bills/allocation': ALLOCATION })
    page('/cost?view=analysis&est=2026-10')
    const card = await screen.findByRole('region', { name: '产品费用对比' })
    // 22 日是今天、账单未出齐，默认比 21 日：21 日 350，20 日 400，少了 50
    expect(await within(card).findByText('350.00')).toBeInTheDocument()
    expect(within(card).getByText('-50.00（-12.5%）')).toBeInTheDocument()
    // 前一天没花钱的产品标为新增，而不是 ∞
    expect(within(card).getByText('新增')).toBeInTheDocument()
    expect(within(card).getByText('-37.5%')).toBeInTheDocument()
    // 按变动排：对象存储 +100 排到 ECS -150 之后
    await userEvent.click(within(card).getByRole('button', { name: '按变动' }))
    const products = within(card).getAllByRole('row').slice(1).map((r) => within(r).getAllByRole('cell')[0]?.textContent)
    expect(products).toEqual(['云服务器 ECS', '对象存储'])
    // 点开一个产品看逐日走势：写明是哪几天、共几天，并标出比较的那两天
    await userEvent.click(within(card).getByRole('button', { name: '云服务器 ECS' }))
    expect(within(card).getByText('9/19–9/21，共 3 天')).toBeInTheDocument()
    expect(within(card).getByText(/竖线标出比较的两天：9\/20 周日 与 9\/21 周一/)).toBeInTheDocument()
    // 点这一行的其他位置同样能收起
    await userEvent.click(within(card).getByText('-37.5%'))
    expect(within(card).queryByText('9/19–9/21，共 3 天')).not.toBeInTheDocument()
  })

  it('分析视图的按产品与账单明细一致：点列头排序、在产品表头筛选', async () => {
    seen.length = 0
    const products = [
      ...ALLOCATION.products,
      { product: '对象存储', rule: null, amount: 100, daily: 50, share: 0.1, prepaid: false },
    ]
    stubApi({ '/api/bills/allocation': { ...ALLOCATION, products } })
    page('/cost?view=analysis&est=2026-10')
    const card = await screen.findByRole('region', { name: '按产品' })
    const names = () => within(card).getAllByRole('row').slice(1).map((r) => within(r).getAllByRole('cell')[0]?.textContent)
    expect(await within(card).findByText('70.0%')).toBeInTheDocument()
    expect(names()).toEqual(['云服务器 ECS', '对象存储'])
    // 排序在页面上做：默认按金额从大到小，再点一次从小到大
    await userEvent.click(within(card).getByRole('button', { name: '金额' }))
    expect(names()).toEqual(['对象存储', '云服务器 ECS'])
    // 产品筛选写进 URL，与账单视图同一套条件，整个分析视图都只看它
    await userEvent.click(within(card).getByRole('button', { name: '按产品筛选' }))
    await userEvent.click(await screen.findByRole('option', { name: '对象存储' }))
    const chips = (await screen.findByText('筛选')).parentElement!
    expect(within(chips).getByText('对象存储')).toBeInTheDocument()
    expect(seen.filter((u) => u.pathname === '/api/bills/allocation').at(-1)?.searchParams.get('product')).toBe('对象存储')
  })

  it('按产品筛选后只剩所选产品，同名的后付费与预付费摊销两行不会残留', async () => {
    const products = [
      { product: '云服务器 ECS', rule: null, amount: 700, daily: 350, share: 0.7, prepaid: false },
      { product: '云服务器 ECS', rule: null, amount: 200, daily: null, share: 0.2, prepaid: true },
      { product: '对象存储', rule: null, amount: 100, daily: 50, share: 0.1, prepaid: false },
    ]
    stubApi({
      '/api/bills/allocation': (u: URL) => {
        const want = u.searchParams.get('product')
        return { ...ALLOCATION, products: want ? products.filter((p) => p.product === want) : products }
      },
    })
    page('/cost?view=analysis&est=2026-10')
    const card = await screen.findByRole('region', { name: '按产品' })
    const names = () => within(card).getAllByRole('row').slice(1).map((r) => within(r).getAllByRole('cell')[0]?.textContent)
    await within(card).findByText('对象存储')
    expect(names()).toHaveLength(3)
    await userEvent.click(within(card).getByRole('button', { name: '按产品筛选' }))
    await userEvent.click(await screen.findByRole('option', { name: '对象存储' }))
    await waitFor(() => expect(names()).toEqual(['对象存储']))
  })

  it('按天钻取：选一天看各业务线由哪些产品构成，再跳到那一天那个产品的账单明细', async () => {
    seen.length = 0
    stubApi({ '/api/bills/allocation': ALLOCATION })
    page('/cost?view=analysis&est=2026-10')
    await userEvent.click(await screen.findByRole('button', { name: '查看某一天的构成' }))
    await userEvent.click(await screen.findByRole('option', { name: /9\/21/ }))
    const drill = await screen.findByRole('region', { name: /当日构成/ })
    expect(seen.find((u) => u.pathname === '/api/bills/allocation/day')?.searchParams.get('day')).toBe('2026-09-21')
    // 合计与前一天比：250 对 550
    expect(within(drill).getByText('-300.00（-54.5%）')).toBeInTheDocument()
    // 业务线可展开到产品，规则名标在产品旁
    await userEvent.click(within(drill).getByRole('button', { name: '甲线' }))
    expect(within(drill).getByText('甲线专用机器')).toBeInTheDocument()
    // 未命中规则、也没并入哪条线的单列一行
    expect(within(drill).getByText('未归属')).toBeInTheDocument()
    // 按产品：云厂商不止一朵时标出来
    await userEvent.click(within(drill).getByRole('button', { name: '按产品' }))
    expect(within(drill).getByText('火山引擎')).toBeInTheDocument()

    // 跳到账单视图的明细：那一天、那朵云、那个产品
    await userEvent.click(within(drill).getByRole('button', { name: '查看 对象存储 在 2026-09-21 的账单明细' }))
    await screen.findByRole('region', { name: '明细' })
    const call = seen.filter((u) => u.pathname === '/api/bills/detail').at(-1)!
    expect(call.searchParams.get('day_from')).toBe('2026-09-21')
    expect(call.searchParams.get('product')).toBe('对象存储')
    expect(call.searchParams.get('provider')).toBe('volcengine')
    expect(screen.getByRole('button', { name: '明细的日期' })).toHaveTextContent('2026-09-21')
  })

  it('明细可按日期筛选：只作用于明细，选了日期就不再按粒度', async () => {
    seen.length = 0
    stubApi()
    page('/cost?gran=monthly')
    await screen.findByText('web-1')
    await userEvent.click(screen.getByRole('button', { name: '明细的日期' }))
    await userEvent.type(screen.getByPlaceholderText('筛日期，如 09-23…'), '09-23')
    await userEvent.click(await screen.findByRole('option', { name: /2026-09-23/ }))
    const detail = seen.filter((u) => u.pathname === '/api/bills/detail').at(-1)!
    expect(detail.searchParams.get('day_from')).toBe('2026-09-23')
    expect(detail.searchParams.get('granularity')).toBeNull()
    // 统计与排行仍按账期
    expect(seen.filter((u) => u.pathname === '/api/bills/summary').every((u) => !u.searchParams.has('day_from'))).toBe(true)
    // 再选截止日看几天；导出跟着同一段日期
    await userEvent.click(screen.getByRole('button', { name: '明细的截止日期' }))
    await userEvent.click(await screen.findByRole('option', { name: /2026-09-25/ }))
    expect(seen.filter((u) => u.pathname === '/api/bills/detail').at(-1)?.searchParams.get('day_to')).toBe('2026-09-25')
    expect(screen.getByRole('link', { name: /导出 CSV/ }).getAttribute('href')).toContain('day_to=2026-09-25')
  })

  it('构成图画出未归属的部分，点图例可单独查看某条线', async () => {
    // 第二天合计 400，各线只摊到 250：多出的 150 就是未归属
    const points = [ALLOCATION.points[0], { ...ALLOCATION.points[1], total: 400 }]
    stubApi({ '/api/bills/allocation': { ...ALLOCATION, points } })
    page('/cost?view=analysis&est=2026-10')
    expect(await findLegend('未归属')).toHaveAttribute('aria-pressed', 'false')

    await userEvent.click(await findLegend('乙线'))
    expect(await findLegend('乙线')).toHaveAttribute('aria-pressed', 'true')
    // 单独查看时列出这条线的金额，并给出复原的入口
    // 乙线区间合计 300.00，占 30.0%
    expect(await screen.findByText('（30.0%）')).toBeInTheDocument()
    await userEvent.click(screen.getByRole('button', { name: '查看全部业务线' }))
    expect(await findLegend('乙线')).toHaveAttribute('aria-pressed', 'false')
  })

  it('未归属已按配置并入某条业务线时如实标明，构成图也不重复画', async () => {
    const points = [ALLOCATION.points[0], { ...ALLOCATION.points[1], total: 400 }]
    stubApi({ '/api/bills/allocation': { ...ALLOCATION, points, unmatched_into: '公共' } })
    page('/cost?view=analysis&est=2026-10')
    // 拆分表下方的口径说明写明这笔钱去了哪里
    expect(await screen.findByText(/已按配置计入「\s*公共」/)).toBeInTheDocument()
    await findLegend('乙线')
    expect(legends('未归属')).toHaveLength(0)
    // 图上不另画，改在并入的那条线的图例上注明
    expect(await findLegend('公共')).toHaveTextContent('含未归属 100')
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
    // 配了 goscan 就指向「同步账单」
    expect(screen.getByRole('alert')).toHaveTextContent('同步账单')
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
    expect(within(await screen.findByRole('region', { name: '按产品' })).getByText('云服务器 ECS')).toBeInTheDocument()
  })

  it('没部署 goscan 就只给一句原因', async () => {
    stubApi({ '/api/meta': { ...META, bills: null, bills_note: 'logs.volcengine_bill 不存在' } })
    page()
    expect(await screen.findByText('费用页未启用')).toBeInTheDocument()
    expect(await screen.findByText(/volcengine_bill 不存在/)).toBeInTheDocument()
  })
})
