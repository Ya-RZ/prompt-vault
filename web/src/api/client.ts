/**
 * 统一的 HTTP 客户端：所有 API 调用都经过这里。
 *
 * 职责：
 * 1. 拼 query（支持 `tag` 这类可重复参数）；
 * 2. 把后端 `{ error: { code, message, detail } }` 解析成 `ApiError`；
 * 3. 把错误码翻译成人话（契约 §0 错误码表 + 用户明确要求的 4 类专门文案）。
 *
 * 注意：后端可能未启动，业务代码必须捕获异常并渲染错误态，不能白屏。
 */

import type { ApiErrorBody } from './types';

/** 生产环境走后端同源；开发期由 vite.config.ts 代理到 127.0.0.1:8918 */
export const API_PREFIX = '/api';

export type QueryValue = string | number | boolean | null | undefined | Array<string | number>;

/** 后端返回的标准化错误 */
export class ApiError extends Error {
  readonly code: string;
  readonly status: number;
  readonly detail: unknown;

  constructor(status: number, code: string, message: string, detail?: unknown) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
    this.detail = detail;
  }

  /** 后端不可达（fetch 直接抛错）时为 true */
  get isNetworkError(): boolean {
    return this.code === 'NETWORK_ERROR';
  }

  /** 后端未启动时返回 true（调用方可据此显示「后端未运行」而不是泛化错误） */
  get isBackendDown(): boolean {
    return this.status === 0;
  }
}

/** 把 query 对象序列化成查询串（数组按同名重复参数输出） */
export function toQueryString(params?: Record<string, QueryValue>): string {
  if (!params) return '';
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === null || value === undefined) continue;
    if (Array.isArray(value)) {
      for (const item of value) {
        if (item === null || item === undefined) continue;
        search.append(key, String(item));
      }
    } else {
      search.append(key, String(value));
    }
  }
  const text = search.toString();
  return text ? `?${text}` : '';
}

export interface RequestOptions {
  method?: 'GET' | 'POST' | 'PUT' | 'DELETE';
  query?: Record<string, QueryValue>;
  /** JSON 请求体；与 formData 互斥 */
  body?: unknown;
  /** multipart 请求体（以图搜图）；设置后不会带 Content-Type，交给浏览器生成 boundary */
  formData?: FormData;
  signal?: AbortSignal;
}

/**
 * 判断是否为「请求被主动中断」。
 *
 * 不依赖 `DOMException` 全局（部分运行环境/无头浏览器没有该构造器，
 * 直接 `instanceof DOMException` 会抛 TypeError 并把真正的错误吞掉）。
 */
export function isAbortError(cause: unknown): boolean {
  return (
    typeof cause === 'object' &&
    cause !== null &&
    'name' in cause &&
    (cause as { name?: unknown }).name === 'AbortError'
  );
}

async function parseErrorResponse(response: Response): Promise<ApiError> {
  const raw = await response.text().catch(() => '');
  let code = 'INTERNAL';
  let message = `请求失败（HTTP ${response.status}）`;
  let detail: unknown;

  if (raw) {
    try {
      const parsed = JSON.parse(raw) as Partial<ApiErrorBody>;
      const err = parsed?.error;
      if (err && typeof err === 'object') {
        if (typeof err.code === 'string' && err.code) code = err.code;
        if (typeof err.message === 'string' && err.message) message = err.message;
        detail = err.detail;
      } else {
        message = raw.slice(0, 500);
      }
    } catch {
      // 非 JSON（例如代理返回的 HTML 错误页），保留原文片段帮助排查
      message = raw.slice(0, 500);
    }
  }

  return new ApiError(response.status, code, message, detail);
}

/** 单次请求超时（毫秒）。用于避免后端半死时 UI 永久停在加载态。 */
const REQUEST_TIMEOUT_MS = 30_000;

/**
 * 发起请求。成功时返回解析后的 JSON；失败一律抛 `ApiError`。
 */
export async function apiRequest<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const { method = 'GET', query, body, formData, signal } = options;
  const url = `${API_PREFIX}${path}${toQueryString(query)}`;

  const headers: Record<string, string> = { Accept: 'application/json' };
  let payload: BodyInit | undefined;

  if (formData) {
    payload = formData;
  } else if (body !== undefined) {
    headers['Content-Type'] = 'application/json; charset=utf-8';
    payload = JSON.stringify(body);
  }

  // 把「调用方取消」和「超时」合并到一个 signal 上，并区分二者
  const controller = new AbortController();
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, REQUEST_TIMEOUT_MS);
  const forwardAbort = () => controller.abort();
  if (signal) {
    if (signal.aborted) controller.abort();
    else signal.addEventListener('abort', forwardAbort, { once: true });
  }

  let response: Response;
  try {
    response = await fetch(url, { method, headers, body: payload, signal: controller.signal });
  } catch (cause) {
    if (timedOut) {
      throw new ApiError(
        0,
        'NETWORK_ERROR',
        `请求超时：${REQUEST_TIMEOUT_MS / 1000} 秒内没有响应`,
        url,
      );
    }
    // 调用方主动取消（组件卸载 / 重新发起请求），原样抛出由上层忽略
    if (isAbortError(cause)) throw cause;
    throw new ApiError(
      0,
      'NETWORK_ERROR',
      '无法连接到主服务',
      cause instanceof Error ? cause.message : String(cause),
    );
  } finally {
    clearTimeout(timer);
    if (signal) signal.removeEventListener('abort', forwardAbort);
  }

  if (!response.ok) throw await parseErrorResponse(response);

  if (response.status === 204) return undefined as T;

  const text = await response.text();
  if (!text) return undefined as T;

  try {
    return JSON.parse(text) as T;
  } catch {
    throw new ApiError(
      response.status,
      'INTERNAL',
      '响应不是合法 JSON',
      text.slice(0, 500),
    );
  }
}

