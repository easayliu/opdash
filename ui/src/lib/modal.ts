import { useEffect, useRef } from 'react'

/** 能接焦点的东西。`:not([tabindex="-1"])` 把只用来接程序化焦点的排掉 */
const FOCUSABLE =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])'

function focusables(box: HTMLElement): HTMLElement[] {
  // offsetParent 为空的是被 display:none 藏起来的，tab 也走不到
  return [...box.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((el) => el.offsetParent !== null || el === document.activeElement)
}

/**
 * 把焦点关在对话框里，关掉时还给原来那个元素，顺带锁住背景滚动。
 *
 * 只写 `role="dialog"` 和 `aria-modal` 是不够的：那两个属性只是告诉读屏「这是个模态」，浏览器
 * 该怎么走 tab 还怎么走，于是 tab 几下焦点就跑到对话框背后的页面上去了——人还在填表单，光标
 * 已经在后面的筛选框里。Base UI 的 Dialog（Kumo 用的那个）默认管这四件事，我们自己写的没有。
 *
 * 用法：把返回的 ref 挂到对话框那层容器上。Escape 各自处理，这里不管。
 */
export function useModal<T extends HTMLElement>(open = true) {
  const box = useRef<T>(null)
  useEffect(() => {
    if (!open) return
    const prev = document.activeElement as HTMLElement | null
    const el = box.current
    // 进来先把焦点收进对话框：有能聚焦的就给第一个，没有就给容器自己
    const first = el ? focusables(el)[0] : null
    ;(first ?? el)?.focus()

    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Tab' || !el) return
      const list = focusables(el)
      if (!list.length) {
        e.preventDefault()
        return
      }
      const head = list[0]
      const tail = list[list.length - 1]
      const now = document.activeElement
      // 走到两端就绕回去；焦点已经在外面（点了背景之类）也拽回来
      if (!el.contains(now)) {
        e.preventDefault()
        ;(e.shiftKey ? tail : head).focus()
      } else if (e.shiftKey && now === head) {
        e.preventDefault()
        tail.focus()
      } else if (!e.shiftKey && now === tail) {
        e.preventDefault()
        head.focus()
      }
    }
    document.addEventListener('keydown', onKey)
    const overflow = document.body.style.overflow
    document.body.style.overflow = 'hidden'
    return () => {
      document.removeEventListener('keydown', onKey)
      document.body.style.overflow = overflow
      // 还回去：不还的话焦点掉到 body 上，接着按 tab 是从页面最顶上重新开始
      prev?.focus?.()
    }
  }, [open])
  return box
}
