// 关键字语法，和后端 src/query/logs.rs 的 parse_query 保持一致：
// 空格 = AND；`OR` 任一；`-词` / `NOT 词` 排除；`"带 空格"` 整体；`( )` 分组；
// AND / OR / NOT 全大写才是操作符。前端只用它算要高亮的正向词。

export type Expr =
  | { kind: 'term'; value: string }
  | { kind: 'not'; inner: Expr }
  | { kind: 'and'; parts: Expr[] }
  | { kind: 'or'; parts: Expr[] }

type Token =
  | { t: 'word'; v: string }
  | { t: 'phrase'; v: string }
  | { t: 'and' }
  | { t: 'or' }
  | { t: 'not' }
  | { t: 'lparen' }
  | { t: 'rparen' }

const isSpace = (c: string | undefined) => c !== undefined && /\s/.test(c)

function tokenize(q: string): Token[] {
  const out: Token[] = []
  let i = 0
  outer: while (i < q.length) {
    while (isSpace(q[i])) i++
    // 前缀：`(` 开组，`-` 取反（后面要紧跟内容）
    for (;;) {
      if (q[i] === '(') {
        out.push({ t: 'lparen' })
        i++
      } else if (q[i] === '-') {
        const next = q[i + 1]
        i++
        if (next === undefined || isSpace(next) || next === ')') continue outer
        out.push({ t: 'not' })
      } else break
    }
    if (i >= q.length) break
    if (q[i] === ')') {
      out.push({ t: 'rparen' })
      i++
      continue
    }
    if (q[i] === '"') {
      i++
      let phrase = ''
      while (i < q.length) {
        const c = q[i++]
        if (c === '"') break
        if (c === '\\') {
          const e = q[i]
          if (e === '"' || e === '\\') {
            phrase += e
            i++
          } else phrase += '\\'
          continue
        }
        phrase += c
      }
      if (phrase) out.push({ t: 'phrase', v: phrase })
      continue
    }
    let word = ''
    while (i < q.length && !isSpace(q[i])) word += q[i++]
    // 词尾多出来的 `)` 是关组；词内配对的括号照字面
    let closers = 0
    while (word.endsWith(')')) {
      const opens = (word.match(/\(/g) ?? []).length
      const closes = (word.match(/\)/g) ?? []).length
      if (closes <= opens) break
      word = word.slice(0, -1)
      closers++
    }
    if (word === 'AND') out.push({ t: 'and' })
    else if (word === 'OR') out.push({ t: 'or' })
    else if (word === 'NOT') out.push({ t: 'not' })
    else if (word) out.push({ t: 'word', v: word })
    for (let k = 0; k < closers; k++) out.push({ t: 'rparen' })
  }
  return out
}

function joinAnd(parts: Expr[]): Expr | undefined {
  const flat = parts.flatMap((p) => (p.kind === 'and' ? p.parts : [p]))
  return flat.length === 0 ? undefined : flat.length === 1 ? flat[0] : { kind: 'and', parts: flat }
}

function joinOr(parts: Expr[]): Expr | undefined {
  const flat = parts.flatMap((p) => (p.kind === 'or' ? p.parts : [p]))
  return flat.length === 0 ? undefined : flat.length === 1 ? flat[0] : { kind: 'or', parts: flat }
}

class Parser {
  pos = 0
  private tokens: Token[]

  constructor(tokens: Token[]) {
    this.tokens = tokens
  }

  peek(): Token | undefined {
    return this.tokens[this.pos]
  }

  bump(): Token | undefined {
    return this.tokens[this.pos++]
  }

  parseOr(): Expr | undefined {
    const parts: Expr[] = []
    const first = this.parseAnd()
    if (first) parts.push(first)
    while (this.peek()?.t === 'or') {
      this.bump()
      const e = this.parseAnd()
      if (e) parts.push(e)
    }
    return joinOr(parts)
  }

  parseAnd(): Expr | undefined {
    const parts: Expr[] = []
    for (;;) {
      const t = this.peek()?.t
      if (t === undefined || t === 'or' || t === 'rparen') break
      if (t === 'and') {
        this.bump()
        continue
      }
      const e = this.parseUnary()
      if (e) parts.push(e)
    }
    return joinAnd(parts)
  }

  parseUnary(): Expr | undefined {
    const tok = this.bump()
    if (!tok) return undefined
    switch (tok.t) {
      case 'not': {
        const inner = this.parseUnary()
        if (!inner) return undefined
        return inner.kind === 'not' ? inner.inner : { kind: 'not', inner }
      }
      case 'lparen': {
        const inner = this.parseOr()
        if (this.peek()?.t === 'rparen') this.bump()
        return inner
      }
      case 'word':
      case 'phrase':
        return { kind: 'term', value: tok.v }
      default:
        return undefined
    }
  }
}

/** 解析关键字串；只有空白 / 操作符时返回 undefined */
export function parseQuery(q: string): Expr | undefined {
  const p = new Parser(tokenize(q))
  const parts: Expr[] = []
  while (p.peek()) {
    const before = p.pos
    if (p.peek()?.t === 'rparen') {
      p.bump()
      continue
    }
    const e = p.parseOr()
    if (e) parts.push(e)
    if (p.pos === before) p.bump()
  }
  return joinAnd(parts)
}

/** 要高亮的词：不在 NOT 下面的所有词 */
export function positiveTerms(q: string): string[] {
  const out: string[] = []
  const walk = (e: Expr, negated: boolean) => {
    switch (e.kind) {
      case 'term':
        if (!negated) out.push(e.value)
        break
      case 'not':
        walk(e.inner, !negated)
        break
      default:
        e.parts.forEach((p) => walk(p, negated))
    }
  }
  const expr = parseQuery(q)
  if (expr) walk(expr, false)
  return out
}
