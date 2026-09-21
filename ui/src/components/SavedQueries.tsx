import { useEffect, useMemo, useRef, useState, type FormEvent, type KeyboardEvent } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { useLocation, useNavigate } from 'react-router'
import { BookmarkCheckIcon, BookmarkIcon, CheckIcon, PencilIcon, RefreshCwIcon, Trash2Icon, XIcon } from 'lucide-react'
import { apiDelete, apiPost, apiPut } from '@/api/client'
import { useAuthMe, useSavedQueries } from '@/api/queries'
import type { SavedQuery } from '@/api/types'
import { Button, Input, Spinner } from '@/components/ui'
import { describeQuery, pageLabel, sameView, savableView, savedHref, suggestName, type View } from '@/lib/saved'
import { cn } from '@/lib/utils'

/**
 * 顶栏的书签：收藏当前查询、打开 / 改名 / 删除已收藏的。收藏归在登录账号名下（`/api/saved`），
 * 换台机器还在；没开认证的部署大家共用一份。
 *
 * 收藏的是当前页面的地址（筛选条件 + 相对时间范围），不记翻页位置和绝对时间段，见 `lib/saved.ts`。
 */
export function SavedQueries() {
  const { pathname, search } = useLocation()
  const navigate = useNavigate()
  const qc = useQueryClient()
  const me = useAuthMe()
  const list = useSavedQueries()
  const [open, setOpen] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const ref = useRef<HTMLDivElement>(null)

  const view = useMemo(() => savableView(pathname, search), [pathname, search])
  const queries = list.data?.queries ?? []
  const existing = view ? queries.find((q) => sameView(view, q)) : undefined

  useEffect(() => {
    if (!open) return
    setError(null)
    const onClick = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false)
    }
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('mousedown', onClick)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onClick)
      document.removeEventListener('keydown', onKey)
    }
  }, [open])

  const refresh = () => qc.invalidateQueries({ queryKey: ['saved'] })
  const run = async (action: () => Promise<unknown>) => {
    setError(null)
    try {
      await action()
      await refresh()
      return true
    } catch (err) {
      setError((err as Error).message)
      return false
    }
  }
  const save = (name: string, v: View) => run(() => apiPost<SavedQuery>('/saved', { name, path: v.path, query: v.query }))
  const rename = (q: SavedQuery, name: string) => run(() => apiPut<SavedQuery>(`/saved/${encodeURIComponent(q.id)}`, { name }))
  const overwrite = (q: SavedQuery, v: View) => run(() => apiPut<SavedQuery>(`/saved/${encodeURIComponent(q.id)}`, { path: v.path, query: v.query }))
  const remove = (q: SavedQuery) => run(() => apiDelete(`/saved/${encodeURIComponent(q.id)}`))
  const go = (q: SavedQuery) => {
    setOpen(false)
    navigate(savedHref(q))
  }

  // 按页面分组，组内保持后端给的顺序（新的在前）
  const groups = useMemo(() => {
    const m = new Map<string, SavedQuery[]>()
    for (const q of queries) {
      const label = pageLabel(q.path)
      m.set(label, [...(m.get(label) ?? []), q])
    }
    return [...m.entries()]
  }, [queries])
  const authed = me.data && me.data.mode !== 'none' && me.data.user

  return (
    <div ref={ref} className="relative flex items-center">
      <Button
        variant="ghost"
        className="px-2.5"
        active={!!existing}
        aria-expanded={open}
        title={existing ? `已收藏为「${existing.name}」` : view ? '收藏当前查询' : '我的收藏'}
        onClick={() => setOpen((o) => !o)}
      >
        {existing ? <BookmarkCheckIcon className="size-4" /> : <BookmarkIcon className="size-4" />}
      </Button>
      {open && (
        // 手机上钉在视口顶部撑满宽度；桌面挂在按钮下面
        <div className="fixed inset-x-3 top-14 z-30 flex max-h-[calc(100dvh-5rem)] flex-col rounded-lg border border-border bg-card shadow-lg md:absolute md:inset-x-auto md:top-full md:right-0 md:mt-1 md:w-[26rem]">
          <div className="shrink-0 border-b border-border p-3">
            {view ? (
              existing ? (
                <div className="flex items-center gap-2 text-sm">
                  <BookmarkCheckIcon className="size-4 shrink-0 text-accent" />
                  <span className="min-w-0 flex-1 truncate">
                    已收藏为 <span className="font-medium">{existing.name}</span>
                  </span>
                  <Button size="xs" onClick={() => remove(existing)}>
                    取消收藏
                  </Button>
                </div>
              ) : (
                <SaveForm view={view} onSave={save} />
              )
            ) : (
              <p className="text-xs leading-5 text-muted-fg">这一页不能收藏：链路详情是一条具体的 trace，不是查询。到日志 / 链路 / 错误 / 指标页设好条件再来。</p>
            )}
          </div>
          <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">
            <div className="flex items-center gap-2 px-3 pt-2.5 pb-1 text-2xs text-muted-fg">
              <span className="font-medium text-fg">我的收藏</span>
              {list.data && (
                <span>
                  {queries.length} 条{queries.length >= list.data.max * 0.9 && ` · 上限 ${list.data.max}`}
                </span>
              )}
              {list.isFetching && <Spinner className="size-3" />}
              <span className="ml-auto truncate" title={me.data?.user?.email ?? undefined}>
                {authed ? `归在 ${me.data?.user?.name} 名下` : me.data?.mode === 'none' ? '没开认证，所有人共用' : ''}
              </span>
            </div>
            {error && <p className="mx-3 my-1 rounded-md border border-danger/40 bg-danger-soft px-2.5 py-1.5 text-xs text-danger">{error}</p>}
            {list.isError && <p className="px-3 py-2 text-xs text-danger">{(list.error as Error).message}</p>}
            {list.data && queries.length === 0 && (
              <p className="m-3 rounded-md border border-dashed border-border px-3 py-6 text-center text-xs leading-5 text-muted-fg">
                还没有收藏。在日志 / 链路 / 错误 / 指标页设好筛选条件，回到这里点「收藏」，下次一步打开。
              </p>
            )}
            {groups.map(([label, items]) => (
              <section key={label} className="pb-1.5">
                <div className="px-3 pt-1.5 pb-0.5 text-2xs font-semibold tracking-wide text-muted-fg uppercase">{label}</div>
                <ul>
                  {items.map((q) => (
                    <Row
                      key={q.id}
                      q={q}
                      current={!!view && sameView(view, q)}
                      canOverwrite={!!view && !existing && view.path === q.path}
                      onOpen={() => go(q)}
                      onRename={(name) => rename(q, name)}
                      onOverwrite={() => view && overwrite(q, view)}
                      onDelete={() => remove(q)}
                    />
                  ))}
                </ul>
              </section>
            ))}
          </div>
        </div>
      )}
    </div>
  )
}

