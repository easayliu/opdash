import { describe, expect, it } from 'vitest'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { Button, Card, Combobox } from '@/components/ui'

describe('Button', () => {
  it('把 active 报成 aria-pressed', () => {
    render(<Button active>跟随</Button>)
    expect(screen.getByRole('button', { name: '跟随' })).toHaveAttribute('aria-pressed', 'true')
  })

  it('没给 active 的普通按钮不报 pressed', () => {
    render(<Button>查询</Button>)
    expect(screen.getByRole('button', { name: '查询' })).not.toHaveAttribute('aria-pressed')
  })

  it('自己带 aria-expanded 的不再报 pressed——那种按钮的「值」是展开与否', () => {
    render(
      <Button active aria-expanded={false}>
        收藏
      </Button>,
    )
    const btn = screen.getByRole('button', { name: '收藏' })
    expect(btn).toHaveAttribute('aria-expanded', 'false')
    expect(btn).not.toHaveAttribute('aria-pressed')
  })
})

describe('Card', () => {
  it('标题是 h2，卡片拿它当自己的名字', () => {
    render(<Card title="请求量与错误">内容</Card>)
    const heading = screen.getByRole('heading', { level: 2, name: '请求量与错误' })
    expect(screen.getByRole('region', { name: '请求量与错误' })).toContainElement(heading)
  })
})

const OPTIONS = [
  { value: 'a', label: 'order-service' },
  { value: 'b', label: 'pay-service' },
  { value: 'c', label: 'user-service' },
]

describe('Combobox', () => {
  it('End 跳到最后一项，Home 跳回第一项', async () => {
    const user = userEvent.setup()
    render(<Combobox value="" options={OPTIONS} onChange={() => {}} placeholder="全部服务" />)
    await user.click(screen.getByRole('button'))
    const input = screen.getByRole('combobox')
    const options = screen.getAllByRole('option')

    await user.keyboard('{End}')
    expect(input).toHaveAttribute('aria-activedescendant', options[options.length - 1].id)
    await user.keyboard('{Home}')
    expect(input).toHaveAttribute('aria-activedescendant', options[0].id)
  })

  it('Escape 关掉菜单，并把焦点还给触发器', async () => {
    const user = userEvent.setup()
    render(<Combobox value="" options={OPTIONS} onChange={() => {}} placeholder="全部服务" />)
    const trigger = screen.getByRole('button')
    await user.click(trigger)
    expect(screen.getByRole('listbox')).toBeInTheDocument()

    await user.keyboard('{Escape}')
    expect(screen.queryByRole('listbox')).not.toBeInTheDocument()
    expect(trigger).toHaveFocus()
  })

  it('选中一项之后焦点也回到触发器，不会掉到 body 上', async () => {
    const user = userEvent.setup()
    const picked: string[] = []
    render(<Combobox value="" options={OPTIONS} onChange={(v) => picked.push(v)} placeholder="全部服务" />)
    const trigger = screen.getByRole('button')
    await user.click(trigger)
    await user.click(screen.getByRole('option', { name: /pay-service/ }))

    expect(picked).toEqual(['b'])
    expect(trigger).toHaveFocus()
  })
})
