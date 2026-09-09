import { useSyncExternalStore } from 'react'

/** 和 Tailwind 的 `md` 断点一致：窄于 768px 当手机处理，表格换成卡片、侧栏换成整屏。 */
const MOBILE_QUERY = '(max-width: 767.98px)'

function subscribe(cb: () => void): () => void {
  const mq = window.matchMedia(MOBILE_QUERY)
  mq.addEventListener('change', cb)
  return () => mq.removeEventListener('change', cb)
}

export function useIsMobile(): boolean {
  return useSyncExternalStore(subscribe, () => window.matchMedia(MOBILE_QUERY).matches, () => false)
}
