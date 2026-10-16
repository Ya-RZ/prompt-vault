//! 检索相关 handler（INTERFACES.md §5）。

use salvo::prelude::*;

use super::st;
use crate::error::ApiError;
use crate::search::{Engine, SearchQuery};
use crate::types::EmbedInputItem;

fn parse_nsfw(raw: Option<&str>) -> Option<bool> {
    match raw {
        Some("true") => Some(true),
        Some("false") => Some(false),
        // all / 空 → 不过滤
        _ => None,
    }
}

/// 默认过滤掉 nsfw（契约 §5）：显式 nsfw=all 才放开。
fn nsfw_default(raw: Option<String>) -> Option<bool> {
    match raw.as_deref() {
        Some("all") | Some("") => None,
        Some("true") => Some(true),
        _ => Some(false),
    }
}

#[handler]
pub async fn search_text(req: &mut Request, depot: &mut Depot, res: &mut Response) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let q = req
        .query::<String>("q")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("缺少查询参数 q"))?;
    let k = req.query::<usize>("k").unwrap_or(20).clamp(1, 200);
    let query = SearchQuery {
        q,
        k,
        mode: req.query::<String>("mode").unwrap_or_else(|| "hybrid".into()),
        tags: req.queries().get_vec("tag").cloned().unwrap_or_default(),
        nsfw: nsfw_default(req.query::<String>("nsfw")),
        kind: req.query::<String>("kind"),
        min_score: req.query::<f64>("min_score"),
    };
    let engine = Engine::new(&state);
    let resp = engine.search_text(query).await?;
    res.render(Json(resp));
    Ok(())
}

#[handler]
pub async fn search_image(req: &mut Request, depot: &mut Depot, res: &mut Response) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let k = req.query::<usize>("k").unwrap_or(20).clamp(1, 200);
    let tags: Vec<String> = req.queries().get_vec("tag").cloned().unwrap_or_default();
    let nsfw = nsfw_default(req.query::<String>("nsfw"));

    let mut data_url: Option<String> = None;
    let mut asset_id: Option<i64> = None;
    let mut label = String::from("<inline>");

    let content_type = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_type.starts_with("multipart/form-data") {
        let file = req
            .file("file")
            .await
            .ok_or_else(|| ApiError::bad_request("multipart 里缺少 file 字段"))?;
        let name = file.name().unwrap_or("upload").to_string();
        let mime = file
            .content_type()
            .map(|m| m.to_string())
            .unwrap_or_else(|| crate::encode::mime_for_path(&name).to_string());
        let bytes = std::fs::read(file.path())
            .map_err(|e| ApiError::bad_request(format!("读取上传文件失败: {e}")))?;
        label = format!("<file:{}>", file.name().unwrap_or("upload"));
        data_url = Some(crate::encode::image_data_url(&bytes, &mime));
    } else if content_type.starts_with("application/json") {
        let body = req
            .parse_json::<serde_json::Value>()
            .await
            .map_err(|e| ApiError::bad_request(format!("请求体解析失败: {e}")))?;
        if let Some(id) = body.get("asset_id").and_then(|v| v.as_i64()) {
            asset_id = Some(id);
            label = format!("<asset_id:{id}>");
        }
        if let Some(b64) = body.get("image_base64").and_then(|v| v.as_str()) {
            let mime = body.get("mime").and_then(|v| v.as_str()).unwrap_or("image/jpeg");
            let cleaned = b64.split(',').last().unwrap_or(b64).to_string();
            data_url = Some(format!("data:{mime};base64,{cleaned}"));
            label = "<base64>".to_string();
        }
    } else {
        // 兼容 form 表单里的 image_base64
        let b64 = req.query::<String>("image_base64");
        if let Some(b64) = b64 {
            let cleaned = b64.split(',').last().unwrap_or(&b64).to_string();
            data_url = Some(format!("data:image/jpeg;base64,{cleaned}"));
            label = "<base64>".to_string();
        }
        if let Some(id) = req.query::<i64>("asset_id") {
            asset_id = Some(id);
            label = format!("<asset_id:{id}>");
        }
    }

    if data_url.is_none() && asset_id.is_none() {
        return Err(ApiError::bad_request("必须提供 file / image_base64 / asset_id 之一"));
    }

    let engine = Engine::new(&state);
    let resp = engine.search_image(data_url, asset_id, k, tags, nsfw, label).await?;
    res.render(Json(resp));
    Ok(())
}

