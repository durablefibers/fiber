use crate::auth::{AuthAgent, AuthUser};
use crate::state::AppState;
use crate::ws::{agent_ws, run_events_ws};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use fiber_core::{
    AddMemberRequest, CreateAgentRequest, CreatePipelineRequest, CreateProjectRequest,
    CreateUserRequest, LoginRequest, ProjectRole, StartRunRequest, UpdateAgentRequest,
    UpdateMemberRequest, UpdatePipelineRequest, UpsertSecretRequest,
};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use chrono::{DateTime, Utc};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/projects", get(list_projects).post(create_project))
        .route("/api/projects/{id}", get(get_project))
        .route(
            "/api/projects/{id}/members",
            get(list_members).post(add_member),
        )
        .route(
            "/api/projects/{id}/members/{user_id}",
            put(update_member).delete(remove_member),
        )
        .route("/api/users", post(create_user))
        .route(
            "/api/projects/{id}/pipelines",
            get(list_pipelines).post(create_pipeline),
        )
        .route(
            "/api/projects/{id}/secrets",
            get(list_secrets).post(upsert_secret),
        )
        .route(
            "/api/projects/{id}/secrets/{key}",
            delete(delete_secret),
        )
        .route("/api/pipelines/parse-yaml", post(parse_yaml))
        .route(
            "/api/pipelines/{id}",
            get(get_pipeline).put(update_pipeline),
        )
        .route("/api/pipelines/{id}/runs", post(start_run))
        .route("/api/projects/{id}/runs", get(list_runs))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/runs/{id}/steps", get(list_steps))
        .route("/api/runs/{id}/artifacts", get(list_run_artifacts))
        .route("/api/artifacts/{id}/download", get(download_artifact))
        .route(
            "/api/agent/steps/{step_run_id}/artifacts",
            put(agent_upload_artifact).layer(DefaultBodyLimit::max(
                crate::artifact_util::MAX_ARTIFACT_BYTES as usize + 1024,
            )),
        )
        .route(
            "/api/agent/steps/{step_run_id}/artifacts/presign",
            post(agent_presign_artifact),
        )
        .route(
            "/api/agent/steps/{step_run_id}/artifacts/complete",
            post(agent_complete_artifact),
        )
        .route(
            "/api/agent/artifacts/{id}/download",
            get(agent_download_artifact),
        )
        .route("/api/steps/{id}/logs", get(list_logs))
        .route("/api/steps/{id}/attempts", get(list_attempts))
        .route(
            "/api/projects/{id}/fibers",
            get(list_fibers).post(create_fiber),
        )
        .route("/api/fibers/{id}", get(get_fiber))
        .route("/api/fibers/{id}/cancel", post(cancel_fiber))
        .route("/api/agents", get(list_agents).post(create_agent))
        .route(
            "/api/agents/{id}",
            put(update_agent).delete(delete_agent),
        )
        .route("/api/agents/{id}/rotate-token", post(rotate_agent_token))
        .route(
            "/api/projects/{id}/webhooks/github",
            post(github_webhook).put(set_github_secret),
        )
        .route("/ws/agent", get(agent_ws))
        .route("/ws/runs/{id}", get(run_events_ws))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "fiber-api" }))
}

async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let mut checks = json!({
        "postgres": "ok",
        "redis": "ok",
    });
    let mut ok = true;

    if let Err(e) = sqlx::query("SELECT 1").execute(&state.store.pool).await {
        ok = false;
        checks["postgres"] = json!(format!("error: {e}"));
    }
    if let Err(e) = state.scheduler.redis_ping().await {
        ok = false;
        checks["redis"] = json!(format!("error: {e}"));
    }

    let body = json!({ "ok": ok, "service": "fiber-api", "checks": checks });
    if ok {
        (StatusCode::OK, Json(body)).into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
    }
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let resp = state
        .store
        .login(&req.username, &req.password)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::Unauthorized)?;
    Ok(Json(resp))
}

