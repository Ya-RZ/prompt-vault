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
use std::sync::{Arc, Mutex, Once};

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

/// 编码设备选择。
///
/// `auto` 是默认值：二进制带 `cuda` feature 且能初始化 0 号卡时用 GPU，否则静默回落 CPU。
/// 回落必须留日志 —— 否则「编了 CUDA 版却还是慢」会变成又一个查不出来现象。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceSpec {
    Auto,
    Cpu,
    Cuda(usize),
}

impl DeviceSpec {
    pub fn parse(raw: &str) -> Self {
        let s = raw.trim().to_ascii_lowercase();
        match s.as_str() {
            "" | "auto" => DeviceSpec::Auto,
            "cpu" => DeviceSpec::Cpu,
            "cuda" | "gpu" => DeviceSpec::Cuda(0),
            other => {
                for prefix in ["cuda:", "gpu:"] {
                    if let Some(idx) = other.strip_prefix(prefix) {
                        if let Ok(n) = idx.parse::<usize>() {
                            return DeviceSpec::Cuda(n);
                        }
                    }
                }
                DeviceSpec::Auto
            }
        }
    }

    pub fn as_str(&self) -> String {
        match self {
            DeviceSpec::Auto => "auto".to_string(),
            DeviceSpec::Cpu => "cpu".to_string(),
            DeviceSpec::Cuda(i) => format!("cuda:{i}"),
        }
    }
}

/// 真正向 candle 要设备。返回 `(设备, 写进指纹的标签)`。
fn resolve_device(spec: DeviceSpec) -> (Device, String) {
    match spec {
        DeviceSpec::Cpu => (Device::Cpu, "cpu".to_string()),
        DeviceSpec::Cuda(ordinal) => match cuda_device(ordinal) {
            Ok(dev) => (dev, format!("cuda:{ordinal}")),
            Err(e) => {
                tracing::warn!(
                    ordinal,
                    error = %e,
                    "指定的 CUDA 设备不可用（二进制是否带 cuda feature、驱动是否装好？），已回落到 CPU"
                );
                (Device::Cpu, "cpu".to_string())
            }
        },
        DeviceSpec::Auto => {
            if cfg!(feature = "cuda") {
                match cuda_device(0) {
                    Ok(dev) => {
                        tracing::info!("candle 使用 CUDA:0");
                        (dev, "cuda:0".to_string())
                    }
                    Err(e) => {
                        tracing::info!(error = %e, "没有可用的 CUDA 设备，candle 使用 CPU");
                        (Device::Cpu, "cpu".to_string())
                    }
                }
            } else {
                (Device::Cpu, "cpu".to_string())
            }
        }
    }
}

fn cuda_device(ordinal: usize) -> Result<Device, String> {
    #[cfg(feature = "cuda")]
    {
        Device::new_cuda(ordinal).map_err(|e| e.to_string())
    }
    #[cfg(not(feature = "cuda"))]
    {
        let _ = ordinal;
        Err("本二进制未启用 cuda feature".to_string())
    }
}

pub struct CandleEncoder {
    model_dir: PathBuf,
    /// HuggingFace 缓存根目录（HF_HOME）。**不再写进模型目录** ——
    /// 模型目录可能来自只读挂载（容器镜像 / 系统目录），在那里建目录会直接失败。
    cache_dir: PathBuf,
    max_length: usize,
    dtype: DType,
    device: Device,
    /// 实际生效的设备标签（cpu / cuda:0），进编码指纹
    device_label: String,
    /// 单张图片的字节上限（与 `--max-media-bytes` 同一口径）
    max_media_bytes: u64,
    /// 懒加载：第一次编码时才真正把权重 mmap 进来（2B 模型加载要几十秒）
    loaded: Mutex<Option<Arc<Qwen3VLEmbedding>>>,
}

