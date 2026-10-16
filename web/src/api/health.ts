import { ApiError, apiRequest } from './client';
import type { HealthResponse } from './types';

/**
 * 契约 §3 要求的关键结构。
 *
 * 后端正在并行开发，可能出现「HTTP 200 但结构不符」（骨架实现、旧版本、
 * 或 SPA fallback 把 index.html 当成响应）。这种情况下必须给出明确错误，
 * 而不是让 UI 在 `health.encoder.mode` 上直接抛错白屏。
 */
function isValidHealth(value: unknown): value is HealthResponse {
  if (typeof value !== 'object' || value === null) return false;
  const record = value as Record<string, unknown>;
  return (
    typeof record.status === 'string' &&
    typeof record.encoder === 'object' &&
    record.encoder !== null &&
    typeof record.index === 'object' &&
    record.index !== null &&
    typeof record.golden === 'object' &&
    record.golden !== null &&
    typeof record.db === 'object' &&
    record.db !== null
  );
}

/** GET /api/health —— 页头状态徽标、编码指纹、mock 标记、golden 状态都来自这里 */
export async function fetchHealth(signal?: AbortSignal): Promise<HealthResponse> {
  const data = await apiRequest<unknown>('/health', { signal });
  if (!isValidHealth(data)) {
    throw new ApiError(
      200,
      'INTERNAL',
      '/api/health 响应结构不符合 INTERFACES.md §3（缺少 status/encoder/golden/index/db 字段）',
      data,
    );
  }
  return data;
}
