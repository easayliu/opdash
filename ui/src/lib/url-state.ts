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
 * 「从哪儿来的」。详情页拿它显示面包屑返回。
 *
 * 链路详情是三条路的共同终点（错误分组、链路检索、日志行的 trace id），写死一个返回目标是错的；
 * 而返回目标不能放 URL 里——它是**这一次导航**的上下文，不是视图状态，放进 URL 会跟着被复制给同事，
 * 别人点了会跳到一个他从没去过的列表。所以走 react-router 的 `state`：跟着这一条历史记录走，
 * 复制 URL 带不出去，刷新也不留。
 *
 * 刚好也给出了正确的显示条件：**直接粘 URL 进来的没有来处，就不显示返回**——总比给一个
 * 猜出来的目标强。
 */
export interface FromState {
  from: { href: string; label: string }
}

/**
 * 页面路径 → 返回按钮上的字。
 *
 * 由路径推导而不是让调用方传：`LogTable` 在日志页和链路详情页里都用，同一个组件传死一个
 * 「日志」，从链路详情点出去的返回就会写着「← 日志」。推导出来的永远是「我现在在哪一页」。
 */
function labelOf(pathname: string): string {
  if (pathname.startsWith('/errors')) return '错误'
  if (pathname.startsWith('/logs')) return '日志'
  if (pathname.startsWith('/metrics')) return '指标'
  if (pathname.startsWith('/cost')) return '费用'
  if (pathname.startsWith('/services')) return '服务'
  if (pathname.startsWith('/traces')) return '链路'
  return '返回'
}

/** 在列表页调用，生成跳详情时要带的 `state`；返回按钮上的字按当前路径推导。 */
export function useFrom(): FromState {
  const { pathname, search } = useLocation()
  return useMemo(() => ({ from: { href: pathname + search, label: labelOf(pathname) } }), [pathname, search])
}

/** 在详情页调用，取出来处；没有（直接粘 URL 进来）返回 null。 */
export function useFromState(): FromState['from'] | null {
  const { state } = useLocation()
  const from = (state as Partial<FromState> | null)?.from
  return from && typeof from.href === 'string' && typeof from.label === 'string' ? from : null
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

/** 刷新要重查的是数据，不包括表结构、登录态和收藏列表。 */
function isDataQuery(key: readonly unknown[]): boolean {
  return key[0] !== 'meta' && key[0] !== 'auth' && key[0] !== 'saved'
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
  // 费用页按账期查（from_period / to_period），顶栏的时间范围在那儿没有意义，别往地址上补
  if (pathname.startsWith('/cost')) return null
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
