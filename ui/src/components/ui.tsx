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
import { CheckIcon, ChevronDownIcon, CopyIcon, FilterIcon, Loader2Icon, SearchIcon } from 'lucide-react'
import type { Side } from '@/components/base-ui'
import { useIsMobile } from '@/lib/media'
import { cn, copyText } from '@/lib/utils'

/** 下拉一次最多画多少行：服务 / 接口的候选能到几百，全画出来白费力气，让人接着敲 */
const MAX_COMBO_ROWS = 200
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
  onPointerEnter?: (e: React.PointerEvent) => void
  onPointerLeave?: (e: React.PointerEvent) => void
  onPointerDown?: (e: React.PointerEvent) => void
  onFocus?: (e: React.FocusEvent) => void
  onBlur?: (e: React.FocusEvent) => void
}

/**
 * Base UI 那一坨（压完 43 kB）切在单独的 chunk 里，首屏不加载——见 `./base-ui`。
 *
 * 光切不预取的话，第一次悬停要等一次网络往返；更糟的是触摸设备上第一下点击会落空，因为那一
 * 下只来得及触发加载，浮层还没挂上。所以首屏画完之后趁空闲捎带取回来：不占关键路径，等人真
 * 去悬停或点击时基本已经就位。
 */
export const HintPopup = lazy(() => import('@/components/base-ui').then((m) => ({ default: m.HintPopup })))
export const PopoverPanel = lazy(() => import('@/components/base-ui').then((m) => ({ default: m.PopoverPanel })))

