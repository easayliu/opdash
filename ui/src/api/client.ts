/** 和后端 /api 说话的最小封装：拼 query、解 JSON、把 `{error, kind}` 变成异常。 */

export class ApiError extends Error {
  status: number
  kind: string
  clickhouseCode?: number
  constructor(status: number, message: string, kind: string, clickhouseCode?: number) {
    super(message)
    this.status = status
    this.kind = kind
    this.clickhouseCode = clickhouseCode
  }
  /** 值得提示用户「缩小范围」的那几类 */
  get tooHeavy(): boolean {
    return this.kind === 'timeout' || this.kind === 'too_heavy'
  }
}

export type Params = Record<string, string | number | boolean | string[] | null | undefined>

export function buildQuery(params: Params): string {
  const q = new URLSearchParams()
  for (const [k, v] of Object.entries(params)) {
    if (v === null || v === undefined || v === '') continue
    if (Array.isArray(v)) {
      for (const item of v) q.append(k, item)
    } else {
      q.set(k, String(v))
    }
  }
  const s = q.toString()
  return s ? `?${s}` : ''
}

export async function apiGet<T>(path: string, params: Params = {}, signal?: AbortSignal): Promise<T> {
  const res = await fetch(`/api${path}${buildQuery(params)}`, {
    headers: { accept: 'application/json' },
    signal,
  })
  if (res.ok) {
    return (await res.json()) as T
  }
  let message = `${res.status} ${res.statusText}`
  let kind = 'internal'
  let code: number | undefined
  try {
    const body = await res.json()
    if (typeof body?.error === 'string') message = body.error
    if (typeof body?.kind === 'string') kind = body.kind
    if (typeof body?.clickhouse_code === 'number') code = body.clickhouse_code
  } catch {
    // 不是 JSON（比如 401 的纯文本）
    if (res.status === 401) message = '需要登录'
  }
  throw new ApiError(res.status, message, kind, code)
}

/** 下载链接（导出用），浏览器直接打开。 */
export function apiUrl(path: string, params: Params = {}): string {
  return `/api${path}${buildQuery(params)}`
}
