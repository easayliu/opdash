import { describe, expect, it, vi } from 'vitest'
import { render, screen } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { MemoryRouter } from 'react-router'
import App from './App'

/** 后端不在，所有请求回一个空壳：这里验的是外壳能不能起来，不是数据 */
function stubApi(meta: Record<string, unknown> = {}) {
  vi.stubGlobal('fetch', async (input: RequestInfo | URL) => {
    const url = String(input)
    const body = url.includes('/meta')
      ? {
          version: 'test',
          database: 'log',
          timezone: 'Asia/Shanghai',
          now_ms: Date.now(),
          limits: { max_rows: 1000, max_offset: 10000, export_max_rows: 50000, max_trace_spans: 5000, max_range_ms: 0, query_timeout_ms: 30000 },
          server: { version: 'test', timezone: 'Asia/Shanghai' },
          logs: { table: 'logpipe', dimensions: ['service_name'] },
          traces: { table: 'tracepipe', dimensions: [] },
          metrics: null,
          bills: null,
          ...meta,
        }
      : { rows: [], services: [], groups: [], stats: null }
    return new Response(JSON.stringify(body), { status: 200, headers: { 'content-type': 'application/json' } })
  })
}

function shell(path = '/services', meta: Record<string, unknown> = {}) {
  stubApi(meta)
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  return render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={[path]}>
        <App />
      </MemoryRouter>
    </QueryClientProvider>,
  )
}

describe('App 外壳', () => {
  it('顶栏、主内容地标、跳过导航的链接都在', async () => {
    shell()
    expect(screen.getByRole('banner')).toBeInTheDocument()
    expect(screen.getByRole('main')).toBeInTheDocument()
    // 平时看不见，Tab 第一下才冒出来
    expect(screen.getByRole('link', { name: '跳到主内容' })).toHaveAttribute('href', '#main')
  })

  it('页签是导航链接，当前那个报 aria-current', async () => {
    shell('/logs')
    const nav = screen.getByRole('navigation')
    const current = await screen.findByRole('link', { name: /日志/ })
    expect(nav).toContainElement(current)
    expect(current).toHaveAttribute('aria-current', 'page')
  })

  it('没部署 metricpipe 就不显示指标页签', async () => {
    shell()
    await screen.findByRole('link', { name: /服务/ })
    expect(screen.queryByRole('link', { name: /指标/ })).not.toBeInTheDocument()
  })

  it('接了 goscan 才显示费用页签', async () => {
    shell()
    await screen.findByRole('link', { name: /服务/ })
    expect(screen.queryByRole('link', { name: /费用/ })).not.toBeInTheDocument()
    shell('/services', {
      bills: { providers: ['alicloud'], daily_providers: [], volcengine: null, alicloud_monthly: { table: 'alicloud_bill_monthly', columns: [], dimensions: [] }, alicloud_daily: null, dedupe: 'group' },
    })
    expect(await screen.findAllByRole('link', { name: /费用/ })).not.toHaveLength(0)
  })

  it('按路由切开的页面能加载出来（懒加载 + Suspense 这条路是通的）', async () => {
    shell('/logs')
    // 日志页自己的标题是 sr-only 的 h1，画出来就说明那个 chunk 到了
    expect(await screen.findByRole('heading', { level: 1, name: '日志' })).toBeInTheDocument()
  })
})
