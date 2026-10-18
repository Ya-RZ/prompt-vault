//! 行结构 → API DTO 的转换。字段名与 `INTERFACES.md` 对齐。

use std::collections::{HashMap, HashSet};

use crate::db::{AssetRow, PromptRow, VectorStateRow};
use crate::types::{AssetDto, PromptDto, VectorStateDto};

pub fn prompt_to_dto(row: PromptRow, encoded: bool) -> PromptDto {
    PromptDto {
        id: row.id,
        asset_id: row.asset_id,
        role: row.role,
        text: row.text,
        created_at: row.created_at,
        encoded,
    }
}

pub fn vector_state_dto(row: Option<VectorStateRow>) -> VectorStateDto {
    match row {
        Some(v) => VectorStateDto {
            encoded: true,
            fingerprint: Some(v.fingerprint),
            dim: Some(v.dim),
            resized: v.resized,
            vision_tokens: v.vision_tokens,
            encoded_at: Some(v.encoded_at),
        },
        None => VectorStateDto::missing(),
    }
}

/// 组装单个资产的完整 DTO（prompts + tags + 向量状态）。
pub fn asset_to_dto(
    asset: &AssetRow,
    prompts: Vec<PromptRow>,
    encoded_prompt_ids: &HashSet<i64>,
    tags: Vec<String>,
    vector: Option<VectorStateRow>,
) -> AssetDto {
    let media_url = asset.media_url();
    let thumb_url = asset.thumb_url();
    AssetDto {
        id: asset.id,
        kind: asset.kind.clone(),
        rel_path: asset.rel_path.clone(),
        title: asset.title.clone(),
        bytes: asset.bytes,
        width: asset.width,
        height: asset.height,
        duration_ms: asset.duration_ms,
        hash: asset.hash.clone(),
        hash_algo: asset.hash_algo.clone(),
        nsfw: asset.nsfw,
        added_at: asset.added_at.clone(),
        tags,
        prompts: prompts
            .into_iter()
            .map(|p| {
                let encoded = encoded_prompt_ids.contains(&p.id);
                prompt_to_dto(p, encoded)
            })
            .collect(),
        vector: vector_state_dto(vector),
        media_url,
        thumb_url,
    }
}

/// 批量组装（列表页用），避免 N+1 的逐条查询。
pub struct BatchLoader {
    pub prompts: HashMap<i64, Vec<PromptRow>>,
    pub encoded_prompts: HashSet<i64>,
    pub tags: HashMap<i64, Vec<String>>,
    pub vectors: HashMap<i64, VectorStateRow>,
}

impl BatchLoader {
    pub fn build(asset: &AssetRow) -> AssetDto {
        asset_to_dto(asset, Vec::new(), &HashSet::new(), Vec::new(), None)
    }

    pub fn apply(&self, asset: &AssetRow) -> AssetDto {
        asset_to_dto(
            asset,
            self.prompts.get(&asset.id).cloned().unwrap_or_default(),
            &self.encoded_prompts,
            self.tags.get(&asset.id).cloned().unwrap_or_default(),
            self.vectors.get(&asset.id).cloned(),
        )
    }
}
