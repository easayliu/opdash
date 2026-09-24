/**
 * 费用页「账单」视图的明细表：逐行账单，可按列排序、在表头按维度筛选、自选显示哪些列。
 *
 * **排序在服务端做**（`sort` / `order`）：明细是分页取的，在页面上排只排得动这一页的 50 行，
 * 翻到下一页顺序就接不上了。表头筛选写进 URL 上的维度参数（同一列可多选，逗号隔开），与点「按产品排行」加的筛选是同一套，
 * 顶上的筛选条、图表与排行都跟着变——明细里筛出来的，就是整页在看的那部分。
 *
 * 列分两类：跨云统一过的常用字段（`DETAIL_COLUMNS`），以及当前这张账单表的全部原始字段
 * （取自元数据的列清单，火山与阿里云各不相同），中文说明与分组见 `@/lib/billFields`。原始字段
 * 勾了才查（接口的 `cols`），不勾不增加查询的开销；它们能排序，不带表头筛选。
 *
 * 阿里云的 `instance_name` 在 goscan 里恒为空（它用的 SDK 没有这个字段），「实例」一列对阿里云
 * 只能显示实例 ID；有可读名字的是 `nick_name`，所以它默认就显示。
 *
 * 表头排序与筛选复用日志表的 `SortHeader` / `HeaderFilter`；显示哪些列只是个人习惯，存在浏览器里。
 */
import { Suspense, useRef, useState } from 'react'
import { Columns3Icon } from 'lucide-react'
import type { BillDetailRow, BillProvider, BillsMeta, TableMeta } from '@/api/types'
import { HeaderFilter, SortHeader, ariaSort, type ColFilter, type LogSort } from '@/components/LogTable'
import { Combobox, Hint, Input, PopoverPanel, Spinner, buttonClass } from '@/components/ui'
import { PROVIDER_LABELS, formatMoney, formatMoneyShort } from '@/lib/bills'
import { fieldDoc } from '@/lib/billFields'
import { useIsMobile } from '@/lib/media'
import { cn } from '@/lib/utils'

export interface DetailColumn {
  /** 列键，也是接口 `sort` 的取值；为 null 表示这一列不能排 */
  key: string
  sort: string | null
  label: string
  /** 能在表头按它筛选的维度名（与 URL 上的维度参数同名） */
  dim?: string
  numeric?: boolean
  /** 默认显示 */
  shown: boolean
  /** 原始字段：表里的真实列名。统一字段没有这一项 */
  raw?: string
  /** 原始字段所属的分组（goscan 的字段分组） */
  section?: string
}

export const DETAIL_COLUMNS: DetailColumn[] = [
  { key: 'day', sort: 'day', label: '日期', shown: true },
  { key: 'product', sort: 'product', label: '产品', dim: 'product', shown: true },
  { key: 'item', sort: 'item', label: '计费项', dim: 'item', shown: true },
  { key: 'instance', sort: 'instance', label: '实例', dim: 'instance', shown: true },
  { key: 'instance_id', sort: null, label: '实例 ID', shown: false },
  { key: 'region', sort: 'region', label: '地域', dim: 'region', shown: true },
  { key: 'zone', sort: 'zone', label: '可用区', dim: 'zone', shown: false },
  { key: 'account', sort: 'account', label: '账号', dim: 'account', shown: false },
  { key: 'project', sort: 'project', label: '项目 / 资源组', dim: 'project', shown: false },
  { key: 'subscription', sort: 'subscription', label: '计费模式', dim: 'subscription', shown: true },
  { key: 'currency', sort: 'currency', label: '币种', dim: 'currency', shown: false },
  { key: 'usage', sort: 'usage', label: '用量', numeric: true, shown: true },
  { key: 'original', sort: 'original', label: '原价', numeric: true, shown: true },
  { key: 'discount', sort: 'discount', label: '优惠', numeric: true, shown: true },
  { key: 'paid', sort: 'paid', label: '现金', numeric: true, shown: false },
  { key: 'amount', sort: 'amount', label: '金额', numeric: true, shown: true },
]

/** 默认就显示的原始字段，见文件头 */
const DEFAULT_RAW = new Set(['nick_name'])

