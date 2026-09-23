import { afterEach, describe, expect, it, vi } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { Button, Card, Combobox, Hint } from '@/components/ui'

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
  // 拉取账单对话框里出过的事：打开菜单时 scrollIntoView / focus 顺带滚动了对话框本身，
  // 对话框是 overflow-hidden，整块内容被推出可视区，只剩一片空白
  it('打开菜单、上下移动高亮时只滚菜单自己的列表，不去滚动外面的容器', async () => {
    const user = userEvent.setup()
    const scrolled = vi.spyOn(Element.prototype, 'scrollIntoView')
    const focus = vi.spyOn(HTMLElement.prototype, 'focus')
    render(<Combobox value="" options={OPTIONS} onChange={() => {}} placeholder="全部服务" floating />)
    await user.click(screen.getByRole('button'))
    await user.keyboard('{ArrowDown}{ArrowDown}{End}')
    expect(scrolled).not.toHaveBeenCalled()
    // 聚焦搜索框时不许顺带滚动祖先
    const input = screen.getByRole('combobox')
    expect(focus.mock.contexts.some((el, i) => el === input && (focus.mock.calls[i][0] as FocusOptions | undefined)?.preventScroll)).toBe(true)
    scrolled.mockRestore()
    focus.mockRestore()
  })

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

describe('Hint', () => {
  const desktop = window.matchMedia
  afterEach(() => {
    window.matchMedia = desktop
  })

  it('不先悬停、直接点，第一次点击就生效，按钮节点也不会被换掉', async () => {
    const onClick = vi.fn()
    render(
      <Hint text="说明文字" asChild>
        <button type="button" onClick={onClick}>
          拉取账单
        </button>
      </Hint>,
    )
    const before = screen.getByRole('button', { name: '拉取账单' })
    // userEvent.click 在同一瞬间完成移入、按下、抬起、点击——早先的实现会在按下那一刻把按钮
    // 换成一个新节点，这一下点击就落空了。手机上的一次轻触正是这个样子
    await userEvent.click(before)
    expect(onClick).toHaveBeenCalledTimes(1)
    expect(screen.getByRole('button', { name: '拉取账单' })).toBe(before)
  })

  it('鼠标停留一会儿才浮出，移开即收起；子元素自己的处理函数照常执行', async () => {
    const onEnter = vi.fn()
    render(
      <Hint text="说明文字" asChild>
        <button type="button" onPointerEnter={onEnter}>
          拉取账单
        </button>
      </Hint>,
    )
    const button = screen.getByRole('button', { name: '拉取账单' })
    await userEvent.hover(button)
    expect(onEnter).toHaveBeenCalled()
    expect(await screen.findByText('说明文字', {}, { timeout: 3000 })).toBeInTheDocument()
    await userEvent.unhover(button)
    await waitFor(() => expect(screen.queryByText('说明文字')).not.toBeInTheDocument())
    expect(screen.getByRole('button', { name: '拉取账单' })).toBe(button)
  })

  it('触摸设备上点一下开、再点一下关，按钮本身的动作照常执行', async () => {
    window.matchMedia = ((query: string) => ({
      matches: true,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    })) as unknown as typeof window.matchMedia
    const onClick = vi.fn()
    render(
      <Hint text="说明文字" asChild>
        <button type="button" onClick={onClick}>
          拉取账单
        </button>
      </Hint>,
    )
    const button = screen.getByRole('button', { name: '拉取账单' })
    await userEvent.click(button)
    expect(onClick).toHaveBeenCalledTimes(1)
    expect(await screen.findByText('说明文字', {}, { timeout: 3000 })).toBeInTheDocument()
    await userEvent.click(screen.getByRole('button', { name: '拉取账单' }))
    expect(onClick).toHaveBeenCalledTimes(2)
    await waitFor(() => expect(screen.queryByText('说明文字')).not.toBeInTheDocument())
  })
})
