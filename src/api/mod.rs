//! HTTP 层：路由装配 + 各资源 handler。
//!
//! ⚠️ `/media/{**path}` 必须挂在**根路由**，不能 push 进 `/api`（开发文档 §7.1：
//! 上一版踩过，缩略图和 `<video src>` 全 404）。

pub mod assets;
pub mod dto;
pub mod jobs;
pub mod maintenance;
pub mod media;
pub mod search;
pub mod stats;
pub mod tags;
pub mod upload;

use std::sync::Arc;

use salvo::prelude::*;

use crate::error::ApiError;
use crate::state::AppState;
use crate::types::{AssetDto, Paginated};

pub fn state_of(depot: &Depot) -> ApiResult<Arc<AppState>> {
    depot
        .get_typed::<Arc<AppState>>()
        .cloned()
        .map_err(|_| ApiError::internal("服务状态未注入（内部错误）"))
}

pub type ApiResult<T> = Result<T, ApiError>;

/// 从 Depot 里取状态（`obtain` 的引用版）。
pub fn st(depot: &Depot) -> ApiResult<&Arc<AppState>> {
    depot
        .get_typed::<Arc<AppState>>()
        .map_err(|_| ApiError::internal("服务状态未注入（内部错误）"))
}

pub fn page_of(req: &Request) -> (i64, i64) {
    let page = req.query::<i64>("page").unwrap_or(1).max(1);
    let page_size = req.query::<i64>("page_size").unwrap_or(50).clamp(1, 200);
    (page, page_size)
}

/// 批量把资产行组装成 DTO（列表页避免 N+1）。
pub fn load_asset_dtos(state: &AppState, rows: &[crate::db::AssetRow]) -> ApiResult<Vec<AssetDto>> {
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let prompts = state.db.prompts_for_many(&ids)?;
    let tags = state.db.tags_for_many(&ids)?;
    let vectors = state.db.vectors_for_many(&ids)?;
    let mut all_prompt_ids: Vec<i64> = Vec::new();
    for list in prompts.values() {
        all_prompt_ids.extend(list.iter().map(|p| p.id));
    }
    let encoded = state.db.encoded_prompt_ids(&all_prompt_ids)?;

    Ok(rows
        .iter()
        .map(|row| {
            let row_prompts = prompts.get(&row.id).cloned().unwrap_or_default();
            dto::asset_to_dto(
                row,
                row_prompts,
                &encoded,
                tags.get(&row.id).cloned().unwrap_or_default(),
                vectors.get(&row.id).cloned(),
            )
        })
        .collect())
}

pub fn single_asset_dto(state: &AppState, row: &crate::db::AssetRow) -> ApiResult<AssetDto> {
    let prompts = state.db.prompts_for(row.id)?;
    let encoded = state.db.encoded_prompt_ids(&prompts.iter().map(|p| p.id).collect::<Vec<_>>())?;
    let tags = state.db.tags_for(row.id)?;
    let vector = state.db.asset_vector_state(row.id)?;
    Ok(dto::asset_to_dto(row, prompts, &encoded, tags, vector))
}

pub fn paginated<T>(items: Vec<T>, total: i64, page: i64, page_size: i64) -> Paginated<T> {
    Paginated::new(items, total, page, page_size)
}

