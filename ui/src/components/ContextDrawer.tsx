import { Suspense, useId, useState } from 'react'
import { XIcon } from 'lucide-react'
import type { LogRow } from '@/api/types'
import { useLogContext } from '@/api/queries'
import { Button, ErrorBox, Hint, ModalPanel, Spinner } from '@/components/ui'
import { LogTable } from '@/components/LogTable'
import { StatsLine } from '@/components/StatsLine'
import { rowKey } from '@/lib/log-row'
import { formatTs } from '@/lib/time'

/** 某一行日志前后的上下文：同一个 host + file（也就是同一个容器的日志流）。 */
export function ContextDrawer({ row, dims, onClose }: { row: LogRow; dims: string[]; onClose: () => void }) {
  const [before, setBefore] = useState(50)
  const [after, setAfter] = useState(50)
  const q = useLogContext({ host: row.host, file: row.file, ts: row.ts_ms, before, after })
  const anchor = rowKey(row)
  const rows = q.data ? [...q.data.before, ...q.data.after] : []
  const hasAnchor = rows.some((r) => rowKey(r) === anchor)
  const titleId = useId()

  return (
    /*
     * 这是个模态抽屉，原来只是一块 `fixed` 的面板：没有 dialog 语义、焦点不进来也出不去，
     * 打开之后光标还留在背后那张表里，按几下 Tab 就走进了被盖住的页面。
     *
     * 交给 Base UI 的 Dialog（见 ModalPanel）：焦点关在抽屉里、背景不可点、Escape 关、
     * 点遮罩关、关掉之后焦点还回刚才那一行的按钮。遮罩压暗一点点就行——人还要拿它和
     * 背后那张表对照着看。
     */
    <Suspense fallback={null}>
      <ModalPanel
        open
        onOpenChange={(next) => !next && onClose()}
        labelledBy={titleId}
        backdropClassName="fixed inset-0 z-30 bg-black/20"
        className="fixed inset-y-0 right-0 z-40 flex w-[min(100vw,64rem)] flex-col border-l border-border bg-card shadow-xl"
      >
        <header className="flex h-14 shrink-0 items-center gap-3 border-b border-border px-3 md:px-4">
          <div className="min-w-0 flex-1">
            <h2 id={titleId} className="text-base font-semibold">
              日志上下文
            </h2>
            <Hint text={row.file}>
              <div className="mono truncate text-2xs text-muted-fg">
                {row.host} · {row.file}
              </div>
            </Hint>
          </div>
          <StatsLine stats={q.data?.stats} className="hidden text-2xs text-muted-fg md:inline" />
          <Button variant="ghost" className="px-2.5" onClick={onClose} title="关闭 (Esc)">
            <XIcon className="size-5" />
          </Button>
        </header>
        <div className="flex shrink-0 flex-wrap items-center justify-between gap-2 border-b border-border px-3 py-2 text-xs text-muted-fg md:px-4">
          <span className="min-w-0">
            锚点 {formatTs(row.ts_ms)}；前后各查找至多 {q.data ? Math.round(q.data.window_ms / 60_000) : 60} 分钟
            {q.data && q.data.before.length < before && ' · 之前没有更多日志'}
            {q.data && q.data.after.length < after && ' · 之后没有更多日志'}
            {q.data && !hasAnchor && ' · 锚点行不在结果中（同一毫秒内的日志顺序不固定）'}
          </span>
          <span className="flex shrink-0 gap-1.5">
            <Button size="xs" onClick={() => setBefore((n) => Math.min(500, n + 100))} disabled={!q.data || q.data.before.length < before}>
              向前 +100
            </Button>
            <Button size="xs" onClick={() => setAfter((n) => Math.min(500, n + 100))} disabled={!q.data || q.data.after.length < after}>
              向后 +100
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
      </ModalPanel>
    </Suspense>
  )
}
