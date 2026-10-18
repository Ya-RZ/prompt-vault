//! 作业相关 handler（INTERFACES.md §6）：进度、暂停/续跑/取消、导入与编码。

use std::sync::Arc;

use salvo::prelude::*;
use uuid::Uuid;

use super::{st, ApiResult};
use crate::db::JobRow;
use crate::error::ApiError;
use crate::jobs::{job_to_dto, EncodeParams, ImportParams};
use crate::state::AppState;
use crate::types::{EncodeJobRequest, ImportJobRequest, JobErrorDto, JobListQuery};

#[handler]
pub async fn list_jobs(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let page = req.query::<i64>("page").unwrap_or(1).max(1);
    let page_size = req.query::<i64>("page_size").unwrap_or(50).clamp(1, 200);
    let query = JobListQuery {
        page: Some(page),
        page_size: Some(page_size),
        status: req.query::<String>("status"),
        kind: req.query::<String>("kind"),
    };
    let (rows, total) = state.db.list_jobs(&query)?;
    let items: Vec<_> = rows.iter().map(|j| job_to_dto(j, Vec::new())).collect();
    res.render(Json(crate::types::Paginated::new(items, total, page, page_size)));
    Ok(())
}

async fn job_detail(state: &Arc<AppState>, id: &str) -> ApiResult<serde_json::Value> {
    let job = state
        .db
        .get_job(id)?
        .ok_or_else(|| ApiError::not_found(format!("作业 {id} 不存在")))?;
    let (errors, _) = state.db.job_errors(id, 50, 0)?;
    let error_dtos: Vec<JobErrorDto> = errors
        .into_iter()
        .map(|e| JobErrorDto { rel_path: e.rel_path, code: e.code, message: e.message, at: e.at })
        .collect();
    let mut dto = job_to_dto(&job, error_dtos);
    if state.jobs.is_active(id) && dto.status != "running" {
        dto.status = "running".into();
    }
    Ok(serde_json::to_value(dto).unwrap_or(serde_json::Value::Null))
}

#[handler]
pub async fn get_job(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let id = req.param::<String>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    res.render(Json(job_detail(&state, &id).await?));
    Ok(())
}

#[handler]
pub async fn job_errors(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let id = req.param::<String>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    let page = req.query::<i64>("page").unwrap_or(1).max(1);
    let page_size = req.query::<i64>("page_size").unwrap_or(50).clamp(1, 200);
    let (rows, total) = state.db.job_errors(&id, page_size, (page - 1) * page_size)?;
    let items: Vec<JobErrorDto> = rows
        .into_iter()
        .map(|e| JobErrorDto { rel_path: e.rel_path, code: e.code, message: e.message, at: e.at })
        .collect();
    res.render(Json(crate::types::Paginated::new(items, total, page, page_size)));
    Ok(())
}

#[handler]
pub async fn create_import(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let body: ImportJobRequest = req
        .parse_json()
        .await
        .map_err(|e| ApiError::bad_request(format!("请求体解析失败: {e}")))?;

    let defaults = ImportParams::default();
    let params = ImportParams {
        root: body.root.unwrap_or(defaults.root),
        recursive: body.recursive.unwrap_or(defaults.recursive),
        extensions: body.extensions.unwrap_or(defaults.extensions),
        default_prompt_role: body.default_prompt_role.unwrap_or(defaults.default_prompt_role),
        dedupe_by: body.dedupe_by.unwrap_or(defaults.dedupe_by),
    };
    // 导入前金标闸门（§9.4）
    state.check_golden_gate()?;

    let id = Uuid::new_v4().to_string();
    let params_json = serde_json::to_string(&params).ok();
    state.db.create_job(
        &id,
        "import",
        Some(if params.root.is_empty() { "" } else { params.root.as_str() }),
        params_json.as_deref(),
        state.index_fingerprint()?.as_deref(),
    )?;
    state.jobs.start_import(Arc::clone(&state), id.clone(), params);
    res.status_code(salvo::http::StatusCode::ACCEPTED);
    res.render(Json(job_detail(&state, &id).await?));
    Ok(())
}

