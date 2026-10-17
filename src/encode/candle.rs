//! 进程内编码后端：用 candle 直接加载 Qwen3-VL-Embedding-2B，替代 Python 侧车。
//!
//! 为什么可以这么做（开发文档 §2 需要更新的一条）：
//! 那里说「自己用 Rust 重写推理会得到不同的向量空间」——这句话仍然成立，
//! 但它约束的是「同一索引里混用两种实现」，不是「不能用 Rust 实现」。
//! 只要**整个索引用同一个后端重建**，内部就是自洽的；指纹机制（§9）会拦住混用。
//!
//! 三个硬约束：
//! 1. **强制离线**：只读本地模型，绝不联网。做法是把模型目录伪装成 HuggingFace
//!    的缓存结构（`$HF_HOME/hub/models--.../snapshots/<rev>/`），再设 `HF_HUB_OFFLINE=1`。
//! 2. **max_pixels = 1,843,200**（官方参考脚本口径）。fastembed 读的是模型目录里的
//!    `preprocessor_config.json`（原值 1,310,720），所以我们在构造缓存时写一份
//!    改写过的副本 —— **不动原始模型文件**。
//! 3. **只支持 text-only / image-only**：fastembed 尚未实现图文混合输入，
//!    遇到 `{text, image}` 同时存在的项直接报错，不静默降级。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use candle_core::{DType, Device};
use fastembed::Qwen3VLEmbedding;

use crate::error::{ApiError, ApiResult};
use crate::types::EmbedInputItem;

use super::EncodeOutcome;

/// HuggingFace 上的仓库 id（同时决定缓存目录名 `models--Qwen--Qwen3-VL-Embedding-2B`）
pub const MODEL_REPO_ID: &str = "Qwen/Qwen3-VL-Embedding-2B";
/// 缓存里的 revision 名。我们自己造，只要 `refs/main` 与 `snapshots/<rev>` 一致即可。
const CACHE_REVISION: &str = "local";
/// 官方参考脚本口径（开发文档 §4 / §11）
pub const MAX_PIXELS: i64 = 1_843_200;
pub const MIN_PIXELS: i64 = 4_096;
pub const NATIVE_DIM: usize = 2_048;

/// 加载模型必须存在的文件（缺一个就直接报错，而不是等到加载到一半）
const REQUIRED_FILES: &[&str] = &[
    "config.json",
    "preprocessor_config.json",
    "tokenizer.json",
    "model.safetensors",
];

pub struct CandleEncoder {
    model_dir: PathBuf,
    max_length: usize,
    dtype: DType,
    device: Device,
    /// 懒加载：第一次编码时才真正把权重 mmap 进来（2B 模型加载要几十秒）
    loaded: Mutex<Option<Arc<Qwen3VLEmbedding>>>,
}

impl CandleEncoder {
    pub fn new(model_dir: impl Into<PathBuf>, max_length: usize, dtype: DType) -> Self {
        Self {
            model_dir: model_dir.into(),
            max_length,
            dtype,
            // 目前只在本机 CPU 上构建；CUDA 由 candle 的 cuda feature 在目标机上提供
            device: Device::Cpu,
            loaded: Mutex::new(None),
        }
    }

    pub fn model_dir(&self) -> &Path {
        &self.model_dir
    }

