//! prompt-vault 主服务入口（Salvo 1.0.1）。
//!
//! 启动顺序（开发文档 §9.4）：
//! 1. 打开 SQLite + Qdrant Edge shard；
//! 2. 解析模型目录并打印编码指纹与设备（candle 进程内，没有远端可探）；
//! 3. 跑一次金标校验 —— **failed 就拒绝启动**；
//!    `unsupported`（当前后端没有历史基线）只记录，不拦。

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use salvo::affix_state;
use salvo::fs::NamedFile;
use salvo::http::HeaderValue;
use rust_embed::RustEmbed;
use salvo::prelude::*;

use prompt_vault::api;
use prompt_vault::state::{AppState, Config};
use prompt_vault::vector::QuantMode;

#[derive(Parser, Debug)]
#[command(name = "prompt-vault", version, about = "本地提示词 / 图片资产库：Rust 主服务")]
struct Args {
    #[arg(long, env = "PV_HOST", default_value = "127.0.0.1")]
    host: String,
    #[arg(long, env = "PV_PORT", default_value_t = 8918)]
    port: u16,
    #[arg(long, env = "PV_DATA_DIR", default_value = "data")]
    data_dir: PathBuf,
    #[arg(long, env = "PV_MEDIA_DIR", default_value = "media")]
    media_dir: PathBuf,
    /// candle 模型目录；不指定则按约定路径查找（显式指定必须有效）
    #[arg(long, env = "PV_MODEL_DIR")]
    model_dir: Option<PathBuf>,
    /// candle 权重精度：f32（默认，最稳）/ f16 / bf16
    #[arg(long, env = "PV_CANDLE_DTYPE", default_value = "f32")]
    candle_dtype: String,
    /// candle 计算设备：auto（默认，二进制带 cuda 且驱动可用时用 GPU，否则回落 CPU）/ cpu / cuda:0
    #[arg(long, env = "PV_CANDLE_DEVICE", default_value = "auto")]
    candle_device: String,
    /// candle 的 HuggingFace 缓存目录（默认 <data-dir>/hf-cache）
    #[arg(long, env = "PV_HF_CACHE_DIR")]
    hf_cache_dir: Option<PathBuf>,
    /// candle 的序列长度上限（默认 8192）
    #[arg(long, env = "PV_MAX_LENGTH", default_value_t = 8192)]
    max_length: usize,
    /// 不加载模型，用确定性伪向量跑通链路（仅开发/冒烟；开了它 --model-dir 会被忽略）
    #[arg(long, env = "PV_MOCK", default_value_t = false)]
    mock: bool,
    /// 向量维度：本项目固定 2048 不截断（开发文档 §4.2）
    #[arg(long, env = "PV_DIM", default_value_t = 2048)]
    dim: usize,
    /// 索引里是否启用稀疏（BM25）向量；只在首次建 shard 时生效
    #[arg(long, env = "PV_SPARSE", default_value_t = true, action = clap::ArgAction::Set)]
    sparse: bool,
    /// 量化：scalar（默认，int8）| turbo4 | off
    #[arg(long, env = "PV_QUANT", default_value = "scalar")]
    quant: String,
    /// 前端产物目录（web/dist）
    #[arg(long, env = "PV_WEB_DIR", default_value = "web/dist")]
    web_dir: PathBuf,
    /// 单个媒体文件字节上限（开发文档 §12 第 8 条：按字节和像素双重设限）
    #[arg(long, env = "PV_MAX_MEDIA_BYTES", default_value_t = 268_435_456)]
    max_media_bytes: u64,
    /// 图片宽高比上限（长边/短边，默认 8.0）；超过直接拒绝编码，0 表示不检查
    #[arg(long, env = "PV_MAX_ASPECT_RATIO", default_value_t = 8.0)]
    max_aspect_ratio: f64,
    /// 图片像素安全区上限（默认 3,145,728 ≈ 1774²）；更大的图会先等比缩小
    #[arg(long, env = "PV_MAX_PIXELS", default_value_t = 3_145_728)]
    max_pixels: u64,
}

