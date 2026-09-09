import { useCallback, useEffect, useMemo, useSyncExternalStore } from 'react'
import { useQueryClient } from '@tanstack/react-query'
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

/**
 * 相对范围（`最近 N 分钟`）解析成绝对毫秒时用的「现在」。全局共享一个锚点，只在重新选范围、
 * 点刷新时推进——不是每次渲染重算：
 *
 * * 翻页、改排序、改每页条数、切页面都会改 URL，`now` 跟着变的话直方图和 facet 的 query key
 *   也跟着变，这些查询和翻页无关却要在 ClickHouse 上重扫一遍全时间范围；
 * * 同一批结果也才来自同一个窗口——窗口边走边移的话，第二页和第一页的边界对不上，行会错位。
 */
let anchorMs = Date.now()
const anchorSubscribers = new Set<() => void>()

function advanceAnchor(): void {
  anchorMs = Date.now()
  for (const notify of [...anchorSubscribers]) notify()
}

function subscribeAnchor(notify: () => void): () => void {
  anchorSubscribers.add(notify)
  return () => {
    anchorSubscribers.delete(notify)
  }
}

const readAnchor = () => anchorMs

/** 刷新要重查的是数据，不包括表结构和登录态。 */
function isDataQuery(key: readonly unknown[]): boolean {
  return key[0] !== 'meta' && key[0] !== 'auth'
}

/** 时间范围（读自 URL）。相对范围按共享锚点解析，见 [`advanceAnchor`]。 */
export function useTimeRange(): { range: Range; setRange: (r: Range) => void; refresh: () => void } {
  const { params, setParams } = useUrlState()
  const queryClient = useQueryClient()
  const now = useSyncExternalStore(subscribeAnchor, readAnchor, readAnchor)
  const range = useMemo(() => parseRange(params, now), [params, now])
  const setRange = useCallback(
    (r: Range) => {
      // 重新选范围就是一次新的查询：相对范围从这一刻起算
      advanceAnchor()
      setParams((prev) => {
        const next = new URLSearchParams(prev)
        writeRange(next, r)
        next.delete('offset')
        return next
      })
    },
    [setParams],
  )
  const relative = range.relative
  const refresh = useCallback(() => {
    // 相对范围：锚点推到现在，窗口右移，query key 跟着变，自然重查
    if (relative) {
      advanceAnchor()
      return
    }
    // 绝对范围：窗口没变，只能让缓存失效
    queryClient.invalidateQueries({ predicate: (q) => isDataQuery(q.queryKey) })
  }, [relative, queryClient])
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
