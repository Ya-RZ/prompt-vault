//! 资产相关 handler（INTERFACES.md §4）。

use salvo::prelude::*;

use super::{load_asset_dtos, page_of, paginated, single_asset_dto, st};
use crate::error::ApiError;
use crate::search::{Engine, SearchQuery};
use crate::types::{
    AssetListQuery, CreatePromptRequest, DeletedResponse, UpdateMetaRequest,
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
    let keep_file = req.query::<bool>("keep_file").unwrap_or(false);
    let row = state.db.asset_must_exist(id)?;
    let prompt_ids: Vec<i64> = state.db.prompts_for(id)?.iter().map(|p| p.id).collect();

    // 向量两路一起删（开发文档 §13 第 7 条），删完必须 optimize（§6.3 没有后台 optimizer）
    let vector_deleted = state.vectors.delete_asset_and_prompts(id, &prompt_ids).is_ok();
    let _ = state.vectors.optimize();
    let _ = state.vectors.flush();

    let deleted = state.db.delete_asset(id)?;
    let mut file_deleted = false;
    if !keep_file {
        let path = state.cfg.media_path(&row.rel_path);
        if path.is_file() && std::fs::remove_file(&path).is_ok() {
            file_deleted = true;
        }
    }
    res.render(Json(DeletedResponse { deleted, id, vector_deleted, file_deleted }));
    Ok(())
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
                state.vectors.upsert_prompt(
                    prompt_id,
                    id,
                    &outcome.dense,
                    sparse,
                    &outcome.fingerprint,
                    nsfw,
                    &tags,
                )?;
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
    let _ = state.vectors.set_asset_payload(id, nsfw, &tags);
    for prompt in state.db.prompts_for(id)? {
        let _ = state.vectors.set_prompt_payload(prompt.id, id, nsfw, &tags);
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
    let row = state.db.asset_must_exist(id)?;
    let engine = Engine::new(&state);
    let resp = engine
        .search_image(None, Some(id), k, Vec::new(), Some(false), format!("<asset_id:{}>", row.id))
        .await?;
    res.render(Json(resp));
    Ok(())
}

#[handler]
pub async fn similar_prompts(req: &mut Request, depot: &mut Depot, res: &mut Response) -> Result<(), ApiError> {
    let state = st(depot)?.clone();
    let id = req.param::<i64>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    let k = req.query::<usize>("k").unwrap_or(20).clamp(1, 100);
    let engine = Engine::new(&state);
    let items = engine.similar_prompts(id, k)?;
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