export function prefetchBaseUi(): void {
  const load = () => void import('@/components/base-ui')
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
}: {
  text: ReactNode
  children: ReactNode
  side?: Side
  className?: string
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
   * 碰到才挂载。`live` 记的是「这会儿指针 / 焦点还在不在触发器上」：浮层是异步挂上来的，挂上来
   * 那一刻 `pointerenter` 早过去了，得靠它决定要不要直接显示；人已经移开了就别凭空弹一个。
   */
  const [armed, setArmed] = useState(false)
  const live = useRef(false)
  const arm = useCallback(() => {
    live.current = true
    setArmed(true)
  }, [])
  const release = useCallback(() => {
    live.current = false
  }, [])
  const armProps = {
    onPointerEnter: arm,
    onPointerLeave: release,
    onPointerDown: arm,
    onFocus: arm,
    onBlur: release,
  }
  const trigger = el
    ? cloneElement(el, {
        ...armProps,
        className: cn(el.props.className, !asChild && 'cursor-help', className),
        ...(described ? { 'aria-describedby': descId } : {}),
        ...(typeof text === 'string' && !el.props['aria-label'] && !hasTextChild(el.props.children) ? { 'aria-label': text } : {}),
      })
    : <span {...armProps} className={cn('cursor-help', className)} />
  const inner = el ? undefined : children
  const desc = described ? (
    <span id={descId} hidden>
      {text}
    </span>
  ) : null
  // 没碰过就只有触发器本身，Base UI 那个 chunk 连挂载都不挂载
  if (!armed) {
    return (
      <>
        {desc}
        {trigger}
      </>
    )
  }
  return (
    <>
      {desc}
      <Suspense fallback={trigger}>
        <HintPopup text={text} side={side} touch={isMobile} defaultOpen={live.current} trigger={trigger}>
          {inner}
        </HintPopup>
      </Suspense>
    </>
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
      {e?.tooHeavy && <div className="mt-1 opacity-80">建议：缩小时间范围，或先加一个服务 / pod / 级别的筛选再查。</div>}
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

/** 顶部带标题的小卡片。`ref` 是给「滚进视口才查」用的（见 useInView）。 */
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
  return (
    <section ref={ref} className={cn('rounded-lg border border-border bg-card', className)}>
      {(title || extra) && (
        <header className="flex h-11 items-center justify-between gap-2 border-b border-border px-4">
          <div className="text-sm font-semibold text-fg">{title}</div>
          <div className="flex items-center gap-2">{extra}</div>
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

/**
 * 带搜索的下拉。选项上百的地方（服务、pod、接口、指标名）用它，原生 `<select>` 只能靠首字母跳，
 * 找一个名字要滚半天。选项只有几个的（排序、步长、对比区间）继续用 `Select`。
 */
export function Combobox({
  value,
  options,
  onChange,
  placeholder = '全部',
  searchPlaceholder = '输入筛选…',
  emptyText = '没有匹配项',
  className,
  title,
  disabled,
  loading,
  mono,
  clearable = true,
  variant = 'default',
  trigger,
}: {
  value: string
  options: ComboOption[]
  onChange: (v: string) => void
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
  /**
   * `inline`：嵌在表头 / 一行文字里用——触发器没有边框和高度，只是一个图标加当前值；
   * 菜单按触发器在屏幕上的位置 fixed 定位，不受外层滚动容器裁切。
   */
  variant?: 'default' | 'inline'
  /** inline 时触发器上的图标（默认漏斗） */
  trigger?: ReactNode
}) {
  const [open, setOpen] = useState(false)
  const [q, setQ] = useState('')
  const [cursor, setCursor] = useState(0)
  const [alignRight, setAlignRight] = useState(false)
  // inline 变体的菜单锚点：触发器的屏幕坐标（打开那一刻量的）
  const [anchor, setAnchor] = useState<{ top: number; left: number; right: number } | null>(null)
  const inline = variant === 'inline'
  const box = useRef<HTMLDivElement>(null)
  const listBox = useRef<HTMLDivElement>(null)
  /**
   * 给读屏用的 id。焦点一直在搜索框里，高亮项只是 `cursor` 这个下标——不把它写成
   * `aria-activedescendant`，读屏就只知道「编辑框」，上下键按半天听不到任何动静。
   */
  const uid = useId()
  const listId = `${uid}-listbox`
  const optId = (i: number) => `${uid}-opt-${i}`
  const input = useRef<HTMLInputElement>(null)

  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase()
    const hit = needle ? options.filter((o) => (o.label ?? o.value).toLowerCase().includes(needle) || o.value.toLowerCase().includes(needle)) : options
    // 空值那一项（「全部服务」之类）始终排在最前，且不参与筛选
    return clearable ? [{ value: '', label: placeholder }, ...hit] : hit
  }, [options, q, clearable, placeholder])
  const capped = shown.slice(0, MAX_COMBO_ROWS)
  const current = options.find((o) => o.value === value)

  useEffect(() => {
    if (!open) return
    input.current?.focus()
    const onClick = (e: MouseEvent) => {
      if (box.current && !box.current.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener('mousedown', onClick)
    // fixed 定位的菜单跟不上外层滚动，滚了就收起来
    const onScroll = (e: Event) => {
      if (inline && !listBox.current?.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener('scroll', onScroll, true)
    return () => {
      document.removeEventListener('mousedown', onClick)
      document.removeEventListener('scroll', onScroll, true)
    }
  }, [open, inline])

  // 键盘上下移动时把高亮那行带进视口；刚打开时也要滚到当前选中那行
  useEffect(() => {
    if (open) listBox.current?.children[cursor]?.scrollIntoView({ block: 'nearest' })
  }, [cursor, open])

  const openMenu = () => {
    if (disabled) return
    const rect = box.current?.getBoundingClientRect()
    // 靠右的下拉（页头那几个）往左展开，不然超出屏幕
    setAlignRight(!!rect && rect.left + COMBO_MENU_W > window.innerWidth)
    setAnchor(rect ? { top: rect.bottom, left: rect.left, right: window.innerWidth - rect.right } : null)
    setQ('')
    // 清了搜索词，高亮直接按未筛选的列表算（clearable 的话前面还多一行「全部」）
    const idx = options.findIndex((o) => o.value === value) + (clearable ? 1 : 0)
    setCursor(idx > 0 && idx < MAX_COMBO_ROWS ? idx : 0)
    setOpen(true)
  }
  const commit = (v: string) => {
    onChange(v)
    setOpen(false)
  }
  const onKey = (e: ReactKeyboardEvent) => {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault()
      if (!open) return openMenu()
      setCursor((c) => (capped.length ? (c + (e.key === 'ArrowDown' ? 1 : capped.length - 1)) % capped.length : 0))
    } else if (e.key === 'Enter') {
      // 筛选栏都在 <form> 里，回车不能让它提交
      e.preventDefault()
      if (!open) openMenu()
      else if (capped[cursor]) commit(capped[cursor].value)
    } else if (e.key === 'Escape') {
      if (open) e.stopPropagation()
      setOpen(false)
    }
  }

  const triggerBtn = (
      <button
        type="button"
        disabled={disabled}
        aria-label={title}
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => (open ? setOpen(false) : openMenu())}
        onKeyDown={onKey}
        className={cn(
          inline
            ? 'flex min-w-0 max-w-full items-center gap-0.5 rounded hover:text-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand/25 disabled:cursor-not-allowed disabled:opacity-50'
            : 'flex h-9 w-full items-center gap-1.5 rounded-md border border-input bg-card px-2.5 text-left text-sm text-fg focus:border-brand focus:ring-2 focus:ring-brand/25 focus:outline-none disabled:cursor-not-allowed disabled:opacity-50',
          value && (inline ? 'text-accent hover:text-accent' : 'border-accent text-accent'),
        )}
      >
        {inline ? (
          <>
            {trigger ?? <FilterIcon className="size-3 shrink-0" aria-hidden />}
            {value && <span className={cn('min-w-0 truncate font-medium', mono && 'mono')}>{current?.label ?? value}</span>}
          </>
        ) : (
          <>
            <span className={cn('min-w-0 flex-1 truncate', mono && value && 'mono', !value && 'text-fg')}>
              {value ? (current?.label ?? value) : placeholder}
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
            inline ? 'fixed min-w-56 text-xs' : 'absolute top-full mt-1 min-w-full',
            !inline && (alignRight ? 'right-0' : 'left-0'),
          )}
          style={{
            maxWidth: `min(90vw, ${COMBO_MENU_W}px)`,
            ...(inline && anchor ? { top: anchor.top + 4, ...(alignRight ? { right: anchor.right } : { left: anchor.left }) } : {}),
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
                  setCursor(0)
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
          <div ref={listBox} id={listId} role="listbox" className="max-h-72 overflow-auto py-1">
            {capped.map((o, i) => (
              <button
                key={o.value || '__all__'}
                id={optId(i)}
                type="button"
                role="option"
                aria-selected={o.value === value}
                // 焦点留在搜索框里，选项不进 tab 序列——上下键走的是 activedescendant
                tabIndex={-1}
                onMouseEnter={() => setCursor(i)}
                onClick={() => commit(o.value)}
                className={cn(
                  'flex w-full items-center gap-2 px-2.5 py-1.5 text-left text-xs',
                  i === cursor && 'bg-accent-soft',
                  o.value === value && 'font-semibold text-accent',
                )}
              >
                <span className={cn('min-w-0 flex-1 truncate', mono && o.value && 'mono')}>{o.label ?? o.value ?? ''}</span>
                {o.note && <span className="shrink-0 text-2xs text-muted-fg">{o.note}</span>}
              </button>
            ))}
            {capped.length === 0 && <div className="px-3 py-6 text-center text-xs text-muted-fg">{loading ? '加载中…' : emptyText}</div>}
          </div>
          {shown.length > capped.length && (
            <div className="border-t border-border px-2.5 py-1.5 text-2xs text-muted-fg">
              还有 {shown.length - capped.length} 项没列出来，继续输入缩小范围
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
