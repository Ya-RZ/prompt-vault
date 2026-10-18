//! 统计 handler（INTERFACES.md §3）：图表可用 + facet 分布（开发文档 §7.2）。

use salvo::prelude::*;

use super::{st, ApiResult};
use crate::error::ApiError;
use crate::types::{
    AssetsCounts, CountItem, EncodeProgress, FacetItem, FacetsResponse, PromptsCounts,
    SizeResponse, StatsResponse, VectorsCounts,
};

#[handler]
pub async fn stats(depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let (total, images, videos, nsfw, bytes) = state.db.count_assets()?;
    let (prompts_total, prompts_encoded) = state.db.count_prompts()?;
    let pending = state.db.pending_encode_count()?;
    let percent = if prompts_total > 0 {
        (prompts_encoded as f64 / prompts_total as f64 * 1000.0).round() / 10.0
    } else {
        0.0
    };

    let body = StatsResponse {
        assets: AssetsCounts { total, images, videos, nsfw, bytes },
        prompts: PromptsCounts { total: prompts_total, encoded: prompts_encoded, pending },
        vectors: VectorsCounts {
            points: state.vectors.points_estimate(),
            dense_size: state.cfg.dim as i64,
            sparse_enabled: state.vectors.sparse_enabled(),
            disk_bytes: state.vectors.disk_bytes(),
            quantization: state.vectors.quantization().as_str().to_string(),
        },
        encode_progress: EncodeProgress { total: prompts_total, encoded: prompts_encoded, percent },
        by_tag: state.db.count_by_tag(30)?,
        by_month: state.db.count_by_month(24)?,
        by_source: state.db.count_by_source(20)?,
        by_kind: state.db.count_by_kind()?,
        jobs: state.db.job_counts_by_status()?,
    };
    res.render(Json(body));
    Ok(())
}

/// facet：分布优先走 Qdrant 的 `facet()`（开发文档 §7.2），取不到再降级 SQL，
/// 并在 `source` 字段里如实标注数据来自哪里。
/// 只统计资产点（不含提示词点）。
fn assets_only() -> qdrant_edge::Filter {
    crate::vector::filter_by_kind(crate::vector::KIND_ASSET)
}

#[handler]
pub async fn facets(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let key = req.query::<String>("key").unwrap_or_else(|| "tag".into());
    let limit = req.query::<usize>("limit").unwrap_or(20).clamp(1, 200);

    let (source, items): (String, Vec<FacetItem>) = match key.as_str() {
        // tag 一律走**实时 SQL**（需求 7）。原来优先用 Qdrant 的 `facet()`，
        // 而 facet 只看已 optimize 的段：详情页给资产加标签时 payload 刚落进 delta 段，
        // 没 optimize 之前 facet 查不到，于是首页标签过滤器里看不到新标签。
        // 走 SQLite 同时让计数口径和 `GET /api/tags` 的 `asset_count` 完全一致
        // （Qdrant 只统计已编码的资产，未编码的资产标签会漏算）。
        // 计数为 0 的标签（无人引用）不进过滤器，与 qdrant-facet 的语义保持一致。
        "tag" => (
            "sql".to_string(),
            state
                .db
                .count_by_tag(limit as i64)?
                .into_iter()
                .filter(|CountItem { count, .. }| *count > 0)
                .map(|CountItem { key, count }| FacetItem { value: key, count })
                .collect(),
        ),
        // kind 走 SQL：payload 里的 kind 是**点类型**（asset/prompt），
        // 页面要的是**媒体类型**（image/video），后者在 SQLite 里。
        "kind" => (
            "sql".to_string(),
            state
                .db
                .count_by_kind()?
                .into_iter()
                .map(|CountItem { key, count }| FacetItem { value: key, count })
                .collect(),
        ),
        "nsfw" => match state.vectors.facet(crate::vector::F_NSFW, limit, Some(assets_only())) {
            Ok(hits) if !hits.is_empty() => (
                "qdrant-facet".to_string(),
                hits.into_iter().map(|(value, count)| FacetItem { value, count }).collect(),
            ),
            _ => {
                let (_, _, _, nsfw, _) = state.db.count_assets()?;
                let total = state.db.count_assets()?.0;
                (
                    "sql".to_string(),
                    vec![
                        FacetItem { value: "true".into(), count: nsfw },
                        FacetItem { value: "false".into(), count: total - nsfw },
                    ],
                )
            }
        },
        "month" => (
            "sql".to_string(),
            state
                .db
                .count_by_month(limit as i64)?
                .into_iter()
                .map(|CountItem { key, count }| FacetItem { value: key, count })
                .collect(),
        ),
        "source" => (
            "sql".to_string(),
            state
                .db
                .count_by_source(limit as i64)?
                .into_iter()
                .map(|CountItem { key, count }| FacetItem { value: key, count })
                .collect(),
        ),
        other => {
            return Err(ApiError::bad_request(format!(
                "不支持的 facet key: {other}（可选 tag|kind|nsfw|month|source）"
            )))
        }
    };

    res.render(Json(FacetsResponse { key, source, items }));
    Ok(())
}

#[handler]
pub async fn size(depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let media_bytes = state.db.media_bytes()?.max(0) as u64;
    let vector_bytes = state.vectors.disk_bytes();
    let db_bytes = std::fs::metadata(&state.cfg.db_path).map(|m| m.len()).unwrap_or(0);
    let wal_bytes = std::fs::metadata(format!("{}-wal", state.cfg.db_path.display()))
        .map(|m| m.len())
        .unwrap_or(0);
    res.render(Json(SizeResponse {
        media_bytes,
        vector_bytes,
        db_bytes,
        wal_bytes,
        total_bytes: media_bytes + vector_bytes + db_bytes + wal_bytes,
    }));
    Ok(())
}
