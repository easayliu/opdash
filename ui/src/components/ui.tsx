/** 几个够用的基础控件。内部工具，不上组件库，样式全靠 Tailwind。 */
import {
  Children,
  Suspense,
  cloneElement,
  isValidElement,
  forwardRef,
  lazy,
  useCallback,
  useId,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ButtonHTMLAttributes,
  type InputHTMLAttributes,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactElement,
  type ReactNode,
  type SelectHTMLAttributes,
} from 'react'
import { CheckIcon, ChevronDownIcon, CopyIcon, FilterIcon, InfoIcon, Loader2Icon, SearchIcon } from 'lucide-react'
import type { Side } from '@/components/base-ui'
import { useIsMobile } from '@/lib/media'
import { cn, copyText } from '@/lib/utils'

/** 下拉一次最多画多少行：服务 / 接口的候选能到几百，全画出来白费力气，让人接着敲 */
const MAX_COMBO_ROWS = 200
/** PageUp / PageDown 一次走几行 */
const PAGE_ROWS = 10
/** 菜单最宽多少 px（值可能很长，比触发按钮宽），也用来判断要不要往左展开 */
const COMBO_MENU_W = 420

export interface ButtonLook {
  variant?: 'default' | 'primary' | 'ghost' | 'danger'
  size?: 'sm' | 'md' | 'xs'
}

type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> & ButtonLook & { active?: boolean }

/**
 * 按钮的那身皮，单独拆出来给**链接**用。
 *
 * `<a>` 是交互内容，HTML 的内容模型不允许它里面再放 `<button>`——`<Link><Button/></Link>` 这种
 * 写法读屏会念出两层可点的东西，键盘上也说不清该按 Enter 还是空格。跳转就该是一个 `<a>`，
 * 长得像按钮而已，所以把类名拿出来挂在链接上，别再套一个按钮进去。
 */
export function buttonClass({ variant = 'default', size = 'md' }: ButtonLook = {}, className?: string): string {
  return cn(
    'inline-flex shrink-0 cursor-pointer items-center justify-center gap-1.5 whitespace-nowrap rounded-md border font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand/60 disabled:cursor-not-allowed disabled:opacity-50',
    size === 'md' && 'h-9 px-3.5 text-sm',
    size === 'sm' && 'h-8 px-3 text-xs',
    size === 'xs' && 'h-7 px-2.5 text-2xs',
    // Cloudflare：次级按钮是浅灰面 + hail 描边，主按钮是品牌橙；选中态用 marine 蓝
    variant === 'default' &&
      'border-input bg-face text-fg hover:bg-face-hover data-[active=true]:border-accent data-[active=true]:bg-accent-soft data-[active=true]:text-accent',
    variant === 'primary' && 'border-brand bg-brand text-brand-fg hover:border-brand-strong hover:bg-brand-strong',
    variant === 'ghost' &&
      'border-transparent bg-transparent text-muted-fg hover:bg-muted hover:text-fg data-[active=true]:bg-accent-soft data-[active=true]:text-accent',
    variant === 'danger' && 'border-danger/40 bg-card text-danger hover:bg-danger-soft',
    className,
  )
}

/** 子元素里有没有文字。只有图标的按钮要拿提示当无障碍名字，有文字的不能覆盖 */
function hasTextChild(children: ReactNode): boolean {
  return Children.toArray(children).some((c) => typeof c === 'string' || typeof c === 'number')
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  { className, variant = 'default', size = 'md', active, type = 'button', title, children, ...props },
  ref,
) {
  const btn = (
    <button
      ref={ref}
      type={type}
      // 提示和无障碍名字都交给 Hint；原生 title 留着会跟气泡一起冒出来，同一句话显示两遍
      className={buttonClass({ variant, size }, className)}
      data-active={active ? 'true' : undefined}
      /**
       * 「按下去了」这件事**只有颜色在说**，读屏听到的和没选中时一模一样——所以 `active` 同时
       * 落成 `aria-pressed`（WAI-ARIA 的 Toggle Button：名字、角色之外还得有「值」）。
       *
       * 自己带了 `aria-expanded` 的不算：那是开合浮层的按钮（收藏、更多筛选），它的「值」是
       * 展开与否，`active` 在那儿只是顺带把图标点亮，再报一个 pressed 反而互相打架。
       */
      aria-pressed={active !== undefined && props['aria-expanded'] === undefined ? active : undefined}
      {...props}
    >
      {children}
    </button>
  )
  return title ? (
    <Hint text={title} asChild>
      {btn}
    </Hint>
  ) : (
    btn
  )
})

export const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement>>(function Input(
  { className, ...props },
  ref,
) {
  return (
    <input
      ref={ref}
      className={cn(
        'h-9 w-full min-w-0 rounded-md border border-input bg-card px-3 text-sm text-fg placeholder:text-muted-fg/70 focus:border-brand focus:ring-2 focus:ring-brand/25 focus:outline-none disabled:opacity-50',
        className,
      )}
      {...props}
    />
  )
})

export const Select = forwardRef<HTMLSelectElement, SelectHTMLAttributes<HTMLSelectElement>>(function Select(
  { className, ...props },
  ref,
) {
  return (
    <select
      ref={ref}
      className={cn(
        'h-9 rounded-md border border-input bg-card px-2.5 text-sm text-fg focus:border-brand focus:ring-2 focus:ring-brand/25 focus:outline-none disabled:opacity-50',
        className,
      )}
      {...props}
    />
  )
})

/**
 * 正文里的强调链接：日志跳链路、链路跳日志、空状态里的「换个查法」全用它。
 *
 * 以前这串类名在 8 个文件里各写各的，共 15 份，谁也没带焦点环——键盘 tab 过去完全看不出停在
 * 哪。Kumo 有个 Link 组件，但它的正事是适配路由库（LinkProvider），我们直接用 react-router 的
 * `Link`，所以这里只要一份类名，挂在 `Link` 或 `button` 上都行。
 *
 * 注意跟 `hover:text-accent` 不是一回事：那个是平时用正文色、鼠标扫上去才变色，用在日志正文
 * 和 logger 这种「整行都是内容、不想画成一片蓝」的地方。
 */
