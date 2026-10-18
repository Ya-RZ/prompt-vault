//! 批量作业的落库读写（INTERFACES.md §6）。
//!
//! 作业状态**必须落库**（开发文档 §7.2）：50 万张回填要跑好几天（§11），
//! 进程重启后必须能从 `cursor` 续跑。

use rusqlite::{params, params_from_iter, types::Value};

use super::{now_rfc3339, Db, JobErrorRow, JobRow};
use crate::error::ApiResult;
use crate::types::{JobListQuery, JobsCounts};

const JOB_COLS: &str = "id, kind, status, root, total, done, failed, skipped, cursor, fingerprint, \
                        params, message, created_at, updated_at, finished_at";

impl Db {
    pub fn create_job(
        &self,
        id: &str,
        kind: &str,
        root: Option<&str>,
        params_json: Option<&str>,
        fingerprint: Option<&str>,
    ) -> ApiResult<JobRow> {
        let now = now_rfc3339();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO import_job
                    (id, kind, status, root, total, done, failed, skipped, cursor, fingerprint, params, message, created_at, updated_at, finished_at)
                 VALUES (?1, ?2, 'pending', ?3, 0, 0, 0, 0, NULL, ?4, ?5, NULL, ?6, ?6, NULL)",
                params![id, kind, root, fingerprint, params_json, now],
            )
        })?;
        self.get_job(id)?.ok_or_else(|| crate::error::ApiError::internal("作业创建后读取失败"))
    }

    pub fn get_job(&self, id: &str) -> ApiResult<Option<JobRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!("SELECT {JOB_COLS} FROM import_job WHERE id = ?1"))?;
            let mut rows = stmt.query_map([id], JobRow::from_row)?;
            match rows.next() {
                Some(r) => Ok(Some(r?)),
                None => Ok(None),
            }
        })
    }

    pub fn list_jobs(&self, q: &JobListQuery) -> ApiResult<(Vec<JobRow>, i64)> {
        let page = q.page.unwrap_or(1).max(1);
        let page_size = q.page_size.unwrap_or(50).clamp(1, 200);

        let mut where_parts: Vec<String> = Vec::new();
        let mut vals: Vec<Value> = Vec::new();
        if let Some(status) = q.status.as_deref().filter(|s| !s.is_empty()) {
            where_parts.push("status = ?".to_string());
            vals.push(Value::Text(status.to_string()));
        }
        if let Some(kind) = q.kind.as_deref().filter(|s| !s.is_empty()) {
            where_parts.push("kind = ?".to_string());
            vals.push(Value::Text(kind.to_string()));
        }
        let where_sql =
            if where_parts.is_empty() { String::new() } else { format!("WHERE {}", where_parts.join(" AND ")) };

        self.with_conn(|c| {
            let total: i64 = c.query_row(
                &format!("SELECT COUNT(*) FROM import_job {where_sql}"),
                params_from_iter(vals.iter()),
                |r| r.get(0),
            )?;
            let sql = format!(
                "SELECT {JOB_COLS} FROM import_job {where_sql} ORDER BY created_at DESC, rowid DESC LIMIT ? OFFSET ?"
            );
            let mut all_vals = vals.clone();
            all_vals.push(Value::Integer(page_size));
            all_vals.push(Value::Integer((page - 1) * page_size));
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(all_vals.iter()), JobRow::from_row)?;
            let mut items = Vec::new();
            for r in rows {
                items.push(r?);
            }
            Ok((items, total))
        })
    }

    pub fn set_job_total(&self, id: &str, total: i64) -> ApiResult<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE import_job SET total = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, total, now_rfc3339()],
            )
            .map(|_| ())
        })
    }

    pub fn set_job_status(&self, id: &str, status: &str, message: Option<&str>) -> ApiResult<()> {
        let now = now_rfc3339();
        let finished = matches!(status, "succeeded" | "failed" | "cancelled").then_some(now.as_str());
        self.with_conn(|c| {
            c.execute(
                "UPDATE import_job SET status = ?2, message = COALESCE(?3, message),
                        updated_at = ?4,
                        finished_at = COALESCE(?5, finished_at)
                 WHERE id = ?1",
                params![id, status, message, now, finished],
            )
            .map(|_| ())
        })
    }

    pub fn update_job_progress(
        &self,
        id: &str,
        done: i64,
        failed: i64,
        skipped: i64,
        cursor: Option<&str>,
    ) -> ApiResult<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE import_job SET done = ?2, failed = ?3, skipped = ?4,
                        cursor = COALESCE(?5, cursor), updated_at = ?6
                 WHERE id = ?1",
                params![id, done, failed, skipped, cursor, now_rfc3339()],
            )
            .map(|_| ())
        })
    }

    pub fn set_job_cursor(&self, id: &str, cursor: &str) -> ApiResult<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE import_job SET cursor = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, cursor, now_rfc3339()],
            )
            .map(|_| ())
        })
    }

    pub fn add_job_error(
        &self,
        job_id: &str,
        rel_path: Option<&str>,
        code: &str,
        message: &str,
    ) -> ApiResult<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO import_job_error (job_id, rel_path, code, message, at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![job_id, rel_path, code, message, now_rfc3339()],
            )
            .map(|_| ())
        })
    }

    pub fn job_errors(&self, job_id: &str, limit: i64, offset: i64) -> ApiResult<(Vec<JobErrorRow>, i64)> {
        self.with_conn(|c| {
            let total: i64 =
                c.query_row("SELECT COUNT(*) FROM import_job_error WHERE job_id = ?1", [job_id], |r| r.get(0))?;
            let mut stmt = c.prepare(
                "SELECT rel_path, code, message, at FROM import_job_error
                 WHERE job_id = ?1 ORDER BY id ASC LIMIT ?2 OFFSET ?3",
            )?;
            let rows = stmt.query_map(params![job_id, limit, offset], |r| {
                Ok(JobErrorRow {
                    rel_path: r.get(0)?,
                    code: r.get(1)?,
                    message: r.get(2)?,
                    at: r.get(3)?,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok((out, total))
        })
    }

    pub fn job_counts_by_status(&self) -> ApiResult<JobsCounts> {
        self.with_conn(|c| {
            let get = |status: &str| -> rusqlite::Result<i64> {
                c.query_row("SELECT COUNT(*) FROM import_job WHERE status = ?1", [status], |r| r.get(0))
            };
            Ok(JobsCounts {
                running: get("running")?,
                succeeded: get("succeeded")?,
                failed: get("failed")?,
                cancelled: get("cancelled")?,
                paused: get("paused")?,
            })
        })
    }

    /// 删除作业及其错误明细（需求 2：只删已结束的作业）。
    ///
    /// **状态校验不在这里做**：`DELETE /api/jobs/{id}` 需要按状态回 409，
    /// 那个判断放在 handler 里，这里只负责落库。
    /// `import_job_error.job_id` 带 `ON DELETE CASCADE`，但外键开关有可能被关掉，
    /// 所以显式删一遍明细，避免留下孤儿错误行。
    pub fn delete_job(&self, id: &str) -> ApiResult<bool> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            tx.execute("DELETE FROM import_job_error WHERE job_id = ?1", [id])?;
            let removed = tx.execute("DELETE FROM import_job WHERE id = ?1", [id])?;
            tx.commit()?;
            Ok(removed > 0)
        })
    }

    /// 找出还在 running 的作业 id（用于取消/并发保护）。
    pub fn running_job_ids(&self) -> ApiResult<Vec<String>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT id FROM import_job WHERE status IN ('running','pending')")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
    }
}
