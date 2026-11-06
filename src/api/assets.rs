//! 资产相关 handler（INTERFACES.md §4）。

use salvo::prelude::*;
use serde::Deserialize;

use super::{load_asset_dtos, page_of, paginated, single_asset_dto, st};
use crate::error::ApiError;
use crate::search::{Engine, SearchQuery};
use crate::types::{
    AssetListQuery, CreatePromptRequest, DeletedResponse, EmbedInputItem, UpdateMetaRequest,
};

#[handler]
pub async fn list_assets(req: &mut Request, depot: &mut Depot, res: &mut Response) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let (page, page_size) = page_of(req);
    let query = AssetListQuery {
        page: Some(page),
        page_size: Some(page_size),
        q: req.query::<String>("q"),
        tag: req.queries().get_vec("tag").cloned().unwrap_or_default(),
        kind: req.query::<String>("kind"),
        nsfw: req.query::<String>("nsfw"),
        encoded: req.query::<String>("encoded"),
        sort: req.query::<String>("sort"),
    };
    let (rows, total) = state.db.list_assets(&query)?;
    let items = load_asset_dtos(&state, &rows)?;
    res.render(Json(paginated(items, total, page, page_size)));
    Ok(())
}

#[handler]
pub async fn get_asset(req: &mut Request, depot: &mut Depot, res: &mut Response) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let id = req.param::<i64>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    let row = state.db.asset_must_exist(id)?;
    res.render(Json(single_asset_dto(&state, &row)?));
    Ok(())
}

#[handler]
pub async fn delete_asset(req: &mut Request, depot: &mut Depot, res: &mut Response) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let id = req.param::<i64>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    let row = state.db.asset_must_exist(id)?;
    let prompt_ids: Vec<i64> = state.db.prompts_for(id)?.iter().map(|p| p.id).collect();

    // 1) 文件移到回收站（保留可恢复的能力）
    let trash_rel = trash_rel_path(&row);
    let src = state.cfg.media_path(&row.rel_path);
    let dst = state.cfg.data_dir.join(&trash_rel);
    let file_moved = if src.is_file() {
        crate::jobs::move_file(&src, &dst).is_ok()
    } else {
        false
    };

    // 2) 向量两路一起删（开发文档 §13 第 7 条）
    let vector_deleted = state
        .vectors
        .delete_asset_and_prompts(id, &prompt_ids)
        .await
        .is_ok();
    let _ = state.vectors.optimize();
    let _ = state.vectors.flush();

    // 3) 数据库整行迁移到回收站（含提示词）
    let trash_id = state.db.move_asset_to_trash(id, &trash_rel)?;

    res.render(Json(DeletedResponse {
        deleted: true,
        id,
        vector_deleted,
        file_deleted: file_moved,
        trashed: true,
        trash_id: Some(trash_id),
    }));
    Ok(())
}

/// 回收站里的相对路径（相对 data 目录）：`trash/<hash 前两位>/<hash16><后缀>`。
fn trash_rel_path(row: &crate::db::AssetRow) -> String {
    let hash = row.hash.as_deref().unwrap_or("");
    let ext = std::path::Path::new(&row.rel_path)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let name = if hash.len() >= 16 {
        format!("{}{}", &hash[..16], ext)
    } else {
        format!("{}{}", row.id, ext)
    };
    let bucket = hash.get(..2).unwrap_or("misc");
    format!("trash/{bucket}/{name}")
}

#[handler]
pub async fn add_prompt(req: &mut Request, depot: &mut Depot, res: &mut Response) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let id = req.param::<i64>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    let body = req
        .parse_json::<CreatePromptRequest>()
        .await
        .map_err(|e| ApiError::bad_request(format!("请求体解析失败: {e}")))?;
    if body.text.trim().is_empty() {
        return Err(ApiError::bad_request("提示词文本不能为空"));
    }
    state.db.asset_must_exist(id)?;
    let role = body.role.unwrap_or_else(|| "caption".into());
    let prompt_id = state.db.insert_prompt(id, &role, body.text.trim())?;

    let want_encode = body.encode.unwrap_or(true);
    if want_encode {
        let item = crate::types::EmbedInputItem { text: Some(body.text.trim().to_string()), image: None };
        match state.encoder.embed(vec![item]).await {
            Ok(mut outcomes) if !outcomes.is_empty() => {
                let outcome = outcomes.remove(0);
                state.ensure_index_fingerprint(&outcome.fingerprint)?;
                let tags = state.db.tags_for(id).unwrap_or_default();
                let nsfw = state.db.get_asset(id)?.map(|a| a.nsfw).unwrap_or(false);
                let sparse = outcome
                    .sparse
                    .as_ref()
                    .map(|(idx, val)| (idx.as_slice(), val.as_slice()));
                state
                    .vectors
                    .upsert_prompt(
                        prompt_id,
                        id,
                        &outcome.dense,
                        sparse,
                        &outcome.fingerprint,
                        nsfw,
                        &tags,
                    )
                    .await?;
                state.db.upsert_prompt_vector(prompt_id, &outcome.fingerprint, outcome.dense.len() as i64)?;
                let _ = state.vectors.flush();
            }
            Ok(_) => {}
            Err(e) => {
                // 提示词已经落库（SQLite 是兜底），把 id 带回去让前端知道不用重填
                return Err(e.with_detail(serde_json::json!({
                    "prompt_id": prompt_id,
                    "note": "提示词已写入 SQLite，但向量未生成；可用编码作业补编码"
                })));
            }
        }
    }

    let row = state
        .db
        .get_prompt(prompt_id)?
        .ok_or_else(|| ApiError::internal("提示词写入后读取失败"))?;
    let encoded = state.db.encoded_prompt_ids(&[prompt_id])?.contains(&prompt_id);
    res.status_code(salvo::http::StatusCode::CREATED);
    res.render(Json(super::dto::prompt_to_dto(row, encoded)));
    Ok(())
}