export const linkClass =
  'rounded-sm text-accent focus-visible:outline-none hover:underline focus-visible:ring-2 focus-visible:ring-brand/60'

interface HintChildProps {
  children?: ReactNode
  className?: string
  'aria-label'?: string
  'aria-describedby'?: string
  ref?: React.Ref<HTMLElement>
  onPointerEnter?: (e: React.PointerEvent<HTMLElement>) => void
  onPointerLeave?: (e: React.PointerEvent<HTMLElement>) => void
  onPointerDown?: (e: React.PointerEvent<HTMLElement>) => void
  onFocus?: (e: React.FocusEvent<HTMLElement>) => void
  onBlur?: (e: React.FocusEvent<HTMLElement>) => void
  onClick?: (e: React.MouseEvent<HTMLElement>) => void
}

/**
 * 鼠标悬停多久才浮出来：扫过去不弹，停下来才弹。原来用 Base UI 的默认值 600ms，停在上面要干等
 * 半秒多，嫌慢；300ms 仍足以挡住鼠标一路扫过满页格子时的乱弹。带 ⓘ 的标签不等（见 InfoHint）
 */
const HINT_DELAY_MS = 300
/**
 * 刚收起一个提示之后多久之内，移到下一个就不再等：连着看几处说明时，每一处都重新等一遍很磨人。
 * 和常见 tooltip 的「预热」一样，只在短时间内有效，过后又回到正常的延迟
 */
const HINT_WARM_MS = 500
let lastHintClosedAt = 0

/** 键盘聚焦才立即浮出，鼠标点出来的焦点不算——否则每点一次按钮都弹一下 */
function focusVisible(el: Element): boolean {
  try {
    return el.matches(':focus-visible')
  } catch {
    return true
  }
}

/** 先调子元素自己的处理函数，再调我们的。克隆时直接覆盖会把子元素原有的 onClick 之类吞掉 */
function chain<E>(own: ((e: E) => void) | undefined, mine: ((e: E) => void) | undefined): ((e: E) => void) | undefined {
  if (!mine) return own
  return (e) => {
    own?.(e)
    mine(e)
  }
}

type HintHandlers = Pick<HintChildProps, 'onPointerEnter' | 'onPointerLeave' | 'onPointerDown' | 'onFocus' | 'onBlur' | 'onClick'>

/**
 * Base UI 那一坨（压完 43 kB）切在单独的 chunk 里，首屏不加载——见 `./base-ui`。
 *
 * 光切不预取的话，第一次悬停要等一次网络往返才看得到气泡。所以首屏画完之后趁空闲捎带取回来：
 * 不占关键路径，等人真去悬停或点击时基本已经就位。（没取回来也不影响按钮本身：触发器始终是
 * 原来那个节点，点击照常生效，只是气泡晚一点出来。）
 */
/**
 * 取回之后把模块记下来。`lazy` 组件哪怕文件早已在缓存里，第一次渲染也要先挂起一次，而 React
 * 挂起后重新显示内容有约 300ms 的节流——页面上第一次悬停提示因此要干等三百多毫秒，之后才快。
 * 模块到手之后，气泡直接用模块里的组件渲染，不再经过 `lazy`（见 Hint 里的 `Popup`）
 */
let baseUi: typeof import('@/components/base-ui') | null = null
const loadBaseUi = () => import('@/components/base-ui').then((m) => (baseUi = m))

export const HintPopup = lazy(() => loadBaseUi().then((m) => ({ default: m.HintPopup })))
export const PopoverPanel = lazy(() => loadBaseUi().then((m) => ({ default: m.PopoverPanel })))
export const ModalPanel = lazy(() => loadBaseUi().then((m) => ({ default: m.ModalPanel })))

export function prefetchBaseUi(): void {
  const load = () => void loadBaseUi()
  if ('requestIdleCallback' in window) window.requestIdleCallback(load, { timeout: 3_000 })
  else setTimeout(load, 1_000)
}

/**
 * 一句解释，鼠标悬停或聚焦时浮出来；触摸设备上点一下出来。
 *
 * 原生 `title` 在手机上根本打不开——没有 hover 就没有 tooltip，而这个项目是管手机的。全站有两百
 * 多处 `title`，短标签（「复制」「展开」）留着无妨，真正写着「为什么」的那些得换成这个。
 *
 * 用 Base UI（Cloudflare 的 Kumo 就建在它上面）。触摸设备上 tooltip 按惯例不响应点击，所以那边
 * 换成 popover：同样的气泡，点一下开、点外面关。
 *
 * 触发器就是传进来的那个元素本身，不占额外的 tab 位；解释怎么给到读屏见下面 `described`。
 */