    pub fn dtype_name(&self) -> &'static str {
        match self.dtype {
            DType::F32 => "f32",
            DType::F16 => "f16",
            DType::BF16 => "bf16",
            _ => "unknown",
        }
    }

    /// 编码指纹：必须把决定「向量落在哪个空间」的因素都写进去（开发文档 §10 铁律 1）。
    /// 与 Python 侧车的指纹不同 —— 这正是索引闸门要拦住混用的依据。
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|candle|dtype={}|dim={}|pool=lasttoken|norm=L2|max_pixels={}|max_length={}|backend=fastembed-7.1|device=cpu",
            "Qwen3-VL-Embedding-2B",
            self.dtype_name(),
            NATIVE_DIM,
            MAX_PIXELS,
            self.max_length
        )
    }

    /// 模型是否已加载（health 里用来区分「配置好了」与「已经能编码了」）
    pub fn is_loaded(&self) -> bool {
        self.loaded.lock().map(|slot| slot.is_some()).unwrap_or(false)
    }

    /// 同步加载（内部使用；调用方负责放进阻塞线程）
    fn ensure_loaded(&self) -> ApiResult<Arc<Qwen3VLEmbedding>> {
        if let Ok(slot) = self.loaded.lock() {
            if let Some(enc) = slot.as_ref() {
                return Ok(Arc::clone(enc));
            }
        }

        // 先把缓存目录准备好（离线的前提）
        let hf_home = prepare_hf_cache(&self.model_dir)?;
        // 这两行是「强制离线」的全部秘密：cache 目录 + 禁止联网
        std::env::set_var("HF_HOME", &hf_home);
        std::env::set_var("HF_HUB_OFFLINE", "1");
        std::env::set_var("HF_HUB_DISABLE_TELEMETRY", "1");

        tracing::info!(
            model_dir = %self.model_dir.display(),
            hf_home = %hf_home.display(),
            dtype = self.dtype_name(),
            max_length = self.max_length,
            "开始加载 Qwen3-VL-Embedding-2B（candle）"
        );
        let began = std::time::Instant::now();
        let encoder =
            Qwen3VLEmbedding::from_hf(MODEL_REPO_ID, &self.device, self.dtype, self.max_length)
                .map_err(|e| {
                    ApiError::encoder_error(format!(
                        "candle 加载模型失败（模型目录 {}）: {e}",
                        self.model_dir.display()
                    ))
                })?;
        tracing::info!(elapsed_s = began.elapsed().as_secs_f64(), "模型加载完成");

        let encoder = Arc::new(encoder);
        if let Ok(mut slot) = self.loaded.lock() {
            *slot = Some(Arc::clone(&encoder));
        }
        Ok(encoder)
    }

    /// 编码一批输入（在 spawn_blocking 里跑，避免卡住 async 运行时）
    pub fn embed_blocking(&self, input: &[EmbedInputItem]) -> ApiResult<Vec<EncodeOutcome>> {
        // 按 fastembed 的能力分组：它只支持纯文本与纯图片
        let mut text_slots: Vec<usize> = Vec::new();
        let mut text_payloads: Vec<String> = Vec::new();
        let mut image_slots: Vec<usize> = Vec::new();
        let mut image_payloads: Vec<Vec<u8>> = Vec::new();

        for (idx, item) in input.iter().enumerate() {
            match (&item.text, &item.image) {
                (Some(text), None) => {
                    text_slots.push(idx);
                    text_payloads.push(text.clone());
                }
                (None, Some(image)) => {
                    image_slots.push(idx);
                    image_payloads.push(decode_image_input(image)?);
                }
                (Some(_), Some(_)) => {
                    return Err(ApiError::bad_request(
                        "candle 后端暂不支持图文混合输入（fastembed 的 Qwen3VLEmbedding 尚未实现）；\
                         请把文本与图片分别编码 —— 本项目的检索路径本来也是分开存两个向量名的",
                    )
                    .with_detail(serde_json::json!({ "index": idx })))
                }
                (None, None) => {
                    return Err(ApiError::bad_request("输入项既没有 text 也没有 image")
                        .with_detail(serde_json::json!({ "index": idx })))
                }
            }
        }

        let encoder = self.ensure_loaded()?;
        let fingerprint = self.fingerprint();
        let mut out: Vec<Option<EncodeOutcome>> = (0..input.len()).map(|_| None).collect();

        if !text_payloads.is_empty() {
            let vectors = encoder
                .embed_texts(&text_payloads)
                .map_err(|e| ApiError::encoder_error(format!("candle 文本编码失败: {e}")))?;
            for (slot, vector) in text_slots.iter().zip(vectors) {
                out[*slot] = Some(EncodeOutcome {
                    dense: vector,
                    sparse: None, // 稀疏向量本来就不由编码器提供（见 encode/mod.rs 的说明）
                    resized: None,
                    vision_tokens: None,
                    kind: Some("text".to_string()),
                    fingerprint: fingerprint.clone(),
                });
            }
        }

        if !image_payloads.is_empty() {
            let refs: Vec<&[u8]> = image_payloads.iter().map(Vec::as_slice).collect();
            let vectors = encoder
                .embed_image_bytes(&refs)
                .map_err(|e| ApiError::encoder_error(format!("candle 图片编码失败: {e}")))?;
            for (slot, vector) in image_slots.iter().zip(vectors) {
                out[*slot] = Some(EncodeOutcome {
                    dense: vector,
                    sparse: None,
                    // fastembed 不回传 resize 后的尺寸与视觉 token 数（开发文档 §5.1 说这两个
                    // 数字是排查质量的第一现场，所以这里如实置空而不是编一个值）
                    resized: None,
                    vision_tokens: None,
                    kind: Some("image".to_string()),
                    fingerprint: fingerprint.clone(),
                });
            }
        }

        out.into_iter()
            .map(|slot| {
                slot.ok_or_else(|| ApiError::internal("编码结果缺失（内部错误：槽位未填充）"))
            })
            .collect::<ApiResult<Vec<_>>>()
    }
}