#[handler]
pub async fn update_meta(req: &mut Request, depot: &mut Depot, res: &mut Response) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let id = req.param::<i64>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    let body = req
        .parse_json::<UpdateMetaRequest>()
        .await
        .map_err(|e| ApiError::bad_request(format!("请求体解析失败: {e}")))?;
    state.db.asset_must_exist(id)?;

    state.db.update_asset_meta(
        id,
        body.nsfw,
        body.rel_path.as_deref(),
        body.title.as_deref(),
        body.width,
        body.height,
        body.duration_ms,
    )?;
    if let Some(tags) = body.tags.as_ref() {
        state.db.set_asset_tags(id, tags)?;
    }

    // payload 里的 tags/nsfw 是过滤用的，必须同步，否则标签筛选会失效
    let tags = state.db.tags_for(id)?;
    let nsfw = state.db.get_asset(id)?.map(|a| a.nsfw).unwrap_or(false);
    let _ = state.vectors.set_asset_payload(id, nsfw, &tags).await;
    for prompt in state.db.prompts_for(id)? {
        let _ = state
            .vectors
            .set_prompt_payload(prompt.id, id, nsfw, &tags)
            .await;
    }
    let _ = state.vectors.flush();

    let row = state.db.asset_must_exist(id)?;
    res.render(Json(single_asset_dto(&state, &row)?));
    Ok(())
}

#[handler]
pub async fn similar_assets(req: &mut Request, depot: &mut Depot, res: &mut Response) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let id = req.param::<i64>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    let k = req.query::<usize>("k").unwrap_or(20).clamp(1, 200);

    // by=prompt / by=image：可重复（by=prompt&by=image）或逗号分隔；缺省保持旧行为（image）。
    let raw = req.queries().get_vec("by").cloned().unwrap_or_default();
    let mut by: Vec<String> = raw
        .iter()
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if by.is_empty() {
        by.push("image".to_string());
    }
    let by_prompt = by.iter().any(|s| s == "prompt");
    let by_image = by.iter().any(|s| s == "image");
    if !by_prompt && !by_image {
        return Err(ApiError::bad_request("by 只支持 prompt / image（可多选）"));
    }

    let engine = Engine::new(&state);

    // 只用提示词：不碰图片向量。
    if by_prompt && !by_image {
        let mut resp = engine.similar_by_prompts(id, k).await?;
        resp.items.retain(|item| item.asset.id != id);
        resp.total = resp.items.len();
        for item in &mut resp.items {
            item.sources = vec!["prompt".to_string()];
        }
        res.render(Json(resp));
        return Ok(());
    }

    let row = state.db.asset_must_exist(id)?;
    let mut resp = engine
        .search_image(None, Some(id), k, Vec::new(), Some(false), format!("<asset_id:{}>", row.id))
        .await?;
    // 排除自身，并把来源标成 image（合并时提示词路标 prompt）
    for item in &mut resp.items {
        item.sources = vec!["image".to_string()];
    }
    resp.items.retain(|item| item.asset.id != id);
    resp.total = resp.items.len();

    // 混合：图片结果 + 提示词结果，按最高分合并、去重。
    if by_prompt {
        match engine.similar_by_prompts(id, k).await {
            Ok(mut prompt_resp) => {
                for item in &mut prompt_resp.items {
                    item.sources = vec!["prompt".to_string()];
                }
                prompt_resp.items.retain(|item| item.asset.id != id);

                let mut merged: std::collections::HashMap<i64, crate::types::SearchItem> =
                    std::collections::HashMap::new();
                for item in resp.items.drain(..).chain(prompt_resp.items.into_iter()) {
                    let aid = item.asset.id;
                    match merged.get_mut(&aid) {
                        Some(existing) => {
                            if item.score > existing.score {
                                existing.score = item.score;
                                existing.rank = item.rank;
                            }
                            for source in item.sources {
                                if !existing.sources.contains(&source) {
                                    existing.sources.push(source);
                                }
                            }
                        }
                        None => {
                            merged.insert(aid, item);
                        }
                    }
                }
                let mut items: Vec<_> = merged.into_values().collect();
                items.sort_by(|a, b| {
                    b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)
                });
                items.truncate(k);
                resp.items = items;
                resp.total = resp.items.len();
                resp.effective_mode = "image+prompt".to_string();
                resp.warnings.push("已合并图片与提示词两路，按最高分排序".to_string());
            }
            Err(e) => resp
                .warnings
                .push(format!("提示词路径失败，仅返回图片结果: {e}")),
        }
    }

    res.render(Json(resp));
    Ok(())
}