/**
 * 当前账单表的全部原始字段，按分组排、组内照表里的列序。键与排序都写作 `raw:<列名>`；
 * 有中文说明的以说明作列名，没有的显示字段名本身
 */
export function rawColumns(source: DetailSource | null): DetailColumn[] {
  if (!source) return []
  return source.meta.columns
    .map((c, i) => ({ c, i, doc: fieldDoc(source.provider, c.name) }))
    .sort((a, b) => a.doc.order - b.doc.order || a.i - b.i)
    .map(({ c, doc }) => ({
      key: `raw:${c.name}`,
      sort: `raw:${c.name}`,
      label: doc.label || c.name,
      numeric: c.kind === 'int' || c.kind === 'float',
      shown: DEFAULT_RAW.has(c.name),
      raw: c.name,
      section: doc.section,
    }))
}

export interface DetailSource {
  meta: TableMeta
  provider: BillProvider
}

/**
 * 明细这一页读的是哪张表，与后端 `detail_source` 同一个挑法：没指定云时火山在前；阿里云按粒度
 * 选日度或月度表，月度表不在时退到日度表。原始字段的列清单取决于它
 */
export function detailTable(bills: BillsMeta, provider: BillProvider | '', gran: string | null): DetailSource | null {
  if ((!provider || provider === 'volcengine') && bills.volcengine) return { meta: bills.volcengine, provider: 'volcengine' }
  if (provider === 'volcengine') return null
  const meta = gran === 'daily' ? bills.alicloud_daily : (bills.alicloud_monthly ?? bills.alicloud_daily)
  return meta ? { meta, provider: 'alicloud' } : null
}

/** 表头下拉要查候选值的维度：表里所有可筛选的列 */
export const FACET_DIMS = DETAIL_COLUMNS.flatMap((c) => (c.dim ? [c.dim] : []))

/**
 * 显示哪些列存在浏览器里。键带版本号：默认列有变化（加了 `nick_name`）时换一个键，否则调过
 * 列的人存着的是旧的那一套，永远看不到新加的默认列
 */
const STORAGE_KEY = 'opdash.bills.detail-columns.v2'

/** 默认显示的列：常用字段里标了 shown 的，加上 DEFAULT_RAW */
const defaultShown = () => new Set([...DETAIL_COLUMNS.filter((c) => c.shown).map((c) => c.key), ...[...DEFAULT_RAW].map((n) => `raw:${n}`)])

/** 读出上次选的列。浏览器存储可能不可用（隐私模式、被禁用），读不到就用默认 */
function loadShown(): Set<string> {
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    const keys = raw ? (JSON.parse(raw) as unknown) : null
    // 原始字段因表而异，这里认不全，一律留着：换到那张表时自然生效
    if (Array.isArray(keys) && keys.every((k) => typeof k === 'string')) {
      const known = keys.filter((k) => k.startsWith('raw:') || DETAIL_COLUMNS.some((c) => c.key === k))
      if (known.length) return new Set(known)
    }
  } catch {
    /* 退回默认 */
  }
  return defaultShown()
}

function saveShown(shown: Set<string>) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify([...shown]))
  } catch {
    /* 存不下就只在本次有效 */
  }
}

/** 显示哪些列：由调用方把它放在卡片右上角，与表格本身分开 */
export function useDetailColumns() {
  const [shown, setShown] = useState(loadShown)
  const toggle = (key: string) => {
    const next = new Set(shown)
    if (next.has(key)) next.delete(key)
    else next.add(key)
    // 至少留一列，全关掉的表没有意义
    if (!next.size) return
    setShown(next)
    saveShown(next)
  }
  const reset = () => {
    const next = defaultShown()
    setShown(next)
    saveShown(next)
  }
  /** 一次勾上或取消一批（「全部显示」「清空原始字段」） */
  const setMany = (keys: string[], on: boolean) => {
    const next = new Set(shown)
    for (const k of keys) {
      if (on) next.add(k)
      else next.delete(k)
    }
    if (!next.size) return
    setShown(next)
    saveShown(next)
  }
  return { shown, toggle, reset, setMany }
}

