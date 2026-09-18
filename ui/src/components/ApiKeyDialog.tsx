import { useEffect, useState } from 'react'
import { CheckIcon, CopyIcon, KeyRoundIcon, XIcon } from 'lucide-react'
import { apiPost } from '@/api/client'
import type { ApiKeyCreated, AuthMe } from '@/api/types'
import { Button, Input, Select } from '@/components/ui'
import { copyText } from '@/lib/utils'

/** 有效期的几档。上限来自后端的 `--api-key-ttl`，比上限长的档不显示 */
const TTL_OPTIONS = [
  { value: '7d', label: '7 天' },
  { value: '30d', label: '30 天' },
  { value: '90d', label: '90 天' },
  { value: '180d', label: '180 天' },
  { value: '365d', label: '1 年' },
]

function ttlDays(s: string): number {
  const m = /^(\d+)(d|h|m)$/.exec(s.trim())
  if (!m) return Infinity
  const n = Number(m[1])
  return m[2] === 'd' ? n : m[2] === 'h' ? n / 24 : n / 1440
}

/**
 * 给自己生成一把 API key：MCP 客户端（Claude Code 等）和脚本用它当 `Authorization: Bearer`。
 * key 是签名 token，服务端不存、只显示这一次；关掉就再也看不到，丢了重新生成一把。
 */
export function ApiKeyDialog({ me, onClose }: { me: AuthMe; onClose: () => void }) {
  const maxDays = ttlDays(me.api_keys?.max_ttl ?? '90d')
  const options = TTL_OPTIONS.filter((o) => ttlDays(o.value) <= maxDays)
  const [name, setName] = useState('claude-code')
  const [ttl, setTtl] = useState(options[options.length - 1]?.value ?? me.api_keys?.max_ttl ?? '90d')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [created, setCreated] = useState<ApiKeyCreated | null>(null)

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [onClose])

  const submit = async (e: React.FormEvent) => {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      setCreated(await apiPost<ApiKeyCreated>('/auth/keys', { name, ttl }))
    } catch (err) {
      setError((err as Error).message)
    } finally {
      setBusy(false)
    }
  }

  const mcpCommand = created
    ? `claude mcp add --transport http opdash ${created.mcp_url} \\\n  --header "Authorization: Bearer ${created.key}"`
    : ''

  return (
    <>
      <div className="fixed inset-0 z-40 bg-black/30" onClick={onClose} />
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="api-key-title"
        className="fixed inset-x-3 top-16 z-50 mx-auto max-w-xl rounded-lg border border-border bg-card shadow-xl md:inset-x-auto md:left-1/2 md:w-[36rem] md:-translate-x-1/2"
      >
        <header className="flex h-12 items-center gap-2 border-b border-border px-4">
          <KeyRoundIcon className="size-4 text-muted-fg" />
          <h2 id="api-key-title" className="flex-1 text-sm font-semibold">
            API key · 给 AI 助手和脚本用
          </h2>
          <Button variant="ghost" className="px-2" onClick={onClose} title="关闭 (Esc)">
            <XIcon className="size-4" />
          </Button>
        </header>

        {!created ? (
          <form onSubmit={submit} className="flex flex-col gap-4 p-4">
            <p className="text-xs leading-5 text-muted-fg">
              key 代表你本人，能看的和你登录后能看的一样。Claude Code 这类 MCP 客户端不会跳浏览器登录，
              所以要靠 key 接 <span className="mono">/mcp</span>；curl / 脚本也可以拿它当 <span className="mono">Authorization: Bearer</span>。
            </p>
            <label className="flex flex-col gap-1 text-xs text-muted-fg">
              名字（只是标签，日志里认它）
              <Input value={name} onChange={(e) => setName(e.target.value)} maxLength={64} placeholder="claude-code" />
            </label>
            <label className="flex flex-col gap-1 text-xs text-muted-fg">
              有效期（到期自动失效；服务端不存 key，没法单个吊销，别给太长）
              <Select value={ttl} onChange={(e) => setTtl(e.target.value)}>
                {options.map((o) => (
                  <option key={o.value} value={o.value}>
                    {o.label}
                  </option>
                ))}
                {!options.some((o) => o.value === ttl) && <option value={ttl}>{ttl}（上限）</option>}
              </Select>
            </label>
            {me.api_keys && !me.api_keys.persistent && (
              <p className="rounded-md border border-warn/40 bg-warn-soft px-3 py-2 text-xs text-warn">
                服务端没配 <span className="mono">--session-secret</span>，签名密钥是随机的：opdash 一重启这把 key 就失效。
              </p>
            )}
            {error && <p className="text-xs text-danger">{error}</p>}
            <div className="flex justify-end gap-2">
              <Button onClick={onClose}>取消</Button>
              <Button type="submit" variant="primary" disabled={busy}>
                {busy ? '生成中…' : '生成 key'}
              </Button>
            </div>
          </form>
        ) : (
          <div className="flex flex-col gap-4 p-4">
            <p className="text-xs leading-5 text-muted-fg">
              <span className="font-medium text-fg">现在就复制。</span>这把 key 只显示这一次，关掉后看不到了；丢了重新生成一把。
              有效期 {created.expires_in}，到 {new Date(created.expires_at).toLocaleString()} 失效。
            </p>
            <Secret label="API key" value={created.key} />
            <Secret label="接入 Claude Code（Streamable HTTP）" value={mcpCommand} />
            <p className="text-2xs leading-5 text-muted-fg">
              其它 MCP 客户端填地址 <span className="mono">{created.mcp_url}</span>，请求头{' '}
              <span className="mono">Authorization: Bearer &lt;key&gt;</span>。curl 也一样：
              <span className="mono"> curl -H "Authorization: Bearer …" {created.mcp_url.replace(/\/mcp$/, '')}/api/meta</span>
            </p>
            <div className="flex justify-end">
              <Button variant="primary" onClick={onClose}>
                复制好了，关闭
              </Button>
            </div>
          </div>
        )}
      </div>
    </>
  )
}

function Secret({ label, value }: { label: string; value: string }) {
  const [copied, setCopied] = useState(false)
  const copy = async () => {
    if (await copyText(value)) {
      setCopied(true)
      setTimeout(() => setCopied(false), 1500)
    }
  }
  return (
    <div className="flex flex-col gap-1">
      <div className="flex items-center justify-between text-xs text-muted-fg">
        <span>{label}</span>
        <Button size="xs" variant="ghost" onClick={copy} title="复制">
          {copied ? <CheckIcon className="size-3.5 text-ok" /> : <CopyIcon className="size-3.5" />}
          {copied ? '已复制' : '复制'}
        </Button>
      </div>
      <pre className="mono max-h-32 overflow-auto rounded-md border border-border bg-muted px-3 py-2 text-2xs leading-5 break-all whitespace-pre-wrap select-all">
        {value}
      </pre>
    </div>
  )
}
