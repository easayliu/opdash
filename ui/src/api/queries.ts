/** react-query 封装。所有查询都用 placeholderData 保留上一次结果：重新查询时图表按住旧画面变淡，不闪白。 */
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { apiGet, type Params } from './client'
import type {
  ContextResponse,
  FacetsResponse,
  HistogramResponse,
  KeysResponse,
  LogSearchResponse,
  Meta,
  OperationsResponse,
  OverviewResponse,
  TimeseriesResponse,
  TraceDetailResponse,
  TraceSearchResponse,
  ValuesResponse,
} from './types'

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

export function useTraceDetail(traceId: string | undefined) {
  return useQuery({
    queryKey: ['traces', 'detail', traceId],
    queryFn: ({ signal }) => apiGet<TraceDetailResponse>(`/traces/${encodeURIComponent(traceId ?? '')}`, {}, signal),
    enabled: !!traceId,
    staleTime: 60_000,
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
