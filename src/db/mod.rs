//! SQLite 层：schema、连接、行结构、共享工具。
//!
//! 元数据只在 SQLite（开发文档 §8）；向量本体在 Qdrant Edge，这里只留**指纹**。

pub mod assets;
pub mod jobs;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::Connection;

use crate::error::{ApiError, ApiResult};

pub const SCHEMA_SQL: &str = include_str!("../../migrations/0001_init.sql");

/// 0002：`kind` 允许 `other`、`sha256` → `hash` + `hash_algo`、清库重导。
///
/// ⚠️ 这份脚本会**删掉全部资产数据**，只能执行一次，靠 `apply_migrations` 里的列存在性判断兜住。
pub const MIGRATION_0002_SQL: &str = include_str!("../../migrations/0002_hash_blake3.sql");

/// 当前 schema 版本（写进 `meta.schema_version`，仅供排查）。
pub const SCHEMA_VERSION: &str = "2";

/// 元数据键
pub const META_INDEX_FINGERPRINT: &str = "index_fingerprint";
pub const META_SCHEMA_VERSION: &str = "schema_version";
pub const META_LAST_GOLDEN: &str = "last_golden";
pub const META_SPARSE_ENABLED: &str = "sparse_enabled";

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    path: PathBuf,
}

impl Db {
    /// 打开（必要时创建）数据库并应用 schema。
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(SCHEMA_SQL)?;
        apply_migrations(&conn)?;
        let db = Self { conn: Arc::new(Mutex::new(conn)), path };
        db.set_meta(META_SCHEMA_VERSION, SCHEMA_VERSION)?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 拿连接（短临界区；**不要在 async 上下文里跨 await 持有**）。
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        match self.conn.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> ApiResult<T> {
        let guard = self.lock();
        f(&guard).map_err(ApiError::from)
    }

    // ------------------------------------------------------------ meta

    pub fn get_meta(&self, key: &str) -> ApiResult<Option<String>> {
        self.with_conn(|c| {
            c.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })
        })
    }

    pub fn set_meta(&self, key: &str, value: &str) -> ApiResult<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                (key, value),
            )
            .map(|_| ())
        })
    }

    pub fn delete_meta(&self, key: &str) -> ApiResult<()> {
        self.with_conn(|c| c.execute("DELETE FROM meta WHERE key = ?1", [key]).map(|_| ()))
    }

    /// 进程重启后，把上次遗留的 running 作业标成 paused（§6 断点续传：能续，不假装在跑）。
    pub fn reconcile_jobs_on_start(&self) -> ApiResult<usize> {
        let now = now_rfc3339();
        self.with_conn(|c| {
            c.execute(
                "UPDATE import_job SET status = 'paused', message = ?1, updated_at = ?2
                 WHERE status IN ('running', 'pending')",
                ("进程重启，已暂停，可 resume", now),
            )
        })
    }
}

// ---------------------------------------------------------------- 迁移

