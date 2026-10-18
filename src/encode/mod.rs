//! 进程内编码层：用 candle 加载本地 Qwen3-VL-Embedding-2B。
//!
//! 编码只有一条真实路径（进程内 candle）；另有 `--mock` 开关，用确定性伪向量
//! 跑通「导入 → 编码 → 入库 → 检索 → 前端」全链路，不加载任何模型（仅开发/冒烟用）。
//!
//! 没有可用模型时服务**仍能启动**：`EncoderClient` 处于「编码不可用」状态，
//! 检索自动退化为纯词法（lexical），写入类接口返回明确错误。

use std::sync::{Arc, Mutex};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::{ApiError, ApiResult};
use crate::types::{EmbedInputItem, GoldenInfo};

pub mod candle;

pub use candle::{CandleEncoder, DeviceSpec, MAX_PIXELS};

/// mock 模式的固定指纹：只要开关不变，向量空间就自洽。
pub const MOCK_FINGERPRINT: &str = "mock|dim=2048|v1|device=cpu";

#[derive(Debug, Clone)]
pub struct EncodeOutcome {
    pub dense: Vec<f32>,
    pub sparse: Option<(Vec<u32>, Vec<f32>)>,
    pub resized: Option<String>,
    pub vision_tokens: Option<i64>,
    pub kind: Option<String>,
    pub fingerprint: String,
    /// 预处理提示（图片被等比缩小等）。上层负责把它们带进检索响应的 warnings。
    pub notices: Vec<String>,
}

pub struct EncoderClient {
    dim: usize,
    /// 进程内编码器（权重懒加载：第一次编码才 mmap）。没有模型时为 None。
    candle: Option<Arc<CandleEncoder>>,
    /// `--mock`：不加载模型，用确定性伪向量（仅开发/冒烟）
    mock: bool,
    last_error: Mutex<Option<String>>,
    last_checked_at: Mutex<Option<String>>,
}

impl EncoderClient {
    pub fn new(dim: usize, candle: Option<Arc<CandleEncoder>>, mock: bool) -> Self {
        Self {
            dim,
            candle,
            mock,
            last_error: Mutex::new(None),
            last_checked_at: Mutex::new(None),
        }
    }

    pub fn is_mock(&self) -> bool {
        self.mock
    }