export function Hint({
  text,
  children,
  side = 'top',
  className,
  asChild,
  delay = HINT_DELAY_MS,
}: {
  text: ReactNode
  children: ReactNode
  side?: Side
  className?: string
  /** 悬停多少毫秒后浮出。默认 300；人主动来看说明的地方（ⓘ）传 0 */
  delay?: number
  /**
   * 子元素本身就是触发器（按钮、链接这些），别再套一层 span：套了就是可聚焦的东西里面还嵌一个
   * 可聚焦的东西，tab 要按两下才过得去，读屏也会念两遍。
   */
  asChild?: boolean
}) {
  const isMobile = useIsMobile()
  const descId = useId()
  /**
   * 只有图标的按钮和链接，原来是靠 `title` 当无障碍名字的（浏览器会拿 title 兜底）。换成气泡之后
   * 这个兜底就没了：Base UI 给触发器挂的是 `aria-describedby`，那是「描述」不是「名字」，读屏会
   * 念成「按钮」三个字。所以子元素里没有文字时，把提示同时当名字补上；有文字的不能覆盖，
   * 否则可见文字和读出来的名字对不上。
   */
  /**
   * 触发器永远是子元素本身，绝不另外套一层盒子。
   *
   * 套过一次，代价是时间轴那条 `flex-1` 的容器认了新的父节点（一个静态 span），宽度塌成内容宽，
   * 刻度按百分比定位就全挤到最左边糊成一团。同理，凡是 `absolute`、`flex-1`、`grid` 子项这些
   * 靠父子关系吃饭的样式，中间插任何一层都会散架。
   *
   * 代价是子元素得能接 props 和 ref。传进来的不是单个元素时（纯文本之类）才退回套 span。
   */
  const el = isValidElement(children) ? (children as ReactElement<HintChildProps>) : null
  /**
   * 挂在徽标、健康点这类东西上的提示**不占 Tab 位**。
   *
   * 占过一阵：给它们都加 `tabIndex={0}`，键盘用户才够得着解释。代价是服务总览这种密集页面上
   * 每行的健康点都成了一站，Tab 一遍长得离谱。改成把解释放进一个 `hidden` 节点、用
   * `aria-describedby` 指过去——`hidden` 不生成盒子所以零布局影响，而被 `aria-describedby`
   * 引用的隐藏内容读屏照样会念，等于把原生 `title` 对读屏的那一半行为原样还回来了。
   *
   * 于是三种人都有着落：鼠标悬停看得见，读屏听得到，键盘用户少走几十站。
   */
  const described = !asChild && typeof text === 'string' && hasTextChild(el?.props.children)
  /**
   * 两件事分开记：`armed` 是「气泡那一坨挂没挂上」（首屏不加载 Base UI，碰到才挂），`open` 是
   * 「这会儿该不该显示」。开合完全由这里决定，Base UI 只负责画和定位。
   */
  const [armed, setArmed] = useState(false)
  const [open, setOpen] = useState(false)
  const node = useRef<HTMLElement | null>(null)
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  const clear = useCallback(() => {
    if (timer.current) clearTimeout(timer.current)
    timer.current = undefined
  }, [])
  useEffect(() => clear, [clear])

  // 桌面上按 Esc 收起；触摸那边的 popover 由 Base UI 自己处理
  useEffect(() => {
    if (!open || isMobile) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false)
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [open, isMobile])

  const openRef = useRef(false)
  useEffect(() => {
    openRef.current = open
  }, [open])
  const hide = useCallback(() => {
    clear()
    if (openRef.current) lastHintClosedAt = Date.now()
    setOpen(false)
  }, [clear])
  const handlers: HintHandlers = isMobile
    ? {
        // 触摸设备没有悬停：点一下开，再点一下（或点外面）关。子元素自己的 onClick 照常执行
        onClick: () => {
          setArmed(true)
          setOpen((o) => !o)
        },
      }
    : {
        onPointerEnter: (e: React.PointerEvent<HTMLElement>) => {
          if (e.pointerType !== 'mouse') return
          setArmed(true)
          clear()
          if (delay <= 0 || Date.now() - lastHintClosedAt < HINT_WARM_MS) setOpen(true)
          else timer.current = setTimeout(() => setOpen(true), delay)
        },
        onPointerLeave: hide,
        // 按下就收起：人已经在操作这个按钮了，解释挡在旁边只会碍事
        onPointerDown: hide,
        onFocus: (e: React.FocusEvent<HTMLElement>) => {
          setArmed(true)
          if (focusVisible(e.currentTarget)) setOpen(true)
        },
        onBlur: hide,
      }

  const childRef = el?.props.ref
  const ref = useMemo(
    () => (v: HTMLElement | null) => {
      node.current = v
      if (typeof childRef === 'function') childRef(v)
      else if (childRef) (childRef as React.RefObject<HTMLElement | null>).current = v
    },
    [childRef],
  )
  /**
   * 触发器**始终渲染在同一个位置、同一层组件下**。早先是碰到之后才把它挪进 Base UI 的 Trigger，
   * 挪动会让 React 卸掉旧按钮、另建一个——恰好在按下的那一刻，点击就落在了被换掉的旧节点上，
   * 手机上第一次轻触因此无效。现在气泡作为兄弟节点单独挂，按 `anchor` 对准这个节点定位。
   */
  const trigger = el ? (
    cloneElement(el, {
      ref,
      onPointerEnter: chain(el.props.onPointerEnter, handlers.onPointerEnter),
      onPointerLeave: chain(el.props.onPointerLeave, handlers.onPointerLeave),
      onPointerDown: chain(el.props.onPointerDown, handlers.onPointerDown),
      onFocus: chain(el.props.onFocus, handlers.onFocus),
      onBlur: chain(el.props.onBlur, handlers.onBlur),
      onClick: chain(el.props.onClick, handlers.onClick),
      // 不加 cursor-help：满页的格子、徽标、标题都挂着 Hint，全换成问号鼠标，就成了「移到哪里都是问号」。
      // 问号只留给真正要人来看说明的地方——带 ⓘ 的标签，见 InfoHint
      className: cn(el.props.className, className),
      ...(described ? { 'aria-describedby': descId } : {}),
      ...(typeof text === 'string' && !el.props['aria-label'] && !hasTextChild(el.props.children) ? { 'aria-label': text } : {}),
    })
  ) : (
    <span ref={ref} {...handlers} className={className}>
      {children}
    </span>
  )
  const desc = described ? (
    <span id={descId} hidden>
      {text}
    </span>
  ) : null
  // 模块已取回就直接用，免得第一次悬停被 lazy 的挂起节流拖慢（见 loadBaseUi）
  const Popup = baseUi?.HintPopup ?? HintPopup
  return (
    <>
      {desc}
      {trigger}
      {/* 说明为空（调用方按条件传 undefined）就不弹：弹出来也只是一个空白的小框 */}
      {armed && text != null && text !== '' && (
        <Suspense fallback={null}>
          <Popup
            text={text}
            side={side}
            touch={isMobile}
            open={open}
            anchor={node}
            onOpenChange={(next, target) => {
              // 点的是触发器本身：交给它自己的 onClick 去切换，这里若先关一次，紧接着又会被点开
              if (!next && target instanceof Node && node.current?.contains(target)) return
              if (!next) clear()
              setOpen(next)
            }}
          />
        </Suspense>
      )}
    </>
  )
}

