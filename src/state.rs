//! 运行时状态与配置。

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::db::{Db, META_INDEX_FINGERPRINT};
use crate::encode::{EncoderClient, HttpEncoder, HttpEncoderConfig};
use crate::error::{ApiError, ApiResult};
use crate::jobs::JobManager;
use crate::types::GoldenInfo;
use crate::vector::{QuantMode, VectorStore};

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub media_dir: PathBuf,
    pub db_path: PathBuf,
    /// Qdrant 服务地址（`--qdrant-url`）
    pub qdrant_url: String,
    /// Qdrant 集合名（`--qdrant-collection`）
    pub qdrant_collection: String,
    pub thumb_dir: PathBuf,
    pub export_dir: PathBuf,
    pub dim: usize,
    /// 是否在索引里启用稀疏（BM25）向量 —— 只在建 shard 时生效
    pub sparse_enabled: bool,
    /// 编码作业是否编码图片（资产）；`false` = 只编码提示词
    pub encode_images: bool,
    pub quant: QuantMode,
    /// `--mock`：不调用第三方服务，用确定性伪向量跑通链路（仅开发/冒烟）
    pub mock: bool,
    /// 第三方 embedding 服务地址（OpenAI 兼容，如 `https://api.siliconflow.cn/v1`）
    pub embedding_base_url: String,
    /// embedding 模型名（如 `Qwen/Qwen3-VL-Embedding-8B`）
    pub embedding_model: String,
    /// embedding API Key（`config.toml` 的 `[embedding] api_key`）
    pub embedding_api_key: Option<String>,
    /// 单次 HTTP 请求超时（秒）
    pub embedding_timeout_secs: u64,
    /// web/dist 的路径（开发期直接读盘；release 走 rust-embed）
    pub web_dir: PathBuf,
    /// 媒体文件大小上限（字节），超了直接拒（开发文档 §12 第 8 条）
    pub max_media_bytes: u64,
    /// ComfyUI 的 output 目录（`config.toml` 的 `[comfyui] output_dir`）：
    /// 导入作业不填 root 时默认扫这里
    pub comfyui_output_dir: Option<PathBuf>,
    /// 导入时是否复制源文件（`true`，默认）而不是移动；复制后源目录保持完整
    pub import_copy: bool,
}

impl Config {
    pub fn media_path(&self, rel_path: &str) -> PathBuf {
        let cleaned = rel_path.trim_start_matches('/').replace('\\', "/");
        self.media_dir.join(cleaned)
    }

    /// 校验相对路径不越界（防目录穿越）。
    pub fn safe_rel_path(&self, rel_path: &str) -> ApiResult<String> {
        let cleaned = rel_path.trim_start_matches('/').replace('\\', "/");
        if cleaned.split('/').any(|seg| seg == ".." || seg.is_empty() && cleaned.len() > 1) {
            return Err(ApiError::bad_request("路径非法"));
        }
        if cleaned.contains("..") {
            return Err(ApiError::bad_request("路径非法"));
        }
        Ok(cleaned)
    }
}

pub struct AppState {
    pub db: Db,
    pub vectors: Arc<VectorStore>,
    pub encoder: Arc<EncoderClient>,
    pub cfg: Arc<Config>,
    pub jobs: Arc<JobManager>,
    pub started_at: Instant,
    pub golden: RwLock<GoldenInfo>,
}

impl AppState {
    pub async fn new(cfg: Config) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        std::fs::create_dir_all(&cfg.media_dir)?;
        std::fs::create_dir_all(&cfg.thumb_dir)?;
        std::fs::create_dir_all(&cfg.export_dir)?;

        let db = Db::open(&cfg.db_path)?;
        // 存储层不保留绝对路径：把历史作业的绝对 root 规范成相对形式（幂等）
        if let Ok(n) = db.normalize_job_roots() {
            if n > 0 {
                tracing::info!(
                    count = n,
                    "已把历史作业里的绝对路径规范为相对路径（存储层不保留绝对路径）"
                );
            }
        }
        // 进程重启后把残留的 running 作业标成 paused（断点续传，不假装在跑）
        if let Ok(n) = db.reconcile_jobs_on_start() {
            if n > 0 {
                tracing::warn!(count = n, "发现上次未完成作业，已标记为 paused（可 resume）");
            }
        }
        // 向量配置（维度 / 稀疏 / 量化）在服务端集合创建时固定，之后改不了；
        // 这里把本次使用的开关记进 meta，方便排查"这个库是什么配置建的"。
        let _ = db.set_meta(
            crate::db::META_SPARSE_ENABLED,
            if cfg.sparse_enabled { "true" } else { "false" },
        );
        let vectors = VectorStore::connect(
            &cfg.qdrant_url,
            &cfg.qdrant_collection,
            cfg.dim,
            cfg.sparse_enabled,
            cfg.quant,
        )
        .await?;
        // 编码走第三方 embedding API（OpenAI 兼容）。
        //
        // 两条不致命的退化路径（服务照常启动，health 报 DEGRADED）：
        // - `--mock`：不联网，向量由 sha256 派生，只用于跑通链路；
        // - 没配 API Key / 客户端初始化失败：编码不可用，检索自动退化为纯词法。
        let encoder = if cfg.mock {
            tracing::warn!("--mock 模式：不调用第三方服务，向量由 sha256 派生（仅用于链路验证）");
            EncoderClient::mock(cfg.dim)
        } else if cfg.embedding_api_key.is_some() && !cfg.embedding_base_url.trim().is_empty() {
            let http_cfg = HttpEncoderConfig {
                base_url: cfg.embedding_base_url.clone(),
                model: cfg.embedding_model.clone(),
                api_key: cfg.embedding_api_key.clone().unwrap_or_default(),
                dim: cfg.dim,
                timeout_secs: cfg.embedding_timeout_secs,
            };
            let fingerprint = http_cfg.fingerprint();
            match HttpEncoder::new(http_cfg) {
                Ok(encoder) => {
                    tracing::info!(
                        base_url = %cfg.embedding_base_url,
                        model = %cfg.embedding_model,
                        dim = cfg.dim,
                        fingerprint = %fingerprint,
                        "第三方 embedding 已配置"
                    );
                    EncoderClient::new(cfg.dim, Some(std::sync::Arc::new(encoder)), false)
                }
                Err(e) => {
                    tracing::error!(error = %e.message, "embedding 客户端初始化失败：编码不可用");
                    EncoderClient::unconfigured(cfg.dim)
                }
            }
        } else {
            tracing::warn!(
                "未配置 embedding api_key：编码不可用，检索将退化为纯词法（在 config.toml 的 [embedding] 里填写）"
            );
            EncoderClient::unconfigured(cfg.dim)
        };

