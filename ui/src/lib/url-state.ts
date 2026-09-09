import { useCallback, useEffect, useMemo } from 'react'
import { useLocation, useSearchParams } from 'react-router'
import { hasRangeParams, parseRange, writeRange, type Range } from './time'

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

const RANGE_MEMORY_KEY = 'opdash:range'
const RANGE_KEYS = ['range', 'from', 'to'] as const

function readRangeMemory(): string | null {
  try {
    return sessionStorage.getItem(RANGE_MEMORY_KEY) || null
  } catch {
    // 隐私模式 / 禁了存储，就退回默认范围
    return null
  }
}

/**
 * 时间范围跟着人走：URL 上带了就记下来，跳到没带范围的地址（顶栏导航、trace / 服务详情、快速跳转）
 * 时补回去，免得一换页就掉回默认 1 小时。记在 sessionStorage 里，每个标签页可以各盯各的时间窗。
 * 返回需要补参数时的目标地址，不需要补就是 null——调用方据此先 `<Navigate replace>` 再渲染页面，
 * 这样页面第一次渲染拿到的就是正确的范围，不会先用默认范围白查一次。
 */
export function useRangeMemory(): string | null {
  const { pathname, search } = useLocation()
  const params = useMemo(() => new URLSearchParams(search), [search])
  const explicit = hasRangeParams(params)

  useEffect(() => {
    if (!explicit) return
    const keep = new URLSearchParams()
    for (const k of RANGE_KEYS) {
      const v = params.get(k)
      if (v !== null) keep.set(k, v)
    }
    try {
      sessionStorage.setItem(RANGE_MEMORY_KEY, keep.toString())
    } catch {
      // 存不下就只在当前页面有效
    }
  }, [explicit, params])

  if (explicit) return null
  const remembered = readRangeMemory()
  if (!remembered) return null
  const next = new URLSearchParams(search)
  for (const [k, v] of new URLSearchParams(remembered)) next.set(k, v)
  return `${pathname}?${next}`
}

/** 逗号分隔的多选值 */
export function splitList(raw: string | null): string[] {
  return (raw ?? '')
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean)
}