#[tokio::main]
async fn main() {
    // 注意：RUST_LOG 只填了空白时 try_from_default_env 会返回一个空 filter，
    // 结果是"一条日志都不输出"这种很难查的静默现象，所以这里显式 trim 与兜底。
    let log_filter = std::env::var("RUST_LOG")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "info,quinn=warn".to_string());
    // qdrant-edge 0.8.0 把 `common` 模块声明成私有的（`mod common;`），
    // 既没有 `init_feature_flags` 也没有 `MULTI_MMAP_SUPPORT_CHECK_RESULT` 的对外入口，
    // 于是它每次读 feature flags / 多 mmap 开关都会打一条 WARN（启动一次至少 6 条）。
    // 这些告警对调用方没有任何可操作信息：未初始化时库用的就是 `FeatureFlags::default()`，
    // 与显式初始化成默认值的行为完全一致。
    //
    // 按模块前缀压到 error：`qdrant_edge::common` 同时覆盖 `common::flags` 与
    // `common::mmap::ops` 两个 target（实测写到更细的 `…::common::flags` 反而不生效，
    // 所以用模块级前缀）。其余模块（如 `qdrant_edge::wal`）的日志照常输出。
    // 上游暴露初始化入口后应改回真正的初始化。
    // 想临时看回这些日志：RUST_LOG="info,qdrant_edge::common=warn"。
    // 另：该库对「多 mmap 支持」的兜底开关是环境变量 QDRANT_NO_MULTI_MMAP=1
    // （数据目录挂在 Docker-on-Windows 之类场景下建议打开）。
    let log_filter = format!("{log_filter},qdrant_edge::common=error");
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(log_filter))
        .init();

    let args = Args::parse();
    let flags = Arc::new(Flags {
        host: args.host.clone(),
        port: args.port,
        web_dir: args.web_dir.clone(),
    });

    let cfg = Config {
        host: args.host.clone(),
        port: args.port,
        data_dir: args.data_dir.clone(),
        media_dir: args.media_dir.clone(),
        db_path: args.data_dir.join("pv.sqlite"),
        vector_dir: args.data_dir.join("qdrant"),
        thumb_dir: args.data_dir.join("thumbs"),
        export_dir: args.data_dir.join("exports"),
        dim: args.dim,
        sparse_enabled: args.sparse,
        quant: QuantMode::parse(&args.quant),
        mock: args.mock,
        model_dir: args.model_dir.clone(),
        candle_dtype: args.candle_dtype.clone(),
        candle_device: args.candle_device.clone(),
        hf_cache_dir: args
            .hf_cache_dir
            .clone()
            .unwrap_or_else(|| args.data_dir.join("hf-cache")),
        max_length: args.max_length,
        web_dir: args.web_dir.clone(),
        max_media_bytes: args.max_media_bytes,
        max_aspect_ratio: args.max_aspect_ratio,
        max_pixels: args.max_pixels,
    };

    let state = match AppState::new(cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("启动失败: {e:#}");
            std::process::exit(1);
        }
    };

    print_banner(&state, &flags);

    // ---- 启动自检：编码指纹 + 金标（开发文档 §9.4）----
    // 侧车已移除：编码在进程内完成，这里只打印指纹与设备，
    // 供排查「索引与当前模型是否同一空间」。
    tracing::info!(
        fingerprint = %state.encoder.fingerprint_hint(),
        device = ?state.encoder.candle_encoder().map(|c| c.device_display()),
        "candle 后端就绪（编码在进程内完成）"
    );

    let golden = state.encoder.verify_golden();
    tracing::info!(status = %golden.status, detail = ?golden.detail, "金标校验结果");
    let blocked = golden.status == "failed";
    state.set_golden(golden.clone());
    if blocked {
        eprintln!(
            "金标校验未通过（status=failed），按开发文档 §9.4 拒绝启动。\n详情: {:?}",
            golden.detail
        );
        std::process::exit(1);
    }

    let router = api::build_router().push(Router::with_path("{**path}").get(serve_frontend));
    let service = Service::new(router).hoop(affix_state::inject(Arc::clone(&state)));

    let addr = format!("{}:{}", state.cfg.host, state.cfg.port);
    let acceptor = TcpListener::new(addr.clone()).bind().await;
    tracing::info!(%addr, "prompt-vault 已启动 → http://{addr}/");
    let shutdown_state = Arc::clone(&state);
    tokio::spawn(async move {
        Server::new(acceptor).serve(service).await;
    });

    // 关键：EdgeShard 只在 drop 时 flush，被 kill -9/sigterm 直接终止就会丢数据（实测过），
    // 所以这里拦下信号，先 flush 再退出。
    wait_for_shutdown().await;
    tracing::info!("收到退出信号：flush 向量索引…");
    if let Err(e) = shutdown_state.vectors.flush() {
        tracing::error!(error = %e, "退出前 flush 失败");
    }
    if let Err(e) = shutdown_state.db.with_conn(|c| c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")) {
        tracing::warn!(error = %e, "SQLite checkpoint 失败（WAL 数据仍在，下次打开自动恢复）");
    }
    tracing::info!("已安全退出");
    std::process::exit(0);
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

struct Flags {
    host: String,
    port: u16,
    web_dir: PathBuf,
}