/// 从输入串里取出图片字节：支持 `data:image/...;base64,...` 与本地路径。
fn decode_image_input(raw: &str) -> ApiResult<Vec<u8>> {
    if let Some(rest) = raw.strip_prefix("data:") {
        let payload = rest
            .split_once(',')
            .map(|(_, p)| p)
            .ok_or_else(|| ApiError::bad_request("data URL 缺少 base64 负载"))?;
        use base64::Engine;
        return base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|e| ApiError::bad_request(format!("data URL 解码失败: {e}")));
    }
    std::fs::read(raw).map_err(|e| ApiError::bad_request(format!("读取图片失败（{raw}）: {e}")))
}

/// 把本地模型目录伪装成 HuggingFace 缓存结构，并返回 `$HF_HOME`。
///
/// 产物：
/// ```text
/// <model_dir>/.hf-home/hub/models--Qwen--Qwen3-VL-Embedding-2B/
///   refs/main              ← 内容 "local"
///   snapshots/local/
///     config.json          ← 硬链接（不复制，不占额外空间）
///     preprocessor_config.json ← **改写过的副本**（max_pixels = 1,843,200）
///     tokenizer.json       ← 硬链接
///     model.safetensors    ← 硬链接（4.2 GB 秒级完成）
/// ```
///
/// 为什么不直接改原文件：那个目录被 Python 侧车共用，动它会改变侧车行为；
/// 改写只作用在我们自己的缓存副本上。
fn prepare_hf_cache(model_dir: &Path) -> ApiResult<PathBuf> {
    for file in REQUIRED_FILES {
        let p = model_dir.join(file);
        if !p.is_file() {
            return Err(ApiError::encoder_error(format!(
                "模型文件缺失：{}（模型目录 {}）。\
                 需要 config.json / preprocessor_config.json / tokenizer.json / model.safetensors",
                p.display(),
                model_dir.display()
            )));
        }
    }

    let hf_home = model_dir.join(".hf-home");
    let snapshot = hf_home
        .join("hub")
        .join("models--Qwen--Qwen3-VL-Embedding-2B")
        .join("snapshots")
        .join(CACHE_REVISION);
    std::fs::create_dir_all(&snapshot)
        .map_err(|e| ApiError::internal(format!("创建缓存目录失败: {e}")))?;

    for file in REQUIRED_FILES {
        let src = model_dir.join(file);
        let dst = snapshot.join(file);
        if dst.exists() {
            continue;
        }
        if *file == "preprocessor_config.json" {
            // 只在这份副本里改口径，原始模型文件保持不动
            let raw = std::fs::read(&src)
                .map_err(|e| ApiError::internal(format!("读取 preprocessor_config 失败: {e}")))?;
            let mut cfg: serde_json::Value = serde_json::from_slice(&raw)
                .map_err(|e| ApiError::internal(format!("preprocessor_config 不是合法 JSON: {e}")))?;
            cfg["max_pixels"] = serde_json::json!(MAX_PIXELS);
            cfg["min_pixels"] = serde_json::json!(MIN_PIXELS);
            let pretty = serde_json::to_vec_pretty(&cfg)
                .map_err(|e| ApiError::internal(format!("序列化 preprocessor_config 失败: {e}")))?;
            std::fs::write(&dst, pretty)
                .map_err(|e| ApiError::internal(format!("写入 preprocessor_config 副本失败: {e}")))?;
            tracing::info!(
                max_pixels = MAX_PIXELS,
                "已为 candle 缓存生成 preprocessor_config 副本（1,843,200 口径）"
            );
        } else {
            // 硬链接：同一文件系统上瞬间完成且不占额外空间；跨设备时退回复制
            if let Err(e) = std::fs::hard_link(&src, &dst) {
                tracing::warn!(file = *file, error = %e, "硬链接失败，改为复制");
                std::fs::copy(&src, &dst)
                    .map_err(|e| ApiError::internal(format!("复制模型文件失败: {e}")))?;
            }
        }
    }

    let refs = hf_home
        .join("hub")
        .join("models--Qwen--Qwen3-VL-Embedding-2B")
        .join("refs");
    std::fs::create_dir_all(&refs)
        .map_err(|e| ApiError::internal(format!("创建 refs 目录失败: {e}")))?;
    std::fs::write(refs.join("main"), CACHE_REVISION)
        .map_err(|e| ApiError::internal(format!("写入 refs/main 失败: {e}")))?;

    Ok(hf_home)
}

