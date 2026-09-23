/** react-query 封装。所有查询都用 placeholderData 保留上一次结果：重新查询时图表按住旧画面变淡，不闪白。 */
import { keepPreviousData, useQueries, useQuery } from '@tanstack/react-query'
import { useMemo } from 'react'
import { apiGet, type Params } from './client'
import type {
  ApiKeyInfo,
  AuthMe,
  BillAllocationResponse,
  BillBreakdownResponse,
  BillDailyResponse,
  BillDetailResponse,
  BillPeriodsResponse,
  BillSummaryResponse,
  BillSyncTask,
  ContextResponse,
  ErrorsResponse,
  FacetsResponse,
  HeatmapResponse,
  HistogramResponse,
  KeysResponse,
  LogRow,
  LogSearchResponse,
  Meta,
  MetricCatalogResponse,
  MetricEventsResponse,
  MetricExemplarsResponse,
  MetricNamesResponse,
  MetricQueryResponse,
  SpanAttrsResponse,
  OperationsResponse,
  OverviewResponse,
  SavedQueryList,
  Stats,
  TimeseriesResponse,
  TraceDetailResponse,
  TraceSearchResponse,
  ValuesResponse,
} from './types'

export function useAuthMe() {
  return useQuery({
    queryKey: ['auth', 'me'],
    queryFn: () => apiGet<AuthMe>('/auth/me'),
    staleTime: 5 * 60_000,
    retry: false,
  })
}

/** 我的 API key 列表；对话框打开时才查。 */
export function useApiKeys(enabled: boolean) {
  return useQuery({
    queryKey: ['auth', 'keys'],
    queryFn: () => apiGet<{ keys: ApiKeyInfo[] }>('/auth/keys'),
    enabled,
    retry: false,
  })
}

/** 我的收藏。顶栏的书签图标要靠它判断「当前视图收藏过没有」，所以一直开着 */
export function useSavedQueries() {
  return useQuery({
    queryKey: ['saved'],
    queryFn: () => apiGet<SavedQueryList>('/saved'),
    staleTime: 5 * 60_000,
    retry: false,
  })
}

export function useMeta() {
  return useQuery({ queryKey: ['meta'], queryFn: () => apiGet<Meta>('/meta'), staleTime: 5 * 60_000 })
}

export function useLogSearch(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['logs', 'search', params],
    queryFn: ({ signal }) => apiGet<LogSearchResponse>('/logs/search', params, signal),
    placeholderData: keepPreviousData,
    enabled,
  })
}

export interface TraceLogs {
  rows: LogRow[]
  /** 库里带这个 trace id 的总条数。只有 `truncated` 时才去数，正常路径不为它多扫一遍 */
  total?: number
  /** 拉满 [`TRACE_LOG_MAX_PAGES`] 页还没到头，剩下的没拉 */
  truncated: boolean
  stats: Stats
}

/**
 * 这一页最多拉几页日志。
 *
 * 以前是拉到服务端的翻页上限（max_offset，默认 10000）为止，也就是最多 11 趟。按 trace id 查
 * 没有排序键可用，`OFFSET` 又是「读满 offset + limit 行再把前面的丢掉」，所以页越深越贵：
 * 线上一条 trace id 被复用、挂着 26 万条日志的（常驻消费者一直用同一个 id），第 1 页读
 * 31.9 M 行 / 1.46 GB，第 10 页读 100.2 M 行 / 9.97 GB，11 趟串行累计 60 GB 以上、几十秒，
 * 最后还是弹一个「未拉全」。
 *
 * 那些日志本来就不属于用户正在看的这一次请求。超过两页就停下来说清楚，比闷头拉完有用。
 */
const TRACE_LOG_MAX_PAGES = 2

