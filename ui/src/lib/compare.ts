/**
 * 「和哪一段时间比」，以及**具体是哪些接口变了**。
 *
 * 服务级的「P95 是昨天的 3 倍」只说明有事发生，能动手的信息是「是 `POST /order/submit` 拖的，
 * 它的 P95 从 120ms 变成 2.1s，这一小时多耗了 34 秒」。接口表两个窗口各查一次、按
 * `(span_name, span_kind)` 对齐之后，这里把一行行数字挑成一句句话——和 Datadog 的 endpoint
 * 变化列表、Cloudflare 的 Top 表是同一个套路：**先按性质分档，再按影响面排，不按百分比排**。
 *
 * 为什么不按百分比排：一小时 20 次的接口从 10ms 变成 40ms 是 +300%，排在「5 万次的接口从
 * 80ms 变成 120ms」前面——但后者才是用户在喊的那个。所以每一档都有自己的「影响面」：
 * 错误看多出来多少次错误，延迟看多耗的总时间（ΔP95 × 次数），流量看多出来多少次请求。
 */
import type { OperationStat } from '@/api/types'
import { formatChange, pct } from './health'
import { formatDurationMs, formatNumber } from './time'

/** 对比基线。默认昨天同时段：和上一小时比的话，白天永远在涨、晚上永远在跌 */
export const COMPARE = [
  { value: 'day', label: '和昨天同时段比', short: '昨天同时段' },
  { value: 'week', label: '和上周同时段比', short: '上周同时段' },
  { value: 'prev', label: '和上一周期比', short: '上一周期' },
] as const

export type Compare = (typeof COMPARE)[number]['value']

export const DEFAULT_COMPARE: Compare = 'day'

export function parseCompare(raw: string | null | undefined): Compare {
  return COMPARE.find((c) => c.value === raw)?.value ?? DEFAULT_COMPARE
}

/** 「昨天同时段」这种短名，句子里用 */
export function compareShort(c: Compare): string {
  return COMPARE.find((x) => x.value === c)!.short
}

/** 判一个接口「变慢了」之前，两个窗口各要有这么多次请求。P95 在几十个样本上就是噪声 */
const MIN_OP_SAMPLES = 100
/** P95 的倍数和绝对变化都要够：40ms → 52ms 是 +30%，但没人会为它起床 */
const P95_RATIO = 1.3
const P95_ABS_MS = 20
/** 错误：多出来（少掉）的错误次数，以及两边至少有一边的错误率像回事 */
const ERROR_MIN_DELTA = 5
const ERROR_MIN_RATE = 0.005
/** 流量：倍数和绝对次数都要够，不然几十次的接口翻个倍就上榜 */
const TRAFFIC_RATIO = 1.3
const TRAFFIC_MIN_DELTA = 100
/** 「整个接口没了 / 新出现」要有这么多次才值得提一句 */
const APPEAR_MIN = 100

export type MoverKind = 'gone' | 'errors' | 'latency' | 'new' | 'traffic'

export interface Mover {
  op: OperationStat
  kind: MoverKind
  /** 变好了（错误变少、变快了）。流量涨跌不算好坏，一律 false */
  better: boolean
  /** 变成什么样了：「P95 120ms → 2.1s（3.1×）」 */
  detail: string
  /** 影响面，行尾那个徽章：「多耗 34s」「+328 错误」 */
  impact: string
  tone: 'danger' | 'warn' | 'ok' | 'muted'
  /** 同一档里排序用的那个数，单位见各档注释 */
  score: number
}

/** 档位：坏消息在前，同档按影响面。变好的一律排到所有坏消息后面 */
const RANK: Record<MoverKind, number> = { gone: 0, errors: 1, latency: 2, new: 3, traffic: 4 }

function rank(m: Mover): number {
  return RANK[m.kind] + (m.better ? 10 : 0)
}

/**
 * 一个接口只出一条：档位从上往下判，先命中的算数（错误爆了的接口，不用再提它顺带变慢了）。
 * 两个窗口都没有的、量太小的全部不提——总览页加过同一条规则之后，6 个「异常」里 5 个是噪声。
 */
