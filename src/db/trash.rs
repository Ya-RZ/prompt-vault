//! 回收站：删除的资产整行迁移到 `deleted_asset`，媒体文件移到 `data/trash/`。
//!
//! 设计取舍：
//! - **整行迁移**（含提示词 JSON），恢复时不依赖 media 里的文件是否还在；
//! - 恢复后资产没有任何向量（删除时 Qdrant 点已删），由用户/作业重新编码；
//! - 导入时按 `hash` 查这张表，命中就跳过，避免把刚删掉的内容又导回来。

use rusqlite::{params, OptionalExtension};

use crate::db::{now_rfc3339, AssetRow, Db, PromptRow};
use crate::error::ApiResult;

const TRASH_COLS: &str = "id, asset_id, kind, rel_path, title, bytes, width, height, duration_ms, \
     hash, hash_algo, nsfw, positive_prompt, negative_prompt, workflow, prompts_json, \
     trash_path, added_at, deleted_at";

#[derive(Debug, Clone)]
pub struct DeletedAssetRow {
    pub id: i64,
    pub asset_id: i64,
    pub kind: String,
    pub rel_path: String,
    pub title: Option<String>,
    pub bytes: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    pub hash: Option<String>,
    pub hash_algo: String,
    pub nsfw: bool,
    pub positive_prompt: Option<String>,
    pub negative_prompt: Option<String>,
    pub workflow: Option<String>,
    pub prompts_json: Option<String>,
    pub trash_path: String,
    pub added_at: String,
    pub deleted_at: String,
}

impl DeletedAssetRow {
    pub fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            asset_id: row.get("asset_id")?,
            kind: row.get("kind")?,
            rel_path: row.get("rel_path")?,
            title: row.get("title")?,
            bytes: row.get("bytes")?,
            width: row.get("width")?,
            height: row.get("height")?,
            duration_ms: row.get("duration_ms")?,
            hash: row.get("hash")?,
            hash_algo: row.get("hash_algo")?,
            nsfw: row.get::<_, i64>("nsfw")? != 0,
            positive_prompt: row.get("positive_prompt")?,
            negative_prompt: row.get("negative_prompt")?,
            workflow: row.get("workflow")?,
            prompts_json: row.get("prompts_json")?,
            trash_path: row.get("trash_path")?,
            added_at: row.get("added_at")?,
            deleted_at: row.get("deleted_at")?,
        })
    }

    pub fn prompts(&self) -> Vec<(String, String)> {
        let Some(raw) = self.prompts_json.as_deref() else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<serde_json::Value>>(raw)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| {
                Some((
                    v.get("role")?.as_str()?.to_string(),
                    v.get("text")?.as_str()?.to_string(),
                ))
            })
            .collect()
    }
}

