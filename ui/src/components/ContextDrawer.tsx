import { useState } from 'react'
import { XIcon } from 'lucide-react'
import type { LogRow } from '@/api/types'
import { useLogContext } from '@/api/queries'
import { Button, ErrorBox, Spinner } from '@/components/ui'
import { LogTable, rowKey } from '@/components/LogTable'
import { StatsLine } from '@/components/StatsLine'
import { formatTs } from '@/lib/time'

/** 某一行日志前后的上下文：同一个 host + file（也就是同一个容器的日志流）。 */
export function ContextDrawer({ row, dims, onClose }: { row: LogRow; dims: string[]; onClose: () => void }) {
  const [before, setBefore] = useState(50)
  const [after, setAfter] = useState(50)
  const q = useLogContext({ host: row.host, file: row.file, ts: row.ts_ms, before, after })
  const anchor = rowKey(row)
  const rows = q.data ? [...q.data.before, ...q.data.after] : []
  const hasAnchor = rows.some((r) => rowKey(r) === anchor)

  return (
    <div className="fixed inset-y-0 right-0 z-30 flex w-[min(100vw,64rem)] flex-col border-l border-border bg-card shadow-xl">
      <header className="flex h-14 shrink-0 items-center gap-3 border-b border-border px-4">
        <div className="min-w-0 flex-1">
          <div className="text-base font-semibold">日志上下文</div>
          <div className="mono truncate text-2xs text-muted-fg" title={row.file}>
            {row.host} · {row.file}
          </div>
        </div>
        <StatsLine stats={q.data?.stats} />
        <Button variant="ghost" className="px-2.5" onClick={onClose} title="关闭 (Esc)">
          <XIcon className="size-5" />
        </Button>
      </header>
      <div className="flex shrink-0 items-center justify-between gap-2 border-b border-border px-4 py-2 text-xs text-muted-fg">
        <span>
          锚点 {formatTs(row.ts_ms)}；前后各最多找 {q.data ? Math.round(q.data.window_ms / 60_000) : 60} 分钟
          {q.data && q.data.before.length < before && ' · 往前已到头'}
          {q.data && q.data.after.length < after && ' · 往后已到头'}
          {q.data && !hasAnchor && ' · 锚点行不在结果里（同一毫秒内的行顺序不定）'}
        </span>
        <span className="flex gap-1.5">
          <Button size="xs" onClick={() => setBefore((n) => Math.min(500, n + 100))} disabled={!q.data || q.data.before.length < before}>
            往前再看 100 行
          </Button>
          <Button size="xs" onClick={() => setAfter((n) => Math.min(500, n + 100))} disabled={!q.data || q.data.after.length < after}>
            往后再看 100 行
          </Button>
        </span>
      </div>
      <div className="min-h-0 flex-1 overflow-auto">
        {q.isError && <ErrorBox error={q.error} onRetry={() => q.refetch()} />}
        {q.isPending && (
          <div className="flex justify-center py-10">
            <Spinner />
          </div>
        )}
        {q.data && <LogTable rows={rows} dims={dims} anchorKey={anchor} compact />}
      </div>
    </div>
  )
}
