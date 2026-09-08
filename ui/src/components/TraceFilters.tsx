import { useEffect, useState, type FormEvent } from 'react'
import { PlusIcon, XIcon } from 'lucide-react'
import type { Params } from '@/api/client'
import { useAttrKeys, useAttrValues, useTraceValues } from '@/api/queries'
import { Button, Input, Select } from '@/components/ui'
import { cn } from '@/lib/utils'

export const KINDS = ['Server', 'Client', 'Internal', 'Producer', 'Consumer']

export interface TraceFilterState {
  service: string
  span_name: string
  kinds: string[]
  error_only: boolean
  min_ms: string
  max_ms: string
  /** `key=value` 或 `key` */
  attrs: string[]
  sort: 'time' | 'duration'
}

interface Props {
  state: TraceFilterState
  rangeParams: Params
  onChange: (next: TraceFilterState) => void
}

export function TraceFilters({ state, rangeParams, onChange }: Props) {
  const services = useTraceValues({ ...rangeParams, field: 'service', limit: 500 })
  const ops = useTraceValues({ ...rangeParams, field: 'span_name', service: state.service, kind: 'all', limit: 500 }, !!state.service)
  const [minMs, setMinMs] = useState(state.min_ms)
  const [maxMs, setMaxMs] = useState(state.max_ms)
  useEffect(() => setMinMs(state.min_ms), [state.min_ms])
  useEffect(() => setMaxMs(state.max_ms), [state.max_ms])
  const submit = (e?: FormEvent) => {
    e?.preventDefault()
    onChange({ ...state, min_ms: minMs.trim(), max_ms: maxMs.trim() })
  }
  const serviceOptions = state.service && !services.data?.values.some((v) => v.value === state.service) ? [{ value: state.service, count: 0 }, ...(services.data?.values ?? [])] : (services.data?.values ?? [])
  const opOptions = state.span_name && !ops.data?.values.some((v) => v.value === state.span_name) ? [{ value: state.span_name, count: 0 }, ...(ops.data?.values ?? [])] : (ops.data?.values ?? [])

  return (
    <form onSubmit={submit} className="flex flex-col gap-2 border-b border-border bg-card px-3 py-2">
      <div className="flex flex-wrap items-center gap-2">
        <Select
          value={state.service}
          onChange={(e) => onChange({ ...state, service: e.target.value, span_name: '' })}
          className={cn('max-w-64', state.service && 'border-accent text-accent')}
          title="服务（service.name）"
        >
          <option value="">全部服务{services.isPending ? '…' : ''}</option>
          {serviceOptions.map((v) => (
            <option key={v.value} value={v.value}>
              {v.value}
              {v.count ? ` (${v.count.toLocaleString('zh-CN')})` : ''}
            </option>
          ))}
        </Select>
        <Select
          value={state.span_name}
          onChange={(e) => onChange({ ...state, span_name: e.target.value })}
          className={cn('max-w-80', state.span_name && 'border-accent text-accent')}
          disabled={!state.service}
          title={state.service ? '接口 / 操作（span_name）' : '先选服务'}
        >
          <option value="">{state.service ? `全部操作${ops.isPending ? '…' : ''}` : '操作（先选服务）'}</option>
          {opOptions.map((v) => (
            <option key={v.value} value={v.value}>
              {v.value}
              {v.count ? ` (${v.count.toLocaleString('zh-CN')})` : ''}
            </option>
          ))}
        </Select>
        <div className="flex items-center gap-0.5 rounded-md border border-border p-0.5" title="span 类型：Server = 收到的请求，Client = 对外调用（HTTP / DB / MQ），Consumer = 消费消息">
          {KINDS.map((k) => (
            <button
              key={k}
              type="button"
              onClick={() => onChange({ ...state, kinds: state.kinds.includes(k) ? state.kinds.filter((x) => x !== k) : [...state.kinds, k] })}
              className={cn('h-6 rounded-sm px-2 text-2xs font-semibold text-muted-fg hover:bg-muted', state.kinds.includes(k) && 'bg-accent-soft text-accent')}
            >
              {k}
            </button>
          ))}
        </div>
        <Button size="md" active={state.error_only} onClick={() => onChange({ ...state, error_only: !state.error_only })} title="只看 status = Error 的 span 所在的链路">
          只看错误
        </Button>
        <span className="flex items-center gap-1 text-xs text-muted-fg">
          耗时
          <Input value={minMs} onChange={(e) => setMinMs(e.target.value)} placeholder="≥ ms" className="w-20" inputMode="decimal" aria-label="最小耗时" />
          ~
          <Input value={maxMs} onChange={(e) => setMaxMs(e.target.value)} placeholder="≤ ms" className="w-20" inputMode="decimal" aria-label="最大耗时" />
        </span>
        <Select value={state.sort} onChange={(e) => onChange({ ...state, sort: e.target.value === 'duration' ? 'duration' : 'time' })} title="排序">
          <option value="time">最新在前</option>
          <option value="duration">最慢在前</option>
        </Select>
        <Button type="submit" variant="primary">
          查询
        </Button>
      </div>
      <AttrFilters attrs={state.attrs} service={state.service} rangeParams={rangeParams} onChange={(attrs) => onChange({ ...state, attrs })} />
    </form>
  )
}

/** 属性过滤：key=value 的小标签，加一个带 key / value 提示的输入行。 */
function AttrFilters({ attrs, service, rangeParams, onChange }: { attrs: string[]; service: string; rangeParams: Params; onChange: (a: string[]) => void }) {
  const [key, setKey] = useState('')
  const [value, setValue] = useState('')
  const keys = useAttrKeys({ ...rangeParams, service: service || undefined, limit: 300 })
  const values = useAttrValues({ ...rangeParams, service: service || undefined, key, limit: 50 }, !!key && keys.data?.keys.some((k) => k.key === key) === true)
  const add = () => {
    const k = key.trim()
    if (!k) return
    const item = value.trim() ? `${k}=${value.trim()}` : k
    if (!attrs.includes(item)) onChange([...attrs, item])
    setKey('')
    setValue('')
  }
  return (
    <div className="flex flex-wrap items-center gap-1.5">
      <span className="text-2xs text-muted-fg">属性</span>
      {attrs.map((a) => (
        <span key={a} className="mono inline-flex items-center gap-1 rounded-md bg-accent-soft px-2 py-0.5 text-2xs text-accent">
          {a}
          <button type="button" onClick={() => onChange(attrs.filter((x) => x !== a))} title="去掉">
            <XIcon className="size-3" />
          </button>
        </span>
      ))}
      <Input value={key} onChange={(e) => setKey(e.target.value)} list="attr-keys" placeholder="属性名，如 http.route" className="mono h-7 w-56 text-xs" aria-label="属性名" />
      <datalist id="attr-keys">
        {(keys.data?.keys ?? []).map((k) => (
          <option key={k.key} value={k.key} />
        ))}
      </datalist>
      <span className="text-muted-fg">=</span>
      <Input
        value={value}
        onChange={(e) => setValue(e.target.value)}
        list="attr-values"
        placeholder="值（留空 = 只要有这个属性）"
        className="mono h-7 w-64 text-xs"
        aria-label="属性值"
        onKeyDown={(e) => {
          if (e.key === 'Enter') {
            e.preventDefault()
            add()
          }
        }}
      />
      <datalist id="attr-values">
        {(values.data?.values ?? []).map((v) => (
          <option key={v.value} value={v.value} />
        ))}
      </datalist>
      <Button size="sm" onClick={add} disabled={!key.trim()}>
        <PlusIcon className="size-3.5" />
        加条件
      </Button>
    </div>
  )
}
