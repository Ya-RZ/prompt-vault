//! Qdrant Server 封装（2026-10-18 由 qdrant-edge 嵌入式 shard 切换而来）。
//!
//! 这一层只做四件事：建/开集合、写点、查点、导出点。融合排序在 `crate::search`。
//!
//! 服务地址与集合名由命令行配置：`--qdrant-url`（`PV_QDRANT_URL`）与
//! `--qdrant-collection`（`PV_QDRANT_COLLECTION`）。启动时连不上会重试 3 次，
//! 仍然失败就让启动失败——比"服务起来了但搜不到东西"可预测。

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use qdrant_client::qdrant::{
    point_id, quantization_config, vector_output, vectors_config, vectors_output, Condition,
    CountPointsBuilder, CreateCollectionBuilder, CreateFieldIndexCollectionBuilder, Datatype,
    DeletePointsBuilder, Distance, FacetCountsBuilder, FieldType, Filter, GetPointsBuilder,
    Modifier, PointId, PointStruct, PointsIdsList, QuantizationSearchParamsBuilder,
    QuantizationType, Query, QueryPointsBuilder, ScalarQuantization, ScrollPointsBuilder,
    SearchParamsBuilder, SetPayloadPointsBuilder, SparseVectorConfig, SparseVectorParamsBuilder,
    TurboQuantBitSize, TurboQuantization, UpsertPointsBuilder, Value as QValue, Vector,
    VectorInput, VectorOutput, VectorParams, VectorParamsBuilder, VectorParamsMap, VectorsConfig,
    VectorsOutput,
};
use qdrant_client::{Payload, Qdrant};
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};

/// 向量名（开发文档 §6.1 / INTERFACES.md §2）
pub const VEC_IMAGE: &str = "image";
pub const VEC_TEXT: &str = "text";
pub const VEC_SPARSE_IMAGE: &str = "sparse_image";
pub const VEC_SPARSE_TEXT: &str = "sparse_text";

/// 默认集合名（可用 `--qdrant-collection` 覆盖）
pub const DEFAULT_COLLECTION: &str = "pv";

/// 提示词点的 id 偏移：asset.id 直接用，prompt 点 = prompt.id + 10 亿（两者不会撞）
pub const PROMPT_ID_OFFSET: i64 = 1_000_000_000;

/// payload 字段名（过滤与 facet 都按这些键）
pub const F_KIND: &str = "kind";
pub const F_ROW_ID: &str = "row_id";
pub const F_ASSET_ID: &str = "asset_id";
pub const F_FINGERPRINT: &str = "fingerprint";
pub const F_NSFW: &str = "nsfw";
pub const F_TAGS: &str = "tags";
pub const F_ADDED_AT_MS: &str = "added_at_ms";
pub const F_SOURCE: &str = "source";

pub const KIND_ASSET: &str = "asset";
pub const KIND_PROMPT: &str = "prompt";

/// 量化方式（服务端集合级别配置，建集合时定，之后改不了 —— 开发文档 §6.1）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantMode {
    /// Scalar int8（本项目默认）
    Scalar,
    /// TurboQuant bits4（A/B 备选）
    Turbo4,
    /// 不量化
    Off,
}

impl QuantMode {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "turbo4" | "turbo" | "bits4" => QuantMode::Turbo4,
            "off" | "none" | "f32" => QuantMode::Off,
            _ => QuantMode::Scalar,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            QuantMode::Scalar => "scalar-int8",
            QuantMode::Turbo4 => "turbo-bits4",
            QuantMode::Off => "none",
        }
    }
}

pub struct VectorStore {
    client: Qdrant,
    url: String,
    collection: String,
    dim: usize,
    sparse_enabled: bool,
    quantization: QuantMode,
}

