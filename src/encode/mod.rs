//! 编码侧车客户端（开发文档 §5、§9）。
//!
//! 侧车是 Python + 官方 `Qwen3VLEmbedder`，**本项目不修改它**，只按现状对接：
//! - `GET /health` → 指纹、设备、维度
//! - `POST /v1/embeddings` → 稠密 2048 维；若响应里带 `sparse` 就一并使用
//! - `GET|POST /v1/verify` → 金标校验（**侧车未实现时降级为 unsupported，不假装通过**）
//!
//! 另内置一个开发用 **mock 编码器**（`PV_ENCODER=mock`）：不加载任何模型，
//! 用 deterministic 伪向量跑通「导入→编码→入库→检索→前端」全链路。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use qdrant_edge::external::serde_json as _;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::{ApiError, ApiResult};
use crate::types::{
    EmbedInputItem, EmbedItem, EmbedRequest, EmbedResponse, GoldenInfo, SidecarHealth,
};

pub mod candle;

pub use candle::{CandleEncoder, DeviceSpec, MAX_PIXELS};

pub const MOCK_FINGERPRINT: &str = "mock|dim=2048|v1|device=cpu";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderMode {
    /// 真侧车（HTTP）
    Live,
    /// 内置假编码器（不运行模型）
    Mock,
    /// 进程内 candle（直接加载 Qwen3-VL-Embedding-2B，替代 Python 侧车）
    Candle,
}

impl EncoderMode {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "mock" | "fake" => EncoderMode::Mock,
            "candle" | "local" | "inproc" | "rust" => EncoderMode::Candle,
            _ => EncoderMode::Live,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            EncoderMode::Live => "live",
            EncoderMode::Mock => "mock",
            EncoderMode::Candle => "candle",
        }
    }
}

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
    http: reqwest::Client,
    base: String,
    mode: EncoderMode,
    dim: usize,
    /// candle 模式下的进程内编码器（懒加载，第一次编码才 mmap 权重）
    candle: Option<Arc<CandleEncoder>>,
    /// 最近一次看到的侧车信息（health 或 embeddings 响应里的）
    last_health: Mutex<Option<SidecarHealth>>,
    last_error: Mutex<Option<String>>,
    last_checked_at: Mutex<Option<String>>,
}

