/**
 * 收藏的查询在前端这边是什么：一个页面地址（路径 + 查询串）。
 *
 * 页面的全部状态本来就在 URL 里，所以「收藏当前查询」= 把当前地址存起来，「打开一条收藏」=
 * 导航过去。这里负责两件事：把地址里**这一次**的状态（翻页位置、绝对时间窗）去掉、参数排个序，
 * 好和已收藏的比对；以及把一段查询串翻成人能看的几个词，给列表当副标题、给新收藏当默认名字。
 */

import { QUICK_RANGES } from './time'

export interface View {
  path: string
  /** 已归一化：去掉瞬态参数、按名字排序、不带 `?` */
  query: string
}

/** 只有列表 / 看板页能收藏。链路详情是一条具体的 trace，30 天后就没了，不算「查询」 */
const SAVABLE = ['/logs', '/traces', '/errors', '/metrics', '/services']

/**
 * 这一次的状态，不进收藏：翻到第几页、绝对时间窗（`from` / `to`）。相对范围（`range=1h`）留着——
 * 「最近 24 小时的 ERROR」里那个 24 小时是查询的一部分。没有范围参数时打开收藏会补上这个标签页
 * 记住的范围（见 url-state 的 useRangeMemory）。
 */
const TRANSIENT = ['offset', 'from', 'to']

export function savableView(pathname: string, search: string): View | null {
  const ok = SAVABLE.includes(pathname) || (pathname.startsWith('/services/') && pathname.length > '/services/'.length)
  if (!ok) return null
  return { path: pathname, query: normalizeQuery(search) }
}

export function normalizeQuery(search: string): string {
  const p = new URLSearchParams(search)
  for (const k of TRANSIENT) p.delete(k)
  p.sort()
  return p.toString()
}

/** 一条收藏对应的地址 */
export function savedHref(q: { path: string; query: string }): string {
  return q.query ? `${q.path}?${q.query}` : q.path
}

/** 当前视图和一条收藏是不是同一个查询 */
export function sameView(view: View, q: { path: string; query: string }): boolean {
  return view.path === q.path && view.query === normalizeQuery(q.query)
}

const PAGE_LABEL: Record<string, string> = {
  '/logs': '日志',
  '/traces': '链路',
  '/errors': '错误',
  '/metrics': '指标',
  '/services': '服务总览',
}

export function pageLabel(path: string): string {
  if (path.startsWith('/services/')) {
    try {
      return `服务 · ${decodeURIComponent(path.slice('/services/'.length))}`
    } catch {
      return `服务 · ${path.slice('/services/'.length)}`
    }
  }
  return PAGE_LABEL[path] ?? path
}

/** 纯展示的参数，副标题里不提 */
const HIDDEN = new Set(['limit', 'order', 'term', 'g'])

/** 说得更像人话的几个；没列的按 `key=value` 显示 */
const PRETTY: Record<string, (v: string) => string> = {
  q: (v) => `"${v}"`,
  regex: () => '正则',
  error_only: () => '只看错误',
  follow: () => '跟随',
  min_ms: (v) => `≥ ${v} ms`,
  max_ms: (v) => `≤ ${v} ms`,
  range: (v) => `最近 ${QUICK_RANGES.find((r) => r.key === v)?.label ?? v}`,
  view: (v) => (v === 'all' ? '全部指标' : `view=${v}`),
  sort: (v) => (v === 'duration' ? '最慢在前' : `sort=${v}`),
  cmp: (v) => `对比 ${v}`,
  kind: (v) => v,
  level: (v) => v,
  service: (v) => v,
  service_name: (v) => v,
  span_name: (v) => v,
  metric: (v) => v,
  attr: (v) => v,
  namespace: (v) => v,
  pod: (v) => v,
  container: (v) => v,
}

/** 副标题里的先后：先是「查什么」，再是「在哪」，范围放最后 */
const PRIORITY = ['q', 'regex', 'metric', 'service', 'service_name', 'span_name', 'level', 'kind', 'error_only', 'min_ms', 'max_ms', 'attr', 'namespace', 'pod', 'container', 'logger', 'thread']

/** 把查询串翻成几个词，如 `["timeout"", "order", "ERROR", "最近 24 小时"]`。 */
export function describeQuery(query: string): string[] {
  const pairs = [...new URLSearchParams(query)].filter(([k, v]) => !HIDDEN.has(k) && v !== '')
  const rank = (k: string) => {
    if (k === 'range') return PRIORITY.length + 1
    const i = PRIORITY.indexOf(k)
    return i < 0 ? PRIORITY.length : i
  }
  pairs.sort((a, b) => rank(a[0]) - rank(b[0]))
  return pairs.map(([k, v]) => (PRETTY[k] ? PRETTY[k](v) : `${k}=${v}`))
}

/** 新收藏的默认名字：几个词拼起来，没有条件就叫页面名 */
export function suggestName(view: View): string {
  const words = describeQuery(view.query).filter((w) => !w.startsWith('最近 '))
  const s = words.length ? words.join(' · ') : pageLabel(view.path)
  return s.length > 64 ? `${s.slice(0, 63)}…` : s
}