impl CandleEncoder {
    pub fn new(
        model_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        max_length: usize,
        dtype: DType,
        device_spec: DeviceSpec,
        max_media_bytes: u64,
    ) -> Self {
        let (device, device_label) = resolve_device(device_spec);
        Self {
            model_dir: model_dir.into(),
            cache_dir: cache_dir.into(),
            max_length,
            dtype,
            device,
            device_label,
            max_media_bytes,
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

    /// 实际生效的设备标签（cpu / cuda:0）。health 与指纹都用它。
    pub fn device_label(&self) -> &str {
        &self.device_label
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// 编码指纹：必须把决定「向量落在哪个空间」的因素都写进去（开发文档 §10 铁律 1）。
    /// 与 Python 侧车的指纹不同 —— 这正是索引闸门要拦住混用的依据。
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|candle|dtype={}|dim={}|pool=lasttoken|norm=L2|max_pixels={}|max_length={}|backend=fastembed-7.1|device={}",
            MODEL_REPO_ID,
            self.dtype_name(),
            NATIVE_DIM,
            MAX_PIXELS,
            self.max_length,
            self.device_label
        )
    }

    /// 模型是否已加载（health 里用来区分「配置好了」与「已经能编码了」）
    pub fn is_loaded(&self) -> bool {
        self.loaded.lock().map(|slot| slot.is_some()).unwrap_or(false)
    }

    /// 同步加载（内部使用；调用方负责放进阻塞线程）。
    ///
    /// **整个加载过程持锁**：并发的第二个调用会阻塞等待，而不是再 mmap 一份 2B 权重。
    /// 加载要几十秒，重复加载会同时吃掉两份内存 —— 宁可让请求在这里排队。
    fn ensure_loaded(&self) -> ApiResult<Arc<Qwen3VLEmbedding>> {
        let mut slot = self
            .loaded
            .lock()
            .map_err(|_| ApiError::internal("编码器状态锁被污染，无法加载模型"))?;
        if let Some(enc) = slot.as_ref() {
            return Ok(Arc::clone(enc));
        }

        // 先把缓存目录准备好（离线的前提）
        let hf_home = prepare_hf_cache(&self.model_dir, &self.cache_dir)?;
        install_hf_env(&hf_home);

        tracing::info!(
            model_dir = %self.model_dir.display(),
            hf_home = %hf_home.display(),
            dtype = self.dtype_name(),
            device = %self.device_label,
            max_length = self.max_length,
            "开始加载 Qwen3-VL-Embedding-2B（candle）"
        );
        let began = std::time::Instant::now();
        let encoder =
            Qwen3VLEmbedding::from_hf(MODEL_REPO_ID, &self.device, self.dtype, self.max_length)
                .map_err(|e| {
                    ApiError::encoder_error(format!(
                        "candle 加载模型失败（模型目录 {}，设备 {}）: {e}",
                        self.model_dir.display(),
                        self.device_label
                    ))
                })?;
        tracing::info!(elapsed_s = began.elapsed().as_secs_f64(), "模型加载完成");

        let encoder = Arc::new(encoder);
        *slot = Some(Arc::clone(&encoder));
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
                    image_payloads.push(decode_image_input(image, self.max_media_bytes)?);
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
///
/// 两条路径都按 `max_bytes` 设上限：本地路径先看文件元数据再读，避免把一个
/// 几百 MB 的文件整个读进内存之后才拒绝；data URL 解码后也复查一次。
fn decode_image_input(raw: &str, max_bytes: u64) -> ApiResult<Vec<u8>> {
    if let Some(rest) = raw.strip_prefix("data:") {
        let payload = rest
            .split_once(',')
            .map(|(_, p)| p)
            .ok_or_else(|| ApiError::bad_request("data URL 缺少 base64 负载"))?;
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|e| ApiError::bad_request(format!("data URL 解码失败: {e}")))?;
        ensure_within_limit(bytes.len() as u64, max_bytes)?;
        return Ok(bytes);
    }
    let meta = std::fs::metadata(raw)
        .map_err(|e| ApiError::bad_request(format!("读取图片失败（{raw}）: {e}")))?;
    ensure_within_limit(meta.len(), max_bytes)?;
    std::fs::read(raw).map_err(|e| ApiError::bad_request(format!("读取图片失败（{raw}）: {e}")))
}

fn ensure_within_limit(len: u64, max_bytes: u64) -> ApiResult<()> {
    if len > max_bytes {
        return Err(ApiError::payload_too_large(format!(
            "图片 {len} 字节超过上限 {max_bytes} 字节（--max-media-bytes）"
        )));
    }
    Ok(())
}

/// 「强制离线」的进程级环境只设一次。
///
/// `HF_HOME` / `HF_HUB_OFFLINE` 是全局变量，设置本身就是进程副作用：
/// 每次加载都重复 `set_var` 既无意义，也会在 edition 2024 下变成 unsafe 调用。
static HF_ENV: Once = Once::new();

fn install_hf_env(hf_home: &Path) {
    HF_ENV.call_once(|| {
        std::env::set_var("HF_HOME", hf_home);
        std::env::set_var("HF_HUB_OFFLINE", "1");
        std::env::set_var("HF_HUB_DISABLE_TELEMETRY", "1");
    });
}

/// 把本地模型目录伪装成 HuggingFace 缓存结构，并返回 `$HF_HOME`。
///
/// 缓存根目录是 `cache_dir`（默认 `<data-dir>/hf-cache`），**不再写进模型目录**：
/// 模型目录可能来自只读挂载，在里面建 `.hf-home` 会直接失败。
///
/// 产物：
/// ```text
/// <cache_dir>/hub/models--Qwen--Qwen3-VL-Embedding-2B/
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
fn prepare_hf_cache(model_dir: &Path, cache_dir: &Path) -> ApiResult<PathBuf> {
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

    let hf_home = cache_dir.to_path_buf();
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

/// 目录看起来是不是一个完整模型目录。
///
/// 口径必须和 [`prepare_hf_cache`] 一致：这里放过、那里报错的组合最坑 ——
/// 启动时校验通过，直到第一次编码才因为少一个 `preprocessor_config.json` 失败。
fn looks_like_model_dir(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    REQUIRED_FILES.iter().all(|f| dir.join(f).is_file())
}

/// dtype 字符串 → candle 类型。默认 f32 最稳（CPU 上对 f16/bf16 的支持依赖后端）。
pub fn parse_dtype(name: &str) -> DType {
    match name.to_ascii_lowercase().as_str() {
        "f16" | "float16" | "half" => DType::F16,
        "bf16" | "bfloat16" => DType::BF16,
        _ => DType::F32,
    }
}
