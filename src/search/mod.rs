//! 检索：多路召回 + RRF 融合（开发文档 §1、§6.2）。
//!
//! 两条路径并存：
//! 1. **词法路（零模型成本）**：SQLite 模糊命中 prompt/tag/path → 元数据 join 到图；
//! 2. **向量路**：文本/图片编码 → Qdrant 稠密（+ 稀疏，若编码器提供了 sparse）。
//!
//! 稀疏路缺失时**显式降级**并在 `warnings` 里说明，绝不假装成功。

use std::collections::HashMap;

use serde_json::Value;

use crate::encode::EncodeOutcome;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::types::{
    EmbedInputItem, LexicalItem, MatchedPromptDto, QueryImageInfo, RankDto, SearchAssetDto,
    SearchItem, SearchResponse, SimilarPromptItem,
};
use crate::vector::{
    build_filter, payload_asset_id, payload_is_prompt, payload_row_id, KIND_ASSET, KIND_PROMPT,
    VEC_IMAGE, VEC_SPARSE_IMAGE, VEC_SPARSE_TEXT, VEC_TEXT,
};

/// RRF 常数（标准值 60）
const RRF_K: f64 = 60.0;

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub q: String,
    pub k: usize,
    pub mode: String,
    pub tags: Vec<String>,
    /// None = 不过滤；Some(false) = 只要非 nsfw（默认）
    pub nsfw: Option<bool>,
    pub kind: Option<String>,
    pub min_score: Option<f64>,
}

#[derive(Debug, Default, Clone)]
struct Candidate {
    best_score: f64,
    rrf: f64,
    dense: Option<usize>,
    sparse: Option<usize>,
    lexical: Option<usize>,
    matched_prompt: Option<i64>,
    sources: Vec<String>,
}

pub struct Engine<'a> {
    pub state: &'a AppState,
}

impl<'a> Engine<'a> {
    pub fn new(state: &'a AppState) -> Self {
        Self { state }
    }

    /// 文本检索（`GET /api/search/text`）
    pub async fn search_text(&self, query: SearchQuery) -> ApiResult<SearchResponse> {
        let started = std::time::Instant::now();
        let mut warnings: Vec<String> = Vec::new();
        let mode = normalize_mode(&query.mode);
        let mut candidates: HashMap<i64, Candidate> = HashMap::new();
        let mut effective = mode.clone();

        // ---- 词法路：零模型成本，永远可用 ----
        if mode == "lexical" || mode == "hybrid" {
            let hits = self.state.db.lexical_search(&query.q, query.k.max(20) * 2)?;
            let list: Vec<(i64, f64, Option<i64>)> =
                hits.iter().map(|(aid, _, _, _, s)| (*aid, *s, None)).collect();
            merge_ranked(&mut candidates, "lexical", &list);
        }

        // ---- 向量路 ----
        if mode != "lexical" {
            let space_ok = self.state.spaces_are_consistent(warnings_from(&mut warnings))?;
            if !space_ok {
                return Err(ApiError::fingerprint_mismatch(
                    "编码指纹与索引指纹不一致，禁止检索（开发文档 §9.3）",
                ));
            }

            let outcome = self.encode_text(&query.q).await?;
            let dense_list = self.dense_recall(&outcome, query.k)?;
            merge_ranked(&mut candidates, "dense", &dense_list);

            // 稀疏路：只有编码器给了 sparse 且索引启用了稀疏才有
            let sparse_available = self.state.vectors.sparse_enabled()
                && outcome.sparse.as_ref().map(|(i, _)| !i.is_empty()).unwrap_or(false);
            if sparse_available {
                if let Some((indices, values)) = outcome.sparse.as_ref() {
                    let filter = build_filter(Some(KIND_PROMPT), query.nsfw, &query.tags);
                    match self.state.vectors.search_sparse(
                        VEC_SPARSE_TEXT,
                        indices,
                        values,
                        query.k.max(20) * 3,
                        filter,
                    ) {
                        Ok(hits) => {
                            let list: Vec<(i64, f64, Option<i64>)> = hits
                                .iter()
                                .filter_map(|(_, score, payload)| {
                                    prompt_asset_id(payload).map(|aid| {
                                        (aid, *score as f64, payload_row_id(payload))
                                    })
                                })
                                .collect();
                            merge_ranked(&mut candidates, "sparse", &list);
                        }
                        Err(e) => warnings.push(format!("稀疏召回失败，已跳过: {e}")),
                    }
                }
            } else if mode == "hybrid" {
                // 稀疏路只在「编码器真的产出了 BM25 稀疏向量」时才走。
                // candle 只给稠密向量，所以 hybrid 实际一直是 dense + lexical —— 如实说明，
                // 别让调用方以为三路都跑了。
                warnings.push(
                    "candle 后端不产出 BM25 稀疏向量：hybrid 实际是 dense + lexical，索引里的 sparse 向量名不参与召回"
                        .to_string(),
                );
                effective = "dense+lexical".to_string();
            }
        }

        self.finish(query, candidates, effective, warnings, started).await
    }

