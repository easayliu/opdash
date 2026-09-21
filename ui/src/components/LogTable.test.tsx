import { describe, expect, it } from 'vitest'
import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router'
import { LogTable } from '@/components/LogTable'
import type { LogRow } from '@/api/types'

function row(i: number): LogRow {
  return {
    ts_ms: 1_700_000_000_000 + i * 1000,
    level: i % 2 ? 'ERROR' : 'INFO',
    trace_id: '',
    span_id: '',
    thread: 'main',
    logger: 'com.example.Order',
    message: `第 ${i} 条\n堆栈第二行`,
    file: '/var/log/app.log',
    host: 'node-1',
    service_name: 'order-service',
  }
}

const ROWS = Array.from({ length: 30 }, (_, i) => row(i))

function show(rows = ROWS) {
  return render(
    <MemoryRouter>
      {/* 虚拟列表要找一个滚动祖先（scrollParent 看的是 overflowY 的计算值） */}
      <div style={{ overflowY: 'auto', height: 600 }}>
        <LogTable rows={rows} dims={['service_name']} />
      </div>
    </MemoryRouter>,
  )
}

describe('LogTable', () => {
  it('每行的展开是个真按钮，按下去报 aria-expanded 并指向展开出来的那块', async () => {
    const user = userEvent.setup()
    show()
    const toggle = (await screen.findAllByRole('button', { name: '展开这条日志的详情' }))[0]
    expect(toggle).toHaveAttribute('aria-expanded', 'false')

    await user.click(toggle)

    const opened = screen.getByRole('button', { name: '收起这条日志的详情' })
    expect(opened).toHaveAttribute('aria-expanded', 'true')
    const detailId = opened.getAttribute('aria-controls')
    expect(detailId).toBeTruthy()
    // 指过去的那一块真的在页面上，而且装着这条日志的全文
    expect(document.getElementById(detailId!)).toBeTruthy()
    expect(within(document.getElementById(detailId!)!).getByText(/堆栈第二行/)).toBeInTheDocument()
  })

  it('键盘能一路 Tab 到展开按钮并用空格打开（原来整行 onClick 键盘够不着）', async () => {
    const user = userEvent.setup()
    show([row(0)])
    await screen.findByRole('button', { name: '展开这条日志的详情' })
    await user.tab()
    expect(screen.getByRole('button', { name: '展开这条日志的详情' })).toHaveFocus()
    await user.keyboard(' ')
    expect(screen.getByRole('button', { name: '收起这条日志的详情' })).toBeInTheDocument()
  })

  it('报的是全部行数，不是 DOM 里那几行', () => {
    show()
    // 表头占第 1 行，所以是 30 + 1
    expect(screen.getByRole('table')).toHaveAttribute('aria-rowcount', '31')
  })

  it('可排序的列在 th 上报 aria-sort', () => {
    render(
      <MemoryRouter>
        <div style={{ overflowY: 'auto', height: 600 }}>
          <LogTable rows={ROWS} dims={['service_name']} sort={{ key: 'ts_ms', dir: 'desc' }} onSort={() => {}} />
        </div>
      </MemoryRouter>,
    )
    expect(screen.getByRole('columnheader', { name: /时间/ })).toHaveAttribute('aria-sort', 'descending')
    expect(screen.getByRole('columnheader', { name: /级别/ })).toHaveAttribute('aria-sort', 'none')
  })
})
