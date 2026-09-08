import { useCallback, useMemo } from 'react'
import { useSearchParams } from 'react-router'
import { parseRange, writeRange, type Range } from './time'

/**
 * 页面状态全在 URL query 里：筛选条件、时间范围、选中项。链接复制给同事就是同一个视图。
 * `set` 只改给定的键，其它保留；值为 null / '' 时删掉这个键。
 */
export function useUrlState() {
  const [params, setParams] = useSearchParams()
  const set = useCallback(
    (patch: Record<string, string | number | null | undefined>, opts: { replace?: boolean } = {}) => {
      setParams(
        (prev) => {
          const next = new URLSearchParams(prev)
          for (const [k, v] of Object.entries(patch)) {
            if (v === null || v === undefined || v === '') next.delete(k)
            else next.set(k, String(v))
          }
          return next
        },
        { replace: opts.replace ?? false },
      )
    },
    [setParams],
  )
  return { params, set, setParams }
}

/** 时间范围（读自 URL）。相对范围每次渲染都按当前时间重算，所以「刷新」就是重新查最近 N 分钟。 */
export function useTimeRange(): { range: Range; setRange: (r: Range) => void; refresh: () => void } {
  const { params, setParams } = useUrlState()
  // 把 now 固定到参数变化的那一刻，避免同一次渲染里两个 hook 算出不同的 to
  const range = useMemo(() => parseRange(params), [params])
  const setRange = useCallback(
    (r: Range) => {
      setParams((prev) => {
        const next = new URLSearchParams(prev)
        writeRange(next, r)
        next.delete('offset')
        return next
      })
    },
    [setParams],
  )
  // 相对范围：重新 set 一遍同样的参数会触发 useMemo 重算 now
  const refresh = useCallback(() => {
    setParams((prev) => new URLSearchParams(prev), { replace: true })
  }, [setParams])
  return { range, setRange, refresh }
}

/** 逗号分隔的多选值 */
export function splitList(raw: string | null): string[] {
  return (raw ?? '')
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean)
}