    /// 以图搜图（`POST /api/search/image`）
    pub async fn search_image(
        &self,
        image_data_url: Option<String>,
        asset_id: Option<i64>,
        k: usize,
        tags: Vec<String>,
        nsfw: Option<bool>,
        query_label: String,
    ) -> ApiResult<SearchResponse> {
        let started = std::time::Instant::now();
        let mut warnings: Vec<String> = Vec::new();

        let mut space_warnings: Vec<String> = Vec::new();
        if !self.state.spaces_are_consistent(&mut space_warnings)? {
            return Err(ApiError::fingerprint_mismatch(
                "编码指纹与索引指纹不一致，禁止检索（开发文档 §9.3）",
            ));
        }
        warnings.extend(space_warnings);

        let (outcome, query_image) = if let Some(asset_id) = asset_id {
            // 按已有资产找相似：先取它的向量，不再重复编码
            let asset = self.state.db.asset_must_exist(asset_id)?;
            let stored = self.state.vectors.retrieve_vector(asset_id, VEC_IMAGE)?;
            match stored {
                Some(v) => (
                    EncodeOutcome {
                        dense: v,
                        sparse: None,
                        resized: None,
                        vision_tokens: None,
                        kind: Some("image".into()),
                        fingerprint: self.state.encoder.fingerprint_hint(),
                    },
                    QueryImageInfo { resized: None, vision_tokens: None },
                ),
                None => {
                    warnings.push(format!(
                        "资产 {asset_id}（{}）还没有向量，已改为按图片内容重新编码",
                        asset.rel_path
                    ));
                    let data_url = self.read_media_as_data_url(&asset.rel_path)?;
                    let outcome = self.encode_image(data_url).await?;
                    let info = QueryImageInfo {
                        resized: outcome.resized.clone(),
                        vision_tokens: outcome.vision_tokens,
                    };
                    (outcome, info)
                }
            }
        } else if let Some(data_url) = image_data_url {
            let outcome = self.encode_image(data_url).await?;
            let info =
                QueryImageInfo { resized: outcome.resized.clone(), vision_tokens: outcome.vision_tokens };
            (outcome, info)
        } else {
            return Err(ApiError::bad_request("必须提供 file / image_base64 / asset_id 之一"));
        };

        let mut candidates: HashMap<i64, Candidate> = HashMap::new();
        let filter = build_filter(Some(KIND_ASSET), nsfw, &tags);
        let filter = filter.or_else(|| build_filter(Some(KIND_ASSET), nsfw, &[]));
        let hits = self.state.vectors.search_dense(
            VEC_IMAGE,
            &outcome.dense,
            k.max(20) * 2,
            filter,
            true,
            Some(2.0),
        )?;
        let list: Vec<(i64, f64, Option<i64>)> = hits
            .iter()
            .filter_map(|(_, score, payload)| {
                payload_asset_id_or_row(payload)
                    .map(|aid| (aid, *score as f64, payload_matched_prompt(payload)))
            })
            .collect();
        merge_ranked(&mut candidates, "dense", &list);

        // 稀疏以图搜图：只有编码器给了图片 sparse 才有（本项目的图片 sparse 预留）
        if self.state.vectors.sparse_enabled() {
            if let Some((indices, values)) = outcome.sparse.as_ref() {
                let filter = build_filter(Some(KIND_ASSET), nsfw, &tags);
                if let Ok(hits) =
                    self.state.vectors.search_sparse(VEC_SPARSE_IMAGE, indices, values, k.max(20) * 3, filter)
                {
                    let list: Vec<(i64, f64, Option<i64>)> = hits
                        .iter()
                        .filter_map(|(_, score, payload)| {
                            payload_asset_id_or_row(payload)
                                .map(|aid| (aid, *score as f64, payload_matched_prompt(payload)))
                        })
                        .collect();
                    merge_ranked(&mut candidates, "sparse", &list);
                }
            }
        }

        let query = SearchQuery {
            q: query_label,
            k,
            mode: "dense".into(),
            tags,
            nsfw,
            // 注意：这里的 kind 是**资产类型**（image/video），不是 Qdrant 的点类型；
            // 以图搜图不按资产类型过滤，否则会把结果全滤掉。
            kind: None,
            min_score: None,
        };
        let mut resp = self.finish(query, candidates, "dense".into(), warnings, started).await?;
        resp.query_image = Some(query_image);
        Ok(resp)
    }

