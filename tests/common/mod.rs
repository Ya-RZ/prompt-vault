//! 集成测试共用的小工具。
//!
//! 这里只放「确定性伪向量」：测试不需要真实模型，只要同一 seed 每次得到同一向量、
//! 不同 seed 近似正交，就足以验证索引写入 / 检索 / 量化往返这些链路。

use sha2::{Digest, Sha256};

/// sha256 流式展开 → 均匀分布 → L2 归一化（与真实编码器一样输出单位向量）。
pub fn mock_dense(seed: &str, dim: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(dim);
    let mut counter: u32 = 0;
    while out.len() < dim {
        let mut hasher = Sha256::new();
        hasher.update(seed.as_bytes());
        hasher.update(counter.to_le_bytes());
        let digest = hasher.finalize();
        for chunk in digest.chunks(4) {
            if out.len() >= dim {
                break;
            }
            let mut b = [0u8; 4];
            b.copy_from_slice(chunk);
            let raw = u32::from_le_bytes(b) as f32 / u32::MAX as f32;
            out.push(raw * 2.0 - 1.0);
        }
        counter += 1;
    }
    let norm = out.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in out.iter_mut() {
            *v /= norm;
        }
    }
    out
}
