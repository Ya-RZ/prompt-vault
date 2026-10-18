//! 进程内编码后端：用 candle 直接加载 Qwen3-VL-Embedding-2B，替代原先的 Python 侧车。
//!
//! 为什么可以这么做（开发文档 §2 需要更新的一条）：
//! 那里说「自己用 Rust 重写推理会得到不同的向量空间」——这句话仍然成立，
//! 但它约束的是「同一索引里混用两种实现」，不是「不能用 Rust 实现」。
//! 只要**整个索引用同一个后端重建**，内部就是自洽的；指纹机制（§9）会拦住混用。
//!
//! 三个硬约束：
//! 1. **强制离线**：只读本地模型，绝不联网。做法是把模型目录伪装成 hf-hub 的缓存结构
//!    （`$HF_HOME/models--.../snapshots/<rev>/` + `refs/main`）。
//!
//!    ⚠️ hf-hub 0.5 **没有实现** `HF_HUB_OFFLINE`：`repo.get()` 一旦缓存未命中就会去下载。
//!    真正保证「不联网」的是**缓存命中**，所以 `prepare_hf_cache` 必须先逐个校验文件齐全，
//!    缺一个就在本地报错，而不是把决定权交给 hf-hub。
//! 2. **max_pixels 默认 3,145,728**（`--max-pixels` 可覆盖）。fastembed 读的是模型目录里的
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
/// 默认的像素安全区：约 1774×1774，对应约 3072 个视觉 token
/// （factor = patch_size 16 × merge_size 2 = 32，1 token = 1024 像素）。
/// 可用 `--max-pixels` 覆盖；硬上限是 max_length 推导出的约 8,366,080。
pub const MAX_PIXELS: i64 = 3_145_728;
pub const MIN_PIXELS: i64 = 4_096;
pub const NATIVE_DIM: usize = 2_048;

/// 金标自检用的三段固定文本：全中文 / 全英文 / 中英混合。
///
/// 每次模型加载后都跑同样这三段：既验证「编码是否可重复」（与基线向量的余弦），
/// 也验证「tokenizer 对三种文本都正常」（维度、有限值、区分度）。
const GOLDEN_TEXTS: [(&str, &str); 3] = [
    ("zh", "一只橘色的猫坐在窗台上，阳光透过玻璃照在它的背上。"),
    ("en", "A ginger cat sits on the windowsill while sunlight falls across its back."),
    ("mix", "一只 ginger cat 坐在 window sill 上，阳光照在它的 back。"),
];

/// 同一指纹下同一输入应逐位相同，这里留一点浮点/调度余量。
pub const GOLDEN_THRESHOLD: f64 = 0.9999;
/// 三段文本两两之间的余弦必须低于这个值 —— 否则说明编码器退化成了常数向量。
const GOLDEN_MAX_INTER_SIMILARITY: f64 = 0.99;

/// 模型加载后自检的结果。
#[derive(Debug, Clone)]
pub struct GoldenCheck {
    /// passed | failed | created | rebuilt
    pub status: &'static str,
    pub passed: bool,
    /// 三段里与基线余弦的最小值（无基线时为 NaN）
    pub min_cosine: f64,
    pub detail: String,
    pub checked_at: String,
}

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

/// 按显存选像素上限（未显式指定 `--max-pixels` 时使用）。
///
/// 依据实测（RTX 4060 Laptop 8 GiB + f16，同一张图）：
/// - 786,432 → 1.5 s/张
/// - 1,048,576 → 2.8 s/张（甜点）
/// - 1,310,720 → 10 s/张（显存贴顶，分配抖动）
/// - 1,572,864 → 24 s/张
///
/// 所以按显存保守分档：宁可少一点细节，也不掉进 10 倍慢的区间。
pub fn auto_max_pixels(device_label: &str) -> (u64, String) {
    let ordinal = device_label
        .strip_prefix("cuda:")
        .and_then(|s| s.parse::<usize>().ok());
    let Some(ordinal) = ordinal else {
        return (1_048_576, "CPU 推理，取保守档".to_string());
    };
    let Some(info) = cuda_inventory().into_iter().find(|d| d.ordinal == ordinal) else {
        return (1_048_576, format!("cuda:{ordinal} 信息不可读，取保守档"));
    };
    let gib = info.total_mem_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let value = if gib < 10.0 {
        1_048_576
    } else if gib < 24.0 {
        1_572_864
    } else {
        3_145_728
    };
    (value, format!("显存 {gib:.1} GiB"))
}

/// 一张 NVIDIA 卡的静态信息。只有带 `cuda` feature 且驱动可用时才可能非空。
#[derive(Debug, Clone)]
pub struct CudaDeviceInfo {
    pub ordinal: usize,
    pub name: String,
    pub compute_capability: (i32, i32),
    pub total_mem_bytes: usize,
}