impl VectorStore {
    /// 连接并准备集合。连不上时最多重试 3 次（每次间隔递增），仍失败返回错误。
    pub async fn connect(
        url: &str,
        collection: &str,
        dim: usize,
        sparse_enabled: bool,
        quant: QuantMode,
    ) -> ApiResult<Self> {
        let mut last: Option<ApiError> = None;
        for attempt in 1u64..=3 {
            match Self::connect_once(url, collection, dim, sparse_enabled, quant).await {
                Ok(store) => return Ok(store),
                Err(e) => {
                    tracing::warn!(
                        attempt,
                        url,
                        error = %e.message,
                        "连接 Qdrant 失败，稍后重试"
                    );
                    last = Some(e);
                    if attempt < 3 {
                        tokio::time::sleep(Duration::from_secs(attempt)).await;
                    }
                }
            }
        }
        Err(last.unwrap_or_else(|| ApiError::internal("连接 Qdrant 失败")))
    }

    async fn connect_once(
        url: &str,
        collection: &str,
        dim: usize,
        sparse_enabled: bool,
        quant: QuantMode,
    ) -> ApiResult<Self> {
        let url = grpc_url(url);
        let client = Qdrant::from_url(&url)
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ApiError::internal(format!("Qdrant 客户端构造失败（{url}）: {e}")))?;
        client
            .health_check()
            .await
            .map_err(|e| ApiError::internal(format!("Qdrant 健康检查失败（{url}）: {e}")))?;

