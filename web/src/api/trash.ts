import { apiRequest } from './client';

export interface TrashPrompt {
  role: string;
  text: string;
}

export interface TrashItem {
  id: number;
  asset_id: number;
  kind: string;
  rel_path: string;
  title: string | null;
  bytes: number;
  width: number | null;
  height: number | null;
  hash: string | null;
  nsfw: boolean;
  prompts: TrashPrompt[];
  added_at: string;
  deleted_at: string;
  file_url: string;
}

export interface TrashListResponse {
  items: TrashItem[];
  total: number;
  page: number;
  page_size: number;
  trash_dir: string;
}

/** GET /api/trash */
export function listTrash(
  page = 1,
  pageSize = 50,
  signal?: AbortSignal,
): Promise<TrashListResponse> {
  return apiRequest<TrashListResponse>('/trash', {
    query: { page, page_size: pageSize },
    signal,
  });
}

/** POST /api/trash/{id}/restore */
export function restoreTrash(id: number): Promise<{
  restored: boolean;
  asset_id: number;
  note: string;
}> {
  return apiRequest(`/trash/${id}/restore`, { method: 'POST' });
}

/** DELETE /api/trash/{id} */
export function purgeTrash(id: number): Promise<{ deleted: boolean; file_deleted: boolean }> {
  return apiRequest(`/trash/${id}`, { method: 'DELETE' });
}

/** DELETE /api/trash */
export function clearTrash(): Promise<{ cleared: number; files_deleted: number }> {
  return apiRequest('/trash', { method: 'DELETE' });
}
