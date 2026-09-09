import { useState, type FormEvent } from 'react'
import { NavLink, Navigate, Route, Routes, useNavigate } from 'react-router'
import { ActivityIcon, GitBranchIcon, ScrollTextIcon, SearchIcon } from 'lucide-react'
import { TimeRangePicker } from '@/components/TimeRangePicker'
import { ThemeSwitcher } from '@/components/ThemeSwitcher'
import { UserMenu } from '@/components/UserMenu'
import { Input } from '@/components/ui'
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

export default function App() {
  return (
    <div className="flex min-h-dvh flex-col">
      <header className="sticky top-0 z-20 border-b border-border bg-card">
        <div className="flex h-14 items-stretch gap-4 px-4">
          <NavLink to="/logs" className="flex items-center gap-2 pr-4">
            <span className="flex size-7 items-center justify-center rounded-md bg-brand text-white">
              <ActivityIcon className="size-4" />
            </span>
            <span className="text-base font-semibold tracking-tight">opdash</span>
          </NavLink>
          <nav className="flex items-stretch gap-1">
            {NAV.map(({ to, label, icon: Icon }) => (
              <NavLink
                key={to}
                to={to}
                className={({ isActive }) =>
                  cn(
                    'cf-tab flex items-center gap-1.5 px-3 text-sm font-medium text-muted-fg hover:text-fg',
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
          <div className="ml-auto flex items-center gap-2 py-2.5">
            <QuickJump />
            <TimeRangePicker />
            <ThemeSwitcher />
            <UserMenu />
          </div>
        </div>
      </header>
      <main className="flex min-h-0 flex-1 flex-col">
        <Routes>
          <Route path="/" element={<Navigate to="/logs" replace />} />
          <Route path="/logs" element={<LogsPage />} />
          <Route path="/traces" element={<TracesPage />} />
          <Route path="/traces/:traceId" element={<TraceDetailPage />} />
          <Route path="/services" element={<ServicesPage />} />
          <Route path="/services/:name" element={<ServiceDetailPage />} />
          <Route path="*" element={<Navigate to="/logs" replace />} />
        </Routes>
      </main>
      <footer className="flex h-8 items-center justify-end gap-3 border-t border-border px-4 text-2xs text-muted-fg">
        <span>opdash v{__APP_VERSION__}</span>
      </footer>
    </div>
  )
}