/**
 * 一条 trace 的全部日志：一页页拉到没有为止，最多 [`TRACE_LOG_MAX_PAGES`] 页。
 * 一条 trace 的日志通常几十到几百条，一趟就完。
 *
 * 不传 `count`：服务端默认会并发一条 `count()` 把总数也数出来，而按 trace id 查没有索引能让
 * 它提前停，等于把同一段数据再扫一遍（线上实测 32.7 M 行 / 120.7 MB）。页面只在「没拉全」
 * 时才需要这个总数，所以挪到下面那条路上单独要。
 */
export function useTraceLogs(
  filter: { trace_id: string; from?: number; to?: number },
  limits: { max_rows: number } | undefined,
  enabled = true,
) {
  return useQuery({
    queryKey: ['traces', 'logs', filter, limits],
    queryFn: async ({ signal }): Promise<TraceLogs> => {
      const limit = limits?.max_rows ?? 1000
      const rows: LogRow[] = []
      const stats: Stats = { read_rows: 0, read_bytes: 0, result_rows: 0, elapsed_ms: 0 }
      const absorb = (s: Stats) => {
        stats.read_rows += s.read_rows
        stats.read_bytes += s.read_bytes
        stats.result_rows += s.result_rows
        stats.elapsed_ms += s.elapsed_ms
      }
      let offset = 0
      for (;;) {
        const page = await apiGet<LogSearchResponse>('/logs/search', { ...filter, order: 'asc', limit, offset, count: false }, signal)
        rows.push(...page.rows)
        absorb(page.stats)
        if (page.rows.length < limit) return { rows, truncated: false, stats }
        offset += limit
        if (offset >= limit * TRACE_LOG_MAX_PAGES) {
          // 到这儿说明 trace id 多半被复用了。只有这一条路需要总数，才去数
          const counted = await apiGet<LogSearchResponse>('/logs/search', { ...filter, order: 'asc', limit: 1, offset: 0, count: true }, signal)
          absorb(counted.stats)
          return { rows, total: counted.total, truncated: true, stats }
        }
      }
    },
    placeholderData: keepPreviousData,
    // trace 是不会变的历史数据，开关日志页签、来回点 span 不该重打
    staleTime: 10 * 60_000,
    enabled: enabled && !!limits,
  })
}

/** 错误分组。默认 `kind=entry`（入口 span），和服务总览上那个错误率同一口径 */
export function useErrorGroups(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['errors', params],
    queryFn: ({ signal }) => apiGet<ErrorsResponse>('/errors', params, signal),
    placeholderData: keepPreviousData,
    enabled,
  })
}

export function useLogHistogram(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['logs', 'histogram', params],
    queryFn: ({ signal }) => apiGet<HistogramResponse>('/logs/histogram', params, signal),
    placeholderData: keepPreviousData,
    enabled,
  })
}

/**
 * 筛选下拉的候选值，**所有维度一条查询**。
 *
 * 以前是一个维度一条：光是打开日志页就发 4 条（服务 / 命名空间 / pod / 容器），点「更多
 * 筛选」再发 10 条，14 条的 WHERE 一模一样、只差分组的那一列。线上 1 小时窗实测每条都要
 * 扫 916 万行，4 条合起来 3666 万行 / 591.6 MB——只为了填几个下拉框。一条 `approx_top_k`
 * 把 14 个维度并成一次扫描：911 万行 / 439.6 MB，比原来光打开页面那 4 条还便宜。
 */
export function useLogFacets(fields: string[], params: Params, enabled = true) {
  const list = fields.join(',')
  return useQuery({
    queryKey: ['logs', 'facets', list, params],
    queryFn: ({ signal }) => apiGet<FacetsResponse>('/logs/facets', { ...params, field: list }, signal),
    placeholderData: keepPreviousData,
    staleTime: 60_000,
    enabled: enabled && !!list,
  })
}

export function useLogContext(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['logs', 'context', params],
    queryFn: ({ signal }) => apiGet<ContextResponse>('/logs/context', params, signal),
    placeholderData: keepPreviousData,
    enabled,
  })
}

