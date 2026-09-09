import { useEffect, useRef, useState } from 'react'
import { CalendarIcon, ChevronDownIcon, RefreshCwIcon } from 'lucide-react'
import { Button, Input } from '@/components/ui'
import { useTimeRange } from '@/lib/url-state'
import { QUICK_RANGES, fromLocalInputValue, rangeLabel, toLocalInputValue } from '@/lib/time'
import { cn } from '@/lib/utils'

/** 顶栏共享的时间范围：快捷相对范围 + 自定义绝对范围，写进 URL。 */
export function TimeRangePicker({ className }: { className?: string }) {
  const { range, setRange, refresh } = useTimeRange()
  const [open, setOpen] = useState(false)
  const [from, setFrom] = useState(toLocalInputValue(range.fromMs))
  const [to, setTo] = useState(toLocalInputValue(range.toMs))
  const ref = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!open) return
    setFrom(toLocalInputValue(range.fromMs))
    setTo(toLocalInputValue(range.toMs))
    const onClick = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onClick)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onClick)
      document.removeEventListener('keydown', onKey)
    }
  }, [open, range.fromMs, range.toMs])

  const applyAbsolute = () => {
    const f = fromLocalInputValue(from)
    const t = fromLocalInputValue(to)
    if (f === null || t === null || f >= t) return
    setRange({ fromMs: f, toMs: t, relative: null })
    setOpen(false)
  }

  return (
    <div ref={ref} className={cn('relative flex min-w-0 items-center gap-1', className)}>
      <Button onClick={() => setOpen((o) => !o)} aria-expanded={open} className="min-w-0 gap-1.5 px-2.5 md:pl-3">
        <CalendarIcon className="size-4 shrink-0 text-muted-fg" />
        <span className="max-w-[9.5rem] truncate sm:max-w-[16rem] md:max-w-[22rem]">{rangeLabel(range)}</span>
        <ChevronDownIcon className="hidden size-4 shrink-0 text-muted-fg sm:block" />
      </Button>
      <Button variant="ghost" size="md" className="px-2.5" onClick={refresh} title="按当前时间重新查询">
        <RefreshCwIcon className="size-4" />
      </Button>
      {open && (
        // 手机上钉在视口顶部撑满宽度；桌面挂在按钮下面
        <div className="fixed inset-x-3 top-14 z-30 rounded-lg border border-border bg-card p-4 shadow-lg md:absolute md:inset-x-auto md:top-full md:right-0 md:mt-1 md:w-[28rem]">
          <div className="mb-2 text-2xs font-semibold tracking-wide text-muted-fg uppercase">快捷范围</div>
          <div className="grid grid-cols-3 gap-1.5">
            {QUICK_RANGES.map((q) => (
              <Button
                key={q.key}
                size="sm"
                active={range.relative === q.key}
                onClick={() => {
                  setRange({ fromMs: Date.now() - q.ms, toMs: Date.now(), relative: q.key })
                  setOpen(false)
                }}
              >
                最近 {q.label}
              </Button>
            ))}
          </div>
          <div className="mt-3 mb-2 text-2xs font-semibold tracking-wide text-muted-fg uppercase">自定义（本地时间）</div>
          <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
            <Input type="datetime-local" step={1} value={from} onChange={(e) => setFrom(e.target.value)} />
            <span className="hidden text-muted-fg sm:block">~</span>
            <Input type="datetime-local" step={1} value={to} onChange={(e) => setTo(e.target.value)} />
          </div>
          <div className="mt-2 flex justify-end gap-2">
            <Button size="sm" onClick={() => setOpen(false)}>
              取消
            </Button>
            <Button size="sm" variant="primary" onClick={applyAbsolute}>
              应用
            </Button>
          </div>
        </div>
      )}
    </div>
  )
}
