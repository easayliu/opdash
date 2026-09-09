import { useEffect, useState, type FormEvent } from 'react'
import { FilterIcon, PlusIcon, XIcon } from 'lucide-react'
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
  // 手机上只常驻服务下拉和查询按钮，其余条件收起
  const [mobileOpen, setMobileOpen] = useState(false)
  const conditionCount = (state.span_name ? 1 : 0) + (state.kinds.length ? 1 : 0) + (state.error_only ? 1 : 0) + (state.min_ms || state.max_ms ? 1 : 0) + state.attrs.length
  useEffect(() => setMinMs(state.min_ms), [state.min_ms])
  useEffect(() => setMaxMs(state.max_ms), [state.max_ms])
  const submit = (e?: FormEvent) => {
    e?.preventDefault()
    onChange({ ...state, min_ms: minMs.trim(), max_ms: maxMs.trim() })
  }
  const serviceOptions = state.service && !services.data?.values.some((v) => v.value === state.service) ? [{ value: state.service, count: 0 }, ...(services.data?.values ?? [])] : (services.data?.values ?? [])
  const opOptions = state.span_name && !ops.data?.values.some((v) => v.value === state.span_name) ? [{ value: state.span_name, count: 0 }, ...(ops.data?.values ?? [])] : (ops.data?.values ?? [])

  return (
    <form onSubmit={submit} className="flex flex-col gap-2.5 border-b border-border bg-card px-3 py-2.5 md:px-4 md:py-3">
      <div className="flex flex-wrap items-center gap-2">
        <Select
          value={state.service}
          onChange={(e) => onChange({ ...state, service: e.target.value, span_name: '' })}
          className={cn('min-w-0 flex-1 md:max-w-72 md:flex-none', state.service && 'border-accent text-accent')}
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
        <Button
          size="md"
          active={mobileOpen || conditionCount > 0}
          className="px-2.5 md:hidden"
          onClick={() => setMobileOpen((v) => !v)}
          title="操作 / 类型 / 耗时 / 属性筛选"
          aria-expanded={mobileOpen}
        >
          <FilterIcon className="size-4" />
          {conditionCount > 0 && conditionCount}
        </Button>
        <Button type="submit" variant="primary" className="px-3.5 md:hidden">
          查询
        </Button>
        <Select
          value={state.span_name}
          onChange={(e) => onChange({ ...state, span_name: e.target.value })}
          className={cn('w-full md:w-auto md:max-w-96', !mobileOpen && 'hidden md:block', state.span_name && 'border-accent text-accent')}
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
        <div className={cn('flex h-9 w-full items-center gap-0.5 rounded-md border border-input p-0.5 md:w-auto', !mobileOpen && 'hidden md:flex')} title="span 类型：Server = 收到的请求，Client = 对外调用（HTTP / DB / MQ），Consumer = 消费消息">
          {KINDS.map((k) => (
            <button
              key={k}
              type="button"
              onClick={() => onChange({ ...state, kinds: state.kinds.includes(k) ? state.kinds.filter((x) => x !== k) : [...state.kinds, k] })}
              className={cn('h-full flex-1 rounded-sm px-1.5 text-xs font-semibold text-muted-fg hover:bg-muted md:flex-none md:px-2.5', state.kinds.includes(k) && 'bg-accent-soft text-accent')}
            >
              {k}
            </button>
          ))}
        </div>
        <Button size="md" active={state.error_only} className={cn(!mobileOpen && 'hidden md:inline-flex')} onClick={() => onChange({ ...state, error_only: !state.error_only })} title="只看 status = Error 的 span 所在的链路">
          只看错误
        </Button>
        <span className={cn('flex min-w-0 items-center gap-1.5 text-sm text-muted-fg', !mobileOpen && 'hidden md:flex')}>
          耗时
          <Input value={minMs} onChange={(e) => setMinMs(e.target.value)} placeholder="≥ ms" className="w-20 md:w-24" inputMode="decimal" aria-label="最小耗时" />
          ~
          <Input value={maxMs} onChange={(e) => setMaxMs(e.target.value)} placeholder="≤ ms" className="w-20 md:w-24" inputMode="decimal" aria-label="最大耗时" />
        </span>
        <Select value={state.sort} onChange={(e) => onChange({ ...state, sort: e.target.value === 'duration' ? 'duration' : 'time' })} title="排序" className={cn(!mobileOpen && 'hidden md:block')}>
          <option value="time">最新在前</option>
          <option value="duration">最慢在前</option>
        </Select>
        <Button type="submit" variant="primary" className="hidden px-5 md:inline-flex">
          查询
        </Button>
      </div>
      <AttrFilters attrs={state.attrs} service={state.service} rangeParams={rangeParams} onChange={(attrs) => onChange({ ...state, attrs })} className={cn(!mobileOpen && 'hidden md:flex')} />
    </form>
  )
}

/** 属性过滤：key=value 的小标签，加一个带 key / value 提示的输入行。 */
function AttrFilters({ attrs, service, rangeParams, onChange, className }: { attrs: string[]; service: string; rangeParams: Params; onChange: (a: string[]) => void; className?: string }) {
  const [key, setKey] = useState('')
  const [value, setValue] = useState('')
  // 属性名 / 属性值的采样必须锁定服务：表按 service_name 排序，不锁的话抓到的永远是排序最靠前
  // 那个服务的行，下拉里既不完整也不稳定（同一条查询连跑四次拿到 33 / 20 / 10 / 10 个 key）。
  // 和「操作」下拉同一个约定：先选服务。
  const keys = useAttrKeys({ ...rangeParams, service, limit: 300 }, !!service)
  const values = useAttrValues(
    { ...rangeParams, service, key, limit: 50 },
    !!service && !!key && keys.data?.keys.some((k) => k.key === key) === true,
  )
  const add = () => {
    const k = key.trim()
    if (!k) return
    const item = value.trim() ? `${k}=${value.trim()}` : k
    if (!attrs.includes(item)) onChange([...attrs, item])
    setKey('')
    setValue('')
  }
  return (
    <div className={cn('flex flex-wrap items-center gap-2', className)}>
      <span className="text-sm text-muted-fg">属性</span>
      {attrs.map((a) => (
        <span key={a} className="mono inline-flex h-8 items-center gap-1.5 rounded-md bg-accent-soft px-2.5 text-xs text-accent">
          {a}
          <button type="button" onClick={() => onChange(attrs.filter((x) => x !== a))} title="去掉">
            <XIcon className="size-3.5" />
          </button>
        </span>
      ))}
      <Input
        value={key}
        onChange={(e) => setKey(e.target.value)}
        list="attr-keys"
        disabled={!service}
        placeholder={service ? '属性名，如 http.route' : '属性名（先选服务）'}
        title={service ? '属性名（span_attributes / resource_attributes）' : '先选服务'}
        className="mono h-8 w-full text-xs md:w-64"
        aria-label="属性名"
      />
      <datalist id="attr-keys">
        {(keys.data?.keys ?? []).map((k) => (
          <option key={k.key} value={k.key} />
        ))}
      </datalist>
      <span className="hidden text-muted-fg md:inline">=</span>
      <Input
        value={value}
        onChange={(e) => setValue(e.target.value)}
        list="attr-values"
        disabled={!service}
        placeholder="值（留空 = 只要有这个属性）"
        className="mono h-8 min-w-0 flex-1 text-xs md:w-72 md:flex-none"
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
        <PlusIcon className="size-4" />
        加条件
      </Button>
    </div>
  )
}