async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(token) = crate::auth::bearer_from_headers(&headers) {
        let _ = state.store.logout(&token).await;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn me(AuthUser(user): AuthUser) -> impl IntoResponse {
    Json(user)
}

async fn list_projects(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    let projects = state
        .store
        .list_projects_for_user(user.id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(projects))
}

async fn create_project(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateProjectRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let project = state
        .store
        .create_project(user.id, req)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(project)))
}

async fn get_project(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let role = crate::access::require_project(&state, &user, id, ProjectRole::Reader).await?;
    let project = state
        .store
        .get_project(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(json!({
        "id": project.id,
        "name": project.name,
        "slug": project.slug,
        "created_at": project.created_at,
        "role": role.as_str(),
    })))
}

async fn list_members(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Reader).await?;
    let members = state
        .store
        .list_project_members(id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(members))
}

async fn add_member(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<AddMemberRequest>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Admin).await?;
    let role = ProjectRole::parse(&req.role)
        .ok_or_else(|| ApiError::BadRequest("invalid role".into()))?;
    if role == ProjectRole::Owner {
        crate::access::require_project(&state, &user, id, ProjectRole::Owner).await?;
    }
    let target = match state
        .store
        .find_user_by_username(&req.username)
        .await
        .map_err(ApiError::from)?
    {
        Some(u) => u,
        None => {
            let pw = req
                .password
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    ApiError::BadRequest("user not found; provide password to create".into())
                })?;
            state
                .store
                .create_user(&req.username, pw)
                .await
                .map_err(ApiError::from)?
        }
    };
    state
        .store
        .add_project_member(id, target.id, role)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true, "user_id": target.id, "role": role.as_str() })))
}

async fn update_member(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path((id, user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateMemberRequest>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Admin).await?;
    let role = ProjectRole::parse(&req.role)
        .ok_or_else(|| ApiError::BadRequest("invalid role".into()))?;
    if role == ProjectRole::Owner {
        crate::access::require_project(&state, &user, id, ProjectRole::Owner).await?;
    }
    state
        .store
        .add_project_member(id, user_id, role)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true })))
}

async fn remove_member(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path((id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Admin).await?;
    state
        .store
        .remove_project_member(id, user_id)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("cannot remove") {
                ApiError::BadRequest(msg)
            } else {
                ApiError::from(e)
            }
        })?;
    Ok(Json(json!({ "ok": true })))
}

async fn create_user(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateUserRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Any project owner may create users for invites.
    let projects = state
        .store
        .list_projects_for_user(user.id)
        .await
        .map_err(ApiError::from)?;
    let mut ok = false;
    for p in &projects {
        if let Ok(Some(r)) = state.store.member_role(p.id, user.id).await {
            if r.at_least(ProjectRole::Owner) {
                ok = true;
                break;
            }
        }
    }
    if !ok {
        return Err(ApiError::Forbidden);
    }
    let created = state
        .store
        .create_user(&req.username, &req.password)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn list_pipelines(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Reader).await?;
    let pipelines = state
        .store
        .list_pipelines(id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(pipelines))
}

async fn create_pipeline(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<CreatePipelineRequest>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Writer).await?;
    let pipeline = state
        .store
        .create_pipeline(id, req)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(pipeline)))
}

async fn get_pipeline(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_pipeline(&state, &user, id, ProjectRole::Reader).await?;
    let pipeline = state
        .store
        .get_pipeline(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(pipeline))
}

async fn update_pipeline(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdatePipelineRequest>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_pipeline(&state, &user, id, ProjectRole::Writer).await?;
    let pipeline = state
        .store
        .update_pipeline(id, req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(pipeline))
}

#[derive(Deserialize)]
struct ParseYamlBody {
    yaml: String,
}

async fn parse_yaml(
    _user: AuthUser,
    Json(body): Json<ParseYamlBody>,
) -> Result<impl IntoResponse, ApiError> {
    let def = fiber_core::dag::parse_pipeline_yaml(&body.yaml)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    fiber_core::compile_definition(&def).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok(Json(def))
}

