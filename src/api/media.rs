//! 媒体流：`GET /media/{**path}` —— **必须挂在根路由**（开发文档 §7.1）。
//!
//! 支持 `?w=256` 生成缩略图（缓存到 `data/thumbs/`），大图不重复解码。

use salvo::fs::NamedFile;
use salvo::prelude::*;
use sha2::{Digest, Sha256};

use crate::error::ApiError;

#[handler]
pub async fn serve_media(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let state = match depot.get_typed::<std::sync::Arc<crate::state::AppState>>() {
        Ok(s) => s.clone(),
        Err(_) => {
            res.status_code(salvo::http::StatusCode::INTERNAL_SERVER_ERROR);
            return;
        }
    };

    let rel = req.param::<String>("path").unwrap_or_default();
    if rel.is_empty() {
        res.status_code(salvo::http::StatusCode::BAD_REQUEST);
        res.render(Text::Plain("缺少媒体路径"));
        return;
    }
    let rel = match state.cfg.safe_rel_path(&rel) {
        Ok(r) => r,
        Err(e) => {
            res.status_code(salvo::http::StatusCode::BAD_REQUEST);
            res.render(Text::Plain(e.message));
            return;
        }
    };

    let path = state.cfg.media_path(&rel);
    if !path.is_file() {
        res.status_code(salvo::http::StatusCode::NOT_FOUND);
        res.render(Text::Plain(format!("媒体不存在: {rel}")));
        return;
    }

    // 缩略图：只对图片生效
    if let Some(w) = req.query::<u32>("w").filter(|w| *w > 0 && *w < 4096) {
        let is_video = rel.to_ascii_lowercase().ends_with(".mp4")
            || rel.to_ascii_lowercase().ends_with(".webm")
            || rel.to_ascii_lowercase().ends_with(".mov");
        if !is_video {
            match thumbnail(&state, &rel, &path, w) {
                Ok(thumb_path) => {
                    match NamedFile::open(&thumb_path).await {
                        Ok(file) => {
                            file.send(req.headers(), res).await;
                            return;
                        }
                        Err(e) => tracing::debug!(error = %e, "缩略图发送失败"),
                    }
                    return;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "缩略图生成失败，回落原图");
                }
            }
        }
    }

    match NamedFile::open(&path).await {
        Ok(file) => {
            // Salvo 1.0 的 NamedFile::send 自己处理 Range / Etag / Last-Modified，
            // 所以 <video src> 拖进度条是有效的。
            file.send(req.headers(), res).await;
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "媒体打开失败");
            res.status_code(salvo::http::StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Text::Plain("媒体打开失败"));
        }
    }
}

/// 生成并缓存缩略图；键含 mtime，源文件变了自动失效。
fn thumbnail(
    state: &std::sync::Arc<crate::state::AppState>,
    rel: &str,
    src: &std::path::Path,
    target_w: u32,
) -> Result<std::path::PathBuf, ApiError> {
    let mtime = std::fs::metadata(src)
        .map(|m| {
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0)
        })
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(format!("{rel}|{mtime}|{target_w}").as_bytes());
    let key = hex::encode(&hasher.finalize()[..16]);
    let out = state.cfg.thumb_dir.join(format!("{key}.jpg"));
    if out.is_file() {
        return Ok(out);
    }

    let img = image::open(src)
        .map_err(|e| ApiError::bad_request(format!("无法解码图片 {rel}: {e}")))?;
    // `thumbnail` 保持宽高比缩放到不超过 target_w×target_w 的框内 —— 不裁剪。
    let thumb = img.thumbnail(target_w, target_w);
    // 默认 JPEG 质量是 75，放大到卡片尺寸后观感偏糊，这里提到 85
    let file = std::fs::File::create(&out)
        .map_err(|e| ApiError::internal(format!("缩略图创建失败: {e}")))?;
    let mut writer = std::io::BufWriter::new(file);
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, 85);
    encoder
        .encode_image(&thumb)
        .map_err(|e| ApiError::internal(format!("缩略图写入失败: {e}")))?;
    Ok(out)
}