/// 模型目录解析：显式指定优先，其次按约定路径查找（与 Python 侧车的查找顺序对齐）。
///
/// 顺序：
/// 1. `--model-dir` / `PV_MODEL_DIR`（**显式指定就必须有效**，不合法直接报错，不回落）
/// 2. `<exe 所在目录>/models/qwen3-vl-embedding-2b`
/// 3. `<当前工作目录>/models/qwen3-vl-embedding-2b`
/// 4. `<当前工作目录>/embed-server/models/qwen3-vl-embedding-2b`
/// 5. `<当前工作目录>/../embed-server/models/qwen3-vl-embedding-2b`
pub fn resolve_model_dir(explicit: Option<&Path>) -> ApiResult<PathBuf> {
    if let Some(dir) = explicit {
        if looks_like_model_dir(dir) {
            return Ok(dir.to_path_buf());
        }
        return Err(ApiError::encoder_error(format!(
            "显式指定的模型目录不合法：{}（缺少 config.json / model.safetensors 等文件）",
            dir.display()
        ))
        .with_detail(serde_json::json!({ "model_dir": dir.display().to_string() })));
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("models").join("qwen3-vl-embedding-2b"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("models").join("qwen3-vl-embedding-2b"));
        candidates.push(
            cwd.join("embed-server")
                .join("models")
                .join("qwen3-vl-embedding-2b"),
        );
        if let Some(parent) = cwd.parent() {
            candidates.push(
                parent
                    .join("embed-server")
                    .join("models")
                    .join("qwen3-vl-embedding-2b"),
            );
        }
    }

    let tried: Vec<String> = candidates
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    for candidate in &candidates {
        if looks_like_model_dir(candidate) {
            return Ok(candidate.clone());
        }
    }

    Err(ApiError::encoder_error(format!(
        "找不到可用的模型目录。已尝试：{}。请用 --model-dir 或 PV_MODEL_DIR 指定",
        tried.join("；")
    ))
    .with_detail(serde_json::json!({ "tried": tried })))
}

/** 供 health 检查复用：目录看起来是不是一个完整模型目录 */
pub fn looks_like_model_dir_public(dir: &Path) -> bool {
    looks_like_model_dir(dir)
}

fn looks_like_model_dir(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    ["config.json", "tokenizer.json", "model.safetensors"]
        .iter()
        .all(|f| dir.join(f).is_file())
}

/// dtype 字符串 → candle 类型。默认 f32 最稳（CPU 上对 f16/bf16 的支持依赖后端）。
pub fn parse_dtype(name: &str) -> DType {
    match name.to_ascii_lowercase().as_str() {
        "f16" | "float16" | "half" => DType::F16,
        "bf16" | "bfloat16" => DType::BF16,
        _ => DType::F32,
    }
}
