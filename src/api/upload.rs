//! 单图上传（需求 1）：`POST /api/assets/upload`，multipart 字段 `file`。
//!
//! 流程与文件夹导入保持同一套规则：
//! 落盘（multipart 临时文件）→ blake3 指纹 → 按 `<hash 前两位>/{image|video|other}/` 分桶
//! **移动**进资产库 → 建 asset（pending：没有向量，等前端补信息 / 编码作业处理）。
//!
//! 契约（与前端 `web/src/api/assets.ts` 对齐）：201 + `AssetDto`（含 `hash` / `hash_algo`）。

use salvo::prelude::*;

use super::{single_asset_dto, st, ApiResult};
use crate::error::ApiError;
use crate::jobs::{
    blake3_file, move_file, pick_target_rel, probe_kind, sanitize_file_name, TargetChoice,
};

/// 上传上限之外再留 1MB，给 multipart 的边界串和普通表单字段。
const MULTIPART_SLACK: u64 = 1024 * 1024;

#[handler]
pub async fn upload_asset(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let max_bytes = state.cfg.max_media_bytes;
    let limit = max_bytes.saturating_add(MULTIPART_SLACK) as usize;

    // 默认表单上限只有 64KB（salvo 的 secure_max_size），这里按媒体上限放开。
    let form = req.form_data_max_size(limit).await.map_err(|e| match e {
        salvo::http::ParseError::PayloadTooLarge => ApiError::payload_too_large(format!(
            "上传内容超过上限 {max_bytes} 字节（开发文档 §12 第 8 条：按字节和像素双重设限）"
        )),
        other => ApiError::bad_request(format!("multipart 解析失败: {other}")),
    })?;

    let part = form
        .files
        .get("file")
        .ok_or_else(|| ApiError::bad_request("缺少 multipart 字段 file"))?;
    if part.size() > max_bytes {
        return Err(ApiError::payload_too_large(format!(
            "文件 {} 字节超过上限 {max_bytes} 字节",
            part.size()
        )));
    }

    // 原始文件名只用来定 kind 与落盘名；真正的内容在 multipart 的临时文件里。
    let original = part.name().unwrap_or_default().to_string();
    let file_name = sanitize_file_name(&original);
    let temp_path = part.path().clone();
    let bytes = part.size() as i64;

    let hash = blake3_file(&temp_path)?;

    // 同一份内容已经入库（换个文件名再传一次）→ 直接返回既有资产，不重复占磁盘。
    if let Some(existing) = state.db.find_asset_by_hash(&hash)? {
        res.status_code(salvo::http::StatusCode::CREATED);
        res.render(Json(single_asset_dto(&state, &existing)?));
        return Ok(());
    }

    // 临时文件名保留了原扩展名（salvo 的 FilePart 按 `<nonce>.<ext>` 落盘），
    // 所以尺寸探测能直接对着临时文件做。
    let (kind, width, height) = probe_kind(&temp_path);

    let rel = match pick_target_rel(state.as_ref(), &hash, &file_name)? {
        TargetChoice::New(rel) => {
            move_file(&temp_path, &state.cfg.media_path(&rel))?;
            rel
        }
        // 目标路径上已有同 hash 的文件（库清过但文件还在）：只补登记，不动文件
        TargetChoice::Existing(rel) => rel,
    };

    let id = state
        .db
        .insert_asset(&kind, &rel, None, bytes, width, height, None, Some(&hash))?;
    let _ = state.db.set_asset_dimensions(id, width, height, None, Some(&hash));

    let row = state.db.asset_must_exist(id)?;
    res.status_code(salvo::http::StatusCode::CREATED);
    res.render(Json(single_asset_dto(&state, &row)?));
    Ok(())
}
