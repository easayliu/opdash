import { describe, expect, it } from 'vitest'
import { buildTree } from '@/components/Waterfall'
import type { Span } from '@/api/types'

function span(id: string, parent: string | null, startUs: number): Span {
  return {
    span_id: id,
    parent_span_id: parent ?? '',
    service: 'order-service',
    name: id,
    kind: 'Server',
    start_us: startUs,
    duration_ns: 1_000_000,
    status: 'Ok',
    status_message: '',
    scope_name: '',
    scope_version: '',
    trace_state: '',
    attributes: {},
    resource: {},
    events: [],
    links: [],
    extra: {},
  }
}

describe('buildTree', () => {
  /**
   * 树的 `aria-posinset` / `aria-setsize` 说的是「同层兄弟里的第几个」，不是拍平之后的行号。
   * 这两个数就是在建树时算的，算错了读屏会把一棵三层的树念成一条直线。
   */
  it('位置和总数按层算，不是按拍平后的行号', () => {
    const tree = buildTree([span('root', null, 0), span('a', 'root', 10), span('b', 'root', 20), span('a1', 'a', 15)])
    const root = tree.roots[0]
    expect([root.posinset, root.setsize]).toEqual([1, 1])

    const [a, b] = root.children
    expect([a.span.span_id, a.posinset, a.setsize]).toEqual(['a', 1, 2])
    expect([b.span.span_id, b.posinset, b.setsize]).toEqual(['b', 2, 2])

    // a1 是 a 的独生子：拍平之后它是第 3 行，但同层里它是「第 1 个，共 1 个」
    const a1 = a.children[0]
    expect([a1.span.span_id, a1.posinset, a1.setsize, a1.depth]).toEqual(['a1', 1, 1, 2])
  })

  it('父 span 不在结果里的挂到顶层并标记出来', () => {
    const tree = buildTree([span('x', 'missing', 0)])
    expect(tree.roots).toHaveLength(1)
    expect(tree.roots[0].orphan).toBe(true)
  })
})
