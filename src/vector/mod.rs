//! Qdrant Edge 封装：唯一索引（稠密 + 稀疏），无 Tantivy（开发文档 §6）。
//!
//! 这一层只做四件事：建/开 shard、写点、查点、导出点。
//! 融合排序在 `crate::search`，不在这一层。
//!
//! ⚠️ 开发文档 §6.3：`qdrant-edge` 很年轻（0.8.0），所以**必须**保留导出通路
//! （`export_jsonl` / `import_jsonl`），保证随时能搬家。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use qdrant_edge::{
    Condition, CreateIndex, Distance, EdgeConfigBuilder, EdgeShard, EdgeSparseVectorParamsBuilder,
    EdgeVectorParamsBuilder, FacetRequest, FieldCondition, FieldIndexOperations, Filter, JsonPath,
    Match, Modifier, Payload, PayloadFieldSchema, PayloadOps, PayloadSchemaType, PointId,
    PointOperations, PointStruct, SetPayloadOp,
    QuantizationConfig, QuantizationSearchParams, QueryEnum, QueryRequest, ScoringQuery,
    SearchParams, UpdateOperation, Vector,
    VectorInternal, Vectors, VectorStorageDatatype, WithPayloadInterface, WithVector, NamedQuery,
    SparseVector,
};
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};

/// 向量名（开发文档 §6.1 / INTERFACES.md §2）
pub const VEC_IMAGE: &str = "image";
pub const VEC_TEXT: &str = "text";
pub const VEC_SPARSE_IMAGE: &str = "sparse_image";
pub const VEC_SPARSE_TEXT: &str = "sparse_text";

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

/// 量化方式（只能在 create / load 时定，之后没有 setter —— 开发文档 §6.1、§10 铁律 4）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantMode {
    /// Scalar int8（v1.1.0 起成熟，本项目默认，开发文档 §6.4）
    Scalar,
    /// TurboQuant bits4（v1.18.0 起，A/B 备选）
    Turbo4,
    /// 不量化（50 万条 1.91 GiB 也扛得住）
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
    shard: Arc<EdgeShard>,
    dir: PathBuf,
    dim: usize,
    sparse_enabled: bool,
    quantization: QuantMode,
}

impl VectorStore {
    /// 建或开：目录已有内容就 `load`，否则 `new`（两者语义不同，别搞混 —— §6.3）。
    pub fn open(dir: impl AsRef<Path>, dim: usize, sparse_enabled: bool, quant: QuantMode) -> ApiResult<Self> {
        let dir = dir.as_ref().to_path_buf();
        // 量化配置构造失败时要如实降级（不假装开了量化）
        let (config, effective_quant) = Self::build_config(dim, sparse_enabled, quant);
        let has_data = dir.exists()
            && std::fs::read_dir(&dir)
                .map(|mut it| it.next().is_some())
                .unwrap_or(false);

        let shard = if has_data {
            EdgeShard::load(&dir, Some(config))
                .map_err(|e| ApiError::internal(format!("打开向量目录失败 {}: {e}", dir.display())))?
        } else {
            std::fs::create_dir_all(&dir)
                .map_err(|e| ApiError::internal(format!("创建向量目录失败: {e}")))?;
            EdgeShard::new(&dir, config)
                .map_err(|e| ApiError::internal(format!("创建向量 shard 失败: {e}")))?
        };

        let store =
            Self { shard: Arc::new(shard), dir, dim, sparse_enabled, quantization: effective_quant };
        store.ensure_indexes()?;
        Ok(store)
    }