/**
 * 带 ⓘ 的标签：说明只挂在「标签文字 + ⓘ」这一小段上，鼠标移上去才出现问号与说明。
 *
 * 传了 `children` 就把标签文字一并作为悬停目标——14px 的图标单独作目标太小，不好碰到。
 *
 * 不要用 `Hint` 直接包住整张卡片、整行或表头：那样鼠标移到哪里都是问号，说明四处弹出。
 * 按钮上也别挂长段解释。也不必处处都放：只给名字本身说不清的概念加，能从上下文看懂的就不加。
 */
export function InfoHint({ text, children, className }: { text: ReactNode; children?: ReactNode; className?: string }) {
  return (
    // 移到 ⓘ 上就是来看说明的，不必再等
    <Hint text={text} delay={0}>
      <span className={cn('group/info inline-flex cursor-help items-center gap-1', className)}>
        {children}
        <InfoIcon aria-hidden className="size-3.5 shrink-0 text-muted-fg/70 group-hover/info:text-fg" />
      </span>
    </Hint>
  )
}

export function Badge({
  children,
  tone = 'muted',
  className,
}: {
  children: ReactNode
  tone?: 'muted' | 'danger' | 'warn' | 'ok' | 'info' | 'debug' | 'accent'
  className?: string
}) {
  return (
    <span
      className={cn(
        'inline-flex items-center rounded-sm px-1.5 py-px text-2xs font-semibold leading-[1.125rem] tracking-wide',
        tone === 'muted' && 'bg-muted text-muted-fg',
        tone === 'danger' && 'bg-danger-soft text-danger',
        tone === 'warn' && 'bg-warn-soft text-warn',
        tone === 'ok' && 'bg-ok-soft text-ok',
        tone === 'info' && 'bg-info-soft text-info',
        tone === 'debug' && 'bg-debug-soft text-debug',
        tone === 'accent' && 'bg-accent-soft text-accent',
        className,
      )}
    >
      {children}
    </span>
  )
}

/** 日志级别 → 徽章颜色 */
export function levelTone(level: string): 'danger' | 'warn' | 'info' | 'debug' | 'muted' {
  switch (level.toUpperCase()) {
    case 'ERROR':
    case 'FATAL':
      return 'danger'
    case 'WARN':
    case 'WARNING':
      return 'warn'
    case 'INFO':
      return 'info'
    case 'DEBUG':
    case 'TRACE':
      return 'debug'
    default:
      return 'muted'
  }
}

export function Spinner({ className }: { className?: string }) {
  return <Loader2Icon className={cn('size-4 animate-spin text-muted-fg', className)} aria-label="加载中" />
}

export function EmptyState({ title, hint, action }: { title: string; hint?: ReactNode; action?: ReactNode }) {
  return (
    <div className="flex flex-col items-center justify-center gap-2 px-4 py-20 text-center">
      <div className="text-base font-medium text-fg">{title}</div>
      {hint && <div className="max-w-lg text-sm leading-6 text-muted-fg">{hint}</div>}
      {action && <div className="mt-2">{action}</div>}
    </div>
  )
}

export function ErrorBox({ error, onRetry }: { error: unknown; onRetry?: () => void }) {
  const e = error as { message?: string; tooHeavy?: boolean; status?: number }
  return (
    <div className="m-4 rounded-md border border-danger/30 bg-danger-soft px-4 py-3 text-sm text-danger">
      <div className="font-medium">{e?.status === 401 ? '需要登录' : '查询失败'}</div>
      <div className="mt-0.5 break-all font-normal opacity-90">{e?.message ?? String(error)}</div>
      {e?.tooHeavy && <div className="mt-1 opacity-80">建议缩小时间范围，或先添加服务、pod 或级别筛选后再查询。</div>}
      {onRetry && (
        <Button size="xs" className="mt-2" onClick={onRetry}>
          重试
        </Button>
      )}
    </div>
  )
}

export function Kbd({ children }: { children: ReactNode }) {
  return <kbd className="rounded border border-border bg-muted px-1 font-mono text-2xs text-muted-fg">{children}</kbd>
}

/**
 * 顶部带标题的小卡片。`ref` 是给「滚进视口才查」用的（见 useInView）。
 *
 * 标题是 `<h2>`，卡片用 `aria-labelledby` 认它当自己的名字。两件事都不是摆设：
 * 没有名字的 `<section>` 根本不会被曝露成地标，读屏的地标列表里一个卡片都没有；标题写成
 * `<div>` 则是另一半——读屏用户找路最常用的就是「列出本页所有标题」，指标看板二十来张卡
 * 全是 div 的话那个列表是空的，只能一路 Tab 过去。
 *
 * 层级固定 h2：页面标题是 h1（每页都有，视觉上没有的写成 `sr-only`），卡片是它下面一级。
 */
export function Card({
  title,
  extra,
  children,
  className,
  ref,
}: {
  title?: ReactNode
  extra?: ReactNode
  children: ReactNode
  className?: string
  ref?: React.Ref<HTMLElement>
}) {
  const titleId = useId()
  return (
    <section ref={ref} aria-labelledby={title ? titleId : undefined} className={cn('rounded-lg border border-border bg-card', className)}>
      {/* 放得下时一行，标题靠左、右侧控件靠右；放不下（手机上，或右侧控件多）时控件换到
          下一行，别把标题挤成一字一行——以前是定高 h-11 不许换行，窄屏上两边互相压 */}
      {(title || extra) && (
        <header className="flex min-h-11 flex-wrap items-center justify-between gap-x-2 gap-y-1.5 border-b border-border px-4 py-1.5">
          <h2 id={titleId} className="min-w-0 text-sm font-semibold text-fg">
            {title}
          </h2>
          <div className="ml-auto flex min-w-0 items-center gap-2">{extra}</div>
        </header>
      )}
      {children}
    </section>
  )
}

