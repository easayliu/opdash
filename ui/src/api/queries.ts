/** react-query 封装。所有查询都用 placeholderData 保留上一次结果：重新查询时图表按住旧画面变淡，不闪白。 */
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { apiGet, type Params } from './client'
import type {
  AuthMe,
  ContextResponse,
  FacetsResponse,
  HeatmapResponse,
  HistogramResponse,
  KeysResponse,
  LogRow,
  LogSearchResponse,
  Meta,
  MetricCatalogResponse,
  MetricExemplarsResponse,
  MetricNamesResponse,
  MetricQueryResponse,
  SpanAttrsResponse,
  OperationsResponse,
  OverviewResponse,
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
  /** 库里带这个 trace id 的总条数（count 查询超时时没有） */
  total?: number
  /** 翻到服务端允许的最深一页仍没拉完 */
  truncated: boolean
  stats: Stats
}

/**
 * 一条 trace 的全部日志：按服务端每页上限一页页拉到没有为止，翻页深度到 max_offset 就停。
 * 一条 trace 的日志通常几十到几百条，一页就完；异常多的也能拉到上万条。
 */
export function useTraceLogs(
  filter: { trace_id: string; span_id?: string; from?: number; to?: number },
  limits: { max_rows: number; max_offset: number } | undefined,
  enabled = true,
) {
  return useQuery({
    queryKey: ['traces', 'logs', filter, limits],
    queryFn: async ({ signal }): Promise<TraceLogs> => {
      const limit = limits?.max_rows ?? 1000
      const maxOffset = limits?.max_offset ?? 0
      const rows: LogRow[] = []
      const stats: Stats = { read_rows: 0, read_bytes: 0, result_rows: 0, elapsed_ms: 0 }
      let total: number | undefined
      let offset = 0
      for (;;) {
        const page = await apiGet<LogSearchResponse>('/logs/search', { ...filter, order: 'asc', limit, offset }, signal)
        rows.push(...page.rows)
        stats.read_rows += page.stats.read_rows
        stats.read_bytes += page.stats.read_bytes
        stats.result_rows += page.stats.result_rows
        stats.elapsed_ms += page.stats.elapsed_ms
        if (offset === 0) total = page.total
        if (page.rows.length < limit) return { rows, total, truncated: false, stats }
        offset += limit
        if (offset > maxOffset) return { rows, total, truncated: true, stats }
      }
    },
    placeholderData: keepPreviousData,
    staleTime: 60_000,
    enabled: enabled && !!limits,
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

export function useLogFacets(field: string, params: Params, enabled = true) {
  return useQuery({
    queryKey: ['logs', 'facets', field, params],
    queryFn: ({ signal }) => apiGet<FacetsResponse>('/logs/facets', { ...params, field }, signal),
    placeholderData: keepPreviousData,
    staleTime: 60_000,
    enabled,
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

/** `at`：trace 开始时间（unix 毫秒），给了服务端只查前后一两天的分区，快很多 */
export function useTraceDetail(traceId: string | undefined, at?: string | null) {
  return useQuery({
    queryKey: ['traces', 'detail', traceId, at ?? null],
    queryFn: ({ signal }) => apiGet<TraceDetailResponse>(`/traces/${encodeURIComponent(traceId ?? '')}`, { at: at ?? undefined }, signal),
    enabled: !!traceId,
    staleTime: 60_000,
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