/** 选择显示哪些列的按钮与弹层：常用字段在上，当前账单表的原始字段在下，可搜索 */
export function ColumnPicker({
  shown,
  raw,
  table,
  onToggle,
  onReset,
  onSetMany,
}: {
  shown: Set<string>
  /** 当前账单表的原始字段 */
  raw: DetailColumn[]
  table?: string
  onToggle: (key: string) => void
  onReset: () => void
  onSetMany: (keys: string[], on: boolean) => void
}) {
  const [open, setOpen] = useState(false)
  const [q, setQ] = useState('')
  const anchor = useRef<HTMLButtonElement>(null)
  const needle = q.trim().toLowerCase()
  const match = (c: DetailColumn) => !needle || c.label.toLowerCase().includes(needle) || !!c.raw?.toLowerCase().includes(needle)
  const fixed = DETAIL_COLUMNS.filter(match)
  const rawShown = raw.filter(match)
  const rawOn = raw.filter((c) => shown.has(c.key)).length
  const item = (c: DetailColumn) => (
    <li key={c.key}>
      <label className="flex cursor-pointer items-center gap-2 rounded-md px-2 py-1 text-xs hover:bg-muted/60">
        <input type="checkbox" checked={shown.has(c.key)} onChange={() => onToggle(c.key)} className="accent-[var(--accent)]" />
        <span className="min-w-0 flex-1 truncate">{c.label}</span>
        {/* 有中文说明的原始字段，把字段名附在后面：导出的 CSV 里用的是字段名 */}
        {c.raw && c.label !== c.raw && <span className="mono max-w-28 shrink-0 truncate text-2xs text-muted-fg">{c.raw}</span>}
      </label>
    </li>
  )
  return (
    <>
      <button type="button" ref={anchor} aria-expanded={open} onClick={() => setOpen((o) => !o)} className={buttonClass({ size: 'sm' })}>
        <Columns3Icon className="size-4" />
        显示列
      </button>
      {open && (
        <Suspense fallback={null}>
          <PopoverPanel
            open={open}
            onOpenChange={setOpen}
            anchor={anchor}
            className="flex max-h-[min(32rem,calc(100dvh-6rem))] w-72 flex-col rounded-lg border border-border bg-card shadow-lg"
          >
            <div className="shrink-0 border-b border-border p-2">
              <Input value={q} onChange={(e) => setQ(e.target.value)} placeholder="搜索字段…" aria-label="搜索字段" className="h-7 text-xs" />
            </div>
            <div className="min-h-0 flex-1 overflow-y-auto p-1">
              {fixed.length > 0 && (
                <>
                  <div className="px-2 pt-1 pb-0.5 text-2xs text-muted-fg">常用字段</div>
                  <ul>{fixed.map(item)}</ul>
                </>
              )}
              {raw.length > 0 && (
                <>
                  <div className="flex items-baseline gap-2 px-2 pt-2 pb-0.5 text-2xs text-muted-fg">
                    <span className="min-w-0 flex-1 truncate">
                      原始字段{table && <span className="mono"> · {table}</span>}（{rawOn}/{raw.length}）
                    </span>
                    {rawOn > 0 && (
                      <button type="button" onClick={() => onSetMany(raw.map((c) => c.key), false)} className="shrink-0 text-accent hover:underline">
                        全部取消
                      </button>
                    )}
                  </div>
                  {rawShown.length ? (
                    // 按 goscan 的字段分组列出，七八十个字段不分组找起来费劲
                    [...new Set(rawShown.map((c) => c.section))].map((section) => (
                      <div key={section}>
                        <div className="px-2 pt-1.5 pb-0.5 text-2xs text-muted-fg/80">{section}</div>
                        <ul>{rawShown.filter((c) => c.section === section).map(item)}</ul>
                      </div>
                    ))
                  ) : (
                    <p className="px-2 py-1 text-2xs text-muted-fg">没有匹配的字段</p>
                  )}
                </>
              )}
            </div>
            <div className="flex shrink-0 items-center gap-3 border-t border-border px-3 py-1.5 text-2xs">
              <button type="button" onClick={() => onSetMany([...DETAIL_COLUMNS, ...raw].map((c) => c.key), true)} className="text-accent hover:underline">
                全部显示
              </button>
              <button type="button" onClick={onReset} className="text-accent hover:underline">
                恢复默认列
              </button>
            </div>
          </PopoverPanel>
        </Suspense>
      )}
    </>
  )
}