function moverFor(op: OperationStat): Mover | null {
  const prev = op.prev
  // 对比时段没有这个接口：新上的路由 / 刚改的 span 名
  if (!prev) {
    if (op.requests < APPEAR_MIN) return null
    return {
      op,
      kind: 'new',
      better: false,
      detail: '对比时段没有这个接口',
      impact: `新出现 ${formatNumber(op.requests)} 次`,
      tone: 'muted',
      score: op.requests,
    }
  }
  // 之前有、现在一次都没有：整条流量没了，比任何百分比都值得看一眼
  if (op.requests === 0) {
    if (prev.requests < APPEAR_MIN) return null
    return {
      op,
      kind: 'gone',
      better: false,
      detail: `${formatNumber(prev.requests)} 次 → 0 次`,
      impact: '本时段无调用',
      tone: 'warn',
      score: prev.requests,
    }
  }

  const dErrors = op.errors - prev.errors
  if (
    Math.abs(dErrors) >= ERROR_MIN_DELTA &&
    (op.error_rate >= ERROR_MIN_RATE || prev.error_rate >= ERROR_MIN_RATE)
  ) {
    const worse = dErrors > 0
    return {
      op,
      kind: 'errors',
      better: !worse,
      detail: `错误 ${formatNumber(prev.errors)} → ${formatNumber(op.errors)}，错误率 ${pct(prev.error_rate)} → ${pct(op.error_rate)}`,
      impact: `${worse ? '+' : '−'}${formatNumber(Math.abs(dErrors))} 错误`,
      tone: worse ? 'danger' : 'ok',
      // 影响面 = 多出来多少次失败的请求
      score: Math.abs(dErrors),
    }
  }

  const enough = op.requests >= MIN_OP_SAMPLES && prev.requests >= MIN_OP_SAMPLES
  const ratio = prev.p95_ms > 0 ? op.p95_ms / prev.p95_ms : null
  if (
    enough &&
    ratio !== null &&
    Math.abs(op.p95_ms - prev.p95_ms) >= P95_ABS_MS &&
    (ratio >= P95_RATIO || ratio <= 1 / P95_RATIO)
  ) {
    const slower = ratio >= 1
    // 影响面折算成时间：这一窗里多花（少花）的总时间 ≈ ΔP95 × 次数。手上只有分位数，
    // 拿 P95 当代表值算出来的是个量级——排序够用，不当账算
    const extraMs = (op.p95_ms - prev.p95_ms) * op.requests
    return {
      op,
      kind: 'latency',
      better: !slower,
      detail: `P95 ${formatDurationMs(prev.p95_ms)} → ${formatDurationMs(op.p95_ms)}（${formatChange(ratio - 1)}）`,
      impact: `耗时${slower ? '增加' : '减少'} ${formatDurationMs(Math.abs(extraMs))}`,
      tone: slower ? (ratio >= 2 ? 'danger' : 'warn') : 'ok',
      score: Math.abs(extraMs),
    }
  }

  const dRequests = op.requests - prev.requests
  const trafficRatio = prev.requests > 0 ? op.requests / prev.requests : Infinity
  if (
    Math.abs(dRequests) >= TRAFFIC_MIN_DELTA &&
    (trafficRatio >= TRAFFIC_RATIO || trafficRatio <= 1 / TRAFFIC_RATIO)
  ) {
    return {
      op,
      kind: 'traffic',
      better: false,
      detail: `次数 ${formatNumber(prev.requests)} → ${formatNumber(op.requests)}（${formatChange(trafficRatio - 1)}）`,
      impact: `${dRequests > 0 ? '+' : '−'}${formatNumber(Math.abs(dRequests))} 次`,
      tone: 'muted',
      score: Math.abs(dRequests),
    }
  }
  return null
}

/**
 * 接口级的「慢点」：服务汇总看不见、但该看一眼的那种慢。
 *
 * 服务健康度只有三个输入（错误率、服务级 P95 倍数、从没错到开始出错，见 `serviceHealth`），
 * 一个 605,323 次的服务里那个 149 次的接口从 31.8ms 变成 3.73s，三条全都不响：它占不到万分之
 * 三的流量，服务 P95 是别的接口定的。可它这一小时多耗了 9 分钟，正是该点进去看的那个。
 *
 * 判定接着 `moverFor` 那一档走（P95 至少翻倍才是 `danger`，样本、绝对变化的门槛都在那边），
 * 这里只多加一条影响面：多耗的总时间要占到窗口时长的 2%——一小时窗就是 72 秒。按窗口比例而
 * 不是绝对秒数，是因为同样的 72 秒在 24 小时窗里什么都不是；短窗口再兜一个 10 秒的下限。
 *
 * 2% 是照着线上量出来的：一小时窗、83 个服务，候选按多耗的时间排下来是
 * 4093s / 655s / 617s / 434s / 36s / 13s / 12s / 9s——真事故和噪声之间有一条一个数量级的缝，
 * 2% 正落在缝里。1% 会多收一个 169ms → 389ms、多耗 36 秒的接口，那不值一张大卡。
 */
const HOTSPOT_SHARE = 0.02
const HOTSPOT_MIN_MS = 10_000

export function latencyHotspot(ops: OperationStat[], windowMs: number): Mover | null {
  const floor = Math.max(HOTSPOT_MIN_MS, windowMs * HOTSPOT_SHARE)
  let worst: Mover | null = null
  for (const op of ops) {
    const m = moverFor(op)
    if (!m || m.kind !== 'latency' || m.better || m.tone !== 'danger') continue
    if (m.score < floor) continue
    if (!worst || m.score > worst.score) worst = m
  }
  return worst
}

/** 变化最大的几个接口，坏消息在前 */
export function topMovers(ops: OperationStat[], limit = 6): Mover[] {
  const movers = ops.map(moverFor).filter((m): m is Mover => m !== null)
  movers.sort((a, b) => rank(a) - rank(b) || b.score - a.score)
  return movers.slice(0, limit)
}