async fn start_run(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<StartRunRequest>>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_pipeline(&state, &user, id, ProjectRole::Writer).await?;
    let trigger = body
        .and_then(|b| b.trigger.clone())
        .unwrap_or_else(|| "manual".into());
    let (run, steps, _) = state
        .store
        .start_run(id, &trigger)
        .await
        .map_err(ApiError::from)?;
    state
        .scheduler
        .enqueue_run_ready(run.id)
        .await
        .map_err(ApiError::from)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "run": run, "steps": steps })),
    ))
}

async fn list_runs(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Reader).await?;
    let runs = state
        .store
        .list_runs(id, 50)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(runs))
}

async fn get_run(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_run(&state, &user, id, ProjectRole::Reader).await?;
    let run = state
        .store
        .get_run(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    let steps = state
        .store
        .list_step_runs(id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "run": run, "steps": steps })))
}

async fn cancel_run(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_run(&state, &user, id, ProjectRole::Writer).await?;
    let run = state
        .scheduler
        .cancel_run(id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(run))
}

async fn list_steps(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_run(&state, &user, id, ProjectRole::Reader).await?;
    let steps = state
        .store
        .list_step_runs(id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(steps))
}

async fn list_run_artifacts(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_run(&state, &user, id, ProjectRole::Reader).await?;
    let artifacts = state
        .store
        .list_artifacts(id)
        .await
        .map_err(ApiError::from)?;
    let public: Vec<_> = artifacts
        .into_iter()
        .map(|a| {
            json!({
                "id": a.id,
                "run_id": a.run_id,
                "step_run_id": a.step_run_id,
                "name": a.name,
                "size": a.size,
                "created_at": a.created_at,
            })
        })
        .collect();
    Ok(Json(public))
}

async fn download_artifact(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::Redirect;
    crate::access::require_artifact(&state, &user, id, ProjectRole::Reader).await?;
    let artifact = state
        .store
        .get_artifact(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;

    if let Ok(Some(url)) = state.artifacts.presign_get(&artifact.path, 600).await {
        return Ok(Redirect::temporary(&url).into_response());
    }

    let bytes = state
        .artifacts
        .get_bytes(&artifact.path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let mut headers = HeaderMap::new();
    let filename = crate::artifact_util::artifact_basename(&artifact.name);
    let disposition = format!("attachment; filename=\"{}\"", filename.replace('"', ""));
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition)
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    Ok((headers, bytes).into_response())
}

/// Agent HTTP upload: `PUT` raw body with `X-Fiber-Artifact-Path: out/VERSION`.
async fn agent_upload_artifact(
    AuthAgent(agent): AuthAgent,
    State(state): State<AppState>,
    Path(step_run_id): Path<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let step = state
        .store
        .get_step_run(step_run_id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    if step.agent_id != Some(agent.id) {
        return Err(ApiError::Unauthorized);
    }
    if body.len() as u64 > crate::artifact_util::MAX_ARTIFACT_BYTES {
        return Err(ApiError::BadRequest(format!(
            "artifact exceeds {} bytes",
            crate::artifact_util::MAX_ARTIFACT_BYTES
        )));
    }
    let header_path = headers
        .get("x-fiber-artifact-path")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let rel = crate::artifact_util::sanitize_artifact_rel_path(header_path).ok_or_else(|| {
        ApiError::BadRequest("missing or invalid X-Fiber-Artifact-Path".into())
    })?;
    let key = crate::artifacts::ArtifactBackend::object_key(
        &step.run_id.to_string(),
        &step_run_id.to_string(),
        &rel.replace('/', "__"),
    );
    let stored = state
        .artifacts
        .put(&key, &body)
        .await
        .map_err(ApiError::from)?;
    let art = state
        .store
        .create_artifact(
            step.run_id,
            step_run_id,
            &rel,
            &stored,
            body.len() as i64,
        )
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "id": art.id,
        "name": art.name,
        "size": art.size,
    })))
}

