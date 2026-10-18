//! 资产 / 提示词 / 标签的读写与统计（INTERFACES.md §4、§3）。

use std::collections::{HashMap, HashSet};

use rusqlite::{params, params_from_iter, types::Value};

use super::{now_rfc3339, AssetRow, Db, PromptRow, VectorStateRow};
use crate::error::{ApiError, ApiResult};
use crate::types::{AssetListQuery, CountItem};

const ASSET_COLS: &str =
    "id, kind, rel_path, bytes, width, height, duration_ms, hash, hash_algo, nsfw, added_at";

impl Db {
    // ------------------------------------------------------------ 资产

    #[allow(clippy::too_many_arguments)]
    pub fn insert_asset(
        &self,
        kind: &str,
        rel_path: &str,
        bytes: i64,
        width: Option<i64>,
        height: Option<i64>,
        duration_ms: Option<i64>,
        hash: Option<&str>,
    ) -> ApiResult<i64> {
        let now = now_rfc3339();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO asset (kind, rel_path, bytes, width, height, duration_ms, hash, hash_algo, nsfw, added_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'blake3', 0, ?8)
                 ON CONFLICT(rel_path) DO UPDATE SET
                     bytes = excluded.bytes,
                     width = COALESCE(excluded.width, asset.width),
                     height = COALESCE(excluded.height, asset.height),
                     duration_ms = COALESCE(excluded.duration_ms, asset.duration_ms),
                     hash = COALESCE(excluded.hash, asset.hash)",
                params![kind, rel_path, bytes, width, height, duration_ms, hash, now],
            )?;
            c.query_row("SELECT id FROM asset WHERE rel_path = ?1", [rel_path], |r| r.get(0))
        })
    }

    pub fn get_asset(&self, id: i64) -> ApiResult<Option<AssetRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!("SELECT {ASSET_COLS} FROM asset WHERE id = ?1"))?;
            let mut rows = stmt.query_map([id], AssetRow::from_row)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    pub fn get_asset_by_path(&self, rel_path: &str) -> ApiResult<Option<AssetRow>> {
        self.with_conn(|c| {
            let mut stmt =
                c.prepare(&format!("SELECT {ASSET_COLS} FROM asset WHERE rel_path = ?1"))?;
            let mut rows = stmt.query_map([rel_path], AssetRow::from_row)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    /// 按内容指纹查（上传去重用）：同一份文件不重复建资产。
    pub fn find_asset_by_hash(&self, hash: &str) -> ApiResult<Option<AssetRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {ASSET_COLS} FROM asset WHERE hash = ?1 ORDER BY id ASC LIMIT 1"
            ))?;
            let mut rows = stmt.query_map([hash], AssetRow::from_row)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    pub fn assets_by_ids(&self, ids: &[i64]) -> ApiResult<HashMap<i64, AssetRow>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        self.with_conn(|c| {
            let placeholders = vec!["?"; ids.len()].join(",");
            let sql = format!("SELECT {ASSET_COLS} FROM asset WHERE id IN ({placeholders})");
            let mut stmt = c.prepare(&sql)?;
            let vals: Vec<Value> = ids.iter().map(|i| Value::Integer(*i)).collect();
            let rows = stmt.query_map(params_from_iter(vals.iter()), AssetRow::from_row)?;
            let mut out = HashMap::new();
            for r in rows {
                let a = r?;
                out.insert(a.id, a);
            }
            Ok(out)
        })
    }

    pub fn list_assets(&self, q: &AssetListQuery) -> ApiResult<(Vec<AssetRow>, i64)> {
        let page = q.page.unwrap_or(1).max(1);
        let page_size = q.page_size.unwrap_or(50).clamp(1, 200);

        let mut where_parts: Vec<String> = Vec::new();
        let mut vals: Vec<Value> = Vec::new();

        if let Some(query) = q.q.as_ref().filter(|s| !s.trim().is_empty()) {
            let like = format!("%{}%", query.trim());
            where_parts.push(
                "(a.rel_path LIKE ? OR EXISTS (SELECT 1 FROM prompt p WHERE p.asset_id = a.id AND p.text LIKE ?))"
                    .to_string(),
            );
            vals.push(Value::Text(like.clone()));
            vals.push(Value::Text(like));
        }
        if let Some(kind) = q.kind.as_ref().filter(|s| !s.is_empty()) {
            where_parts.push("a.kind = ?".to_string());
            vals.push(Value::Text(kind.clone()));
        }
        match q.nsfw.as_deref() {
            Some("true") => where_parts.push("a.nsfw = 1".to_string()),
            Some("false") => where_parts.push("a.nsfw = 0".to_string()),
            _ => {}
        }
        match q.encoded.as_deref() {
            Some("true") => where_parts.push(
                "EXISTS (SELECT 1 FROM asset_vector av WHERE av.asset_id = a.id)".to_string(),
            ),
            Some("false") => where_parts.push(
                "NOT EXISTS (SELECT 1 FROM asset_vector av WHERE av.asset_id = a.id)".to_string(),
            ),
            _ => {}
        }
        for tag in q.tag.iter().filter(|t| !t.trim().is_empty()) {
            where_parts.push(
                "EXISTS (SELECT 1 FROM asset_tag at JOIN tag t ON t.id = at.tag_id
                          WHERE at.asset_id = a.id AND t.name = ?)"
                    .to_string(),
            );
            vals.push(Value::Text(tag.trim().to_string()));
        }

        let where_sql =
            if where_parts.is_empty() { String::new() } else { format!("WHERE {}", where_parts.join(" AND ")) };

        let order_sql = match q.sort.as_deref().unwrap_or("-added_at") {
            "added_at" => "ORDER BY a.added_at ASC, a.id ASC",
            "-added_at" => "ORDER BY a.added_at DESC, a.id DESC",
            "id" => "ORDER BY a.id ASC",
            "-id" => "ORDER BY a.id DESC",
            "bytes" => "ORDER BY a.bytes ASC, a.id ASC",
            "-bytes" => "ORDER BY a.bytes DESC, a.id DESC",
            _ => "ORDER BY a.added_at DESC, a.id DESC",
        };

        let offset = (page - 1) * page_size;
        self.with_conn(|c| {
            let total: i64 = c.query_row(
                &format!("SELECT COUNT(*) FROM asset a {where_sql}"),
                params_from_iter(vals.iter()),
                |r| r.get(0),
            )?;

            let sql = format!(
                "SELECT {} FROM asset a {where_sql} {order_sql} LIMIT ? OFFSET ?",
                ASSET_COLS.split(", ").map(|c| format!("a.{c}")).collect::<Vec<_>>().join(", ")
            );
            let mut all_vals = vals.clone();
            all_vals.push(Value::Integer(page_size));
            all_vals.push(Value::Integer(offset));
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(all_vals.iter()), AssetRow::from_row)?;
            let mut items = Vec::new();
            for r in rows {
                items.push(r?);
            }
            Ok((items, total))
        })
    }

    pub fn delete_asset(&self, id: i64) -> ApiResult<bool> {
        self.with_conn(|c| c.execute("DELETE FROM asset WHERE id = ?1", [id]).map(|n| n > 0))
    }

    pub fn update_asset_meta(
        &self,
        id: i64,
        nsfw: Option<bool>,
        rel_path: Option<&str>,
        width: Option<i64>,
        height: Option<i64>,
        duration_ms: Option<i64>,
    ) -> ApiResult<()> {
        let mut sets: Vec<&str> = Vec::new();
        let mut vals: Vec<Value> = Vec::new();
        if let Some(v) = nsfw {
            sets.push("nsfw = ?");
            vals.push(Value::Integer(if v { 1 } else { 0 }));
        }
        if let Some(v) = rel_path {
            sets.push("rel_path = ?");
            vals.push(Value::Text(v.to_string()));
        }
        if let Some(v) = width {
            sets.push("width = ?");
            vals.push(Value::Integer(v));
        }
        if let Some(v) = height {
            sets.push("height = ?");
            vals.push(Value::Integer(v));
        }
        if let Some(v) = duration_ms {
            sets.push("duration_ms = ?");
            vals.push(Value::Integer(v));
        }
        if sets.is_empty() {
            return Ok(());
        }
        vals.push(Value::Integer(id));
        self.with_conn(|c| {
            c.execute(
                &format!("UPDATE asset SET {} WHERE id = ?", sets.join(", ")),
                params_from_iter(vals.iter()),
            )
            .map(|_| ())
        })
    }

    /// 图片尺寸等元数据回填（编码/导入时用）。
    pub fn set_asset_dimensions(
        &self,
        id: i64,
        width: Option<i64>,
        height: Option<i64>,
        duration_ms: Option<i64>,
        hash: Option<&str>,
    ) -> ApiResult<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE asset SET
                    width = COALESCE(?2, width),
                    height = COALESCE(?3, height),
                    duration_ms = COALESCE(?4, duration_ms),
                    hash = COALESCE(?5, hash)
                 WHERE id = ?1",
                params![id, width, height, duration_ms, hash],
            )
            .map(|_| ())
        })
    }

    // ------------------------------------------------------------ 提示词

    pub fn insert_prompt(&self, asset_id: i64, role: &str, text: &str) -> ApiResult<i64> {
        let now = now_rfc3339();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO prompt (asset_id, role, text, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![asset_id, role, text, now],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn get_prompt(&self, id: i64) -> ApiResult<Option<PromptRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, asset_id, role, text, created_at FROM prompt WHERE id = ?1",
            )?;
            let mut rows = stmt.query_map([id], PromptRow::from_row)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    pub fn prompts_for(&self, asset_id: i64) -> ApiResult<Vec<PromptRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, asset_id, role, text, created_at FROM prompt
                 WHERE asset_id = ?1 ORDER BY id ASC",
            )?;
            let rows = stmt.query_map([asset_id], PromptRow::from_row)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    pub fn prompts_by_ids(&self, ids: &[i64]) -> ApiResult<HashMap<i64, PromptRow>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        self.with_conn(|c| {
            let placeholders = vec!["?"; ids.len()].join(",");
            let sql = format!(
                "SELECT id, asset_id, role, text, created_at FROM prompt WHERE id IN ({placeholders})"
            );
            let mut stmt = c.prepare(&sql)?;
            let vals: Vec<Value> = ids.iter().map(|i| Value::Integer(*i)).collect();
            let rows = stmt.query_map(params_from_iter(vals.iter()), PromptRow::from_row)?;
            let mut out = HashMap::new();
            for r in rows {
                let p = r?;
                out.insert(p.id, p);
            }
            Ok(out)
        })
    }

    /// 批量取提示词（列表页避免 N+1）。
    pub fn prompts_for_many(&self, ids: &[i64]) -> ApiResult<HashMap<i64, Vec<PromptRow>>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        self.with_conn(|c| {
            let placeholders = vec!["?"; ids.len()].join(",");
            let sql = format!(
                "SELECT id, asset_id, role, text, created_at FROM prompt
                 WHERE asset_id IN ({placeholders}) ORDER BY id ASC"
            );
            let mut stmt = c.prepare(&sql)?;
            let vals: Vec<Value> = ids.iter().map(|i| Value::Integer(*i)).collect();
            let rows = stmt.query_map(params_from_iter(vals.iter()), PromptRow::from_row)?;
            let mut out: HashMap<i64, Vec<PromptRow>> = HashMap::new();
            for r in rows {
                let p = r?;
                out.entry(p.asset_id).or_default().push(p);
            }
            Ok(out)
        })
    }

    /// 批量取向量状态。
    pub fn vectors_for_many(&self, ids: &[i64]) -> ApiResult<HashMap<i64, VectorStateRow>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        self.with_conn(|c| {
            let placeholders = vec!["?"; ids.len()].join(",");
            let sql = format!(
                "SELECT asset_id, fingerprint, dim, resized, vision_tokens, encoded_at
                 FROM asset_vector WHERE asset_id IN ({placeholders})"
            );
            let mut stmt = c.prepare(&sql)?;
            let vals: Vec<Value> = ids.iter().map(|i| Value::Integer(*i)).collect();
            let rows = stmt.query_map(params_from_iter(vals.iter()), |r| {
                Ok((
                    r.get::<_, i64>("asset_id")?,
                    VectorStateRow {
                        fingerprint: r.get("fingerprint")?,
                        dim: r.get("dim")?,
                        resized: r.get("resized")?,
                        vision_tokens: r.get("vision_tokens")?,
                        encoded_at: r.get("encoded_at")?,
                    },
                ))
            })?;
            let mut out = HashMap::new();
            for r in rows {
                let (id, v) = r?;
                out.insert(id, v);
            }
            Ok(out)
        })
    }

    pub fn encoded_prompt_ids(&self, ids: &[i64]) -> ApiResult<HashSet<i64>> {
        if ids.is_empty() {
            return Ok(HashSet::new());
        }
        self.with_conn(|c| {
            let placeholders = vec!["?"; ids.len()].join(",");
            let sql = format!("SELECT prompt_id FROM prompt_vector WHERE prompt_id IN ({placeholders})");
            let mut stmt = c.prepare(&sql)?;
            let vals: Vec<Value> = ids.iter().map(|i| Value::Integer(*i)).collect();
            let rows = stmt.query_map(params_from_iter(vals.iter()), |r| r.get::<_, i64>(0))?;
            let mut out = HashSet::new();
            for r in rows {
                out.insert(r?);
            }
            Ok(out)
        })
    }

    /// 已编码且指纹与当前索引一致 → 断点续传时跳过（开发文档 §13 第 6 条）。
    pub fn prompt_vector_fingerprint(&self, prompt_id: i64) -> ApiResult<Option<String>> {
        self.with_conn(|c| {
            c.query_row("SELECT fingerprint FROM prompt_vector WHERE prompt_id = ?1", [prompt_id], |r| r.get(0))
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })
        })
    }

    pub fn upsert_prompt_vector(&self, prompt_id: i64, fingerprint: &str, dim: i64) -> ApiResult<()> {
        let now = now_rfc3339();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO prompt_vector (prompt_id, fingerprint, dim, encoded_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(prompt_id) DO UPDATE SET
                    fingerprint = excluded.fingerprint, dim = excluded.dim, encoded_at = excluded.encoded_at",
                params![prompt_id, fingerprint, dim, now],
            )
            .map(|_| ())
        })
    }

    // ------------------------------------------------------------ 向量状态

    pub fn asset_vector_state(&self, asset_id: i64) -> ApiResult<Option<VectorStateRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT fingerprint, dim, resized, vision_tokens, encoded_at
                 FROM asset_vector WHERE asset_id = ?1",
            )?;
            let mut rows = stmt.query_map([asset_id], VectorStateRow::from_row)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    pub fn upsert_asset_vector(
        &self,
        asset_id: i64,
        fingerprint: &str,
        dim: i64,
        resized: Option<&str>,
        vision_tokens: Option<i64>,
    ) -> ApiResult<()> {
        let now = now_rfc3339();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO asset_vector (asset_id, fingerprint, dim, resized, vision_tokens, encoded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(asset_id) DO UPDATE SET
                    fingerprint = excluded.fingerprint, dim = excluded.dim,
                    resized = excluded.resized, vision_tokens = excluded.vision_tokens,
                    encoded_at = excluded.encoded_at",
                params![asset_id, fingerprint, dim, resized, vision_tokens, now],
            )
            .map(|_| ())
        })
    }

    /// 待编码清单（资产 + 提示词），按 id 排序，供作业断点续传。
    pub fn pending_assets(&self, limit: Option<i64>, fingerprint: Option<&str>) -> ApiResult<Vec<(i64, String)>> {
        self.with_conn(|c| {
            let sql = match fingerprint {
                Some(_) => "SELECT a.id, a.rel_path FROM asset a
                            LEFT JOIN asset_vector av ON av.asset_id = a.id
                            WHERE av.asset_id IS NULL OR av.fingerprint <> ?1
                            ORDER BY a.id ASC",
                None => "SELECT a.id, a.rel_path FROM asset a
                         LEFT JOIN asset_vector av ON av.asset_id = a.id
                         WHERE av.asset_id IS NULL
                         ORDER BY a.id ASC",
            };
            let mut stmt = c.prepare(sql)?;
            let map_row = |r: &rusqlite::Row<'_>| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?));
            let rows = match fingerprint {
                Some(fp) => stmt.query_map(params![fp], map_row)?,
                None => stmt.query_map([], map_row)?,
            };
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            if let Some(l) = limit {
                out.truncate(l.max(0) as usize);
            }
            Ok(out)
        })
    }

    pub fn pending_prompts(&self, limit: Option<i64>, fingerprint: Option<&str>) -> ApiResult<Vec<(i64, i64, String)>> {
        self.with_conn(|c| {
            let sql = match fingerprint {
                Some(_) => "SELECT p.id, p.asset_id, p.text FROM prompt p
                            LEFT JOIN prompt_vector pv ON pv.prompt_id = p.id
                            WHERE pv.prompt_id IS NULL OR pv.fingerprint <> ?1
                            ORDER BY p.id ASC",
                None => "SELECT p.id, p.asset_id, p.text FROM prompt p
                         LEFT JOIN prompt_vector pv ON pv.prompt_id = p.id
                         WHERE pv.prompt_id IS NULL
                         ORDER BY p.id ASC",
            };
            let mut stmt = c.prepare(sql)?;
            let map_row = |r: &rusqlite::Row<'_>| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
            };
            let rows = match fingerprint {
                Some(fp) => stmt.query_map(params![fp], map_row)?,
                None => stmt.query_map([], map_row)?,
            };
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            if let Some(l) = limit {
                out.truncate(l.max(0) as usize);
            }
            Ok(out)
        })
    }

    // ------------------------------------------------------------ 标签

    pub fn ensure_tag(&self, name: &str) -> ApiResult<i64> {
        let now = now_rfc3339();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO tag (name, created_at) VALUES (?1, ?2) ON CONFLICT(name) DO NOTHING",
                params![name, now],
            )?;
            c.query_row("SELECT id FROM tag WHERE name = ?1", [name], |r| r.get(0))
        })
    }

    pub fn set_asset_tags(&self, asset_id: i64, names: &[String]) -> ApiResult<()> {
        let clean: Vec<String> = names
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            tx.execute("DELETE FROM asset_tag WHERE asset_id = ?1", [asset_id])?;
            for name in &clean {
                tx.execute(
                    "INSERT INTO tag (name, created_at) VALUES (?1, ?2) ON CONFLICT(name) DO NOTHING",
                    params![name, now_rfc3339()],
                )?;
                tx.execute(
                    "INSERT INTO asset_tag (asset_id, tag_id)
                     SELECT ?1, id FROM tag WHERE name = ?2
                     ON CONFLICT(asset_id, tag_id) DO NOTHING",
                    params![asset_id, name],
                )?;
            }
            // 清掉不再被引用的标签
            tx.execute("DELETE FROM tag WHERE id NOT IN (SELECT tag_id FROM asset_tag)", [])?;
            tx.commit()?;
            Ok(())
        })
    }

    pub fn tags_for(&self, asset_id: i64) -> ApiResult<Vec<String>> {
        let mut map = self.tags_for_many(&[asset_id])?;
        Ok(map.remove(&asset_id).unwrap_or_default())
    }

    pub fn tags_for_many(&self, ids: &[i64]) -> ApiResult<HashMap<i64, Vec<String>>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        self.with_conn(|c| {
            let placeholders = vec!["?"; ids.len()].join(",");
            let sql = format!(
                "SELECT at.asset_id, t.name FROM asset_tag at
                 JOIN tag t ON t.id = at.tag_id
                 WHERE at.asset_id IN ({placeholders})
                 ORDER BY t.name ASC"
            );
            let mut stmt = c.prepare(&sql)?;
            let vals: Vec<Value> = ids.iter().map(|i| Value::Integer(*i)).collect();
            let rows = stmt.query_map(params_from_iter(vals.iter()), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut out: HashMap<i64, Vec<String>> = HashMap::new();
            for r in rows {
                let (aid, name) = r?;
                out.entry(aid).or_default().push(name);
            }
            Ok(out)
        })
    }

    // ------------------------------------------------------------ 统计

    pub fn count_assets(&self) -> ApiResult<(i64, i64, i64, i64, i64)> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(CASE WHEN kind = 'image' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN kind = 'video' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(nsfw), 0),
                        COALESCE(SUM(bytes), 0)
                 FROM asset",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
        })
    }

    pub fn count_prompts(&self) -> ApiResult<(i64, i64)> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT (SELECT COUNT(*) FROM prompt),
                        (SELECT COUNT(*) FROM prompt_vector)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
    }

    pub fn count_by_tag(&self, limit: i64) -> ApiResult<Vec<CountItem>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT t.name, COUNT(at.asset_id) FROM tag t
                 LEFT JOIN asset_tag at ON at.tag_id = t.id
                 GROUP BY t.id ORDER BY COUNT(at.asset_id) DESC, t.name ASC LIMIT ?1",
            )?;
            let rows = stmt.query_map([limit], |r| {
                Ok(CountItem { key: r.get(0)?, count: r.get(1)? })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    pub fn count_by_month(&self, limit: i64) -> ApiResult<Vec<CountItem>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT substr(added_at, 1, 7) AS m, COUNT(*) FROM asset
                 GROUP BY m ORDER BY m DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([limit], |r| Ok(CountItem { key: r.get(0)?, count: r.get(1)? }))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    pub fn count_by_source(&self, limit: i64) -> ApiResult<Vec<CountItem>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT CASE WHEN instr(rel_path, '/') > 0
                             THEN substr(rel_path, 1, instr(rel_path, '/') - 1)
                             ELSE '(root)' END AS src,
                        COUNT(*)
                 FROM asset GROUP BY src ORDER BY COUNT(*) DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map([limit], |r| Ok(CountItem { key: r.get(0)?, count: r.get(1)? }))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    pub fn count_by_kind(&self) -> ApiResult<Vec<CountItem>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT kind, COUNT(*) FROM asset GROUP BY kind ORDER BY kind")?;
            let rows = stmt.query_map([], |r| Ok(CountItem { key: r.get(0)?, count: r.get(1)? }))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    pub fn count_by_encoded(&self) -> ApiResult<i64> {
        self.with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM asset_vector", [], |r| r.get(0))
        })
    }

    pub fn pending_encode_count(&self) -> ApiResult<i64> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT (SELECT COUNT(*) FROM asset a
                          LEFT JOIN asset_vector av ON av.asset_id = a.id
                          WHERE av.asset_id IS NULL)
                      + (SELECT COUNT(*) FROM prompt p
                          LEFT JOIN prompt_vector pv ON pv.prompt_id = p.id
                          WHERE pv.prompt_id IS NULL)",
                [],
                |r| r.get(0),
            )
        })
    }

    pub fn media_bytes(&self) -> ApiResult<i64> {
        self.with_conn(|c| c.query_row("SELECT COALESCE(SUM(bytes), 0) FROM asset", [], |r| r.get(0)))
    }

    // ------------------------------------------------------------ 纯词法检索（零模型成本路径）

    /// 路径 / 提示词 / 标签三处模糊命中。开发文档 §1：这条路径不需要模型。
    #[allow(clippy::type_complexity)]
    pub fn lexical_search(&self, q: &str, k: usize) -> ApiResult<Vec<(i64, String, String, String, f64)>> {
        // 用户输入里的 % 和 _ 是 LIKE 的通配符；不转义的话输入一个 "%" 就能把整库捞出来
        let like = format!("%{}%", escape_like(q.trim()));
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                // 三路命中先合并，再按 asset 去重（每个 asset 只留分最高的那条），最后排序后 LIMIT。
                // 没有 ORDER BY 时 SQLite 的返回顺序不确定，而 RRF 是拿返回下标当 rank 算
                // 1/(k+rank) 的 —— 无序就等于随机加分，同一查询重复执行排名会漂。
                "WITH hits AS (
                     SELECT a.id AS aid, a.rel_path AS rel_path, 'path' AS kind,
                            a.rel_path AS hit, 1.0 AS score
                       FROM asset a WHERE a.rel_path LIKE ?1 ESCAPE '\\'
                     UNION ALL
                     SELECT a.id, a.rel_path, 'prompt', p.text, 0.95
                       FROM prompt p JOIN asset a ON a.id = p.asset_id
                      WHERE p.text LIKE ?1 ESCAPE '\\'
                     UNION ALL
                     SELECT a.id, a.rel_path, 'tag', t.name, 0.9
                       FROM asset_tag at JOIN tag t ON t.id = at.tag_id
                       JOIN asset a ON a.id = at.asset_id
                      WHERE t.name LIKE ?1 ESCAPE '\\'
                 ),
                 ranked AS (
                     SELECT aid, rel_path, kind, hit, score,
                            ROW_NUMBER() OVER (PARTITION BY aid ORDER BY score DESC, kind) AS rn
                       FROM hits
                 )
                 SELECT aid, rel_path, kind, hit, score
                   FROM ranked WHERE rn = 1
                  ORDER BY score DESC, aid ASC
                  LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![like, k as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, f64>(4)?,
                ))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// 提示词精确文本兜底（写入 Qdrant 失败时仍可从 SQLite 找回）。
    pub fn prompts_matching(&self, q: &str, k: usize) -> ApiResult<Vec<i64>> {
        let like = format!("%{}%", q.trim());
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT id FROM prompt WHERE text LIKE ?1 LIMIT ?2")?;
            let rows = stmt.query_map(params![like, k as i64], |r| r.get::<_, i64>(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }

    pub fn asset_must_exist(&self, id: i64) -> ApiResult<AssetRow> {
        self.get_asset(id)?.ok_or_else(|| ApiError::not_found(format!("资产 {id} 不存在")))
    }
}

// ---------------------------------------------------------------- 标签管理（需求 8）
//
// 只追加，不动上面既有函数：`src/db/assets.rs` 同时有导入/哈希相关改动在进行。

/// 标签页的一行：标签 + 被多少个资产引用。
#[derive(Debug, Clone)]
pub struct TagCountRow {
    pub id: i64,
    pub name: String,
    pub asset_count: i64,
}

/// 删除单个标签的结果：让 handler 分别回 200 / 404 / 409。
#[derive(Debug, Clone)]
pub enum TagDeleteOutcome {
    Deleted,
    NotFound,
    /// 仍有这么多资产引用着它
    InUse(i64),
}

impl Db {
    /// 标签列表（`GET /api/tags`）：引用数降序、同名升序分页。
    ///
    /// `q` 是名称子串过滤（LIKE，通配符已转义）；`None`/空串表示不过滤。
    /// 总数在分页**之前**统计，供前端显示 `total`。
    pub fn list_tags_with_counts(
        &self,
        limit: i64,
        offset: i64,
        q: Option<&str>,
    ) -> ApiResult<(Vec<TagCountRow>, i64)> {
        let pattern = q.map(str::trim).filter(|s| !s.is_empty()).map(|s| format!("%{}%", escape_like(s)));
        self.with_conn(|c| {
            let total: i64 = c.query_row(
                "SELECT COUNT(*) FROM tag t WHERE (?1 IS NULL OR t.name LIKE ?1 ESCAPE '\\')",
                params![pattern],
                |r| r.get(0),
            )?;
            let mut stmt = c.prepare(
                "SELECT t.id, t.name, COUNT(at.asset_id) AS asset_count
                   FROM tag t
                   LEFT JOIN asset_tag at ON at.tag_id = t.id
                  WHERE (?1 IS NULL OR t.name LIKE ?1 ESCAPE '\\')
                  GROUP BY t.id, t.name
                  ORDER BY asset_count DESC, t.name ASC
                  LIMIT ?2 OFFSET ?3",
            )?;
            let rows = stmt.query_map(params![pattern, limit, offset], |r| {
                Ok(TagCountRow { id: r.get(0)?, name: r.get(1)?, asset_count: r.get(2)? })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok((out, total))
        })
    }

    /// 删除一个标签，但只在它没有任何资产引用时真的删（`DELETE /api/tags/{id}`）。
    ///
    /// 「存在性 / 引用数 / 删除」放在同一个事务里，避免检查完再被引用上。
    pub fn delete_tag_if_unused(&self, id: i64) -> ApiResult<TagDeleteOutcome> {
        use rusqlite::OptionalExtension;
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let name: Option<String> =
                tx.query_row("SELECT name FROM tag WHERE id = ?1", [id], |r| r.get(0)).optional()?;
            if name.is_none() {
                return Ok(TagDeleteOutcome::NotFound);
            }
            let used: i64 =
                tx.query_row("SELECT COUNT(*) FROM asset_tag WHERE tag_id = ?1", [id], |r| r.get(0))?;
            if used > 0 {
                return Ok(TagDeleteOutcome::InUse(used));
            }
            tx.execute("DELETE FROM tag WHERE id = ?1", [id])?;
            tx.commit()?;
            Ok(TagDeleteOutcome::Deleted)
        })
    }

    /// 清理所有「没有任何资产引用」的标签，返回删掉的行数（`POST /api/tags/cleanup`）。
    pub fn delete_unused_tags(&self) -> ApiResult<usize> {
        self.with_conn(|c| {
            c.execute("DELETE FROM tag WHERE id NOT IN (SELECT tag_id FROM asset_tag)", [])
        })
    }
}

/// 转义 LIKE 的模式字符（配合 SQL 里的 `ESCAPE '\'`）。
fn escape_like(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    for ch in raw.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}
