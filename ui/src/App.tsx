import { useState, type FormEvent } from 'react'
import { NavLink, Navigate, Route, Routes, useNavigate } from 'react-router'
import { ActivityIcon, GitBranchIcon, ScrollTextIcon, SearchIcon } from 'lucide-react'
import { TimeRangePicker } from '@/components/TimeRangePicker'
import { ThemeSwitcher } from '@/components/ThemeSwitcher'
import { UserMenu } from '@/components/UserMenu'
import { Input } from '@/components/ui'
import { useRangeMemory } from '@/lib/url-state'
import { cn, isHexId } from '@/lib/utils'
import { LogsPage } from '@/pages/LogsPage'
import { TracesPage } from '@/pages/TracesPage'
import { TraceDetailPage } from '@/pages/TraceDetailPage'
import { ServicesPage } from '@/pages/ServicesPage'
import { ServiceDetailPage } from '@/pages/ServiceDetailPage'

const NAV = [
  { to: '/logs', label: '日志', icon: ScrollTextIcon },
  { to: '/traces', label: '链路', icon: GitBranchIcon },
  { to: '/services', label: '服务', icon: ActivityIcon },
]

/** 顶栏的直达框：粘一个 trace id 直接开链路；不是 id 就当关键字去搜日志。 */
function QuickJump() {
  const navigate = useNavigate()
  const [value, setValue] = useState('')
  const submit = (e: FormEvent) => {
    e.preventDefault()
    const v = value.trim()
    if (!v) return
    if (isHexId(v, 32)) navigate(`/traces/${v.toLowerCase()}`)
    else if (isHexId(v, 16)) navigate(`/logs?span_id=${v.toLowerCase()}&range=7d`)
    else navigate(`/logs?q=${encodeURIComponent(v)}`)
    setValue('')
  }
  return (
    <form onSubmit={submit} className="relative hidden w-80 md:block">
      <SearchIcon className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-muted-fg" />
      <Input
        value={value}
        onChange={(e) => setValue(e.target.value)}
        placeholder="trace id 直达，或输入关键字搜日志"
        className="pl-9"
        aria-label="快速跳转"
      />
    </form>
  )
}

/**
 * 路由出口。地址上没带时间范围时先补上记住的那一个再渲染页面——
 * 补参数走 replace，不会在历史里多留一条。
 */
function AppRoutes() {
  const redirect = useRangeMemory()
  if (redirect) return <Navigate to={redirect} replace />
  return (
    <Routes>
      <Route path="/" element={<Navigate to="/logs" replace />} />
      <Route path="/logs" element={<LogsPage />} />
      <Route path="/traces" element={<TracesPage />} />
      <Route path="/traces/:traceId" element={<TraceDetailPage />} />
      <Route path="/services" element={<ServicesPage />} />
      <Route path="/services/:name" element={<ServiceDetailPage />} />
      <Route path="*" element={<Navigate to="/logs" replace />} />
    </Routes>
  )
}

export default function App() {
  return (
    // 外壳钉在视口高度，页面各自在内部滚（表头 sticky、瀑布图 / 日志分栏滚动、右侧 span 面板都靠这个），
    // 顶栏和页脚固定；没自带滚动区的页面退回到 main 滚
    <div className="flex h-dvh flex-col">
      {/* 手机上导航页签换到第二行，第一行只留 logo 和时间 / 主题 / 用户 */}
      <header className="z-20 shrink-0 border-b border-border bg-card">
        <div className="flex flex-wrap items-stretch gap-x-4 px-3 md:h-14 md:flex-nowrap md:px-4">
          <NavLink to="/logs" className="flex h-12 items-center gap-2 pr-2 md:h-auto md:pr-4">
            <span className="flex size-7 items-center justify-center rounded-md bg-brand text-white">
              <ActivityIcon className="size-4" />
            </span>
            <span className="text-base font-semibold tracking-tight">opdash</span>
          </NavLink>
          <nav className="order-last -mx-3 flex h-10 w-[calc(100%+1.5rem)] items-stretch border-t border-border md:order-none md:mx-0 md:h-auto md:w-auto md:gap-1 md:border-t-0">
            {NAV.map(({ to, label, icon: Icon }) => (
              <NavLink
                key={to}
                to={to}
                className={({ isActive }) =>
                  cn(
                    'cf-tab flex flex-1 items-center justify-center gap-1.5 px-3 text-sm font-medium text-muted-fg hover:text-fg md:flex-none',
                    isActive && 'text-fg',
                  )
                }
              >
                {({ isActive }) => (
                  <span className="cf-tab flex h-full items-center gap-1.5" data-active={isActive ? 'true' : undefined}>
                    <Icon className="size-4" />
                    {label}
                  </span>
                )}
              </NavLink>
            ))}
          </nav>
          <div className="ml-auto flex min-w-0 items-center gap-1 py-2 md:gap-2 md:py-2.5">
            <QuickJump />
            <TimeRangePicker />
            <ThemeSwitcher />
            <UserMenu />
          </div>
        </div>
      </header>
      <main className="flex min-h-0 flex-1 flex-col overflow-auto">
        <AppRoutes />
      </main>
      <footer className="hidden h-8 shrink-0 items-center justify-end gap-3 border-t border-border px-4 text-2xs text-muted-fg md:flex">
        <span>opdash v{__APP_VERSION__}</span>
      </footer>
    </div>
  )
}
