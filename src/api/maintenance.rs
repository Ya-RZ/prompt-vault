//! 运维口：向量导出/重建/优化、金标校验、编码器状态刷新（开发文档 §6.3 导出通路 + §9）。
//!
//! ⚠️ `qdrant-edge` 只有 0.8.0，所以导出通路是**必做项**：保证随时能搬家（§6.3）。

use salvo::prelude::*;

use super::{st, ApiResult};
use crate::error::ApiError;
use crate::state::AppState;
use crate::types::{GoldenInfo, MessageResponse};

#[handler]
pub async fn vector_info(depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    res.render(Json(serde_json::json!({
        "dir": state.cfg.vector_dir.display().to_string(),
        "dim": state.cfg.dim,
        "sparse_enabled": state.vectors.sparse_enabled(),
        "quantization": state.vectors.quantization().as_str(),
        "points": state.vectors.points_estimate(),
        "assets_points": state.vectors.count_assets().ok(),
        "disk_bytes": state.vectors.disk_bytes(),
        "index_fingerprint": state.index_fingerprint()?,
        "vector_names": [
            crate::vector::VEC_IMAGE,
            crate::vector::VEC_TEXT,
            crate::vector::VEC_SPARSE_IMAGE,
            crate::vector::VEC_SPARSE_TEXT,
        ],
    })));
    Ok(())
}

#[handler]
pub async fn vector_optimize(depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let optimized = state.vectors.optimize()?;
    res.render(Json(serde_json::json!({
        "optimized": optimized,
        "note": "删除是标记删除，必须调 optimize（开发文档 §6.3：没有后台 optimizer）",
        "points": state.vectors.points_estimate(),
    })));
    Ok(())
}

#[handler]
pub async fn vector_export(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let body: serde_json::Value = req.parse_json().await.unwrap_or(serde_json::Value::Null);
    let path = body
        .get("path")
        .and_then(|v| v.as_str())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            state
                .cfg
                .export_dir
                .join(format!("vectors-{}.jsonl", chrono::Utc::now().format("%Y%m%d-%H%M%S")))
        });
    let points = state.vectors.export_jsonl(&path)?;
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    res.render(Json(serde_json::json!({
        "exported_points": points,
        "path": path.display().to_string(),
        "bytes": bytes,
    })));
    Ok(())
}

#[handler]
pub async fn vector_import(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let body: serde_json::Value = req
        .parse_json()
        .await
        .map_err(|e| ApiError::bad_request(format!("请求体解析失败: {e}")))?;
    let path = body
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::bad_request("缺少 path"))?;
    let imported = state.vectors.import_jsonl(path)?;
    let _ = state.vectors.optimize();
    res.render(Json(serde_json::json!({ "imported_points": imported, "path": path })));
    Ok(())
}

/// 金标校验（开发文档 §9）：candle 没有可复用的历史基线，恒为 unsupported，不假装通过。
#[handler]
pub async fn golden_verify(depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let info: GoldenInfo = state.encoder.verify_golden();
    state.set_golden(info.clone());
    let will_block = info.status == "failed";
    res.render(Json(serde_json::json!({
        "golden": info,
        "blocks_jobs": will_block,
        "note": "status=failed 时导入/编码作业会被拒绝（开发文档 §9.4）；unsupported=当前后端没有历史基线"
    })));
    Ok(())
}

/// 编码器状态刷新：candle 没有远端可探，返回当前进程内的模型/设备/加载状态。
#[handler]
pub async fn encoder_refresh(depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let info = crate::encode::health_snapshot(&state.encoder);
    res.render(Json(info));
    Ok(())
}

/// 进程内的自检：把「索引指纹 vs 当前编码指纹」的结论直接给出来。
#[allow(dead_code)]
pub fn space_check(state: &AppState) -> serde_json::Value {
    let mut warnings = Vec::new();
    let consistent = state.spaces_are_consistent(&mut warnings).unwrap_or(false);
    serde_json::json!({ "consistent": consistent, "warnings": warnings })
}

#[allow(dead_code)]
fn _msg(m: impl Into<String>) -> MessageResponse {
    MessageResponse { message: m.into(), data: None }
}
