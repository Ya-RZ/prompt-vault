//! 共享 DTO：字段与 `INTERFACES.md` 一一对应，前端类型定义照这里抄。
//! 改字段名 = 改契约，两边一起改。

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------- 分页

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paginated<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
}

impl<T> Paginated<T> {
    pub fn new(items: Vec<T>, total: i64, page: i64, page_size: i64) -> Self {
        Self { items, total, page, page_size }
    }
}

// ---------------------------------------------------------------- 资产

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptDto {
    pub id: i64,
    pub asset_id: i64,
    pub role: String,
    pub text: String,
    pub created_at: String,
    /// 该提示词是否已有向量（在 prompt_vector 表里）
    #[serde(default)]
    pub encoded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorStateDto {
    pub encoded: bool,
    pub fingerprint: Option<String>,
    pub dim: Option<i64>,
    pub resized: Option<String>,
    pub vision_tokens: Option<i64>,
    pub encoded_at: Option<String>,
}

impl VectorStateDto {
    pub fn missing() -> Self {
        Self {
            encoded: false,
            fingerprint: None,
            dim: None,
            resized: None,
            vision_tokens: None,
            encoded_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetDto {
    pub id: i64,
    pub kind: String,
    pub rel_path: String,
    pub bytes: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    pub sha256: Option<String>,
    pub nsfw: bool,
    pub added_at: String,
    pub tags: Vec<String>,
    pub prompts: Vec<PromptDto>,
    pub vector: VectorStateDto,
    /// 后端给的媒体地址，前端**不要**自己拼前缀
    pub media_url: String,
    pub thumb_url: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AssetListQuery {
    pub page: Option<i64>,
    pub page_size: Option<i64>,
    pub q: Option<String>,
    #[serde(default)]
    pub tag: Vec<String>,
    pub kind: Option<String>,
    pub nsfw: Option<String>,
    pub encoded: Option<String>,
    pub sort: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreatePromptRequest {
    pub text: String,
    #[serde(default)]
    pub role: Option<String>,
    /// 是否同步编码该提示词文本（默认 true）
    #[serde(default)]
    pub encode: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UpdateMetaRequest {
    pub nsfw: Option<bool>,
    pub tags: Option<Vec<String>>,
    pub rel_path: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
}

// ---------------------------------------------------------------- 健康 / 统计

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncoderInfo {
    pub reachable: bool,
    pub url: String,
    pub model: Option<String>,
    pub device: Option<String>,
    pub dim: Option<i64>,
    pub model_dir: Option<String>,
    pub max_pixels: Option<i64>,
    pub max_length: Option<i64>,
    pub template_overhead: Option<i64>,
    pub fingerprint: Option<String>,
    /// live（真侧车）| mock（开发用假编码器，未运行模型）
    pub mode: String,
    pub last_error: Option<String>,
    pub last_checked_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldenInfo {
    /// passed | failed | unsupported | unchecked
    pub status: String,
    pub passed: Option<bool>,
    pub checked_at: Option<String>,
    pub detail: Option<String>,
    pub cos_text: Option<f64>,
    pub cos_image: Option<f64>,
    pub threshold: Option<f64>,
    pub device_mismatch: bool,
    pub encoded_device: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexInfo {
    pub fingerprint: Option<String>,
    pub points: Option<u64>,
    pub assets: i64,
    pub prompts: i64,
    pub dense_size: i64,
    pub sparse_enabled: bool,
    pub quantization: String,
    pub dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbInfo {
    pub path: String,
    pub assets: i64,
    pub pending_encode: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    /// READY | DEGRADED
    pub status: String,
    pub version: String,
    pub uptime_seconds: u64,
    pub encoder: EncoderInfo,
    pub golden: GoldenInfo,
    pub index: IndexInfo,
    pub db: DbInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CountItem {
    pub key: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetsCounts {
    pub total: i64,
    pub images: i64,
    pub videos: i64,
    pub nsfw: i64,
    pub bytes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptsCounts {
    pub total: i64,
    pub encoded: i64,
    pub pending: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorsCounts {
    pub points: Option<u64>,
    pub dense_size: i64,
    pub sparse_enabled: bool,
    pub disk_bytes: u64,
    pub quantization: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodeProgress {
    pub total: i64,
    pub encoded: i64,
    pub percent: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobsCounts {
    pub running: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub cancelled: i64,
    pub paused: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsResponse {
    pub assets: AssetsCounts,
    pub prompts: PromptsCounts,
    pub vectors: VectorsCounts,
    pub encode_progress: EncodeProgress,
    pub by_tag: Vec<CountItem>,
    pub by_month: Vec<CountItem>,
    pub by_source: Vec<CountItem>,
    pub by_kind: Vec<CountItem>,
    pub jobs: JobsCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FacetItem {
    pub value: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FacetsResponse {
    pub key: String,
    /// qdrant-facet | sql
    pub source: String,
    pub items: Vec<FacetItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizeResponse {
    pub media_bytes: u64,
    pub vector_bytes: u64,
    pub db_bytes: u64,
    pub wal_bytes: u64,
    pub total_bytes: u64,
}

// ---------------------------------------------------------------- 搜索

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RankDto {
    pub dense: Option<usize>,
    pub sparse: Option<usize>,
    pub lexical: Option<usize>,
    pub rrf: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchedPromptDto {
    pub prompt_id: i64,
    pub text: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchAssetDto {
    pub id: i64,
    pub rel_path: String,
    pub media_url: String,
    pub thumb_url: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub nsfw: bool,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchItem {
    pub score: f64,
    pub sources: Vec<String>,
    pub rank: RankDto,
    pub asset: SearchAssetDto,
    pub matched: Option<MatchedPromptDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryImageInfo {
    pub resized: Option<String>,
    pub vision_tokens: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub mode: String,
    pub effective_mode: String,
    pub took_ms: u64,
    pub warnings: Vec<String>,
    pub items: Vec<SearchItem>,
    pub total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_image: Option<QueryImageInfo>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TextSearchQuery {
    pub q: Option<String>,
    pub k: Option<usize>,
    pub mode: Option<String>,
    #[serde(default)]
    pub tag: Vec<String>,
    pub nsfw: Option<String>,
    pub kind: Option<String>,
    pub min_score: Option<f64>,
    pub with_vector: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LexicalQuery {
    pub q: Option<String>,
    pub k: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LexicalItem {
    pub asset_id: i64,
    pub rel_path: String,
    /// path | prompt | tag
    pub hit_field: String,
    pub hit_text: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarPromptItem {
    pub prompt: PromptDto,
    pub asset: SearchAssetDto,
    pub score: f64,
}

// ---------------------------------------------------------------- 作业

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobProgress {
    pub percent: f64,
    pub eta_seconds: Option<f64>,
    pub rate_per_minute: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobErrorDto {
    pub rel_path: Option<String>,
    pub code: String,
    pub message: String,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDto {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub root: Option<String>,
    pub total: i64,
    pub done: i64,
    pub failed: i64,
    pub skipped: i64,
    pub cursor: Option<String>,
    pub fingerprint: Option<String>,
    pub message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    pub params: Option<Value>,
    pub progress: JobProgress,
    pub errors: Vec<JobErrorDto>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImportJobRequest {
    pub root: Option<String>,
    #[serde(default)]
    pub recursive: Option<bool>,
    #[serde(default)]
    pub extensions: Option<Vec<String>>,
    #[serde(default)]
    pub default_prompt_role: Option<String>,
    /// sha256 | rel_path | none
    #[serde(default)]
    pub dedupe_by: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct EncodeJobRequest {
    pub limit: Option<i64>,
    #[serde(default)]
    pub only_missing: Option<bool>,
    pub batch_size: Option<usize>,
    #[serde(default)]
    pub asset_ids: Option<Vec<i64>>,
    #[serde(default)]
    pub prompt_ids: Option<Vec<i64>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct JobListQuery {
    pub page: Option<i64>,
    pub page_size: Option<i64>,
    pub status: Option<String>,
    pub kind: Option<String>,
}

// ---------------------------------------------------------------- 侧车

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarHealth {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub dim: Option<i64>,
    #[serde(default)]
    pub max_pixels: Option<i64>,
    #[serde(default)]
    pub max_length: Option<i64>,
    #[serde(default)]
    pub template_overhead: Option<i64>,
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub model_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedItem {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub kind: Option<String>,
    pub embedding: Vec<f32>,
    #[serde(default)]
    pub resized: Option<String>,
    #[serde(default)]
    pub vision_tokens: Option<i64>,
    /// 开发文档 §5.4 改动 2 的预留字段：侧车给了就用，没给就降级
    #[serde(default)]
    pub sparse: Option<SparseVectorDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SparseVectorDto {
    pub indices: Vec<u32>,
    pub values: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedResponse {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub dim: Option<i64>,
    #[serde(default)]
    pub fingerprint: Option<String>,
    pub data: Vec<EmbedItem>,
    #[serde(default)]
    pub elapsed_seconds: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedInputItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbedRequest {
    pub input: Vec<EmbedInputItem>,
}

// ---------------------------------------------------------------- 其它

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletedResponse {
    pub deleted: bool,
    pub id: i64,
    pub vector_deleted: bool,
    pub file_deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageResponse {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
