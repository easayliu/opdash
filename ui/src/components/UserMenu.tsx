import { useEffect } from 'react'
import { LogOutIcon, UserIcon } from 'lucide-react'
import { useAuthMe } from '@/api/queries'
import { redirectToLogin } from '@/api/client'
import { Button } from '@/components/ui'

/**
 * 顶栏右侧的当前用户：OIDC 模式显示用户名和退出；Basic / 不认证时什么都不画。
 * 页面打开时会话已过期（后端只对 HTML 导航跳登录，SPA 内部路由切换不会再经过后端）就主动跳去登录。
 */
export function UserMenu() {
  const { data } = useAuthMe()
  useEffect(() => {
    if (data?.mode === 'oidc' && !data.user && data.login_url) redirectToLogin(data.login_url)
  }, [data])
  if (data?.mode !== 'oidc' || !data.user) return null
  const { user, logout_url } = data
  return (
    <div className="flex items-center gap-1 border-l border-border pl-3">
      <span
        className="hidden max-w-44 items-center gap-1.5 truncate text-sm text-muted-fg sm:flex"
        title={user.email ?? user.name}
      >
        <UserIcon className="size-4 shrink-0" />
        <span className="truncate">{user.name}</span>
      </span>
      {logout_url && (
        <Button
          variant="ghost"
          className="px-2.5"
          title="退出登录"
          onClick={() => window.location.assign(logout_url)}
        >
          <LogOutIcon className="size-4" />
        </Button>
      )}
    </div>
  )
}
