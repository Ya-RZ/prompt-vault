//! 运维口：向量导出/重建/优化、金标校验、编码器状态刷新（开发文档 §6.3 导出通路 + §9）。
//!
//! 导出通路仍然保留：即使换成 Qdrant Server，JSONL 仍是跨实现搬家的最稳格式。

use std::sync::Arc;

use salvo::prelude::*;

use super::{st, ApiResult};
use crate::error::ApiError;
use crate::state::AppState;
use crate::types::{GoldenInfo, MessageResponse};

#[handler]
pub async fn vector_info(depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    res.render(Json(serde_json::json!({
        "url": state.vectors.url(),
        "collection": state.vectors.collection(),
        "dim": state.cfg.dim,
        "sparse_enabled": state.vectors.sparse_enabled(),
        "quantization": state.vectors.quantization().as_str(),
        "points": state.vectors.points_estimate().await,
        "assets_points": state.vectors.count_assets().await.ok(),
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
        "points": state.vectors.points_estimate().await,
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
    let points = state.vectors.export_jsonl(&path).await?;
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
    let imported = state.vectors.import_jsonl(path).await?;
    let _ = state.vectors.optimize();
    res.render(Json(serde_json::json!({ "imported_points": imported, "path": path })));
    Ok(())
}

/// 金标校验（开发文档 §9）：主动探测第三方 embedding 服务；failed 时作业闸门会拦住。
#[handler]
pub async fn golden_verify(depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let info: GoldenInfo = state.encoder.probe().await;
    state.set_golden(info.clone());
    let will_block = info.status == "failed";
    res.render(Json(serde_json::json!({
        "golden": info,
        "blocks_jobs": will_block,
        "note": "status=failed 时导入/编码作业会被拒绝（开发文档 §9.4）；云端编码没有本地基线，只验证连通性与维度"
    })));
    Ok(())
}

/// 回填 ComfyUI 元数据：扫描已有 PNG 资产，重新解析正/负提示词与工作流（幂等）。
#[handler]
pub async fn comfy_reparse(depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let rows = state.db.png_assets()?;
    let total = rows.len();
    let mut updated = 0usize;
    for (id, rel) in rows {
        let path = state.cfg.media_path(&rel);
        if crate::jobs::apply_comfy_meta(&state, id, &path) {
            updated += 1;
        }
    }
    res.render(Json(serde_json::json!({
        "scanned": total,
        "updated": updated,
        "note": "只处理 PNG；没有 ComfyUI 文本块的图片不会变化"
    })));
    Ok(())
}

/// 编码器状态刷新：返回第三方服务地址、模型、维度与最近一次调用状态。
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

#[derive(Debug, Default, serde::Deserialize)]
pub struct PurgeRequest {
    /// 必须显式传 `"DELETE"`，避免误触
    pub confirm: Option<String>,
}

fn require_confirm(body: &PurgeRequest) -> ApiResult<()> {
    if body.confirm.as_deref() != Some("DELETE") {
        return Err(ApiError::bad_request(
            "清空操作需要在 body 里传 {\"confirm\": \"DELETE\"}",
        ));
    }
    Ok(())
}

fn ensure_no_active_jobs(state: &Arc<AppState>) -> ApiResult<()> {
    let active = state.db.active_job_count()?;
    if active > 0 {
        return Err(ApiError::conflict(format!(
            "还有 {active} 个运行中/排队中的作业，先取消或等待完成再清空"
        )));
    }
    Ok(())
}

/// POST /api/maintenance/purge/database —— 只清数据库（DROP 业务表并重建）
#[handler]
pub async fn purge_database(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let body: PurgeRequest = req.parse_json().await.unwrap_or_default();
    require_confirm(&body)?;
    ensure_no_active_jobs(&state)?;

    let (assets, _images, _videos, _nsfw, _bytes) = state.db.count_assets()?;
    state.db.purge_all()?;
    tracing::warn!(assets, "已清空数据库（向量与文件保留）");
    res.render(Json(serde_json::json!({
        "scope": "database",
        "assets_deleted": assets,
    })));
    Ok(())
}

/// POST /api/maintenance/purge/vectors —— 只重建 Qdrant 集合
#[handler]
pub async fn purge_vectors(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let body: PurgeRequest = req.parse_json().await.unwrap_or_default();
    require_confirm(&body)?;
    ensure_no_active_jobs(&state)?;

    state.vectors.recreate().await?;
    tracing::warn!(collection = %state.vectors.collection(), "已重建向量集合（数据库与文件保留）");
    res.render(Json(serde_json::json!({
        "scope": "vectors",
        "collection": state.vectors.collection(),
        "recreated": true,
    })));
    Ok(())
}

/// POST /api/maintenance/purge/media —— 只删媒体文件并清理空目录
#[handler]
pub async fn purge_media(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let body: PurgeRequest = req.parse_json().await.unwrap_or_default();
    require_confirm(&body)?;
    ensure_no_active_jobs(&state)?;

    let (files, bytes, dirs) = clear_dir(&state.cfg.media_dir);
    let (thumbs, _thumb_bytes, thumb_dirs) = clear_dir(&state.cfg.thumb_dir);
    tracing::warn!(files, thumbs, dirs, "已删除媒体与缩略图文件");
    res.render(Json(serde_json::json!({
        "scope": "media",
        "files_deleted": files + thumbs,
        "dirs_removed": dirs + thumb_dirs,
        "bytes_freed": bytes,
    })));
    Ok(())
}

/// POST /api/maintenance/purge —— 一键全清：数据库 + 向量 + 文件
#[handler]
pub async fn purge_all(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let body: PurgeRequest = req.parse_json().await.unwrap_or_default();
    require_confirm(&body)?;
    ensure_no_active_jobs(&state)?;

    let (assets, _images, _videos, _nsfw, _bytes) = state.db.count_assets()?;
    state.db.purge_all()?;
    state.vectors.recreate().await?;
    let (files, bytes, dirs) = clear_dir(&state.cfg.media_dir);
    let (thumbs, _thumb_bytes, thumb_dirs) = clear_dir(&state.cfg.thumb_dir);

    tracing::warn!(assets, files, thumbs, dirs, "已执行强制清空");
    res.render(Json(serde_json::json!({
        "scope": "all",
        "assets_deleted": assets,
        "files_deleted": files + thumbs,
        "dirs_removed": dirs + thumb_dirs,
        "bytes_freed": bytes,
        "collection_recreated": true,
    })));
    Ok(())
}

/// 递归删除目录下的文件并清理空目录（保留根目录），返回 (文件数, 总字节, 删除的目录数)。
fn clear_dir(root: &std::path::Path) -> (usize, u64, usize) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut dirs = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            files += 1;
            bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = std::fs::remove_file(entry.path());
        } else if entry.file_type().is_dir() {
            dirs.push(entry.path().to_path_buf());
        }
    }
    let mut removed = 0usize;
    for dir in dirs.into_iter().rev() {
        if std::fs::remove_dir(&dir).is_ok() {
            removed += 1;
        }
    }
    (files, bytes, removed)
}
