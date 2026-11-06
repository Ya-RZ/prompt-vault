//! 编码层：调用第三方 embedding API（OpenAI 兼容 `/v1/embeddings`）。
//!
//! 2026-11 起不再在进程内加载模型（原 candle + 4 GB 权重已删除）：
//! 服务启动只需要一个 base_url + model + api_key，内存与显存占用降到最低。
//!
//! 两条路径：
//! - **真实编码**：`HttpEncoder`（见 `http.rs`），文本与图片走同一个模型空间；
//! - **`--mock`**：不联网，用 sha256 派生确定性伪向量，只用于开发/冒烟。
//!
//! 没配置 API Key / 服务连不上时服务**仍能启动**：`EncoderClient` 处于
//! 「编码不可用」状态，检索自动退化为纯词法，写入类接口返回明确错误。

use std::sync::{Arc, Mutex};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::{ApiError, ApiResult};
use crate::types::{EmbedInputItem, GoldenInfo};

pub mod http;

pub use http::{HttpEncoder, HttpEncoderConfig};

/// mock 模式的固定指纹：只要开关不变，向量空间就自洽。
pub const MOCK_FINGERPRINT: &str = "mock|dim=4096|v1|device=cpu";

/// 金标阈值：云端服务没有可复用的本地基线，这里只用于「探测请求是否成功」，
/// 不再做同设备余弦比对。
pub const GOLDEN_THRESHOLD: f64 = 0.999;

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
    /// 第三方编码器；没配置时为 None（编码不可用，检索降级为纯词法）。
    http: Option<Arc<HttpEncoder>>,
    /// `--mock`：不联网，用确定性伪向量（仅开发/冒烟）
    mock: bool,
    last_error: Mutex<Option<String>>,
    last_checked_at: Mutex<Option<String>>,
}

impl EncoderClient {
    pub fn new(dim: usize, http: Option<Arc<HttpEncoder>>, mock: bool) -> Self {
        Self {
            dim,
            http,
            mock,
            last_error: Mutex::new(None),
            last_checked_at: Mutex::new(None),
        }
    }

    /// mock 模式：不联网，确定性伪向量。
    pub fn mock(dim: usize) -> Self {
        Self::new(dim, None, true)
    }

    /// 未配置第三方服务：服务可启动，但编码不可用。
    pub fn unconfigured(dim: usize) -> Self {
        Self::new(dim, None, false)
    }

    pub fn is_mock(&self) -> bool {
        self.mock
    }

    /// 编码能力是否可用：mock 恒可用，否则要有第三方客户端。
    pub fn is_available(&self) -> bool {
        self.mock || self.http.is_some()
    }

    pub fn provider(&self) -> &'static str {
        if self.mock {
            "mock"
        } else if self.http.is_some() {
            "openai-compatible"
        } else {
            "unconfigured"
        }
    }

    pub fn model(&self) -> Option<&str> {
        self.http.as_ref().map(|h| h.model())
    }

    pub fn base_url(&self) -> Option<&str> {
        self.http.as_ref().map(|h| h.base_url())
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
        match &self.http {
            Some(h) => h.fingerprint(),
            None => "http|unconfigured".to_string(),
        }
    }

    /// 编码一批输入，按输入顺序返回。
    pub async fn embed(&self, input: Vec<EmbedInputItem>) -> ApiResult<Vec<EncodeOutcome>> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        if self.mock {
            let out: Vec<EncodeOutcome> = input.iter().map(|item| mock_encode(item, self.dim)).collect();
            self.set_error(None);
            return Ok(out);
        }
        let encoder = self.http.as_ref().ok_or_else(|| {
            ApiError::encoder_error(
                "编码不可用：未配置第三方 embedding 服务（config.toml 的 [embedding] 或 --embedding-* 参数）",
            )
        })?;

        match encoder.embed(&input).await {
            Ok(vectors) => {
                self.set_error(None);
                Ok(input
                    .iter()
                    .zip(vectors)
                    .map(|(item, dense)| EncodeOutcome {
                        dense,
                        // 第三方只给稠密向量；稀疏路由检索层如实降级。
                        sparse: None,
                        resized: None,
                        vision_tokens: None,
                        kind: Some(kind_of(item)),
                        fingerprint: encoder.fingerprint(),
                        notices: Vec::new(),
                    })
                    .collect())
            }
            Err(e) => {
                self.set_error(Some(e.message.clone()));
                Err(e)
            }
        }
    }

    /// 金标快照：基于**最近一次真实调用**的结果，不再做本地基线余弦比对。
    ///
    /// - `--mock` → `unsupported`
    /// - 没配置服务 → `unchecked`（不阻塞，但 health 里 reachable=false）
    /// - 最近一次调用失败 → `failed`（作业闸门会拦）
    /// - 最近一次调用成功 → `passed`
    pub fn verify_golden(&self) -> GoldenInfo {
        let encoded_device = Some(if self.mock { "cpu" } else { "cloud" }.to_string());

        if self.mock {
            return GoldenInfo {
                status: "unsupported".into(),
                passed: None,
                checked_at: Some(crate::db::now_rfc3339()),
                detail: Some("mock 模式未调用第三方服务，不参与金标校验".into()),
                cos_text: None,
                cos_image: None,
                threshold: None,
                device_mismatch: false,
                encoded_device,
            };
        }

        if self.http.is_none() {
            return GoldenInfo {
                status: "unchecked".into(),
                passed: None,
                checked_at: self.last_checked_at(),
                detail: Some("未配置第三方 embedding 服务（config.toml 的 [embedding]）".into()),
                cos_text: None,
                cos_image: None,
                threshold: Some(GOLDEN_THRESHOLD),
                device_mismatch: false,
                encoded_device,
            };
        }

        match self.last_error() {
            Some(err) => GoldenInfo {
                status: "failed".into(),
                passed: Some(false),
                checked_at: self.last_checked_at(),
                detail: Some(format!("最近一次编码调用失败: {err}")),
                cos_text: None,
                cos_image: None,
                threshold: Some(GOLDEN_THRESHOLD),
                device_mismatch: false,
                encoded_device,
            },
            None if self.last_checked_at().is_some() => GoldenInfo {
                status: "passed".into(),
                passed: Some(true),
                checked_at: self.last_checked_at(),
                detail: Some("第三方 embedding 调用可用（云端无本地基线，只验证连通性与维度）".into()),
                cos_text: None,
                cos_image: None,
                threshold: Some(GOLDEN_THRESHOLD),
                device_mismatch: false,
                encoded_device,
            },
            None => GoldenInfo {
                status: "unchecked".into(),
                passed: None,
                checked_at: None,
                detail: Some("尚未发起过编码调用（可用 /api/maintenance/golden/verify 主动探测）".into()),
                cos_text: None,
                cos_image: None,
                threshold: Some(GOLDEN_THRESHOLD),
                device_mismatch: false,
                encoded_device,
            },
        }
    }

    /// 主动探测一次：调用 API 编码一条固定文本，顺便验证维度。
    pub async fn probe(&self) -> GoldenInfo {
        if self.mock || self.http.is_none() {
            return self.verify_golden();
        }
        let item = EmbedInputItem {
            text: Some("prompt-vault embedding probe".to_string()),
            image: None,
        };
        match self.embed(vec![item]).await {
            Ok(out) if out.len() == 1 && out[0].dense.len() == self.dim => self.verify_golden(),
            Ok(out) => {
                let got = out.first().map(|o| o.dense.len()).unwrap_or(0);
                let msg = format!("返回维度不一致：请求 {}，实际 {}", self.dim, got);
                self.set_error(Some(msg.clone()));
                let mut info = self.verify_golden();
                info.detail = Some(msg);
                info
            }
            Err(e) => {
                let mut info = self.verify_golden();
                info.detail = Some(diagnose(&e.message));
                info
            }
        }
    }
}