#[derive(Deserialize)]
struct PresignArtifactBody {
    path: String,
    size: u64,
}

/// Ask for a direct S3 PUT URL when configured; otherwise `{ "mode": "proxy" }`.
async fn agent_presign_artifact(
    AuthAgent(agent): AuthAgent,
    State(state): State<AppState>,
    Path(step_run_id): Path<Uuid>,
    Json(body): Json<PresignArtifactBody>,
) -> Result<impl IntoResponse, ApiError> {
    let step = state
        .store
        .get_step_run(step_run_id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    if step.agent_id != Some(agent.id) {
        return Err(ApiError::Unauthorized);
    }
    if body.size > crate::artifact_util::MAX_ARTIFACT_BYTES {
        return Err(ApiError::BadRequest(format!(
            "artifact exceeds {} bytes",
            crate::artifact_util::MAX_ARTIFACT_BYTES
        )));
    }
    let rel = crate::artifact_util::sanitize_artifact_rel_path(&body.path).ok_or_else(|| {
        ApiError::BadRequest("missing or invalid path".into())
    })?;
    let key = crate::artifacts::ArtifactBackend::object_key(
        &step.run_id.to_string(),
        &step_run_id.to_string(),
        &rel.replace('/', "__"),
    );
    const EXPIRES: u64 = 600;
    let Some(upload_url) = state
        .artifacts
        .presign_put(&key, EXPIRES)
        .await
        .map_err(ApiError::from)?
    else {
        return Ok(Json(json!({ "mode": "proxy" })));
    };
    let stored_path = state.artifacts.stored_path_for_key(&key);
    Ok(Json(json!({
        "mode": "presign",
        "upload_url": upload_url,
        "stored_path": stored_path,
        "path": rel,
        "expires_in": EXPIRES,
    })))
}

#[derive(Deserialize)]
struct CompleteArtifactBody {
    path: String,
    size: u64,
    stored_path: String,
}

/// Register metadata after a successful direct (presigned) upload.
async fn agent_complete_artifact(
    AuthAgent(agent): AuthAgent,
    State(state): State<AppState>,
    Path(step_run_id): Path<Uuid>,
    Json(body): Json<CompleteArtifactBody>,
) -> Result<impl IntoResponse, ApiError> {
    let step = state
        .store
        .get_step_run(step_run_id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    if step.agent_id != Some(agent.id) {
        return Err(ApiError::Unauthorized);
    }
    if body.size > crate::artifact_util::MAX_ARTIFACT_BYTES {
        return Err(ApiError::BadRequest(format!(
            "artifact exceeds {} bytes",
            crate::artifact_util::MAX_ARTIFACT_BYTES
        )));
    }
    let rel = crate::artifact_util::sanitize_artifact_rel_path(&body.path).ok_or_else(|| {
        ApiError::BadRequest("missing or invalid path".into())
    })?;
    let key = crate::artifacts::ArtifactBackend::object_key(
        &step.run_id.to_string(),
        &step_run_id.to_string(),
        &rel.replace('/', "__"),
    );
    let expected = state.artifacts.stored_path_for_key(&key);
    if body.stored_path != expected {
        return Err(ApiError::BadRequest("stored_path mismatch".into()));
    }
    match state
        .artifacts
        .object_size(&body.stored_path)
        .await
        .map_err(ApiError::from)?
    {
        Some(n) if n != body.size => {
            return Err(ApiError::BadRequest(format!(
                "uploaded size {n} != claimed {}",
                body.size
            )));
        }
        None => {
            return Err(ApiError::BadRequest(
                "object not found after upload".into(),
            ));
        }
        Some(_) => {}
    }
    let art = state
        .store
        .create_artifact(
            step.run_id,
            step_run_id,
            &rel,
            &body.stored_path,
            body.size as i64,
        )
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "id": art.id,
        "name": art.name,
        "size": art.size,
    })))
}

