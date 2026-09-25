import { Suspense, useEffect, useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import { KeyRoundIcon, Trash2Icon, XIcon } from 'lucide-react'
import { apiDelete, apiPost } from '@/api/client'
import { useApiKeys } from '@/api/queries'
import type { ApiKeyCreated, ApiKeyInfo, AuthMe } from '@/api/types'
import { Badge, Button, CopyButton, Hint, Input, ModalPanel, Select, Spinner } from '@/components/ui'

/** 有效期的几档。上限来自后端的 `--api-key-ttl`，比上限长的档不显示 */
const TTL_OPTIONS = [
  { value: '7d', label: '7 天' },
  { value: '30d', label: '30 天' },
  { value: '90d', label: '90 天' },
  { value: '180d', label: '180 天' },
  { value: '365d', label: '1 年' },
]

/** 到期前多少天开始提醒。7 天够换一把了 */
const EXPIRING_SOON_DAYS = 7
const DAY_MS = 86_400_000

function ttlDays(s: string): number {
  const m = /^(\d+)(d|h|m)$/.exec(s.trim())
  if (!m) return Infinity
  const n = Number(m[1])
  return m[2] === 'd' ? n : m[2] === 'h' ? n / 24 : n / 1440
}

const pad = (n: number) => String(n).padStart(2, '0')

/** `2026-09-18 15:01`。固定写法，不跟浏览器 locale 走（`2026/9/18` 这种宽度会飘） */
function fmt(iso: string): string {
  const d = new Date(iso)
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

/**
 * 「还有 89 天」「3 分钟前」。表格里真正要看的是**还能用多久**、**多久没用了**，
 * 绝对时间挪到 title 里 —— 顺带把行宽压下来，窄屏不再横向滚。
 */
function rel(iso: string): string {
  const diff = new Date(iso).getTime() - Date.now()
  const abs = Math.abs(diff)
  const [unit, ms] =
    abs < 60_000 ? (['秒', 1000] as const)
    : abs < 3_600_000 ? (['分钟', 60_000] as const)
    : abs < DAY_MS ? (['小时', 3_600_000] as const)
    : (['天', DAY_MS] as const)
  const n = Math.max(1, Math.round(abs / ms))
  return diff >= 0 ? `还有 ${n} ${unit}` : `${n} ${unit}前`
}

/**
 * 管理自己的 API key：MCP 客户端（Claude Code 等）和脚本用它当 `Authorization: Bearer`。
 * 服务端只存哈希，key 本身只在生成那一刻显示一次；不要了随时吊销，立刻失效。
 */
export function ApiKeyDialog({ me, onClose }: { me: AuthMe; onClose: () => void }) {
  const qc = useQueryClient()
  const list = useApiKeys(true)
  const maxDays = ttlDays(me.api_keys?.max_ttl ?? '90d')
  const options = TTL_OPTIONS.filter((o) => ttlDays(o.value) <= maxDays)
  const [name, setName] = useState('claude-code')
  const [ttl, setTtl] = useState(options[options.length - 1]?.value ?? me.api_keys?.max_ttl ?? '90d')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [created, setCreated] = useState<ApiKeyCreated | null>(null)

  const refresh = () => qc.invalidateQueries({ queryKey: ['auth', 'keys'] })

  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      setCreated(await apiPost<ApiKeyCreated>('/auth/keys', { name: name.trim(), ttl }))
      await refresh()
    } catch (err) {
      setError((err as Error).message)
    } finally {
      setBusy(false)
    }
  }

  const revoke = async (k: ApiKeyInfo) => {
    setError(null)
    try {
      await apiDelete(`/auth/keys/${encodeURIComponent(k.id)}`)
      if (created?.id === k.id) setCreated(null)
      await refresh()
    } catch (err) {
      setError((err as Error).message)
    }
  }

  // 服务名由后端给（--mcp-name）：生产、UAT 各一套时名字必须不同，否则下面那条 remove
  // 会把另一套删掉
  const mcpName = created?.mcp_name ?? 'opdash'
  // add 碰上同名的会直接报 already exists，所以前面带一条 remove：没装过时它只在 stderr 说句找不到，
  // 不挡后面那条；装过就是换成这把新 key
  const mcpCommand = created
    ? `claude mcp remove ${mcpName} 2>/dev/null\nclaude mcp add --transport http ${mcpName} ${created.mcp_url} \\\n  --header "Authorization: Bearer ${created.key}"`
    : ''
  // Codex 不用命令行加远程服务，直接写 ~/.codex/config.toml
  const codexConfig = created
    ? `[mcp_servers.${mcpName}]\nurl = "${created.mcp_url}"\nhttp_headers = { Authorization = "Bearer ${created.key}" }`
    : ''
  const keys = list.data?.keys ?? []
  const live = keys.filter((k) => !k.expired).length
  const user = me.user

  return (
    // 焦点关在里面、背景不可点也不滚、Escape 关、关掉焦点还回「API key」那个按钮，
    // 全归 Base UI 的 Dialog（见 ModalPanel）；面板在异步 chunk 里，点开才加载
    <Suspense fallback={null}>
      <ModalPanel
        open
        onOpenChange={(next) => !next && onClose()}
        labelledBy="api-key-title"
        className="fixed inset-x-3 top-16 z-50 mx-auto flex max-h-[calc(100dvh-5rem)] max-w-2xl flex-col overflow-hidden rounded-lg border border-border bg-card shadow-xl md:inset-x-0 md:w-[44rem]"
      >
        <header className="flex shrink-0 items-start gap-2 border-b border-border px-4 py-3">
          <KeyRoundIcon className="mt-0.5 size-4 shrink-0 text-muted-fg" />
          <div className="min-w-0 flex-1">
            <h2 id="api-key-title" className="text-sm font-semibold">
              API 密钥 · 供 AI 助手与脚本使用
            </h2>
            {user && (
              <Hint text={user.email ?? undefined}>
                <p className="mt-0.5 truncate text-2xs text-muted-fg">
                  当前登录 <span className="font-medium text-fg">{user.name}</span> · 密钥归属于你本人，仅你自己可见
                </p>
              </Hint>
            )}
          </div>
          <Button variant="ghost" className="px-2" onClick={onClose} title="关闭 (Esc)">
            <XIcon className="size-4" />
          </Button>
        </header>

        <div className="flex min-h-0 flex-1 flex-col gap-4 overflow-x-hidden overflow-y-auto p-4">
          <p className="text-2xs leading-5 text-muted-fg">
            密钥代表你本人，访问权限与你登录后相同。Claude Code 等 MCP 客户端无法通过浏览器登录，需凭密钥接入{' '}
            <span className="mono">/mcp</span>；curl 与脚本也可将其用作 <span className="mono">Authorization: Bearer</span>。服务端仅保存哈希值，密钥只在生成时显示一次；不再使用时可随时吊销，吊销后立即失效。
          </p>

          <form onSubmit={submit} className="flex flex-wrap items-end gap-2 rounded-md border border-border bg-muted/40 p-3">
            <label className="flex min-w-40 flex-1 flex-col gap-1 text-2xs text-muted-fg">
              名称（仅作标识，便于日后区分机器或客户端）
              <Input
                value={name}
                onChange={(e) => setName(e.target.value)}
                maxLength={64}
                placeholder="claude-code"
                className="h-8 text-xs"
              />
            </label>
            <label className="flex flex-col gap-1 text-2xs text-muted-fg">
              有效期
              <Select value={ttl} onChange={(e) => setTtl(e.target.value)} className="h-8 text-xs">
                {options.map((o) => (
                  <option key={o.value} value={o.value}>
                    {o.label}
                  </option>
                ))}
                {!options.some((o) => o.value === ttl) && <option value={ttl}>{ttl}（上限）</option>}
              </Select>
            </label>
            <Button type="submit" variant="primary" size="sm" disabled={busy || !name.trim()}>
              {busy ? '生成中…' : '生成新密钥'}
            </Button>
          </form>

          {created && (
            <div className="flex flex-col gap-3 rounded-md border border-brand/50 bg-card p-3">
              <p className="text-xs leading-5">
                <span className="font-medium">请立即复制。</span>
                <span className="text-muted-fg">
                  「{created.name}」仅显示一次，关闭后无法再次查看；如有遗失，请吊销后重新生成。有效期 {created.expires_in}，将于{' '}
                  {fmt(created.expires_at)} 失效。
                </span>
              </p>
              <Secret label="API 密钥" value={created.key} />
              <Secret label="接入 Claude Code（Streamable HTTP，已接入的将改用此密钥）" value={mcpCommand} />
              <Secret label="接入 Codex（写入 ~/.codex/config.toml，替换已有的同名段落）" value={codexConfig} />
              <p className="text-2xs leading-5 text-muted-fg">
                其他 MCP 客户端请填写地址 <span className="mono">{created.mcp_url}</span>，请求头{' '}
                <span className="mono">Authorization: Bearer &lt;key&gt;</span>；已接入的客户端替换旧密钥即可。
              </p>
              <div className="flex justify-end">
                <Button size="xs" onClick={() => setCreated(null)}>
                  已复制，收起
                </Button>
              </div>
            </div>
          )}

          {error && (
            <p className="rounded-md border border-danger/40 bg-danger-soft px-3 py-2 text-xs text-danger">{error}</p>
          )}

          <section className="flex min-w-0 flex-col gap-1.5">
            <div className="flex items-center gap-2 text-xs text-muted-fg">
              <span className="font-medium text-fg">我的密钥</span>
              {keys.length > 0 && (
                <span>
                  {live} 个有效
                  {keys.length > live && ` · ${keys.length - live} 个已过期`}
                </span>
              )}
              {list.isFetching && <Spinner className="size-3" />}
            </div>
            {list.isError && <p className="text-xs text-danger">{(list.error as Error).message}</p>}
            {list.data && keys.length === 0 && (
              <p className="rounded-md border border-dashed border-border px-3 py-8 text-center text-xs text-muted-fg">
                暂无密钥，可在上方生成
              </p>
            )}
            {keys.length > 0 && (
              <div className="min-w-0 overflow-x-auto rounded-md border border-border">
                <table className="w-full min-w-[34rem] text-xs">
                  <thead className="bg-muted/60 text-left text-2xs text-muted-fg">
                    <tr>
                      <th className="px-2.5 py-1.5 font-medium">名称</th>
                      <th className="px-2.5 py-1.5 font-medium">key</th>
                      <th className="px-2.5 py-1.5 font-medium">到期</th>
                      <th className="hidden px-2.5 py-1.5 font-medium sm:table-cell">最近使用</th>
                      <th className="w-10 px-2.5 py-1.5" />
                    </tr>
                  </thead>
                  <tbody>
                    {keys.map((k) => (
                      <KeyRow key={k.id} k={k} highlight={created?.id === k.id} onRevoke={() => revoke(k)} />
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </section>
        </div>
      </ModalPanel>
    </Suspense>
  )
}

function KeyRow({ k, highlight, onRevoke }: { k: ApiKeyInfo; highlight: boolean; onRevoke: () => void }) {
  // 吊销分两步点，不弹 confirm：第一下变成「确认吊销」，几秒不点就复原
  const [arm, setArm] = useState(false)
  useEffect(() => {
    if (!arm) return
    const t = setTimeout(() => setArm(false), 4000)
    return () => clearTimeout(t)
  }, [arm])
  const soon = !k.expired && new Date(k.expires_at).getTime() - Date.now() < EXPIRING_SOON_DAYS * DAY_MS
  return (
    <tr className={highlight ? 'border-t border-border bg-brand/5' : 'row-hover border-t border-border'}>
      {/* 这一行四处解释原来都是原生 title。手机上没有 hover 就打不开，而这个对话框恰恰是
          「在手机上照着抄一条命令」的场景——全换成 Hint（触摸设备上点一下就出来） */}
      <td className="max-w-40 truncate px-2.5 py-1.5 font-medium">
        <Hint text={`创建于 ${fmt(k.created_at)}`}>
          <span className="block truncate">{k.name}</span>
        </Hint>
      </td>
      <td className="mono px-2.5 py-1.5 text-2xs text-muted-fg">
        <Hint text="密钥前缀；其余部分服务端不保存">
          <span>{k.prefix}…</span>
        </Hint>
      </td>
      <td className="px-2.5 py-1.5 whitespace-nowrap">
        <Hint text={`到期时间 ${fmt(k.expires_at)}`}>
          <span>
            {k.expired ?
              <Badge tone="danger">已过期</Badge>
            : soon ?
              <Badge tone="warn">{rel(k.expires_at)}</Badge>
            : <span className="text-muted-fg">{rel(k.expires_at)}</span>}
          </span>
        </Hint>
      </td>
      <td className="hidden px-2.5 py-1.5 whitespace-nowrap text-muted-fg sm:table-cell">
        <Hint text={k.last_used_at ? `最近使用 ${fmt(k.last_used_at)}` : '生成后尚未使用'}>
          <span>{k.last_used_at ? rel(k.last_used_at) : '未使用'}</span>
        </Hint>
      </td>
      <td className="px-1.5 py-1.5 text-right">
        <Button
          size="xs"
          variant={arm ? 'danger' : 'ghost'}
          className="px-1.5"
          onClick={() => (arm ? onRevoke() : setArm(true))}
          title={arm ? '再次点击以确认吊销' : '吊销后立即失效'}
        >
          <Trash2Icon className="size-3.5" />
          {arm && '确认'}
        </Button>
      </td>
    </tr>
  )
}

/**
 * 一段只显示这一次的东西（key 本身、两段接入配置），配一个复制按钮。
 *
 * 复制走公共的 `CopyButton`：以前这里自己管 `copied`，失败时一声不吭——而 key 关掉对话框就
 * 再也拿不到了，明文 http 上剪贴板又恰恰最可能被拦，正是最不该沉默的地方。
 *
 * `select-all` 是兜底：真复制不了，点一下代码块也能整段选中，自己按 Cmd+C。
 */
function Secret({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-center justify-between text-xs text-muted-fg">
        <span>{label}</span>
        <CopyButton text={value} title="复制" label="复制" className="h-7 rounded-md px-2.5 text-2xs hover:bg-muted" />
      </div>
      <pre className="mono max-h-32 overflow-auto rounded-md border border-border bg-muted px-3 py-2 text-2xs leading-5 break-all whitespace-pre-wrap select-all">
        {value}
      </pre>
    </div>
  )
}
