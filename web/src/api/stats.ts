import { apiRequest } from './client';
import type { FacetKey, FacetsResponse, SizeResponse, StatsResponse } from './types';

/** GET /api/stats —— 总量卡片 + 四组分布 + 编码进度 + 作业状态计数 */
export function fetchStats(signal?: AbortSignal): Promise<StatsResponse> {
  return apiRequest<StatsResponse>('/stats', { signal });
}

/** GET /api/stats/facets?key=tag|kind|nsfw|month|source */
export function fetchFacets(key: FacetKey, signal?: AbortSignal): Promise<FacetsResponse> {
  return apiRequest<FacetsResponse>('/stats/facets', { query: { key }, signal });
}

/** GET /api/stats/size */
export function fetchSize(signal?: AbortSignal): Promise<SizeResponse> {
  return apiRequest<SizeResponse>('/stats/size', { signal });
}
