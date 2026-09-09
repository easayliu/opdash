/**
 * 日志跟随：一条 SSE 长连接，服务端按游标推增量（后端见 src/api/tail.rs）。
 *
 * 以前是前端每 5 秒重查一遍整个窗口，延迟高、每次都在库上重扫同一段。现在轮询在服务端，
 * 浏览器这边只负责收行、去重、按时间倒序摆进表格。
 */
import { useEffect, useState } from 'react'
import { apiGet, buildQuery, type Params } from './client'
import type { LogRow, Meta, Stats } from './types'
import { rowKey } from '@/lib/log-row'

/** 页面上最多留多少行，更早的丢掉——跟随不是用来翻历史的（要翻历史就停下来分页） */
export const TAIL_MAX_ROWS = 2000

/** 会话过期时 EventSource 只会闷头重连（拿不到状态码），隔这么久探一次 /api/meta，401 会跳登录 */
const PROBE_EVERY_MS = 30_000

/** 服务端建连时给的一份参数说明（`hello` 事件） */
export interface TailHello {
  cursor_ms: number
  interval_ms: number
  lookback_ms: number
  limit: number
  resumed: boolean
}

export interface TailState {
  rows: LogRow[]
  /** connecting: 还没收到 hello；live: 在推了；reconnecting: 断了，浏览器正在重连；failed: 服务端说别再试了 */
  status: 'connecting' | 'live' | 'reconnecting' | 'failed'
  hello?: TailHello
  /** 最近一次报错：服务端推的查询错误，或者连接本身断了 */
  error?: string
  /** 最近一批增量查询在 ClickHouse 上的开销 */
  stats?: Stats
}

const INITIAL: TailState = { rows: [], status: 'connecting' }

/**
 * 开一条跟随流。`params` 就是检索用的那套筛选条件；`enabled` 关掉时连接立刻断开、行清空。
 *
 * 服务端在一条连接内已经去过重了，这里再按 [`rowKey`] 去一次：断线重连是浏览器自己发起的，
 * 服务端换了个新的游标状态，边界那一毫秒的行会重复推下来。
 */
export function useLogTail(params: Params, enabled: boolean): TailState {
  const [state, setState] = useState<TailState>(INITIAL)
  // 参数对象每次渲染都是新的，用拼好的查询串当依赖，筛选条件没变就不重连
  const query = buildQuery(params)

  useEffect(() => {
    if (!enabled) {
      setState(INITIAL)
      return
    }
    setState(INITIAL)
    const seen = new Set<string>()
    const source = new EventSource(`/api/logs/tail${query}`)
    let lastProbe = 0

    source.addEventListener('hello', (e) => {
      const hello = JSON.parse((e as MessageEvent).data) as TailHello
      setState((s) => ({ ...s, status: 'live', hello, error: undefined }))
    })

    source.addEventListener('rows', (e) => {
      const batch = JSON.parse((e as MessageEvent).data) as { rows: LogRow[]; stats: Stats }
      setState((s) => {
        const fresh = batch.rows.filter((r) => !seen.has(rowKey(r)))
        for (const r of fresh) seen.add(rowKey(r))
        // 回看补上来的行比表头还老，所以整体重排一次，不能只往前面插
        const rows = fresh.length ? [...fresh, ...s.rows].sort((a, b) => b.ts_ms - a.ts_ms).slice(0, TAIL_MAX_ROWS) : s.rows
        // 丢掉的行也从指纹表里去掉，跟一整天不会越攒越多
        if (rows.length >= TAIL_MAX_ROWS) {
          seen.clear()
          for (const r of rows) seen.add(rowKey(r))
        }
        return { ...s, rows, status: 'live', stats: batch.stats, error: undefined }
      })
    })

    // 服务端推的查询错误。事件名不叫 error：EventSource 把连接自身的故障也派发成 error，混在一起分不清
    source.addEventListener('query_error', (e) => {
      const body = JSON.parse((e as MessageEvent).data) as { error: string; kind: string; fatal: boolean }
      if (body.fatal) source.close()
      setState((s) => ({ ...s, error: body.error, status: body.fatal ? 'failed' : s.status }))
    })

    source.onerror = () => {
      if (source.readyState === EventSource.CLOSED) {
        setState((s) => ({ ...s, status: 'failed', error: s.error ?? '跟随连接断了，点一下「跟随」重开' }))
        return
      }
      setState((s) => ({ ...s, status: 'reconnecting' }))
      if (Date.now() - lastProbe > PROBE_EVERY_MS) {
        lastProbe = Date.now()
        void apiGet<Meta>('/meta').catch(() => {
          // 只是探活：401 时 apiGet 自己会跳登录，其它错误交给下一轮重连
        })
      }
    }

    return () => source.close()
  }, [enabled, query])

  return state
}