    /// 编码能力是否可用：mock 模式恒可用，否则要有模型。
    ///
    /// 这个判断决定了「检索要不要退化为纯词法」以及 health 的 reachable。
    pub fn is_available(&self) -> bool {
        self.mock || self.candle.is_some()
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

    /// 当前编码空间的指纹。没有编码能力时给一个显式占位串，让索引闸门能拦住写入。
    pub fn fingerprint_hint(&self) -> String {
        if self.mock {
            return MOCK_FINGERPRINT.to_string();
        }
        match &self.candle {
            Some(c) => c.fingerprint(),
            None => "candle|unconfigured".to_string(),
        }
    }

    /// 编码一批输入，按输入顺序返回。
    ///
    /// 推理在阻塞线程里跑：2B 模型一次前向是秒级，直接占用 tokio worker 会把
    /// 同一时间的所有请求都拖住。
    pub async fn embed(&self, input: Vec<EmbedInputItem>) -> ApiResult<Vec<EncodeOutcome>> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        if self.mock {
            let out: Vec<EncodeOutcome> = input.iter().map(|item| mock_encode(item, self.dim)).collect();
            self.set_error(None);
            return Ok(out);
        }
        let encoder = self.candle.as_ref().ok_or_else(|| {
            ApiError::encoder_error(
                "编码不可用：没有可用模型（用 --model-dir 指定模型目录，或加 --mock 跑链路验证）",
            )
        })?
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
    /// mock 模式更没跑模型 —— 两种情况都返回 `unsupported`，不假装通过、也不拦作业。
    pub fn verify_golden(&self) -> GoldenInfo {
        let detail = if self.mock {
            "mock 模式未运行模型，不参与金标校验"
        } else {
            "candle 后端没有可复用的历史基线（与旧实现不同源）；换后端后需全量重编码"
        };
        GoldenInfo {
            status: "unsupported".into(),
            passed: None,
            checked_at: Some(crate::db::now_rfc3339()),
            detail: Some(detail.into()),
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
    let mock = client.is_mock();
    let model_ok = candle
        .as_ref()
        .map(|c| candle::looks_like_model_dir_public(c.model_dir()))
        .unwrap_or(false);

    crate::types::EncoderInfo {
        reachable: mock || model_ok,
        mock,
        model: Some(if mock {
            "mock".to_string()
        } else {
            candle::MODEL_REPO_ID.to_string()
        }),
        device: if mock {
            Some("cpu".to_string())
        } else {
            candle.as_ref().map(|c| c.device_display())
        },
        dim: Some(client.dim() as i64),
        model_dir: candle.as_ref().map(|c| c.model_dir().display().to_string()),
        max_pixels: candle.as_ref().map(|c| c.max_pixels() as i64),
        max_length: candle.as_ref().map(|c| c.max_length() as i64),
        loaded: candle.as_ref().map(|c| c.is_loaded()).unwrap_or(false),
        fingerprint: Some(client.fingerprint_hint()),
        last_error: client.last_error(),
        last_checked_at: client.last_checked_at().unwrap_or_else(crate::db::now_rfc3339),
    }
}

// ---------------------------------------------------------------- mock 向量

/// mock：确定性伪向量。同一输入永远同一向量；不同输入近似正交，
/// 这样「相似度排序」在全链路上仍然有意义。
fn mock_encode(item: &EmbedInputItem, dim: usize) -> EncodeOutcome {
    let seed = format!(
        "text={}|image={}",
        item.text.as_deref().unwrap_or(""),
        item.image.as_deref().unwrap_or("")
    );
    let dense = mock_dense(&seed, dim);
    let sparse = item.text.as_deref().map(mock_sparse);
    let kind = match (&item.text, &item.image) {
        (Some(_), Some(_)) => "text+image",
        (None, Some(_)) => "image",
        _ => "text",
    };
    EncodeOutcome {
        dense,
        sparse,
        resized: item.image.as_ref().map(|_| "mock".to_string()),
        vision_tokens: item.image.as_ref().map(|_| 0),
        kind: Some(kind.to_string()),
        fingerprint: MOCK_FINGERPRINT.to_string(),
        notices: Vec::new(),
    }
}

/// sha256 流式展开 → 均匀分布 → L2 归一化（与真实编码器一样输出单位向量）。
pub fn mock_dense(seed: &str, dim: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(dim);
    let mut counter: u32 = 0;
    while out.len() < dim {
        let mut hasher = Sha256::new();
        hasher.update(seed.as_bytes());
        hasher.update(counter.to_le_bytes());
        let digest = hasher.finalize();
        for chunk in digest.chunks(4) {
            if out.len() >= dim {
                break;
            }
            let mut b = [0u8; 4];
            b.copy_from_slice(chunk);
            let raw = u32::from_le_bytes(b) as f32 / u32::MAX as f32;
            out.push(raw * 2.0 - 1.0);
        }
        counter += 1;
    }
    let norm = out.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in out.iter_mut() {
            *v /= norm;
        }
    }
    out
}

/// 把文本切成 token 再做 TF（模拟 BM25 的输入形状；真正的 IDF 由 Qdrant 侧算）。
pub fn mock_sparse(text: &str) -> (Vec<u32>, Vec<f32>) {
    let mut counts: std::collections::BTreeMap<u32, f32> = std::collections::BTreeMap::new();
    let mut tokens: Vec<String> = Vec::new();
    let mut buf = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            buf.push(ch.to_ascii_lowercase());
        } else if !buf.is_empty() {
            tokens.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        tokens.push(buf);
    }
    // 中文没有空格：补一层 2-gram，保证中文查询也能命中
    if tokens.iter().all(|t| t.chars().all(|c| !c.is_ascii())) {
        let chars: Vec<char> = text.chars().filter(|c| c.is_alphanumeric()).collect();
        for w in chars.windows(2) {
            tokens.push(w.iter().collect());
        }
    }
    for token in tokens {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        let digest = hasher.finalize();
        let idx = u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]);
        *counts.entry(idx).or_insert(0.0) += 1.0;
    }
    let indices: Vec<u32> = counts.keys().copied().collect();
    let values: Vec<f32> = counts.values().copied().collect();
    (indices, values)
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