/** 收藏当前视图：名字预填一句由条件拼出来的话，直接回车就行 */
function SaveForm({ view, onSave }: { view: View; onSave: (name: string, view: View) => Promise<boolean> }) {
  const suggested = suggestName(view)
  const [name, setName] = useState(suggested)
  const [busy, setBusy] = useState(false)
  // 换了页面 / 条件，预填的名字跟着换（用户没改过才换）
  const [base, setBase] = useState(suggested)
  if (base !== suggested) {
    setBase(suggested)
    if (name === base) setName(suggested)
  }
  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setBusy(true)
    await onSave(name.trim() || suggested, view)
    setBusy(false)
  }
  const words = describeQuery(view.query)
  return (
    <form onSubmit={submit} className="flex flex-col gap-2">
      <div className="flex items-center gap-2">
        <Input value={name} onChange={(e) => setName(e.target.value)} maxLength={64} placeholder={suggested} className="h-8 text-xs" aria-label="收藏的名字" autoFocus />
        <Button type="submit" variant="primary" size="sm" disabled={busy}>
          <BookmarkIcon className="size-3.5" />
          收藏
        </Button>
      </div>
      <p className="truncate text-2xs text-muted-fg" title={words.join(' · ')}>
        {pageLabel(view.path)}
        {words.length > 0 && ` · ${words.join(' · ')}`}
        {words.length === 0 && ' · 没有筛选条件'}
      </p>
    </form>
  )
}

