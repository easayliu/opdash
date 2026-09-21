import { clsx, type ClassValue } from 'clsx'
import { twMerge } from 'tailwind-merge'

export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs))
}

/** 32 位（trace）或 16 位（span）小写 hex；大写也认，用的时候转小写。 */
export function isHexId(s: string, len: 16 | 32): boolean {
  return new RegExp(`^[0-9a-fA-F]{${len}}$`).test(s.trim())
}

/**
 * 复制到剪贴板。成功返回 true——调用方据此给反馈。
 *
 * 只走 `navigator.clipboard`，和 Kumo 一样。它要求安全上下文，也就是 https 或 localhost：线上
 * 是 https，本机开发是 localhost，两边都满足。失败只往控制台记一行，不在界面上另造一套提示。
 *
 * 唯一够不着的场景是从别的机器用 http 加 IP 访问开发服务器（比如拿手机连 `http://192.168.x.x`
 * 试移动端布局），那时剪贴板 API 根本不存在，复制会静默失效。
 */
export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch (error) {
    console.warn('复制失败', error)
    return false
  }
}

/** 拆成一行一行，第一行和其余（堆栈）分开。 */
export function splitFirstLine(s: string): [string, string] {
  const idx = s.indexOf('\n')
  return idx < 0 ? [s, ''] : [s.slice(0, idx), s.slice(idx + 1)]
}

/** 最近的纵向滚动祖先：日志列表自己不滚，滚的是外面那层 overflow-auto。 */
export function scrollParent(el: HTMLElement | null): HTMLElement | null {
  for (let box = el?.parentElement ?? null; box; box = box.parentElement) {
    if (/(auto|scroll)/.test(getComputedStyle(box).overflowY)) return box
  }
  return null
}
