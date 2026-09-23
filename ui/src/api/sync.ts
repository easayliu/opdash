/**
 * 账单同步的进度：优先订阅事件流，订阅不上再退回轮询（后端见 src/api/bills.rs 的 `sync_events`）。
 *
 * goscan v0.5 起每变一次（受理、开跑、写完一批、换账期、结束）就推一条，页面不必再每两秒问一次；
 * 更老的 goscan 没有事件流，后端回 404，`EventSource` 遇到非 200 会直接关闭、不再重连，
 * 这里据此改用 [`useBillSyncTask`] 轮询。测试环境（jsdom）和极老的浏览器没有 `EventSource`，同样走轮询。
 */
import { useEffect, useState } from 'react'
import { useBillSyncTask } from './queries'
import type { BillSyncTask } from './types'

export interface SyncTaskState {
  data?: BillSyncTask
  /** 轮询那条路出的错；事件流断线由浏览器自己重连，不在这里报 */
  error?: Error
  /** 正在用事件流（false 表示已退回轮询） */
  live: boolean
}

export function useBillSyncProgress(taskId: string | null): SyncTaskState {
  const [live, setLive] = useState<BillSyncTask | undefined>(undefined)
  const [fallback, setFallback] = useState(typeof EventSource === 'undefined')
  const poll = useBillSyncTask(fallback ? taskId : null)

  useEffect(() => {
    setLive(undefined)
    if (!taskId || fallback) return
    const source = new EventSource(`/api/bills/sync/${encodeURIComponent(taskId)}/events`)
    source.addEventListener('task', (e) => setLive(JSON.parse((e as MessageEvent).data) as BillSyncTask))
    // 任务结束后必须主动关：EventSource 断线会自己重连，不关就会反复连上、拿到已结束的任务、再被关掉
    source.addEventListener('done', () => source.close())
    source.onerror = () => {
      // CLOSED：服务端回了非 200（老版本 goscan 没有事件流，或任务已查不到），浏览器不会再重连。
      // 其余情况是连接断了、浏览器正在重连，重连后第一条就是最新状态，什么都不用做
      if (source.readyState === EventSource.CLOSED) setFallback(true)
    }
    return () => source.close()
  }, [taskId, fallback])

  if (fallback) return { data: poll.data ?? live, error: poll.error ?? undefined, live: false }
  return { data: live, live: true }
}
