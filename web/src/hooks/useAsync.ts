import type { Dispatch, SetStateAction } from 'react';
import { useCallback, useEffect, useRef, useState } from 'react';

import { isAbortError } from '../api/client';

export interface AsyncState<T> {
  data: T | null;
  error: unknown;
  loading: boolean;
  /** 重新拉取（用于「重试」按钮） */
  reload: () => void;
  setData: Dispatch<SetStateAction<T | null>>;
}

/**
 * 极简的异步数据 hook：加载态 / 错误态 / 重放。
 *
 * 设计取舍：不引入 react-query 等额外依赖（任务要求最小实现）。
 * 后端可能整个不可用，所以 error 必须是「可渲染的一等公民」而不是例外。
 */
export function useAsync<T>(
  loader: (signal: AbortSignal) => Promise<T>,
  deps: readonly unknown[],
  options: { enabled?: boolean } = {},
): AsyncState<T> {
  const enabled = options.enabled ?? true;
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [loading, setLoading] = useState<boolean>(enabled);
  const [nonce, setNonce] = useState(0);

  const loaderRef = useRef(loader);
  loaderRef.current = loader;

  useEffect(() => {
    if (!enabled) {
      setLoading(false);
      return;
    }
    const controller = new AbortController();
    let active = true;

    setLoading(true);
    setError(null);

    loaderRef
      .current(controller.signal)
      .then((result) => {
        if (!active) return;
        setData(result);
        setError(null);
      })
      .catch((cause: unknown) => {
        if (!active) return;
        if (isAbortError(cause)) return;
        setError(cause);
      })
      .finally(() => {
        if (active) setLoading(false);
      });

    return () => {
      active = false;
      controller.abort();
    };
    // deps 由调用方显式给出（与 useEffect 语义一致）
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, enabled, nonce]);

  const reload = useCallback(() => setNonce((value) => value + 1), []);

  return { data, error, loading, reload, setData };
}