/// 枚举机器上所有 NVIDIA 设备。
///
/// 集成显卡（Intel / AMD）不会出现在这张表里 —— CUDA 的设备列表只由 NVIDIA 驱动提供，
/// 所以「双显卡笔记本选到核显」在 CUDA 体系下不成立。返回空表表示没有可用 NVIDIA 卡，
/// 调用方应回落到 CPU。
pub fn cuda_inventory() -> Vec<CudaDeviceInfo> {
    #[cfg(feature = "cuda")]
    {
        use cudarc::driver::CudaContext;
        let count = match CudaContext::device_count() {
            Ok(n) => n,
            Err(e) => {
                tracing::debug!(error = %e, "枚举 CUDA 设备失败（多半是没有 NVIDIA 驱动）");
                return Vec::new();
            }
        };
        let mut out = Vec::with_capacity(count.max(0) as usize);
        for ordinal in 0..count {
            match CudaContext::new(ordinal as usize) {
                Ok(ctx) => {
                    let name = ctx.name().unwrap_or_else(|_| format!("cuda:{ordinal}"));
                    let compute_capability = ctx.compute_capability().unwrap_or((0, 0));
                    let total_mem_bytes = ctx.total_mem().unwrap_or(0);
                    out.push(CudaDeviceInfo {
                        ordinal: ordinal as usize,
                        name,
                        compute_capability,
                        total_mem_bytes,
                    });
                }
                Err(e) => tracing::warn!(ordinal, error = %e, "CUDA 设备初始化失败，跳过"),
            }
        }
        out
    }
    #[cfg(not(feature = "cuda"))]
    {
        Vec::new()
    }
}

/// auto 的选卡规则：算力优先，同算力比显存。
///
/// 为什么要这样排：机器上可能同时插着一张亮机卡和一张计算卡，驱动的 0 号不保证是
/// 能用的那张；而纯按显存排，又会把大显存的老架构排到新卡前面。
fn best_cuda(inventory: &[CudaDeviceInfo]) -> Option<&CudaDeviceInfo> {
    inventory.iter().max_by_key(|d| {
        let (major, minor) = d.compute_capability;
        let cc = (major.max(0) as u64) * 100 + (minor.max(0) as u64);
        cc * (1u64 << 40) + d.total_mem_bytes as u64
    })
}

