/**
 * 全站的动效词汇，就这么几个常量。
 *
 * 时长 150ms、缓动是 CSS 的 `ease`——数值照抄 Cloudflare 自己的设计系统
 * （`@cloudflare/style-const` 里的 `all 150ms ease`，它整套组件的动效只有这一个时长，弹层和
 * Toast 连入场都没有）。所以这里也只做三件事：**淡入淡出**、**位置变了的元素平滑挪过去**
 * （FLIP，CSS 做不了的那件）、**展开区把高度撑开**。不缩放、不位移、不弹。
 *
 * **只动「变了的东西」，不动「刚加载出来的东西」**：页面第一次画出来的内容一律直接出现，不做
 * 入场——转圈、空状态、概览数字、看板分区都不挂动效，进度条式的淡入只是拖慢第一眼。动效留给
 * 挂载之后真正发生的变化：晚一步查回来的卡冒出来、筛掉的行退场、换排序时整行挪位、展开详情。
 * 具体做法是所有 `AnimatePresence` 都带 `initial={false}`：它挂载时就在里面的孩子不播入场，
 * 之后进来的才播。
 *
 * provider 在 `App` 上一次性套好（`LazyMotion` + `MotionConfig`），页面里直接写 `m.div` 就行，
 * 不要再各自套一层。`reducedMotion="user"` 也在那里：系统关了动效的人自动拿到静止版本。
 *
 * 引入方式按官方那套（motion.dev「Reduce bundle size」）：组件从 `motion/react-m` 取轻量的
 * `m.*`，功能包用 `LazyMotion features={异步函数}` 在首屏画完之后再拉，`strict` 兜住任何写成
 * `motion.*` 的地方。
 *
 * 用在列表上时：`AnimatePresence` 包住 map，**动效挂在它的直接子节点上**。直接子节点如果是
 * `memo` 过的组件，列表每重渲染一次，留在原地的那些都会把入场动画重放一遍（实测 opacity 被
 * 写回 0 再涨回 1）——所以外面单独套一层 `m.div`，memo 过的卡 / 行放在里面。
 */
export const EASE = [0.25, 0.1, 0.25, 1] as const

export const TRANSITION = { duration: 0.15, ease: EASE }

/** 进出场：只动透明度 */
export const FADE = { initial: { opacity: 0 }, animate: { opacity: 1 }, exit: { opacity: 0 } }

/**
 * 「晚到的那一类」：淡入 + 上浮 4px、200ms。
 *
 * 只给「查回来之后才冒出来」的东西用（现在就服务页那几张接口级异常卡）。纯 150ms 的淡入在
 * 一屏九张卡里肉眼基本抓不住，人会以为页面自己变了一张；带一点点位移就能在余光里读出
 * 「这张是刚到的」。**退场和重排不跟着改**——那两件事只要不打断阅读就行，仍然是 150ms 只淡不移。
 */
export const RISE = {
  initial: { opacity: 0, y: 4 },
  animate: { opacity: 1, y: 0, transition: { ...TRANSITION, duration: 0.2 } },
  exit: { opacity: 0 },
}

/**
 * 页签之间的整页过场：旧页 120ms 淡出，等它走完新页再 180ms 淡入 + 上浮 6px。
 *
 * 这是**唯一一处「加载有动画」**，而且只在导航时播：按 `pathname` 记 key，同一页改时间范围、
 * 改筛选只是换 query，不重播；轮询刷新更不会。出场比入场短，是为了别让切页签变慢——
 * `mode="wait"` 下这两段是串行的，加起来 300ms 已经是能接受的上限。
 */
export const PAGE = {
  initial: { opacity: 0, y: 6 },
  animate: { opacity: 1, y: 0, transition: { duration: 0.18, ease: EASE } },
  exit: { opacity: 0, transition: { duration: 0.12, ease: EASE } },
}

/** 展开区（报错详情、日志全文这类）：高度一起动，内容才不是「啪」地撑开 */
export const DISCLOSE = {
  initial: { height: 0, opacity: 0 },
  animate: { height: 'auto', opacity: 1 },
  exit: { height: 0, opacity: 0 },
}