#[handler]
pub async fn search_image_by_asset(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let asset_id = req
        .query::<i64>("asset_id")
        .ok_or_else(|| ApiError::bad_request("缺少查询参数 asset_id"))?;
    let k = req.query::<usize>("k").unwrap_or(20).clamp(1, 200);
    let tags: Vec<String> = req.queries().get_vec("tag").cloned().unwrap_or_default();
    let nsfw = nsfw_default(req.query::<String>("nsfw"));
    state.db.asset_must_exist(asset_id)?;
    let engine = Engine::new(&state);
    let resp = engine
        .search_image(None, Some(asset_id), k, tags, nsfw, format!("<asset_id:{asset_id}>"))
        .await?;
    res.render(Json(resp));
    Ok(())
}

#[handler]
pub async fn lexical(req: &mut Request, depot: &mut Depot, res: &mut Response) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let q = req
        .query::<String>("q")
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request("缺少查询参数 q"))?;
    let k = req.query::<usize>("k").unwrap_or(50).clamp(1, 200);
    let engine = Engine::new(&state);
    let items = engine.lexical(&q, k)?;
    res.render(Json(serde_json::json!({ "items": items, "total": items.len() })));
    Ok(())
}

/// 稀疏/稠密单路诊断口：稀疏没启用时显式 501，不假装成功。
#[handler]
pub async fn similar_by_kind(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let kind = req.param::<String>("vector_kind").unwrap_or_default();
    let q = req
        .query::<String>("q")
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request("缺少查询参数 q"))?;
    let k = req.query::<usize>("k").unwrap_or(20).clamp(1, 100);

    match kind.as_str() {
        "dense" => {
            let engine = Engine::new(&state);
            let resp = engine
                .search_text(SearchQuery {
                    q,
                    k,
                    mode: "dense".into(),
                    tags: Vec::new(),
                    nsfw: Some(false),
                    kind: None,
                    min_score: None,
                })
                .await?;
            res.render(Json(resp));
            Ok(())
        }
        "sparse" => {
            if !state.vectors.sparse_enabled() {
                return Err(ApiError::sparse_unavailable(
                    "本索引未启用稀疏向量；侧车也尚未返回 sparse（开发文档 §5.4 改动 2 未实施）",
                ));
            }
            let outcome = state
                .encoder
                .embed(vec![EmbedInputItem { text: Some(q.clone()), image: None }])
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| ApiError::encoder_error("侧车没有返回向量"))?;
            let Some((indices, values)) = outcome.sparse.clone() else {
                return Err(ApiError::sparse_unavailable(
                    "侧车响应里没有 sparse 字段（开发文档 §5.4 改动 2 未实施）",
                ));
            };
            let hits = state.vectors.search_sparse(
                crate::vector::VEC_SPARSE_TEXT,
                &indices,
                &values,
                k,
                None,
            )?;
            res.render(Json(serde_json::json!({
                "query": q,
                "mode": "sparse",
                "items": hits.iter().map(|(id, score, payload)| serde_json::json!({
                    "point_id": id,
                    "score": score,
                    "payload": payload,
                })).collect::<Vec<_>>(),
                "total": hits.len(),
            })));
            Ok(())
        }
        _ => Err(ApiError::bad_request("vector_kind 只能是 dense 或 sparse")),
    }
}

// 保留：可选的 nsfw 解析入口（前端传 all 时上层用它）
#[allow(dead_code)]
fn parse_nsfw_pub(raw: Option<&str>) -> Option<bool> {
    parse_nsfw(raw)
}
