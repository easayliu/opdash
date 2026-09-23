/**
 * 分析视图导出成 xlsx，给财务用。
 *
 * 数字全部取自 allocTable（与页面同一份计算），样式照财务那张《阿里云费用.xlsx》：表头深蓝底白字、
 * 合计行黄底。金额按数字写入并带千分位格式，拿到后能直接求和、做公式，而不是一格格文本。
 *
 * 写 xlsx 的库（write-excel-file）只在点「导出」时才加载：它不小，也不是每个人都会用到。
 */
import type { Cell, Row, SheetData } from 'write-excel-file/browser'
import type { BillAllocationResponse } from '@/api/types'
import { allocSection, allocSections, allocSummary, currentLabel, thisPeriod } from '@/lib/allocTable'
import { PROVIDER_LABELS, formatMoney, periodTick } from '@/lib/bills'

/**
 * 会计格式：零显示「-」、负数标红。满屏「0.00」会把真正有数的格子淹没（火山那段七行里五行全是 0），
 * 退款冲销的负数则要一眼看得见
 */
const MONEY = '#,##0.00;[Red]-#,##0.00;"-"'
const PERCENT = '0.0%;[Red]-0.0%;"-"'

/** 细边框：打印时网格线会消失，表格靠它成形；工作表本身关掉网格线，页面更干净 */
const BORDER = { borderStyle: 'thin', borderColor: '#BFBFBF' } as const
/** 与财务表同色：表头 1F4E78、合计行 FFE699 */
const HEADER = { ...BORDER, backgroundColor: '#1F4E78', textColor: '#FFFFFF', fontWeight: 'bold', align: 'center', alignVertical: 'center', wrap: true, height: 26 } as const
const TOTAL = { ...BORDER, backgroundColor: '#FFE699', fontWeight: 'bold', height: 22 } as const
const SUBTOTAL = { ...BORDER, backgroundColor: '#FFF2CC', fontWeight: 'bold' } as const
/** 小计列浅灰、日均与预估两列浅蓝：与实绩月份分开，一眼看出哪几列是汇总、哪几列是推算 */
const SUB_COL = { backgroundColor: '#F2F2F2' } as const
const EST_COL = { backgroundColor: '#DDEBF7' } as const
const ROW_H = 20
const SECTION_NUMBERS = ['一', '二', '三', '四', '五', '六']

export interface AllocExportOptions {
  data: BillAllocationResponse
  /** 业务线顺序（配置里的顺序） */
  order: string[]
  nights: number
  estimate: string
  /** 「应付」「现金」「原价」 */
  amountLabel: string
  amountHint?: string
  /** 「所选账期」「最近 7 天」…… */
  windowLabel: string
}

type Style = Record<string, unknown>
/** 金额写入前舍入到分：几十个两位小数相加会带出 122267.3758 这种浮点尾数，财务复制、做公式时会碰到 */
const cents = (v: number) => Math.round(v * 100) / 100
const money = (v: number | null | undefined, style: Style = {}): Cell =>
  v == null ? { ...BORDER, ...style } : { value: cents(v), type: Number, format: MONEY, align: 'right', ...BORDER, ...style }
const percent = (v: number | null | undefined, style: Style = {}): Cell =>
  v == null ? { ...BORDER, ...style } : { value: v, type: Number, format: PERCENT, align: 'right', ...BORDER, ...style }
const text = (v: string, style: Style = {}): Cell => ({ value: v, type: String, ...style })
const cellText = (v: string, style: Style = {}): Cell => text(v, { ...BORDER, alignVertical: 'center', ...style })
/** 跨满整个表宽的一行（标题、说明） */
const banner = (v: string, span: number, style: Style): Row => [text(v, { columnSpan: span, alignVertical: 'center', ...style }), ...Array.from({ length: span - 1 }, () => null)]
/** 空行：库会丢掉完全为空的行，所以放一个空格并给定行高 */
const spacer = (h = 10): Row => [text(' ', { height: h })]

