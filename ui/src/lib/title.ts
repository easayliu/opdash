import { useEffect } from 'react'

/** 标签页上的名字。加后缀是为了一排标签页里一眼认出哪几个是 opdash 的 */
const SUFFIX = 'opdash'

/**
 * 设置浏览器标题。
 *
 * 单页应用不做这件事的话，七个页面在标签页、浏览历史、读屏的页面播报里全叫「opdash」：开一排
 * 标签分不清哪个是日志哪个是链路，历史记录里更是一串一模一样的条目。
 *
 * `name` 为空（数据还没回来）时先只挂后缀，别闪一下「undefined · opdash」。
 */
export function usePageTitle(name?: string): void {
  useEffect(() => {
    document.title = name ? `${name} · ${SUFFIX}` : SUFFIX
    return () => {
      document.title = SUFFIX
    }
  }, [name])
}
