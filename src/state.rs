//! 运行时状态与配置。

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::db::{Db, META_INDEX_FINGERPRINT};
use crate::encode::{EncoderClient, EncoderMode};
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
    pub vector_dir: PathBuf,
    pub thumb_dir: PathBuf,
    pub export_dir: PathBuf,
    pub dim: usize,
    /// 是否在索引里启用稀疏（BM25）向量 —— 只在建 shard 时生效
    pub sparse_enabled: bool,
    pub quant: QuantMode,
    pub encoder_url: String,
    pub encoder_mode: EncoderMode,
    /// candle 模式的模型目录（None = 按约定路径查找）
    pub model_dir: Option<PathBuf>,
    /// candle 权重精度（f32 / f16 / bf16）
    pub candle_dtype: String,
    /// candle 计算设备：auto（有 CUDA 就用）/ cpu / cuda:N
    pub candle_device: String,
    /// candle 的 HuggingFace 缓存根目录（HF_HOME）；不再写进模型目录
    pub hf_cache_dir: PathBuf,
    /// 序列长度上限（与侧车口径一致：8192）
    pub max_length: usize,
    /// web/dist 的路径（开发期直接读盘；release 走 rust-embed）
    pub web_dir: PathBuf,
    /// 媒体文件大小上限（字节），超了直接拒（开发文档 §12 第 8 条）
    pub max_media_bytes: u64,
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
    pub fn new(mut cfg: Config) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        std::fs::create_dir_all(&cfg.media_dir)?;
        std::fs::create_dir_all(&cfg.thumb_dir)?;
        std::fs::create_dir_all(&cfg.export_dir)?;

        let db = Db::open(&cfg.db_path)?;
        // 进程重启后把残留的 running 作业标成 paused（断点续传，不假装在跑）
        if let Ok(n) = db.reconcile_jobs_on_start() {
            if n > 0 {
                tracing::warn!(count = n, "发现上次未完成作业，已标记为 paused（可 resume）");
            }
        }
        // 稀疏向量的开关只在建 shard 时生效（开发文档 §6.1：配置不可变），
        // 所以以库里记的为准，命令行只能影响第一次建索引。
        match db.get_meta(crate::db::META_SPARSE_ENABLED)? {
            Some(v) => {
                let recorded = v == "true";
                if recorded != cfg.sparse_enabled {
                    tracing::warn!(
                        recorded,
                        requested = cfg.sparse_enabled,
                        "稀疏开关与已建索引不一致，以索引记录为准（改了要重建 shard）"
                    );
                }
                cfg.sparse_enabled = recorded;
            }
            None => db.set_meta(
                crate::db::META_SPARSE_ENABLED,
                if cfg.sparse_enabled { "true" } else { "false" },
            )?,
        }
        let vectors = VectorStore::open(&cfg.vector_dir, cfg.dim, cfg.sparse_enabled, cfg.quant)?;
        // candle 模式：启动时就把模型目录解析好（显式指定必须有效，找不到直接启动失败，
        // 比跑到第一次编码才发现问题早得多）。权重本身仍是懒加载 —— 2B 模型 mmap 要几十秒。
        let candle = match cfg.encoder_mode {
            EncoderMode::Candle => {
                let dir = crate::encode::candle::resolve_model_dir(cfg.model_dir.as_deref())
                    .map_err(|e| anyhow::anyhow!("candle 模型目录解析失败: {}", e.message))?;
                let dtype = crate::encode::candle::parse_dtype(&cfg.candle_dtype);
                tracing::info!(
                    model_dir = %dir.display(),
                    dtype = %cfg.candle_dtype,
                    device = %cfg.candle_device,
                    hf_cache = %cfg.hf_cache_dir.display(),
                    max_length = cfg.max_length,
                    "candle 后端已配置（权重将在首次编码时加载）"
                );
                Some(std::sync::Arc::new(crate::encode::CandleEncoder::new(
                    dir,
                    cfg.hf_cache_dir.clone(),
                    cfg.max_length,
                    dtype,
                    crate::encode::candle::DeviceSpec::parse(&cfg.candle_device),
                    cfg.max_media_bytes,
                )))
            }
            _ => None,
        };
        let encoder = EncoderClient::new(
            cfg.encoder_url.clone(),
            cfg.encoder_mode,
            cfg.dim,
            candle,
        );

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
                "hint": "不同空间的向量不能混进同一个索引：确认编码设备/侧车配置后全量重编码"
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
