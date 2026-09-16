import { memo, useMemo } from 'react'

/**
 * 卡片里的迷你趋势：一排细柱，请求量是主色，错误叠在顶上用红。没有坐标轴、没有交互，
 * 只回答「这一段时间平不平、最近有没有突变」——Cloudflare 总览页每张卡右下角那种。
 *
 * `prev` 是对比窗口里同一格的量，画成浅灰影子垫在后面：形状一比就知道是「今天这个时段
 * 本来就该这样」还是「今天不一样」——纵轴按两边一起的最大值算，不然影子会撑出去。
 * 用 viewBox 撑满容器宽度，柱数变了也不用改尺寸。
 *
 * **每一层是一条 `<path>`，不是一根柱子一个 `<rect>`。** 服务总览上有 80 多张卡，一张卡
 * 30 个桶、三层（影子 / 请求 / 错误）就是 90 个 SVG 节点，整页 7000 多个——React 每次
 * 重渲染都要 diff 一遍，在搜索框里打个字都卡。三条 path 之后一张卡只剩 3 个节点。
 *
 * 外面包了 `memo`：柱子数组来自查询结果，引用不变就不用重画。
 */
export const Sparkline = memo(function Sparkline({
  requests,
  errors,
  prev,
  height = 32,
  className,
}: {
  requests: number[]
  errors?: number[]
  prev?: number[]
  height?: number
  className?: string
}) {
  const n = requests.length
  const max = useMemo(() => Math.max(1, ...requests, ...(prev ?? [])), [requests, prev])
  // 每根柱占 1 个单位宽、留 0.25 的缝；viewBox 宽 = n，高 = 100，按容器拉伸
  const gap = 0.25
  const paths = useMemo(() => {
    /** 一根柱子：从 (x, 100-top) 往下画 h 高、w 宽的方块 */
    const bar = (x: number, top: number, h: number, w: number) =>
      h > 0 ? `M${x} ${100 - top}h${w}v${h}h${-w}Z` : ''
    const shadow = (prev ?? []).map((v, i) => bar(i, (v / max) * 100, (v / max) * 100, 1)).join('')
    let bars = ''
    let errs = ''
    for (let i = 0; i < n; i++) {
      const h = (requests[i] / max) * 100
      bars += bar(i + gap / 2, h, h, 1 - gap)
      const e = errors?.[i] ?? 0
      // 错误那一截从请求柱的顶上往下画；再少也给 2 个单位，不然一两条错就看不见了
      if (e > 0) errs += bar(i + gap / 2, h, Math.max((e / max) * 100, 2), 1 - gap)
    }
    return { shadow, bars, errs }
  }, [requests, errors, prev, max, n])
  if (!n) return <div style={{ height }} className={className} />
  return (
    <svg viewBox={`0 0 ${n} 100`} preserveAspectRatio="none" width="100%" height={height} className={className} aria-hidden>
      {paths.shadow && <path d={paths.shadow} fill="var(--muted-fg)" opacity={0.18} />}
      {paths.bars && <path d={paths.bars} fill="var(--chart-1)" opacity={0.8} />}
      {paths.errs && <path d={paths.errs} fill="var(--level-error)" />}
    </svg>
  )
})
