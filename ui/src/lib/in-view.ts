import { useEffect, useRef, useState } from 'react'

/**
 * 元素进过视口没有。看板一屏放二十个面板，全部一上来就查会有两个问题：后端的查询名额
 * （`--max-concurrent-queries`，默认 16）会被一个人的一次刷新占满，别人排队；集群也白扫了
 * 用户根本没往下翻的那些面板。所以面板滚进视口才发查询——和一般看板的做法一样。
 *
 * 只认「第一次进来」：进过之后就一直算数，滚出去不取消、不重查。
 */
export function useInView<T extends HTMLElement>(rootMargin = '200px'): [React.RefObject<T | null>, boolean] {
  const ref = useRef<T>(null)
  const [seen, setSeen] = useState(false)
  useEffect(() => {
    if (seen) return
    const el = ref.current
    if (!el) return
    // 老浏览器 / jsdom 没有这个 API 的话就当一直可见，别把内容藏没了
    if (typeof IntersectionObserver === 'undefined') {
      setSeen(true)
      return
    }
    const io = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) setSeen(true)
      },
      { rootMargin },
    )
    io.observe(el)
    return () => io.disconnect()
  }, [seen, rootMargin])
  return [ref, seen]
}
