import { useEffect, useState } from 'react'
import { KeyRoundIcon, LogOutIcon, UserIcon } from 'lucide-react'
import { useAuthMe } from '@/api/queries'
import { redirectToLogin } from '@/api/client'
import { Button, Hint } from '@/components/ui'
import { ApiKeyDialog } from '@/components/ApiKeyDialog'

/**
 * 顶栏右侧的当前用户：OIDC 模式显示用户名和退出；开了认证（OIDC 或 Basic）就有「API key」按钮，
 * 给 MCP 客户端 / 脚本生成凭证；不认证时什么都不画（不需要凭证）。
 * 页面打开时会话已过期（后端只对 HTML 导航跳登录，SPA 内部路由切换不会再经过后端）就主动跳去登录。
 */
export function UserMenu() {
  const { data } = useAuthMe()
  const [keys, setKeys] = useState(false)
  useEffect(() => {
    if (data?.mode === 'oidc' && !data.user && data.login_url) redirectToLogin(data.login_url)
  }, [data])
  if (!data || data.mode === 'none' || !data.user) return null
  const { user, logout_url } = data
  const oidc = data.mode === 'oidc'
  return (
    <div className="flex items-center gap-1 border-l border-border pl-3">
      {oidc && (
        <Hint text={user.email ?? user.name}>
          <span
            className="hidden max-w-44 items-center gap-1.5 truncate text-sm text-muted-fg sm:flex"
          >
            <UserIcon className="size-4 shrink-0" />
            <span className="truncate">{user.name}</span>
          </span>
        </Hint>
      )}
      {data.api_keys && (
        <Button variant="ghost" className="px-2.5" title="API key" onClick={() => setKeys(true)}>
          <KeyRoundIcon className="size-4" />
        </Button>
      )}
      {oidc && logout_url && (
        <Button
          variant="ghost"
          className="px-2.5"
          title="退出登录"
          onClick={() => window.location.assign(logout_url)}
        >
          <LogOutIcon className="size-4" />
        </Button>
      )}
      {keys && <ApiKeyDialog me={data} onClose={() => setKeys(false)} />}
    </div>
  )
}
