import { useEffect, useState, type FormEvent } from 'react'
import { HelpCircleIcon, XIcon } from 'lucide-react'
import { useLogFacets } from '@/api/queries'
import { Button, Input, Kbd, Select } from '@/components/ui'
import type { Params } from '@/api/client'
import { cn } from '@/lib/utils'

export const LEVELS = ['ERROR', 'WARN', 'INFO', 'DEBUG', 'TRACE']

export interface LogFilterState {
  q: string
  regex: boolean
  levels: string[]
  logger: string
  thread: string
  trace_id: string
  span_id: string
  dims: Record<string, string[]>
}

interface Props {
  state: LogFilterState
  dims: string[]
  /** facet 查询用的时间范围参数 */
  rangeParams: Params
  onChange: (next: LogFilterState) => void
}

/** 日志筛选栏：一行关键字 + 级别 + 动态维度下拉。回车 / 点查询才生效。 */
export function LogFilters({ state, dims, rangeParams, onChange }: Props) {
  const [q, setQ] = useState(state.q)
  const [logger, setLogger] = useState(state.logger)
  const [thread, setThread] = useState(state.thread)
  const [help, setHelp] = useState(false)
  useEffect(() => setQ(state.q), [state.q])
  useEffect(() => setLogger(state.logger), [state.logger])
  useEffect(() => setThread(state.thread), [state.thread])

  const submit = (e?: FormEvent) => {
    e?.preventDefault()
    onChange({ ...state, q: q.trim(), logger: logger.trim(), thread: thread.trim() })
  }
  const toggleLevel = (l: string) => {
    const levels = state.levels.includes(l) ? state.levels.filter((x) => x !== l) : [...state.levels, l]
    onChange({ ...state, levels })
  }
  const setDim = (dim: string, value: string) => {
    const dimsNext = { ...state.dims }
    if (value) dimsNext[dim] = [value]
    else delete dimsNext[dim]
    onChange({ ...state, dims: dimsNext })
  }
  const dimOrder = ['service_name', 'namespace', 'pod', 'container', 'host', 'stream', 'cluster']
  const shownDims = [...dims.filter((d) => dimOrder.includes(d)).sort((a, b) => dimOrder.indexOf(a) - dimOrder.indexOf(b)), ...dims.filter((d) => !dimOrder.includes(d))]
  const activeIds = state.trace_id || state.span_id

  return (
    <form onSubmit={submit} className="flex flex-col gap-2 border-b border-border bg-card px-3 py-2">
      <div className="flex items-center gap-2">
        <div className="relative flex-1">
          <Input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder={state.regex ? '正则（RE2）：如 orderId=\\d+ .*timeout' : '关键字：多个词都要命中；-词 排除；"带 空格" 整体匹配'}
            className="mono pr-8"
            aria-label="日志关键字"
          />
          <button
            type="button"
            className="absolute top-1/2 right-2 -translate-y-1/2 text-muted-fg hover:text-fg"
            onClick={() => setHelp((h) => !h)}
            title="语法说明"
          >
            <HelpCircleIcon className="size-3.5" />
          </button>
        </div>
        <Button
          size="md"
          active={state.regex}
          onClick={() => onChange({ ...state, regex: !state.regex, q: q.trim() })}
          title="按正则匹配 message（ClickHouse match，RE2 语法，区分大小写；加 (?i) 忽略大小写）"
        >
          .*
        </Button>
        <Button type="submit" variant="primary">
          查询
        </Button>
      </div>
      {help && (
        <div className="rounded-md border border-border bg-muted/50 px-3 py-2 text-2xs leading-5 text-muted-fg">
          关键字模式：空格分隔的词全部要命中（不分大小写）；<Kbd>-词</Kbd> 排除；<Kbd>"带 空格"</Kbd> 当一个整体。正则模式（<Kbd>.*</Kbd>）：ClickHouse{' '}
          <code>match()</code>，RE2 语法，区分大小写，<Kbd>(?i)</Kbd> 忽略大小写。message 没有索引，扫的是时间范围内的全部日志——先选服务 / pod、缩小时间范围，查询更快。
        </div>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <div className="flex items-center gap-0.5 rounded-md border border-border p-0.5">
          {LEVELS.map((l) => (
            <button
              key={l}
              type="button"
              onClick={() => toggleLevel(l)}
              className={cn(
                'h-6 rounded-sm px-2 text-2xs font-semibold text-muted-fg hover:bg-muted',
                state.levels.includes(l) && 'bg-accent-soft text-accent',
              )}
            >
              {l}
            </button>
          ))}
        </div>
        {shownDims.map((dim) => (
          <DimSelect key={dim} dim={dim} value={state.dims[dim]?.[0] ?? ''} rangeParams={rangeParams} onChange={(v) => setDim(dim, v)} />
        ))}
        <Input value={logger} onChange={(e) => setLogger(e.target.value)} placeholder="logger 包含…" className="w-40" aria-label="logger" />
        <Input value={thread} onChange={(e) => setThread(e.target.value)} placeholder="thread 包含…" className="w-36" aria-label="thread" />
        {activeIds && (
          <span className="inline-flex items-center gap-1 rounded-md bg-accent-soft px-2 py-1 text-2xs text-accent">
            {state.trace_id ? `trace ${state.trace_id.slice(0, 12)}…` : `span ${state.span_id}`}（已忽略时间范围）
            <button type="button" onClick={() => onChange({ ...state, trace_id: '', span_id: '' })} title="去掉">
              <XIcon className="size-3" />
            </button>
          </span>
        )}
        {(state.levels.length > 0 || Object.keys(state.dims).length > 0 || state.q || state.logger || state.thread) && (
          <Button
            variant="ghost"
            size="sm"
            onClick={() => onChange({ q: '', regex: state.regex, levels: [], logger: '', thread: '', trace_id: state.trace_id, span_id: state.span_id, dims: {} })}
          >
            清空条件
          </Button>
        )}
      </div>
    </form>
  )
}

function DimSelect({ dim, value, rangeParams, onChange }: { dim: string; value: string; rangeParams: Params; onChange: (v: string) => void }) {
  const facets = useLogFacets(dim, rangeParams)
  const values = facets.data?.values ?? []
  const options = value && !values.some((v) => v.value === value) ? [{ value, count: 0 }, ...values] : values
  const label: Record<string, string> = { service_name: '服务', namespace: 'namespace', pod: 'pod', container: '容器', host: '主机', stream: 'stream', cluster: '集群' }
  return (
    <Select value={value} onChange={(e) => onChange(e.target.value)} className={cn('max-w-56', value && 'border-accent text-accent')} title={dim}>
      <option value="">{label[dim] ?? dim}{facets.isPending ? '…' : ''}</option>
      {options.map((v) => (
        <option key={v.value} value={v.value}>
          {v.value || '(空)'}
          {v.count ? ` (${v.count.toLocaleString('zh-CN')})` : ''}
        </option>
      ))}
    </Select>
  )
}