/// 给 `/api/health` 用的编码器摘要。
pub fn health_snapshot(client: &EncoderClient) -> crate::types::EncoderInfo {
    let mock = client.is_mock();
    crate::types::EncoderInfo {
        reachable: client.is_available() && client.last_error().is_none(),
        mock,
        model: Some(if mock {
            "mock".to_string()
        } else {
            client.model().unwrap_or("(未配置)").to_string()
        }),
        device: Some(if mock { "cpu" } else { "cloud" }.to_string()),
        dim: Some(client.dim() as i64),
        // 云端没有模型目录 / 序列长度这些本地概念，字段保留但为空。
        model_dir: None,
        max_pixels: None,
        max_length: None,
        loaded: !mock && client.is_available(),
        fingerprint: Some(client.fingerprint_hint()),
        last_error: client.last_error(),
        last_checked_at: client.last_checked_at().unwrap_or_else(crate::db::now_rfc3339),
    }
}

/// 把服务端原始错误翻译成可操作的提示（保留原文，便于对照 API 文档）。
fn diagnose(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    let hint = if lower.contains("401") || lower.contains("invalid api") || lower.contains("unauthorized") {
        "API Key 无效或过期：检查 [embedding] api_key"
    } else if lower.contains("404") || lower.contains("not found") {
        "服务地址或模型名不对：base_url 必须含 /v1（如 https://api.siliconflow.cn/v1），model 用文档里的完整名"
    } else if lower.contains("dimensions") || lower.contains("parameter is invalid") {
        "请求参数被拒：优先检查 dimensions 是否在模型支持列表（Qwen3 系列支持 64~4096；bge 等模型不支持 dimensions，需换成 Qwen3 或去掉该参数）"
    } else if lower.contains("429") || lower.contains("rate limit") {
        "被限流：稍后重试或降低并发"
    } else {
        "调用失败"
    };
    format!("{hint}；服务端原文: {raw}")
}

fn kind_of(item: &EmbedInputItem) -> String {
    match (&item.text, &item.image) {
        (Some(_), Some(_)) => "text+image",
        (None, Some(_)) => "image",
        _ => "text",
    }
    .to_string()
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
    EncodeOutcome {
        dense,
        sparse,
        resized: item.image.as_ref().map(|_| "mock".to_string()),
        vision_tokens: item.image.as_ref().map(|_| 0),
        kind: Some(kind_of(item)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_vectors_are_deterministic_and_normalized() {
        let a = mock_dense("hello", 64);
        let b = mock_dense("hello", 64);
        let c = mock_dense("world", 64);
        assert_eq!(a, b, "同一输入必须逐位相同");
        assert_ne!(a, c, "不同输入应产生不同向量");
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "向量应归一化，实际 {norm}");
    }

    #[test]
    fn video_input_is_rejected_by_http_encoder() {
        let item = EmbedInputItem {
            text: None,
            image: Some("data:video/mp4;base64,AAAA".to_string()),
        };
        let err = to_request_item_err(&item);
        assert!(err.contains("不支持视频"), "实际错误: {err}");
    }

    fn to_request_item_err(item: &EmbedInputItem) -> String {
        // 复用 http 模块的校验逻辑（不需要网络）
        crate::encode::http::validate_item(item)
            .err()
            .map(|e| e.message)
            .unwrap_or_default()
    }
}