        let store = Self {
            client,
            url: url.clone(),
            collection: collection.to_string(),
            dim,
            sparse_enabled,
            quantization: quant,
        };
        store.ensure_collection().await?;
        store.ensure_indexes().await?;
        Ok(store)
    }

    fn dense_params(&self) -> VectorParams {
        let mut builder = VectorParamsBuilder::new(self.dim as u64, Distance::Cosine)
            .datatype(Datatype::Float16);
        if let Some(qc) = self.quant_config() {
            builder = builder.quantization_config(qc);
        }
        builder.build()
    }

    fn quant_config(&self) -> Option<quantization_config::Quantization> {
        match self.quantization {
            QuantMode::Scalar => {
                Some(quantization_config::Quantization::Scalar(
                    ScalarQuantization {
                        r#type: QuantizationType::Int8 as i32,
                        quantile: Some(0.99),
                        ..Default::default()
                    },
                ))
            }
            QuantMode::Turbo4 => {
                Some(quantization_config::Quantization::Turboquant(
                    TurboQuantization {
                        bits: Some(TurboQuantBitSize::Bits4 as i32),
                        ..Default::default()
                    },
                ))
            }
            QuantMode::Off => None,
        }
    }

    /// 集合不存在就建；已存在则沿用（向量配置不可变，不试图修改）。
    async fn ensure_collection(&self) -> ApiResult<()> {
        let exists = self
            .client
            .collection_exists(&self.collection)
            .await
            .map_err(|e| ApiError::internal(format!("查询集合是否存在失败: {e}")))?;
        if exists {
            return Ok(());
        }

        let dense = self.dense_params();
        let mut map = HashMap::new();
        map.insert(VEC_IMAGE.to_string(), dense);
        map.insert(VEC_TEXT.to_string(), dense);
        let config = VectorsConfig {
            config: Some(vectors_config::Config::ParamsMap(VectorParamsMap { map })),
        };
        let mut builder = CreateCollectionBuilder::new(&self.collection).vectors_config(config);

        if self.sparse_enabled {
            let params = SparseVectorParamsBuilder::default()
                .modifier(Modifier::Idf as i32)
                .build();
            let mut sparse = HashMap::new();
            sparse.insert(VEC_SPARSE_IMAGE.to_string(), params);
            sparse.insert(VEC_SPARSE_TEXT.to_string(), params);
            builder = builder.sparse_vectors_config(SparseVectorConfig { map: sparse });
        }

        self.client
            .create_collection(builder)
            .await
            .map_err(|e| ApiError::internal(format!("创建集合 {} 失败: {e}", self.collection)))?;
        tracing::info!(
            collection = %self.collection,
            dim = self.dim,
            sparse = self.sparse_enabled,
            quant = self.quantization.as_str(),
            "已创建 Qdrant 集合"
        );
        Ok(())
    }

    /// payload 索引：过滤与 facet 都要它。已存在会报错，按幂等处理。
    async fn ensure_indexes(&self) -> ApiResult<()> {
        let fields: Vec<(&str, FieldType)> = vec![
            (F_KIND, FieldType::Keyword),
            (F_FINGERPRINT, FieldType::Keyword),
            (F_NSFW, FieldType::Bool),
            (F_TAGS, FieldType::Keyword),
            (F_ADDED_AT_MS, FieldType::Integer),
            (F_SOURCE, FieldType::Keyword),
        ];
        for (field, field_type) in fields {
            let request =
                CreateFieldIndexCollectionBuilder::new(&self.collection, field, field_type);
            if let Err(e) = self.client.create_field_index(request).await {
                tracing::debug!(field, error = %e, "payload 索引创建被跳过（可能已存在）");
            }
        }
        Ok(())
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn sparse_enabled(&self) -> bool {
        self.sparse_enabled
    }

    pub fn quantization(&self) -> QuantMode {
        self.quantization
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn collection(&self) -> &str {
        &self.collection
    }

    /// Server 侧自己负责刷盘，这里保留空实现以免上层到处改。
    pub fn flush(&self) -> ApiResult<()> {
        Ok(())
    }

    /// Server 有后台 optimizer，这里保留方法但不再主动触发。
    pub fn optimize(&self) -> ApiResult<bool> {
        Ok(false)
    }

    // ------------------------------------------------------------ 写入

    /// 一个 asset 点：稠密（image 向量 + payload）
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_asset(
        &self,
        asset_id: i64,
        dense: &[f32],
        sparse: Option<(&[u32], &[f32])>,
        fingerprint: &str,
        nsfw: bool,
        tags: &[String],
        added_at_ms: i64,
        source: &str,
    ) -> ApiResult<()> {
        let mut named: HashMap<String, Vector> = HashMap::new();
        named.insert(VEC_IMAGE.to_string(), Vector::from(dense.to_vec()));
        if self.sparse_enabled {
            if let Some((idx, val)) = sparse {
                named.insert(VEC_SPARSE_IMAGE.to_string(), sparse_vector(idx, val));
            }
        }
        let payload = json!({
            F_KIND: KIND_ASSET,
            F_ROW_ID: asset_id,
            F_FINGERPRINT: fingerprint,
            F_NSFW: nsfw,
            F_TAGS: tags,
            F_ADDED_AT_MS: added_at_ms,
            F_SOURCE: source,
        });
        self.upsert_one(asset_id, named, payload).await
    }

    /// 一个 prompt 点：稠密（text 向量 + payload），带上它所属 asset
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_prompt(
        &self,
        prompt_id: i64,
        asset_id: i64,
        dense: &[f32],
        sparse: Option<(&[u32], &[f32])>,
        fingerprint: &str,
        nsfw: bool,
        tags: &[String],
    ) -> ApiResult<()> {
        let mut named: HashMap<String, Vector> = HashMap::new();
        named.insert(VEC_TEXT.to_string(), Vector::from(dense.to_vec()));
        if self.sparse_enabled {
            if let Some((idx, val)) = sparse {
                named.insert(VEC_SPARSE_TEXT.to_string(), sparse_vector(idx, val));
            }
        }
        let payload = json!({
            F_KIND: KIND_PROMPT,
            F_ROW_ID: prompt_id,
            F_ASSET_ID: asset_id,
            F_FINGERPRINT: fingerprint,
            F_NSFW: nsfw,
            F_TAGS: tags,
        });
        self.upsert_one(prompt_id + PROMPT_ID_OFFSET, named, payload)
            .await
    }

    async fn upsert_one(
        &self,
        row_id: i64,
        named: HashMap<String, Vector>,
        payload: Value,
    ) -> ApiResult<()> {
        let payload = Payload::try_from(payload)
            .map_err(|e| ApiError::internal(format!("payload 不是合法 JSON 对象: {e}")))?;
        let point = PointStruct::new(row_id as u64, named, payload);
        self.client
            .upsert_points(UpsertPointsBuilder::new(&self.collection, vec![point]).wait(true))
            .await
            .map_err(|e| ApiError::internal(format!("写入向量失败: {e}")))?;
        Ok(())
    }

    /// 删资产点与其名下提示词点（开发文档 §13 第 7 条）。
    pub async fn delete_asset_and_prompts(
        &self,
        asset_id: i64,
        prompt_ids: &[i64],
    ) -> ApiResult<usize> {
        let mut ids: Vec<PointId> = vec![(asset_id as u64).into()];
        for p in prompt_ids {
            ids.push(((*p + PROMPT_ID_OFFSET) as u64).into());
        }
        let n = ids.len();
        self.client
            .delete_points(
                DeletePointsBuilder::new(&self.collection)
                    .points(PointsIdsList { ids })
                    .wait(true),
            )
            .await
            .map_err(|e| ApiError::internal(format!("删除向量失败: {e}")))?;
        Ok(n)
    }

    pub async fn delete_prompt(&self, prompt_id: i64) -> ApiResult<()> {
        self.client
            .delete_points(
                DeletePointsBuilder::new(&self.collection)
                    .points(PointsIdsList {
                        ids: vec![((prompt_id + PROMPT_ID_OFFSET) as u64).into()],
                    })
                    .wait(true),
            )
            .await
            .map_err(|e| ApiError::internal(format!("删除提示词向量失败: {e}")))?;
        Ok(())
    }

    /// 同步 payload（tags / nsfw 变了要更新，否则过滤与 facet 会失真）。
    pub async fn set_payload(&self, point_ids: &[i64], payload: Value) -> ApiResult<()> {
        if point_ids.is_empty() {
            return Ok(());
        }
        let payload = Payload::try_from(payload)
            .map_err(|_| ApiError::bad_request("payload 必须是 JSON 对象"))?;
        let ids: Vec<PointId> = point_ids.iter().map(|i| (*i as u64).into()).collect();
        self.client
            .set_payload(
                SetPayloadPointsBuilder::new(&self.collection, payload)
                    .points_selector(PointsIdsList { ids })
                    .wait(true),
            )
            .await
            .map_err(|e| ApiError::internal(format!("更新 payload 失败: {e}")))?;
        Ok(())
    }

    pub async fn set_asset_payload(
        &self,
        asset_id: i64,
        nsfw: bool,
        tags: &[String],
    ) -> ApiResult<()> {
        self.set_payload(&[asset_id], json!({ F_NSFW: nsfw, F_TAGS: tags }))
            .await
    }

    pub async fn set_prompt_payload(
        &self,
        prompt_id: i64,
        asset_id: i64,
        nsfw: bool,
        tags: &[String],
    ) -> ApiResult<()> {
        let _ = asset_id;
        self.set_payload(
            &[prompt_id + PROMPT_ID_OFFSET],
            json!({ F_NSFW: nsfw, F_TAGS: tags }),
        )
        .await
    }

    // ------------------------------------------------------------ 查询

    /// 稠密查询：`using` 选向量名（`image` 或 `text`）。
    pub async fn search_dense(
        &self,
        using: &str,
        vector: &[f32],
        limit: usize,
        filter: Option<Filter>,
        rescore: bool,
        oversampling: Option<f64>,
    ) -> ApiResult<Vec<(i64, f32, Value)>> {
        self.run_query(
            Query::new_nearest(vector.to_vec()),
            using,
            limit,
            filter,
            rescore,
            oversampling,
        )
        .await
    }

    /// 稀疏查询（BM25）。
    ///
    /// 现状：现有编码后端（candle）只产出稠密向量，不产出 BM25 稀疏向量，
    /// 所以这条路径在正常检索里走不到 —— 保留它是为了 import/导出与将来接入 BM25。
    pub async fn search_sparse(
        &self,
        using: &str,
        indices: &[u32],
        values: &[f32],
        limit: usize,
        filter: Option<Filter>,
    ) -> ApiResult<Vec<(i64, f32, Value)>> {
        if !self.sparse_enabled {
            return Err(ApiError::sparse_unavailable(
                "本集合未启用稀疏向量（建集合时 --sparse=false）",
            ));
        }
        self.run_query(
            Query::new_nearest(VectorInput::new_sparse(
                indices.to_vec(),
                values.to_vec(),
            )),
            using,
            limit,
            filter,
            false,
            None,
        )
        .await
    }

    async fn run_query(
        &self,
        query: Query,
        using: &str,
        limit: usize,
        filter: Option<Filter>,
        rescore: bool,
        oversampling: Option<f64>,
    ) -> ApiResult<Vec<(i64, f32, Value)>> {
        let mut params = QuantizationSearchParamsBuilder::default()
            .ignore(false)
            .rescore(rescore);
        if let Some(o) = oversampling {
            params = params.oversampling(o);
        }
        let search_params = SearchParamsBuilder::default()
            .quantization(params.build())
            .build();

        let mut request = QueryPointsBuilder::new(&self.collection)
            .query(query)
            .using(using)
            .limit(limit as u64)
            .with_payload(true)
            .params(search_params);
        if let Some(f) = filter {
            request = request.filter(f);
        }

        let response = self
            .client
            .query(request)
            .await
            .map_err(|e| ApiError::internal(format!("查询失败: {e}")))?;
        Ok(response
            .result
            .into_iter()
            .map(|point| {
                let id = point.id.as_ref().map(point_id_to_i64).unwrap_or(-1);
                let payload = payload_to_json(&point.payload);
                (id, point.score, payload)
            })
            .collect())
    }

    pub async fn count(&self) -> ApiResult<usize> {
        let response = self
            .client
            .count(CountPointsBuilder::new(&self.collection).exact(true))
            .await
            .map_err(|e| ApiError::internal(format!("count 失败: {e}")))?;
        Ok(response.result.map(|r| r.count as usize).unwrap_or(0))
    }

    pub async fn count_assets(&self) -> ApiResult<usize> {
        let response = self
            .client
            .count(
                CountPointsBuilder::new(&self.collection)
                    .filter(filter_by_kind(KIND_ASSET))
                    .exact(true),
            )
            .await
            .map_err(|e| ApiError::internal(format!("count 失败: {e}")))?;
        Ok(response.result.map(|r| r.count as usize).unwrap_or(0))
    }

    /// facet 直接出分布（开发文档 §7.2）。
    ///
    /// `filter` 用来把统计范围限定在资产点上：asset 与 prompt 是两类点，
    /// 不加过滤会把同一张图算两遍。
    pub async fn facet(
        &self,
        key: &str,
        limit: usize,
        filter: Option<Filter>,
    ) -> ApiResult<Vec<(String, i64)>> {
        let mut request = FacetCountsBuilder::new(&self.collection, key)
            .limit(limit as u64)
            .exact(true);
        if let Some(f) = filter {
            request = request.filter(f);
        }
        let response = self
            .client
            .facet(request)
            .await
            .map_err(|e| ApiError::internal(format!("facet 失败: {e}")))?;
        Ok(response
            .hits
            .into_iter()
            .map(|hit| {
                let value = hit.value.map(facet_value_to_string).unwrap_or_default();
                (value, hit.count as i64)
            })
            .collect())
    }

    /// 取回指定点（用于按 asset_id 找相似时先取它的向量）。
    pub async fn retrieve_vector(&self, row_id: i64, using: &str) -> ApiResult<Option<Vec<f32>>> {
        let response = self
            .client
            .get_points(
                GetPointsBuilder::new(&self.collection, vec![PointId::from(row_id as u64)])
                    .with_vectors(true)
                    .with_payload(false),
            )
            .await
            .map_err(|e| ApiError::internal(format!("retrieve 失败: {e}")))?;
        Ok(response
            .result
            .first()
            .and_then(|p| p.vectors.as_ref())
            .and_then(|v| extract_named_dense(v, using)))
    }

    // ------------------------------------------------------------ 导出通路

    /// 全量导出为 JSONL：`{"id":..,"payload":{..},"vectors":{"name":{"dense":[..]}|{"indices":[..],"values":[..]}}}`
    pub async fn export_jsonl(&self, path: impl AsRef<Path>) -> ApiResult<usize> {
        use std::io::Write;
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ApiError::internal(e.to_string()))?;
        }
        let mut file = std::fs::File::create(path).map_err(|e| ApiError::internal(e.to_string()))?;

        let mut offset: Option<PointId> = None;
        let mut total = 0usize;
        loop {
            let mut request = ScrollPointsBuilder::new(&self.collection)
                .with_payload(true)
                .with_vectors(true)
                .limit(500);
            if let Some(o) = offset.clone() {
                request = request.offset(o);
            }
            let response = self
                .client
                .scroll(request)
                .await
                .map_err(|e| ApiError::internal(format!("导出失败: {e}")))?;
            if response.result.is_empty() {
                break;
            }
            for point in &response.result {
                let line = json!({
                    "id": point.id.as_ref().map(point_id_to_i64).unwrap_or(-1),
                    "payload": payload_to_json(&point.payload),
                    "vectors": point.vectors.as_ref().map(vectors_to_json),
                });
                writeln!(file, "{line}").map_err(|e| ApiError::internal(e.to_string()))?;
                total += 1;
            }
            match response.next_page_offset {
                Some(next) => offset = Some(next),
                None => break,
            }
        }
        file.flush().map_err(|e| ApiError::internal(e.to_string()))?;
        Ok(total)
    }

    /// 从 JSONL 重建（搬家 / 灾备）。
    pub async fn import_jsonl(&self, path: impl AsRef<Path>) -> ApiResult<usize> {
        let text = std::fs::read_to_string(path.as_ref())
            .map_err(|e| ApiError::internal(format!("读取导出文件失败: {e}")))?;
        let mut total = 0usize;
        let mut batch: Vec<PointStruct> = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line)
                .map_err(|e| ApiError::bad_request(format!("导出文件格式错误: {e}")))?;
            let id = v["id"].as_i64().unwrap_or_default();
            let payload = Payload::try_from(v.get("payload").cloned().unwrap_or(json!({})))
                .map_err(|_| ApiError::bad_request("导出文件里的 payload 不是对象"))?;
            let mut named: HashMap<String, Vector> = HashMap::new();
            if let Some(obj) = v.get("vectors").and_then(|x| x.as_object()) {
                for (name, vec_json) in obj {
                    if let Some(dense) = vec_json.get("dense").and_then(|d| d.as_array()) {
                        let vals: Vec<f32> = dense
                            .iter()
                            .filter_map(|x| x.as_f64().map(|f| f as f32))
                            .collect();
                        named.insert(name.clone(), Vector::from(vals));
                    } else if let (Some(idx), Some(vals)) = (
                        vec_json.get("indices").and_then(|d| d.as_array()),
                        vec_json.get("values").and_then(|d| d.as_array()),
                    ) {
                        let indices: Vec<u32> = idx
                            .iter()
                            .filter_map(|x| x.as_u64().map(|u| u as u32))
                            .collect();
                        let values: Vec<f32> = vals
                            .iter()
                            .filter_map(|x| x.as_f64().map(|f| f as f32))
                            .collect();
                        named.insert(name.clone(), sparse_vector(&indices, &values));
                    }
                }
            }
            batch.push(PointStruct::new(id as u64, named, payload));
            total += 1;
            if batch.len() >= 100 {
                self.flush_batch(&mut batch).await?;
            }
        }
        self.flush_batch(&mut batch).await?;
        Ok(total)
    }

    async fn flush_batch(&self, batch: &mut Vec<PointStruct>) -> ApiResult<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let points = std::mem::take(batch);
        self.client
            .upsert_points(UpsertPointsBuilder::new(&self.collection, points).wait(true))
            .await
            .map_err(|e| ApiError::internal(format!("导入向量失败: {e}")))?;
        Ok(())
    }

    /// 磁盘占用由 Qdrant 服务端管理，客户端拿不到；返回 0。
    pub fn disk_bytes(&self) -> u64 {
        0
    }

    pub async fn points_estimate(&self) -> Option<u64> {
        self.count().await.ok().map(|n| n as u64)
    }
}

