import { describe, expect, it, vi } from 'vitest'
import { act, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { BillSyncDialog, Elapsed } from './BillSyncDialog'

const STARTED = { task_id: 't-1', provider: 'alicloud', from: '2026-04', to: '2026-09', message: 'Sync triggered' }

/**
 * 触发返回 task id，之后的轮询按 `task` 给状态。
 *
 * 对话框的壳 `ModalPanel` 是 `lazy` 进来的（它不在首屏），首次渲染时页面上什么都没有，
 * 因此每个用例的第一次取元素都得用 `findBy*` 等它这一帧。
 */
function stubApi(task: Record<string, unknown>) {
  vi.stubGlobal('fetch', async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input), 'http://localhost')
    const body = init?.method === 'POST' ? STARTED : url.pathname.startsWith('/api/bills/sync/') ? task : {}
    return new Response(JSON.stringify(body), { status: 200, headers: { 'content-type': 'application/json' } })
  })
}

function dialog() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  return render(
    <QueryClientProvider client={client}>
      <BillSyncDialog providers={['alicloud', 'volcengine']} from="2026-04" to="2026-09" onClose={() => {}} />
    </QueryClientProvider>,
  )
}

describe('拉取账单对话框', () => {
  it('goscan 报了进度就画确定进度条，并说明这一趟写哪张表', async () => {
    // 把「此刻」钉住：已用时是渲染时现算的，CI 的机器慢，打桩到断言之间走过的一两秒
    // 会让「1 分 15 秒」变成「1 分 17 秒」
    const NOW = Date.parse('2026-09-23T08:00:00Z')
    const now = vi.spyOn(Date, 'now').mockReturnValue(NOW)
    stubApi({
      id: 't-1',
      status: 'running',
      provider: 'alicloud',
      done: false,
      ok: false,
      records: 0,
      fetched: 0,
      message: '',
      started_at: new Date(NOW - 75_000).toISOString(),
      progress: { period: '2026-06', granularity: 'daily', periods_done: 2, periods_total: 6 },
    })
    dialog()
    await userEvent.click(await screen.findByRole('button', { name: '开始拉取' }))

    const bar = await screen.findByRole('progressbar', { name: '账单拉取进度' })
    expect(bar).toHaveAttribute('aria-valuenow', '2')
    expect(bar).toHaveAttribute('aria-valuemax', '6')
    expect(await screen.findByText(/2 \/ 6 趟 · 正在拉 2026-06 日度/)).toBeInTheDocument()
    // 已用时按服务端给的开始时间算
    expect(await screen.findByText(/已用 1 分 15 秒/)).toBeInTheDocument()
    now.mockRestore()
  })

  it('goscan 不报进度时退回不确定进度条，不编百分比', async () => {
    stubApi({ id: 't-1', status: 'running', provider: 'alicloud', done: false, ok: false, records: 0, fetched: 0, message: '' })
    dialog()
    await userEvent.click(await screen.findByRole('button', { name: '开始拉取' }))

    const bar = await screen.findByRole('progressbar', { name: '账单拉取进度' })
    expect(bar).not.toHaveAttribute('aria-valuenow')
    expect(bar).toHaveAttribute('aria-valuetext', '正在拉取')
  })

  // 一个账期一种粒度算一趟：选了「月度 + 日度」要等的是两倍的时间，填完账期就该看得见
  it('选两种粒度时把趟数一并告知', async () => {
    stubApi({ id: 't-1', status: 'running', provider: 'alicloud', done: false, ok: false, records: 0, fetched: 0, message: '' })
    dialog()

    expect(await screen.findByText('共 6 个账期、12 趟')).toBeInTheDocument()

    // 只要一种粒度就是一个账期一趟，这时候再报趟数是噪声
    await userEvent.selectOptions(screen.getByRole('combobox', { name: '粒度' }), 'monthly')
    expect(screen.getByText('共 6 个账期')).toBeInTheDocument()

    // 火山只有一张表，粒度这一项根本不出现
    await userEvent.selectOptions(screen.getByRole('combobox', { name: '云' }), 'volcengine')
    expect(screen.queryByRole('combobox', { name: '粒度' })).not.toBeInTheDocument()
    expect(screen.getByText('共 6 个账期')).toBeInTheDocument()
  })

  it('跑完了报条数', async () => {
    stubApi({
      id: 't-1',
      status: 'completed',
      provider: 'alicloud',
      done: true,
      ok: true,
      records: 1200,
      fetched: 1200,
      message: 'ok',
      started_at: new Date(Date.now() - 30_000).toISOString(),
      ended_at: new Date().toISOString(),
      progress: { period: '', periods_done: 6, periods_total: 6 },
    })
    dialog()
    await userEvent.click(await screen.findByRole('button', { name: '开始拉取' }))

    expect(await screen.findByText('同步完成')).toBeInTheDocument()
    expect(await screen.findByText(/写入 1,200 条/)).toBeInTheDocument()
    const bar = await screen.findByRole('progressbar', { name: '账单拉取进度' })
    expect(bar).toHaveAttribute('aria-valuenow', '6')
  })

  it('已用时每秒自己走，不等轮询；任务结束后停表', () => {
    vi.useFakeTimers()
    vi.setSystemTime(Date.parse('2026-09-23T08:00:00Z'))
    const { rerender } = render(<Elapsed startedAt="2026-09-23T08:00:00Z" />)
    expect(screen.getByText('已用 0 秒')).toBeInTheDocument()
    act(() => vi.advanceTimersByTime(3000))
    expect(screen.getByText('已用 3 秒')).toBeInTheDocument()
    // 结束之后显示服务端给的定值，钟也不再走
    rerender(<Elapsed startedAt="2026-09-23T08:00:00Z" endedAt="2026-09-23T08:01:05Z" />)
    act(() => vi.advanceTimersByTime(5000))
    expect(screen.getByText('已用 1 分 5 秒')).toBeInTheDocument()
    vi.useRealTimers()
  })

  it('goscan 报来的零值结束时间（0001-01-01）当作尚未结束，钟照常走', () => {
    vi.useFakeTimers()
    vi.setSystemTime(Date.parse('2026-09-23T08:00:10Z'))
    render(<Elapsed startedAt="2026-09-23T08:00:00Z" endedAt="0001-01-01T00:00:00Z" />)
    expect(screen.getByText('已用 10 秒')).toBeInTheDocument()
    act(() => vi.advanceTimersByTime(2000))
    expect(screen.getByText('已用 12 秒')).toBeInTheDocument()
    vi.useRealTimers()
  })
})