impl EncoderClient {
    pub fn new(
        base: impl Into<String>,
        mode: EncoderMode,
        dim: usize,
        candle: Option<Arc<CandleEncoder>>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(1800))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self {
            http,
            base: base.into(),
            mode,
            dim,
            candle,
            last_health: Mutex::new(None),
            last_error: Mutex::new(None),
            last_checked_at: Mutex::new(None),
        }
    }

    pub fn mode(&self) -> EncoderMode {
        self.mode
    }

    /// candle 后端（None 表示当前不是 candle 模式或没配模型目录）
    pub fn candle_encoder(&self) -> Option<Arc<CandleEncoder>> {
        self.candle.clone()
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// 当前应该使用的指纹：mock 模式下是固定的 mock 指纹。
    pub fn fingerprint_hint(&self) -> String {
        match self.mode {
            EncoderMode::Mock => MOCK_FINGERPRINT.to_string(),
            EncoderMode::Candle => self
                .candle
                .as_ref()
                .map(|c| c.fingerprint())
                .unwrap_or_else(|| "candle|unconfigured".to_string()),
            EncoderMode::Live => self
                .last_health
                .lock()
                .ok()
                .and_then(|h| h.as_ref().and_then(|h| h.fingerprint.clone()))
                .unwrap_or_else(|| format!("Qwen3-VL-Embedding-2B|dim={}|device=unknown", self.dim)),
        }
    }

    pub fn cached_health(&self) -> Option<SidecarHealth> {
        self.last_health.lock().ok().and_then(|h| h.clone())
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|e| e.clone())
    }

    pub fn last_checked_at(&self) -> Option<String> {
        self.last_checked_at.lock().ok().and_then(|e| e.clone())
    }

    /// `GET /health`：拿指纹 + 设备。失败**不致命**（服务照常起，只是 DEGRADED）。
    pub async fn health(&self) -> Option<SidecarHealth> {
        let now = crate::db::now_rfc3339();
        if let Ok(mut t) = self.last_checked_at.lock() {
            *t = Some(now);
        }
        let url = format!("{}/health", self.base.trim_end_matches('/'));
        let result = self.http.get(&url).send().await;
        match result {
            Ok(resp) if resp.status().is_success() => match resp.json::<SidecarHealth>().await {
                Ok(h) => {
                    if let Ok(mut slot) = self.last_health.lock() {
                        *slot = Some(h.clone());
                    }
                    if let Ok(mut e) = self.last_error.lock() {
                        *e = None;
                    }
                    Some(h)
                }
                Err(e) => {
                    self.remember_error(format!("侧车 /health 响应解析失败: {e}"));
                    None
                }
            },
            Ok(resp) => {
                self.remember_error(format!("侧车 /health 返回 {}", resp.status()));
                None
            }
            Err(e) => {
                self.remember_error(format!("侧车连不上（{}）: {e}", self.base));
                None
            }
        }
    }

    fn remember_error(&self, msg: String) {
        if let Ok(mut e) = self.last_error.lock() {
            *e = Some(msg);
        }
    }

    /// 编码一批输入。响应 `data[i]` 与 `input[i]` 同序（侧车契约）。
    pub async fn embed(&self, input: Vec<EmbedInputItem>) -> ApiResult<Vec<EncodeOutcome>> {
        if input.is_empty() {
            return Ok(Vec::new());
        }
        // mock：确定性伪向量；candle：进程内推理（放阻塞线程，避免卡住 async 运行时）
        if self.mode == EncoderMode::Mock {
            return Ok(input.iter().map(|item| self.mock_encode(item)).collect());
        }
        if self.mode == EncoderMode::Candle {
            let encoder = self
                .candle
                .as_ref()
                .ok_or_else(|| ApiError::encoder_error("candle 后端未初始化（缺少模型目录）"))?
                .clone();
            let payload = input.clone();
            return tokio::task::spawn_blocking(move || encoder.embed_blocking(&payload))
                .await
                .map_err(|e| ApiError::encoder_error(format!("编码线程异常: {e}")))?;
        }

        let url = format!("{}/v1/embeddings", self.base.trim_end_matches('/'));
        let body = EmbedRequest { input };
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::encoder_unreachable(format!("侧车连不上（{}）: {e}", self.base)))?;

        let status = resp.status();
        if status.as_u16() == 413 {
            // 开发文档 §10 铁律 3：超预算宁可报错，也不要一个看起来正常的垃圾向量
            let detail = resp.json::<serde_json::Value>().await.unwrap_or(serde_json::Value::Null);
            let plan = detail.get("plan").cloned().unwrap_or(serde_json::Value::Null);
            let hint = plan
                .get("seq_len")
                .and_then(|v| v.as_i64())
                .map(|n| format!("图像会被静默截断：算出来的序列长度 {n} 超过 max_length，已拒绝"))
                .unwrap_or_else(|| "图像超序列预算，会被静默截断，已拒绝".to_string());
            return Err(ApiError::payload_too_large(hint).with_detail(detail));
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let short: String = text.chars().take(500).collect();
            return Err(ApiError::encoder_error(format!("侧车返回 {status}: {short}")));
        }

        let parsed: EmbedResponse = resp
            .json()
            .await
            .map_err(|e| ApiError::encoder_error(format!("侧车响应解析失败: {e}")))?;

        if let Some(fp) = parsed.fingerprint.clone() {
            if let Ok(mut slot) = self.last_health.lock() {
                let entry = slot.get_or_insert_with(|| SidecarHealth {
                    status: "READY".into(),
                    model: parsed.model.clone(),
                    device: None,
                    dim: parsed.dim,
                    max_pixels: None,
                    max_length: None,
                    template_overhead: None,
                    fingerprint: Some(fp.clone()),
                    model_dir: None,
                });
                entry.fingerprint = Some(fp.clone());
                if entry.dim.is_none() {
                    entry.dim = parsed.dim;
                }
            }
        }

        let dim = parsed.dim.unwrap_or(self.dim as i64) as usize;
        let fingerprint = parsed.fingerprint.clone().unwrap_or_else(|| self.fingerprint_hint());

        Ok(parsed.data.into_iter().map(|item| outcome_from_sidecar(item, fingerprint.clone(), dim)).collect())
    }

    /// 金标校验（开发文档 §9）。侧车没有 `/v1/verify` → `unsupported`。
    pub async fn verify_golden(&self) -> GoldenInfo {
        let checked_at = crate::db::now_rfc3339();
        if self.mode == EncoderMode::Candle {
            return GoldenInfo {
                status: "unsupported".into(),
                passed: None,
                checked_at: Some(checked_at),
                detail: Some(
                    "candle 后端没有历史基线（它与 Python 实现不同源，基线也不通用）；\
                     换后端后必须全量重编码，指纹闸门会拦住混用"
                        .into(),
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
            };
        }
        if self.mode == EncoderMode::Mock {
            return GoldenInfo {
                status: "unsupported".into(),
                passed: None,
                checked_at: Some(checked_at),
                detail: Some("mock 编码器不参与金标校验（未运行模型）".into()),
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
            };
        }
        let url = format!("{}/v1/verify", self.base.trim_end_matches('/'));
        let resp = match self.http.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return GoldenInfo {
                    status: "unchecked".into(),
                    passed: None,
                    checked_at: Some(checked_at),
                    detail: Some(format!("侧车连不上，金标未校验: {e}")),
                    cos_text: None,
                    cos_image: None,
                    threshold: None,
                    device_mismatch: false,
                    encoded_device: None,
                }
            }
        };
        if resp.status().as_u16() == 404 || resp.status().as_u16() == 405 {
            return GoldenInfo {
                status: "unsupported".into(),
                passed: None,
                checked_at: Some(checked_at),
                detail: Some(
                    "侧车未提供 /v1/verify（开发文档 §5.4 改动 1 未实施）——本项目禁止修改侧车，故降级为未校验"
                        .into(),
                ),
                cos_text: None,
                cos_image: None,
                threshold: None,
                device_mismatch: false,
                encoded_device: None,
            };
        }
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return GoldenInfo {
                    status: "unchecked".into(),
                    passed: None,
                    checked_at: Some(checked_at),
                    detail: Some(format!("金标响应解析失败: {e}")),
                    cos_text: None,
                    cos_image: None,
                    threshold: None,
                    device_mismatch: false,
                    encoded_device: None,
                }
            }
        };
        let passed = body.get("passed").and_then(|v| v.as_bool());
        GoldenInfo {
            status: if passed == Some(true) { "passed".into() } else { "failed".into() },
            passed,
            checked_at: Some(checked_at),
            detail: body.get("detail").and_then(|v| v.as_str()).map(|s| s.to_string()),
            cos_text: body.get("cos_text").and_then(|v| v.as_f64()),
            cos_image: body.get("cos_image").and_then(|v| v.as_f64()),
            threshold: body.get("threshold").and_then(|v| v.as_f64()),
            device_mismatch: body.get("device_mismatch").and_then(|v| v.as_bool()).unwrap_or(false),
            encoded_device: body.get("device").and_then(|v| v.as_str()).map(|s| s.to_string()),
        }
    }

    /// mock：确定性伪向量。同一输入永远同一向量；不同输入近似正交，
    /// 这样「相似度排序」在全链路上仍然有意义。
    fn mock_encode(&self, item: &EmbedInputItem) -> EncodeOutcome {
        let seed = format!(
            "text={}|image={}",
            item.text.as_deref().unwrap_or(""),
            item.image.as_deref().unwrap_or("")
        );
        let dense = mock_dense(&seed, self.dim);
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
        }
    }
}