/// 把 schema 推进到最新版本。
///
/// 0001 用的是 `CREATE TABLE IF NOT EXISTS`，老库不会被它改写，所以 0002 单独按需执行：
/// 以「asset 表里有没有 hash 列」作为「0002 已应用」的标记（0002 会清空资产数据，不能重复跑）。
fn apply_migrations(conn: &Connection) -> rusqlite::Result<()> {
    if has_column(conn, "asset", "hash")? {
        return Ok(());
    }
    // 重建 asset 表要 DROP 再 RENAME。外键开关只能在事务外切换（事务内设置是 no-op），
    // 而 `Db::open` 一进来就把它打开了 —— 这里单独关一次，迁移完再打开。
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let result = (|| {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        if let Err(e) = conn.execute_batch(MIGRATION_0002_SQL) {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
        conn.execute_batch("COMMIT")
    })();
    conn.pragma_update(None, "foreign_keys", "ON")?;
    result?;

    // 迁移后自检：asset 表是重建出来的，任何悬空引用都应该在这里暴露出来。
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let mut rows = stmt.query([])?;
    if rows.next()?.is_some() {
        tracing::warn!("迁移 0002 后 PRAGMA foreign_key_check 报告了悬空引用，请检查 asset 相关表");
    }
    Ok(())
}

/// 表里是否存在某列（SQLite 没有 `information_schema`，只能问 `pragma_table_info`）。
fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let sql = format!(
        "SELECT 1 FROM pragma_table_info('{}') WHERE name = ?1 LIMIT 1",
        table.replace('\'', "''")
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.exists([column])
}

// ---------------------------------------------------------------- 行结构

#[derive(Debug, Clone)]
pub struct AssetRow {
    pub id: i64,
    pub kind: String,
    pub rel_path: String,
    pub bytes: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    /// 内容指纹（blake3）；>100MB 的文件只对前 100MB 求值
    pub hash: Option<String>,
    /// 指纹算法标识；迁移 0002 起恒为 `blake3`
    pub hash_algo: String,
    pub nsfw: bool,
    pub added_at: String,
}

impl AssetRow {
    pub fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            kind: row.get("kind")?,
            rel_path: row.get("rel_path")?,
            bytes: row.get("bytes")?,
            width: row.get("width")?,
            height: row.get("height")?,
            duration_ms: row.get("duration_ms")?,
            hash: row.get("hash")?,
            hash_algo: row.get("hash_algo")?,
            nsfw: row.get::<_, i64>("nsfw")? != 0,
            added_at: row.get("added_at")?,
        })
    }

    pub fn media_url(&self) -> String {
        format!("/media/{}", encode_media_path(&self.rel_path))
    }

    pub fn thumb_url(&self) -> String {
        if self.kind == "image" {
            format!("/media/{}?w=256", encode_media_path(&self.rel_path))
        } else {
            self.media_url()
        }
    }
}

#[derive(Debug, Clone)]
pub struct PromptRow {
    pub id: i64,
    pub asset_id: i64,
    pub role: String,
    pub text: String,
    pub created_at: String,
}

impl PromptRow {
    pub fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            asset_id: row.get("asset_id")?,
            role: row.get("role")?,
            text: row.get("text")?,
            created_at: row.get("created_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct VectorStateRow {
    pub fingerprint: String,
    pub dim: i64,
    pub resized: Option<String>,
    pub vision_tokens: Option<i64>,
    pub encoded_at: String,
}

impl VectorStateRow {
    pub fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            fingerprint: row.get("fingerprint")?,
            dim: row.get("dim")?,
            resized: row.get("resized")?,
            vision_tokens: row.get("vision_tokens")?,
            encoded_at: row.get("encoded_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub root: Option<String>,
    pub total: i64,
    pub done: i64,
    pub failed: i64,
    pub skipped: i64,
    pub cursor: Option<String>,
    pub fingerprint: Option<String>,
    pub params: Option<String>,
    pub message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

impl JobRow {
    pub fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            kind: row.get("kind")?,
            status: row.get("status")?,
            root: row.get("root")?,
            total: row.get("total")?,
            done: row.get("done")?,
            failed: row.get("failed")?,
            skipped: row.get("skipped")?,
            cursor: row.get("cursor")?,
            fingerprint: row.get("fingerprint")?,
            params: row.get("params")?,
            message: row.get("message")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
            finished_at: row.get("finished_at")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct JobErrorRow {
    pub rel_path: Option<String>,
    pub code: String,
    pub message: String,
    pub at: String,
}

// ---------------------------------------------------------------- 工具

/// RFC3339 UTC，秒精度，形如 `2026-10-16T12:00:00Z`（契约里时间就这格式）。
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// 解析 RFC3339 到 epoch 毫秒（作业速率/ETA 用）。
pub fn parse_epoch_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}

pub fn epoch_ms_to_rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// 媒体 URL 里的路径：逐段 percent-encode，保留 `/`。
pub fn encode_media_path(rel_path: &str) -> String {
    rel_path
        .split('/')
        .map(url_encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn url_encode_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b' ');
        if keep {
            if b == b' ' {
                out.push_str("%20");
            } else {
                out.push(b as char);
            }
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 把用户给的 `?w=256` 解析成 (宽, 高)，两者都给时按 contain 缩放。
pub fn parse_thumb_size(w: Option<u32>, h: Option<u32>) -> Option<(u32, u32)> {
    match (w, h) {
        (Some(w), Some(h)) => Some((w, h)),
        (Some(w), None) => Some((w, w)),
        (None, Some(h)) => Some((h, h)),
        (None, None) => None,
    }
}