/** 按「方法 + 路径」分派的桩；`state.task` 可在用例中途改，模拟 goscan 那边的进展 */
function routedApi(routes: {
  running?: () => unknown
  task?: () => unknown
  post?: () => Response
  del?: () => Response
}) {
  const calls: string[] = []
  vi.stubGlobal('fetch', async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input), 'http://localhost')
    const method = init?.method ?? 'GET'
    calls.push(`${method} ${url.pathname}`)
    const json = (body: unknown, status = 200) =>
      new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })
    if (method === 'POST') return routes.post?.() ?? json(STARTED)
    if (method === 'DELETE') return routes.del?.() ?? json({ task_id: 't-1', status: 'cancelling', message: '' }, 202)
    if (url.pathname === '/api/bills/sync/running') return json(routes.running?.() ?? { task: null })
    if (url.pathname.startsWith('/api/bills/sync/')) return json(routes.task?.() ?? {})
    return json({})
  })
  return calls
}

const RUNNING = {
  id: 't-9',
  status: 'running',
  provider: 'alicloud',
  done: false,
  ok: false,
  records: 0,
  fetched: 0,
  message: '',
  cancel_requested: false,
  from: '2026-04',
  to: '2026-09',
  progress: { period: '2026-06', granularity: 'daily', periods_done: 4, periods_total: 12, records: 3400, records_total: 9120 },
}