async fn agent_download_artifact(
    AuthAgent(_agent): AuthAgent,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::Redirect;
    let artifact = state
        .store
        .get_artifact(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;

    if let Ok(Some(url)) = state.artifacts.presign_get(&artifact.path, 600).await {
        return Ok(Redirect::temporary(&url).into_response());
    }

    let bytes = state
        .artifacts
        .get_bytes(&artifact.path)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    Ok((headers, bytes).into_response())
}

async fn list_logs(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_step(&state, &user, id, ProjectRole::Reader).await?;
    let logs = state.store.list_logs(id).await.map_err(ApiError::from)?;
    Ok(Json(logs))
}

async fn list_attempts(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_step(&state, &user, id, ProjectRole::Reader).await?;
    let attempts = state
        .store
        .list_step_attempts(id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(attempts))
}

#[derive(Debug, Deserialize)]
struct CreateFiberRequest {
    name: String,
    #[serde(default)]
    input: Value,
    /// Optional wake time (RFC3339). Future → starts suspended.
    wake_at: Option<DateTime<Utc>>,
}

async fn list_fibers(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Reader).await?;
    let fibers = state
        .fiber_scheduler
        .store()
        .list_by_project(id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(fibers))
}

async fn create_fiber(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<CreateFiberRequest>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Writer).await?;
    if state.store.get_project(id).await.map_err(ApiError::from)?.is_none() {
        return Err(ApiError::NotFound);
    }
    if !state.fiber_scheduler.registry().contains(&req.name) {
        return Err(ApiError::BadRequest(format!(
            "unknown durable task '{}'",
            req.name
        )));
    }
    let fiber = state
        .fiber_scheduler
        .store()
        .create(id, &req.name, req.input, req.wake_at)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(fiber)))
}

async fn get_fiber(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_fiber(&state, &user, id, ProjectRole::Reader).await?;
    let fiber = state
        .fiber_scheduler
        .store()
        .get(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound)?;
    Ok(Json(fiber))
}

async fn cancel_fiber(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_fiber(&state, &user, id, ProjectRole::Writer).await?;
    let fiber = state
        .fiber_scheduler
        .store()
        .cancel(id)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound)?;
    Ok(Json(fiber))
}

async fn list_agents(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Query(q): Query<ListAgentsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(pid) = q.project_id {
        crate::access::require_project(&state, &user, pid, ProjectRole::Reader).await?;
    }
    let mut agents = state
        .store
        .list_agents(q.project_id)
        .await
        .map_err(ApiError::from)?;
    for a in &mut agents {
        a.token_hash = "***".into();
    }
    Ok(Json(agents))
}

#[derive(Debug, Deserialize)]
struct ListAgentsQuery {
    project_id: Option<Uuid>,
}

async fn require_agent_manage(
    state: &AppState,
    user: &fiber_core::PublicUser,
    agent: &fiber_core::Agent,
) -> Result<(), ApiError> {
    if let Some(pid) = agent.project_id {
        crate::access::require_project(state, user, pid, ProjectRole::Admin).await?;
    }
    Ok(())
}

async fn create_agent(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateAgentRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(pid) = req.project_id {
        crate::access::require_project(&state, &user, pid, ProjectRole::Admin).await?;
    }
    let mut resp = state.store.create_agent(req).await.map_err(|e| {
        if e.to_string().contains("project not found") {
            ApiError::NotFound
        } else {
            ApiError::from(e)
        }
    })?;
    resp.agent.token_hash = "***".into();
    Ok((StatusCode::CREATED, Json(resp)))
}

