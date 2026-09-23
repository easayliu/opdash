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