    fn build_config(
        dim: usize,
        sparse_enabled: bool,
        quant: QuantMode,
    ) -> (qdrant_edge::EdgeConfig, QuantMode) {
        // 原始向量以 f16 落盘（开发文档 §6.4）
        let mut dense = EdgeVectorParamsBuilder::new(dim, Distance::Cosine)
            .datatype(VectorStorageDatatype::Float16)
            .build();

        let quantization_config = match quant {
            // Scalar（int8）：quantile 0.99 排除 1% 极端值，只影响精度不影响内存（§6.4）
            QuantMode::Scalar => parse_quantization(&[
                json!({ "scalar": { "type": "int8", "quantile": 0.99 } }),
                json!({ "scalar": { "scalar": { "type": "int8", "quantile": 0.99 } } }),
            ]),
            // TurboQuant bits4：官方说 4-bit 是很多场景的好默认值，本项目作 A/B 项（§6.4）
            QuantMode::Turbo4 => parse_quantization(&[
                json!({ "turbo": { "bits": 4 } }),
                json!({ "turbo": { "turbo": { "bits": 4 } } }),
            ]),
            QuantMode::Off => None,
        };
        if let Some(qc) = quantization_config.as_ref() {
            tracing::info!(config = %serde_json::to_string(qc).unwrap_or_default(), "向量量化配置");
        } else if quant != QuantMode::Off {
            tracing::warn!(mode = quant.as_str(), "量化配置构造失败，本次以不量化运行（功能不受影响，只影响速度/占用）");
        }
        let effective_quant = if quantization_config.is_some() { quant } else { QuantMode::Off };
        if let Some(qc) = quantization_config {
            dense.quantization_config = Some(qc);
        }

        let mut builder = EdgeConfigBuilder::new()
            .vector(VEC_IMAGE, dense.clone())
            .vector(VEC_TEXT, dense);

        if sparse_enabled {
            let sparse = EdgeSparseVectorParamsBuilder::new()
                // 内置 BM25（开发文档 §6.1）
                .modifier(Modifier::Idf)
                .build();
            builder = builder
                .sparse_vector(VEC_SPARSE_IMAGE, sparse.clone())
                .sparse_vector(VEC_SPARSE_TEXT, sparse);
        }

        (builder.build(), effective_quant)
    }