#[handler]
pub async fn create_encode(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let body: EncodeJobRequest = req.parse_json().await.unwrap_or_default();
    let defaults = EncodeParams::default();
    let params = EncodeParams {
        limit: body.limit,
        only_missing: body.only_missing.unwrap_or(defaults.only_missing),
        batch_size: body.batch_size.unwrap_or(defaults.batch_size),
        asset_ids: body.asset_ids,
        prompt_ids: body.prompt_ids,
    };
    state.check_golden_gate()?;

    let id = Uuid::new_v4().to_string();
    let params_json = serde_json::to_string(&params).ok();
    state.db.create_job(
        &id,
        "encode",
        None,
        params_json.as_deref(),
        state.index_fingerprint()?.as_deref(),
    )?;
    state
        .jobs
        .start_encode(Arc::clone(&state), id.clone(), params);
    res.status_code(salvo::http::StatusCode::ACCEPTED);
    res.render(Json(job_detail(&state, &id).await?));
    Ok(())
}

async fn job_action(
    state: &Arc<AppState>,
    id: &str,
    action: &str,
) -> ApiResult<serde_json::Value> {
    let job: JobRow = state
        .db
        .get_job(id)?
        .ok_or_else(|| ApiError::not_found(format!("作业 {id} 不存在")))?;

    match action {
        "pause" => {
            let signalled = state.jobs.request_pause(id);
            if signalled {
                state.db.set_job_status(id, "paused", Some("已请求暂停，当前批次结束后生效"))?;
            } else if job.status != "paused" {
                return Err(ApiError::conflict("作业不在运行中，无法暂停"));
            }
        }
        "cancel" => {
            let signalled = state.jobs.request_cancel(id);
            if signalled {
                state.db.set_job_status(id, "cancelled", Some("已请求取消"))?;
            } else if !matches!(job.status.as_str(), "cancelled" | "succeeded" | "failed") {
                state.db.set_job_status(id, "cancelled", Some("作业未在运行，已直接标记取消"))?;
            }
        }
        "resume" => {
            if state.jobs.is_active(id) {
                return Err(ApiError::conflict("作业已在运行中"));
            }
            if job.status == "succeeded" {
                return Err(ApiError::conflict("作业已完成，无需续跑"));
            }
            match job.kind.as_str() {
                "import" => {
                    let params: ImportParams = job
                        .params
                        .as_deref()
                        .and_then(|p| serde_json::from_str(p).ok())
                        .unwrap_or_default();
                    state.db.set_job_status(id, "running", Some("已续跑（从 cursor 之后继续）"))?;
                    state.jobs.start_import(Arc::clone(state), id.to_string(), params);
                }
                "encode" => {
                    let params: EncodeParams = job
                        .params
                        .as_deref()
                        .and_then(|p| serde_json::from_str(p).ok())
                        .unwrap_or_default();
                    state.db.set_job_status(id, "running", Some("已续跑（只补未编码/指纹不符的）"))?;
                    state.jobs.start_encode(Arc::clone(state), id.to_string(), params);
                }
                other => return Err(ApiError::bad_request(format!("未知作业类型 {other}"))),
            }
        }
        _ => return Err(ApiError::bad_request("未知操作")),
    }
    job_detail(state, id).await
}

#[handler]
pub async fn pause_job(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let id = req.param::<String>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    res.render(Json(job_action(&state, &id, "pause").await?));
    Ok(())
}

#[handler]
pub async fn resume_job(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let id = req.param::<String>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    res.render(Json(job_action(&state, &id, "resume").await?));
    Ok(())
}

#[handler]
pub async fn cancel_job(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let id = req.param::<String>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    res.render(Json(job_action(&state, &id, "cancel").await?));
    Ok(())
}

/// 删除作业（需求 2）：只允许删**已结束**的作业（failed / succeeded / cancelled）。
///
/// pending / running / paused 一律 409，提示先取消——运行中的作业还在往库里写进度，
/// 直接删行会让它后面的写入落空，所以在入口就拒绝。
#[handler]
pub async fn delete_job(req: &mut Request, depot: &mut Depot, res: &mut Response) -> ApiResult<()> {
    let state = st(depot)?.clone();
    let id = req.param::<String>("id").ok_or_else(|| ApiError::bad_request("id 非法"))?;
    let job = state
        .db
        .get_job(&id)?
        .ok_or_else(|| ApiError::not_found(format!("作业 {id} 不存在")))?;
    if !matches!(job.status.as_str(), "failed" | "succeeded" | "cancelled") {
        return Err(ApiError::conflict(format!(
            "作业状态为 {}，只有 failed / succeeded / cancelled 的作业可删除；请先取消该作业",
            job.status
        )));
    }
    let deleted = state.db.delete_job(&id)?;
    res.render(Json(serde_json::json!({ "deleted": deleted })));
    Ok(())
}
