import { apiRequest } from './client';
import type {
  EncodeJobRequest,
  ImportJobRequest,
  Job,
  JobErrorListResponse,
  JobErrorItem,
  Paged,
} from './types';

export interface JobListQuery {
  page?: number;
  page_size?: number;
}

/**
 * GET /api/jobs
 *
 * 契约 §6 没有给出列表接口的响应外壳（只给了单个 job 结构）。
 * 按 §0 的通用分页约定按 `{ items, total, page, page_size }` 解析；
 * 若后端改成直接返回数组，这里做兼容包装，避免前端直接崩。
 * —— 该差异已列入交付报告「待确认」。
 */
export async function listJobs(
  query: JobListQuery = {},
  signal?: AbortSignal,
): Promise<Paged<Job>> {
  const raw = await apiRequest<Job[] | Paged<Job>>('/jobs', {
    query: { page: query.page, page_size: query.page_size },
    signal,
  });
  if (Array.isArray(raw)) {
    return { items: raw, total: raw.length, page: 1, page_size: raw.length };
  }
  return {
    items: raw.items ?? [],
    total: raw.total ?? raw.items?.length ?? 0,
    page: raw.page ?? 1,
    page_size: raw.page_size ?? (raw.items?.length ?? 0),
  };
}

/** GET /api/jobs/{id} */
export function getJob(id: string, signal?: AbortSignal): Promise<Job> {
  return apiRequest<Job>(`/jobs/${encodeURIComponent(id)}`, { signal });
}

/** POST /api/jobs/import —— 202 + job；金标不过会返回 409 GOLDEN_FAILED */
export function createImportJob(
  payload: ImportJobRequest,
  signal?: AbortSignal,
): Promise<Job> {
  return apiRequest<Job>('/jobs/import', { method: 'POST', body: payload, signal });
}

/** POST /api/jobs/encode —— 202 + job */
export function createEncodeJob(
  payload: EncodeJobRequest,
  signal?: AbortSignal,
): Promise<Job> {
  return apiRequest<Job>('/jobs/encode', { method: 'POST', body: payload, signal });
}

/** POST /api/jobs/{id}/pause */
export function pauseJob(id: string, signal?: AbortSignal): Promise<Job> {
  return apiRequest<Job>(`/jobs/${encodeURIComponent(id)}/pause`, { method: 'POST', signal });
}

/** POST /api/jobs/{id}/resume —— 从 cursor 继续，不重复编码已完成项 */
export function resumeJob(id: string, signal?: AbortSignal): Promise<Job> {
  return apiRequest<Job>(`/jobs/${encodeURIComponent(id)}/resume`, { method: 'POST', signal });
}

/** POST /api/jobs/{id}/cancel */
export function cancelJob(id: string, signal?: AbortSignal): Promise<Job> {
  return apiRequest<Job>(`/jobs/${encodeURIComponent(id)}/cancel`, { method: 'POST', signal });
}

/** GET /api/jobs/{id}/errors —— 分页错误明细 */
export async function listJobErrors(
  id: string,
  query: JobListQuery = {},
  signal?: AbortSignal,
): Promise<JobErrorListResponse> {
  const raw = await apiRequest<JobErrorItem[] | Paged<JobErrorItem>>(
    `/jobs/${encodeURIComponent(id)}/errors`,
    { query: { page: query.page, page_size: query.page_size }, signal },
  );
  if (Array.isArray(raw)) {
    return { items: raw, total: raw.length, page: 1, page_size: raw.length };
  }
  return {
    items: raw.items ?? [],
    total: raw.total ?? raw.items?.length ?? 0,
    page: raw.page ?? 1,
    page_size: raw.page_size ?? (raw.items?.length ?? 0),
  };
}
