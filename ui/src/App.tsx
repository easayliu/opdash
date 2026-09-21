import { useEffect, useRef, useState, type FormEvent } from 'react'
import { NavLink, Navigate, Route, Routes, useLocation, useNavigate, useSearchParams } from 'react-router'
import { ActivityIcon, AlertTriangleIcon, ChartLineIcon, GitBranchIcon, ScrollTextIcon, SearchIcon } from 'lucide-react'
import { AnimatePresence, LazyMotion, MotionConfig } from 'motion/react'
import * as m from 'motion/react-m'
import { ErrorBoundary } from '@/components/ErrorBoundary'
import { TimeRangePicker } from '@/components/TimeRangePicker'
import { SavedQueries } from '@/components/SavedQueries'
import { ThemeSwitcher } from '@/components/ThemeSwitcher'
import { UserMenu } from '@/components/UserMenu'
import { Input } from '@/components/ui'
import { useMeta } from '@/api/queries'
import { useRangeMemory } from '@/lib/url-state'
import { PAGE, TRANSITION } from '@/lib/motion'
import { cn, isHexId } from '@/lib/utils'
import { LogsPage } from '@/pages/LogsPage'
import { TracesPage } from '@/pages/TracesPage'
import { TraceDetailPage } from '@/pages/TraceDetailPage'
import { ServicesPage } from '@/pages/ServicesPage'
import { ErrorsPage } from '@/pages/ErrorsPage'
import { ServiceDetailPage } from '@/pages/ServiceDetailPage'
import { MetricsPage } from '@/pages/MetricsPage'

// 服务总览排第一、也是首页：打开先看「谁不对」，再去翻它的日志 / 链路 / 指标
const NAV = [
  { to: '/services', label: '服务', icon: ActivityIcon },
  // 错误紧挨着服务：总览说「谁不对」，这一页说「不对在哪一句报错上」
  { to: '/errors', label: '错误', icon: AlertTriangleIcon },
  { to: '/logs', label: '日志', icon: ScrollTextIcon },
  { to: '/traces', label: '链路', icon: GitBranchIcon },
  { to: '/metrics', label: '指标', icon: ChartLineIcon, needs: 'metrics' as const },
]

/** motion 的功能包异步加载（官方推荐的那条路，见 `@/lib/motion-features` 为什么要单独一个模块） */
const loadMotionFeatures = () => import('@/lib/motion-features').then((mod) => mod.default)

/**
 * 粘 trace id 直达时没有时刻可用，拿当前页面时间范围的中点当猜测。
 *
 * 链路详情按这个中心点从窄往宽探窗口（见后端 `DETAIL_PROBE_WINDOWS`），猜中能少读两个数量级；
 * 猜错了前几档空查、最后一档不限时间兜回来，多读约 27%，结果一样全。范围本身就宽到没法当
 * 提示的（超过一天）就不猜，直接让它走兜底那档。
 */
function guessAt(params: URLSearchParams): number | undefined {
  const from = Number(params.get('from'))
  const to = Number(params.get('to'))
  if (!from || !to || to <= from || to - from > 24 * 3_600_000) return undefined
  return Math.round((from + to) / 2)
}