export async function exportAllocXlsx(opts: AllocExportOptions): Promise<void> {
  const { data, order, nights, estimate, amountLabel, amountHint, windowLabel } = opts
  const { default: writeXlsxFile } = await import('write-excel-file/browser')
  const current = thisPeriod()
  const monthHeader = (p: string) => (p === current ? `${periodTick(p)}\n（${currentLabel(data, p)}）` : periodTick(p))
  const range = `${data.from} 至 ${data.to}`
  const exportedAt = new Date().toLocaleString('zh-CN', { hour12: false })

  // ---- 按月拆分：各段依次排下，段与段之间空一行
  const periodCount = allocSection(data, order, 'all', nights, estimate).periods.length
  const splitCols = 1 + periodCount + 4
  const split: SheetData = [
    banner(`业务线月度拆分（${range}，${amountLabel}口径）`, splitCols, { fontWeight: 'bold', fontSize: 15, height: 30 }),
    banner(`导出时间 ${exportedAt}　日均按${windowLabel}计算　预估账期 ${estimate}（${nights} 天）　口径详见「口径说明」`, splitCols, { textColor: '#595959', height: 18 }),
  ]
  allocSections(data).forEach((sec, i) => {
    const t = allocSection(data, order, sec.key, nights, estimate)
    split.push(spacer(14))
    split.push([text(`${SECTION_NUMBERS[i] ?? i + 1}、${sec.label}`, { fontWeight: 'bold', fontSize: 12, textColor: '#1F4E78', height: 22, alignVertical: 'bottom' })])
    split.push([
      text('业务线', HEADER),
      ...t.periods.map((p) => text(monthHeader(p), HEADER)),
      text('小计', HEADER),
      text('占比', HEADER),
      text('日均', HEADER),
      text(`${estimate} 预估`, HEADER),
    ])
    for (const r of t.rows) {
      split.push([
        cellText(r.key === null ? '未归属（未命中规则）' : r.name, { height: ROW_H, ...(r.key === null ? { textColor: '#7F7F7F' } : {}) }),
        ...t.periods.map((p) => money(r.byPeriod[p] ?? 0)),
        money(r.subtotal, SUB_COL),
        percent(r.share),
        money(r.daily, EST_COL),
        money(r.projected, EST_COL),
      ])
    }
    split.push([
      cellText(sec.label, TOTAL),
      ...t.periods.map((p) => money(t.colTotals[p], TOTAL)),
      money(t.total, TOTAL),
      percent(t.total ? 1 : null, TOTAL),
      money(t.footDaily, TOTAL),
      money(t.footProject, TOTAL),
    ])
  })

  // ---- 按云厂商与付费方式汇总
  const sum = allocSummary(data)
  const summaryCols = 1 + sum.periods.length + 1
  const summary: SheetData = [
    banner(`按云厂商与付费方式汇总（${range}，${amountLabel}口径）`, summaryCols, { fontWeight: 'bold', fontSize: 15, height: 30 }),
    banner(`导出时间 ${exportedAt}　与「按月拆分」各段的合计行一致`, summaryCols, { textColor: '#595959', height: 18 }),
    spacer(14),
    [cellText('云厂商 · 付费方式', HEADER), ...sum.periods.map((p) => text(monthHeader(p), HEADER)), text('小计', HEADER)],
    ...sum.rows.map((r) => {
      const style = r.tone === 'total' ? TOTAL : r.tone === 'sub' ? SUBTOTAL : {}
      return [cellText(r.label, { height: ROW_H, ...style }), ...sum.periods.map((p) => money(r.byPeriod[p], style)), money(r.subtotal, { ...SUB_COL, ...style })]
    }),
  ]

  // ---- 按产品：不分业务线，与账单本身一致；末尾补一行合计
  const productTotal = (pick: (it: BillAllocationResponse['products'][number]) => number | null) =>
    data.products.reduce<number | null>((n, it) => {
      const v = pick(it)
      return v == null ? n : (n ?? 0) + v
    }, null)
  const products: SheetData = [
    banner(`按产品（${range}，${amountLabel}口径）`, 5, { fontWeight: 'bold', fontSize: 15, height: 30 }),
    banner(`导出时间 ${exportedAt}　不区分业务线，与账单口径一致；日均按${windowLabel}计算`, 5, { textColor: '#595959', height: 18 }),
    spacer(14),
    [text('产品', HEADER), text('类型', HEADER), text('金额', HEADER), text('日均', HEADER), text('月度预估', HEADER)],
    ...data.products.map((it) => [
      cellText(it.product, { height: ROW_H }),
      cellText(it.prepaid ? '预付费摊销' : '后付费', { align: 'center', ...(it.prepaid ? { textColor: '#2F75B5' } : { textColor: '#595959' }) }),
      money(it.amount),
      money(it.daily, EST_COL),
      money(it.daily == null ? null : it.daily * nights, EST_COL),
    ]),
    [
      cellText('合计', TOTAL),
      cellText('', TOTAL),
      money(productTotal((it) => it.amount), TOTAL),
      money(productTotal((it) => it.daily), TOTAL),
      money(productTotal((it) => (it.daily == null ? null : it.daily * nights)), TOTAL),
    ],
  ]

  // ---- 口径说明：与财务表表头那几行说明同一个作用
  const notes = [
    `统计范围：${range}${data.window_days ? `（金额只含最近 ${data.window_days} 天）` : ''}；云厂商：${[...new Set(data.monthly.map((r) => PROVIDER_LABELS[r.provider]))].join('、') || '—'}。`,
    `金额口径：${amountLabel}${amountHint ? `——${amountHint}` : ''}。`,
    '后付费计入出账所在的账期；预付费（包年包月）按服务期摊入各账期，未配置预付费规则时同样计入出账所在的账期。',
    `日均 = 统计窗口（${windowLabel}）内的后付费 ÷ 有账单的天数，各云厂商按各自的天数折算；预付费摊销不计入日均。`,
    `月度预估 = 后付费日均 × 预估账期的自然天数（${estimate}，${nights} 天）+ 该账期的预付费摊销。分段中，后付费的预估为该云厂商的日均 × 天数，预付费摊销的预估为摊入该账期的金额。`,
    '按月拆分按整月统计，不受日均统计窗口影响；当月一列只含截至目前已出具的账单。',
    data.unmatched_into && data.unmatched.amount > 0
      ? `未命中任何归属规则的费用（${formatMoney(data.unmatched.amount)}）已按配置计入「${data.unmatched_into}」，占 ${(data.unmatched.share * 100).toFixed(1)}%。`
      : data.unmatched.amount > 0
        ? `未命中任何归属规则的费用（${formatMoney(data.unmatched.amount)}）单列为「未归属」。`
        : '所有费用均已命中归属规则。',
    '金额单元格为会计格式：零显示为「-」，负数（退款、冲销）以红色显示。',
    `数据来源：opdash 分析视图（goscan 同步的云账单），数字与页面一致。导出时间 ${exportedAt}。`,
  ]
  const readme: SheetData = [
    [text('口径说明', { fontWeight: 'bold', fontSize: 15, height: 30, alignVertical: 'center' })],
    spacer(8),
    ...notes.map((n, i) => [text(`${i + 1}. ${n}`, { wrap: true, alignVertical: 'center', height: n.length > 60 ? 36 : 22 })]),
  ]

  const monthCols = (n: number) => Array.from({ length: n }, () => ({ width: 14 }))
  // 宽表横向打印；网格线关掉，表格靠边框成形
  const page = { showGridLines: false, orientation: 'landscape' } as const
  await writeXlsxFile(
    [
      { sheet: '按月拆分', data: split, columns: [{ width: 24 }, ...monthCols(periodCount), { width: 15 }, { width: 9 }, { width: 12 }, { width: 15 }], stickyColumnsCount: 1, ...page },
      { sheet: '按云厂商与付费方式汇总', data: summary, columns: [{ width: 24 }, ...monthCols(sum.periods.length), { width: 16 }], stickyColumnsCount: 1, ...page },
      { sheet: '按产品', data: products, columns: [{ width: 38 }, { width: 12 }, { width: 16 }, { width: 13 }, { width: 16 }], stickyRowsCount: 4, ...page },
      { sheet: '口径说明', data: readme, columns: [{ width: 100 }], showGridLines: false },
    ],
    // 等线是 Excel 中文版的默认字体，Windows 与 macOS 的 Office 都自带
    { fontFamily: '等线', fontSize: 11 },
  ).toFile(`费用拆分_${data.from}_${data.to}.xlsx`)
}
