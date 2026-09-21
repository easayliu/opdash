import { describe, expect, it, vi } from 'vitest'
import { render, screen } from '@testing-library/react'
import { ErrorBoundary } from '@/components/ErrorBoundary'

function Boom(): never {
  throw new Error('后端少给了一个字段')
}

describe('ErrorBoundary', () => {
  it('接住渲染异常，把错误显示出来而不是整页白掉', () => {
    // React 会把这次异常往 console.error 打一遍，测试里不需要看
    const quiet = vi.spyOn(console, 'error').mockImplementation(() => {})
    render(
      <ErrorBoundary>
        <Boom />
      </ErrorBoundary>,
    )
    expect(screen.getByText('这一页画不出来了')).toBeInTheDocument()
    expect(screen.getByText('后端少给了一个字段')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /重试/ })).toBeInTheDocument()
    quiet.mockRestore()
  })

  it('没出事的时候原样渲染子节点', () => {
    render(
      <ErrorBoundary>
        <p>日志</p>
      </ErrorBoundary>,
    )
    expect(screen.getByText('日志')).toBeInTheDocument()
  })
})
