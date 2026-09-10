/** 几个够用的基础控件。内部工具，不上组件库，样式全靠 Tailwind。 */
import { forwardRef, type ButtonHTMLAttributes, type InputHTMLAttributes, type ReactNode, type SelectHTMLAttributes } from 'react'
import { Loader2Icon } from 'lucide-react'
import { cn } from '@/lib/utils'

type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: 'default' | 'primary' | 'ghost' | 'danger'
  size?: 'sm' | 'md' | 'xs'
  active?: boolean
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(function Button(
  { className, variant = 'default', size = 'md', active, type = 'button', ...props },
  ref,
) {
  return (
    <button
      ref={ref}
      type={type}
      className={cn(
        'inline-flex shrink-0 cursor-pointer items-center justify-center gap-1.5 whitespace-nowrap rounded-md border font-medium transition-colors outline-none focus-visible:ring-2 focus-visible:ring-brand/60 disabled:cursor-not-allowed disabled:opacity-50',
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
      )}
      data-active={active ? 'true' : undefined}
      {...props}
    />
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
        'h-9 w-full min-w-0 rounded-md border border-input bg-card px-3 text-sm text-fg outline-none placeholder:text-muted-fg/70 focus:border-brand focus:ring-2 focus:ring-brand/25 disabled:opacity-50',
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
        'h-9 rounded-md border border-input bg-card px-2.5 text-sm text-fg outline-none focus:border-brand focus:ring-2 focus:ring-brand/25 disabled:opacity-50',
        className,
      )}
      {...props}
    />
  )
})

export function Badge({
  children,
  tone = 'muted',
  className,
  title,
}: {
  children: ReactNode
  tone?: 'muted' | 'danger' | 'warn' | 'ok' | 'info' | 'debug' | 'accent'
  className?: string
  title?: string
}) {
  return (
    <span
      title={title}
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