const WEEKDAYS = '日一二三四五六'

/** 账期区间内的每一天，新的在前：`2026-09-23 周三` */
function daysOf(fromPeriod: string, toPeriod: string): { value: string; label: string }[] {
  const start = Date.parse(`${fromPeriod}-01T00:00:00Z`)
  const [y, m] = toPeriod.split('-').map(Number)
  const end = Date.UTC(y, m, 1) // 结束账期次月 1 日，不含
  if (Number.isNaN(start) || Number.isNaN(end)) return []
  const out: { value: string; label: string }[] = []
  for (let t = end - 86_400_000; t >= start; t -= 86_400_000) {
    const d = new Date(t)
    out.push({ value: d.toISOString().slice(0, 10), label: `${d.toISOString().slice(0, 10)} 周${WEEKDAYS[d.getUTCDay()]}` })
  }
  return out
}

/**
 * 明细的日期筛选：先选一天，要看几天再选截止日。只作用于明细（及其导出、表头候选值），
 * 统计卡片、趋势与排行仍按账期——「9 月费用」卡片里只装一天，比不筛更容易看错。
 * 只有日度账单有日期，选了日期后阿里云的明细自动换到日度表
 */
export function DayRangePicker({
  fromPeriod,
  toPeriod,
  dayFrom,
  dayTo,
  onChange,
}: {
  fromPeriod: string
  toPeriod: string
  dayFrom: string
  dayTo: string
  onChange: (next: { day_from: string | null; day_to: string | null }) => void
}) {
  const days = daysOf(fromPeriod, toPeriod)
  return (
    <span className="flex items-center gap-1 text-xs text-muted-fg">
      <Combobox
        value={dayFrom}
        onChange={(v) => onChange({ day_from: v || null, day_to: v && dayTo > v ? dayTo : null })}
        options={days}
        placeholder="全部日期"
        searchPlaceholder="筛日期，如 09-23…"
        emptyText="没有匹配的日期"
        title="明细的日期"
        size="sm"
        className="w-36"
      />
      {dayFrom && (
        <>
          <span>至</span>
          <Combobox
            value={dayTo}
            onChange={(v) => onChange({ day_from: dayFrom, day_to: v || null })}
            options={days.filter((d) => d.value > dayFrom)}
            placeholder="当天"
            searchPlaceholder="筛日期…"
            emptyText="没有更晚的日期"
            title="明细的截止日期"
            size="sm"
            className="w-36"
          />
        </>
      )}
    </span>
  )
}

const text = (v: string) => v || '—'

function Cell({ col, r }: { col: DetailColumn; r: BillDetailRow }) {
  if (col.raw) {
    const v = r.extra?.[col.raw] ?? ''
    // 原始字段什么都有（JSON、长串的标签），截断显示，悬停看全文
    return <span title={v || undefined}>{text(v)}</span>
  }
  switch (col.key) {
    case 'day':
      return <span className="tabular-nums">{r.day || r.period}</span>
    case 'product':
      return (
        <Hint text={`${PROVIDER_LABELS[r.provider]} · ${r.account || '—'} · ${r.subscription || '—'}`} asChild>
          <span>{r.product}</span>
        </Hint>
      )
    case 'instance':
      return (
        <Hint text={r.instance_id || r.instance} asChild>
          <span className="mono">{r.instance || r.instance_id || '—'}</span>
        </Hint>
      )
    case 'instance_id':
      return <span className="mono">{text(r.instance_id)}</span>
    case 'usage':
      return <>{r.usage ? `${r.usage} ${r.usage_unit}` : '—'}</>
    case 'original':
      return <>{formatMoney(r.original)}</>
    case 'discount': {
      const d = r.original - r.amount
      return Math.abs(d) < 0.005 ? <span className="text-muted-fg/60">—</span> : <>{formatMoney(d)}</>
    }
    case 'paid':
      return <>{formatMoney(r.paid)}</>
    case 'amount':
      return (
        <>
          {formatMoney(r.amount)}
          {!r.original || r.original <= r.amount ? null : (
            <Hint text={`原价 ${formatMoney(r.original)}`} asChild>
              <span className="ml-1 text-2xs text-muted-fg line-through">{formatMoneyShort(r.original)}</span>
            </Hint>
          )}
        </>
      )
    default:
      return <>{text(String(r[col.key as keyof BillDetailRow] ?? ''))}</>
  }
}

