/**
 * 与 INTERFACES.md（v1，2026-10-16）逐字段对齐的类型定义。
 *
 * 约定：这里**只**放契约里出现过的字段，不额外发明字段。
 * 契约未明确的取值一律按「最小、最保守」处理（可空 / 可选），并在 UI 侧做兜底。
 */

/* ============================================================
 * 0. 通用约定
 * ========================================================== */

/** 统一错误体：{ "error": { "code", "message", "detail" } } */
export interface ApiErrorBody {
  error: {
    code: string;
    message: string;
    detail?: unknown;
  };
}

/** 统一分页响应外壳（page_size 上限 200） */
export interface Paged<T> {
  items: T[];
  total: number;
  page: number;
  page_size: number;
}

/** 契约 §0 错误码表 */
export type ApiErrorCode =
  | 'BAD_REQUEST'
  | 'NOT_FOUND'
  | 'ENCODER_UNREACHABLE'
  | 'FINGERPRINT_MISMATCH'
  | 'PAYLOAD_TOO_LARGE'
  | 'ENCODER_ERROR'
  | 'GOLDEN_FAILED'
  | 'CONFLICT'
  | 'INTERNAL'
  // 契约 §5 预留接口的 501
  | 'SPARSE_UNAVAILABLE'
  // 前端本地构造（后端不可达），非契约码
  | 'NETWORK_ERROR';

/* ============================================================
 * 3. 健康与统计
 * ========================================================== */

export type HealthStatus = 'READY' | 'DEGRADED';
export type GoldenStatus = 'passed' | 'failed' | 'unsupported' | 'unchecked';

export interface EncoderInfo {
  /** 编码能力是否可用（有模型，或处于 --mock 模式） */
  reachable: boolean;
  /** 是否 --mock 模式（未加载模型，向量由 sha256 派生） */
  mock: boolean;
  model: string;
  device: string;
  dim: number;
  model_dir: string | null;
  max_pixels: number | null;
  max_length: number | null;
  /** 权重是否已真正加载（懒加载：首次编码后才为 true） */
  loaded: boolean;
  fingerprint: string;
  last_error?: string | null;
  last_checked_at?: string | null;
}

export interface GoldenInfo {
  status: GoldenStatus;
  passed: boolean | null;
  checked_at: string | null;
  detail: string | null;
  cos_text: number | null;
  cos_image: number | null;
  threshold: number | null;
  device_mismatch: boolean;
  /** 后端实际返回、契约 §3 未列出的附加字段 */
  encoded_device?: string | null;
}

export interface IndexInfo {
  fingerprint: string;
  points: number;
  assets: number;
  prompts: number;
  dense_size: number;
  sparse_enabled: boolean;
  quantization: string;
  dir: string;
}

export interface DbInfo {
  path: string;
  assets: number;
  pending_encode: number;
}

/** GET /api/health */
export interface HealthResponse {
  status: HealthStatus;
  version: string;
  uptime_seconds: number;
  encoder: EncoderInfo;
  golden: GoldenInfo;
  index: IndexInfo;
  db: DbInfo;
}

/** 分布统计的通用条目（/api/stats 的 by_* 用 `key`） */
export interface FacetBucket {
  key: string;
  count: number;
}

export interface StatsResponse {
  assets: {
    total: number;
    images: number;
    videos: number;
    nsfw: number;
    bytes: number;
  };
  prompts: {
    total: number;
    encoded: number;
    pending: number;
  };
  vectors: {
    points: number;
    dense_size: number;
    sparse_enabled: boolean;
    disk_bytes: number;
    quantization: string;
  };
  encode_progress: {
    total: number;
    encoded: number;
    percent: number;
  };
  by_tag: FacetBucket[];
  by_month: FacetBucket[];
  by_source: FacetBucket[];
  by_kind: FacetBucket[];
  jobs: {
    running: number;
    succeeded: number;
    failed: number;
    cancelled: number;
    /** 后端实际返回、契约 §3 未列出的附加字段 */
    paused?: number;
  };
}

/** /api/stats/facets 的条目用 `value`（与 /api/stats 的 `key` 不同，契约原样保留） */
export interface FacetValueItem {
  value: string;
  count: number;
}