/// 真正向 candle 要设备。返回 `(设备, 指纹标签, 设备名)`。
fn resolve_device(spec: DeviceSpec) -> (Device, String, Option<String>) {
    match spec {
        DeviceSpec::Cpu => (Device::Cpu, "cpu".to_string(), None),
        DeviceSpec::Cuda(ordinal) => match cuda_device(ordinal) {
            Ok(dev) => {
                let name = cuda_inventory()
                    .into_iter()
                    .find(|d| d.ordinal == ordinal)
                    .map(|d| d.name);
                (dev, format!("cuda:{ordinal}"), name)
            }
            Err(e) => {
                tracing::warn!(
                    ordinal,
                    error = %e,
                    "指定的 CUDA 设备不可用（二进制是否带 cuda feature、驱动是否装好？），已回落到 CPU"
                );
                (Device::Cpu, "cpu".to_string(), None)
            }
        },
        DeviceSpec::Auto => {
            if cfg!(feature = "cuda") {
                // 先把整机的 NVIDIA 设备列出来：多卡机器上，「到底用了哪张」必须能从日志看到
                let inventory = cuda_inventory();
                for d in &inventory {
                    tracing::info!(
                        ordinal = d.ordinal,
                        name = %d.name,
                        sm = format!("{}{}", d.compute_capability.0, d.compute_capability.1),
                        vram_gb = (d.total_mem_bytes as f64 / 1e9 * 10.0).round() / 10.0,
                        "发现 NVIDIA 设备"
                    );
                }
                // 清单可用就挑最好的；清单为空时再直接试 0 号
                // （极少数环境驱动拒绝枚举，但仍能建上下文）
                let (target, target_name) = match best_cuda(&inventory) {
                    Some(d) => (d.ordinal, Some(d.name.clone())),
                    None => (0, None),
                };
                match cuda_device(target) {
                    Ok(dev) => {
                        let label = format!("cuda:{target}");
                        tracing::info!(
                            device = %label,
                            name = target_name.as_deref().unwrap_or("未知"),
                            "candle 使用 CUDA"
                        );
                        (dev, label, target_name)
                    }
                    Err(e) => {
                        tracing::info!(error = %e, "没有可用的 CUDA 设备，candle 使用 CPU");
                        (Device::Cpu, "cpu".to_string(), None)
                    }
                }
            } else {
                (Device::Cpu, "cpu".to_string(), None)
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
    /// 设备名（如 NVIDIA GeForce RTX 4060），只用于展示，**不进指纹**
    /// —— 换一张同型号卡不该导致索引失效
    device_name: Option<String>,
    /// 单张图片的字节上限（与 `--max-media-bytes` 同一口径）
    max_media_bytes: u64,
    /// 宽高比上限（长边/短边），超限直接拒绝编码；对应 `--max-aspect-ratio`
    max_aspect_ratio: f64,
    /// 像素安全区上限（超过则先等比缩小）；对应 `--max-pixels`
    max_pixels: u64,
    /// 懒加载：第一次编码时才真正把权重 mmap 进来（2B 模型加载要几十秒）
    loaded: Mutex<Option<Arc<Qwen3VLEmbedding>>>,
    /// 加载后自检的结果（每进程一次）
    golden: Mutex<Option<GoldenCheck>>,
}

/// 构造 [`CandleEncoder`] 的一组参数。
///
/// 单独抽出来是为了避免 `new` 参数过多（8 个），也方便以后加开关时不用改调用方签名。
#[derive(Debug, Clone)]
pub struct CandleOptions {
    /// HuggingFace 缓存根目录（HF_HOME）
    pub cache_dir: PathBuf,
    /// 序列长度上限（8,192）
    pub max_length: usize,
    /// 权重 dtype
    pub dtype: DType,
    /// 计算设备选择
    pub device: DeviceSpec,
    /// 单张图片字节上限
    pub max_media_bytes: u64,
    /// 宽高比上限（0 = 不检查）
    pub max_aspect_ratio: f64,
    /// 像素安全区上限；`None` = 按显存自动选择（见 [`auto_max_pixels`]）
    pub max_pixels: Option<u64>,
}

impl CandleEncoder {
    pub fn new(model_dir: impl Into<PathBuf>, opts: CandleOptions) -> Self {
        let model_dir: PathBuf = model_dir.into();
        let (device, device_label, device_name) = resolve_device(opts.device);

        // 未显式指定时按显存分档：8GB 卡上 1,048,576 是甜点，再往上会让显存贴顶，
        // 单张耗时从 2.8s 掉到 10–24s（实测）。
        let max_pixels = match opts.max_pixels {
            Some(v) => v,
            None => {
                let (value, reason) = auto_max_pixels(&device_label);
                tracing::info!(
                    max_pixels = value,
                    reason = %reason,
                    "未指定 --max-pixels，按显存自动选择"
                );
                value
            }
        };

        // 提示模型原始配置里的 max_pixels 不生效（真正生效的是缓存副本 / CLI 值）
        warn_if_max_pixels_overridden(&model_dir, max_pixels);

        // 显式指定了过高的值、而显存偏小时给一条警告
        if let Some(ordinal) = device_label
            .strip_prefix("cuda:")
            .and_then(|s| s.parse::<usize>().ok())
        {
            if let Some(info) = cuda_inventory().into_iter().find(|d| d.ordinal == ordinal) {
                let vram_gib = info.total_mem_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                if max_pixels > 1_048_576 && vram_gib < 12.0 {
                    tracing::warn!(
                        vram_gb = format!("{vram_gib:.1}"),
                        max_pixels,
                        "显存偏小：当前像素上限可能让显存贴顶，编码速度会下降数倍；\
                         8GB 级显卡建议 --max-pixels 1048576"
                    );
                }
            }
        }
        Self {
            model_dir,
            cache_dir: opts.cache_dir,
            max_length: opts.max_length,
            dtype: opts.dtype,
            device,
            device_label,
            device_name,
            max_media_bytes: opts.max_media_bytes,
            max_aspect_ratio: opts.max_aspect_ratio,
            max_pixels,
            loaded: Mutex::new(None),
            golden: Mutex::new(None),
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

    /// 展示用描述：`cuda:0 (NVIDIA GeForce RTX 4060)` / `cpu`。
    pub fn device_display(&self) -> String {
        match &self.device_name {
            Some(name) => format!("{} ({name})", self.device_label),
            None => self.device_label.clone(),
        }
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// 序列长度上限（health 上报用）
    pub fn max_length(&self) -> usize {
        self.max_length
    }

    /// 像素安全区上限（health 上报用）
    pub fn max_pixels(&self) -> u64 {
        self.max_pixels
    }

    /// 编码指纹：必须把决定「向量落在哪个空间」的因素都写进去（开发文档 §10 铁律 1）。
    /// 与被移除的 Python 实现不同源，这正是索引闸门要拦住混用的依据。
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|candle|dtype={}|dim={}|pool=lasttoken|norm=L2|max_pixels={}|max_length={}|backend=fastembed-7.1|device={}",
            MODEL_REPO_ID,
            self.dtype_name(),
            NATIVE_DIM,
            self.max_pixels,
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
        let hf_home = prepare_hf_cache(&self.model_dir, &self.cache_dir, self.max_pixels)?;
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
        // 加载成功后立刻跑一次金标自检：模型坏了/权重不对，在这里就暴露，
        // 而不是等到一大堆资产都编码完才发现向量不可比。
        let check = self.run_golden_check(&encoder);
        if let Ok(mut slot) = self.golden.lock() {
            *slot = Some(check.clone());
        }
        match check.status {
            "failed" => tracing::error!(detail = %check.detail, "模型自检未通过"),
            "created" | "rebuilt" => tracing::warn!(detail = %check.detail, "金标基线已建立"),
            _ => tracing::info!(min_cosine = check.min_cosine, "模型自检通过"),
        }
        *slot = Some(Arc::clone(&encoder));
        Ok(encoder)
    }

    /// 最近一次自检结果（未加载模型时为 None）。
    pub fn golden_check(&self) -> Option<GoldenCheck> {
        self.golden.lock().ok().and_then(|g| g.clone())
    }

    /// 三段固定文本的金标自检。
    ///
    /// 判定：
    /// 1. 三段都要能编码成功，维度为 2048，且不含 NaN/Inf；
    /// 2. 三段两两余弦必须低于 [`GOLDEN_MAX_INTER_SIMILARITY`]（否则是常数向量退化）；
    /// 3. 若基线存在且指纹一致，则每段与基线的余弦都要 ≥ [`GOLDEN_THRESHOLD`]；
    /// 4. 基线不存在或指纹已变（换模型/精度/设备/像素口径）→ 重建基线并标记。
    fn run_golden_check(&self, encoder: &Qwen3VLEmbedding) -> GoldenCheck {
        let checked_at = crate::db::now_rfc3339();
        let texts: Vec<&str> = GOLDEN_TEXTS.iter().map(|(_, t)| *t).collect();
        let vectors = match encoder.embed_texts(&texts) {
            Ok(v) => v,
            Err(e) => {
                return GoldenCheck {
                    status: "failed",
                    passed: false,
                    min_cosine: f64::NAN,
                    detail: format!("三段文本编码失败: {e}"),
                    checked_at,
                }
            }
        };

        // ---- 1. 维度与有限值 ----
        for ((label, _), v) in GOLDEN_TEXTS.iter().zip(&vectors) {
            if v.len() != NATIVE_DIM {
                return GoldenCheck {
                    status: "failed",
                    passed: false,
                    min_cosine: f64::NAN,
                    detail: format!("{label} 向量维度为 {}，期望 {NATIVE_DIM}", v.len()),
                    checked_at,
                };
            }
            if v.iter().any(|x| !x.is_finite()) {
                return GoldenCheck {
                    status: "failed",
                    passed: false,
                    min_cosine: f64::NAN,
                    detail: format!("{label} 向量含 NaN/Inf"),
                    checked_at,
                };
            }
        }

        // ---- 2. 区分度：三段之间不能太像 ----
        let mut worst_inter = 0.0f64;
        for i in 0..vectors.len() {
            for j in (i + 1)..vectors.len() {
                let c = cosine(&vectors[i], &vectors[j]);
                worst_inter = worst_inter.max(c);
            }
        }
        if worst_inter >= GOLDEN_MAX_INTER_SIMILARITY {
            return GoldenCheck {
                status: "failed",
                passed: false,
                min_cosine: f64::NAN,
                detail: format!(
                    "三段文本两两余弦最高 {worst_inter:.6} ≥ {GOLDEN_MAX_INTER_SIMILARITY}，                     疑似编码器退化为常数向量"
                ),
                checked_at,
            };
        }

        // ---- 3. 与基线比对 ----
        let path = self.cache_dir.join(GOLDEN_BASELINE_FILE);
        let expected_fp = self.fingerprint();
        match load_baseline(&path) {
            Some(base) if base.fingerprint == expected_fp => {
                let mut min_cos = f64::INFINITY;
                let mut mismatched: Vec<String> = Vec::new();
                for ((label, _), v) in GOLDEN_TEXTS.iter().zip(&vectors) {
                    match base.vectors.get(*label) {
                        Some(b) if b.len() == v.len() => {
                            let c = cosine(v, b);
                            min_cos = min_cos.min(c);
                            if c < GOLDEN_THRESHOLD {
                                mismatched.push(format!("{label}={c:.6}"));
                            }
                        }
                        _ => mismatched.push(format!("{label}=基线缺失")),
                    }
                }
                if mismatched.is_empty() {
                    GoldenCheck {
                        status: "passed",
                        passed: true,
                        min_cosine: min_cos,
                        detail: format!(
                            "zh/en/mix 三段与基线一致（最小余弦 {min_cos:.6}，互相关 {worst_inter:.4}）"
                        ),
                        checked_at,
                    }
                } else {
                    GoldenCheck {
                        status: "failed",
                        passed: false,
                        min_cosine: min_cos,
                        detail: format!(
                            "向量与基线不一致（阈值 {GOLDEN_THRESHOLD}）：{}",
                            mismatched.join("，")
                        ),
                        checked_at,
                    }
                }
            }
            other => {
                // 无基线，或指纹变了（换模型/精度/设备/像素口径）→ 重建
                let status = if other.is_some() { "rebuilt" } else { "created" };
                let mut map = std::collections::BTreeMap::new();
                for ((label, _), v) in GOLDEN_TEXTS.iter().zip(&vectors) {
                    map.insert((*label).to_string(), v.clone());
                }
                let base = GoldenBaseline {
                    fingerprint: expected_fp,
                    created_at: checked_at.clone(),
                    vectors: map,
                };
                let detail = match write_baseline(&path, &base) {
                    Ok(()) if status == "created" => {
                        format!("基线不存在，已用本次结果创建（{path:?}）")
                    }
                    Ok(()) => format!("指纹已变（换模型/精度/设备/像素口径），基线已重建（{path:?}）"),
                    Err(e) => format!("基线写入失败: {e}"),
                };
                GoldenCheck {
                    status,
                    passed: true,
                    min_cosine: f64::NAN,
                    detail,
                    checked_at,
                }
            }
        }
    }

    /// 编码一批输入（在 spawn_blocking 里跑，避免卡住 async 运行时）
    pub fn embed_blocking(&self, input: &[EmbedInputItem]) -> ApiResult<Vec<EncodeOutcome>> {
        // 按 fastembed 的能力分组：它只支持纯文本与纯图片
        let mut text_slots: Vec<usize> = Vec::new();
        let mut text_payloads: Vec<String> = Vec::new();
        let mut image_slots: Vec<usize> = Vec::new();
        let mut image_payloads: Vec<Vec<u8>> = Vec::new();
        // 与 image_slots 同序的预处理提示（缩放等），用来回传给上层
        let mut image_notices: Vec<Vec<String>> = Vec::new();

        for (idx, item) in input.iter().enumerate() {
            match (&item.text, &item.image) {
                (Some(text), None) => {
                    text_slots.push(idx);
                    text_payloads.push(text.clone());
                }
                (None, Some(image)) => {
                    image_slots.push(idx);
                    let raw = decode_image_input(image, self.max_media_bytes)?;
                    // 比例检查（超限直接拒绝）+ 超尺寸等比缩小到安全区
                    let t_pre = std::time::Instant::now();
                    let prepared = preprocess_image(raw, self.max_aspect_ratio, self.max_pixels)?;
                    tracing::info!(
                        index = idx,
                        preprocess_ms = t_pre.elapsed().as_millis() as u64,
                        "图片预处理耗时（解码/缩放/重编码）"
                    );
                    image_payloads.push(prepared.bytes);
                    image_notices.push(prepared.notices);
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
        // 自检没通过就不出向量：宁可报错，也不要把不可比的向量写进索引
        if let Some(g) = self.golden_check() {
            if !g.passed {
                return Err(ApiError::golden_failed(format!(
                    "模型自检未通过，拒绝编码：{}",
                    g.detail
                ))
                .with_detail(serde_json::json!({
                    "status": g.status,
                    "min_cosine": g.min_cosine,
                    "checked_at": g.checked_at,
                })));
            }
        }
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
                    notices: Vec::new(),
                });
            }
        }

        if !image_payloads.is_empty() {
            // **逐张编码**，不要整批送进去。
            //
            // fastembed 的 embed_image_bytes 会把这一批图片的视觉 token 拼成一个 batch：
            // 作业默认 batch_size=4，等于 4 张图的激活值同时驻留显存。3072 token 的图
            // 在 8GB 卡上单张就接近上限，4 张必然 CUDA_ERROR_OUT_OF_MEMORY ——
            // 这也是「改了 f16 仍然 OOM」的原因：f16 只把权重减半，救不了批量激活。
            for (i, payload) in image_payloads.iter().enumerate() {
                let t_enc = std::time::Instant::now();
                let vectors = encoder
                    .embed_image_bytes(&[payload.as_slice()])
                    .map_err(|e| {
                        let text = e.to_string();
                        if text.contains("OUT_OF_MEMORY") || text.to_ascii_lowercase().contains("out of memory") {
                            ApiError::encoder_error(format!(
                                "编码图片时显存不足（{e}）。当前按单张编码，仍失败说明单张就超限：\
                                 请降低 --max-pixels（当前 {}）或改用 --candle-dtype f16 / --candle-device cpu",
                                self.max_pixels
                            ))
                        } else {
                            ApiError::encoder_error(format!("candle 图片编码失败: {e}"))
                        }
                    })?;
                let encode_ms = t_enc.elapsed().as_millis() as u64;
                tracing::info!(index = i, encode_ms, "单张图片推理耗时");
                if encode_ms > 10_000 && !SLOW_ENCODE_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(
                        encode_ms,
                        max_pixels = self.max_pixels,
                        "单张编码超过 10 秒：显存可能贴顶（或已回退到共享内存）；\
                         建议降低 --max-pixels（8GB 卡用 786432）"
                    );
                }
                let vector = vectors
                    .into_iter()
                    .next()
                    .ok_or_else(|| ApiError::internal("图片编码结果为空"))?;
                let slot = image_slots[i];
                out[slot] = Some(EncodeOutcome {
                    dense: vector,
                    sparse: None,
                    // fastembed 不回传 resize 后的尺寸与视觉 token 数（开发文档 §5.1 说这两个
                    // 数字是排查质量的第一现场，所以这里如实置空而不是编一个值）
                    resized: None,
                    vision_tokens: None,
                    kind: Some("image".to_string()),
                    fingerprint: fingerprint.clone(),
                    notices: image_notices[i].clone(),
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

/// 图片预处理的产物：可能是原 bytes（未改动），也可能是缩放后重新编码的 PNG。
#[derive(Debug)]
struct Preprocessed {
    bytes: Vec<u8>,
    notices: Vec<String>,
}

/// 编码前的图片预处理。
///
/// 1. **宽高比检查**：长边/短边超过 `max_aspect_ratio` 直接拒绝（400）。
///    极端长条会被切出大量无效 patch，留给模型的几乎全是空白。
/// 2. **超大图等比缩小**：像素数超过安全区（[`MAX_PIXELS`]）时按面积等比缩到安全区内。
///
/// 只读 header 拿尺寸：比例不合格的图（往往正是大文件）不需要全解码就能拒绝；
/// 尺寸本来就在安全区内的图原样返回，解码仍交给 fastembed，避免重复解码。
fn preprocess_image(bytes: Vec<u8>, max_aspect_ratio: f64, max_pixels: u64) -> ApiResult<Preprocessed> {
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|e| ApiError::bad_request(format!("无法识别图片格式: {e}")))?
        .into_dimensions()
        .map_err(|e| ApiError::bad_request(format!("读取图片尺寸失败: {e}")))?;
    if w == 0 || h == 0 {
        return Err(ApiError::bad_request("图片宽或高为 0"));
    }

    let long = w.max(h) as f64;
    let short = w.min(h) as f64;
    let ratio = long / short;
    if max_aspect_ratio > 0.0 && ratio > max_aspect_ratio {
        return Err(ApiError::bad_request(format!(
            "图片宽高比 {ratio:.1}:1 超过上限 {max_aspect_ratio:.1}:1（{w}×{h}）；\
             极端长条会切出大量无效 patch，请先裁剪后重试"
        ))
        .with_detail(serde_json::json!({
            "width": w,
            "height": h,
            "aspect_ratio": (ratio * 100.0).round() / 100.0,
            "max_aspect_ratio": max_aspect_ratio,
        })));
    }

    let pixels = w as u64 * h as u64;
    if pixels <= max_pixels {
        return Ok(Preprocessed { bytes, notices: Vec::new() });
    }

    // 超过安全区：解码 → 等比缩到 MAX_PIXELS 以内 → 重新编码成 PNG 交回去
    let img = image::load_from_memory(&bytes)
        .map_err(|e| ApiError::bad_request(format!("解码图片失败: {e}")))?;
    // 用 floor 而不是 round：round 可能向上取整，让缩放后的像素数反过来超过安全区
    // （实测 5400×3603 会落到 3,145,779 > 3,145,728，刚好越界 51 像素）
    let scale = (max_pixels as f64 / pixels as f64).sqrt();
    let mut nw = ((w as f64 * scale).floor() as u32).max(1);
    let mut nh = ((h as f64 * scale).floor() as u32).max(1);
    while nw as u64 * nh as u64 > max_pixels {
        // 极端边界（floor 后仍越界，理论上不会发生）再收一格
        nw = nw.saturating_sub(1).max(1);
        nh = nh.saturating_sub(1).max(1);
    }
    let scaled = img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3);
    // 用 JPEG 而不是 PNG：这一步只是「把缩放结果交给 fastembed」的中间载体，
    // PNG 编码一张 1536×1024 要几百毫秒，JPEG 只要几十毫秒；有损对嵌入质量影响可忽略
    // （fastembed 还会再按 patch 对齐缩放一次）。
    let mut out = Vec::new();
    let mut cur = std::io::Cursor::new(&mut out);
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cur, 92);
    enc.encode_image(&scaled)
        .map_err(|e| ApiError::internal(format!("重新编码图片失败: {e}")))?;

    Ok(Preprocessed {
        bytes: out,
        notices: vec![format!(
            "图片 {w}×{h}（{pixels} 像素）超过安全区 {max_pixels} 像素，已等比缩放到 {nw}×{nh}"
        )],
    })
}

/// 进程级环境只设一次。
///
/// `HF_HOME` 是 hf-hub 用来定位缓存目录的关键变量（`pull_from_hf` 直接读它）。
/// `HF_HUB_OFFLINE` / `HF_HUB_DISABLE_TELEMETRY` 在 hf-hub 0.5 里**并不存在**，
/// 设了只是无害的保险（未来版本可能支持）；真正防止联网靠的是缓存命中 + 文件预校验。
///
/// 这些是全局变量，设置本身就是进程副作用：重复 `set_var` 无意义，
/// 未来切 edition 2024 时还需要 unsafe 包裹。
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
/// <cache_dir>/models--Qwen--Qwen3-VL-Embedding-2B/
///   refs/main              ← 内容 "local"
///   snapshots/local/
///     config.json          ← 硬链接（不复制，不占额外空间）
///     preprocessor_config.json ← **改写过的副本**（max_pixels 取当前配置）
///     tokenizer.json       ← 硬链接
///     model.safetensors    ← 硬链接（4.2 GB 秒级完成）
/// ```
///
/// 为什么不直接改原文件：那是原始下载的模型目录，改动它会破坏离线基线；
/// 改写只作用在我们自己的缓存副本上。
/// 启动自检：如果模型目录里**原始** `preprocessor_config.json` 的 `max_pixels`
/// 与当前生效值不同，提示一句 —— 那个值不生效，真正生效的是缓存副本里的值。
///
/// 为什么要提示：手工去改模型目录里的配置是很自然的想法，但 fastembed 读的是
/// `HF_HOME` 缓存里的副本，改了原文件不会有任何效果，也不会有报错。
pub fn warn_if_max_pixels_overridden(model_dir: &Path, max_pixels: u64) {
    let path = model_dir.join("preprocessor_config.json");
    let Ok(raw) = std::fs::read(&path) else {
        return;
    };
    let Ok(cfg) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return;
    };
    let original = cfg.get("max_pixels").and_then(|v| v.as_u64());
    if original != Some(max_pixels) {
        tracing::warn!(
            original = ?original,
            effective = max_pixels,
            "模型目录里 preprocessor_config.json 的 max_pixels={:?} 不会生效；\
             实际口径由 --max-pixels={}（写在 HF 缓存副本里）决定",
            original,
            max_pixels
        );
    }
}

/// 慢编码警告只打一次，避免整批作业刷屏。
static SLOW_ENCODE_WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 金标基线文件名（放在 HF 缓存目录里，与模型缓存同生命周期）。
const GOLDEN_BASELINE_FILE: &str = "golden-baseline.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GoldenBaseline {
    fingerprint: String,
    created_at: String,
    /// 标签 → 2048 维向量（zh / en / mix）
    vectors: std::collections::BTreeMap<String, Vec<f32>>,
}

fn load_baseline(path: &Path) -> Option<GoldenBaseline> {
    let raw = std::fs::read(path).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn write_baseline(path: &Path, base: &GoldenBaseline) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(base)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, bytes)
}

/// 余弦相似度（向量已 L2 归一化，这里仍按通式算，避免依赖归一化假设）。
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let x = *x as f64;
        let y = *y as f64;
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// 缓存副本里的 max_pixels / min_pixels 是否已经等于期望值。
/// 读不出来（不存在、损坏）一律视为不一致，让它重写。
fn config_matches(path: &Path, max_pixels: u64) -> bool {
    let Ok(raw) = std::fs::read(path) else {
        return false;
    };
    let Ok(cfg) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return false;
    };
    cfg.get("max_pixels").and_then(|v| v.as_u64()) == Some(max_pixels)
        && cfg.get("min_pixels").and_then(|v| v.as_u64()) == Some(MIN_PIXELS as u64)
}

fn prepare_hf_cache(model_dir: &Path, cache_dir: &Path, max_pixels: u64) -> ApiResult<PathBuf> {
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
    // ⚠️ 结构必须与 hf-hub 的 Cache 一致：<cache_dir>/<模型目录>/snapshots/<rev>/，
    // **没有 `hub/` 这一层**（那是 HF_HOME 在 Python 侧的约定，不是 hf-hub 的）。
    // 多套一层 hub/ 的结果是缓存永远不命中 → repo.get() 转去下载 → 断网直接失败。
    let repo_root = hf_home.join("models--Qwen--Qwen3-VL-Embedding-2B");
    let snapshot = repo_root.join("snapshots").join(CACHE_REVISION);
    std::fs::create_dir_all(&snapshot)
        .map_err(|e| ApiError::internal(format!("创建缓存目录失败: {e}")))?;

    for file in REQUIRED_FILES {
        let src = model_dir.join(file);
        let dst = snapshot.join(file);

        if *file == "preprocessor_config.json" {
            // 副本里的 max_pixels 必须与当前配置一致。
            // 不能「已存在就跳过」—— 改了 --max-pixels 之后旧副本会继续生效，
            // 导致「指纹变了、实际口径还是旧的」这种最难查的错位。
            if config_matches(&dst, max_pixels) {
                continue;
            }
            let raw = std::fs::read(&src)
                .map_err(|e| ApiError::internal(format!("读取 preprocessor_config 失败: {e}")))?;
            let mut cfg: serde_json::Value = serde_json::from_slice(&raw)
                .map_err(|e| ApiError::internal(format!("preprocessor_config 不是合法 JSON: {e}")))?;
            cfg["max_pixels"] = serde_json::json!(max_pixels);
            cfg["min_pixels"] = serde_json::json!(MIN_PIXELS);
            let pretty = serde_json::to_vec_pretty(&cfg)
                .map_err(|e| ApiError::internal(format!("序列化 preprocessor_config 失败: {e}")))?;
            std::fs::write(&dst, pretty)
                .map_err(|e| ApiError::internal(format!("写入 preprocessor_config 副本失败: {e}")))?;
            tracing::info!(max_pixels, "已为 candle 缓存写入 preprocessor_config 副本");
        } else {
            if dst.exists() {
                continue;
            }
            // 硬链接：同一文件系统上瞬间完成且不占额外空间；跨设备时退回复制
            if let Err(e) = std::fs::hard_link(&src, &dst) {
                tracing::warn!(file = *file, error = %e, "硬链接失败，改为复制");
                std::fs::copy(&src, &dst)
                    .map_err(|e| ApiError::internal(format!("复制模型文件失败: {e}")))?;
            }
        }
    }

    let refs = repo_root.join("refs");
    std::fs::create_dir_all(&refs)
        .map_err(|e| ApiError::internal(format!("创建 refs 目录失败: {e}")))?;
    std::fs::write(refs.join("main"), CACHE_REVISION)
        .map_err(|e| ApiError::internal(format!("写入 refs/main 失败: {e}")))?;

    Ok(hf_home)
}

/// 模型目录解析：显式指定优先，其次按约定路径查找。
///
/// 顺序：
/// 1. `--model-dir` / `PV_MODEL_DIR`（**显式指定就必须有效**，不合法直接报错，不回落）
/// 2. `<exe 所在目录>/models/qwen3-vl-embedding-2b`
/// 3. `<当前工作目录>/models/qwen3-vl-embedding-2b`
/// 4. `<当前工作目录>/../models/qwen3-vl-embedding-2b`（从仓库子目录启动时用）
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
        if let Some(parent) = cwd.parent() {
            candidates.push(parent.join("models").join("qwen3-vl-embedding-2b"));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(w, h);
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("生成测试 PNG");
        out
    }

    fn dims(bytes: &[u8]) -> (u32, u32) {
        image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .expect("识别格式")
            .into_dimensions()
            .expect("读取尺寸")
    }

    #[test]
    fn rejects_extreme_aspect_ratio() {
        // 4000x100 = 40:1，超过默认 8:1
        let err = preprocess_image(png_bytes(4000, 100), 8.0, MAX_PIXELS as u64).expect_err("应当拒绝");
        assert_eq!(err.code, "BAD_REQUEST", "宽高比超限必须是 400");
        assert!(err.message.contains("宽高比"), "错误信息要说明原因: {}", err.message);
    }

    #[test]
    fn accepts_16_by_9() {
        // 1280x720 = 1.78:1 且 92 万像素，在安全区内：既不拒绝也不缩放
        let out = preprocess_image(png_bytes(1280, 720), 8.0, MAX_PIXELS as u64).expect("16:9 不应被拒绝");
        assert!(out.notices.is_empty(), "未超安全区不应有提示");
    }

    #[test]
    fn downscales_oversized_image_into_safe_area() {
        // 5400x3603 = 1945 万像素，远超默认 3,145,728
        let pre = preprocess_image(png_bytes(5400, 3603), 8.0, MAX_PIXELS as u64).expect("超大图应被缩小而不是报错");
        assert_eq!(pre.notices.len(), 1);
        assert!(pre.notices[0].contains("已等比缩放到"), "{}", pre.notices[0]);
        let (w, h) = dims(&pre.bytes);
        assert!(
            w as u64 * h as u64 <= MAX_PIXELS as u64,
            "缩放后 {w}x{h} 仍超过安全区"
        );
        // 比例要保住（容差按像素取整）
        let before = 5400.0 / 3603.0;
        let after = w as f64 / h as f64;
        assert!((before - after).abs() < 0.01, "缩放应保持宽高比: {before} vs {after}");
    }

    #[test]
    fn cache_config_matches_detects_stale_values() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("preprocessor_config.json");
        std::fs::write(&path, r#"{"max_pixels":1843200,"min_pixels":4096}"#).expect("写文件");
        assert!(
            !config_matches(&path, 3_145_728),
            "旧口径（1,843,200）必须判定为不一致，否则改了配置也不会重建副本"
        );
        assert!(config_matches(&path, 1_843_200), "同值应判定为一致");
        assert!(
            !config_matches(&dir.path().join("missing.json"), 1),
            "副本缺失应判定为不一致"
        );
    }

    #[test]
    fn keeps_image_within_safe_area_untouched() {
        let bytes = png_bytes(1024, 1024);
        let len = bytes.len();
        let pre = preprocess_image(bytes, 8.0, MAX_PIXELS as u64).expect("安全区内不应改动");
        assert!(pre.notices.is_empty());
        assert_eq!(pre.bytes.len(), len, "安全区内的图应原样返回（不重新编码）");
    }
}
