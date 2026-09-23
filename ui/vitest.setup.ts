import '@testing-library/jest-dom/vitest'
import { afterEach, vi } from 'vitest'
import { cleanup, configure } from '@testing-library/react'

afterEach(cleanup)

/**
 * `findBy*` / `waitFor` 默认只等 1 秒。等的多是假接口的一次往返、一次轮询、一个懒加载的 chunk，
 * 本机上几十毫秒就到，但 CI 的机器慢、又和别的任务抢 CPU，偶尔会跨过 1 秒——用例本身没错，
 * 只是等得不够久。放宽到 3 秒：真坏掉的用例照样会失败，只是晚两秒报出来。
 */
configure({ asyncUtilTimeout: 3000 })

/**
 * jsdom 没有的那几个浏览器 API。
 *
 * 界面里到处在用：`useIsMobile` 问 matchMedia，图表和 `useWidth` 用 ResizeObserver，
 * 「滚进视口才查」用 IntersectionObserver，虚拟列表要量元素。缺了会直接抛异常，
 * 补成「桌面宽度、什么都看得见、量出来是 0」——测的是行为和无障碍属性，不是像素。
 */
Object.defineProperty(window, 'matchMedia', {
  writable: true,
  value: (query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    dispatchEvent: () => false,
  }),
})

/**
 * jsdom 里所有元素都是 0×0，而虚拟列表（TanStack Virtual）靠 `offsetHeight` 和
 * ResizeObserver 的 `borderBoxSize` 决定「可视区有多高、每行多高」——量出来是 0 就一行都不画，
 * 测试里只看得到表头。
 *
 * 所以给元素编一套尺寸：写了行内 height 的按它自己的值（测试里的滚动容器、虚拟列表撑滚动条
 * 用的占位行都属于这一类），其余一律当成一行日志的高度。测的是行为和无障碍属性，不是像素，
 * 这套假尺寸只要自洽就够用。
 */
const VIEWPORT_W = 1024
const ROW_H = 34

function sizeOf(el: Element): { width: number; height: number } {
  const h = Number.parseFloat((el as HTMLElement).style?.height ?? '')
  return { width: VIEWPORT_W, height: Number.isFinite(h) && h > 0 ? h : ROW_H }
}

for (const [prop, pick] of [
  ['offsetWidth', 'width'],
  ['clientWidth', 'width'],
  ['offsetHeight', 'height'],
  ['clientHeight', 'height'],
] as const) {
  Object.defineProperty(HTMLElement.prototype, prop, {
    configurable: true,
    get(this: HTMLElement) {
      return sizeOf(this)[pick]
    },
  })
}

Element.prototype.getBoundingClientRect = function () {
  const { width, height } = sizeOf(this)
  return { width, height, top: 0, left: 0, right: width, bottom: height, x: 0, y: 0, toJSON: () => ({}) } as DOMRect
}
Element.prototype.scrollIntoView = () => {}

/**
 * 观察者一挂上就回一次结果，jsdom 里没有任何东西会自己触发。同步回一次给挂载那一轮用，
 * 再异步回一次触发重渲染——虚拟列表是在第二轮才把行画出来的。
 */
class Observer {
  constructor(private cb: (entries: unknown[], obs: unknown) => void) {}
  observe(el: Element) {
    const { width, height } = sizeOf(el)
    const entry = {
      target: el,
      isIntersecting: true,
      contentRect: { width, height, top: 0, left: 0, x: 0, y: 0 },
      borderBoxSize: [{ inlineSize: width, blockSize: height }],
    }
    this.cb([entry], this)
    setTimeout(() => this.cb([entry], this), 0)
  }
  unobserve() {}
  disconnect() {}
  takeRecords() {
    return []
  }
}
vi.stubGlobal('ResizeObserver', Observer)
vi.stubGlobal('IntersectionObserver', Observer)