fn outcome_from_sidecar(item: EmbedItem, fingerprint: String, dim: usize) -> EncodeOutcome {
    let mut dense = item.embedding;
    if dense.len() != dim {
        tracing::warn!(got = dense.len(), want = dim, "侧车向量维度与声明不一致");
    }
    if dense.len() > dim {
        dense.truncate(dim);
    }
    let sparse = item.sparse.map(|s| (s.indices, s.values));
    EncodeOutcome {
        dense,
        sparse,
        resized: item.resized,
        vision_tokens: item.vision_tokens,
        kind: item.kind,
        fingerprint,
    }
}

// ---------------------------------------------------------------- mock 向量

/// sha256 流式展开 → 均匀分布 → L2 归一化（真向量也是 L2 归一化的，口径保持一致）。
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
    if tokens.iter().all(|t| t.chars().all(|c| !c.is_ascii())){
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

/// 侧车返回里是否带了稀疏向量（决定稀疏路能不能跑）。
pub fn has_sparse(outcomes: &[EncodeOutcome]) -> bool {
    outcomes.iter().any(|o| o.sparse.is_some())
}

/// 给 `/api/health` 用的侧车摘要（含 mock 标记）。
pub fn health_snapshot(
    client: &EncoderClient,
    model_dir: Option<String>,
    sidecar: Option<SidecarHealth>,
) -> crate::types::EncoderInfo {
    let mode = client.mode();
    let dim = sidecar
        .as_ref()
        .and_then(|h| h.dim)
        .unwrap_or(client.dim() as i64);

    // candle：进程内后端，没有 HTTP 侧车可探；可用性看模型目录是否解析成功
    let candle = client.candle_encoder();
    let candle_ready = mode == EncoderMode::Candle
        && candle
            .as_ref()
            .map(|c| candle::looks_like_model_dir_public(c.model_dir()))
            .unwrap_or(false);

    let reachable = match mode {
        EncoderMode::Mock => true,
        EncoderMode::Candle => candle_ready,
        EncoderMode::Live => sidecar.is_some(),
    };

    crate::types::EncoderInfo {
        reachable,
        url: match mode {
            EncoderMode::Candle => format!("in-process:candle://{}", candle.as_ref().map(|c| c.model_dir().display().to_string()).unwrap_or_default()),
            _ => client.base().to_string(),
        },
        model: sidecar
            .as_ref()
            .and_then(|h| h.model.clone())
            .or_else(|| match mode {
                EncoderMode::Mock => Some("mock".to_string()),
                EncoderMode::Candle => Some(candle::MODEL_REPO_ID.to_string()),
                EncoderMode::Live => None,
            }),
        device: sidecar
            .as_ref()
            .and_then(|h| h.device.clone())
            .or_else(|| match mode {
                EncoderMode::Mock => Some("cpu".to_string()),
                EncoderMode::Candle => Some(
                    candle
                        .as_ref()
                        .map(|c| c.device_label().to_string())
                        .unwrap_or_else(|| "cpu".to_string()),
                ),
                EncoderMode::Live => None,
            }),
        dim: Some(dim),
        model_dir: model_dir.or_else(|| {
            candle
                .as_ref()
                .map(|c| c.model_dir().display().to_string())
        }),
        max_pixels: sidecar
            .as_ref()
            .and_then(|h| h.max_pixels)
            .or_else(|| (mode == EncoderMode::Candle).then_some(candle::MAX_PIXELS)),
        max_length: sidecar.as_ref().and_then(|h| h.max_length),
        template_overhead: sidecar.as_ref().and_then(|h| h.template_overhead),
        fingerprint: Some(client.fingerprint_hint()),
        mode: mode.as_str().to_string(),
        last_error: client.last_error(),
        last_checked_at: client.last_checked_at().unwrap_or_else(crate::db::now_rfc3339),
    }
}

/// data URL 构造（主服务统一用 base64 提交图片，避开路径编码问题）。
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

/// 便于测试与调试：把结果里的向量摘要成可读字符串。
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