fn print_banner(state: &Arc<AppState>, flags: &Flags) {
    let sparse = if state.vectors.sparse_enabled() { "启用" } else { "关闭" };
    let index_fp = state.index_fingerprint().ok().flatten().unwrap_or_else(|| "(尚未写入)".into());
    let encoder = state.encoder.candle_encoder();
    let model_dir = encoder
        .as_ref()
        .map(|c| c.model_dir().display().to_string())
        .unwrap_or_else(|| "(未配置)".to_string());
    let device = encoder
        .as_ref()
        .map(|c| c.device_display())
        .unwrap_or_else(|| "cpu".to_string());
    println!("──────────────────────────────────────────────");
    println!("prompt-vault {}  http://{}:{}", env!("CARGO_PKG_VERSION"), flags.host, flags.port);
    println!("  编码后端 : candle（进程内）  device={device}");
    println!("  模型目录 : {model_dir}");
    println!("  SQLite   : {}", state.cfg.db_path.display());
    println!("  向量索引 : {}  dim={} 量化={} 稀疏={}", state.cfg.vector_dir.display(), state.cfg.dim, state.vectors.quantization().as_str(), sparse);
    println!("  索引指纹 : {index_fp}");
    println!("  媒体目录 : {}", state.cfg.media_dir.display());
    println!("  缩略图   : {}（等比缩放、不裁剪，按需生成）", state.cfg.thumb_dir.display());
    println!("  前端产物 : {}", flags.web_dir.display());
    println!("──────────────────────────────────────────────");
}

/// 二进制内嵌的前端产物（开发文档 §0：构建产物由 Rust 侧 `embed` 打包）。
///
/// `build.rs` 保证 `web/dist` 一定存在，所以这里不会因为缺目录而编译失败。
#[derive(RustEmbed)]
#[folder = "web/dist"]
struct WebAssets;

/// 前端静态资源：**磁盘优先**（开发期改完前端不用重编译），
/// 磁盘上没有就回落到二进制内嵌的产物，SPA 路由再回落 `index.html`。
#[handler]
async fn serve_frontend(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Ok(state) = depot.get_typed::<Arc<AppState>>() else {
        res.status_code(salvo::http::StatusCode::INTERNAL_SERVER_ERROR);
        return;
    };
    let web_dir = state.cfg.web_dir.clone();
    let raw_path = req.uri().path().trim_start_matches('/').to_string();
    let safe = !raw_path.contains("..");
    let rel = if raw_path.is_empty() || !safe { "index.html".to_string() } else { raw_path };

    // 1) 磁盘上真实存在的文件（开发期热更新走这条）
    if safe {
        let candidate = web_dir.join(&rel);
        if candidate.is_file() {
            send_disk(req, res, &candidate, &rel).await;
            return;
        }
    }

    // 2) 内嵌产物里的静态文件
    if let Some(file) = WebAssets::get(&rel) {
        let mime = mime_of(&rel);
        let _ = res.add_header("content-type", mime, true);
        res.body(file.data.into_owned());
        return;
    }

    // 3) SPA 回落：前端路由（无扩展名）交给 index.html
    if !rel.contains('.') {
        if safe {
            let index = web_dir.join("index.html");
            if index.is_file() {
                send_disk(req, res, &index, "index.html").await;
                return;
            }
        }
        if let Some(file) = WebAssets::get("index.html") {
            let _ = res.add_header("content-type", "text/html; charset=utf-8", true);
            res.body(file.data.into_owned());
            return;
        }
    }

    res.status_code(salvo::http::StatusCode::NOT_FOUND);
    res.render(Text::Html(format!(
        "<h3>前端产物不存在</h3><p>期望目录：{}（二进制里也没有内嵌产物）</p><p>先执行 <code>cd web && pnpm install && pnpm build</code> 再 <code>cargo build</code>，或用 <code>--web-dir</code> 指定。</p><p>API 仍可用：<a href=\"/api/health\">/api/health</a></p>",
        web_dir.display()
    )));
}

async fn send_disk(req: &mut Request, res: &mut Response, path: &std::path::Path, rel: &str) {
    match NamedFile::open(path).await {
        Ok(file) => {
            if rel.ends_with(".html") {
                res.headers_mut()
                    .insert("content-type", HeaderValue::from_static("text/html; charset=utf-8"));
            }
            file.send(req.headers(), res).await;
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "静态资源打开失败");
            res.status_code(salvo::http::StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
}

/// 内嵌产物没有文件系统元数据，MIME 只能按扩展名给（前端产物只有这几种）。
fn mime_of(rel: &str) -> &'static str {
    let lower = rel.to_ascii_lowercase();
    if lower.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if lower.ends_with(".js") || lower.ends_with(".mjs") {
        "text/javascript; charset=utf-8"
    } else if lower.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if lower.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".ico") {
        "image/x-icon"
    } else if lower.ends_with(".woff2") {
        "font/woff2"
    } else if lower.ends_with(".woff") {
        "font/woff"
    } else {
        "application/octet-stream"
    }
}