export type FacetKey = 'tag' | 'kind' | 'nsfw' | 'month' | 'source';

/** GET /api/stats/facets?key=... */
export interface FacetsResponse {
  key: FacetKey;
  source: string;
  items: FacetValueItem[];
}

/** GET /api/stats/size */
export interface SizeResponse {
  media_bytes: number;
  vector_bytes: number;
  db_bytes: number;
  wal_bytes: number;
  total_bytes: number;
}

/* ============================================================
 * 4. 资产
 * ========================================================== */

/** kind 的 CHECK 允许 image/video/other（other 用于既非图片也非视频的上传文件） */
export type AssetKind = 'image' | 'video' | 'other';
export type TriState = 'true' | 'false' | 'all';
export type AssetSort = 'added_at' | '-added_at' | 'id' | '-id' | 'bytes' | '-bytes';

/** 内容哈希算法：2026-10 起由 sha256 换成 blake3（AssetDto.hash_algo） */
export type HashAlgo = 'blake3';

/** 资产内嵌的提示词（无 asset_id）；/api/prompts/{id}/similar 返回的带 asset_id */
export interface Prompt {
  id: number;
  role: string;
  text: string;
  created_at: string;
  asset_id?: number;
}

export interface AssetVectorInfo {
  encoded: boolean;
  fingerprint: string | null;
  dim: number | null;
  resized: string | null;
  vision_tokens: number | null;
  encoded_at: string | null;
}

export interface Asset {
  id: number;
  kind: AssetKind;
  rel_path: string;
  /** 人可读标题（上传后填写；为空时前端回退显示文件名） */
  title: string | null;
  bytes: number;
  width: number | null;
  height: number | null;
  duration_ms: number | null;
  /** 内容哈希（原 sha256 字段已改名；算法见 hash_algo） */
  hash: string | null;
  hash_algo: HashAlgo;
  nsfw: boolean;
  added_at: string;
  tags: string[];
  prompts: Prompt[];
  vector: AssetVectorInfo;
  /** 契约 §7.3：前缀由后端给，前端不拼接 */
  media_url: string;
  thumb_url: string;
}

/** GET /api/assets 查询参数（tag 为单选：首页过滤器只允许一个标签） */
export interface AssetListQuery {
  page?: number;
  page_size?: number;
  q?: string;
  tag?: string;
  kind?: AssetKind;
  nsfw?: TriState;
  encoded?: TriState;
  sort?: AssetSort;
}

/** POST /api/assets/upload（multipart 字段 file）→ 201 + AssetDto */
export type UploadAssetResponse = Asset;

/** GET /api/assets → 分页资产 */
export type AssetListResponse = Paged<Asset>;

/** POST /api/assets/{id}/prompts 请求体 */
export interface CreatePromptRequest {
  text: string;
  role: string;
  encode: boolean;
}

/** POST /api/assets/{id}/meta 请求体（字段全部可选，tags 为全量替换语义） */
export interface UpdateAssetMetaRequest {
  nsfw?: boolean;
  tags?: string[];
  rel_path?: string;
  /** 人可读标题；空串表示清空。与 rel_path 无关，不会移动文件 */
  title?: string;
  width?: number | null;
  height?: number | null;
  duration_ms?: number | null;
}

/** DELETE /api/assets/{id} 响应 */
export interface DeleteAssetResponse {
  deleted: boolean;
  id: number;
  vector_deleted: boolean;
  file_deleted: boolean;
}

/* ============================================================
 * 4.1 标签（GET /api/tags 等）
 * ========================================================== */

/** 标签 + 引用它的资产数 */
export interface TagItem {
  id: number;
  name: string;
  asset_count: number;
}

/** GET /api/tags?page=&page_size=&q= —— 契约只保证 items/total，分页字段按需可选 */
export interface TagListResponse {
  items: TagItem[];
  total: number;
  page?: number;
  page_size?: number;
}

/** GET /api/tags 查询参数 */
export interface TagListQuery {
  page?: number;
  page_size?: number;
  q?: string;
}

/** DELETE /api/tags/{id} → 200 {deleted:true}；仍有引用时后端返回 409 CONFLICT */
export interface DeleteTagResponse {
  deleted: boolean;
}

