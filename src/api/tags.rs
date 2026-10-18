//! 标签管理 handler（需求 8）：列表（带引用数）、删除单个标签、清理未引用标签。
//!
//! 路由由 `src/api/mod.rs` 统一注册（函数名固定）：
//! `tags::list_tags` / `tags::delete_tag` / `tags::cleanup_tags`。

use salvo::prelude::*;
use serde::Serialize;

use super::{st, ApiResult};
use crate::db::assets::TagDeleteOutcome;
use crate::error::ApiError;

/// `GET /api/tags` 的条目。
#[derive(Debug, Clone, Serialize)]
pub struct TagDto {
    pub id: i64,
    pub name: String,
    /// 被多少个资产引用
    pub asset_count: i64,
}

/// `GET /api/tags` 的响应体（契约：`{items, total}`）。
#[derive(Debug, Clone, Serialize)]
pub struct TagListResponse {
    pub items: Vec<TagDto>,
    pub total: i64,
}

/// 列出所有标签及使用次数（按 asset_count 降序、name 升序）。
#[handler]
pub async fn list_tags(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let page = req.query::<i64>("page").unwrap_or(1).max(1);
    // 标签页要一次看全，所以上限比其它列表接口宽（500）
    let page_size = req.query::<i64>("page_size").unwrap_or(100).clamp(1, 500);
    let q = req.query::<String>("q");

    let (rows, total) =
        state.db.list_tags_with_counts(page_size, (page - 1) * page_size, q.as_deref())?;
    let items = rows
        .into_iter()
        .map(|r| TagDto { id: r.id, name: r.name, asset_count: r.asset_count })
        .collect();
    res.render(Json(TagListResponse { items, total }));
    Ok(())
}

/// 删除一个标签：仍被资产引用时 409 并返回引用数，标签不存在时 404。
#[handler]
pub async fn delete_tag(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let id = req.param::<i64>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    match state.db.delete_tag_if_unused(id)? {
        TagDeleteOutcome::Deleted => {
            res.render(Json(serde_json::json!({ "deleted": true })));
            Ok(())
        }
        TagDeleteOutcome::NotFound => Err(ApiError::not_found(format!("标签 {id} 不存在"))),
        TagDeleteOutcome::InUse(n) => Err(ApiError::conflict(format!(
            "标签 {id} 仍被 {n} 个资产引用，无法删除；先解除引用，或用清理接口只删未被引用的标签"
        ))
        .with_detail(serde_json::json!({ "asset_count": n }))),
    }
}

/// 清理所有未被引用的标签，返回删除数量。
#[handler]
pub async fn cleanup_tags(depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let deleted = state.db.delete_unused_tags()?;
    res.render(Json(serde_json::json!({ "deleted": deleted })));
    Ok(())
}