// ---------------------------------------------------------------- 过滤条件

pub fn filter_by_kind(kind: &str) -> Filter {
    Filter::must([Condition::matches(F_KIND, kind.to_string())])
}

/// 组装资产过滤：kind / nsfw / tag 三个可选条件（INTERFACES.md §5）。
pub fn build_filter(
    kind_field: Option<&str>,
    nsfw: Option<bool>,
    tags: &[String],
) -> Option<Filter> {
    let mut must: Vec<Condition> = Vec::new();
    if let Some(k) = kind_field {
        must.push(Condition::matches(F_KIND, k.to_string()));
    }
    if let Some(flag) = nsfw {
        must.push(Condition::matches(F_NSFW, flag));
    }
    for tag in tags.iter().filter(|t| !t.trim().is_empty()) {
        must.push(Condition::matches(F_TAGS, tag.trim().to_string()));
    }
    if must.is_empty() {
        None
    } else {
        Some(Filter::must(must))
    }
}

// ---------------------------------------------------------------- 工具

fn sparse_vector(indices: &[u32], values: &[f32]) -> Vector {
    let pairs: Vec<(u32, f32)> = indices
        .iter()
        .copied()
        .zip(values.iter().copied())
        .collect();
    Vector::from(pairs)
}

pub fn point_id_to_i64(id: &PointId) -> i64 {
    match &id.point_id_options {
        Some(point_id::PointIdOptions::Num(n)) => *n as i64,
        Some(point_id::PointIdOptions::Uuid(_)) => -1,
        None => -1,
    }
}