export interface ComboOption {
  value: string
  /** 不给就显示 value 本身 */
  label?: string
  /** 跟在标签后面的灰字，比如条数 */
  note?: string
}

/** 单选时 `value` 是一个值（'' 为不选），多选时是一组值（空数组为不选） */
type ComboValue = { multiple?: false; value: string; onChange: (v: string) => void } | { multiple: true; value: string[]; onChange: (v: string[]) => void }

export type ComboboxProps = ComboValue & {
  options: ComboOption[]
  /** 菜单开合时通知调用方。候选值要现查的（表头筛选），据此在点开时才发请求 */
  onOpenChange?: (open: boolean) => void
  /** 空值那一项的文案，也是没选时按钮上的字 */
  placeholder?: string
  searchPlaceholder?: string
  emptyText?: string
  className?: string
  title?: string
  disabled?: boolean
  loading?: boolean
  /** 选项是等宽内容（指标名、属性值这类） */
  mono?: boolean
  clearable?: boolean
  /** 允许直接用搜索词当值，见上面的说明 */
  allowCustom?: boolean
  /**
   * `inline`：嵌在表头 / 一行文字里用——触发器没有边框和高度，只是一个图标加当前值；
   * 菜单按触发器在屏幕上的位置 fixed 定位，不受外层滚动容器裁切。
   */
  variant?: 'default' | 'inline'
  /** inline 时触发器上的图标（默认漏斗） */
  trigger?: ReactNode
  /** inline 时把图标放在当前值后面：写在一句话里的下拉（「日均 · 所选账期 ▾」）读起来才顺 */
  triggerEnd?: boolean
  /** `sm`：与 `h-8 text-xs` 的工具栏控件对齐（费用页顶栏） */
  size?: 'md' | 'sm'
  /**
   * 菜单按触发器在屏幕上的位置 fixed 定位（`inline` 本来就是这样），不受外层 `overflow` 裁切。
   * 放在对话框这类 `overflow-hidden` 的容器里时要开：默认的绝对定位会被容器切掉下半截。
   *
   * **前提是祖先里没有 `transform`**：带 transform 的祖先会成为 fixed 的定位基准，菜单于是相对
   * 它定位、照样被它裁掉。对话框的水平居中因此用 `mx-auto`，不用 `-translate-x-1/2`
   */
  floating?: boolean
}

/**
 * 带搜索的下拉。选项上百的地方（服务、pod、接口、指标名）用它，原生 `<select>` 只能靠首字母跳，
 * 找一个名字要滚半天。选项只有几个的（排序、步长、对比区间）继续用 `Select`。
 *
 * 费用页整页都用它：那一页的下拉与筛选栏挨在一起，一半是系统弹出的原生菜单、一半是这种卡片式
 * 菜单，看上去像两个产品。
 *
 * `clearable`（默认开）：第一项是「全部」，选中别的值时触发器描成强调色，表示这个筛选正在生效。
 * 关掉它就是一个普通的「选一个」——账期、排行维度这类永远有值，不该一直亮着。
 *
 * `allowCustom`：搜索词不等于任何候选值时，在「全部」下面多一行「筛选「<搜索词>」」，选中就以搜索词
 * 本身为值。候选只是按量取的前 N 个时要开——量小的值（日志页每小时几十行的服务）排不进候选，
 * 不开的话人明知道名字也选不上。
 *
 * `multiple`：多选。`value` / `onChange` 换成数组，选项前带勾选框，点一项只勾上或取消、菜单不收，
 * 「全部」那一项清空全部。已选但不在候选里的值（候选换了一批、或是自定义输入的）排在最前，
 * 否则勾上之后就再也取消不掉。
 */
