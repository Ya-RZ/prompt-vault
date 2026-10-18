import { apiRequest } from './client';
import type {
  CleanupTagsResponse,
  DeleteTagResponse,
  TagItem,
  TagListQuery,
  TagListResponse,
} from './types';

/** GET /api/tags —— 标签 + 使用次数（按 asset_count 降序、name 升序） */
export async function listTags(
  query: TagListQuery = {},
  signal?: AbortSignal,
): Promise<TagListResponse> {
  const data = await apiRequest<TagListResponse | TagItem[]>('/tags', {
    query: { page: query.page, page_size: query.page_size, q: query.q },
    signal,
  });
  // 契约保证 items/total；若后端直接返回数组，这里兜底包装成同一形状，避免 UI 崩
  if (Array.isArray(data)) return { items: data, total: data.length };
  return {
    items: data.items ?? [],
    total: data.total ?? data.items?.length ?? 0,
    page: data.page,
    page_size: data.page_size,
  };
}

/** DELETE /api/tags/{id} —— 仍有资产引用时后端返回 409 CONFLICT（detail 里带引用数） */
export function deleteTag(id: number, signal?: AbortSignal): Promise<DeleteTagResponse> {
  return apiRequest<DeleteTagResponse>(`/tags/${id}`, { method: 'DELETE', signal });
}

/** POST /api/tags/cleanup —— 删除所有无引用的标签行，返回删除条数 */
export function cleanupTags(signal?: AbortSignal): Promise<CleanupTagsResponse> {
  return apiRequest<CleanupTagsResponse>('/tags/cleanup', { method: 'POST', signal });
}
