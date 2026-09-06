//! Project role checks for API handlers.

use crate::state::AppState;
use fiber_core::{ProjectRole, PublicUser};
use uuid::Uuid;

use crate::routes::ApiError;

pub async fn require_project(
    state: &AppState,
    user: &PublicUser,
    project_id: Uuid,
    min: ProjectRole,
) -> Result<ProjectRole, ApiError> {
    state
        .store
        .require_role(project_id, user.id, min)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("forbidden") {
                ApiError::Forbidden
            } else {
                ApiError::from(e)
            }
        })
}

/// Instance-admin gate. Instance admins manage the global agent pool, every agent,
/// and users. Project routes (pipelines, runs, secrets, members) still require
/// membership — but because an agent token receives a project's secrets, an
/// instance admin is a secrets superuser in practice. Treat the flag as root.
pub fn require_instance_admin(user: &PublicUser) -> Result<(), ApiError> {
    if user.is_admin {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

pub async fn require_pipeline(
    state: &AppState,
    user: &PublicUser,
    pipeline_id: Uuid,
    min: ProjectRole,
) -> Result<(Uuid, ProjectRole), ApiError> {
    let project_id = state
        .store
        .project_id_for_pipeline(pipeline_id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    let role = require_project(state, user, project_id, min).await?;
    Ok((project_id, role))
}

pub async fn require_run(
    state: &AppState,
    user: &PublicUser,
    run_id: Uuid,
    min: ProjectRole,
) -> Result<(Uuid, ProjectRole), ApiError> {
    let project_id = state
        .store
        .project_id_for_run(run_id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    let role = require_project(state, user, project_id, min).await?;
    Ok((project_id, role))
}

pub async fn require_step(
    state: &AppState,
    user: &PublicUser,
    step_run_id: Uuid,
    min: ProjectRole,
) -> Result<(Uuid, ProjectRole), ApiError> {
    let project_id = state
        .store
        .project_id_for_step(step_run_id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    let role = require_project(state, user, project_id, min).await?;
    Ok((project_id, role))
}

pub async fn require_artifact(
    state: &AppState,
    user: &PublicUser,
    artifact_id: Uuid,
    min: ProjectRole,
) -> Result<(Uuid, ProjectRole), ApiError> {
    let project_id = state
        .store
        .project_id_for_artifact(artifact_id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    let role = require_project(state, user, project_id, min).await?;
    Ok((project_id, role))
}

pub async fn require_fiber(
    state: &AppState,
    user: &PublicUser,
    fiber_id: Uuid,
    min: ProjectRole,
) -> Result<(Uuid, ProjectRole), ApiError> {
    let project_id = state
        .store
        .project_id_for_fiber(fiber_id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    let role = require_project(state, user, project_id, min).await?;
    Ok((project_id, role))
}