/// payload 里的 row_id：asset 是 asset.id，prompt 是 prompt.id（不是点 id）。
pub fn payload_row_id(payload: &Value) -> Option<i64> {
    payload.get(F_ROW_ID).and_then(|v| v.as_i64())
}

pub fn payload_is_prompt(payload: &Value) -> bool {
    payload.get(F_KIND).and_then(|v| v.as_str()) == Some(KIND_PROMPT)
}

pub fn payload_asset_id(payload: &Value) -> Option<i64> {
    payload.get(F_ASSET_ID).and_then(|v| v.as_i64())
}

fn payload_to_json(payload: &HashMap<String, QValue>) -> Value {
    Value::from(Payload::from(payload.clone()))
}

fn facet_value_to_string(value: qdrant_client::qdrant::FacetValue) -> String {
    use qdrant_client::qdrant::facet_value::Variant;
    match value.variant {
        Some(Variant::StringValue(s)) => s,
        Some(Variant::IntegerValue(i)) => i.to_string(),
        Some(Variant::BoolValue(b)) => b.to_string(),
        _ => String::new(),
    }
}

fn extract_named_dense(vectors: &VectorsOutput, using: &str) -> Option<Vec<f32>> {
    match &vectors.vectors_options {
        Some(vectors_output::VectorsOptions::Vector(v)) => match &v.vector {
            Some(vector_output::Vector::Dense(d)) => Some(d.data.clone()),
            _ => None,
        },
        Some(vectors_output::VectorsOptions::Vectors(named)) => {
            named.vectors.get(using).and_then(|v| match &v.vector {
                Some(vector_output::Vector::Dense(d)) => Some(d.data.clone()),
                _ => None,
            })
        }
        None => None,
    }
}

