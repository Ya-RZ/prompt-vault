//! prompt-vault 主服务入口（Salvo 1.0.1）。
//!
//! 启动顺序（开发文档 §9.4）：
//! 1. 打开 SQLite，并连接 Qdrant Server（集合不存在则创建）；
//! 2. 读取配置，初始化第三方 embedding 客户端（OpenAI 兼容）；
//! 3. 主动探测一次编码服务：失败只记录并让 health 报 DEGRADED，不阻止启动。

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use salvo::affix_state;
use salvo::fs::NamedFile;
use salvo::http::HeaderValue;
use rust_embed::RustEmbed;
use salvo::prelude::*;

use prompt_vault::api;
use prompt_vault::config_file;
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
    /// Qdrant 服务地址。客户端走 gRPC（默认端口 6334）；
    /// 填 REST 端口 6333 时会自动换成 6334。
    #[arg(long, env = "PV_QDRANT_URL", default_value = "http://127.0.0.1:6334")]
    qdrant_url: String,
    /// Qdrant 集合名
    #[arg(long, env = "PV_QDRANT_COLLECTION", default_value = "pv")]
    qdrant_collection: String,
    #[arg(long, env = "PV_MEDIA_DIR", default_value = "media")]
    media_dir: PathBuf,
    /// 第三方 embedding 服务地址（OpenAI 兼容，含 /v1）；默认硅基流动
    #[arg(long, env = "PV_EMBEDDING_BASE_URL")]
    embedding_base_url: Option<String>,
    /// embedding 模型名；默认 Qwen/Qwen3-VL-Embedding-8B
    #[arg(long, env = "PV_EMBEDDING_MODEL")]
    embedding_model: Option<String>,
    /// embedding API Key（也可写在 config.toml 的 [embedding] api_key）
    #[arg(long, env = "PV_EMBEDDING_API_KEY")]
    embedding_api_key: Option<String>,
    /// 不调用第三方服务，用确定性伪向量跑通链路（仅开发/冒烟）
    #[arg(long, env = "PV_MOCK", default_value_t = false)]
    mock: bool,
    /// 向量维度；不指定时读 config.toml 的 [embedding] dimensions，再退到 4096
    #[arg(long, env = "PV_DIM")]
    dim: Option<usize>,
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
    /// 配置文件路径（默认按 `<exe>/config.toml`、当前目录 `config.toml` 查找）
    #[arg(long, env = "PV_CONFIG")]
    config: Option<PathBuf>,
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

    // 配置文件是必需的输入：找不到就生成模板并退出，让用户填完再启动。
    let file_cfg = match config_file::load(args.config.as_deref()) {
        Ok(Some(c)) => c,
        Ok(None) => {
            match config_file::create_template(args.config.as_deref()) {
                Ok(path) => eprintln!(
                    "未找到配置文件，已生成模板：{}\n\
                     请填写 [embedding] 的 api_key（需要的话再填 comfyui.output_dir）后重新启动。",
                    path.display()
                ),
                Err(e) => eprintln!("生成配置文件失败: {e:#}"),
            }
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("配置文件加载失败: {e:#}");
            std::process::exit(1);
        }
    };

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
        qdrant_url: args.qdrant_url.clone(),
        qdrant_collection: args.qdrant_collection.clone(),
        thumb_dir: args.data_dir.join("thumbs"),
        export_dir: args.data_dir.join("exports"),
        dim: args
            .dim
            .or_else(|| file_cfg.embedding_dimensions())
            .unwrap_or(4096),
        sparse_enabled: args.sparse,
        quant: QuantMode::parse(&args.quant),
        mock: args.mock,
        embedding_base_url: args
            .embedding_base_url
            .clone()
            .or_else(|| file_cfg.embedding_base_url())
            .unwrap_or_else(|| "https://api.siliconflow.cn/v1".to_string()),
        embedding_model: args
            .embedding_model
            .clone()
            .or_else(|| file_cfg.embedding_model())
            .unwrap_or_else(|| "Qwen/Qwen3-VL-Embedding-8B".to_string()),
        embedding_api_key: args
            .embedding_api_key
            .clone()
            .or_else(|| file_cfg.embedding_api_key()),
        embedding_timeout_secs: file_cfg.embedding_timeout_secs().unwrap_or(60),
        web_dir: args.web_dir.clone(),
        max_media_bytes: args.max_media_bytes,
        comfyui_output_dir: file_cfg.comfyui_output_dir(),
        import_copy: file_cfg.import_copy(),
    };

    let state = match AppState::new(cfg).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("启动失败: {e:#}");
            std::process::exit(1);
        }
    };

    print_banner(&state, &flags);

    // 配置的 ComfyUI 目录不存在不阻止启动（可能只是还没接上），但要说清楚。
    match &state.cfg.comfyui_output_dir {
        Some(dir) if dir.is_dir() => {
            tracing::info!(path = %dir.display(), "ComfyUI output 目录已配置（导入作业不填 root 时默认扫这里）")
        }
        Some(dir) => {
            tracing::warn!(path = %dir.display(), "配置的 ComfyUI output 目录不存在：导入作业会直接失败")
        }
        None => tracing::info!("未配置 ComfyUI output 目录（config.toml 的 [comfyui] output_dir）"),
    }

    // ---- 启动自检：编码指纹 + 探测（开发文档 §9.4）----
    // 编码已改为第三方 API：这里打印指纹，并主动探测一次连通性。
    tracing::info!(
        fingerprint = %state.encoder.fingerprint_hint(),
        provider = state.encoder.provider(),
        model = ?state.encoder.model(),
        "embedding 后端就绪（第三方 API）"
    );

    let golden = state.encoder.probe().await;
    tracing::info!(status = %golden.status, detail = ?golden.detail, "金标探测结果");
    state.set_golden(golden.clone());
    if golden.status == "failed" {
        // 云端不可用不等于服务不能起：health 会显示 DEGRADED，导入/编码作业会被金标闸门拦住。
        eprintln!(
            "embedding 探测失败（status=failed）：{}；服务继续启动，但编码/导入作业会被拒绝",
            golden.detail.clone().unwrap_or_default()
        );
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
    tracing::info!("收到退出信号：等待在途请求结束…");
    // 向量数据由 Qdrant 服务端负责持久化，这里不再需要显式 flush。
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
    let mock = state.encoder.is_mock();
    let base_url = state.encoder.base_url().unwrap_or("(未配置)");
    let model = state.encoder.model().unwrap_or("(未配置)");
    println!("──────────────────────────────────────────────");
    println!("prompt-vault {}  http://{}:{}", env!("CARGO_PKG_VERSION"), flags.host, flags.port);
    println!(
        "  编码后端 : {}",
        if mock { "mock（伪向量）" } else { "第三方 embedding（OpenAI 兼容）" }
    );
    println!("  服务地址 : {base_url}");
    println!("  模型     : {model}");
    println!("  SQLite   : {}", state.cfg.db_path.display());
    println!(
        "  向量索引 : {}  collection={}  dim={} 量化={} 稀疏={}",
        state.vectors.url(),
        state.vectors.collection(),
        state.cfg.dim,
        state.vectors.quantization().as_str(),
        sparse
    );
    println!("  索引指纹 : {index_fp}");
    println!("  媒体目录 : {}", state.cfg.media_dir.display());
    println!(
        "  ComfyUI  : {}",
        state
            .cfg
            .comfyui_output_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(未配置)".into())
    );
    println!(
        "  导入模式 : {}",
        if state.cfg.import_copy { "复制（保留源文件）" } else { "移动" }
    );
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