export function useTraceSearch(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['traces', 'search', params],
    queryFn: ({ signal }) => apiGet<TraceSearchResponse>('/traces/search', params, signal),
    placeholderData: keepPreviousData,
    enabled,
  })
}

export function useTraceHeatmap(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['traces', 'heatmap', params],
    queryFn: ({ signal }) => apiGet<HeatmapResponse>('/traces/heatmap', params, signal),
    placeholderData: keepPreviousData,
    enabled,
  })
}

/**
 * 一条 trace 的瀑布图。
 *
 * `at` 是 trace 的开始时间（unix 毫秒），给了服务端只查它前后的分区，快很多。
 *
 * `span` 是**打开这个页面时** URL 上指名要选中的那个（分享的链接、错误分组给的样本）：span 数
 * 超过 `--max-trace-spans` 时截断按时间从早往晚切，指名的那个常常正好在被切掉的后半段，
 * 服务端会单独把它取回来。**只能是进页面时的那一个**：跟着当前选中走的话，点一下瀑布图就换了
 * query key，整条详情要重查一遍。
 */
export function useTraceDetail(traceId: string | undefined, at?: string | null, span?: string | null) {
  return useQuery({
    queryKey: ['traces', 'detail', traceId, at ?? null, span ?? null],
    queryFn: ({ signal }) =>
      apiGet<TraceDetailResponse>(
        `/traces/${encodeURIComponent(traceId ?? '')}`,
        { at: at ?? undefined, span: span ?? undefined },
        signal,
      ),
    enabled: !!traceId,
    // 已经跑完的 trace 不会再变，重新打一遍要走一整套定位 + 取数
    staleTime: 10 * 60_000,
  })
}

/**
 * 一个 span 的属性 / events / links。详情那一趟故意不取这四个 JSON 列——线上实测它们就是
 * 整条查询的全部成本（0.259 GB / 1.8~43 s 对 0.002 GB / 50 ms），点开哪个 span 才查哪个。
 */
export function useSpanAttrs(
  traceId: string | undefined,
  spanId: string | null,
  /** 这个 span 的排序键前缀（service_name / span_name / 毫秒时间戳），详情响应里都有。
   *  带上服务端就不用先跑一遍定位查询：线上实测 0.11 GB → 0.001 GB 量级 */
  hint?: { service: string; name: string; ts_ms: number },
  at?: string | null,
) {
  return useQuery({
    queryKey: ['traces', 'span_attrs', traceId, spanId, at ?? null],
    queryFn: ({ signal }) =>
      apiGet<SpanAttrsResponse>(
        `/traces/${encodeURIComponent(traceId ?? '')}/spans/${encodeURIComponent(spanId ?? '')}`,
        { at: at ?? undefined, service: hint?.service, name: hint?.name, ts: hint?.ts_ms },
        signal,
      ),
    enabled: !!traceId && !!spanId,
    staleTime: 5 * 60_000,
  })
}

export function useTraceValues(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['traces', 'values', params],
    queryFn: ({ signal }) => apiGet<ValuesResponse>('/traces/values', params, signal),
    placeholderData: keepPreviousData,
    staleTime: 60_000,
    enabled,
  })
}

export function useAttrKeys(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['traces', 'attr_keys', params],
    queryFn: ({ signal }) => apiGet<KeysResponse>('/traces/attr_keys', params, signal),
    staleTime: 5 * 60_000,
    enabled,
  })
}

export function useAttrValues(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['traces', 'attr_values', params],
    queryFn: ({ signal }) => apiGet<ValuesResponse>('/traces/attr_values', params, signal),
    staleTime: 60_000,
    enabled,
  })
}

export function useServices(params: Params) {
  return useQuery({
    queryKey: ['services', params],
    queryFn: ({ signal }) => apiGet<OverviewResponse>('/services', params, signal),
    placeholderData: keepPreviousData,
  })
}

