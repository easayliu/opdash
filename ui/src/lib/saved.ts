/**
 * 收藏的查询在前端这边是什么：一个页面地址（路径 + 查询串）。
 *
 * 页面的全部状态本来就在 URL 里，所以「收藏当前查询」= 把当前地址存起来，「打开一条收藏」=
 * 导航过去。这里负责两件事：把地址里不属于查询的部分（翻页位置、时间范围）去掉、参数排个序，
 * 好和已收藏的比对；以及把一段查询串翻成人能看的几个词，给列表当副标题、给新收藏当默认名字。
 */

export interface View {
  path: string
  /** 已归一化：去掉瞬态参数、按名字排序、不带 `?` */
  query: string
}

/** 只有列表 / 看板页能收藏。链路详情是一条具体的 trace，30 天后就没了，不算「查询」 */
const SAVABLE = ['/logs', '/traces', '/errors', '/metrics', '/services']

/**
 * 不进收藏的参数：翻到第几页，以及**时间范围**（`range` / `from` / `to`）。
 *
 * 收藏的是查询条件，时间范围跟顶栏走：顶栏的范围是跨页面共享、跟着人走的（url-state 的
 * useRangeMemory 会把它补到每个没带范围的地址上），打开一条收藏用的就是顶栏当前的范围。
 * 之前把 `range` 留在收藏里，打开之后 URL 被补上的范围和收藏里的对不上，书签就不亮、还会让人
 * 再存一条重复的；绝对时间窗更是一段过去的时间，下次打开多半也不是想看那一段。
 */
const TRANSIENT = ['offset', 'from', 'to', 'range']

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

/** 一条收藏对应的地址。查询串再归一化一遍：早先存进去的可能带着 `range`，打开时也不该改顶栏的范围 */
export function savedHref(q: { path: string; query: string }): string {
  const query = normalizeQuery(q.query)
  return query ? `${q.path}?${query}` : q.path
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

/** 副标题里的先后：先是「查什么」，再是「在哪」 */
const PRIORITY = ['q', 'regex', 'metric', 'service', 'service_name', 'span_name', 'level', 'kind', 'error_only', 'min_ms', 'max_ms', 'attr', 'namespace', 'pod', 'container', 'logger', 'thread']

/** 把查询串翻成几个词，如 `["timeout"", "order", "ERROR"]`。时间范围不算查询的一部分，不提 */
export function describeQuery(query: string): string[] {
  const pairs = [...new URLSearchParams(normalizeQuery(query))].filter(([k, v]) => !HIDDEN.has(k) && v !== '')
  const rank = (k: string) => {
    const i = PRIORITY.indexOf(k)
    return i < 0 ? PRIORITY.length : i
  }
  pairs.sort((a, b) => rank(a[0]) - rank(b[0]))
  return pairs.map(([k, v]) => (PRETTY[k] ? PRETTY[k](v) : `${k}=${v}`))
}

/** 新收藏的默认名字：几个词拼起来，没有条件就叫页面名 */
export function suggestName(view: View): string {
  const words = describeQuery(view.query)
  const s = words.length ? words.join(' · ') : pageLabel(view.path)
  return s.length > 64 ? `${s.slice(0, 63)}…` : s
}