#[handler]
pub async fn similar_prompts(req: &mut Request, depot: &mut Depot, res: &mut Response) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let id = req.param::<i64>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    let k = req.query::<usize>("k").unwrap_or(20).clamp(1, 100);
    let engine = Engine::new(&state);
    let items = engine.similar_prompts(id, k).await?;
    res.render(Json(serde_json::json!({ "items": items, "total": items.len() })));
    Ok(())
}

/// 关键词 → 关键词+图片 的便捷入口（`GET /api/assets/{id}/similar` 的文本版）。
pub async fn _text_search_delegate(
    state: &std::sync::Arc<crate::state::AppState>,
    q: String,
    k: usize,
) -> Result<crate::types::SearchResponse, ApiError> {
    let engine = Engine::new(state);
    engine
        .search_text(SearchQuery {
            q,
            k,
            mode: "hybrid".into(),
            tags: Vec::new(),
            nsfw: Some(false),
            kind: None,
            min_score: None,
        })
        .await
}

#[derive(Debug, Default, Deserialize)]
pub struct EncodeAssetRequest {
    /// `image`（默认）| `positive`
    #[serde(default)]
    pub mode: Option<String>,
}

/// 单张资产的手动编码：`POST /api/assets/{id}/encode`，body `{"mode":"image"|"positive"}`。
#[handler]
pub async fn encode_asset(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let id = req
        .param::<i64>("id")
        .ok_or_else(|| ApiError::bad_request("id 非法"))?;
    let body: EncodeAssetRequest = req.parse_json().await.unwrap_or_default();
    let mode = body
        .mode
        .as_deref()
        .unwrap_or("image")
        .trim()
        .to_ascii_lowercase();
    let asset = state.db.asset_must_exist(id)?;

    match mode.as_str() {
        "image" => {
            let data = crate::jobs::read_image_data_url(&state, &asset.rel_path)?;
            let outcomes = state
                .encoder
                .embed(vec![EmbedInputItem {
                    text: None,
                    image: Some(data),
                }])
                .await?;
            let outcome = outcomes
                .into_iter()
                .next()
                .ok_or_else(|| ApiError::encoder_error("编码器没有返回向量"))?;
            crate::jobs::store_asset_vector(&state, id, &outcome).await?;
            res.render(Json(serde_json::json!({
                "asset_id": id,
                "mode": "image",
                "encoded": true,
                "fingerprint": outcome.fingerprint,
                "dim": outcome.dense.len(),
            })));
        }
        "positive" => {
            // 优先用导入时写入的 role=positive；老数据退回到 asset.positive_prompt 现场建一条。
            let prompts = state.db.prompts_for(id)?;
            let prompt_id = match prompts.iter().find(|p| p.role == "positive") {
                Some(p) => p.id,
                None => {
                    let text = asset.positive_prompt.clone().ok_or_else(|| {
                        ApiError::bad_request("该资产没有正面提示词（不是 ComfyUI PNG，或没解析到）")
                    })?;
                    state.db.insert_prompt(id, "positive", &text)?
                }
            };
            let text = state
                .db
                .get_prompt(prompt_id)?
                .map(|p| p.text)
                .ok_or_else(|| ApiError::not_found("提示词不存在"))?;
            let outcomes = state
                .encoder
                .embed(vec![EmbedInputItem {
                    text: Some(text),
                    image: None,
                }])
                .await?;
            let outcome = outcomes
                .into_iter()
                .next()
                .ok_or_else(|| ApiError::encoder_error("编码器没有返回向量"))?;
            crate::jobs::store_prompt_vector(&state, prompt_id, id, &outcome).await?;
            res.render(Json(serde_json::json!({
                "asset_id": id,
                "prompt_id": prompt_id,
                "mode": "positive",
                "encoded": true,
                "fingerprint": outcome.fingerprint,
                "dim": outcome.dense.len(),
            })));
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "mode 只支持 image / positive，收到 {other}"
            )))
        }
    }
    Ok(())
}
