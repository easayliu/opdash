import { useEffect, useState, type FormEvent } from 'react'
import { FilterIcon, HelpCircleIcon, SlidersHorizontalIcon, XIcon } from 'lucide-react'
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

/** 手机上一行放两个：筛选栏 gap 是 0.5rem，各让出一半 */
const HALF_ON_MOBILE = 'w-[calc(50%-0.25rem)]'

/** 常用维度：直接摆在筛选栏上。其余（host / stream / span_kind 这类不常用的）收进「更多筛选」。 */
const PRIMARY_DIMS = ['service_name', 'namespace', 'pod', 'container']
const DIM_LABEL: Record<string, string> = {
  service_name: '服务',
  namespace: 'namespace',
  pod: 'pod',
  container: '容器',
  host: '主机',
  stream: 'stream',
  cluster: '集群',
}

/** 日志筛选栏：一行关键字 + 一行级别 / 常用维度；少用的维度折叠起来。回车 / 点查询才生效。 */
export function LogFilters({ state, dims, rangeParams, onChange }: Props) {
  const [q, setQ] = useState(state.q)
  const [logger, setLogger] = useState(state.logger)
  const [thread, setThread] = useState(state.thread)
  const [help, setHelp] = useState(false)
  const [more, setMore] = useState(false)
  // 手机上级别 / 维度那行默认收起，按钮上带着生效的条件数
  const [mobileOpen, setMobileOpen] = useState(false)
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
  const primary = PRIMARY_DIMS.filter((d) => dims.includes(d))
  const secondary = dims.filter((d) => !PRIMARY_DIMS.includes(d))
  const secondaryActive = secondary.filter((d) => state.dims[d]?.length).length
  const showMore = more || secondaryActive > 0
  const activeIds = state.trace_id || state.span_id
  const hasCondition = state.levels.length > 0 || Object.keys(state.dims).length > 0 || state.q || state.logger || state.thread
  const conditionCount = (state.levels.length ? 1 : 0) + Object.keys(state.dims).length + (state.logger ? 1 : 0) + (state.thread ? 1 : 0)
  const idChip = activeIds && (
    <span className="inline-flex h-9 max-w-full items-center gap-1.5 rounded-md bg-accent-soft px-3 text-xs text-accent">
      <span className="truncate">{state.trace_id ? `trace ${state.trace_id.slice(0, 12)}…` : `span ${state.span_id}`}（已忽略时间范围）</span>
      <button type="button" className="shrink-0" onClick={() => onChange({ ...state, trace_id: '', span_id: '' })} title="去掉">
        <XIcon className="size-3.5" />
      </button>
    </span>
  )

  return (
    <form onSubmit={submit} className="flex flex-col gap-2.5 border-b border-border bg-card px-3 py-2.5 md:px-4 md:py-3">
      <div className="flex items-center gap-2">
        <div className="relative min-w-0 flex-1">
          <Input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder={state.regex ? '正则（RE2）：如 orderId=\\d+ .*timeout' : '搜索 message：空格 = 且；a OR b 任一；-词 排除；"带 空格" 整体；( ) 分组'}
            className="mono pr-9"
            aria-label="日志关键字"
          />
          <button
            type="button"
            className="absolute top-1/2 right-2.5 -translate-y-1/2 text-muted-fg hover:text-fg"
            onClick={() => setHelp((h) => !h)}
            title="语法说明"
          >
            <HelpCircleIcon className="size-4" />
          </button>
        </div>
        <Button
          size="md"
          active={state.regex}
          className="mono px-2.5 md:px-3.5"
          onClick={() => onChange({ ...state, regex: !state.regex, q: q.trim() })}
          title="按正则匹配 message（ClickHouse match，RE2 语法，区分大小写；加 (?i) 忽略大小写）"
        >
          .*
        </Button>
        <Button
          size="md"
          active={mobileOpen || conditionCount > 0}
          className="px-2.5 md:hidden"
          onClick={() => setMobileOpen((v) => !v)}
          title="级别 / 服务 / pod 等筛选"
          aria-expanded={mobileOpen}
        >
          <FilterIcon className="size-4" />
          {conditionCount > 0 && conditionCount}
        </Button>
        <Button type="submit" variant="primary" className="px-3.5 md:px-5">
          查询
        </Button>
      </div>
      {/* 手机上筛选行收起时，按 id 查的提示也得露出来，不然看不出为什么时间范围不生效 */}
      {idChip && !mobileOpen && <div className="flex md:hidden">{idChip}</div>}
      {help && (
        <div className="rounded-md border border-border bg-muted/50 px-4 py-2.5 text-xs leading-6 text-muted-fg">
          关键字模式（不分大小写）：空格分隔的词全部要命中；<Kbd>a OR b</Kbd> 任一命中；<Kbd>-词</Kbd> / <Kbd>NOT 词</Kbd> 排除；<Kbd>"带 空格"</Kbd> 当一个整体；
          <Kbd>( )</Kbd> 分组，如 <Kbd>(timeout OR refused) -重试</Kbd>。<Kbd>AND</Kbd> / <Kbd>OR</Kbd> / <Kbd>NOT</Kbd> 全大写才是操作符，小写的 or 是普通词。
          正则模式（<Kbd>.*</Kbd>）：ClickHouse <code>match()</code>，RE2 语法，区分大小写，<Kbd>(?i)</Kbd> 忽略大小写。message
          没有索引，扫的是时间范围内的全部日志——先选服务 / pod、缩小时间范围，查询更快。
        </div>
      )}
      <div className={cn('flex flex-wrap items-center gap-2', !mobileOpen && 'hidden md:flex')}>
        <div className="flex h-9 w-full items-center gap-0.5 rounded-md border border-input p-0.5 md:w-auto" title="日志级别（可多选）">
          {LEVELS.map((l) => (
            <button
              key={l}
              type="button"
              onClick={() => toggleLevel(l)}
              className={cn(
                'h-full flex-1 rounded-sm px-2 text-xs font-semibold text-muted-fg hover:bg-muted md:flex-none md:px-2.5',
                state.levels.includes(l) && 'bg-accent-soft text-accent',
              )}
            >
              {l}
            </button>
          ))}
        </div>
        {primary.map((dim) => (
          <DimSelect key={dim} dim={dim} value={state.dims[dim]?.[0] ?? ''} rangeParams={rangeParams} onChange={(v) => setDim(dim, v)} />
        ))}
        <Input value={logger} onChange={(e) => setLogger(e.target.value)} placeholder="logger 包含…" className={HALF_ON_MOBILE + ' md:w-44'} aria-label="logger" />
        <Input value={thread} onChange={(e) => setThread(e.target.value)} placeholder="thread 包含…" className={HALF_ON_MOBILE + ' md:w-40'} aria-label="thread" />
        {secondary.length > 0 && (
          <Button
            variant="ghost"
            size="md"
            active={showMore}
            onClick={() => setMore((m) => !m)}
            title={`其余 ${secondary.length} 个维度：${secondary.join('、')}`}
          >
            <SlidersHorizontalIcon className="size-4" />
            更多筛选{secondaryActive > 0 && ` · ${secondaryActive}`}
          </Button>
        )}
        {idChip}
        {hasCondition && (
          <Button
            variant="ghost"
            size="md"
            onClick={() => onChange({ q: '', regex: state.regex, levels: [], logger: '', thread: '', trace_id: state.trace_id, span_id: state.span_id, dims: {} })}
          >
            清空条件
          </Button>
        )}
      </div>
      {showMore && secondary.length > 0 && (
        <div className="flex flex-wrap items-center gap-2 border-t border-border/60 pt-2.5">
          <span className="text-xs text-muted-fg">其他维度</span>
          {secondary.map((dim) => (
            <DimSelect key={dim} dim={dim} value={state.dims[dim]?.[0] ?? ''} rangeParams={rangeParams} onChange={(v) => setDim(dim, v)} />
          ))}
        </div>
      )}
    </form>
  )
}

function DimSelect({ dim, value, rangeParams, onChange }: { dim: string; value: string; rangeParams: Params; onChange: (v: string) => void }) {
  const facets = useLogFacets(dim, rangeParams)
  const values = facets.data?.values ?? []
  const options = value && !values.some((v) => v.value === value) ? [{ value, count: 0 }, ...values] : values
  return (
    <Select value={value} onChange={(e) => onChange(e.target.value)} className={cn(HALF_ON_MOBILE, 'md:w-44', value && 'border-accent text-accent')} title={dim}>
      <option value="">{DIM_LABEL[dim] ?? dim}{facets.isPending ? '…' : ''}</option>
      {options.map((v) => (
        <option key={v.value} value={v.value}>
          {v.value || '(空)'}
          {v.count ? ` (${v.count.toLocaleString('zh-CN')})` : ''}
        </option>
      ))}
    </Select>
  )
}