/* ============================================================
 * 错误 → 人话
 * ========================================================== */

export interface ErrorPresentation {
  /** 标题：一句话说清发生了什么 */
  title: string;
  /** 说明：为什么 + 怎么办 */
  description: string;
  /** 后端原始 code，便于对照契约 */
  code: string;
}

interface ErrorCopy {
  title: string;
  hint: string;
}

/**
 * 契约 §0 错误码表 + 用户明确要求专门提示的 4 类：
 * FINGERPRINT_MISMATCH / ENCODER_UNREACHABLE / PAYLOAD_TOO_LARGE / GOLDEN_FAILED。
 */
const ERROR_COPY: Record<string, ErrorCopy> = {
  NETWORK_ERROR: {
    title: '无法连接到主服务',
    hint:
      '请求没有到达后端。请确认主服务已在 127.0.0.1:8918 启动（`cargo run` 或已构建的二进制）；' +
      '开发期也可用 `pnpm dev` 由 Vite 代理转发 /api 与 /media。',
  },
  FINGERPRINT_MISMATCH: {
    title: '向量指纹与索引指纹不一致（409，已拒绝）',
    hint:
      '这不是警告而是拒绝：不同指纹的向量不在同一空间，无法比较。' +
      '请改用与索引相同的编码器（同一模型/精度/设备），或用当前编码器重建索引后重试。',
  },
  ENCODER_UNREACHABLE: {
    title: '编码侧车不可达（503）',
    hint:
      '主服务连不上 Python 编码侧车（默认 127.0.0.1:8917），编码与语义检索暂不可用。' +
      '此时仍可使用纯词法检索（mode=lexical）。请先启动侧车，再在页头确认状态回到 READY。',
  },
  PAYLOAD_TOO_LARGE: {
    title: '图片超出序列预算（413）',
    hint:
      '该图会被侧车静默截断，因此主服务直接拒绝。可参考下方 detail 中的 plan，' +
      '裁剪图片或调低 max_pixels 后重试。',
  },
  GOLDEN_FAILED: {
    title: '金标校验未通过（409）',
    hint:
      '侧车 /v1/verify 未通过，按开发文档 §9.4 不允许开始导入/编码作业。' +
      '请检查编码器版本与设备是否与基准一致后重试。',
  },
  CONFLICT: {
    title: '作业状态冲突（409）',
    hint: '例如对已结束的作业再次 resume/pause。请刷新作业列表后再操作。',
  },
  ENCODER_ERROR: {
    title: '编码侧车返回异常（502）',
    hint: '侧车返回 500 或响应结构不符合契约。请查看侧车日志（embed-server）后重试。',
  },
  NOT_FOUND: {
    title: '资源不存在（404）',
    hint: '该资产/作业/提示词可能已被删除，请刷新列表。',
  },
  BAD_REQUEST: {
    title: '请求参数不合法（400）',
    hint: '请检查必填项与取值范围（例如 page_size 上限 200）。',
  },
  SPARSE_UNAVAILABLE: {
    title: '稀疏向量不可用（501）',
    hint: '侧车未提供 sparse 向量，稀疏检索路预留但不可用；hybrid 会自动退化为 dense。',
  },
  INTERNAL: {
    title: '服务内部错误（500）',
    hint: '请查看主服务日志。若刚重启过，可稍后重试。',
  },
};

function stringifyDetail(detail: unknown): string | null {
  if (detail === null || detail === undefined) return null;
  if (typeof detail === 'string') return detail;
  try {
    return JSON.stringify(detail, null, 2);
  } catch {
    return String(detail);
  }
}

function isApiErrorLike(value: unknown): value is { code: string; message: string; detail?: unknown } {
  return typeof value === 'object' && value !== null && 'code' in value && 'message' in value;
}

/** 把任意异常转成可展示的 { title, description, code } */
export function describeError(error: unknown): ErrorPresentation {
  if (error instanceof ApiError) {
    const copy = ERROR_COPY[error.code];
    const detailText = stringifyDetail(error.detail);
    const parts: string[] = [];
    parts.push(copy ? copy.hint : error.message);
    if (!copy) {
      parts.push('该错误码未在契约 §0 错误码表中，请对照主服务日志排查。');
    } else if (error.message && error.message !== copy.title) {
      parts.push(`后端信息：${error.message}`);
    }
    if (detailText) parts.push(`detail：\n${detailText}`);
    return {
      title: copy ? copy.title : `请求失败（${error.status || '网络错误'}）`,
      description: parts.join('\n\n'),
      code: error.code,
    };
  }

  if (isApiErrorLike(error)) {
    const copy = ERROR_COPY[error.code];
    return {
      title: copy ? copy.title : '请求失败',
      description: copy ? copy.hint : error.message,
      code: error.code,
    };
  }

  if (error instanceof Error) {
    return { title: '发生了未预期的错误', description: error.message, code: 'UNKNOWN' };
  }

  return { title: '发生了未预期的错误', description: String(error), code: 'UNKNOWN' };
}