export function useOperations(service: string, params: Params) {
  return useQuery({
    queryKey: ['services', service, 'operations', params],
    queryFn: ({ signal }) => apiGet<OperationsResponse>(`/services/${encodeURIComponent(service)}/operations`, params, signal),
    placeholderData: keepPreviousData,
    enabled: !!service,
  })
}

/** 一条查询问几个服务，和后端的 `MAX_SERVICES_PER_QUERY` 对齐 */
const OPS_PER_QUERY = 24

/**
 * 一次问好几个服务的接口表，超过一条查询的上限就切几批并发问，合成一张表返回。
 *
 * 以前是一张卡一个 `useOperations`，十几个服务同时报警就是十几条查询（每条内部还要查当前窗
 * 和对比窗两遍）——而那正是最需要这一页的时候。同一页的「头号报错」「进程重启」早就是一条
 * 查全站再按服务分了。
 *
 * 切批而不是只问前 24 个，是因为总览页现在要拿接口级的变化判服务健不健康（见
 * `latencyHotspot`）：没问到的服务就等于没判。批次按服务名切——问的是全部服务，谁分在哪一批
 * 不影响结果，按名字切 queryKey 才稳定，不会因为流量排名抖动把几条查询全部作废重发。
 */
export function useServiceOperations(services: string[], params: Params, enabled = true) {
  const list = [...services].sort().join(',')
  const chunks = useMemo(() => {
    const names = list ? list.split(',') : []
    const out: string[][] = []
    for (let i = 0; i < names.length; i += OPS_PER_QUERY) out.push(names.slice(i, i + OPS_PER_QUERY))
    return out
  }, [list])
  return useQueries({
    queries: chunks.map((names) => {
      const service = names.join(',')
      return {
        queryKey: ['services', 'operations', service, params],
        queryFn: ({ signal }: { signal: AbortSignal }) =>
          apiGet<OperationsResponse>('/services/operations', { ...params, service }, signal),
        placeholderData: keepPreviousData,
        staleTime: 60_000,
        enabled,
      }
    }),
    combine: combineOperations,
  })
}

/**
 * 几批结果拼成一张表。一批失败不该让整页的「主要是哪个接口」消失——拿到几批算几批。
 *
 * 写在外面是因为 react-query 按 `combine` 的引用判要不要重算：每次 render 新建一个箭头函数，
 * 它就得把上万行接口拼一遍再深比一次，而这一页每敲一个字就 render。
 */
function combineOperations(results: { data?: OperationsResponse; isFetching: boolean }[]) {
  return {
    operations: results.flatMap((r) => r.data?.operations ?? []),
    isFetching: results.some((r) => r.isFetching),
  }
}

export function useTimeseries(service: string, params: Params) {
  return useQuery({
    queryKey: ['services', service, 'timeseries', params],
    queryFn: ({ signal }) => apiGet<TimeseriesResponse>(`/services/${encodeURIComponent(service)}/timeseries`, params, signal),
    placeholderData: keepPreviousData,
    enabled: !!service,
  })
}

export function useMetricCatalog(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['metrics', 'catalog', params],
    queryFn: ({ signal }) => apiGet<MetricCatalogResponse>('/metrics', params, signal),
    placeholderData: keepPreviousData,
    staleTime: 60_000,
    enabled,
  })
}

export function useMetricQuery(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['metrics', 'query', params],
    queryFn: ({ signal }) => apiGet<MetricQueryResponse>('/metrics/query', params, signal),
    placeholderData: keepPreviousData,
    enabled,
  })
}

export function useMetricLabels(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['metrics', 'labels', params],
    queryFn: ({ signal }) => apiGet<MetricNamesResponse>('/metrics/labels', params, signal),
    staleTime: 60_000,
    enabled,
  })
}

export function useMetricLabelValues(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['metrics', 'label_values', params],
    queryFn: ({ signal }) => apiGet<MetricNamesResponse>('/metrics/label_values', params, signal),
    staleTime: 60_000,
    enabled,
  })
}

