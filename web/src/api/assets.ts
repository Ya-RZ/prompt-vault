import { apiRequest } from './client';
import type {
  Asset,
  AssetListQuery,
  AssetListResponse,
  CreatePromptRequest,
  DeleteAssetResponse,
  Prompt,
  SearchResponse,
  UpdateAssetMetaRequest,
} from './types';

/** POST /api/assets/upload —— multipart 字段固定为 `file`，201 返回新建的 AssetDto */
export function uploadAsset(file: File, signal?: AbortSignal): Promise<Asset> {
  const formData = new FormData();
  formData.append('file', file, file.name);
  return apiRequest<Asset>('/assets/upload', { method: 'POST', formData, signal });
}

/** GET /api/assets —— 分页 + q/tag/kind/nsfw/encoded 筛选 + sort（tag 单选） */
export function listAssets(query: AssetListQuery, signal?: AbortSignal): Promise<AssetListResponse> {
  return apiRequest<AssetListResponse>('/assets', {
    query: {
      page: query.page,
      page_size: query.page_size,
      q: query.q,
      tag: query.tag,
      kind: query.kind,
      nsfw: query.nsfw,
      encoded: query.encoded,
      sort: query.sort,
    },
    signal,
  });
}

/** GET /api/assets/{id} */
export function getAsset(id: number, signal?: AbortSignal): Promise<Asset> {
  return apiRequest<Asset>(`/assets/${id}`, { signal });
}

/** POST /api/assets/{id}/prompts —— 201 返回新建的 prompt */
export function createAssetPrompt(
  id: number,
  payload: CreatePromptRequest,
  signal?: AbortSignal,
): Promise<Prompt> {
  return apiRequest<Prompt>(`/assets/${id}/prompts`, { method: 'POST', body: payload, signal });
}

/** POST /api/assets/{id}/meta —— 字段可选，tags 为全量替换 */
export function updateAssetMeta(
  id: number,
  payload: UpdateAssetMetaRequest,
  signal?: AbortSignal,
): Promise<Asset> {
  return apiRequest<Asset>(`/assets/${id}/meta`, { method: 'POST', body: payload, signal });
}

/** DELETE /api/assets/{id}?keep_file=... */
export function deleteAsset(
  id: number,
  keepFile?: boolean,
  signal?: AbortSignal,
): Promise<DeleteAssetResponse> {
  return apiRequest<DeleteAssetResponse>(`/assets/${id}`, {
    method: 'DELETE',
    query: { keep_file: keepFile === undefined ? undefined : keepFile },
    signal,
  });
}

/** GET /api/assets/{id}/similar?k=&mode= —— 与 /api/search/image 同构 */
export function getSimilarAssets(
  id: number,
  k: number,
  mode?: 'dense' | 'sparse',
  signal?: AbortSignal,
): Promise<SearchResponse> {
  return apiRequest<SearchResponse>(`/assets/${id}/similar`, {
    query: { k, mode },
    signal,
  });
}
