//! 批量导入 / 编码作业：进度落库 + 断点续传（开发文档 §7.2、§11、§13 第 6 条）。
//!
//! 50 万张回填在 GPU 上也要 3~6 天（§11），所以：
//! - 状态全部落 SQLite，不只在内存；
//! - `cursor` 是断点，resume 从它之后继续；
//! - 「已完成」以 `asset_vector.fingerprint == 当前指纹` 判定，指纹变了要重编。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::types::EmbedInputItem;
use crate::vector::{VEC_IMAGE, VEC_TEXT};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportParams {
    /// 相对 media 目录的子路径；空串表示整个 media 目录
    pub root: String,
    pub recursive: bool,
    pub extensions: Vec<String>,
    pub default_prompt_role: String,
    /// sha256 | rel_path | none
    pub dedupe_by: String,
}

impl Default for ImportParams {
    fn default() -> Self {
        Self {
            root: String::new(),
            recursive: true,
            extensions: vec![
                "jpg", "jpeg", "png", "webp", "gif", "bmp", "avif", "mp4", "webm", "mov", "mkv",
            ]
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
            default_prompt_role: "caption".into(),
            dedupe_by: "sha256".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodeParams {
    pub limit: Option<i64>,
    pub only_missing: bool,
    pub batch_size: usize,
    pub asset_ids: Option<Vec<i64>>,
    pub prompt_ids: Option<Vec<i64>>,
}

impl Default for EncodeParams {
    fn default() -> Self {
        Self { limit: None, only_missing: true, batch_size: 4, asset_ids: None, prompt_ids: None }
    }
}

#[derive(Default)]
struct JobControl {
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct JobManager {
    inner: Mutex<HashMap<String, JobControl>>,
}

impl JobManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn register(&self, job_id: &str) -> JobControl {
        let control = JobControl { cancel: Arc::new(AtomicBool::new(false)), pause: Arc::new(AtomicBool::new(false)) };
        if let Ok(mut map) = self.inner.lock() {
            map.insert(job_id.to_string(), control.clone_shallow());
        }
        control
    }

    fn get(&self, job_id: &str) -> Option<JobControl> {
        self.inner.lock().ok().and_then(|m| m.get(job_id).map(|c| c.clone_shallow()))
    }

    fn unregister(&self, job_id: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(job_id);
        }
    }

    pub fn is_active(&self, job_id: &str) -> bool {
        self.get(job_id).is_some()
    }

    /// 请求暂停：作业循环在批次边界停下并把状态写成 paused。
    pub fn request_pause(&self, job_id: &str) -> bool {
        match self.get(job_id) {
            Some(c) => {
                c.pause.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    pub fn request_cancel(&self, job_id: &str) -> bool {
        match self.get(job_id) {
            Some(c) => {
                c.cancel.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    pub fn start_import(self: &Arc<Self>, state: Arc<AppState>, job_id: String, params: ImportParams) {
        let control = self.register(&job_id);
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            run_import(state, job_id.clone(), params, control).await;
            manager.unregister(&job_id);
        });
    }

    pub fn start_encode(self: &Arc<Self>, state: Arc<AppState>, job_id: String, params: EncodeParams) {
        let control = self.register(&job_id);
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            run_encode(state, job_id.clone(), params, control).await;
            manager.unregister(&job_id);
        });
    }
}

impl JobControl {
    fn clone_shallow(&self) -> Self {
        Self { cancel: Arc::clone(&self.cancel), pause: Arc::clone(&self.pause) }
    }
}

/// 批次边界的控制检查。
enum Step {
    Continue,
    Pause,
    Cancel,
}

fn check(control: &JobControl) -> Step {
    if control.cancel.load(Ordering::SeqCst) {
        Step::Cancel
    } else if control.pause.load(Ordering::SeqCst) {
        Step::Pause
    } else {
        Step::Continue
    }
}

// ---------------------------------------------------------------- 导入

async fn run_import(
    state: Arc<AppState>,
    job_id: String,
    params: ImportParams,
    control: JobControl,
) {
    if let Err(e) = state.db.set_job_status(&job_id, "running", Some("开始扫描")) {
        tracing::error!(%job_id, error = %e, "更新作业状态失败");
        return;
    }

    // 作业开始前的金标闸门（开发文档 §9.4）
    if let Err(e) = state.check_golden_gate() {
        let _ = state.db.add_job_error(&job_id, None, e.code, &e.message);
        let _ = state.db.set_job_status(&job_id, "failed", Some(&e.message));
        return;
    }

    let scan_root = if params.root.trim().is_empty() {
        state.cfg.media_dir.clone()
    } else {
        state.cfg.media_path(params.root.trim())
    };
    if !scan_root.exists() {
        let msg = format!("扫描根目录不存在: {}", scan_root.display());
        let _ = state.db.add_job_error(&job_id, None, "NOT_FOUND", &msg);
        let _ = state.db.set_job_status(&job_id, "failed", Some(&msg));
        return;
    }

    let mut files = match collect_files(&state, &scan_root, &params) {
        Ok(f) => f,
        Err(e) => {
            let _ = state.db.add_job_error(&job_id, None, "INTERNAL", &e.to_string());
            let _ = state.db.set_job_status(&job_id, "failed", Some(&e.to_string()));
            return;
        }
    };
    files.sort();

    // 断点续传：cursor 之后的才算待办
    let job = match state.db.get_job(&job_id) {
        Ok(Some(j)) => j,
        _ => return,
    };
    let resume_from = job.cursor.clone();
    let start_idx = match &resume_from {
        Some(cursor) => files.iter().position(|f| f > cursor).unwrap_or(files.len()),
        None => 0,
    };
    let remaining = &files[start_idx..];
    let _ = state.db.set_job_total(&job_id, remaining.len() as i64);

    let mut done = job.done;
    let mut failed = job.failed;
    let mut skipped = job.skipped;
    let mut dirty = 0usize;

    for rel_path in remaining {
        match check(&control) {
            Step::Cancel => {
                let _ = state.db.update_job_progress(&job_id, done, failed, skipped, Some(rel_path));
                let _ = state.db.set_job_status(&job_id, "cancelled", Some("已按请求取消"));
                return;
            }
            Step::Pause => {
                let _ = state.db.update_job_progress(&job_id, done, failed, skipped, Some(rel_path));
                let _ = state.db.set_job_status(&job_id, "paused", Some("已暂停，可 resume 续跑"));
                return;
            }
            Step::Continue => {}
        }

        let abs = state.cfg.media_path(rel_path);
        match import_one(&state, rel_path, &abs, &params) {
            Ok(ImportOutcome::Added) => done += 1,
            Ok(ImportOutcome::Skipped) => skipped += 1,
            Err(e) => {
                failed += 1;
                let _ = state.db.add_job_error(&job_id, Some(rel_path), e.code, &e.message);
                let _ = state.db.set_job_status(&job_id, "running", Some(&format!("失败: {rel_path}")));
            }
        }
        dirty += 1;
        if dirty >= 20 {
            dirty = 0;
            let _ = state.db.update_job_progress(&job_id, done, failed, skipped, Some(rel_path));
        }
    }

    let _ = state.db.update_job_progress(&job_id, done, failed, skipped, None);
    let summary = format!("导入完成：新增 {done}，跳过 {skipped}，失败 {failed}");
    let _ = state.db.set_job_status(&job_id, "succeeded", Some(&summary));
}

enum ImportOutcome {
    Added,
    Skipped,
}

fn import_one(
    state: &Arc<AppState>,
    rel_path: &str,
    abs: &std::path::Path,
    params: &ImportParams,
) -> ApiResult<ImportOutcome> {
    let meta = std::fs::metadata(abs)
        .map_err(|e| ApiError::bad_request(format!("读取文件失败: {e}")))?;
    if meta.len() > state.cfg.max_media_bytes {
        return Err(ApiError::payload_too_large(format!(
            "文件 {} 字节超过上限 {} 字节（开发文档 §12 第 8 条：按字节和像素双重设限）",
            meta.len(),
            state.cfg.max_media_bytes
        )));
    }

    let is_video = is_video(rel_path);
    let bytes = meta.len() as i64;
    let (width, height, duration_ms) = if is_video {
        (None, None, None)
    } else {
        match image::image_dimensions(abs) {
            Ok((w, h)) => (Some(w as i64), Some(h as i64), None),
            Err(e) => {
                return Err(ApiError::bad_request(format!("不是可识别的图片: {e}")));
            }
        }
    };

    // 去重（sha256 需要读全文件，代价可控：导入本来就是 IO 密集）
    let mut sha256 = None;
    if params.dedupe_by == "sha256" {
        let digest = sha256_file(abs)?;
        if let Some(existing) = find_by_sha256(state, &digest)? {
            if existing != rel_path {
                return Ok(ImportOutcome::Skipped);
            }
        }
        sha256 = Some(digest);
    }

    let kind = if is_video { "video" } else { "image" };
    let id = state.db.insert_asset(
        kind,
        rel_path,
        bytes,
        width,
        height,
        duration_ms,
        sha256.as_deref(),
    )?;
    let _ = state.db.set_asset_dimensions(id, width, height, duration_ms, sha256.as_deref());
    Ok(ImportOutcome::Added)
}

fn collect_files(
    state: &Arc<AppState>,
    scan_root: &std::path::Path,
    params: &ImportParams,
) -> ApiResult<Vec<String>> {
    let exts: Vec<String> = params.extensions.iter().map(|e| e.to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    let walker = walkdir::WalkDir::new(scan_root).max_depth(if params.recursive { 64 } else { 1 });
    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !exts.iter().any(|e| e == &ext) {
            continue;
        }
        let rel = crate::state::rel_display(path, &state.cfg.media_dir);
        if rel.is_empty() {
            continue;
        }
        out.push(rel);
    }
    Ok(out)
}

fn is_video(rel_path: &str) -> bool {
    let lower = rel_path.to_ascii_lowercase();
    [".mp4", ".webm", ".mov", ".mkv", ".avi", ".m4v"].iter().any(|e| lower.ends_with(e))
}

fn sha256_file(path: &std::path::Path) -> ApiResult<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .map_err(|e| ApiError::bad_request(format!("打开文件失败: {e}")))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| ApiError::bad_request(format!("读取文件失败: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn find_by_sha256(state: &Arc<AppState>, digest: &str) -> ApiResult<Option<String>> {
    state.db.with_conn(|c| {
        c.query_row("SELECT rel_path FROM asset WHERE sha256 = ?1 LIMIT 1", [digest], |r| r.get(0))
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
    })
}

// ---------------------------------------------------------------- 编码

async fn run_encode(
    state: Arc<AppState>,
    job_id: String,
    params: EncodeParams,
    control: JobControl,
) {
    if let Err(e) = state.db.set_job_status(&job_id, "running", Some("开始编码")) {
        tracing::error!(%job_id, error = %e, "更新作业状态失败");
        return;
    }
    if let Err(e) = state.check_golden_gate() {
        let _ = state.db.add_job_error(&job_id, None, e.code, &e.message);
        let _ = state.db.set_job_status(&job_id, "failed", Some(&e.message));
        return;
    }

    let index_fp = state.index_fingerprint().unwrap_or(None);
    let mut done = 0i64;
    let mut failed = 0i64;
    let skipped = 0i64;

    // ---- 资产（图片/视频封面）----
    let assets = if let Some(ids) = params.asset_ids.as_ref() {
        ids.iter().filter_map(|id| state.db.get_asset(*id).ok().flatten().map(|a| (a.id, a.rel_path))).collect()
    } else {
        state.db.pending_assets(params.limit, index_fp.as_deref()).unwrap_or_default()
    };
    let _ = state.db.set_job_total(&job_id, assets.len() as i64 + 1);

    let batch_size = params.batch_size.clamp(1, 32);
    for chunk in assets.chunks(batch_size) {
        match check(&control) {
            Step::Cancel => {
                let _ = state.db.set_job_status(&job_id, "cancelled", Some("已按请求取消"));
                return;
            }
            Step::Pause => {
                let _ = state.db.update_job_progress(
                    &job_id,
                    done,
                    failed,
                    skipped,
                    Some(&format!("asset:{}", chunk.first().map(|c| c.0).unwrap_or(0))),
                );
                let _ = state.db.set_job_status(&job_id, "paused", Some("已暂停，可 resume 续跑"));
                return;
            }
            Step::Continue => {}
        }

        let mut items = Vec::new();
        let mut rows = Vec::new();
        for (id, rel_path) in chunk {
            match read_image_data_url(&state, rel_path) {
                Ok(data_url) => {
                    items.push(EmbedInputItem { text: None, image: Some(data_url) });
                    rows.push((*id, rel_path.clone()));
                }
                Err(e) => {
                    failed += 1;
                    let _ = state.db.add_job_error(&job_id, Some(rel_path), e.code, &e.message);
                }
            }
        }
        if items.is_empty() {
            continue;
        }

        match state.encoder.embed(items.clone()).await {
            Ok(outcomes) => {
                for ((asset_id, rel_path), outcome) in rows.iter().zip(outcomes.iter()) {
                    for notice in &outcome.notices {
                        tracing::info!(asset_id = *asset_id, path = %rel_path, "{notice}");
                    }
                    match store_asset_vector(&state, *asset_id, outcome) {
                        Ok(()) => done += 1,
                        Err(e) => {
                            failed += 1;
                            let _ = state.db.add_job_error(&job_id, Some(rel_path), e.code, &e.message);
                        }
                    }
                }
            }
            Err(batch_error) => {
                // 批里有一张超出预算就会 413；退化为逐条，把好的救回来、坏的记账
                if batch_error.code == "PAYLOAD_TOO_LARGE" {
                    for (item, (asset_id, rel_path)) in items.iter().zip(rows.iter()) {
                        match state.encoder.embed(vec![item.clone()]).await {
                            Ok(mut outcomes) if !outcomes.is_empty() => {
                                let outcome = outcomes.remove(0);
                                for notice in &outcome.notices {
                                    tracing::info!(asset_id = *asset_id, path = %rel_path, "{notice}");
                                }
                                match store_asset_vector(&state, *asset_id, &outcome) {
                                    Ok(()) => done += 1,
                                    Err(e) => {
                                        failed += 1;
                                        let _ = state.db.add_job_error(&job_id, Some(rel_path), e.code, &e.message);
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(e) => {
                                failed += 1;
                                let _ = state.db.add_job_error(&job_id, Some(rel_path), e.code, &e.message);
                            }
                        }
                    }
                } else {
                    failed += rows.len() as i64;
                    for (_, rel_path) in rows.iter() {
                        let _ = state.db.add_job_error(
                            &job_id,
                            Some(rel_path),
                            batch_error.code,
                            &batch_error.message,
                        );
                    }
                }
            }
        }

        // 显式 flush：EdgeShard 只在 drop 时刷盘，进程被 kill 就丢（实测过）
        if let Err(e) = state.vectors.flush() {
            tracing::warn!(error = %e, "向量 flush 失败，稍后 optimize 会再刷一次");
        }
        let cursor = format!("asset:{}", rows.last().map(|r| r.0).unwrap_or(0));
        let _ = state.db.update_job_progress(&job_id, done, failed, skipped, Some(&cursor));
    }

    // ---- 提示词 ----
    let prompts = if let Some(ids) = params.prompt_ids.as_ref() {
        ids.iter()
            .filter_map(|id| state.db.get_prompt(*id).ok().flatten())
            .map(|p| (p.id, p.asset_id, p.text))
            .collect()
    } else {
        state.db.pending_prompts(params.limit, index_fp.as_deref()).unwrap_or_default()
    };

    for chunk in prompts.chunks(batch_size) {
        match check(&control) {
            Step::Cancel => {
                let _ = state.db.set_job_status(&job_id, "cancelled", Some("已按请求取消"));
                return;
            }
            Step::Pause => {
                let _ = state.db.set_job_status(&job_id, "paused", Some("已暂停，可 resume 续跑"));
                return;
            }
            Step::Continue => {}
        }
        let items: Vec<EmbedInputItem> = chunk
            .iter()
            .map(|(_, _, text)| EmbedInputItem { text: Some(text.clone()), image: None })
            .collect();
        match state.encoder.embed(items).await {
            Ok(outcomes) => {
                for ((prompt_id, asset_id, text), outcome) in chunk.iter().zip(outcomes.iter()) {
                    match store_prompt_vector(&state, *prompt_id, *asset_id, outcome) {
                        Ok(()) => done += 1,
                        Err(e) => {
                            failed += 1;
                            let short: String = text.chars().take(60).collect();
                            let _ = state.db.add_job_error(&job_id, Some(&short), e.code, &e.message);
                        }
                    }
                }
            }
            Err(e) => {
                failed += chunk.len() as i64;
                let _ = state.db.add_job_error(&job_id, None, e.code, &e.message);
            }
        }
        if let Err(e) = state.vectors.flush() {
            tracing::warn!(error = %e, "向量 flush 失败");
        }
        let cursor = format!("prompt:{}", chunk.last().map(|c| c.0).unwrap_or(0));
        let _ = state.db.update_job_progress(&job_id, done, failed, skipped, Some(&cursor));
    }

    let _ = state.vectors.flush();
    let _ = state.db.update_job_progress(&job_id, done, failed, skipped, None);
    let summary = format!("编码完成：成功 {done}，失败 {failed}");
    let _ = state.db.set_job_status(&job_id, "succeeded", Some(&summary));
}

fn read_image_data_url(state: &Arc<AppState>, rel_path: &str) -> ApiResult<String> {
    let path = state.cfg.media_path(rel_path);
    let bytes = std::fs::read(&path)
        .map_err(|e| ApiError::bad_request(format!("读取媒体失败 {}: {e}", path.display())))?;
    if bytes.len() as u64 > state.cfg.max_media_bytes {
        return Err(ApiError::payload_too_large(format!(
            "{} 字节超过上限 {}",
            bytes.len(),
            state.cfg.max_media_bytes
        )));
    }
    Ok(crate::encode::image_data_url(&bytes, crate::encode::mime_for_path(rel_path)))
}

fn store_asset_vector(
    state: &Arc<AppState>,
    asset_id: i64,
    outcome: &crate::encode::EncodeOutcome,
) -> ApiResult<()> {
    state.ensure_index_fingerprint(&outcome.fingerprint)?;
    let asset = state.db.get_asset(asset_id)?;
    let tags = state.db.tags_for(asset_id).unwrap_or_default();
    let (nsfw, added_at_ms) = match asset.as_ref() {
        Some(a) => (a.nsfw, crate::db::parse_epoch_ms(&a.added_at).unwrap_or(0)),
        None => (false, 0),
    };
    let sparse = outcome
        .sparse
        .as_ref()
        .map(|(idx, val)| (idx.as_slice(), val.as_slice()));
    state.vectors.upsert_asset(
        asset_id,
        &outcome.dense,
        sparse,
        &outcome.fingerprint,
        nsfw,
        &tags,
        added_at_ms,
        "",
    )?;
    state.db.upsert_asset_vector(
        asset_id,
        &outcome.fingerprint,
        outcome.dense.len() as i64,
        outcome.resized.as_deref(),
        outcome.vision_tokens,
    )
}

fn store_prompt_vector(
    state: &Arc<AppState>,
    prompt_id: i64,
    asset_id: i64,
    outcome: &crate::encode::EncodeOutcome,
) -> ApiResult<()> {
    state.ensure_index_fingerprint(&outcome.fingerprint)?;
    let tags = state.db.tags_for(asset_id).unwrap_or_default();
    let nsfw = state.db.get_asset(asset_id)?.map(|a| a.nsfw).unwrap_or(false);
    let sparse = outcome
        .sparse
        .as_ref()
        .map(|(idx, val)| (idx.as_slice(), val.as_slice()));
    state
        .vectors
        .upsert_prompt(prompt_id, asset_id, &outcome.dense, sparse, &outcome.fingerprint, nsfw, &tags)?;
    state.db.upsert_prompt_vector(prompt_id, &outcome.fingerprint, outcome.dense.len() as i64)
}

/// 作业进度（百分比 / 速率 / ETA）—— 供 API 层组装。
pub fn progress_of(job: &crate::db::JobRow) -> crate::types::JobProgress {
    let total = job.total.max(0) as f64;
    let done = job.done.max(0) as f64;
    let percent = if total > 0.0 { (done / total * 100.0 * 10.0).round() / 10.0 } else { 0.0 };
    let elapsed_ms = crate::db::parse_epoch_ms(&job.updated_at)
        .zip(crate::db::parse_epoch_ms(&job.created_at))
        .map(|(u, c)| (u - c).max(1) as f64)
        .unwrap_or(1.0);
    let rate_per_minute = if done > 0.0 { done / (elapsed_ms / 60_000.0) } else { 0.0 };
    let eta_seconds = if rate_per_minute > 0.0 && total > done {
        Some(((total - done) / rate_per_minute * 60.0 * 10.0).round() / 10.0)
    } else {
        None
    };
    crate::types::JobProgress {
        percent,
        eta_seconds,
        rate_per_minute: if rate_per_minute > 0.0 { Some((rate_per_minute * 100.0).round() / 100.0) } else { None },
    }
}

pub fn job_to_dto(job: &crate::db::JobRow, errors: Vec<crate::types::JobErrorDto>) -> crate::types::JobDto {
    crate::types::JobDto {
        id: job.id.clone(),
        kind: job.kind.clone(),
        status: job.status.clone(),
        root: job.root.clone(),
        total: job.total,
        done: job.done,
        failed: job.failed,
        skipped: job.skipped,
        cursor: job.cursor.clone(),
        fingerprint: job.fingerprint.clone(),
        message: job.message.clone(),
        created_at: job.created_at.clone(),
        updated_at: job.updated_at.clone(),
        finished_at: job.finished_at.clone(),
        params: job.params.as_ref().and_then(|p| serde_json::from_str(p).ok()),
        progress: progress_of(job),
        errors,
    }
}

/// 编码能力摘要（作业开始时记录到 message，便于事后归因）。
pub fn encoder_capability_note(state: &Arc<AppState>) -> serde_json::Value {
    let candle = state.encoder.candle_encoder();
    json!({
        "mode": "candle",
        "device": candle.as_ref().map(|c| c.device_label().to_string()),
        "dtype": candle.as_ref().map(|c| c.dtype_name().to_string()),
        "fingerprint": state.encoder.fingerprint_hint(),
        "vector_names": [VEC_IMAGE, VEC_TEXT],
        "sparse_enabled_in_index": state.vectors.sparse_enabled(),
    })
}