async fn update_agent(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateAgentRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = state
        .store
        .get_agent(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    require_agent_manage(&state, &user, &existing).await?;
    let mut agent = state
        .store
        .update_agent(id, req)
        .await
        .map_err(|e| {
            if e.to_string().contains("not found") {
                ApiError::NotFound
            } else {
                ApiError::from(e)
            }
        })?;
    let labels = agent
        .labels
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    state
        .scheduler
        .update_agent_presence(id, labels, agent.concurrency.max(1) as u32)
        .await;
    agent.token_hash = "***".into();
    Ok(Json(agent))
}

async fn delete_agent(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = state
        .store
        .get_agent(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    require_agent_manage(&state, &user, &existing).await?;
    if let Err(e) = state.scheduler.on_agent_disconnect(id).await {
        tracing::warn!(error = %e, %id, "agent delete disconnect cleanup");
    }
    let ok = state
        .store
        .delete_agent(id)
        .await
        .map_err(ApiError::from)?;
    if !ok {
        return Err(ApiError::NotFound);
    }
    Ok(Json(json!({ "ok": true })))
}

async fn rotate_agent_token(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    let existing = state
        .store
        .get_agent(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;
    require_agent_manage(&state, &user, &existing).await?;
    // Drop live connection so the old token cannot keep leasing / heartbeating.
    state
        .scheduler
        .force_disconnect_agent(id, "token rotated — reconnect with the new token")
        .await;
    let mut resp = state
        .store
        .rotate_agent_token(id)
        .await
        .map_err(|e| {
            if e.to_string().contains("not found") {
                ApiError::NotFound
            } else {
                ApiError::from(e)
            }
        })?;
    resp.agent.token_hash = "***".into();
    Ok(Json(resp))
}

async fn list_secrets(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Admin).await?;
    let secrets = state
        .store
        .list_secret_keys(id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(secrets))
}

async fn upsert_secret(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpsertSecretRequest>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Admin).await?;
    if req.key.trim().is_empty() || req.key.contains('=') {
        return Err(ApiError::BadRequest("invalid secret key".into()));
    }
    let secret = state
        .store
        .upsert_secret(id, &req.key, &req.value)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(secret))
}

async fn delete_secret(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path((id, key)): Path<(Uuid, String)>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Admin).await?;
    state
        .store
        .delete_secret(id, &key)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SetSecret {
    secret: String,
}

async fn set_github_secret(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<SetSecret>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Admin).await?;
    state
        .store
        .upsert_webhook_secret(id, "github", &body.secret)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true })))
}

