//! 第三方 embedding 客户端（OpenAI 兼容 `/v1/embeddings`）。
//!
//! 当前接硅基流动的 `Qwen/Qwen3-VL-Embedding-8B`；任何兼容该协议的服务
//! （改 `base_url` + `model` + `api_key`）都能直接用。
//!
//! 约定：
//! - 文本输入直接放字符串；图片输入放 `{"image": "data:image/...;base64,..."}`（硅基扩展）。
//! - **不产出 BM25 稀疏向量**：第三方只给稠密向量，检索层的稀疏路会如实降级。
//! - **不支持视频**：遇到 `data:video/*` 直接拒绝，而不是把 mp4 当图片发出去。
//! - 云端没有 device/精度可探，指纹只记录 base_url + model + dim。

use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::error::{ApiError, ApiResult};
use crate::types::EmbedInputItem;

#[derive(Debug, Clone)]
pub struct HttpEncoderConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub dim: usize,
    pub timeout_secs: u64,
}

impl HttpEncoderConfig {
    pub fn endpoint(&self) -> String {
        format!("{}/embeddings", self.base_url.trim_end_matches('/'))
    }

    /// 向量空间指纹。云端实现可能静默升级，所以这里把 base_url + model + dim 都钉进去，
    /// 换服务/换模型/换维度都会让写入闸门拦住旧索引。
    pub fn fingerprint(&self) -> String {
        format!(
            "http|{}|{}|dim={}|norm=l2|v1",
            self.base_url, self.model, self.dim
        )
    }
}

pub struct HttpEncoder {
    cfg: HttpEncoderConfig,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedDatum>,
}

#[derive(Deserialize)]
struct EmbedDatum {
    embedding: Vec<f32>,
    index: Option<usize>,
}

impl HttpEncoder {
    pub fn new(cfg: HttpEncoderConfig) -> ApiResult<Self> {
        if cfg.api_key.trim().is_empty() {
            return Err(ApiError::encoder_error(
                "embedding api_key 为空：在 config.toml 的 [embedding] 里填写 api_key，或设置 PV_EMBEDDING_API_KEY",
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs.max(5)))
            .build()
            .map_err(|e| ApiError::encoder_error(format!("HTTP 客户端初始化失败: {e}")))?;
        Ok(Self { cfg, client })
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    pub fn base_url(&self) -> &str {
        &self.cfg.base_url
    }

    pub fn dim(&self) -> usize {
        self.cfg.dim
    }

    pub fn fingerprint(&self) -> String {
        self.cfg.fingerprint()
    }

    /// 一次请求编码一批输入，按输入顺序返回。
    pub async fn embed(&self, input: &[EmbedInputItem]) -> ApiResult<Vec<Vec<f32>>> {
        if input.is_empty() {
            return Ok(Vec::new());
        }

        let mut items = Vec::with_capacity(input.len());
        for item in input {
            items.push(to_request_item(item)?);
        }

        let body = json!({
            "model": self.cfg.model,
            "input": items,
            "dimensions": self.cfg.dim,
            "encoding_format": "float",
        });

        let resp = self
            .client
            .post(self.cfg.endpoint())
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ApiError::encoder_error(format!("调用 embedding API 失败: {e}")))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ApiError::encoder_error(format!(
                "embedding API 返回 {status}: {}",
                truncate(&text, 300)
            )));
        }

        let parsed: EmbedResponse = serde_json::from_str(&text).map_err(|e| {
            ApiError::encoder_error(format!(
                "解析 embedding 响应失败: {e}; body={}",
                truncate(&text, 300)
            ))
        })?;

        let mut out: Vec<Option<Vec<f32>>> = vec![None; input.len()];
        for (pos, datum) in parsed.data.into_iter().enumerate() {
            let idx = datum.index.unwrap_or(pos);
            if idx < out.len() {
                out[idx] = Some(datum.embedding);
            }
        }
        out.into_iter()
            .enumerate()
            .map(|(i, v)| {
                v.ok_or_else(|| ApiError::encoder_error(format!("embedding 响应缺少第 {i} 项")))
            })
            .collect()
    }
}

/// 供测试与上层预校验使用：把输入转成请求项，非法输入直接报错（不发网络请求）。
pub fn validate_item(item: &EmbedInputItem) -> ApiResult<serde_json::Value> {
    to_request_item(item)
}

fn to_request_item(item: &EmbedInputItem) -> ApiResult<serde_json::Value> {    if let Some(image) = &item.image {
        if image.starts_with("data:video/") {
            return Err(ApiError::encoder_error(
                "第三方编码不支持视频输入：请先抽一帧或上传封面图，再把图片作为资产导入",
            ));
        }
        // 硅基的 input 元素一次只能承载一种模态；图片优先（图片里已包含画面信息）。
        return Ok(json!({ "image": image }));
    }
    match &item.text {
        Some(text) if !text.trim().is_empty() => Ok(json!(text)),
        _ => Err(ApiError::bad_request("编码输入为空：既没有文本也没有图片")),
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