export function BillDetailTable({
  rows,
  pending,
  stale,
  columns,
  shown,
  sort,
  onSort,
  filters,
  amountLabel,
}: {
  rows: BillDetailRow[]
  pending: boolean
  stale?: boolean
  /** 可显示的全部列：常用字段加当前账单表的原始字段 */
  columns: DetailColumn[]
  shown: Set<string>
  sort: LogSort
  onSort: (key: string) => void
  /** 按维度名给出表头的下拉筛选 */
  filters: Record<string, ColFilter>
  /** 金额口径（应付 / 现金 / 原价），写进金额列的表头 */
  amountLabel: string
}) {
  const isMobile = useIsMobile()
  const picked = columns.filter((c) => shown.has(c.key))
  // 手机上一屏只放得下两三列，按原顺序金额排在最末，要横滑到头才看得到。把「产品 + 金额」
  // 提到最前，其余照旧往右排；表头的排序与筛选都还在，不换成卡片丢掉它们
  const lead = isMobile ? picked.filter((c) => c.key === 'product' || c.key === 'amount') : []
  const cols = lead.length ? [...lead, ...picked.filter((c) => !lead.includes(c))] : picked
  if (pending) {
    return (
      <div className="flex justify-center py-10">
        <Spinner />
      </div>
    )
  }
  return (
    <div className={cn('overflow-x-auto', stale && 'opacity-60 transition-opacity')}>
      <table className="w-full text-xs">
        <thead className="sticky top-0 bg-card text-2xs text-muted-fg">
          <tr className="border-b border-border">
            {cols.map((c) => {
              const label = c.key === 'amount' ? `金额（${amountLabel}）` : c.label
              const f = c.dim ? filters[c.dim] : undefined
              return (
                <th
                  key={c.key}
                  title={c.raw && c.raw !== label ? c.raw : undefined}
                  aria-sort={c.sort ? ariaSort(c.sort, sort, onSort) : undefined}
                  className={cn('px-3 py-2 font-medium whitespace-nowrap', c.numeric ? 'text-right' : 'text-left', f && 'min-w-36')}
                >
                  <span className={cn('inline-flex max-w-full items-center gap-1.5', c.numeric && 'justify-end', c.raw === label && 'mono')}>
                    {c.sort ? <SortHeader label={label} col={c.sort} sort={sort} onSort={onSort} /> : label}
                    {f && <HeaderFilter col={c.dim ?? c.key} label={c.label} filter={f} />}
                  </span>
                </th>
              )
            })}
          </tr>
        </thead>
        <tbody>
          {rows.length === 0 ? (
            <tr>
              <td colSpan={cols.length} className="px-4 py-8 text-center text-xs text-muted-fg">
                没有符合条件的账单明细
              </td>
            </tr>
          ) : (
            rows.map((r, i) => (
              <tr key={`${r.instance_id}-${r.item}-${i}`} className="border-b border-border/60 last:border-b-0">
                {cols.map((c) => (
                  <td
                    key={c.key}
                    className={cn(
                      'px-3 py-1.5',
                      c.numeric ? 'text-right whitespace-nowrap tabular-nums' : 'max-w-36 truncate md:max-w-48',
                      c.key === 'amount' && 'font-medium',
                      c.key !== 'amount' && c.key !== 'product' && c.key !== 'instance' && c.key !== 'day' && 'text-muted-fg',
                    )}
                  >
                    <Cell col={c} r={r} />
                  </td>
                ))}
              </tr>
            ))
          )}
        </tbody>
      </table>
    </div>
  )
}
