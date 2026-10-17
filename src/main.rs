//! prompt-vault 主服务入口（Salvo 1.0.1）。
//!
//! 启动顺序（开发文档 §9.4）：
//! 1. 打开 SQLite + Qdrant Edge shard；
//! 2. 探一次侧车 `/health`（拿指纹与设备，失败不致命）；
//! 3. 跑一次金标校验 `/v1/verify` —— **failed 就拒绝启动**；
//!    `unsupported`（侧车未实现该接口）只警告，不拦。

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use salvo::affix_state;
use salvo::fs::NamedFile;
use salvo::http::HeaderValue;
use rust_embed::RustEmbed;
use salvo::prelude::*;

use prompt_vault::api;
use prompt_vault::encode::EncoderMode;
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
    /// 编码侧车地址（Python FastAPI，本项目不修改它）
    #[arg(long, env = "PV_ENCODER_URL", default_value = "http://127.0.0.1:8917")]
    encoder_url: String,
    /// live = 真侧车（HTTP）；candle = 进程内 candle 直接加载模型；mock = 假编码器（不运行模型）
    #[arg(long, env = "PV_ENCODER", default_value = "live")]
    encoder: String,
    /// candle 模式的模型目录；不指定则按约定路径查找（显式指定必须有效）
    #[arg(long, env = "PV_MODEL_DIR")]
    model_dir: Option<PathBuf>,
    /// candle 权重精度：f32（默认，最稳）/ f16 / bf16
    #[arg(long, env = "PV_CANDLE_DTYPE", default_value = "f32")]
    candle_dtype: String,
    /// candle 的序列长度上限（与侧车一致：8192）
    #[arg(long, env = "PV_MAX_LENGTH", default_value_t = 8192)]
    max_length: usize,
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
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(log_filter))
        .init();

    let args = Args::parse();
    let flags = Arc::new(Flags {
        host: args.host.clone(),
        port: args.port,
        encoder_url: args.encoder_url.clone(),
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
        encoder_url: args.encoder_url.clone(),
        encoder_mode: EncoderMode::parse(&args.encoder),
        model_dir: args.model_dir.clone(),
        candle_dtype: args.candle_dtype.clone(),
        max_length: args.max_length,
        web_dir: args.web_dir.clone(),
        max_media_bytes: args.max_media_bytes,
    };

    let state = match AppState::new(cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("启动失败: {e:#}");
            std::process::exit(1);
        }
    };

    print_banner(&state, &flags);

    // ---- 启动自检：侧车 + 金标（开发文档 §9.4）----
    // mock 模式不去探真侧车，否则日志里会出现"侧车不可达"的误导性告警。
    let sidecar = match state.encoder.mode() {
        EncoderMode::Mock => {
            tracing::info!("mock 编码器已启用：跳过侧车探测（本模式不加载任何模型）");
            None
        }
        EncoderMode::Candle => {
            let fp = state.encoder.fingerprint_hint();
            tracing::info!(fingerprint = %fp, "candle 后端：跳过 HTTP 侧车探测（编码在进程内完成）");
            None
        }
        EncoderMode::Live => state.encoder.health().await,
    };
    match (&sidecar, state.encoder.mode()) {
        (Some(h), _) => tracing::info!(
            model = ?h.model,
            device = ?h.device,
            dim = ?h.dim,
            fingerprint = ?h.fingerprint,
            "侧车就绪"
        ),
        // mock / candle 本来就不需要 HTTP 侧车，这里不打告警
        (None, EncoderMode::Mock) | (None, EncoderMode::Candle) => {}
        (None, _) => tracing::warn!(
            url = %state.cfg.encoder_url,
            error = ?state.encoder.last_error(),
            "侧车不可达：检索与编码会返回 503；服务照常启动（DEGRADED）"
        ),
    }

    let golden = state.encoder.verify_golden().await;
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
    encoder_url: String,
    web_dir: PathBuf,
}

fn print_banner(state: &Arc<AppState>, flags: &Flags) {
    let sparse = if state.vectors.sparse_enabled() { "启用" } else { "关闭" };
    let index_fp = state.index_fingerprint().ok().flatten().unwrap_or_else(|| "(尚未写入)".into());
    println!("──────────────────────────────────────────────");
    println!("prompt-vault {}  http://{}:{}", env!("CARGO_PKG_VERSION"), flags.host, flags.port);
    println!("  编码侧车 : {}  (mode={})", flags.encoder_url, state.encoder.mode().as_str());
    println!("  SQLite   : {}", state.cfg.db_path.display());
    println!("  向量索引 : {}  dim={} 量化={} 稀疏={}", state.cfg.vector_dir.display(), state.cfg.dim, state.vectors.quantization().as_str(), sparse);
    println!("  索引指纹 : {index_fp}");
    println!("  媒体目录 : {}", state.cfg.media_dir.display());
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