    /// 相似提示词（`GET /api/prompts/{id}/similar`）
    pub fn similar_prompts(&self, prompt_id: i64, k: usize) -> ApiResult<Vec<SimilarPromptItem>> {
        self.state
            .db
            .get_prompt(prompt_id)?
            .ok_or_else(|| ApiError::not_found(format!("提示词 {prompt_id} 不存在")))?;
        let dense = self.state.vectors.retrieve_vector(prompt_id + crate::vector::PROMPT_ID_OFFSET, VEC_TEXT)?;
        let Some(dense) = dense else {
            return Err(ApiError::conflict(format!(
                "提示词 {prompt_id} 还没有向量，无法找相似（先跑一次编码作业）"
            )));
        };
        let hits = self.state.vectors.search_dense(VEC_TEXT, &dense, k + 5, None, true, Some(2.0))?;
        let mut out = Vec::new();
        for (pid, score, payload) in hits {
            // 排除自己
            let row_id = payload_row_id(&payload).unwrap_or(pid);
            if row_id == prompt_id || pid == prompt_id + crate::vector::PROMPT_ID_OFFSET {
                continue;
            }
            let Some(p) = self.state.db.get_prompt(row_id)? else { continue };
            let Some(asset) = self.state.db.get_asset(p.asset_id)? else { continue };
            out.push(SimilarPromptItem {
                prompt: crate::api::dto::prompt_to_dto(p, true),
                asset: to_search_asset(&asset, Vec::new()),
                score: score as f64,
            });
            if out.len() >= k {
                break;
            }
        }
        Ok(out)
    }

    /// 纯词法检索（调试口）
    pub fn lexical(&self, q: &str, k: usize) -> ApiResult<Vec<LexicalItem>> {
        let hits = self.state.db.lexical_search(q, k)?;
        Ok(hits
            .into_iter()
            .map(|(asset_id, rel_path, hit_field, hit_text, score)| LexicalItem {
                asset_id,
                rel_path,
                hit_field,
                hit_text,
                score,
            })
            .collect())
    }

    // ------------------------------------------------------------ 内部

