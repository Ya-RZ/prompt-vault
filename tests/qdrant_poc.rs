//! Qdrant Edge PoC 七步（开发文档 §6.3 / §13 第 9 条）：
//!
//! 1. 建 shard → 2. 写 1000 点 → 3. 关进程（drop）→ 4. 重开 → 5. 查得到
//!
//! 6. 删几个点 → optimize 后查不到
//! 7. 导出全部数据 → 重建
//!
//! 为什么必须有这个测试：`qdrant-edge` 只有 0.8.0（crates.io 总下载 1.7 万次），
//! 用在核心存储上，**在跑通这一条之前不能把迁移脚本写成只能进不能出**。

use std::path::Path;

mod common;
use common::mock_dense;
use prompt_vault::vector::{build_filter, QuantMode, VectorStore, KIND_ASSET, VEC_IMAGE, VEC_SPARSE_IMAGE};

const DIM: usize = 64;
const POINTS: i64 = 1000;

fn pseudo_dense(i: i64) -> Vec<f32> {
    mock_dense(&format!("asset-{i}"), DIM)
}

fn pseudo_sparse(i: i64) -> (Vec<u32>, Vec<f32>) {
    // 每个点一个独特的 token + 一个公共 token（index 0），验证 BM25 召回。
    // 注意：SparseVector::new 要求 indices 唯一（重复会直接报 Validation error）。
    (vec![i as u32 + 1, 0], vec![1.0, 0.5])
}

fn write_points(store: &VectorStore) {
    for i in 1..=POINTS {
        let dense = pseudo_dense(i);
        let (idx, val) = pseudo_sparse(i);
        store
            .upsert_asset(
                i,
                &dense,
                Some((idx.as_slice(), val.as_slice())),
                "poc-fingerprint",
                i % 100 == 0,
                &[format!("tag-{}", i % 7)],
                i,
                "poc",
            )
            .expect("写入点失败");
    }
}

#[test]
fn poc_seven_steps() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path().join("qdrant");
    let export_path = tmp.path().join("export.jsonl");
    let rebuilt_dir = tmp.path().join("qdrant-rebuilt");

    // ---- 步骤 1 + 2：建 shard 并写 1000 点（稀疏也写，验证 BM25 共存）----
    {
        let store = VectorStore::open(&dir, DIM, true, QuantMode::Off).expect("1) 建 shard");
        assert!(store.dir().exists(), "shard 目录应已创建");
        write_points(&store);
        store.flush().expect("flush");

        // 同进程先确认写得进去
        assert_eq!(store.count().expect("count"), POINTS as usize, "写入后点数应为 1000");
        assert!(store.sparse_enabled(), "稀疏向量应已启用");

        let hits = store
            .search_dense(VEC_IMAGE, &pseudo_dense(42), 5, None, false, None)
            .expect("稠密查询");
        assert!(!hits.is_empty(), "稠密查询应返回结果");
        eprintln!("[PoC] 步骤 1-2 完成：写入 {POINTS} 点，稠密 top1 = {}", hits[0].0);
    } // ★ 步骤 3：drop → EdgeShard 在 Drop 里 flush（这就是"关进程"）

    // ---- 步骤 4 + 5：重开，数据还在（持久化）----
    let store = VectorStore::open(&dir, DIM, true, QuantMode::Off).expect("4) 重开 shard");
    let after_reopen = store.count().expect("count");
    assert_eq!(after_reopen, POINTS as usize, "重开后点数必须还是 1000（持久化失败）");
    let hits = store
        .search_dense(VEC_IMAGE, &pseudo_dense(42), 5, None, false, None)
        .expect("重开后查询");
    assert!(!hits.is_empty(), "重开后应查得到");
    assert_eq!(hits[0].0, 42, "最相似的应该是它自己（精确扫描 + 余弦）");
    eprintln!("[PoC] 步骤 3-5 完成：drop 后重开，点数 = {after_reopen}，top1 = {}", hits[0].0);

    // 稀疏（BM25，Modifier::Idf）也要能在重开后召回
    let sparse_hits = store
        .search_sparse(VEC_SPARSE_IMAGE, &[0], &[0.5], 5, None)
        .expect("稀疏查询");
    assert!(!sparse_hits.is_empty(), "稀疏路应有结果（公共 token=0）");
    eprintln!("[PoC] 稀疏路 top1 = {}（score={:.4}）", sparse_hits[0].0, sparse_hits[0].1);

    // payload 过滤 + facet 也顺带验证
    let filtered = store
        .search_dense(
            VEC_IMAGE,
            &pseudo_dense(100),
            5,
            build_filter(Some(KIND_ASSET), Some(true), &[]),
            false,
            None,
        )
        .expect("带过滤的查询");
    assert!(!filtered.is_empty(), "nsfw=true 过滤后应有结果");
    let tags = store
        .facet("tags", 10, build_filter(Some(KIND_ASSET), None, &[]))
        .expect("facet");
    assert!(!tags.is_empty(), "facet 应返回标签分布");
    eprintln!("[PoC] facet(tags) 前 3 项 = {:?}", &tags[..tags.len().min(3)]);

    // ---- 步骤 6：删几个点 → optimize() 后查不到 ----
    let victims: Vec<i64> = (1..=5).collect();
    for id in &victims {
        store.delete_asset_and_prompts(*id, &[]).expect("删除点");
    }
    let deleted_ok = store.optimize().expect("optimize");
    eprintln!("[PoC] 步骤 6：删除 {victims:?} 后 optimize 返回 {deleted_ok}");
    store.flush().expect("flush");

    let remaining = store.count().expect("count");
    assert_eq!(
        remaining,
        (POINTS - victims.len() as i64) as usize,
        "optimize 后点数应减少"
    );
    for id in &victims {
        let hits = store
            .search_dense(VEC_IMAGE, &pseudo_dense(*id), 1, None, false, None)
            .expect("删除后查询");
        let still_there = hits.iter().any(|(pid, _, _)| pid == id);
        assert!(!still_there, "被删的点 {id} 不应再被查到");
    }

    // ---- 步骤 7：导出全部数据 → 重建 ----
    let exported = store.export_jsonl(&export_path).expect("导出");
    assert_eq!(exported, remaining, "导出的点数应与剩余点数一致");
    assert!(export_path.exists(), "导出文件应存在");
    let size = std::fs::metadata(&export_path).map(|m| m.len()).unwrap_or(0);
    assert!(size > 0, "导出文件不应为空");
    eprintln!("[PoC] 步骤 7：导出 {exported} 点到 {}（{size} 字节）", export_path.display());

    {
        // 重建到一个**全新的目录**，模拟"搬家"
        let rebuilt = VectorStore::open(&rebuilt_dir, DIM, true, QuantMode::Off).expect("新建目标 shard");
        let imported = rebuilt.import_jsonl(&export_path).expect("导入");
        assert_eq!(imported, exported, "导入点数应与导出点数一致");
        rebuilt.flush().expect("flush");
        assert_eq!(rebuilt.count().expect("count"), remaining, "重建后点数应一致");

        // 重建后检索必须仍然可用（向量与 payload 都搬过去了）
        let hits = rebuilt
            .search_dense(VEC_IMAGE, &pseudo_dense(42), 3, None, false, None)
            .expect("重建后查询");
        assert_eq!(hits[0].0, 42, "重建后 top1 仍是它自己");
        let payload_ok = hits[0].2.get("fingerprint").and_then(|v| v.as_str()) == Some("poc-fingerprint");
        assert!(payload_ok, "payload 也要一起搬过去");
        eprintln!("[PoC] 重建完成：{} 点，top1 = {}", imported, hits[0].0);
    }

    // 重建后的目录再开一次，持久化仍然成立
    let rebuilt_again = VectorStore::open(&rebuilt_dir, DIM, true, QuantMode::Off).expect("重开重建目录");
    assert_eq!(rebuilt_again.count().expect("count"), remaining, "重建目录重启后点数应保持");
    eprintln!("[PoC] 七步全部通过 ✅");
}

