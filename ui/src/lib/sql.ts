import type { AttrValue } from '@/api/types'

/**
 * 把 span 属性里的 db.statement（带 ? 占位符）和 db.query.parameter.N 拼回一句能直接跑的 SQL。
 * 没有 statement 或者一个参数都没有就返回 null，界面上就不显示这一块。
 */
export function fillSqlParams(attrs: Record<string, AttrValue>): string | null {
  const stmt = attrs['db.statement']
  if (typeof stmt !== 'string' || !stmt) return null
  const params = collectParams(attrs)
  if (!params.length) return null
  return substitute(stmt, params)
}

/** 按 N 的数字顺序取出 db.query.parameter.0、.1、…（也兼容 db.query.parameter.<N> 形式）。 */
export function collectParams(attrs: Record<string, AttrValue>): AttrValue[] {
  const found: [number, AttrValue][] = []
  for (const [k, v] of Object.entries(attrs)) {
    const m = /^db\.query\.parameter\.(\d+)$/.exec(k)
    if (m) found.push([Number(m[1]), v])
  }
  found.sort((a, b) => a[0] - b[0])
  return found.map(([, v]) => v)
}

/** 参数值转成 SQL 字面量：数字裸写，布尔 TRUE/FALSE，null → NULL，其它按单引号字符串（' 翻倍转义）。 */
export function sqlLiteral(v: AttrValue): string {
  if (v === null || v === undefined) return 'NULL'
  if (typeof v === 'number') return Number.isFinite(v) ? String(v) : 'NULL'
  if (typeof v === 'boolean') return v ? 'TRUE' : 'FALSE'
  if (typeof v === 'string') {
    if (/^-?\d+(\.\d+)?$/.test(v) && !/^-?0\d/.test(v)) return v
    return `'${v.replace(/\\/g, '\\\\').replace(/'/g, "''")}'`
  }
  return `'${JSON.stringify(v).replace(/\\/g, '\\\\').replace(/'/g, "''")}'`
}

/** 逐个把语句里的 ? 换成参数；引号里的 ? 和注释里的 ? 不动。参数不够就保留 ?。 */
function substitute(stmt: string, params: AttrValue[]): string {
  let out = ''
  let i = 0
  let p = 0
  const n = stmt.length
  while (i < n) {
    const c = stmt[i]
    if (c === "'" || c === '"' || c === '`') {
      const start = i
      i++
      while (i < n) {
        if (stmt[i] === '\\') i += 2
        else if (stmt[i] === c) {
          i++
          if (stmt[i] === c) i++ // '' 转义
          else break
        } else i++
      }
      out += stmt.slice(start, i)
      continue
    }
    if (c === '-' && stmt[i + 1] === '-') {
      const end = stmt.indexOf('\n', i)
      const stop = end === -1 ? n : end
      out += stmt.slice(i, stop)
      i = stop
      continue
    }
    if (c === '/' && stmt[i + 1] === '*') {
      const end = stmt.indexOf('*/', i + 2)
      const stop = end === -1 ? n : end + 2
      out += stmt.slice(i, stop)
      i = stop
      continue
    }
    if (c === '?') {
      out += p < params.length ? sqlLiteral(params[p++]) : '?'
      i++
      continue
    }
    out += c
    i++
  }
  return out
}

/**
 * 美化 SQL：按子句换行、列表一行一项、关键字大写。
 * sql-formatter 全量三百多 KB，只在 span 面板用得着，所以动态 import 拆成单独的 chunk，
 * 而且只带 mysql / postgresql / 通用三种方言。格式化失败（方言不支持的语法）就原样返回。
 */
export async function formatSql(sql: string, dbSystem?: AttrValue): Promise<string> {
  const { formatDialect, mysql, postgresql, sql: generic } = await import('sql-formatter')
  const system = typeof dbSystem === 'string' ? dbSystem.toLowerCase() : ''
  const dialect = system === 'mysql' || system === 'mariadb' || system === 'tidb' ? mysql : system === 'postgresql' || system === 'postgres' ? postgresql : generic
  try {
    return formatDialect(sql, { dialect, keywordCase: 'upper', tabWidth: 2 })
  } catch {
    return sql
  }
}
