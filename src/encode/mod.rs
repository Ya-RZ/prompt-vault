//! 进程内编码层：用 candle 加载本地 Qwen3-VL-Embedding-2B。
//!
//! 历史：这里曾经对接一个 Python HTTP 侧车（`live` 模式），并内置一个 mock 编码器
//! 用于无模型跑通链路。两者现在都已移除 —— 只剩一条真实路径：进程内 candle。
//!
//! 这样做的好处很直接：编码空间只有一种实现，指纹闸门不再需要区分
//! 「侧车算出来的向量」和「进程内算出来的向量」，混用的风险从设计上消失了。

use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::error::{ApiError, ApiResult};
use crate::types::{EmbedInputItem, GoldenInfo};

pub mod candle;

pub use candle::{CandleEncoder, DeviceSpec, MAX_PIXELS};

#[derive(Debug, Clone)]
pub struct EncodeOutcome {
    pub dense: Vec<f32>,
    pub sparse: Option<(Vec<u32>, Vec<f32>)>,
    pub resized: Option<String>,
    pub vision_tokens: Option<i64>,
    pub kind: Option<String>,
    pub fingerprint: String,
}

pub struct EncoderClient {
    dim: usize,
    /// 进程内编码器（权重懒加载：第一次编码才 mmap）
    candle: Option<Arc<CandleEncoder>>,
    last_error: Mutex<Option<String>>,
    last_checked_at: Mutex<Option<String>>,
}

impl EncoderClient {
    pub fn new(dim: usize, candle: Option<Arc<CandleEncoder>>) -> Self {
        Self {
            dim,
            candle,
            last_error: Mutex::new(None),
            last_checked_at: Mutex::new(None),
        }
    }

    /// 进程内编码器（None 表示没有可用的模型目录）
    pub fn candle_encoder(&self) -> Option<Arc<CandleEncoder>> {
        self.candle.clone()
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|e| e.clone())
    }

    pub fn last_checked_at(&self) -> Option<String> {
        self.last_checked_at.lock().ok().and_then(|e| e.clone())
    }

    fn set_error(&self, msg: Option<String>) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = msg;
        }
        if let Ok(mut t) = self.last_checked_at.lock() {
            *t = Some(crate::db::now_rfc3339());
        }
    }

    /// 当前编码空间的指纹。没有编码器时给一个显式占位串，让索引闸门能拦住写入。
    pub fn fingerprint_hint(&self) -> String {
        self.candle
            .as_ref()
            .map(|c| c.fingerprint())
            .unwrap_or_else(|| "candle|unconfigured".to_string())
    }

    /// 编码一批输入，按输入顺序返回。
    ///
    /// 推理在阻塞线程里跑：2B 模型一次前向是秒级，直接占用 tokio worker 会把
    /// 同一时间的所有请求都拖住。
    pub async fn embed(&self, input: Vec<EmbedInputItem>) -> ApiResult<Vec<EncodeOutcome>> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let encoder = self
            .candle
            .as_ref()
            .ok_or_else(|| ApiError::encoder_error("编码器未初始化（没有可用的模型目录）"))?
            .clone();
        let payload = input.clone();
        match tokio::task::spawn_blocking(move || encoder.embed_blocking(&payload)).await {
            Ok(Ok(out)) => {
                self.set_error(None);
                Ok(out)
            }
            Ok(Err(e)) => {
                self.set_error(Some(e.message.clone()));
                Err(e)
            }
            Err(e) => {
                let err = ApiError::encoder_error(format!("编码线程异常: {e}"));
                self.set_error(Some(err.message.clone()));
                Err(err)
            }
        }
    }

    /// 金标校验。
    ///
    /// candle 没有可复用的历史基线（它与被移除的 Python 实现不同源，基线不通用），
    /// 所以恒为 `unsupported` —— 这里不假装通过，也不拦作业。
    pub fn verify_golden(&self) -> GoldenInfo {
        GoldenInfo {
            status: "unsupported".into(),
            passed: None,
            checked_at: Some(crate::db::now_rfc3339()),
            detail: Some(
                "candle 后端没有可复用的历史基线（与旧实现不同源）；换后端后需全量重编码".into(),
            ),
            cos_text: None,
            cos_image: None,
            threshold: None,
            device_mismatch: false,
            encoded_device: Some(
                self.candle
                    .as_ref()
                    .map(|c| c.device_label().to_string())
                    .unwrap_or_else(|| "cpu".to_string()),
            ),
        }
    }
}

/// 给 `/api/health` 用的编码器摘要。
pub fn health_snapshot(client: &EncoderClient) -> crate::types::EncoderInfo {
    let candle = client.candle_encoder();
    let ready = candle
        .as_ref()
        .map(|c| candle::looks_like_model_dir_public(c.model_dir()))
        .unwrap_or(false);

    crate::types::EncoderInfo {
        reachable: ready,
        model: Some(candle::MODEL_REPO_ID.to_string()),
        device: candle.as_ref().map(|c| c.device_display()),
        dim: Some(client.dim() as i64),
        model_dir: candle.as_ref().map(|c| c.model_dir().display().to_string()),
        max_pixels: Some(candle::MAX_PIXELS),
        max_length: candle.as_ref().map(|c| c.max_length() as i64),
        loaded: candle.as_ref().map(|c| c.is_loaded()).unwrap_or(false),
        fingerprint: Some(client.fingerprint_hint()),
        mode: "candle".to_string(),
        last_error: client.last_error(),
        last_checked_at: client.last_checked_at().unwrap_or_else(crate::db::now_rfc3339),
    }
}

/// data URL 构造（统一用 base64 提交图片，避开路径编码问题）。
pub fn image_data_url(bytes: &[u8], mime: &str) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!("data:{mime};base64,{encoded}")
}

/// 猜测 mime（只按扩展名，够用）。
pub fn mime_for_path(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".bmp") {
        "image/bmp"
    } else if lower.ends_with(".mp4") {
        "video/mp4"
    } else {
        "image/jpeg"
    }
}

/// 便于测试与调试：把结果里的向量摘要成可读 JSON。
pub fn describe(outcome: &EncodeOutcome) -> serde_json::Value {
    let norm = outcome.dense.iter().map(|x| x * x).sum::<f32>().sqrt();
    json!({
        "dim": outcome.dense.len(),
        "l2_norm": norm,
        "kind": outcome.kind,
        "resized": outcome.resized,
        "vision_tokens": outcome.vision_tokens,
        "sparse_terms": outcome.sparse.as_ref().map(|(i, _)| i.len()),
        "fingerprint": outcome.fingerprint,
    })
}
