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
    /// 扩展名白名单（小写、不带点）。**空 = 不过滤**，任意类型都可导入（进 `other` 桶）。
    pub extensions: Vec<String>,
    pub default_prompt_role: String,
    /// 去重开关：`none` 关闭 hash 去重；其余值（兼容旧请求里的 `sha256` / `rel_path`）一律按 blake3 去重。
    /// 注意指纹无论如何都会算 —— 归档目标路径 `<hash 前两位>/<bucket>/` 依赖它。
    pub dedupe_by: String,
}

impl Default for ImportParams {
    fn default() -> Self {
        Self {
            root: String::new(),
            recursive: true,
            // 空 = 不过滤：图片/视频之外的扩展名会落到 `other` 桶（需求 §4 的三分桶）。
            // 想只导入特定类型时，请求里显式传 extensions 即可。
            extensions: Vec::new(),
            default_prompt_role: "caption".into(),
            dedupe_by: HASH_ALGO.into(),
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

    let scan_root = resolve_import_root(&params.root, &state.cfg.media_dir);
    if !scan_root.exists() {
        let msg = format!("扫描根目录不存在: {}", scan_root.display());
        let _ = state.db.add_job_error(&job_id, None, "NOT_FOUND", &msg);
        let _ = state.db.set_job_status(&job_id, "failed", Some(&msg));
        return;
    }

    let mut files = match collect_files(&scan_root, &params) {
        Ok(f) => f,
        Err(e) => {
            let _ = state.db.add_job_error(&job_id, None, "INTERNAL", &e.to_string());
            let _ = state.db.set_job_status(&job_id, "failed", Some(&e.to_string()));
            return;
        }
    };
    files.sort_by(|a, b| a.label.cmp(&b.label));

    // 断点续传：cursor 之后的才算待办
    let job = match state.db.get_job(&job_id) {
        Ok(Some(j)) => j,
        _ => return,
    };
    let resume_from = job.cursor.clone();
    let start_idx = match &resume_from {
        Some(cursor) => files.iter().position(|f| &f.label > cursor).unwrap_or(files.len()),
        None => 0,
    };
    let remaining = &files[start_idx..];
    let _ = state.db.set_job_total(&job_id, remaining.len() as i64);

    let mut done = job.done;
    let mut failed = job.failed;
    let mut skipped = job.skipped;
    let mut dirty = 0usize;

    for file in remaining {
        match check(&control) {
            Step::Cancel => {
                let _ = state.db.update_job_progress(&job_id, done, failed, skipped, Some(&file.label));
                let _ = state.db.set_job_status(&job_id, "cancelled", Some("已按请求取消"));
                return;
            }
            Step::Pause => {
                let _ = state.db.update_job_progress(&job_id, done, failed, skipped, Some(&file.label));
                let _ = state.db.set_job_status(&job_id, "paused", Some("已暂停，可 resume 续跑"));
                return;
            }
            Step::Continue => {}
        }

        match import_one(state.as_ref(), &file.path, &file.label, &params) {
            Ok(ImportOutcome::Added) => done += 1,
            Ok(ImportOutcome::Skipped) => skipped += 1,
            Err(e) => {
                failed += 1;
                let _ = state.db.add_job_error(&job_id, Some(&file.label), e.code, &e.message);
                let _ = state
                    .db
                    .set_job_status(&job_id, "running", Some(&format!("失败: {}", file.label)));
            }
        }
        dirty += 1;
        if dirty >= 20 {
            dirty = 0;
            let _ = state.db.update_job_progress(&job_id, done, failed, skipped, Some(&file.label));
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
    state: &AppState,
    src: &std::path::Path,
    label: &str,
    params: &ImportParams,
) -> ApiResult<ImportOutcome> {
    let meta = std::fs::metadata(src)
        .map_err(|e| ApiError::bad_request(format!("读取文件失败 {label}: {e}")))?;
    if meta.len() > state.cfg.max_media_bytes {
        return Err(ApiError::payload_too_large(format!(
            "文件 {} 字节超过上限 {} 字节（开发文档 §12 第 8 条：按字节和像素双重设限）",
            meta.len(),
            state.cfg.max_media_bytes
        )));
    }

    // 需求 4：源文件已经落在资产库（media 或其子目录）里 → 跳过，避免自己搬自己。
    if is_inside_media(&state.cfg.media_dir, src) {
        return Ok(ImportOutcome::Skipped);
    }

    // kind 先按扩展名判，图片再试着读尺寸；读不出尺寸的归到 other（迁移 0002 起允许）。
    let (kind, width, height) = probe_kind(src);

    // 需求 4/5：blake3（>100MB 只读前 100MB）。目标路径依赖它，所以**总要算**，
    // 与 dedupe_by 无关；dedupe_by 只决定要不要拿它去库里查重。
    let hash = blake3_file(src)?;
    if params.dedupe_by != "none" && state.db.find_asset_by_hash(&hash)?.is_some() {
        return Ok(ImportOutcome::Skipped);
    }

    let raw_name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let file_name = sanitize_file_name(&raw_name);
    let rel = match pick_target_rel(state, &hash, &kind, &file_name)? {
        TargetChoice::New(rel) => rel,
        // 目标路径上已经是同一份内容（同 hash）→ 去重跳过
        TargetChoice::Existing(_) => return Ok(ImportOutcome::Skipped),
    };
    let target = state.cfg.media_path(&rel);
    move_file(src, &target)?;

    let bytes = meta.len() as i64;
    let id = state
        .db
        .insert_asset(&kind, &rel, bytes, width, height, None, Some(&hash))?;
    let _ = state.db.set_asset_dimensions(id, width, height, None, Some(&hash));
    Ok(ImportOutcome::Added)
}

// ---------------------------------------------------------------- 归档路径 / 哈希

/// 资产指纹算法（迁移 0002 起由 sha256 换成 blake3）。
pub const HASH_ALGO: &str = "blake3";

/// 大文件只对前 100MB 求哈希（需求 4）：库里可能混进几个 GB 的视频，
/// 全量读一遍会让导入成倍变慢，而前 100MB 已经足够区分不同文件。
pub const HASH_READ_LIMIT: u64 = 100 * 1024 * 1024;

const VIDEO_EXTS: [&str; 8] = ["mp4", "webm", "mov", "mkv", "avi", "m4v", "flv", "wmv"];
const IMAGE_EXTS: [&str; 9] = ["jpg", "jpeg", "png", "webp", "gif", "bmp", "avif", "tif", "tiff"];

/// 算文件指纹：`<= 100MB` 读全文件；`> 100MB` 只读前 100MB。
pub fn blake3_file(path: &std::path::Path) -> ApiResult<String> {
    use std::io::Read;
    let meta = std::fs::metadata(path)
        .map_err(|e| ApiError::bad_request(format!("读取文件失败 {}: {e}", path.display())))?;
    let mut file = std::fs::File::open(path)
        .map_err(|e| ApiError::bad_request(format!("打开文件失败 {}: {e}", path.display())))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut remaining = meta.len().min(HASH_READ_LIMIT);
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = file
            .read(&mut buf[..want])
            .map_err(|e| ApiError::bad_request(format!("读取文件失败 {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        remaining -= n as u64;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// 目标分桶目录名：`media/<hash 前两位>/{image|video|other}/<文件名>`。
pub fn bucket_of(kind: &str) -> &'static str {
    match kind {
        "image" => "image",
        "video" => "video",
        _ => "other",
    }
}

/// 分桶后的相对路径（相对 media 根，统一用 `/`）。
pub fn target_rel_path(hash: &str, kind: &str, file_name: &str) -> String {
    let prefix = hash.get(..2).unwrap_or("00");
    format!("{prefix}/{}/{}", bucket_of(kind), file_name)
}

/// 按扩展名 + 能否读出尺寸判定 kind；图片读不出尺寸的归到 `other`。
pub fn probe_kind(path: &std::path::Path) -> (String, Option<i64>, Option<i64>) {
    let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    if VIDEO_EXTS.contains(&ext.as_str()) {
        return ("video".into(), None, None);
    }
    if IMAGE_EXTS.contains(&ext.as_str()) {
        return match image::image_dimensions(path) {
            Ok((w, h)) => ("image".into(), Some(w as i64), Some(h as i64)),
            Err(_) => ("other".into(), None, None),
        };
    }
    ("other".into(), None, None)
}

/// 文件名清洗：去掉路径分隔符 / Windows 非法字符，保证能安全拼进目标目录。
pub fn sanitize_file_name(raw: &str) -> String {
    let base = std::path::Path::new(raw)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
                || c.is_control()
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut name = cleaned.trim().trim_matches('.').to_string();
    if name.is_empty() {
        return "unnamed".to_string();
    }
    // 大多数文件系统的单段名字上限 255 字节；留出 "<hash8>-" 前缀与 UTF-8 多字节的余量
    if name.chars().count() > 180 {
        let (stem, ext) = match name.rfind('.') {
            Some(i) => (name[..i].to_string(), name[i..].to_string()),
            None => (name.clone(), String::new()),
        };
        let ext: String = ext.chars().take(20).collect();
        let keep = 180usize.saturating_sub(ext.chars().count());
        name = format!("{}{}", stem.chars().take(keep).collect::<String>(), ext);
    }
    name
}

/// 需求 4：原始文件**移动**进资产库 —— 同盘 rename，跨盘 copy + remove。
pub fn move_file(src: &std::path::Path, dst: &std::path::Path) -> ApiResult<()> {
    if src == dst {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ApiError::internal(format!("创建目录失败 {}: {e}", parent.display())))?;
    }
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            // 跨卷（EXDEV）时 rename 一定失败，退化成 copy + remove
            std::fs::copy(src, dst).map_err(|e| {
                ApiError::internal(format!(
                    "移动文件失败（rename: {rename_err}；copy: {e}）{} → {}",
                    src.display(),
                    dst.display()
                ))
            })?;
            std::fs::remove_file(src).map_err(|e| {
                ApiError::internal(format!("已复制但删除源文件失败 {}: {e}", src.display()))
            })?;
            Ok(())
        }
    }
}

/// 目标路径的选择结果。
pub enum TargetChoice {
    /// 目标路径没人占（新文件），调用方需要把源文件移动过去
    New(String),
    /// 目标路径上已经是同一份内容（同 hash），不必再搬
    Existing(String),
}

/// 选目标相对路径：先试 `<文件名>`，被不同内容占用时改成 `<hash8>-<文件名>`。
pub fn pick_target_rel(
    state: &AppState,
    hash: &str,
    kind: &str,
    file_name: &str,
) -> ApiResult<TargetChoice> {
    let candidates = [
        target_rel_path(hash, kind, file_name),
        target_rel_path(hash, kind, &format!("{}-{file_name}", hash.get(..8).unwrap_or(hash))),
    ];
    for rel in candidates {
        let abs = state.cfg.media_path(&rel);
        if !abs.exists() {
            return Ok(TargetChoice::New(rel));
        }
        if abs.is_file() && blake3_file(&abs).map(|h| h == hash).unwrap_or(false) {
            return Ok(TargetChoice::Existing(rel));
        }
    }
    Err(ApiError::conflict(format!(
        "目标位置已被不同内容的文件占用（{hash} / {file_name}），未覆盖"
    )))
}

/// 判断某个路径是否落在资产库（media）或其子目录内。
fn is_inside_media(media_dir: &std::path::Path, path: &std::path::Path) -> bool {
    let media = std::fs::canonicalize(media_dir).unwrap_or_else(|_| media_dir.to_path_buf());
    let candidate = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    candidate.starts_with(&media)
}

/// 导入根目录：绝对路径照用；相对路径以**当前可执行文件所在目录**为基准（需求 4，不是 cwd）。
///
/// 空串沿用旧语义 = 整个 media 目录；此时所有文件都在库里，会全部走「跳过」分支。
pub fn resolve_import_root(raw: &str, media_dir: &std::path::Path) -> std::path::PathBuf {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return media_dir.to_path_buf();
    }
    let path = std::path::Path::new(trimmed);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let base = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join(path)
}

/// 扫描出来的一行：`label` 是相对扫描根的展示路径，同时用于 cursor 断点比较。
struct FoundFile {
    label: String,
    path: std::path::PathBuf,
}

fn collect_files(
    scan_root: &std::path::Path,
    params: &ImportParams,
) -> ApiResult<Vec<FoundFile>> {
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
        if !exts.is_empty() && !exts.iter().any(|e| e == &ext) {
            continue;
        }
        // 隐藏文件与系统垃圾不导入（空扩展名白名单模式下尤其需要）
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.')
            || name.eq_ignore_ascii_case("thumbs.db")
            || name.eq_ignore_ascii_case("desktop.ini")
        {
            continue;
        }
        let label = crate::state::rel_display(path, scan_root);
        if label.is_empty() {
            continue;
        }
        out.push(FoundFile { label, path: path.to_path_buf() });
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(out)
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
