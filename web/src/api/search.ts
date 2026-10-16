import { apiRequest } from './client';
import type {
  LexicalResponse,
  SearchImageQuery,
  SearchResponse,
  SearchTextQuery,
  SimilarPromptResponse,
} from './types';

/** GET /api/search/text */
export function searchText(query: SearchTextQuery, signal?: AbortSignal): Promise<SearchResponse> {
  return apiRequest<SearchResponse>('/search/text', {
    query: {
      q: query.q,
      k: query.k,
      mode: query.mode,
      tag: query.tag,
      nsfw: query.nsfw,
      kind: query.kind,
      min_score: query.min_score,
      with_vector: query.with_vector,
    },
    signal,
  });
}

/** POST /api/search/image —— multipart/form-data，字段名固定为 `file` */
export function searchImageByFile(
  file: File,
  query: SearchImageQuery,
  signal?: AbortSignal,
): Promise<SearchResponse> {
  const form = new FormData();
  form.append('file', file, file.name);
  return apiRequest<SearchResponse>('/search/image', {
    method: 'POST',
    formData: form,
    query: {
      k: query.k,
      tag: query.tag,
      nsfw: query.nsfw,
      max_pixels: query.max_pixels,
      with_vector: query.with_vector,
    },
    signal,
  });
}

/** POST /api/search/image —— application/json: { asset_id }，按已有资产找相似 */
export function searchImageByAssetId(
  assetId: number,
  query: SearchImageQuery,
  signal?: AbortSignal,
): Promise<SearchResponse> {
  return apiRequest<SearchResponse>('/search/image', {
    method: 'POST',
    body: { asset_id: assetId },
    query: {
      k: query.k,
      tag: query.tag,
      nsfw: query.nsfw,
      max_pixels: query.max_pixels,
      with_vector: query.with_vector,
    },
    signal,
  });
}

/** POST /api/search/image —— application/json: { image_base64 } */
export function searchImageByBase64(
  imageBase64: string,
  query: SearchImageQuery,
  signal?: AbortSignal,
): Promise<SearchResponse> {
  return apiRequest<SearchResponse>('/search/image', {
    method: 'POST',
    body: { image_base64: imageBase64 },
    query: {
      k: query.k,
      tag: query.tag,
      nsfw: query.nsfw,
      max_pixels: query.max_pixels,
      with_vector: query.with_vector,
    },
    signal,
  });
}

/** GET /api/search/lexical —— 调试用纯词法检索 */
export function searchLexical(q: string, k?: number, signal?: AbortSignal): Promise<LexicalResponse> {
  return apiRequest<LexicalResponse>('/search/lexical', { query: { q, k }, signal });
}

/** GET /api/prompts/{id}/similar */
export function getSimilarPrompts(
  promptId: number,
  k?: number,
  signal?: AbortSignal,
): Promise<SimilarPromptResponse> {
  return apiRequest<SimilarPromptResponse>(`/prompts/${promptId}/similar`, {
    query: { k },
    signal,
  });
}
