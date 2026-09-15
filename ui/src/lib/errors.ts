/**
 * 一组报错怎么写成给人看的一行。
 *
 * 数据决定了这里必须分三档，不能假设「报错 = 异常类名」：线上一小时的错误 span 里只有约
 * 五分之一带 exception 事件，`status_message` 更是几乎全空（55989 条里 2 条）。所以
 * **异常类 → HTTP 响应码 → 只剩接口名**，一档一档退，退到最后也要给出「哪个接口在错」，
 * 剩下的交给展开后的日志堆栈。
 */
import type { ErrorGroup } from '@/api/types'

/** `java.net.SocketException` → `SocketException`。列表上包名占三分之一宽度还全都一样 */
export function shortException(fqcn: string): string {
  const i = fqcn.lastIndexOf('.')
  return i >= 0 && i < fqcn.length - 1 ? fqcn.slice(i + 1) : fqcn
}

/** 标题：一眼要能认出是哪一种错 */
export function errorTitle(g: ErrorGroup): string {
  if (g.exception) return shortException(g.exception)
  if (g.http_status) return `HTTP ${g.http_status}`
  return g.span_name || '(未知错误)'
}

/** 标题的 hover 全文：异常类名截短了，全名在这里 */
export function errorTitleFull(g: ErrorGroup): string {
  return g.exception || (g.http_status ? `HTTP ${g.http_status}` : g.span_name)
}

/**
 * 副标题：出错的是谁。Client / Producer span 的 `span_name` 只有 `GET` / `POST`，
 * 光写它等于没写，所以对外调用补上 `server.address`。
 */
export function errorWhere(g: ErrorGroup): string {
  const outbound = g.span_kind === 'Client' || g.span_kind === 'Producer'
  const name = outbound && g.peer ? `${g.span_name} → ${g.peer}` : g.span_name
  return `${g.service} · ${name}`
}

/**
 * 这一组有没有「具体报错」。没有的（500 但没有 exception 事件，多半是被全局异常处理器
 * 吞了）在列表上要标出来，并且展开后直接去日志里找堆栈——那才是唯一能拿到原因的地方。
 */
export function hasDetail(g: ErrorGroup): boolean {
  return !!g.exception
}
