/**
 * 图表配色。具体色值在 index.css 的 --chart-N / --level-* 里（浅色深色各一套，经 dataviz 校验器校验），
 * 这里只按「角色」取：类别色按首次出现顺序固定分配、不循环，超过 8 个折进「其它」。
 */

export const SERIES_SLOTS = 8

export function seriesVar(slot: number): string {
  return slot < SERIES_SLOTS ? `var(--chart-${slot + 1})` : 'var(--chart-other)'
}

/** 给一组名字（服务名）按首次出现顺序分配固定的颜色槽位。 */
export class ColorAssigner {
  private slots = new Map<string, number>()
  color(name: string): string {
    let slot = this.slots.get(name)
    if (slot === undefined) {
      slot = this.slots.size
      this.slots.set(name, slot)
    }
    return seriesVar(slot)
  }
  entries(): [string, string][] {
    return [...this.slots.entries()].map(([name, slot]) => [name, seriesVar(slot)])
  }
}

export function levelColor(level: string): string {
  switch (level.toUpperCase()) {
    case 'ERROR':
    case 'FATAL':
      return 'var(--level-error)'
    case 'WARN':
    case 'WARNING':
      return 'var(--level-warn)'
    case 'INFO':
      return 'var(--level-info)'
    case 'DEBUG':
      return 'var(--level-debug)'
    default:
      return 'var(--level-trace)'
  }
}