    async fn encode_text(&self, text: &str) -> ApiResult<EncodeOutcome> {
        let items = vec![EmbedInputItem { text: Some(text.to_string()), image: None }];
        let mut outcomes = self.state.encoder.embed(items).await?;
        outcomes
            .pop()
            .ok_or_else(|| ApiError::encoder_error("编码器未返回该条向量"))
    }

    async fn encode_image(&self, data_url: String) -> ApiResult<EncodeOutcome> {
        let items = vec![EmbedInputItem { text: None, image: Some(data_url) }];
        let mut outcomes = self.state.encoder.embed(items).await?;
        outcomes
            .pop()
            .ok_or_else(|| ApiError::encoder_error("编码器未返回该条图片向量"))
    }

    /// 稠密召回：文本查 text（提示词点）与 image（资产点）两个向量名，合并。
    /// 返回 (asset_id, score, 命中的提示词 id)。
    fn dense_recall(&self, outcome: &EncodeOutcome, k: usize) -> ApiResult<Vec<(i64, f64, Option<i64>)>> {
        let mut out: Vec<(i64, f64, Option<i64>)> = Vec::new();
        let filter_prompt = build_filter(Some(KIND_PROMPT), None, &[]);
        if let Ok(hits) = self.state.vectors.search_dense(
            VEC_TEXT,
            &outcome.dense,
            k.max(20) * 2,
            filter_prompt,
            true,
            Some(2.0),
        ) {
            for (_, score, payload) in hits {
                if let Some(aid) = prompt_asset_id(&payload) {
                    out.push((aid, score as f64, payload_matched_prompt(&payload)));
                }
            }
        }
        let filter_asset = build_filter(Some(KIND_ASSET), None, &[]);
        if let Ok(hits) = self.state.vectors.search_dense(
            VEC_IMAGE,
            &outcome.dense,
            k.max(20) * 2,
            filter_asset,
            true,
            Some(2.0),
        ) {
            for (_, score, payload) in hits {
                if let Some(aid) = payload_asset_id_or_row(&payload) {
                    out.push((aid, score as f64, None));
                }
            }
        }
        // 同一个 asset 命中多次时取最高分
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut seen = std::collections::HashSet::new();
        // 同一资产命中多条时只留分最高的那条（它自带 matched 提示词）
        out.retain(|(id, _, _)| seen.insert(*id));
        Ok(out)
    }

    fn read_media_as_data_url(&self, rel_path: &str) -> ApiResult<String> {
        let path = self.state.cfg.media_path(rel_path);
        let bytes = std::fs::read(&path)
            .map_err(|e| ApiError::not_found(format!("媒体文件读不到 {}: {e}", path.display())))?;
        Ok(crate::encode::image_data_url(&bytes, crate::encode::mime_for_path(rel_path)))
    }

    async fn finish(
        &self,
        query: SearchQuery,
        candidates: HashMap<i64, Candidate>,
        effective: String,
        warnings: Vec<String>,
        started: std::time::Instant,
    ) -> ApiResult<SearchResponse> {
        let mut ranked: Vec<(i64, Candidate)> = candidates.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.rrf
                .partial_cmp(&a.1.rrf)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    b.1.best_score.partial_cmp(&a.1.best_score).unwrap_or(std::cmp::Ordering::Equal)
                })
        });

        let ids: Vec<i64> = ranked.iter().map(|(id, _)| *id).take(query.k * 3).collect();
        let assets = self.state.db.assets_by_ids(&ids)?;
        let tags_map = self.state.db.tags_for_many(&ids)?;
        let mut prompt_map = HashMap::new();
        for (_, cand) in ranked.iter() {
            if let Some(pid) = cand.matched_prompt {
                if let Some(p) = self.state.db.get_prompt(pid)? {
                    prompt_map.insert(pid, p);
                }
            }
        }

        let mut items = Vec::new();
        for (asset_id, cand) in ranked {
            let Some(asset) = assets.get(&asset_id) else { continue };
            // nsfw 过滤（在 SQLite 侧再兜一层，Qdrant payload 只是第一道）
            if let Some(want) = query.nsfw {
                if asset.nsfw != want {
                    continue;
                }
            }
            if let Some(kind) = query.kind.as_deref() {
                if asset.kind != kind {
                    continue;
                }
            }
            if let Some(min) = query.min_score {
                if cand.best_score < min {
                    continue;
                }
            }
            let matched = cand.matched_prompt.and_then(|pid| {
                prompt_map.get(&pid).map(|p| MatchedPromptDto {
                    prompt_id: p.id,
                    text: p.text.clone(),
                    role: p.role.clone(),
                })
            });
            items.push(SearchItem {
                score: round4(cand.best_score),
                sources: cand.sources.clone(),
                rank: RankDto {
                    dense: cand.dense,
                    sparse: cand.sparse,
                    lexical: cand.lexical,
                    rrf: round6(cand.rrf),
                },
                asset: to_search_asset(asset, tags_map.get(&asset_id).cloned().unwrap_or_default()),
                matched,
            });
            if items.len() >= query.k {
                break;
            }
        }

        Ok(SearchResponse {
            query: query.q,
            mode: query.mode,
            effective_mode: effective,
            took_ms: started.elapsed().as_millis() as u64,
            warnings,
            total: items.len(),
            items,
            query_image: None,
        })
    }
}

