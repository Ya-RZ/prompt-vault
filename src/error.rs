//! 统一错误类型：每个变体对应 INTERFACES.md §0 错误码表里的一个 `error.code`。
//!
//! 铁律（开发文档 §10）：指纹不一致必须**拒绝**（409），不是警告。

use salvo::http::StatusCode;
use salvo::prelude::*;
use serde_json::json;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: &'static str,
    pub status: u16,
    pub message: String,
    pub detail: Option<serde_json::Value>,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    fn new(code: &'static str, status: u16, message: impl Into<String>) -> Self {
        Self { code, status, message: message.into(), detail: None }
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new("BAD_REQUEST", 400, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("NOT_FOUND", 404, message)
    }

    /// 侧车连不上（开发文档 §5.3 强制离线 + 独立进程）。
    pub fn encoder_unreachable(message: impl Into<String>) -> Self {
        Self::new("ENCODER_UNREACHABLE", 503, message)
    }

    /// 写入/检索被拒：指纹与索引不一致（开发文档 §13 第 4 条）。
    pub fn fingerprint_mismatch(message: impl Into<String>) -> Self {
        Self::new("FINGERPRINT_MISMATCH", 409, message)
    }

    /// 图片超序列预算，侧车 413 透出（开发文档 §10 铁律 3）。
    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new("PAYLOAD_TOO_LARGE", 413, message)
    }

    pub fn encoder_error(message: impl Into<String>) -> Self {
        Self::new("ENCODER_ERROR", 502, message)
    }

    /// 金标校验不通过（开发文档 §9.4：作业开始前校验，不通过禁止开始）。
    pub fn golden_failed(message: impl Into<String>) -> Self {
        Self::new("GOLDEN_FAILED", 409, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new("CONFLICT", 409, message)
    }

    /// 稀疏路未启用时的显式回答（不假装成功）。
    pub fn sparse_unavailable(message: impl Into<String>) -> Self {
        Self::new("SPARSE_UNAVAILABLE", 501, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("INTERNAL", 500, message)
    }

    pub fn body(&self) -> serde_json::Value {
        let mut err = json!({ "code": self.code, "message": self.message });
        if let Some(detail) = &self.detail {
            err["detail"] = detail.clone();
        }
        json!({ "error": err })
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        match e {
            rusqlite::Error::QueryReturnedNoRows => ApiError::not_found("记录不存在"),
            other => ApiError::internal(format!("SQLite 错误: {other}")),
        }
    }
}

impl From<qdrant_edge::OperationError> for ApiError {
    fn from(e: qdrant_edge::OperationError) -> Self {
        ApiError::internal(format!("Qdrant Edge 错误: {e}"))
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}

#[async_trait]
impl Writer for ApiError {
    async fn write(mut self, _req: &mut Request, _depot: &mut Depot, res: &mut Response) {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        res.status_code(status);
        res.render(Json(self.body()));
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
