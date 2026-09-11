import { useMemo } from 'react'

/**
 * 卡片里的迷你趋势：一排细柱，请求量是主色，错误叠在顶上用红。没有坐标轴、没有交互，
 * 只回答「这一段时间平不平、最近有没有突变」——Cloudflare 总览页每张卡右下角那种。
 *
 * `prev` 是对比窗口里同一格的量，画成浅灰影子垫在后面：形状一比就知道是「今天这个时段
 * 本来就该这样」还是「今天不一样」——纵轴按两边一起的最大值算，不然影子会撑出去。
 * 用 viewBox 撑满容器宽度，柱数变了也不用改尺寸。
 */
export function Sparkline({
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
  if (!n) return <div style={{ height }} className={className} />
  // 每根柱占 1 个单位宽、留 0.25 的缝；viewBox 宽 = n，高 = 100，按容器拉伸
  const gap = 0.25
  return (
    <svg viewBox={`0 0 ${n} 100`} preserveAspectRatio="none" width="100%" height={height} className={className} aria-hidden>
      {prev?.map((v, i) => {
        const h = (v / max) * 100
        return h > 0 ? <rect key={`p${i}`} x={i} y={100 - h} width={1} height={h} fill="var(--muted-fg)" opacity={0.18} /> : null
      })}
      {requests.map((v, i) => {
        const h = (v / max) * 100
        const e = errors?.[i] ?? 0
        const eh = (e / max) * 100
        return (
          <g key={i}>
            <rect x={i + gap / 2} y={100 - h} width={1 - gap} height={h} fill="var(--chart-1)" opacity={0.8} />
            {eh > 0 && <rect x={i + gap / 2} y={100 - h} width={1 - gap} height={Math.max(eh, 2)} fill="var(--level-error)" />}
          </g>
        )
      })}
    </svg>
  )
}