export function Combobox(props: ComboboxProps) {
  const {
    options,
    placeholder = '全部',
    searchPlaceholder = '输入筛选…',
    emptyText = '没有匹配项',
    className,
    title,
    disabled,
    loading,
    mono,
    clearable = true,
    allowCustom = false,
    variant = 'default',
    trigger,
    triggerEnd = false,
    size = 'md',
    floating = false,
    onOpenChange,
  } = props
  // 单选也按「选中的一组值」处理，下面只认 picked
  const multiple = props.multiple === true
  const raw = props.value
  const picked = useMemo(() => (Array.isArray(raw) ? raw : raw ? [raw] : []), [raw])
  const isPicked = (v: string) => (v ? picked.includes(v) : picked.length === 0)
  const [open, setOpen] = useState(false)
  const [q, setQ] = useState('')
  const [cursor, setCursor] = useState(0)
  // 开合的入口有好几处（点触发器、点外面、Esc、选中），在这里统一通知，不去每处各补一句
  const notify = useRef(onOpenChange)
  notify.current = onOpenChange
  useEffect(() => {
    notify.current?.(open)
  }, [open])
  const [alignRight, setAlignRight] = useState(false)
  // 菜单最宽能有多宽：朝哪边展开，就是那一边到屏幕边缘的距离（留 8px）
  const [menuMaxW, setMenuMaxW] = useState(COMBO_MENU_W)
  // inline 变体的菜单锚点：触发器的屏幕坐标（打开那一刻量的）
  const [anchor, setAnchor] = useState<{ top: number; left: number; right: number; width: number } | null>(null)
  const inline = variant === 'inline'
  const fixed = inline || floating
  const box = useRef<HTMLDivElement>(null)
  const listBox = useRef<HTMLDivElement>(null)
  const triggerRef = useRef<HTMLButtonElement>(null)
  /**
   * 给读屏用的 id。焦点一直在搜索框里，高亮项只是 `cursor` 这个下标——不把它写成
   * `aria-activedescendant`，读屏就只知道「编辑框」，上下键按半天听不到任何动静。
   */
  const uid = useId()
  const listId = `${uid}-listbox`
  const optId = (i: number) => `${uid}-opt-${i}`
  const input = useRef<HTMLInputElement>(null)

  // 多选时已选、却不在候选里的值补在最前，见上面的说明
  const pool = useMemo((): ComboOption[] => {
    if (!multiple) return options
    const missing = picked.filter((v) => !options.some((o) => o.value === v)).map((v) => ({ value: v }))
    return missing.length ? [...missing, ...options] : options
  }, [multiple, picked, options])
  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase()
    const hit = needle ? pool.filter((o) => (o.label ?? o.value).toLowerCase().includes(needle) || o.value.toLowerCase().includes(needle)) : pool
    // 自定义那一行排在候选前面：候选可能上百条，放后面会被 MAX_COMBO_ROWS 截掉
    const typed = q.trim()
    const custom = allowCustom && typed && !pool.some((o) => o.value === typed) ? [{ value: typed, label: `筛选「${typed}」`, note: '不在候选中' }] : []
    // 空值那一项（「全部服务」之类）始终排在最前，且不参与筛选
    return [...(clearable ? [{ value: '', label: placeholder }] : []), ...custom, ...hit]
  }, [pool, q, clearable, placeholder, allowCustom])
  const capped = shown.slice(0, MAX_COMBO_ROWS)
  const labelOf = (v: string) => options.find((o) => o.value === v)?.label ?? v

  useEffect(() => {
    if (!open) return
    // preventScroll：聚焦默认会把输入框滚进视野，顺带滚动它所有的祖先——在对话框这类
    // overflow-hidden 的容器里，整块内容会被推出可视区，只剩一片空白
    input.current?.focus({ preventScroll: true })
    // 点外面关：人已经把焦点送到别处了，别再抢回触发器
    const onClick = (e: MouseEvent) => {
      if (box.current && !box.current.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener('mousedown', onClick)
    // fixed 定位的菜单跟不上外层滚动，滚了就收起来
    const onScroll = (e: Event) => {
      if (fixed && !listBox.current?.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener('scroll', onScroll, true)
    return () => {
      document.removeEventListener('mousedown', onClick)
      document.removeEventListener('scroll', onScroll, true)
    }
  }, [open, fixed])

  /**
   * 键盘上下移动时把高亮那行带进视口；刚打开时也要滚到当前选中那行。
   *
   * **只滚菜单自己的列表**，不用 `scrollIntoView`：它会滚动所有可滚动的祖先，对话框虽是
   * overflow-hidden 也照样能被程序滚动，整块内容被推出可视区（拉取账单对话框里出过这事）。
   */
  useEffect(() => {
    const list = listBox.current
    const row = list?.children[cursor] as HTMLElement | undefined
    if (!open || !list || !row) return
    if (row.offsetTop < list.scrollTop) list.scrollTop = row.offsetTop
    else if (row.offsetTop + row.offsetHeight > list.scrollTop + list.clientHeight) {
      list.scrollTop = row.offsetTop + row.offsetHeight - list.clientHeight
    }
  }, [cursor, open])

  const openMenu = () => {
    if (disabled) return
    const rect = box.current?.getBoundingClientRect()
    // 靠右的下拉（页头那几个）往左展开，不然超出屏幕。但只在左边地方更大时才往左：手机上两边
    // 都放不下 420px，靠左的下拉（费用页的起始账期）一律往左展开的话，菜单整个出了屏幕
    const spaceRight = rect ? window.innerWidth - rect.left : COMBO_MENU_W
    const spaceLeft = rect ? rect.right : 0
    const toLeft = spaceRight < COMBO_MENU_W && spaceLeft > spaceRight
    setAlignRight(toLeft)
    setMenuMaxW(Math.max(160, Math.min(COMBO_MENU_W, (toLeft ? spaceLeft : spaceRight) - 8)))
    setAnchor(rect ? { top: rect.bottom, left: rect.left, right: window.innerWidth - rect.right, width: rect.width } : null)
    setQ('')
    // 清了搜索词，高亮直接按未筛选的列表算（clearable 的话前面还多一行「全部」）
    const idx = pool.findIndex((o) => o.value === picked[0]) + (clearable ? 1 : 0)
    setCursor(idx > 0 && idx < MAX_COMBO_ROWS ? idx : 0)
    setOpen(true)
  }
  /**
   * 关菜单时**必须**把焦点还给触发器。
   *
   * 焦点这会儿在菜单里的搜索框上，而菜单一关那个 input 就卸载了——焦点无处可去，掉回
   * `<body>`。对键盘用户的实际后果是：选完一个服务之后再按 Tab，是从整页最顶上重新开始走，
   * 而不是接着走筛选栏的下一个控件。
   */
  const close = (backToTrigger = true) => {
    setOpen(false)
    if (backToTrigger) triggerRef.current?.focus()
  }
  const commit = (v: string) => {
    if (!props.multiple) {
      props.onChange(v)
      return close()
    }
    // 多选：「全部」清空并收起；其余只勾上或取消，菜单留着接着选。自定义输入的值加上后清掉搜索词
    if (!v) {
      props.onChange([])
      return close()
    }
    props.onChange(picked.includes(v) ? picked.filter((x) => x !== v) : [...picked, v])
    if (!pool.some((o) => o.value === v)) setQ('')
  }
  /** 往下走 n 行（负数往上），到头绕回去 */
  const move = (n: number) => setCursor((c) => (capped.length ? (((c + n) % capped.length) + capped.length) % capped.length : 0))
  /**
   * 键盘。除了上下和回车，APG 的 combobox 还要求 Home / End 跳首尾、PageUp / PageDown 翻一屏
   * ——候选到几百个的时候（服务、pod、指标名），只有上下键意味着按住方向键滚半天。
   */
  const onKey = (e: ReactKeyboardEvent) => {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault()
      if (!open) return openMenu()
      move(e.key === 'ArrowDown' ? 1 : -1)
    } else if (e.key === 'PageDown' || e.key === 'PageUp') {
      if (!open) return
      e.preventDefault()
      move(e.key === 'PageDown' ? PAGE_ROWS : -PAGE_ROWS)
    } else if (e.key === 'Home' || e.key === 'End') {
      // 搜索框里 Home / End 本来是移动光标的，只有菜单开着才抢过来当「跳到首尾」
      if (!open || !capped.length) return
      e.preventDefault()
      setCursor(e.key === 'Home' ? 0 : capped.length - 1)
    } else if (e.key === 'Enter') {
      // 筛选栏都在 <form> 里，回车不能让它提交
      e.preventDefault()
      if (!open) openMenu()
      else if (capped[cursor]) commit(capped[cursor].value)
    } else if (e.key === 'Escape') {
      if (open) e.stopPropagation()
      close(open)
    } else if (e.key === 'Tab' && open) {
      // Tab 走人就当选好了看完了：收起来，别留一个浮层挂在那儿
      setOpen(false)
    }
  }

  const triggerBtn = (
      <button
        ref={triggerRef}
        type="button"
        disabled={disabled}
        aria-label={title}
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => (open ? close(false) : openMenu())}
        onKeyDown={onKey}
        className={cn(
          inline
            ? 'flex min-w-0 max-w-full items-center gap-0.5 rounded hover:text-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand/25 disabled:cursor-not-allowed disabled:opacity-50'
            : cn(
                'flex w-full items-center gap-1.5 rounded-md border border-input bg-card px-2.5 text-left text-fg focus:border-brand focus:ring-2 focus:ring-brand/25 focus:outline-none disabled:cursor-not-allowed disabled:opacity-50',
                size === 'sm' ? 'h-8 text-xs' : 'h-9 text-sm',
              ),
          // 只有「可清空的筛选」选了值才亮：它表示这一项正在收窄结果
          picked.length > 0 && clearable && (inline ? 'text-accent hover:text-accent' : 'border-accent text-accent'),
        )}
      >
        {inline ? (
          <>
            {!triggerEnd && (trigger ?? <FilterIcon className="size-3 shrink-0" aria-hidden />)}
            {picked.length > 0 && <span className={cn('min-w-0 truncate font-medium', mono && 'mono')}>{labelOf(picked[0])}</span>}
            {picked.length > 1 && <span className="shrink-0 text-2xs font-medium">+{picked.length - 1}</span>}
            {triggerEnd && (trigger ?? <FilterIcon className="size-3 shrink-0" aria-hidden />)}
          </>
        ) : (
          <>
            <span className={cn('min-w-0 flex-1 truncate', mono && picked.length > 0 && 'mono', !picked.length && 'text-fg')}>
              {picked.length ? labelOf(picked[0]) : placeholder}
              {picked.length > 1 && ` 等 ${picked.length} 项`}
              {loading && '…'}
            </span>
            <ChevronDownIcon className="size-4 shrink-0 text-muted-fg" />
          </>
        )}
      </button>
  )
  return (
    <div ref={box} className={cn('relative', className)}>
      {title ? (
        <Hint text={title} asChild>
          {triggerBtn}
        </Hint>
      ) : (
        triggerBtn
      )}
      {open && (
        <div
          className={cn(
            'z-30 w-max rounded-md border border-border bg-card text-fg shadow-lg',
            inline ? 'fixed min-w-56 text-xs' : fixed ? 'fixed' : 'absolute top-full mt-1 min-w-full',
            !fixed && (alignRight ? 'right-0' : 'left-0'),
          )}
          style={{
            maxWidth: menuMaxW,
            ...(fixed && anchor ? { top: anchor.top + 4, ...(alignRight ? { right: anchor.right } : { left: anchor.left }) } : {}),
            // floating 保持「至少和触发器一样宽」，与默认的 min-w-full 一致
            ...(floating && !inline && anchor ? { minWidth: anchor.width } : {}),
          }}
        >
          <div className="border-b border-border p-1.5">
            <div className="relative">
              <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-fg" />
              <Input
                ref={input}
                value={q}
                onChange={(e) => {
                  setQ(e.target.value)
                  // 能自定义时高亮落在「全部」下面那行（自定义那一行或第一个候选），回车就是用这个值
                  setCursor(allowCustom && e.target.value.trim() && clearable ? 1 : 0)
                }}
                onKeyDown={onKey}
                placeholder={searchPlaceholder}
                className="h-8 pl-8 text-xs"
                aria-label="筛选选项"
                role="combobox"
                aria-autocomplete="list"
                aria-expanded
                aria-controls={listId}
                aria-activedescendant={capped[cursor] ? optId(cursor) : undefined}
              />
            </div>
          </div>
          <div ref={listBox} id={listId} role="listbox" aria-label={title ?? placeholder} aria-multiselectable={multiple || undefined} className="max-h-72 overflow-auto py-1">
            {capped.map((o, i) => (
              /*
               * 选项是 `div role="option"`，不是 `button role="option"`：ARIA in HTML 明说
               * 别把 option 这类角色盖在 button 上——按钮的语义被覆盖掉，只剩一个「进不了
               * tab 序列的按钮」。焦点全程留在搜索框里，高亮靠 `aria-activedescendant` 指过来，
               * 所以选项自己不进 tab 序列（APG 的 combobox 就是这么规定的）。
               */
              // eslint-disable-next-line jsx-a11y/click-events-have-key-events
              <div
                key={o.value || '__all__'}
                id={optId(i)}
                role="option"
                aria-selected={isPicked(o.value)}
                tabIndex={-1}
                onMouseEnter={() => setCursor(i)}
                onClick={() => commit(o.value)}
                className={cn(
                  'flex w-full cursor-pointer items-center gap-2 px-2.5 py-1.5 text-left text-xs',
                  i === cursor && 'bg-accent-soft',
                  isPicked(o.value) && 'font-semibold text-accent',
                )}
              >
                {multiple && (
                  <span
                    aria-hidden
                    className={cn(
                      'flex size-3.5 shrink-0 items-center justify-center rounded-sm border',
                      o.value && isPicked(o.value) ? 'border-accent bg-accent text-accent-fg' : 'border-input',
                      !o.value && 'invisible',
                    )}
                  >
                    {o.value && isPicked(o.value) && <CheckIcon className="size-3" />}
                  </span>
                )}
                <span className={cn('min-w-0 flex-1 truncate', mono && o.value && 'mono')}>{o.label ?? o.value ?? ''}</span>
                {o.note && <span className="shrink-0 text-2xs text-muted-fg">{o.note}</span>}
              </div>
            ))}
            {capped.length === 0 && <div className="px-3 py-6 text-center text-xs text-muted-fg">{loading ? '加载中…' : emptyText}</div>}
          </div>
          {shown.length > capped.length && (
            <div className="border-t border-border px-2.5 py-1.5 text-2xs text-muted-fg">
              另有 {shown.length - capped.length} 项未列出，请继续输入以缩小范围
            </div>
          )}
        </div>
      )}
    </div>
  )
}

/** 复制成功的 ✓ 停留多久（毫秒）。和 Kumo 的 `InlineCopyText` 取同一个值 */
const COPY_FLASH_MS = 1500

/**
 * 人是不是正在这个按钮里拖着选文字。
 *
 * 整块可点的按钮把值包进了 `<button>`，浏览器默认不让选按钮里的文本（`select-text` 放开了
 * 这条），但拖完松手照样算一次 click——不管的话，想从一条两百字的 SQL 里只拖出中间那个
 * UUID，一松手整条就被复制走了，选区还会被按钮吃掉。有选区就让这一下什么都不做。
 */
function hasSelectionInside(el: HTMLElement): boolean {
  const sel = window.getSelection()
  if (!sel || sel.isCollapsed || !sel.rangeCount) return false
  return el.contains(sel.getRangeAt(0).commonAncestorContainer)
}

/**
 * 「复制」控件，对着 Cloudflare Kumo 的 `InlineCopyText` 做：点一下把 `text` 塞进剪贴板，
 * 图标换成 ✓ 停 1.5 秒，同时用一个只给读屏的 live region 播报一声。
 *
 * 两种用法：只给 `text` 就是一个光图标的按钮；再给 `children` 就是「整块可点」——文字和图标
 * 一起在按钮里，点文字也复制，Kumo 的 id、表格单元格都是这么摆的。
 *
 * `reveal` 让图标平时透明、hover 或聚焦才浮现，既跟着自己（`group/copy`），也跟着外面套的那层
 * 无名 `group`，所以整行 hover 时行里的图标一起亮。手机上没有 hover，`md` 以下一律常驻。
 *
 * `text` 可以传函数：整页属性拼成 JSON 这种，别在每次渲染时都算一遍，点了才算。
 *
 * 按钮常挂在「点一下就跳转」的行里（链路列表、瀑布图的行），所以默认吃掉事件，不往上冒泡。
 */
export function CopyButton({
  text,
  title = '复制',
  label,
  children,
  reveal,
  align = 'center',
  size = 'sm',
  className,
}: {
  text: string | (() => string)
  /** 鼠标悬停的提示，顺带作为 aria-label */
  title?: string
  /** 图标后面跟一段文字（「复制链接」这种） */
  label?: ReactNode
  /** 图标前面的内容：给了就是整块可点，值本身也在按钮里 */
  children?: ReactNode
  /** 图标平时不显示，hover / 聚焦才浮现（桌面）；手机上照常显示 */
  reveal?: boolean
  /**
   * `children` 会折行时传 `start`：图标贴住第一行，而不是浮在整段的垂直中间。
   * 图标撑成一个 `text-xs` 的行框（`h-5` = `--text-xs--line-height`）再居中，
   * 不然 14px 的图标顶在 20px 行框上沿，看着比字高一截。
   */
  align?: 'center' | 'start'
  size?: 'xs' | 'sm'
  className?: string
}) {
  const [copied, setCopied] = useState(false)
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  useEffect(() => () => clearTimeout(timer.current), [])
  const Icon = copied ? CheckIcon : CopyIcon
  const hint = copied ? '已复制' : title
  const btn = (
    <button
      type="button"
      aria-label={hint}
      className={cn(
        'group/copy cursor-pointer gap-1 rounded-sm transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand/60',
        align === 'start' ? 'items-start' : 'items-center',
        children ? 'flex min-w-0 max-w-full text-left select-text' : 'inline-flex shrink-0 align-middle',
        copied ? 'text-ok' : 'text-muted-fg hover:text-fg',
        className,
      )}
      onClick={async (e) => {
        // 行上还挂着跳转，点复制不该顺带把人送走
        e.stopPropagation()
        e.preventDefault()
        // 刚拖着选了一段，那这一下是「选完松手」，不是「要整条」
        if (hasSelectionInside(e.currentTarget)) return
        if (!(await copyText(typeof text === 'function' ? text() : text))) return
        setCopied(true)
        clearTimeout(timer.current)
        timer.current = setTimeout(() => setCopied(false), COPY_FLASH_MS)
      }}
    >
      {children}
      <Icon
        className={cn(
          'shrink-0',
          size === 'xs' ? 'size-3' : 'size-3.5',
          align === 'start' && 'h-5',
          // ✓ 是复制的结果，任何时候都得看得见，只有待命的图标才躲起来
          reveal &&
            !copied &&
            'transition-opacity motion-reduce:transition-none md:opacity-0 md:group-hover/copy:opacity-100 md:group-focus-visible/copy:opacity-100 md:group-hover:opacity-100 md:group-focus-within:opacity-100',
        )}
      />
      {label}
      {/* 读屏用户不会看见图标变 ✓，得说一声 */}
      <span className="sr-only" aria-live="polite">
        {copied ? '已复制' : ''}
      </span>
    </button>
  )
  return (
    <Hint text={hint} asChild>
      {btn}
    </Hint>
  )
}
