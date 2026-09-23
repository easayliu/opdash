/**
 * 全项目**唯一**静态引入 Base UI 的地方，别处一律 `lazy(() => import('./base-ui'))`。
 *
 * Base UI 带着定位、焦点管理那一整套，压完 43 kB，而它要到「有人悬停某个提示」或者「有人点开
 * 时间范围」才用得上——都不是首屏。收进这一个模块，打包就能单独切出一个 chunk，首屏不碰它。
 *
 * 只切不预取的话第一次悬停要等网络，所以配 [`prefetchBaseUi`] 在首屏画完之后的空闲时段捎带
 * 加载：既不占首屏的关键路径，等人真去悬停 / 点击时又基本已经就位。
 */
import { Dialog } from '@base-ui/react/dialog'
import { Popover } from '@base-ui/react/popover'
import { Tooltip } from '@base-ui/react/tooltip'
import type { ReactNode, RefObject } from 'react'

export type Side = 'top' | 'bottom' | 'left' | 'right'

/** 气泡的样子：小卡片，和 Cloudflare 控制台一路，靠描边分层 */
const POPUP = 'max-w-72 rounded-md border border-border bg-card px-2.5 py-1.5 text-2xs leading-[1.125rem] text-fg shadow-md'

/**
 * 叠放层级要挂在**定位容器**上，不是挂在气泡上。
 *
 * 气泡是定位容器的子元素，给它 `z-50` 只决定它在自己父容器里的次序，跟页面上别的东西没关系。
 * 真正参与比较的是定位容器，而它默认 `z-index: auto`——顶栏是 `z-20`，于是顶栏赢，气泡被切掉
 * 半截。数值取 50，高过顶栏的 20、对话框遮罩的 40。
 */
const LAYER = 'z-50'

/**
 * 一句解释的气泡本体。**只管浮层，不管触发器**：触发器是调用方（[`Hint`](./ui)）自己渲染的那个
 * 元素，这里用 `anchor` 对准它定位，开合也由调用方控制。
 *
 * 不交给 Base UI 的 `Trigger` 去包，是因为本模块是懒加载的：交给它包，就得在「有人碰到」之后
 * 把触发器挪进 `Trigger` 里，挪动会让 React 卸掉原来的按钮、另建一个——恰好发生在按下的那一刻，
 * 这一下点击就落在了被换掉的旧节点上，手机上第一次轻触因此无效。
 *
 * `touch`：触摸设备上 tooltip 按惯例不响应点击，换成 popover，点一下开、点外面关。
 */
export function HintPopup({
  text,
  side,
  touch,
  open,
  anchor,
  onOpenChange,
}: {
  text: ReactNode
  side: Side
  touch: boolean
  open: boolean
  /** 触发器的 DOM 节点 */
  anchor: RefObject<HTMLElement | null>
  /** Base UI 自己要关的时候（点了外面、按了 Esc）；`target` 是触发这次变化的元素 */
  onOpenChange: (open: boolean, target: EventTarget | null) => void
}) {
  if (touch) {
    return (
      <Popover.Root open={open} onOpenChange={(next, details) => onOpenChange(next, details.event?.target ?? null)}>
        <Popover.Portal>
          <Popover.Positioner anchor={anchor} side={side} sideOffset={6} className={LAYER}>
            {/* 只是一句解释：弹出时不抢焦点，关上时也不必把焦点送回哪里 */}
            <Popover.Popup initialFocus={false} finalFocus={false} className={POPUP}>
              {text}
            </Popover.Popup>
          </Popover.Positioner>
        </Popover.Portal>
      </Popover.Root>
    )
  }
  return (
    <Tooltip.Root open={open} onOpenChange={(next, details) => onOpenChange(next, details.event?.target ?? null)}>
      <Tooltip.Portal>
        <Tooltip.Positioner anchor={anchor} side={side} sideOffset={6} className={LAYER}>
          <Tooltip.Popup className={POPUP}>{text}</Tooltip.Popup>
        </Tooltip.Positioner>
      </Tooltip.Portal>
    </Tooltip.Root>
  )
}

/**
 * 挂在按钮下面的浮层面板：时间范围、收藏查询用它。
 *
 * 触发器**不在**这里面——它得在首屏就画出来，而这个模块是异步加载的。所以用 `anchor` 把面板
 * 锚在调用方那个按钮的 ref 上，Base UI 负责定位、点外面关、Escape 关、焦点进出和 aria 接线，
 * 这几样原来是各写各的手写实现。
 *
 * 关掉之后焦点要还给那个按钮。没有 `Popover.Trigger` 的话 Base UI 不知道该还给谁，所以把
 * `anchor` 同时交给 `finalFocus`。
 */
export function PopoverPanel({
  open,
  onOpenChange,
  anchor,
  side = 'bottom',
  align = 'end',
  className,
  children,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  anchor: RefObject<HTMLElement | null>
  side?: Side
  align?: 'start' | 'center' | 'end'
  className?: string
  children: ReactNode
}) {
  return (
    <Popover.Root open={open} onOpenChange={onOpenChange}>
      <Popover.Portal>
        <Popover.Positioner anchor={anchor} side={side} align={align} sideOffset={6} className={LAYER}>
          <Popover.Popup finalFocus={anchor} className={className}>
            {children}
          </Popover.Popup>
        </Popover.Positioner>
      </Popover.Portal>
    </Popover.Root>
  )
}

/**
 * 模态对话框 / 抽屉的壳：遮罩 + 浮层，焦点、滚动、Escape、点外面关全归 Base UI。
 *
 * 这四件事原来是自己写的（那个 `useModal`），而且有一条是空转的：它把滚动锁在
 * `document.body` 上，但这个应用 body 不滚——外壳是 `h-dvh`，真正滚的是 `<main>`，
 * 于是对话框开着背景照滚。手写的焦点陷阱也只认 Tab 键，管不了 `inert`、管不了开着之后
 * 才插进来的可聚焦节点。
 *
 * `modal`（默认开）这一个属性就包含：焦点关在里面、背景不可点也不滚、Escape 关、关掉之后
 * 焦点还回触发它的那个元素。
 *
 * 和 [`PopoverPanel`] 一样收在这个异步 chunk 里：对话框和抽屉都不是首屏内容。
 */
export function ModalPanel({
  open,
  onOpenChange,
  className,
  backdropClassName = 'fixed inset-0 z-40 bg-black/30',
  labelledBy,
  children,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  className?: string
  backdropClassName?: string
  /** 标题元素的 id：对话框的无障碍名字从它来 */
  labelledBy?: string
  children: ReactNode
}) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Backdrop className={backdropClassName} />
        <Dialog.Popup aria-labelledby={labelledBy} className={className}>
          {children}
        </Dialog.Popup>
      </Dialog.Portal>
    </Dialog.Root>
  )
}