fn vector_output_to_json(v: &VectorOutput) -> Value {
    match &v.vector {
        Some(vector_output::Vector::Dense(d)) => json!({ "dense": d.data }),
        Some(vector_output::Vector::Sparse(s)) => json!({ "indices": s.indices, "values": s.values }),
        _ => Value::Null,
    }
}

fn vectors_to_json(vectors: &VectorsOutput) -> Value {
    match &vectors.vectors_options {
        Some(vectors_output::VectorsOptions::Vector(v)) => vector_output_to_json(v),
        Some(vectors_output::VectorsOptions::Vectors(named)) => {
            let mut out = serde_json::Map::new();
            for (name, v) in &named.vectors {
                out.insert(name.clone(), vector_output_to_json(v));
            }
            Value::Object(out)
        }
        None => Value::Null,
    }
}

/// Qdrant 的 REST 端口是 6333、gRPC 端口是 6334。客户端只会走 gRPC，
/// 而用户很容易只记得 REST 那个地址，所以这里把 `:6333` 自动换成 `:6334`。
fn grpc_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if let Some(rest) = trimmed.strip_suffix(":6333") {
        let converted = format!("{rest}:6334");
        tracing::info!(from = %url, to = %converted, "Qdrant 地址是 REST 端口，自动改用 gRPC 端口");
        converted
    } else {
        trimmed.to_string()
    }
}