/// 量化开着的 shard 要能建、能写、能查（开发文档 §6.4：Scalar int8 是默认档）
#[test]
fn scalar_quantization_roundtrip() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path().join("qdrant-quant");
    {
        let store = VectorStore::open(&dir, DIM, false, QuantMode::Scalar).expect("建量化 shard");
        assert_eq!(
            store.quantization().as_str(),
            "scalar-int8",
            "scalar 量化配置应生效（构造失败会降级为 none）"
        );
        for i in 1..=200 {
            store
                .upsert_asset(i, &pseudo_dense(i), None, "fp", false, &[], i, "q")
                .expect("写入");
        }
        store.flush().expect("flush");
    }
    let store = VectorStore::open(&dir, DIM, false, QuantMode::Scalar).expect("重开");
    assert_eq!(store.count().expect("count"), 200);
    // rescore 开着查（开发文档 §6.4：Scalar 默认不开 rescore，本项目显式开）
    let hits = store
        .search_dense(VEC_IMAGE, &pseudo_dense(77), 10, None, true, Some(2.0))
        .expect("量化后查询");
    assert!(!hits.is_empty());
    assert_eq!(hits[0].0, 77, "开了 rescore 之后 top1 应精确命中");
    eprintln!("[PoC] 量化档位 top1 = {}（rescore=true, oversampling=2.0）", hits[0].0);
}

/// `new` 与 `load` 语义不同，别搞混（开发文档 §6.3）——目录已有数据时必须走 load。
#[test]
fn reopened_shard_keeps_config() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path().join("qdrant-cfg");
    {
        let store = VectorStore::open(&dir, DIM, true, QuantMode::Off).expect("建");
        store.upsert_asset(1, &pseudo_dense(1), None, "fp", false, &[], 1, "s").expect("写");
        store.flush().expect("flush");
    }
    // 第二次打开时目录已有数据 → 必须 load 成功，且能读到原来的点
    let store = VectorStore::open(&dir, DIM, true, QuantMode::Off).expect("load");
    assert_eq!(store.count().expect("count"), 1);
    assert!(Path::new(&dir).exists());
}
