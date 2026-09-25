import { formatTs } from '@/lib/time'

/**
 * 图表的一句话替代文本（WCAG 1.1.1）。
 *
 * 一张 SVG 图对读屏就是一团路径：不给名字的话它要么整个被跳过，要么把坐标轴上那一串数字
 * 一个个念出来——两种都等于没有。所以每张图都报 `role="img"` 加一句概要，把人扫一眼图能得到
 * 的东西说出来：什么图、看的是哪一段时间、每条线的量级。
 *
 * 概要由图自己按手上的数据算，不用每个调用点各写一遍——这个项目光指标看板就二十来张图。
 */
export function chartSummary(kind: string, fromMs: number, toMs: number, body: string): string {
  const span = `${formatTs(fromMs, { ms: false })} 到 ${formatTs(toMs, { ms: false })}`
  return body ? `${kind}，${span}，${body}` : `${kind}，${span}，此时段没有数据`
}

/** 系列多的时候只念前几条，剩下的报个数——念二十条没人听得完 */
export const MAX_SPOKEN_SERIES = 6

export function andMore(parts: string[], total: number): string {
  const rest = total - parts.length
  return parts.join('；') + (rest > 0 ? `；等 ${total} 条` : '')
}
