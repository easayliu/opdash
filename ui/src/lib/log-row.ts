import { useMemo } from 'react'
import type { LogRow } from '@/api/types'

/**
 * 一行日志的身份：内容拼出来的。跟随按它去重，上下文视图按它认锚点行，表格按它做渲染 key。
 * 后端跟随流里的指纹（src/api/tail.rs 的 `fingerprint`）用的是同一组字段。
 */
export function rowKey(r: LogRow): string {
  return `${r.ts_ms}|${r.host}|${r.file}|${r.thread}|${r.logger}|${r.message}`
}

/**
 * 渲染用的 key。同一毫秒、同一线程打出一模一样内容的行是真会有的，[`rowKey`] 会撞。撞了的话
 * React 的 key 和虚拟列表按 key 存的高度都会串——同一行渲染好几遍、行序错乱。所以重复的加个序号，
 * 唯一的行还是保持内容 key，翻页 / 跟随时不会无谓重挂。
 */
export function useRowKeys(rows: LogRow[]): string[] {
  return useMemo(() => {
    const seen = new Map<string, number>()
    return rows.map((r) => {
      const base = rowKey(r)
      const n = seen.get(base) ?? 0
      seen.set(base, n + 1)
      return n === 0 ? base : `${base}#${n}`
    })
  }, [rows])
}