impl Db {
    /// 把资产整行迁移到回收站（事务内），返回回收站行 id。
    pub fn move_asset_to_trash(&self, asset_id: i64, trash_path: &str) -> ApiResult<i64> {
        let now = now_rfc3339();
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let asset = tx
                .query_row(
                    &format!("SELECT {ASSET_COLS} FROM asset WHERE id = ?1"),
                    [asset_id],
                    AssetRow::from_row,
                )
                .optional()?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;

            let mut stmt = tx.prepare(
                "SELECT id, asset_id, role, text, created_at FROM prompt WHERE asset_id = ?1 ORDER BY id ASC",
            )?;
            let prompts: Vec<PromptRow> = stmt
                .query_map([asset_id], PromptRow::from_row)?
                .collect::<rusqlite::Result<_>>()?;
            drop(stmt);
            let prompts_json = serde_json::to_string(
                &prompts
                    .iter()
                    .map(|p| serde_json::json!({"role": p.role, "text": p.text}))
                    .collect::<Vec<_>>(),
            )
            .ok();

            tx.execute(
                "INSERT INTO deleted_asset
                 (asset_id, kind, rel_path, title, bytes, width, height, duration_ms,
                  hash, hash_algo, nsfw, positive_prompt, negative_prompt, workflow,
                  prompts_json, trash_path, added_at, deleted_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
                params![
                    asset.id,
                    asset.kind,
                    asset.rel_path,
                    asset.title,
                    asset.bytes,
                    asset.width,
                    asset.height,
                    asset.duration_ms,
                    asset.hash,
                    asset.hash_algo,
                    asset.nsfw as i64,
                    asset.positive_prompt,
                    asset.negative_prompt,
                    asset.workflow,
                    prompts_json,
                    trash_path,
                    asset.added_at,
                    now
                ],
            )?;
            let trash_id = tx.last_insert_rowid();
            tx.execute("DELETE FROM asset WHERE id = ?1", [asset_id])?;
            tx.commit()?;
            Ok(trash_id)
        })
    }

    pub fn list_trash(&self, page: i64, page_size: i64) -> ApiResult<(Vec<DeletedAssetRow>, i64)> {
        self.with_conn(|c| {
            let total: i64 =
                c.query_row("SELECT COUNT(*) FROM deleted_asset", [], |r| r.get(0))?;
            let mut stmt = c.prepare(&format!(
                "SELECT {TRASH_COLS} FROM deleted_asset ORDER BY deleted_at DESC, id DESC LIMIT ?1 OFFSET ?2"
            ))?;
            let rows = stmt
                .query_map(
                    params![page_size, (page - 1).max(0) * page_size],
                    DeletedAssetRow::from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((rows, total))
        })
    }

    pub fn get_trash(&self, id: i64) -> ApiResult<Option<DeletedAssetRow>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                &format!("SELECT {TRASH_COLS} FROM deleted_asset WHERE id = ?1"),
                [id],
                DeletedAssetRow::from_row,
            )
            .optional()?)
        })
    }

    /// 导入过滤：同内容已经在回收站里。
    pub fn find_trash_by_hash(&self, hash: &str) -> ApiResult<Option<DeletedAssetRow>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                &format!("SELECT {TRASH_COLS} FROM deleted_asset WHERE hash = ?1 ORDER BY id DESC LIMIT 1"),
                [hash],
                DeletedAssetRow::from_row,
            )
            .optional()?)
        })
    }

    /// 从回收站恢复：重建 asset 与 prompt（新 id），删除回收站行。返回新 asset id。
    pub fn restore_from_trash(&self, id: i64) -> ApiResult<i64> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let row = tx
                .query_row(
                    &format!("SELECT {TRASH_COLS} FROM deleted_asset WHERE id = ?1"),
                    [id],
                    DeletedAssetRow::from_row,
                )
                .optional()?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;

            tx.execute(
                "INSERT INTO asset
                 (kind, rel_path, title, bytes, width, height, duration_ms,
                  hash, hash_algo, nsfw, positive_prompt, negative_prompt, workflow, added_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    row.kind,
                    row.rel_path,
                    row.title,
                    row.bytes,
                    row.width,
                    row.height,
                    row.duration_ms,
                    row.hash,
                    row.hash_algo,
                    row.nsfw as i64,
                    row.positive_prompt,
                    row.negative_prompt,
                    row.workflow,
                    row.added_at
                ],
            )?;
            let new_id = tx.last_insert_rowid();
            for (role, text) in row.prompts() {
                tx.execute(
                    "INSERT INTO prompt (asset_id, role, text, created_at) VALUES (?1, ?2, ?3, ?4)",
                    params![new_id, role, text, now_rfc3339()],
                )?;
            }
            tx.execute("DELETE FROM deleted_asset WHERE id = ?1", [id])?;
            tx.commit()?;
            Ok(new_id)
        })
    }

    /// 彻底删除一条回收站记录，返回它的 trash_path（文件由调用方删除）。
    pub fn delete_trash_row(&self, id: i64) -> ApiResult<Option<String>> {
        self.with_conn(|c| {
            let path: Option<String> = c
                .query_row("SELECT trash_path FROM deleted_asset WHERE id = ?1", [id], |r| r.get(0))
                .optional()?;
            c.execute("DELETE FROM deleted_asset WHERE id = ?1", [id])?;
            Ok(path)
        })
    }

    /// 清空回收站记录，返回所有 trash_path。
    pub fn clear_trash(&self) -> ApiResult<Vec<String>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT trash_path FROM deleted_asset")?;
            let paths: Vec<String> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            c.execute("DELETE FROM deleted_asset", [])?;
            Ok(paths)
        })
    }

    pub fn count_trash(&self) -> ApiResult<i64> {
        self.with_conn(|c| c.query_row("SELECT COUNT(*) FROM deleted_asset", [], |r| r.get(0)))
    }
}

// 复用 assets.rs 的列常量（同 crate 内可见性：assets.rs 里是 const，不可直接引用，
// 所以这里复制一份等价列名）。
const ASSET_COLS: &str =
    "id, kind, rel_path, title, bytes, width, height, duration_ms, hash, hash_algo, nsfw, \
     positive_prompt, negative_prompt, workflow, added_at";
