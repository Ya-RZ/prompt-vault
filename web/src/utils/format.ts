/** 展示层的纯函数格式化工具（无副作用、可单独推理）。 */

/** 字节 → 人类可读（1024 进制） */
export function formatBytes(bytes: number | null | undefined, fractionDigits = 2): string {
  if (bytes === null || bytes === undefined || !Number.isFinite(bytes)) return '—';
  if (bytes < 0) return '—';
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB', 'TB', 'PB'];
  let value = bytes / 1024;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  return `${value.toFixed(fractionDigits)} ${units[unitIndex]}`;
}

/** 整数千分位 */
export function formatNumber(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return '—';
  return value.toLocaleString('zh-CN');
}

/**
 * 计算百分比并夹在 [0, 100]。
 * 关键：total 为 0 / 缺失时返回 0，绝不产出 NaN（作业页 total=0 场景）。
 */
export function safePercent(part: number, total: number, fallback = 0): number {
  if (!Number.isFinite(part) || !Number.isFinite(total) || total <= 0) return fallback;
  const percent = (part / total) * 100;
  if (!Number.isFinite(percent)) return fallback;
  return Math.min(100, Math.max(0, percent));
}

/** 比例条宽度（0–1）→ 百分比，非有限值归零 */
export function safeRatioRatio(value: number, max: number): number {
  return safePercent(value, max, 0);
}

/** 秒 → 中文时长（用于 ETA） */
export function formatDuration(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined || !Number.isFinite(seconds) || seconds < 0) {
    return '—';
  }
  if (seconds < 60) return `${Math.round(seconds)} 秒`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} 分 ${Math.round(seconds % 60)} 秒`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时 ${minutes % 60} 分`;
  const days = Math.floor(hours / 24);
  return `${days} 天 ${hours % 24} 小时`;
}

/** RFC3339 → 本地时间字符串；无法解析时原样返回 */
export function formatDateTime(value: string | null | undefined): string {
  if (!value) return '—';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString('zh-CN', {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  });
}

/** 速率展示 */
export function formatRate(perMinute: number | null | undefined): string {
  if (perMinute === null || perMinute === undefined || !Number.isFinite(perMinute)) return '—';
  return `${perMinute.toFixed(1)} 项/分`;
}

/**
 * 编码指纹短码。
 *
 * 侧车 fingerprint 形如
 * `Qwen3-VL-Embedding-2B|bfloat16|dim=2048|…|script=8ffa74a1a6bb|weights=…|device=cpu`，
 * 其中 `script=` 就是脚本哈希，天然适合做短码；取不到时回退成整串的稳定哈希。
 */
export function shortFingerprint(fingerprint: string | null | undefined): string {
  if (!fingerprint) return '—';
  const matched = /script=([0-9a-fA-F]+)/.exec(fingerprint);
  if (matched?.[1]) return matched[1].slice(0, 12);
  let hash = 5381;
  for (let index = 0; index < fingerprint.length; index += 1) {
    hash = ((hash * 33) ^ fingerprint.charCodeAt(index)) >>> 0;
  }
  return hash.toString(16).padStart(8, '0');
}

/** 把秒数转成「x 天 x 小时前」这类相对时间，用于作业列表 */
export function formatRelative(fromIso: string | null | undefined, now = Date.now()): string {
  if (!fromIso) return '—';
  const time = new Date(fromIso).getTime();
  if (Number.isNaN(time)) return fromIso;
  const diffSeconds = Math.max(0, Math.round((now - time) / 1000));
  if (diffSeconds < 60) return '刚刚';
  return `${formatDuration(diffSeconds)}前`;
}