function Row({
  q,
  current,
  canOverwrite,
  onOpen,
  onRename,
  onOverwrite,
  onDelete,
}: {
  q: SavedQuery
  current: boolean
  /** 当前页面和这条同属一页、条件不同、也还没收藏过：可以把这条改成当前条件 */
  canOverwrite: boolean
  onOpen: () => void
  onRename: (name: string) => Promise<boolean>
  onOverwrite: () => void
  onDelete: () => void
}) {
  const [editing, setEditing] = useState(false)
  const [name, setName] = useState(q.name)
  // 删除分两步点，不弹 confirm：第一下变成「确认」，几秒不点就复原
  const [arm, setArm] = useState(false)
  useEffect(() => {
    if (!arm) return
    const t = setTimeout(() => setArm(false), 4000)
    return () => clearTimeout(t)
  }, [arm])
  const words = describeQuery(q.query)
  const commit = async () => {
    const next = name.trim()
    if (next && next !== q.name && !(await onRename(next))) return
    setEditing(false)
  }
  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter') {
      e.preventDefault()
      void commit()
    } else if (e.key === 'Escape') {
      // 只收起改名框，别把整个弹层关了
      e.stopPropagation()
      setName(q.name)
      setEditing(false)
    }
  }
  return (
    <li className={cn('group flex items-center gap-1 px-1.5', current && 'bg-accent-soft/60')}>
      {editing ? (
        <div className="flex min-w-0 flex-1 items-center gap-1 py-1 pl-1.5">
          <Input value={name} onChange={(e) => setName(e.target.value)} onKeyDown={onKey} maxLength={64} className="h-7 text-xs" aria-label="新名字" autoFocus />
          <Button size="xs" variant="ghost" className="px-1.5" onClick={commit} title="保存 (Enter)">
            <CheckIcon className="size-3.5" />
          </Button>
          <Button
            size="xs"
            variant="ghost"
            className="px-1.5"
            onClick={() => {
              setName(q.name)
              setEditing(false)
            }}
            title="取消 (Esc)"
          >
            <XIcon className="size-3.5" />
          </Button>
        </div>
      ) : (
        <button type="button" onClick={onOpen} className="flex min-w-0 flex-1 flex-col items-start rounded-md px-1.5 py-1.5 text-left hover:bg-muted" title={savedHref(q)}>
          <span className={cn('w-full truncate text-xs font-medium', current && 'text-accent')}>{q.name}</span>
          <span className="w-full truncate text-2xs text-muted-fg">{words.length ? words.join(' · ') : '没有筛选条件'}</span>
        </button>
      )}
      {!editing && (
        <span className="flex shrink-0 items-center opacity-60 group-hover:opacity-100">
          <Button size="xs" variant="ghost" className="px-1.5" onClick={() => setEditing(true)} title="改名">
            <PencilIcon className="size-3.5" />
          </Button>
          {canOverwrite && (
            <Button size="xs" variant="ghost" className="px-1.5" onClick={onOverwrite} title="把这条改成当前页面的条件">
              <RefreshCwIcon className="size-3.5" />
            </Button>
          )}
          <Button size="xs" variant={arm ? 'danger' : 'ghost'} className="px-1.5" onClick={() => (arm ? onDelete() : setArm(true))} title={arm ? '再点一下就删了' : '删除'}>
            <Trash2Icon className="size-3.5" />
            {arm && '确认'}
          </Button>
        </span>
      )}
    </li>
  )
}