        Ok(Arc::new(Self {
            db,
            vectors: Arc::new(vectors),
            encoder: Arc::new(encoder),
            cfg: Arc::new(cfg),
            jobs: Arc::new(JobManager::new()),
            started_at: Instant::now(),
            golden: RwLock::new(GoldenInfo {
                status: "unchecked".into(),
                passed: None,
                checked_at: None,
                detail: Some("尚未校验".into()),
                cos_text: None,
                cos_image: None,
                threshold: None,
                device_mismatch: false,
                encoded_device: None,
            }),
        }))
    }

    /// 索引指纹（第一篇向量写入时落库，之后只读）。
    pub fn index_fingerprint(&self) -> ApiResult<Option<String>> {
        self.db.get_meta(META_INDEX_FINGERPRINT)
    }

    /// 写入前调用：第一次写入把指纹钉住；之后不一致就**拒绝**（开发文档 §13 第 4 条）。
    pub fn ensure_index_fingerprint(&self, fingerprint: &str) -> ApiResult<()> {
        match self.db.get_meta(META_INDEX_FINGERPRINT)? {
            None => self.db.set_meta(META_INDEX_FINGERPRINT, fingerprint),
            Some(existing) if existing == fingerprint => Ok(()),
            Some(existing) => Err(ApiError::fingerprint_mismatch(
                "写入被拒：向量指纹与索引指纹不一致（开发文档 §10 铁律 1）",
            )
            .with_detail(serde_json::json!({
                "index_fingerprint": existing,
                "incoming_fingerprint": fingerprint,
                "hint": "不同空间的向量不能混进同一个索引：确认模型/精度/设备后全量重编码"
            }))),
        }
    }

    /// 检索前调用：索引与当前编码空间必须一致。
    pub fn spaces_are_consistent(&self, warnings: &mut Vec<String>) -> ApiResult<bool> {
        let current = self.encoder.fingerprint_hint();
        match self.db.get_meta(META_INDEX_FINGERPRINT)? {
            None => Ok(true),
            Some(indexed) if indexed == current => Ok(true),
            Some(indexed) => {
                warnings.push(format!("指纹不一致：索引 = {indexed}，当前编码 = {current}"));
                Ok(false)
            }
        }
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    pub fn golden_snapshot(&self) -> GoldenInfo {
        // 模型加载后会自动跑三段文本自检，那份结果是实时的、比启动时缓存的可信：
        // 启动时模型还没加载，缓存里只能是 unchecked。
        let live = self.encoder.verify_golden();
        if live.status != "unchecked" && live.status != "unsupported" {
            return live;
        }
        self.golden
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| GoldenInfo {
                status: "unchecked".into(),
                passed: None,
                checked_at: None,
                detail: Some("状态锁被污染".into()),
                cos_text: None,
                cos_image: None,
                threshold: None,
                device_mismatch: false,
                encoded_device: None,
            })
    }

    pub fn set_golden(&self, info: GoldenInfo) {
        if let Ok(mut slot) = self.golden.write() {
            *slot = info.clone();
        }
        if let Ok(serialized) = serde_json::to_string(&info) {
            let _ = self.db.set_meta(crate::db::META_LAST_GOLDEN, &serialized);
        }
    }

    /// 作业开始前的金标闸门（开发文档 §9.4）：failed 才拦，unsupported/unchecked 放行但记录。
    pub fn check_golden_gate(&self) -> ApiResult<GoldenInfo> {
        let info = self.golden_snapshot();
        if info.status == "failed" {
            return Err(ApiError::golden_failed(
                "金标校验未通过，禁止开始导入/编码作业（开发文档 §9.4）",
            )
            .with_detail(serde_json::to_value(&info).unwrap_or(serde_json::Value::Null)));
        }
        Ok(info)
    }
}

/// 目录相对路径 → 展示用（统一 `/`）。
pub fn rel_display(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