/** 顶栏的直达框：粘一个 trace id 直接开链路；不是 id 就当关键字去搜日志。 */
function QuickJump() {
  const navigate = useNavigate()
  const [params] = useSearchParams()
  const [value, setValue] = useState('')
  const submit = (e: FormEvent) => {
    e.preventDefault()
    const v = value.trim()
    if (!v) return
    if (isHexId(v, 32)) {
      const at = guessAt(params)
      navigate(`/traces/${v.toLowerCase()}${at ? `?at=${at}` : ''}`)
    } else if (isHexId(v, 16)) navigate(`/logs?span_id=${v.toLowerCase()}&range=7d`)
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
 * 换页之后把焦点收到主内容上。
 *
 * 单页应用换页时浏览器什么都不做：焦点还留在刚点的那个链接上（它已经不在页面上了，于是掉回
 * body），读屏不会重新播报，键盘用户接着按 Tab 是从页面最顶上重来。把焦点放进 `<main>`
 * （它有 `tabIndex={-1}`，专为接程序化焦点），读屏会念一遍新的主区域，Tab 也从内容开始。
 *
 * 第一次渲染不抢焦点——那会儿人可能正在地址栏或者刚打开标签页。`preventScroll` 是因为 main
 * 本身就是滚动容器，聚焦它会把刚恢复的滚动位置顶掉。
 */
function useFocusMainOnNav(pathname: string): void {
  const first = useRef(true)
  useEffect(() => {
    if (first.current) {
      first.current = false
      return
    }
    document.getElementById('main')?.focus({ preventScroll: true })
  }, [pathname])
}

/**
 * 路由出口。地址上没带时间范围时先补上记住的那一个再渲染页面——
 * 补参数走 replace，不会在历史里多留一条。
 */
function AppRoutes() {
  const redirect = useRangeMemory()
  // 补时间范围是内部重定向，得把 state 原样带过去——面包屑的「来处」就放在里面，
  // 丢了的话从错误分组点进链路详情就没有返回按钮了（`traceHref` 不带 range，必走这条重定向）
  const location = useLocation()
  useFocusMainOnNav(location.pathname)
  if (redirect) return <Navigate to={redirect} replace state={location.state} />
  return (
    /*
     * 切页签时整页过一次场（见 `PAGE`）。`mode="wait"` 是官方给页面过渡的那档：旧页先淡出，
     * 新页再进来，两页不会同时叠在一起——同时叠的话滚动容器里会短暂出现两份内容。
     *
     * `location` 要显式传给 `Routes`：不传的话旧页在退场那 120ms 里会被立刻换成新页的内容，
     * 淡出的就不是你刚离开的那一页了。`initial={false}` 让冷启动不播——那会儿页面上只有一个
     * 转圈，淡入一个转圈没有意义。
     */
    <AnimatePresence mode="wait" initial={false}>
      <m.div key={location.pathname} {...PAGE} className="flex min-h-0 flex-1 flex-col">
        {/* 崩的只是内容区：顶栏、时间范围、页签都还在，换个页签就能接着用。
            这层跟着 `key={pathname}` 一起重挂，所以换页时错误状态自动清掉 */}
        <ErrorBoundary>
          <Routes location={location}>
            <Route path="/" element={<Navigate to="/services" replace />} />
            <Route path="/logs" element={<LogsPage />} />
            <Route path="/traces" element={<TracesPage />} />
            <Route path="/traces/:traceId" element={<TraceDetailPage />} />
            <Route path="/metrics" element={<MetricsPage />} />
            <Route path="/errors" element={<ErrorsPage />} />
            <Route path="/services" element={<ServicesPage />} />
            <Route path="/services/:name" element={<ServiceDetailPage />} />
            <Route path="*" element={<Navigate to="/services" replace />} />
          </Routes>
        </ErrorBoundary>
      </m.div>
    </AnimatePresence>
  )
}

export default function App() {
  // 没部署 metricpipe（指标表不存在）就不显示指标页签，点进去也只会看到一句「未启用」
  const meta = useMeta()
  const nav = NAV.filter((n) => n.needs !== 'metrics' || meta.data?.metrics)
  return (
    // 动效：功能包异步加载（`motion-features` 单独一个 chunk），口径全站一份见 `@/lib/motion`；
    // strict 会拦住写成 `motion.div` 的地方——那样等于把整包同步拉进首屏
    <LazyMotion features={loadMotionFeatures} strict>
      <MotionConfig transition={TRANSITION} reducedMotion="user">
        {/* 外壳钉在视口高度，页面各自在内部滚（表头 sticky、瀑布图 / 日志分栏滚动、右侧 span 面板
            都靠这个），顶栏和页脚固定；没自带滚动区的页面退回到 main 滚 */}
        <div className="flex h-dvh flex-col">
      {/* 平时看不见，Tab 第一下才冒出来：不给它的话，键盘用户每切一个页面都要把顶栏
          的六个页签和时间 / 主题 / 用户挨个 Tab 一遍才摸得到内容 */}
      <a
        href="#main"
        className="sr-only focus:not-sr-only focus:absolute focus:top-2 focus:left-2 focus:z-50 focus:rounded-md focus:border focus:border-border focus:bg-card focus:px-3 focus:py-2 focus:text-sm focus:shadow-md"
      >
        跳到主内容
      </a>
      {/* 手机上导航页签换到第二行，第一行只留 logo 和时间 / 主题 / 用户 */}
      <header className="z-20 shrink-0 border-b border-border bg-card">
        <div className="flex flex-wrap items-stretch gap-x-4 px-3 md:h-14 md:flex-nowrap md:px-4">
          <NavLink to="/services" className="flex h-12 items-center gap-2 pr-2 md:h-auto md:pr-4">
            <span className="flex size-7 items-center justify-center rounded-md bg-brand text-white">
              <ActivityIcon className="size-4" />
            </span>
            <span className="text-base font-semibold tracking-tight">opdash</span>
          </NavLink>
          <nav className="order-last -mx-3 flex h-10 w-[calc(100%+1.5rem)] items-stretch border-t border-border md:order-none md:mx-0 md:h-auto md:w-auto md:gap-1 md:border-t-0">
            {nav.map(({ to, label, icon: Icon }) => (
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
            <SavedQueries />
            <TimeRangePicker />
            <ThemeSwitcher />
            <UserMenu />
          </div>
        </div>
      </header>
      <main id="main" tabIndex={-1} className="flex min-h-0 flex-1 flex-col overflow-auto">
        <AppRoutes />
      </main>
      <footer className="hidden h-8 shrink-0 items-center justify-end gap-3 border-t border-border px-4 text-2xs text-muted-fg md:flex">
        <span>opdash v{__APP_VERSION__}</span>
      </footer>
        </div>
      </MotionConfig>
    </LazyMotion>
  )
}
