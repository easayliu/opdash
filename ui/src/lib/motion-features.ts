/**
 * LazyMotion 要异步加载的那包功能，单独一个模块。
 *
 * 不能在页面里直接 `import('motion/react')`——那个模块同一个文件里已经静态 import 过（`m`、
 * `AnimatePresence`），打包器会把动态那次并回主包，什么都没省下。摘出来成独立模块，Rollup 才
 * 会切出一个单独的 chunk，首屏画完再拉。
 *
 * `domMax` = 动画 + 进出场 + layout（FLIP）。少一档的 `domAnimation` 不带 layout，卡片重排就
 * 没有平滑位移了。
 */
import { domMax } from 'motion/react'

export default domMax