/** POST /api/tags/cleanup → 删掉的无引用标签行数 */
export interface CleanupTagsResponse {
  deleted: number;
}

/* ============================================================
 * 5. 搜索
 * ========================================================== */

export type SearchMode = 'hybrid' | 'dense' | 'lexical';

export interface SearchRank {
  dense: number | null;
  sparse: number | null;
  lexical: number | null;
  rrf: number | null;
}

/** 搜索结果里精简的资产对象（字段少于 /api/assets） */
export interface SearchAssetRef {
  id: number;
  rel_path: string;
  media_url: string;
  thumb_url: string;
  width: number | null;
  height: number | null;
  nsfw: boolean;
  tags: string[];
}

export interface SearchMatched {
  prompt_id: number;
  text: string;
  role: string;
}

export interface SearchHit {
  score: number;
  sources: string[];
  rank: SearchRank;
  asset: SearchAssetRef;
  /** 词法路命中 rel_path/tag 时可能没有命中提示词 —— 契约未明确，按可空处理 */
  matched: SearchMatched | null;
}

export interface QueryImageInfo {
  resized: string | null;
  vision_tokens: number | null;
}

/** GET /api/search/text 与 POST /api/search/image 同构 */
export interface SearchResponse {
  query: string;
  mode: SearchMode;
  effective_mode: SearchMode;
  took_ms: number;
  warnings: string[];
  items: SearchHit[];
  total: number;
  /** 仅 /api/search/image 带 */
  query_image?: QueryImageInfo;
}

/** GET /api/search/text 查询参数 */
export interface SearchTextQuery {
  q: string;
  k?: number;
  mode?: SearchMode;
  tag?: string[];
  nsfw?: boolean;
  kind?: AssetKind;
  min_score?: number;
  with_vector?: boolean;
}

/** POST /api/search/image 查询参数 */
export interface SearchImageQuery {
  k?: number;
  tag?: string[];
  nsfw?: boolean;
  max_pixels?: number;
  with_vector?: boolean;
}

/** GET /api/search/lexical（调试，纯词法） */
export interface LexicalHit {
  asset_id: number;
  rel_path: string;
  hit_field: 'path' | 'prompt' | 'tag';
  hit_text: string;
  score: number;
}

export interface LexicalResponse {
  items: LexicalHit[];
}

/** GET /api/prompts/{id}/similar */
export interface SimilarPromptItem {
  prompt: Prompt;
  asset: {
    id: number;
    rel_path: string;
    media_url: string;
  };
  score: number;
}

export interface SimilarPromptResponse {
  items: SimilarPromptItem[];
  total: number;
}

/* ============================================================
 * 6. 作业
 * ========================================================== */

export type JobKind = 'import' | 'encode';
export type JobStatus = 'running' | 'paused' | 'succeeded' | 'failed' | 'cancelled';

export interface JobProgress {
  percent: number;
  eta_seconds: number | null;
  rate_per_minute: number | null;
}

export interface JobErrorItem {
  rel_path: string;
  code: string;
  message: string;
  at: string;
}

export interface Job {
  id: string;
  kind: JobKind;
  status: JobStatus;
  root: string | null;
  total: number;
  done: number;
  failed: number;
  skipped: number;
  cursor: string | null;
  fingerprint: string | null;
  message: string | null;
  created_at: string;
  updated_at: string;
  finished_at: string | null;
  progress: JobProgress;
  /** 列表接口可能不带，详情接口带 */
  errors?: JobErrorItem[];
  /** 后端实际返回、契约 §6 未列出的附加字段（作业创建参数回显） */
  params?: unknown;
}

/** GET /api/jobs/{id}/errors → 分页返回错误明细 */
export type JobErrorListResponse = Paged<JobErrorItem>;

/** POST /api/jobs/import 请求体 */
export interface ImportJobRequest {
  root: string;
  recursive: boolean;
  extensions: string[];
  default_prompt_role?: string;
  dedupe_by?: string;
}

/** POST /api/jobs/encode 请求体 */
export interface EncodeJobRequest {
  limit: number | null;
  only_missing: boolean;
  batch_size: number;
  asset_ids: number[] | null;
  prompt_ids: number[] | null;
}