/// 组装完整路由。
pub fn build_router() -> Router {
    Router::new()
        // ---- 媒体（根路由，不在 /api 下）----
        .push(Router::with_path("media/{**path}").get(media::serve_media))
        // ---- API ----
        .push(
            Router::with_path("api")
                .push(Router::with_path("health").get(health))
                .push(
                    Router::with_path("stats")
                        .get(stats::stats)
                        .push(Router::with_path("facets").get(stats::facets))
                        .push(Router::with_path("size").get(stats::size)),
                )
                .push(
                    Router::with_path("assets")
                        .get(assets::list_assets)
                        // 需求 1：单图上传。必须排在 `{id}` 前面，Salvo 按注册顺序匹配
                        .push(Router::with_path("upload").post(upload::upload_asset))
                        .push(
                            Router::with_path("{id}")
                                .get(assets::get_asset)
                                .delete(assets::delete_asset)
                                .push(Router::with_path("prompts").post(assets::add_prompt))
                                .push(Router::with_path("meta").post(assets::update_meta))
                                .push(Router::with_path("similar").get(assets::similar_assets)),
                        ),
                )
                .push(
                    Router::with_path("prompts/{id}/similar").get(assets::similar_prompts),
                )
                .push(
                    Router::with_path("search")
                        .push(Router::with_path("text").get(search::search_text))
                        .push(
                            Router::with_path("image")
                                .get(search::search_image_by_asset)
                                .post(search::search_image),
                        )
                        .push(Router::with_path("lexical").get(search::lexical))
                        .push(Router::with_path("similar/{vector_kind}").get(search::similar_by_kind)),
                )
                .push(
                    Router::with_path("jobs")
                        .get(jobs::list_jobs)
                        .push(Router::with_path("import").post(jobs::create_import))
                        .push(Router::with_path("encode").post(jobs::create_encode))
                        .push(
                            Router::with_path("{id}")
                                .get(jobs::get_job)
                                .delete(jobs::delete_job)
                                .push(Router::with_path("errors").get(jobs::job_errors))
                                .push(Router::with_path("pause").post(jobs::pause_job))
                                .push(Router::with_path("resume").post(jobs::resume_job))
                                .push(Router::with_path("cancel").post(jobs::cancel_job)),
                        ),
                )
                .push(
                    // 需求 8：标签管理。`cleanup` 是字面量路由，注册在 `{id}` 之前更稳妥
                    Router::with_path("tags")
                        .get(tags::list_tags)
                        .push(Router::with_path("cleanup").post(tags::cleanup_tags))
                        .push(Router::with_path("{id}").delete(tags::delete_tag)),
                )
                .push(
                    Router::with_path("maintenance")
                        .push(Router::with_path("vector/info").get(maintenance::vector_info))
                        .push(Router::with_path("vector/optimize").post(maintenance::vector_optimize))
                        .push(Router::with_path("vector/export").post(maintenance::vector_export))
                        .push(Router::with_path("vector/import").post(maintenance::vector_import))
                        .push(Router::with_path("golden/verify").post(maintenance::golden_verify))
                        .push(Router::with_path("encoder/refresh").post(maintenance::encoder_refresh)),
                ),
        )
}

// ---------------------------------------------------------------- health

#[handler]
pub async fn health(depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = state_of(depot)?;
    // 编码在进程内完成，health 只读当前状态（不再有 HTTP 侧车可探）。
    let encoder = crate::encode::health_snapshot(&state.encoder);
    let golden = state.golden_snapshot();
    let index_fp = state.index_fingerprint()?;
    let points = state.vectors.points_estimate();
    let (assets_total, _, _, _, _) = state.db.count_assets()?;
    let (prompts_total, _prompts_encoded) = state.db.count_prompts()?;
    let pending = state.db.pending_encode_count()?;

    let degraded = !encoder.reachable
        || golden.status == "failed"
        || matches!((&index_fp, &encoder.fingerprint), (Some(i), Some(c)) if i != c);

    let body = crate::types::HealthResponse {
        status: if degraded { "DEGRADED".into() } else { "READY".into() },
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: state.uptime_seconds(),
        encoder,
        golden,
        index: crate::types::IndexInfo {
            fingerprint: index_fp,
            points,
            assets: assets_total,
            prompts: prompts_total,
            dense_size: state.cfg.dim as i64,
            sparse_enabled: state.vectors.sparse_enabled(),
            quantization: state.vectors.quantization().as_str().to_string(),
            dir: state.cfg.vector_dir.display().to_string(),
        },
        db: crate::types::DbInfo {
            path: state.cfg.db_path.display().to_string(),
            assets: assets_total,
            pending_encode: pending,
        },
    };
    res.render(Json(body));
    Ok(())
}