/** exemplar 是旁路数据，没有也不影响画图，所以单独查、失败不打扰 */
export function useMetricExemplars(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['metrics', 'exemplars', params],
    queryFn: ({ signal }) => apiGet<MetricExemplarsResponse>('/metrics/exemplars', params, signal),
    placeholderData: keepPreviousData,
    staleTime: 60_000,
    retry: false,
    enabled,
  })
}

/** 进程重启 / pod 启动的时刻。旁路数据，失败不打扰 */
export function useMetricEvents(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['metrics', 'events', params],
    queryFn: ({ signal }) => apiGet<MetricEventsResponse>('/metrics/events', params, signal),
    placeholderData: keepPreviousData,
    staleTime: 60_000,
    retry: false,
    enabled,
  })
}

/**
 * 有哪些账期有账单。费用页一进来先问它：账单是「昨天出昨天的、月初出上个月的」，
 * 默认看哪几个月得跟着数据走，而不是跟着今天走（这个月一号打开页面，全是空的）。
 */
export function useBillPeriods(enabled = true) {
  return useQuery({
    queryKey: ['bills', 'periods'],
    queryFn: ({ signal }) => apiGet<BillPeriodsResponse>('/bills/periods', {}, signal),
    staleTime: 10 * 60_000,
    enabled,
  })
}

export function useBillSummary(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['bills', 'summary', params],
    queryFn: ({ signal }) => apiGet<BillSummaryResponse>('/bills/summary', params, signal),
    placeholderData: keepPreviousData,
    staleTime: 5 * 60_000,
    enabled,
  })
}

/** 按天的花费。旁路数据：只有日度表在的时候才查，失败不打扰 */
export function useBillDaily(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['bills', 'daily', params],
    queryFn: ({ signal }) => apiGet<BillDailyResponse>('/bills/daily', params, signal),
    placeholderData: keepPreviousData,
    staleTime: 5 * 60_000,
    retry: false,
    enabled,
  })
}

export function useBillBreakdown(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['bills', 'breakdown', params],
    queryFn: ({ signal }) => apiGet<BillBreakdownResponse>('/bills/breakdown', params, signal),
    placeholderData: keepPreviousData,
    staleTime: 5 * 60_000,
    enabled,
  })
}

/**
 * 成本归属：按规则把账单摊到业务线，并给出日均。
 *
 * 比排行那条重一些（每张表两条查询），所以只在分析视图打开时才发。
 */
export function useBillAllocation(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['bills', 'allocation', params],
    queryFn: ({ signal }) => apiGet<BillAllocationResponse>('/bills/allocation', params, signal),
    placeholderData: keepPreviousData,
    staleTime: 5 * 60_000,
    enabled,
  })
}

export function useBillDetail(params: Params, enabled = true) {
  return useQuery({
    queryKey: ['bills', 'detail', params],
    queryFn: ({ signal }) => apiGet<BillDetailResponse>('/bills/detail', params, signal),
    placeholderData: keepPreviousData,
    enabled,
  })
}

/**
 * 手动拉取的任务状态，跑完之前每 [`SYNC_POLL_MS`] 问一次。
 *
 * 触发那一下只是让 goscan 登记了一个后台任务，账单是之后才进库的——所以这里要轮，
 * 而不是等 POST 的返回。
 */
const SYNC_POLL_MS = 2_000

export function useBillSyncTask(taskId: string | null) {
  return useQuery({
    queryKey: ['bills', 'sync', taskId],
    queryFn: ({ signal }) => apiGet<BillSyncTask>(`/bills/sync/${encodeURIComponent(taskId ?? '')}`, {}, signal),
    enabled: !!taskId,
    // 跑完就停下来，别一直问
    refetchInterval: (query) => (query.state.data?.done ? false : SYNC_POLL_MS),
    retry: false,
  })
}