async fn github_webhook(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    body: String,
) -> Result<impl IntoResponse, ApiError> {
    if let Some(secret) = state
        .store
        .get_webhook_secret(id, "github")
        .await
        .map_err(ApiError::from)?
    {
        let sig = headers
            .get("x-hub-signature-256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !verify_github_sig(&secret, &body, sig) {
            return Err(ApiError::Unauthorized);
        }
    }

    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let payload: Value =
        serde_json::from_str(&body).map_err(|e| ApiError::BadRequest(e.to_string()))?;

    match event {
        "push" => {
            let branch = payload
                .get("ref")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .trim_start_matches("refs/heads/")
                .to_string();
            let changed = collect_push_changed_files(&payload);
            let pipelines = state
                .store
                .find_pipelines_for_push(id, &branch, &changed)
                .await
                .map_err(ApiError::from)?;
            let mut run_ids = Vec::new();
            for p in pipelines {
                let (run, _, _) = state
                    .store
                    .start_run(p.id, &format!("github:push:{branch}"))
                    .await
                    .map_err(ApiError::from)?;
                state
                    .scheduler
                    .enqueue_run_ready(run.id)
                    .await
                    .map_err(ApiError::from)?;
                run_ids.push(run.id);
            }
            Ok(Json(json!({ "started": run_ids, "changed_files": changed.len() })))
        }
        "pull_request" => {
            let action = payload
                .get("action")
                .and_then(|a| a.as_str())
                .unwrap_or("");
            let base = payload
                .pointer("/pull_request/base/ref")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let number = payload
                .pointer("/pull_request/number")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let mut changed = collect_explicit_changed_files(&payload);
            let mut files_source = if changed.is_empty() {
                "none"
            } else {
                "payload"
            };
            // PR webhooks omit file lists; fetch via API when path filters may apply.
            if changed.is_empty()
                && project_has_pr_path_filters(&state, id).await.unwrap_or(true)
            {
                match crate::github::resolve_token(&state.store, id).await {
                    Ok(Some(token)) => {
                        if let Some((owner, repo)) = crate::github::repo_full_name(&payload) {
                            match crate::github::list_pull_request_files(
                                &owner, &repo, number, &token,
                            )
                            .await
                            {
                                Ok(files) => {
                                    changed = files;
                                    files_source = "api";
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "failed to list PR files; path-filtered pipelines will skip"
                                    );
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::debug!(
                            "no GITHUB_TOKEN; PR path filters need API file list or payload.changed_files"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "resolve github token failed");
                    }
                }
            }
            let pipelines = state
                .store
                .find_pipelines_for_pull_request(id, &base, action, &changed)
                .await
                .map_err(ApiError::from)?;
            let mut run_ids = Vec::new();
            for p in pipelines {
                let (run, _, _) = state
                    .store
                    .start_run(p.id, &format!("github:pr:{number}:{action}"))
                    .await
                    .map_err(ApiError::from)?;
                state
                    .scheduler
                    .enqueue_run_ready(run.id)
                    .await
                    .map_err(ApiError::from)?;
                run_ids.push(run.id);
            }
            Ok(Json(json!({
                "started": run_ids,
                "action": action,
                "base": base,
                "changed_files": changed.len(),
                "files_source": files_source,
            })))
        }
        _ => Ok(Json(json!({ "ignored": true, "event": event }))),
    }
}

fn collect_push_changed_files(payload: &Value) -> Vec<String> {
    let mut files = std::collections::BTreeSet::new();
    if let Some(commits) = payload.get("commits").and_then(|c| c.as_array()) {
        for c in commits {
            for key in ["added", "modified", "removed"] {
                if let Some(arr) = c.get(key).and_then(|a| a.as_array()) {
                    for f in arr {
                        if let Some(s) = f.as_str() {
                            files.insert(s.to_string());
                        }
                    }
                }
            }
        }
    }
    if let Some(head) = payload.get("head_commit") {
        for key in ["added", "modified", "removed"] {
            if let Some(arr) = head.get(key).and_then(|a| a.as_array()) {
                for f in arr {
                    if let Some(s) = f.as_str() {
                        files.insert(s.to_string());
                    }
                }
            }
        }
    }
    files.into_iter().collect()
}

fn collect_explicit_changed_files(payload: &Value) -> Vec<String> {
    let mut files = Vec::new();
    if let Some(arr) = payload.get("changed_files").and_then(|a| a.as_array()) {
        for f in arr {
            if let Some(s) = f.as_str() {
                files.push(s.to_string());
            }
        }
    }
    files
}

async fn project_has_pr_path_filters(
    state: &AppState,
    project_id: Uuid,
) -> Result<bool, ApiError> {
    let pipelines = state
        .store
        .list_pipelines(project_id)
        .await
        .map_err(ApiError::from)?;
    for p in pipelines {
        if let Ok(def) = fiber_core::store::value_to_definition(&p.definition) {
            if let Some(pr) = def.on.as_ref().and_then(|o| o.pull_request.as_ref()) {
                if !pr.paths.is_empty() || !pr.paths_ignore.is_empty() {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn verify_github_sig(secret: &str, body: &str, signature: &str) -> bool {
    let Some(hex_sig) = signature.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body.as_bytes());
    let result = mac.finalize().into_bytes();
    let expected = hex::encode(result);
    constant_time_eq(expected.as_bytes(), hex_sig.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Debug)]
pub enum ApiError {
    NotFound,
    Unauthorized,
    Forbidden,
    BadRequest(String),
    Internal(String),
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let msg = e.to_string();
        if msg.contains("forbidden") {
            ApiError::Forbidden
        } else {
            ApiError::Internal(msg)
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".into()),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden".into()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}
