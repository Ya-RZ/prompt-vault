//! 以图搜图路径的回归测试：复现"用 retrieve 出来的向量查不到自己"的问题。
//!
//! 覆盖三个变量：向量来源（原始 vs retrieve）、filter（有/无）、rescore（开/关）。

mod common;
use common::mock_dense;
use prompt_vault::vector::{build_filter, QuantMode, VectorStore, KIND_ASSET, VEC_IMAGE};

const DIM: usize = 2048;

fn setup() -> (tempfile::TempDir, VectorStore) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("qdrant");
    let store = VectorStore::open(&dir, DIM, true, QuantMode::Off).unwrap();
    for i in 1..=3 {
        let dense = mock_dense(&format!("img-{i}"), DIM);
        store
            .upsert_asset(i, &dense, None, "fp", false, &[], i, "test")
            .unwrap();
    }
    store.flush().unwrap();
    (tmp, store)
}

#[test]
fn retrieve_then_query() {
    let (_tmp, store) = setup();
    let q = store.retrieve_vector(2, VEC_IMAGE).expect("retrieve 不应报错");
    let q = q.expect("资产 2 的 image 向量应该取得到");
    eprintln!("[debug] retrieve 出来的向量: len={} 前 3 个 = {:?}", q.len(), &q[..3.min(q.len())]);

    let raw = mock_dense("img-2", DIM);

    // 1) 原始向量 + 无 filter
    let a = store.search_dense(VEC_IMAGE, &raw, 10, None, false, None).unwrap();
    eprintln!("[debug] raw + no-filter + no-rescore → {} 条，top={:?}", a.len(), a.first().map(|x| x.0));

    // 2) retrieve 向量 + 无 filter
    let b = store.search_dense(VEC_IMAGE, &q, 10, None, false, None).unwrap();
    eprintln!("[debug] retrieved + no-filter + no-rescore → {} 条，top={:?}", b.len(), b.first().map(|x| x.0));

    // 3) retrieve 向量 + kind=asset filter
    let f = build_filter(Some(KIND_ASSET), None, &[]);
    let c = store.search_dense(VEC_IMAGE, &q, 10, f, false, None).unwrap();
    eprintln!("[debug] retrieved + kind-filter + no-rescore → {} 条，top={:?}", c.len(), c.first().map(|x| x.0));

    // 4) retrieve 向量 + filter + rescore（这正是 /api/search/image 的参数组合）
    let f = build_filter(Some(KIND_ASSET), None, &[]);
    let d = store.search_dense(VEC_IMAGE, &q, 40, f, true, Some(2.0)).unwrap();
    eprintln!("[debug] retrieved + kind-filter + rescore → {} 条，top={:?}", d.len(), d.first().map(|x| x.0));

    // 5) retrieve 向量 + nsfw=false filter
    let f = build_filter(Some(KIND_ASSET), Some(false), &[]);
    let e = store.search_dense(VEC_IMAGE, &q, 40, f, true, Some(2.0)).unwrap();
    eprintln!("[debug] retrieved + nsfw=false + rescore → {} 条", e.len());

    assert!(!a.is_empty(), "原始向量应能查到");
    assert_eq!(a[0].0, 2, "原始向量 top1 应是它自己");
    assert!(!b.is_empty(), "retrieve 出来的向量也应能查到（问题点）");
    assert_eq!(b[0].0, 2, "retrieve 向量 top1 应是它自己");
}
