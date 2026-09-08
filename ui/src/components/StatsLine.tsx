import type { Stats } from '@/api/types'
import { formatBytes, formatNumber } from '@/lib/time'

/** 「扫描 1.2M 行 · 2.0 GB · 308 ms」——让人知道这次查询贵不贵。 */
export function StatsLine({ stats, className }: { stats?: Stats; className?: string }) {
  if (!stats) return null
  return (
    <span className={className ?? 'text-2xs text-muted-fg'} title="ClickHouse 本次查询扫描的数据量和耗时">
      扫描 {formatNumber(stats.read_rows)} 行 · {formatBytes(stats.read_bytes)} · {Math.round(stats.elapsed_ms)} ms
    </span>
  )
}
