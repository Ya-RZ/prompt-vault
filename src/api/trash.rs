//! 回收站 API：列表、恢复、彻底删除、清空、文件预览。

use salvo::fs::NamedFile;
use salvo::prelude::*;
use serde::Serialize;

use super::{st, ApiResult};
use crate::db::DeletedAssetRow;
use crate::error::ApiError;

#[derive(Debug, Serialize)]
pub struct TrashPromptDto {
    pub role: String,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct TrashItemDto {
    pub id: i64,
    pub asset_id: i64,
    pub kind: String,
    pub rel_path: String,
    pub title: Option<String>,
    pub bytes: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub hash: Option<String>,
    pub nsfw: bool,
    pub prompts: Vec<TrashPromptDto>,
    pub added_at: String,
    pub deleted_at: String,
    /// 回收站文件预览地址
    pub file_url: String,
}

fn to_dto(row: &DeletedAssetRow) -> TrashItemDto {
    TrashItemDto {
        id: row.id,
        asset_id: row.asset_id,
        kind: row.kind.clone(),
        rel_path: row.rel_path.clone(),
        title: row.title.clone(),
        bytes: row.bytes,
        width: row.width,
        height: row.height,
        hash: row.hash.clone(),
        nsfw: row.nsfw,
        prompts: row
            .prompts()
            .into_iter()
            .map(|(role, text)| TrashPromptDto { role, text })
            .collect(),
        added_at: row.added_at.clone(),
        deleted_at: row.deleted_at.clone(),
        file_url: format!("/api/trash/{}/file", row.id),
    }
}

/// GET /api/trash?page=1&page_size=50
#[handler]
pub async fn list_trash(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let page = req.query::<i64>("page").unwrap_or(1).max(1);
    let page_size = req.query::<i64>("page_size").unwrap_or(50).clamp(1, 200);
    let (rows, total) = state.db.list_trash(page, page_size)?;
    let items: Vec<TrashItemDto> = rows.iter().map(to_dto).collect();
    res.render(Json(serde_json::json!({
        "items": items,
        "total": total,
        "page": page,
        "page_size": page_size,
        "trash_dir": state.cfg.data_dir.join("trash").display().to_string(),
    })));
    Ok(())
}

/// POST /api/trash/{id}/restore —— 文件移回 media，重建资产与提示词（待编码）
#[handler]
pub async fn restore_trash(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let id = req.param::<i64>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    let row = state
        .db
        .get_trash(id)?
        .ok_or_else(|| ApiError::not_found(format!("回收站记录 {id} 不存在")))?;

    let src = state.cfg.data_dir.join(&row.trash_path);
    let dst = state.cfg.media_path(&row.rel_path);
    if src.is_file() {
        crate::jobs::move_file(&src, &dst)?;
    }

    let new_id = state.db.restore_from_trash(id)?;
    let asset = state.db.asset_must_exist(new_id)?;
    res.render(Json(serde_json::json!({
        "restored": true,
        "trash_id": id,
        "asset_id": new_id,
        "asset": crate::api::single_asset_dto(&state, &asset)?,
        "note": "已恢复为未编码状态，可手动编码或跑编码作业"
    })));
    Ok(())
}

/// DELETE /api/trash/{id} —— 彻底删除（记录 + 文件）
#[handler]
pub async fn purge_trash(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let id = req.param::<i64>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    let path = state
        .db
        .delete_trash_row(id)?
        .ok_or_else(|| ApiError::not_found(format!("回收站记录 {id} 不存在")))?;
    let abs = state.cfg.data_dir.join(path);
    let file_deleted = abs.is_file() && std::fs::remove_file(&abs).is_ok();
    res.render(Json(serde_json::json!({
        "deleted": true,
        "id": id,
        "file_deleted": file_deleted
    })));
    Ok(())
}

/// DELETE /api/trash —— 清空回收站
#[handler]
pub async fn clear_trash(_req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let paths = state.db.clear_trash()?;
    let mut files = 0usize;
    for path in &paths {
        let abs = state.cfg.data_dir.join(path);
        if abs.is_file() && std::fs::remove_file(&abs).is_ok() {
            files += 1;
        }
    }
    res.render(Json(serde_json::json!({
        "cleared": paths.len(),
        "files_deleted": files
    })));
    Ok(())
}

/// GET /api/trash/{id}/file —— 预览回收站文件
#[handler]
pub async fn trash_file(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let id = req.param::<i64>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    let row = state
        .db
        .get_trash(id)?
        .ok_or_else(|| ApiError::not_found(format!("回收站记录 {id} 不存在")))?;
    let path = state.cfg.data_dir.join(&row.trash_path);
    if !path.is_file() {
        return Err(ApiError::not_found("回收站文件已不存在"));
    }
    match NamedFile::open(&path).await {
        Ok(file) => {
            file.send(req.headers(), res).await;
            Ok(())
        }
        Err(e) => Err(ApiError::internal(format!("读取回收站文件失败: {e}"))),
    }
}