describe('拉取账单对话框 · goscan v0.5 的新接口', () => {
  it('打开时这朵云已有同步在跑（比如 cron 起的），直接接上它的进度', async () => {
    routedApi({ running: () => ({ task: RUNNING }), task: () => RUNNING })
    dialog()
    expect(await screen.findByText(/已有一个同步正在进行/)).toBeInTheDocument()
    expect(await screen.findByText(/4 \/ 12 趟 · 正在拉 2026-06 日度/)).toBeInTheDocument()
    // 一趟要跑好几分钟，本趟的行数让人看得出还在动
    expect(await screen.findByText(/本趟已写入 3,400 \/ 9,120 行/)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: '停止同步' })).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: '开始拉取' })).not.toBeInTheDocument()
  })

  it('停止：先显示「正在停止」，停下后列出没跑的那几趟', async () => {
    let task: Record<string, unknown> = RUNNING
    const calls = routedApi({ running: () => ({ task: RUNNING }), task: () => task })
    dialog()
    await userEvent.click(await screen.findByRole('button', { name: '停止同步' }))
    expect(calls).toContain('DELETE /api/bills/sync/t-9')
    expect(await screen.findByRole('button', { name: '正在停止…' })).toBeDisabled()
    expect(screen.getByText(/会把当前这一趟写完再停/)).toBeInTheDocument()

    // goscan 写完手上那一趟，停了
    task = {
      ...RUNNING,
      status: 'cancelled',
      done: true,
      cancel_requested: true,
      records: 109440,
      not_run: ['2026-09 monthly', '2026-09 daily'],
    }
    expect(await screen.findByText('已停止', {}, { timeout: 4000 })).toBeInTheDocument()
    expect(screen.getByText(/未执行：2026-09 月度、2026-09 日度，这些账期的数据保持原样/)).toBeInTheDocument()
    expect(screen.getByText(/停止前已写入 109,440 条/)).toBeInTheDocument()
    // 停下不算失败，不能标红说「同步失败」
    expect(screen.queryByText('同步失败')).not.toBeInTheDocument()
  })

  it('点「开始拉取」撞上 409（这朵云已有同步），接上那个任务而不是只报错', async () => {
    let running: unknown = { task: null }
    routedApi({
      running: () => running,
      task: () => RUNNING,
      post: () => {
        running = { task: RUNNING }
        return new Response(JSON.stringify({ error: '已有同步任务正在执行', kind: 'busy' }), {
          status: 409,
          headers: { 'content-type': 'application/json' },
        })
      },
    })
    dialog()
    await userEvent.click(await screen.findByRole('button', { name: '开始拉取' }))
    expect(await screen.findByText(/已有一个同步正在进行/)).toBeInTheDocument()
    expect(screen.queryByText(/已有同步任务正在执行/)).not.toBeInTheDocument()
  })
})

/** 一个够用的假 EventSource：记下实例，用例里手动派发事件 */
class FakeEventSource {
  static CONNECTING = 0
  static OPEN = 1
  static CLOSED = 2
  static last: FakeEventSource | null = null
  readyState = FakeEventSource.OPEN
  onerror: (() => void) | null = null
  private listeners: Record<string, ((e: MessageEvent) => void)[]> = {}
  constructor(public url: string) {
    FakeEventSource.last = this
  }
  addEventListener(name: string, fn: (e: MessageEvent) => void) {
    ;(this.listeners[name] ??= []).push(fn)
  }
  emit(name: string, data: unknown) {
    for (const fn of this.listeners[name] ?? []) fn(new MessageEvent(name, { data: JSON.stringify(data) }))
  }
  close() {
    this.readyState = FakeEventSource.CLOSED
  }
}

describe('同步进度走事件流', () => {
  it('订阅事件流，收到 done 就主动关闭；订阅不上（非 200）时退回轮询', async () => {
    vi.stubGlobal('EventSource', FakeEventSource)
    const calls = routedApi({ running: () => ({ task: RUNNING }), task: () => ({ ...RUNNING, progress: { ...RUNNING.progress, periods_done: 7 } }) })
    dialog()
    await screen.findByText(/已有一个同步正在进行/)
    const es = FakeEventSource.last!
    expect(es.url).toBe('/api/bills/sync/t-9/events')
    act(() => es.emit('task', RUNNING))
    expect(await screen.findByText(/4 \/ 12 趟/)).toBeInTheDocument()
    // 有事件流就不轮询
    expect(calls.filter((c) => c === 'GET /api/bills/sync/t-9')).toHaveLength(0)

    // 老版本 goscan 没有事件流：服务端回 404，浏览器把连接关掉、不再重连 → 改用轮询
    act(() => {
      es.readyState = FakeEventSource.CLOSED
      es.onerror?.()
    })
    expect(await screen.findByText(/7 \/ 12 趟/)).toBeInTheDocument()
    expect(calls).toContain('GET /api/bills/sync/t-9')
    // 只还原 EventSource：unstubAllGlobals 会把 vitest.setup.ts 里打的 IntersectionObserver 等桩一并撤掉
    vi.stubGlobal('EventSource', undefined)
  })

  it('收到 done 后关闭连接，免得 EventSource 自己反复重连', async () => {
    vi.stubGlobal('EventSource', FakeEventSource)
    routedApi({ running: () => ({ task: RUNNING }) })
    dialog()
    await screen.findByText(/已有一个同步正在进行/)
    const es = FakeEventSource.last!
    act(() => {
      es.emit('task', { ...RUNNING, status: 'completed', done: true, ok: true, records: 120, fetched: 120 })
      es.emit('done', {})
    })
    expect(await screen.findByText('同步完成')).toBeInTheDocument()
    expect(es.readyState).toBe(FakeEventSource.CLOSED)
    // 只还原 EventSource：unstubAllGlobals 会把 vitest.setup.ts 里打的 IntersectionObserver 等桩一并撤掉
    vi.stubGlobal('EventSource', undefined)
  })
})
