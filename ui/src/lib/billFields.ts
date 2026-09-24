/**
 * 账单表原始字段的中文说明与分组，明细「显示列」与表头用。
 *
 * 口径以 goscan 为准：阿里云取自 goscan `pkg/alicloud/types.go` 里 `BillDetail` 各字段的注释，
 * 分组照那里的 `=== … ===` 小标题；火山引擎的分组取自 goscan `pkg/ddl/tables.go` 建表列上的
 * `Section`，goscan 没有逐列注释，逐项说明按火山账单明细接口的字段含义补写，拿不准的留空，
 * 页面上就只显示字段名——宁可不写，不写错。
 *
 * 表里有、这里没列的列（goscan 日后加的新列）照常可选，归在「其他字段」。
 */
import type { BillProvider } from '@/api/types'

export interface FieldGroup {
  section: string
  /** 字段名 → 说明；说明为空串表示只列出、不作说明 */
  fields: Record<string, string>
}

const ALICLOUD: FieldGroup[] = [
  {
    section: '核心标识字段',
    fields: {
      instance_id: '实例 ID',
      instance_name: '实例名称',
      bill_account_id: '账单归属账号 ID',
      bill_account_name: '账单归属账号名称',
    },
  },
  { section: '时间字段', fields: { billing_cycle: '账期', billing_date: '账单日期（仅日粒度有值）' } },
  { section: '产品信息', fields: { product_code: '产品代码', product_name: '产品名称', product_type: '产品类型', product_detail: '产品明细' } },
  {
    section: '计费信息',
    fields: {
      subscription_type: '付费方式（Subscription 包年包月，PayAsYouGo 按量付费）',
      pricing_unit: '计费单位',
      currency: '币种',
      billing_type: '账单类型',
      item: '账单行类型（SubscriptionOrder 预付订单、PayAsYouGoBill 后付账单、Refund 退款、Adjustment 调账）',
    },
  },
  { section: '用量信息', fields: { usage: '用量', usage_unit: '用量单位' } },
  {
    section: '金额信息',
    fields: {
      pretax_gross_amount: '税前原价',
      invoice_discount: '开票折扣金额',
      deducted_by_coupons: '代金券抵扣金额',
      pretax_amount: '税前应付金额',
      currency_amount: '本币金额',
      payment_amount: '现金支付金额',
      outstanding_amount: '未结算金额',
    },
  },
  { section: '地域信息', fields: { region: '地域', zone: '可用区' } },
  { section: '规格信息', fields: { instance_spec: '实例规格', internet_ip: '公网 IP', intranet_ip: '内网 IP' } },
  { section: '资源组和标签', fields: { resource_group: '资源组', tags: '标签' } },
  {
    section: '其他信息',
    fields: {
      service_period: '服务周期',
      service_period_unit: '服务周期单位',
      list_price: '官网价格',
      list_price_unit: '官网价格单位',
      owner_id: '资源拥有者 ID',
    },
  },
  {
    section: '成本分摊',
    fields: { split_item_id: '分拆项 ID', split_item_name: '分拆项名称', split_account_id: '分拆账号 ID', split_account_name: '分拆账号名称' },
  },
  { section: '成本单元', fields: { cost_unit: '成本单元' } },
  { section: '订单信息', fields: { nick_name: '用户昵称', product_detail_code: '产品明细代码' } },
  { section: '账单归属', fields: { biz_type: '业务类型', adjust_type: '调整类型', adjust_amount: '调整金额' } },
  {
    section: '系统字段',
    fields: { line_seq: '同一次拉取中同键账单行的序号', granularity: '粒度（MONTHLY 月度，DAILY 日度）', created_at: '写入时间', updated_at: '更新时间' },
  },
]