// ---------------------------------------------------------------- 融合

/// RRF 融合：把多路有序列表按 `1/(k+rank)` 累加（开发文档 §2「融合排序（RRF）」）。
fn merge_ranked(
    candidates: &mut HashMap<i64, Candidate>,
    source: &str,
    list: &[(i64, f64, Option<i64>)],
) {
    for (rank0, (asset_id, score, matched_prompt)) in list.iter().enumerate() {
        let rank = rank0 + 1;
        let entry = candidates.entry(*asset_id).or_default();
        entry.rrf += 1.0 / (RRF_K + rank as f64);
        if score > &entry.best_score {
            entry.best_score = *score;
        }
        // 列表按分数降序，第一条就是最高分那条 —— 它带的提示词才是"命中提示词"
        if entry.matched_prompt.is_none() {
            entry.matched_prompt = *matched_prompt;
        }
        let slot = match source {
            "dense" => &mut entry.dense,
            "sparse" => &mut entry.sparse,
            "lexical" => &mut entry.lexical,
            _ => &mut entry.dense,
        };
        if slot.is_none() {
            *slot = Some(rank);
        }
        if !entry.sources.iter().any(|s| s == source) {
            entry.sources.push(source.to_string());
        }
    }
}

fn normalize_mode(mode: &str) -> String {
    match mode.to_ascii_lowercase().as_str() {
        "lexical" | "text" | "keyword" => "lexical".to_string(),
        "dense" | "vector" => "dense".to_string(),
        _ => "hybrid".to_string(),
    }
}

// ---------------------------------------------------------------- 工具

fn payload_asset_id_or_row(payload: &Value) -> Option<i64> {
    if payload_is_prompt(payload) {
        payload_asset_id(payload)
    } else {
        payload_row_id(payload)
    }
}

fn prompt_asset_id(payload: &Value) -> Option<i64> {
    payload_asset_id(payload)
}

/// 命中提示词点时的提示词 id（asset 点没有）。
fn payload_matched_prompt(payload: &Value) -> Option<i64> {
    if payload_is_prompt(payload) {
        payload_row_id(payload)
    } else {
        None
    }
}

fn warnings_from(_w: &mut Vec<String>) -> &mut Vec<String> {
    _w
}

pub fn to_search_asset(asset: &crate::db::AssetRow, tags: Vec<String>) -> SearchAssetDto {
    SearchAssetDto {
        id: asset.id,
        rel_path: asset.rel_path.clone(),
        media_url: asset.media_url(),
        thumb_url: asset.thumb_url(),
        width: asset.width,
        height: asset.height,
        nsfw: asset.nsfw,
        tags,
    }
}

fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}