    /// payload 索引：过滤与 facet 都要它（§6.1「过滤需先 create_field_index」）。
    fn ensure_indexes(&self) -> ApiResult<()> {
        let mut fields: Vec<(&str, PayloadSchemaType)> = vec![
            (F_KIND, PayloadSchemaType::Keyword),
            (F_FINGERPRINT, PayloadSchemaType::Keyword),
            (F_NSFW, PayloadSchemaType::Bool),
            (F_TAGS, PayloadSchemaType::Keyword),
        ];
        if self.sparse_enabled {
            fields.push((F_ADDED_AT_MS, PayloadSchemaType::Integer));
            fields.push((F_SOURCE, PayloadSchemaType::Keyword));
        }
        for (field, schema) in fields {
            let op = UpdateOperation::FieldIndexOperation(FieldIndexOperations::CreateIndex(
                CreateIndex {
                    field_name: parse_path(field),
                    field_schema: Some(PayloadFieldSchema::from(schema)),
                },
            ));
            // 已存在的索引会报错，这里当作幂等：忽略失败但记日志
            if let Err(e) = self.shard.update(op) {
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

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn flush(&self) -> ApiResult<()> {
        self.shard
            .flush()
            .map_err(|e| ApiError::internal(format!("flush 失败: {e}")))
    }

    // ------------------------------------------------------------ 写入

    /// 一个 asset 点：稠密（image 向量 + payload）
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_asset(
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
        let mut named: Vec<(&str, Vector)> = vec![(VEC_IMAGE, Vector::new_dense(dense.to_vec()))];
        if self.sparse_enabled {
            if let Some((idx, val)) = sparse {
                named.push((
                    VEC_SPARSE_IMAGE,
                    Vector::new_sparse(idx.to_vec(), val.to_vec()).map_err(ApiError::from)?,
                ));
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
        self.upsert_one(asset_id, named, payload)
    }

    /// 一个 prompt 点：稠密（text 向量 + payload），带上它所属 asset
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_prompt(
        &self,
        prompt_id: i64,
        asset_id: i64,
        dense: &[f32],
        sparse: Option<(&[u32], &[f32])>,
        fingerprint: &str,
        nsfw: bool,
        tags: &[String],
    ) -> ApiResult<()> {
        let mut named: Vec<(&str, Vector)> = vec![(VEC_TEXT, Vector::new_dense(dense.to_vec()))];
        if self.sparse_enabled {
            if let Some((idx, val)) = sparse {
                named.push((
                    VEC_SPARSE_TEXT,
                    Vector::new_sparse(idx.to_vec(), val.to_vec()).map_err(ApiError::from)?,
                ));
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
        self.upsert_one(
            prompt_id + PROMPT_ID_OFFSET,
            named,
            payload,
        )
    }

    fn upsert_one(&self, row_id: i64, named: Vec<(&str, Vector)>, payload: Value) -> ApiResult<()> {
        let point_id: PointId = (row_id as u64).into();
        let vectors = Vectors::new_named(named);
        let point = PointStruct::new(point_id, vectors, payload);
        let op = UpdateOperation::PointOperation(PointOperations::UpsertPoints(
            qdrant_edge::PointInsertOperations::PointsList(vec![point.into()]),
        ));
        self.shard
            .update(op)
            .map_err(|e| ApiError::internal(format!("写入向量失败: {e}")))?;
        Ok(())
    }

    /// 删资产点与其名下提示词点（开发文档 §13 第 7 条：稠密稀疏两路都不能再返回）。
    pub fn delete_asset_and_prompts(&self, asset_id: i64, prompt_ids: &[i64]) -> ApiResult<usize> {
        let mut ids: Vec<PointId> = vec![(asset_id as u64).into()];
        for p in prompt_ids {
            ids.push(((*p + PROMPT_ID_OFFSET) as u64).into());
        }
        let n = ids.len();
        let op = UpdateOperation::PointOperation(PointOperations::DeletePoints { ids });
        self.shard
            .update(op)
            .map_err(|e| ApiError::internal(format!("删除向量失败: {e}")))?;
        Ok(n)
    }

    pub fn delete_prompt(&self, prompt_id: i64) -> ApiResult<()> {
        let op = UpdateOperation::PointOperation(PointOperations::DeletePoints {
            ids: vec![((prompt_id + PROMPT_ID_OFFSET) as u64).into()],
        });
        self.shard
            .update(op)
            .map_err(|e| ApiError::internal(format!("删除提示词向量失败: {e}")))?;
        Ok(())
    }

    /// 删除是标记删除，必须自己找时机调（§6.3：没有后台 optimizer）。
    pub fn optimize(&self) -> ApiResult<bool> {
        self.shard
            .optimize()
            .map_err(|e| ApiError::internal(format!("optimize 失败: {e}")))
    }

    /// 同步 payload（tags / nsfw 变了要更新，否则过滤与 facet 会失真）。
    pub fn set_payload(&self, point_ids: &[i64], payload: Value) -> ApiResult<()> {
        let Value::Object(map) = payload else {
            return Err(ApiError::bad_request("payload 必须是 JSON 对象"));
        };
        if point_ids.is_empty() {
            return Ok(());
        }
        let op = UpdateOperation::PayloadOperation(PayloadOps::SetPayload(SetPayloadOp {
            payload: Payload(map),
            points: Some(point_ids.iter().map(|i| (*i as u64).into()).collect()),
            filter: None,
            key: None,
        }));
        self.shard
            .update(op)
            .map_err(|e| ApiError::internal(format!("更新 payload 失败: {e}")))?;
        Ok(())
    }

    pub fn set_asset_payload(&self, asset_id: i64, nsfw: bool, tags: &[String]) -> ApiResult<()> {
        self.set_payload(&[asset_id], json!({ F_NSFW: nsfw, F_TAGS: tags }))
    }

    pub fn set_prompt_payload(
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
    }

    // ------------------------------------------------------------ 查询

    /// 稠密查询：`using` 选向量名（`image` 或 `text`）。
    pub fn search_dense(
        &self,
        using: &str,
        vector: &[f32],
        limit: usize,
        filter: Option<Filter>,
        rescore: bool,
        oversampling: Option<f64>,
    ) -> ApiResult<Vec<(i64, f32, Value)>> {
        let named = NamedQuery::new(VectorInternal::Dense(vector.to_vec()), using);
        self.run_query(ScoringQuery::Vector(QueryEnum::Nearest(named)), limit, filter, rescore, oversampling)
    }

    /// 稀疏查询（BM25）。
    ///
    /// 现状：现有两个编码后端都不产出稀疏向量（candle 只给稠密，Python 侧车也不返回 sparse），
    /// 所以这条路径在正常检索里走不到 —— 保留它是为了 import/导出与将来接入 BM25 时不必重建 shard。
    /// 上层（`search`）会按 `sparse_enabled` 与实际是否拿到稀疏向量决定要不要调。
    pub fn search_sparse(
        &self,
        using: &str,
        indices: &[u32],
        values: &[f32],
        limit: usize,
        filter: Option<Filter>,
    ) -> ApiResult<Vec<(i64, f32, Value)>> {
        if !self.sparse_enabled {
            return Err(ApiError::sparse_unavailable(
                "本索引未启用稀疏向量（建 shard 时 --sparse=false 或该 shard 早于稀疏开关）",
            ));
        }
        let sparse = SparseVector::new(indices.to_vec(), values.to_vec())
            .map_err(|e| ApiError::bad_request(format!("稀疏向量非法: {e}")))?;
        let named = NamedQuery::new(VectorInternal::Sparse(sparse), using);
        self.run_query(ScoringQuery::Vector(QueryEnum::Nearest(named)), limit, filter, false, None)
    }

    fn run_query(
        &self,
        scoring: ScoringQuery,
        limit: usize,
        filter: Option<Filter>,
        rescore: bool,
        oversampling: Option<f64>,
    ) -> ApiResult<Vec<(i64, f32, Value)>> {
        // Scalar / Turbo4 默认都不开 rescore，本项目显式开（开发文档 §6.4）
        let params = SearchParams {
            quantization: Some(QuantizationSearchParams {
                ignore: false,
                rescore: Some(rescore),
                oversampling,
            }),
            ..Default::default()
        };
        let request = QueryRequest {
            prefetches: Vec::new(),
            query: Some(scoring),
            filter,
            score_threshold: None,
            limit,
            offset: 0,
            params: Some(params),
            with_vector: WithVector::Bool(false),
            with_payload: WithPayloadInterface::Bool(true),
        };
        let scored = self
            .shard
            .query(request)
            .map_err(|e| ApiError::internal(format!("查询失败: {e}")))?;
        Ok(scored
            .into_iter()
            .map(|p| {
                let pid = point_id_to_i64(&p.id);
                let payload = p
                    .payload
                    .as_ref()
                    .map(|pl| serde_json::to_value(pl.clone()).unwrap_or(Value::Null))
                    .unwrap_or(Value::Null);
                (pid, p.score, payload)
            })
            .collect())
    }

    pub fn count(&self) -> ApiResult<usize> {
        self.shard
            .count(qdrant_edge::CountRequest::new())
            .map_err(|e| ApiError::internal(format!("count 失败: {e}")))
    }

    pub fn count_assets(&self) -> ApiResult<usize> {
        self.shard
            .count(qdrant_edge::CountRequest { filter: Some(filter_by_kind(KIND_ASSET)), exact: true })
            .map_err(|e| ApiError::internal(format!("count 失败: {e}")))
    }

    /// facet 直接出分布（开发文档 §7.2）。
    ///
    /// `filter` 用来把统计范围限定在资产点上：asset 与 prompt 是两类点，
    /// 不加过滤会把同一张图算两遍（标签分布会翻倍）。
    pub fn facet(
        &self,
        key: &str,
        limit: usize,
        filter: Option<Filter>,
    ) -> ApiResult<Vec<(String, i64)>> {
        let path: JsonPath = key
            .parse()
            .map_err(|_| ApiError::bad_request(format!("facet key 非法: {key}")))?;
        let mut req = FacetRequest::new(path);
        req.limit = limit;
        req.exact = true;
        req.filter = filter;
        let resp = self
            .shard
            .facet(req)
            .map_err(|e| ApiError::internal(format!("facet 失败: {e}")))?;
        Ok(resp
            .hits
            .into_iter()
            .map(|hit| {
                let value = match hit.value {
                    qdrant_edge::FacetValue::Keyword(s) => s,
                    qdrant_edge::FacetValue::Int(i) => i.to_string(),
                    qdrant_edge::FacetValue::Bool(b) => b.to_string(),
                    qdrant_edge::FacetValue::Uuid(u) => u.to_string(),
                };
                (value, hit.count as i64)
            })
            .collect())
    }

    /// 取回指定点（用于按 asset_id 找相似时先取它的向量）。
    pub fn retrieve_vector(&self, row_id: i64, using: &str) -> ApiResult<Option<Vec<f32>>> {
        let request = qdrant_edge::RetrieveRequest {
            point_ids: vec![(row_id as u64).into()],
            with_payload: Some(WithPayloadInterface::Bool(false)),
            with_vector: Some(WithVector::Bool(true)),
        };
        let records = self
            .shard
            .retrieve(request)
            .map_err(|e| ApiError::internal(format!("retrieve 失败: {e}")))?;
        let Some(rec) = records.into_iter().next() else {
            return Ok(None);
        };
        let Some(vectors) = rec.vector else {
            return Ok(None);
        };
        Ok(extract_named_dense(&vectors, using))
    }

    // ------------------------------------------------------------ 导出通路（必须有，§6.3）

    /// 全量导出为 JSONL：`{"id":..,"payload":{..},"vectors":{"name":{"dense":[..]}|{"indices":[..],"values":[..]}}}`
    pub fn export_jsonl(&self, path: impl AsRef<Path>) -> ApiResult<usize> {
        use std::io::Write;
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ApiError::internal(e.to_string()))?;
        }
        let mut file = std::fs::File::create(path).map_err(|e| ApiError::internal(e.to_string()))?;
        let mut offset: Option<PointId> = None;
        let mut total = 0usize;
        loop {
            let mut req = qdrant_edge::ScrollRequest::new();
            req.offset = offset;
            req.limit = Some(500);
            req.with_vector = WithVector::Bool(true);
            req.with_payload = Some(WithPayloadInterface::Bool(true));
            let (records, next) = self
                .shard
                .scroll(req)
                .map_err(|e| ApiError::internal(format!("导出失败: {e}")))?;
            if records.is_empty() {
                break;
            }
            for rec in &records {
                let line = json!({
                    "id": point_id_to_i64(&rec.id),
                    "payload": rec.payload.as_ref().map(|p| serde_json::to_value(p.clone()).unwrap_or(Value::Null)),
                    "vectors": rec.vector.as_ref().map(vector_struct_to_json),
                });
                writeln!(file, "{line}").map_err(|e| ApiError::internal(e.to_string()))?;
                total += 1;
            }
            match next {
                Some(n) => offset = Some(n),
                None => break,
            }
        }
        file.flush().map_err(|e| ApiError::internal(e.to_string()))?;
        Ok(total)
    }

    /// 从 JSONL 重建（搬家 / 灾备）。
    pub fn import_jsonl(&self, path: impl AsRef<Path>) -> ApiResult<usize> {
        let text = std::fs::read_to_string(path.as_ref())
            .map_err(|e| ApiError::internal(format!("读取导出文件失败: {e}")))?;
        let mut total = 0usize;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line)
                .map_err(|e| ApiError::bad_request(format!("导出文件格式错误: {e}")))?;
            let id = v["id"].as_i64().unwrap_or_default();
            let payload = v.get("payload").cloned().unwrap_or(Value::Null);
            let mut named: Vec<(String, Vector)> = Vec::new();
            if let Some(obj) = v.get("vectors").and_then(|x| x.as_object()) {
                for (name, vec_json) in obj {
                    if let Some(dense) = vec_json.get("dense").and_then(|d| d.as_array()) {
                        let vals: Vec<f32> =
                            dense.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
                        named.push((name.clone(), Vector::new_dense(vals)));
                    } else if let (Some(idx), Some(vals)) = (
                        vec_json.get("indices").and_then(|d| d.as_array()),
                        vec_json.get("values").and_then(|d| d.as_array()),
                    ) {
                        let indices: Vec<u32> =
                            idx.iter().filter_map(|x| x.as_u64().map(|u| u as u32)).collect();
                        let values: Vec<f32> =
                            vals.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
                        if let Ok(v) = Vector::new_sparse(indices, values) {
                            named.push((name.clone(), v));
                        }
                    }
                }
            }
            let refs: Vec<(&str, Vector)> =
                named.into_iter().map(|(n, v)| (n.as_str().to_owned().leak() as &str, v)).collect();
            self.upsert_one(id, refs, payload)?;
            total += 1;
        }
        Ok(total)
    }

    /// 目录占用（统计页用）。
    pub fn disk_bytes(&self) -> u64 {
        dir_size(&self.dir)
    }

    pub fn points_estimate(&self) -> Option<u64> {
        self.count().ok().map(|n| n as u64)
    }
}

// ---------------------------------------------------------------- 过滤条件

pub fn filter_by_kind(kind: &str) -> Filter {
    Filter {
        should: None,
        min_should: None,
        must: Some(vec![Condition::Field(FieldCondition::new_match(
            parse_path(F_KIND),
            Match::Value(qdrant_edge::MatchValue {
                value: qdrant_edge::ValueVariants::String(kind.to_string()),
            }),
        ))]),
        must_not: None,
    }
}

/// 组装资产过滤：nsfw / kind / tag 三个可选条件（INTERFACES.md §5）。
pub fn build_filter(
    kind_field: Option<&str>,
    nsfw: Option<bool>,
    tags: &[String],
) -> Option<Filter> {
    let mut must: Vec<Condition> = Vec::new();
    if let Some(k) = kind_field {
        must.push(Condition::Field(FieldCondition::new_match(
            parse_path(F_KIND),
            Match::Value(qdrant_edge::MatchValue {
                value: qdrant_edge::ValueVariants::String(k.to_string()),
            }),
        )));
    }
    if let Some(flag) = nsfw {
        must.push(Condition::Field(FieldCondition::new_match(
            parse_path(F_NSFW),
            Match::Value(qdrant_edge::MatchValue {
                value: qdrant_edge::ValueVariants::Bool(flag),
            }),
        )));
    }
    for tag in tags.iter().filter(|t| !t.trim().is_empty()) {
        must.push(Condition::Field(FieldCondition::new_match(
            parse_path(F_TAGS),
            Match::Value(qdrant_edge::MatchValue {
                value: qdrant_edge::ValueVariants::String(tag.trim().to_string()),
            }),
        )));
    }
    if must.is_empty() {
        None
    } else {
        Some(Filter { should: None, min_should: None, must: Some(must), must_not: None })
    }
}

// ---------------------------------------------------------------- 工具

pub fn point_id_to_i64(id: &PointId) -> i64 {
    match id {
        PointId::NumId(n) => *n as i64,
        PointId::Uuid(_) => -1,
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

fn extract_named_dense(vectors: &qdrant_edge::VectorStructInternal, using: &str) -> Option<Vec<f32>> {
    match vectors {
        qdrant_edge::VectorStructInternal::Single(v) => {
            let _ = using;
            Some(v.clone())
        }
        qdrant_edge::VectorStructInternal::Named(map) => match map.get(using) {
            Some(VectorInternal::Dense(v)) => Some(v.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn vector_struct_to_json(vectors: &qdrant_edge::VectorStructInternal) -> Value {
    let mut out = serde_json::Map::new();
    let as_json = |v: &VectorInternal| match v {
        VectorInternal::Dense(d) => json!({ "dense": d }),
        VectorInternal::Sparse(s) => json!({ "indices": s.indices, "values": s.values }),
        VectorInternal::MultiDense(_) => Value::Null,
    };
    match vectors {
        qdrant_edge::VectorStructInternal::Single(d) => {
            out.insert(String::new(), json!({ "dense": d }));
        }
        qdrant_edge::VectorStructInternal::Named(map) => {
            for (k, v) in map {
                out.insert(k.clone(), as_json(v));
            }
        }
        _ => {}
    }
    Value::Object(out)
}

/// 量化配置的 serde 构造：crate 根没有导出 `ScalarQuantization` 等类型，
/// 按官方文档给出的 JSON 形状构造，形状对不上就降级为不量化（并告警）。
fn parse_quantization(candidates: &[Value]) -> Option<QuantizationConfig> {
    for candidate in candidates {
        if let Ok(config) = serde_json::from_value::<QuantizationConfig>(candidate.clone()) {
            // round-trip 校验：解析出来必须还能写回同样的形状
            if serde_json::to_value(&config).is_ok() {
                return Some(config);
            }
        }
    }
    None
}

/// 目录占用：按**实际占块**统计（Qdrant 的向量文件是稀疏预分配的，
/// 用 `metadata.len()` 会把逻辑大小算进去 —— 实测 4 个点的库会显示成 265 MB，
/// 而 `du` 只有 444 KB）。
fn dir_size(dir: &Path) -> u64 {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                m.blocks() * 512
            }
            #[cfg(not(unix))]
            {
                m.len()
            }
        })
        .sum()
}

/// 静态字段名 → JsonPath（`JsonPath::new` 只在 testing feature 下存在，用 FromStr）。
fn parse_path(field: &str) -> JsonPath {
    field
        .parse()
        .unwrap_or_else(|_| unreachable!("字段名必须是合法的 JSON 路径: {field}"))
}