const VOLCENGINE: FieldGroup[] = [
  { section: '核心标识字段', fields: { BillDetailId: '账单明细 ID', BillID: '账单 ID', InstanceNo: '实例 ID' } },
  {
    section: '时间字段',
    fields: {
      BillPeriod: '账期',
      BusiPeriod: '业务账期',
      ExpenseDate: '消费日期',
      ExpenseBeginTime: '消费开始时间',
      ExpenseEndTime: '消费结束时间',
      TradeTime: '交易时间',
    },
  },
  {
    section: '用户信息字段',
    fields: {
      PayerID: '付款方账号 ID',
      PayerUserName: '付款方用户名',
      PayerCustomerName: '付款方客户名称',
      SellerID: '销售方账号 ID',
      SellerUserName: '销售方用户名',
      SellerCustomerName: '销售方客户名称',
      OwnerID: '使用方账号 ID',
      OwnerUserName: '使用方用户名',
      OwnerCustomerName: '使用方客户名称',
    },
  },
  {
    section: '产品信息字段',
    fields: {
      Product: '产品代码',
      ProductZh: '产品名称',
      SolutionZh: '解决方案名称',
      Element: '计费项',
      ElementCode: '计费项代码',
      Factor: '计费因子',
      FactorCode: '计费因子代码',
    },
  },
  { section: '配置信息字段', fields: { ConfigName: '配置名称', ConfigurationCode: '配置代码', InstanceName: '实例名称' } },
  { section: '地域信息字段', fields: { Region: '地域', RegionCode: '地域代码', Zone: '可用区', ZoneCode: '可用区代码', CountryRegion: '国家 / 地区' } },
  {
    section: '计费模式信息',
    fields: {
      BillingMode: '计费模式',
      BusinessMode: '业务模式',
      BillingFunction: '计费方式',
      BillingMethodCode: '计费方式代码',
      SellingMode: '销售模式',
      SettlementType: '结算类型',
    },
  },
  {
    section: '用量信息字段',
    fields: { Count: '用量', Unit: '用量单位', UseDuration: '使用时长', UseDurationUnit: '使用时长单位', DeductionCount: '抵扣用量', DeductionUseDuration: '抵扣时长' },
  },
  {
    section: '价格信息字段',
    fields: { Price: '单价', PriceUnit: '单价单位', PriceInterval: '单价区间', MarketPrice: '市场价', MeasureInterval: '计量周期', Formula: '计费公式' },
  },
  {
    section: '金额信息字段',
    fields: {
      OriginalBillAmount: '原价',
      PreferentialBillAmount: '优惠金额',
      DiscountBillAmount: '折后金额',
      RoundAmount: '抹零金额',
      PayableAmount: '应付金额',
      PreTaxPayableAmount: '税前应付金额',
      SettlePayableAmount: '结算应付金额',
      SettlePreTaxPayableAmount: '结算税前应付金额',
      PretaxAmount: '税前金额',
      PosttaxAmount: '税后金额',
      SettlePretaxAmount: '结算税前金额',
      SettlePosttaxAmount: '结算税后金额',
      Tax: '税额',
      SettleTax: '结算税额',
      TaxRate: '税率',
      PaidAmount: '现金支付金额',
      UnpaidAmount: '未支付金额',
      CreditCarriedAmount: '',
    },
  },
  {
    section: '实际价值和结算信息',
    fields: { RealValue: '实际价值', PretaxRealValue: '税前实际价值', SettleRealValue: '结算实际价值', SettlePretaxRealValue: '结算税前实际价值' },
  },
  {
    section: '优惠和抵扣信息',
    fields: {
      CouponAmount: '代金券抵扣金额',
      DiscountInfo: '优惠信息',
      SavingPlanDeductionDiscountAmount: '节省计划抵扣的优惠金额',
      SavingPlanDeductionSpID: '节省计划 ID',
      SavingPlanOriginalAmount: '节省计划抵扣前的原价',
      ReservationInstance: '预留实例',
    },
  },
  { section: '货币信息', fields: { Currency: '币种', CurrencySettlement: '结算币种', ExchangeRate: '汇率' } },
  {
    section: '项目和分类信息',
    fields: { Project: '项目', ProjectDisplayName: '项目名称', BillCategory: '账单类别', SubjectName: '签约主体', Tag: '标签' },
  },
  {
    section: '折扣相关业务信息',
    fields: {
      DiscountBizBillingFunction: '折扣业务的计费方式',
      DiscountBizMeasureInterval: '折扣业务的计量周期',
      DiscountBizUnitPrice: '折扣业务的单价',
      DiscountBizUnitPriceInterval: '折扣业务的单价区间',
    },
  },
  { section: '其他业务信息', fields: { MainContractNumber: '主合同编号', OriginalOrderNo: '原订单号', EffectiveFactor: '', ExpandField: '扩展字段' } },
  { section: '系统字段', fields: { created_at: '写入时间', updated_at: '更新时间' } },
]

const DOCS: Record<BillProvider, FieldGroup[]> = { alicloud: ALICLOUD, volcengine: VOLCENGINE }

/** 表里有、说明里没列的字段归在这一组 */
export const OTHER_SECTION = '其他字段'

/** 某朵云某个字段的分组与说明；没收录的归「其他字段」、说明为空 */
export function fieldDoc(provider: BillProvider, name: string): { section: string; label: string; order: number } {
  const groups = DOCS[provider]
  for (let g = 0; g < groups.length; g++) {
    const label = groups[g].fields[name]
    if (label !== undefined) return { section: groups[g].section, label, order: g }
  }
  return { section: OTHER_SECTION, label: '', order: groups.length }
}
