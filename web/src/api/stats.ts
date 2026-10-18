import { apiRequest } from './client';
import type { FacetKey, FacetsResponse, SizeResponse, StatsResponse } from './types';

/** GET /api/stats —— 总量卡片 + 四组分布 + 编码进度 + 作业状态计数 */
export function fetchStats(signal?: AbortSignal): Promise<StatsResponse> {
  return apiRequest<StatsResponse>('/stats', { signal });
}

/**
 * GET /api/stats/facets?key=tag|kind|nsfw|month|source
 *
 * `limit` 是后端的分桶上限（默认 20，最大 200）。首页 tag 过滤器传 200，
 * 避免新加的标签因为排在 20 名以外而「看不到」。
 */
export function fetchFacets(
  key: FacetKey,
  signal?: AbortSignal,
  limit?: number,
): Promise<FacetsResponse> {
  return apiRequest<FacetsResponse>('/stats/facets', { query: { key, limit }, signal });
}

/** GET /api/stats/size */
export function fetchSize(signal?: AbortSignal): Promise<SizeResponse> {
  return apiRequest<SizeResponse>('/stats/size', { signal });
}
