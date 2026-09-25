import { Component, type ErrorInfo, type ReactNode } from 'react'
import { RotateCwIcon } from 'lucide-react'
import { Button, EmptyState } from '@/components/ui'

interface Props {
  children: ReactNode
}

interface State {
  error: Error | null
}

/**
 * 接住页面渲染时抛出来的异常。
 *
 * 没有它的时候，任何一处渲染出错——某个字段后端没给、某个数组是空的、某个第三方组件自己炸
 * 了——React 都会把整棵树卸载干净：白屏一片，顶栏和页签也没了，人只能猜着按刷新。而这是个
 * 排障工具，它自己坏掉的时候恰恰是别人最需要它的时候。
 *
 * 放在页面外壳里面、`<Routes>` 外面：崩的只是内容区，顶栏、时间范围、页签都还在，换个页签
 * 就能接着用（外面那层 `m.div` 按 `pathname` 记 key，换页时这个边界跟着重挂，状态自动清）。
 *
 * React 到今天也只有 class 组件能当错误边界，`react-error-boundary` 那个包裹的也是同一个东西，
 * 就这二十行，不值得多一个依赖。
 */
export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null }

  static getDerivedStateFromError(error: Error): State {
    return { error }
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    // 控制台留一份带组件栈的，报错截图里能看出是哪一块
    console.error('页面渲染出错', error, info.componentStack)
  }

  render(): ReactNode {
    const { error } = this.state
    if (!error) return this.props.children
    return (
      <EmptyState
        title="本页无法显示"
        hint={
          <>
            <span className="mono break-all">{error.message || String(error)}</span>
            <br />
            其他页签不受影响。请先重试；若问题反复出现，请附上上述错误信息提交 issue。
          </>
        }
        action={
          <Button onClick={() => this.setState({ error: null })}>
            <RotateCwIcon className="size-4" />
            重试
          </Button>
        }
      />
    )
  }
}
