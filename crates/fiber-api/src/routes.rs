use crate::auth::{AuthAgent, AuthUser};
use crate::state::AppState;
use crate::ws::{agent_ws, run_events_ws};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use fiber_core::{
    AddMemberRequest, CreateAgentRequest, CreatePipelineRequest, CreateProjectRequest,
    CreateUserRequest, LoginRequest, ProjectRole, StartRunRequest, UpdateAgentRequest,
    UpdateMemberRequest, UpdatePipelineRequest, UpsertSecretRequest,
};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use std::time::Duration;
use tower_http::timeout::TimeoutLayer;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// An ordinary request that has not answered in this long is answered `408` and its
/// handler dropped, so a slow client or a stuck query cannot hold a hyper task
/// indefinitely. Streams and artifact transfers are routed around it below.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// GitHub delivers webhook payloads up to 25 MiB; axum's 2 MiB default turned a large
/// push into a `413` and no run.
const WEBHOOK_MAX_BYTES: usize = 25 << 20;
/// A readiness probe that hangs is worse than one that fails: the orchestrator's own
/// probe timeout kills the pod with "probe timeout" and no diagnosis. Each dependency
/// gets this long to answer before it is reported down.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub fn router(state: AppState) -> Router {
    // Two sub-routers, one layer. `Router::layer` wraps the routes present when it is
    // called, so the request timeout goes on the JSON surface and the long-lived
    // routes are merged in afterwards, outside it: a WebSocket upgrade lives for the
    // session and an artifact moves up to MAX_ARTIFACT_BYTES at whatever speed the
    // link allows.
    let api = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/auth/password", post(change_password))
        .route("/api/auth/sessions", axum::routing::delete(revoke_sessions))
        .route("/api/projects", get(list_projects).post(create_project))
        .route(
            "/api/projects/{id}",
            get(get_project).delete(delete_project),
        )
        .route(
            "/api/projects/{id}/members",
            get(list_members).post(add_member),
        )
        .route(
            "/api/projects/{id}/members/{user_id}",
            put(update_member).delete(remove_member),
        )
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{id}", put(update_user))
        .route(
            "/api/projects/{id}/pipelines",
            get(list_pipelines).post(create_pipeline),
        )
        .route(
            "/api/projects/{id}/secrets",
            get(list_secrets).post(upsert_secret),
        )
        .route("/api/projects/{id}/secrets/{key}", delete(delete_secret))
        .route("/api/pipelines/parse-yaml", post(parse_yaml))
        .route(
            "/api/pipelines/{id}",
            get(get_pipeline).put(update_pipeline),
        )
        .route("/api/pipelines/{id}/runs", post(start_run))
        .route("/api/projects/{id}/runs", get(list_runs))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/cancel", post(cancel_run))
        .route("/api/runs/{id}/retry", post(retry_run))
        .route("/api/runs/{id}/steps", get(list_steps))
        .route("/api/runs/{id}/artifacts", get(list_run_artifacts))
        .route(
            "/api/agent/steps/{step_run_id}/artifacts/presign",
            post(agent_presign_artifact),
        )
        .route(
            "/api/agent/steps/{step_run_id}/artifacts/complete",
            post(agent_complete_artifact),
        )
        .route("/api/steps/{id}/logs", get(list_logs))
        .route("/api/steps/{id}/attempts", get(list_attempts))
        .route(
            "/api/projects/{id}/fibers",
            get(list_fibers).post(create_fiber),
        )
        .route("/api/fibers/tasks", get(list_fiber_tasks))
        .route("/api/fibers/{id}", get(get_fiber))
        .route("/api/fibers/{id}/cancel", post(cancel_fiber))
        .route("/api/agents", get(list_agents).post(create_agent))
        .route("/api/agents/{id}", put(update_agent).delete(delete_agent))
        .route("/api/agents/{id}/rotate-token", post(rotate_agent_token))
        .route(
            "/api/projects/{id}/webhooks/github",
            // The limit wraps only what is on the method router when `.layer` runs,
            // so the delivery gets 25 MiB and the secret-setting PUT keeps the default.
            post(github_webhook)
                .layer(DefaultBodyLimit::max(WEBHOOK_MAX_BYTES))
                .put(set_github_secret),
        )
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ));
    let streaming = Router::new()
        .route("/api/artifacts/{id}/download", get(download_artifact))
        .route(
            "/api/agent/steps/{step_run_id}/artifacts",
            put(agent_upload_artifact).layer(DefaultBodyLimit::max(
                crate::artifact_util::MAX_ARTIFACT_BYTES as usize + 1024,
            )),
        )
        .route(
            "/api/agent/artifacts/{id}/download",
            get(agent_download_artifact),
        )
        .route("/ws/agent", get(agent_ws))
        .route("/ws/runs/{id}", get(run_events_ws));
    api.merge(streaming).with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "fiber-api" }))
}

/// Prometheus exposition of the queue, agents, runs, and fibers.
///
/// Off unless `FIBER_METRICS_TOKEN` is set, and then it wants that token as a bearer
/// credential. This process is reachable from the internet in a normal deployment, and the
/// figures here describe a customer's build volume, so it fails closed like the webhooks do
/// rather than defaulting to open the way a private-network exporter would.
async fn metrics(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let Some(expected) = std::env::var("FIBER_METRICS_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
    else {
        return (StatusCode::NOT_FOUND, "metrics disabled\n").into_response();
    };
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !constant_time_eq(presented.as_bytes(), expected.trim().as_bytes()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response();
    }
    let snapshot = match state.store.metrics_snapshot().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "metrics snapshot");
            return (StatusCode::INTERNAL_SERVER_ERROR, "metrics unavailable\n").into_response();
        }
    };
    let mut out = String::new();
    render_metrics(&mut out, &snapshot);
    render_loop_health(&mut out, &state.loop_health.snapshot());
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
        .into_response()
}

/// Prometheus text format. Kept separate from the handler so it can be tested without a
/// database.
fn render_metrics(out: &mut String, m: &fiber_core::MetricsSnapshot) {
    use std::fmt::Write;

    let _ = writeln!(out, "# HELP fiber_build_info Version of this fiber-api.");
    let _ = writeln!(out, "# TYPE fiber_build_info gauge");
    let _ = writeln!(
        out,
        "fiber_build_info{{version=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION")
    );

    let _ = writeln!(out, "# HELP fiber_step_runs Step runs by status.");
    let _ = writeln!(out, "# TYPE fiber_step_runs gauge");
    for (status, n) in &m.step_runs {
        let _ = writeln!(out, "fiber_step_runs{{status=\"{}\"}} {n}", esc(status));
    }

    let _ = writeln!(out, "# HELP fiber_runs Runs by status.");
    let _ = writeln!(out, "# TYPE fiber_runs gauge");
    for (status, n) in &m.runs {
        let _ = writeln!(out, "fiber_runs{{status=\"{}\"}} {n}", esc(status));
    }

    let _ = writeln!(out, "# HELP fiber_fibers Durable fibers by status.");
    let _ = writeln!(out, "# TYPE fiber_fibers gauge");
    for (status, n) in &m.fibers {
        let _ = writeln!(out, "fiber_fibers{{status=\"{}\"}} {n}", esc(status));
    }

    let _ = writeln!(
        out,
        "# HELP fiber_agents Registered agents by connectedness."
    );
    let _ = writeln!(out, "# TYPE fiber_agents gauge");
    let _ = writeln!(out, "fiber_agents{{state=\"online\"}} {}", m.agents_online);
    let _ = writeln!(
        out,
        "fiber_agents{{state=\"offline\"}} {}",
        m.agents_total - m.agents_online
    );

    let _ = writeln!(
        out,
        "# HELP fiber_oldest_queued_step_age_seconds Age of the oldest step waiting to be leased. 0 when nothing is waiting."
    );
    let _ = writeln!(out, "# TYPE fiber_oldest_queued_step_age_seconds gauge");
    let _ = writeln!(
        out,
        "fiber_oldest_queued_step_age_seconds {}",
        m.oldest_queued_step_age_secs.unwrap_or(0.0)
    );

    render_histogram(
        out,
        "fiber_step_queue_wait_seconds",
        "Seconds an attempt waited to be leased, from the step becoming queued.",
        &m.queue_wait,
    );
    render_histogram(
        out,
        "fiber_step_duration_seconds",
        "Seconds an attempt spent running, workspace preparation and artifact transfer included.",
        &m.step_duration,
    );
}

/// Background loop liveness. Restarts are cumulative and survive nothing but the process,
/// which is the honest shape: a restarted API has genuinely lost that history.
fn render_loop_health(
    out: &mut String,
    loops: &std::collections::BTreeMap<&'static str, crate::supervisor::LoopState>,
) {
    use std::fmt::Write;

    let _ = writeln!(
        out,
        "# HELP fiber_background_loop_up Whether a supervised background loop is running."
    );
    let _ = writeln!(out, "# TYPE fiber_background_loop_up gauge");
    for (name, st) in loops {
        let _ = writeln!(
            out,
            "fiber_background_loop_up{{loop=\"{name}\"}} {}",
            u8::from(st.running)
        );
    }
    let _ = writeln!(
        out,
        "# HELP fiber_background_loop_restarts_total Times a supervised loop had to be restarted."
    );
    let _ = writeln!(out, "# TYPE fiber_background_loop_restarts_total counter");
    for (name, st) in loops {
        let _ = writeln!(
            out,
            "fiber_background_loop_restarts_total{{loop=\"{name}\"}} {}",
            st.restarts
        );
    }
}

/// A Prometheus histogram: cumulative `_bucket` series, then `_sum` and `_count`.
fn render_histogram(out: &mut String, name: &str, help: &str, h: &fiber_core::Histogram) {
    use std::fmt::Write;

    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} histogram");
    for (le, n) in &h.buckets {
        let _ = writeln!(out, "{name}_bucket{{le=\"{le}\"}} {n}");
    }
    // `+Inf` must always be present and equal to the count, or the series is not a
    // histogram as far as Prometheus is concerned.
    let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {}", h.count);
    let _ = writeln!(out, "{name}_sum {}", h.sum);
    let _ = writeln!(out, "{name}_count {}", h.count);
}

/// Escape a Prometheus label value. Statuses are ours, but a label that could carry a quote
/// or a newline would produce a file no scraper can parse.
fn esc(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Readiness for the load balancer and the Compose healthcheck.
///
/// `503` when Postgres cannot be reached or a supervised loop is down: nothing useful
/// happens without them. Redis is reported but does not fail the probe. Leases,
/// scheduling, and the queue live in Postgres, so a replica without Redis still runs
/// builds; what it loses is live `/ws/runs` streaming, cross-replica cancel and
/// disconnect fan-out, and durable-fiber events. That is `"redis": "degraded"` with
/// `"degraded": true` at `200`, not an outage from the balancer's point of view.
async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    let mut checks = json!({
        "postgres": "ok",
        "redis": "ok",
    });
    let mut ok = true;
    let mut degraded = false;

    // Detail goes to the log, not to unauthenticated callers (connection strings leak).
    match tokio::time::timeout(
        PROBE_TIMEOUT,
        sqlx::query("SELECT 1").execute(&state.store.pool),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            ok = false;
            tracing::error!(error = %e, "readiness: postgres");
            checks["postgres"] = json!("error");
        }
        Err(_) => {
            ok = false;
            tracing::error!(
                secs = PROBE_TIMEOUT.as_secs(),
                "readiness: postgres probe timed out"
            );
            checks["postgres"] = json!("error");
        }
    }
    match tokio::time::timeout(PROBE_TIMEOUT, state.scheduler.redis_ping()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            degraded = true;
            tracing::warn!(error = %e, "readiness: redis unreachable; serving degraded");
            checks["redis"] = json!("degraded");
        }
        Err(_) => {
            degraded = true;
            tracing::warn!(
                secs = PROBE_TIMEOUT.as_secs(),
                "readiness: redis probe timed out; serving degraded"
            );
            checks["redis"] = json!("degraded");
        }
    }
    // A dead scheduler loop leaves the process answering requests while nothing is
    // reclaimed or scheduled. Saying `ok` through that is the failure this reports.
    let down = state.loop_health.down();
    if down.is_empty() {
        checks["loops"] = json!("ok");
    } else {
        ok = false;
        tracing::error!(loops = ?down, "readiness: background loops down");
        checks["loops"] = json!(down);
    }

    let body = json!({
        "ok": ok,
        "degraded": degraded,
        "service": "fiber-api",
        "checks": checks,
    });
    if ok {
        (StatusCode::OK, Json(body)).into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
    }
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<axum::response::Response, ApiError> {
    let key = crate::login_guard::LoginGuard::key(&req.username);
    if let Err(retry_after) = state.login_guard.check(&key) {
        return Ok(too_many_logins(retry_after));
    }
    let resp = state
        .store
        .login(&req.username, &req.password)
        .await
        .map_err(ApiError::from)?;
    match resp {
        Some(resp) => {
            state.login_guard.record_success(&key);
            Ok(Json(resp).into_response())
        }
        None => {
            if let Some(lock) = state.login_guard.record_failure(&key) {
                tracing::warn!(username = %key, lock_secs = lock.as_secs(), "login locked out");
            } else {
                tracing::warn!(username = %key, "login failed");
            }
            Err(ApiError::Unauthorized)
        }
    }
}

fn too_many_logins(retry_after: std::time::Duration) -> axum::response::Response {
    let secs = retry_after.as_secs().max(1);
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, secs.to_string())],
        Json(json!({
            "error": "too many failed logins; try again later",
            "retry_after_secs": secs,
        })),
    )
        .into_response()
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

#[derive(serde::Deserialize)]
pub struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

/// Change your own password. Requires the current one, and drops your other sessions.
///
/// Deliberately not something an admin can do to someone else: `PUT /api/users/{id}` sets
/// the admin flag and nothing more. An admin who could set passwords could take over an
/// account silently, which is a different power from managing one.
async fn change_password(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<impl IntoResponse, ApiError> {
    // Same floor the bootstrap admin password gets. Long enough to be worth argon2.
    if req.new_password.chars().count() < 8 {
        return Err(ApiError::BadRequest(
            "new password must be at least 8 characters".into(),
        ));
    }
    let token = crate::auth::bearer_from_headers(&headers).unwrap_or_default();
    let ok = state
        .store
        .change_password(user.id, &req.current_password, &req.new_password, &token)
        .await
        .map_err(ApiError::from)?;
    if !ok {
        // Same shape as a failed login: saying which half was wrong tells an attacker
        // holding a stolen session whether they have guessed the password.
        return Err(ApiError::Unauthorized);
    }
    Ok(Json(json!({ "ok": true })))
}

/// Drop every other session for the caller. The session making the request survives.
async fn revoke_sessions(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let token = crate::auth::bearer_from_headers(&headers);
    let revoked = state
        .store
        .revoke_sessions(user.id, token.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "revoked": revoked })))
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

/// How many runs are deleted per statement. Matches retention's batch: the cascade
/// reaches step runs, attempts, log lines and artifact rows, so one unbounded `DELETE`
/// would hold a write transaction and a pool connection for as long as the project is
/// large — an authenticated denial of service, since anyone can create a project and
/// fill it with runs.
const PROJECT_DELETE_BATCH: i64 = 100;

/// Delete a project and everything under it. Owner only, and irreversible.
///
/// No confirmation token on the wire: the role *is* the gate, matching every other
/// destructive route here, and the UI asks the operator to type the project name.
/// Instance admins are not exempt from membership — consistent with every other
/// project-scoped route (see `access.rs`).
///
/// The order is deliberate:
///
/// 1. Drop the sockets of agents dedicated to this project. They are about to be
///    cascade-deleted, and every other path that invalidates an agent row
///    (`delete_agent`, `rotate_agent_token`) disconnects first so the old session
///    cannot keep leasing.
/// 2. Cancel runs an agent is actually holding a step of, so those agents are told to
///    stop and release their slots while the rows still exist.
/// 3. Delete the runs in batches, taking each batch's artifact blob paths in the same
///    transaction. Retention only ever considers blobs belonging to runs it deletes
///    itself and never sweeps the backend for orphans, so a blob whose last row went
///    without being listed here is leaked for good.
/// 4. Delete the project. What is left for the cascade is small and fixed.
async fn delete_project(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Owner).await?;

    for agent_id in state
        .store
        .project_agent_ids(id)
        .await
        .map_err(ApiError::from)?
    {
        state
            .scheduler
            .force_disconnect_agent(agent_id, "project deleted")
            .await;
    }

    let leased = state
        .store
        .leased_run_ids_for_project(id)
        .await
        .map_err(ApiError::from)?;
    for run_id in &leased {
        // Best effort: a run that reached a terminal status between the query and here
        // is already where we want it, and must not block the delete.
        if let Err(e) = state
            .scheduler
            .cancel_run_with_reason(*run_id, Some("project deleted"))
            .await
        {
            tracing::warn!(%run_id, error = %e, "could not cancel run before project delete");
        }
    }

    let mut runs_deleted = 0usize;
    let mut blobs_deleted = 0usize;
    loop {
        let batch = state
            .store
            .run_ids_for_project(id, PROJECT_DELETE_BATCH)
            .await
            .map_err(ApiError::from)?;
        if batch.is_empty() {
            break;
        }
        runs_deleted += batch.len();
        let paths = state
            .store
            .delete_runs_returning_artifact_paths(&batch)
            .await
            .map_err(ApiError::from)?;
        blobs_deleted += gc_artifact_blobs(&state, paths).await;
    }

    if !state
        .store
        .delete_project(id)
        .await
        .map_err(ApiError::from)?
    {
        return Err(ApiError::NotFound);
    }

    tracing::info!(
        project_id = %id,
        by = %user.username,
        cancelled_runs = leased.len(),
        runs_deleted,
        blobs_deleted,
        "project deleted"
    );
    Ok(Json(json!({
        "ok": true,
        "cancelled_runs": leased.len(),
        "runs_deleted": runs_deleted,
        "blobs_deleted": blobs_deleted,
    })))
}

/// Remove the blobs among `paths` that no surviving artifact row points at. Returns how
/// many went.
///
/// Same rule retention follows: if the reference check fails, keep every blob. A
/// transient error must not read as "nothing points at these" and delete artifacts a
/// surviving retry still needs. Leaked bytes beat lost ones.
async fn gc_artifact_blobs(state: &AppState, paths: Vec<String>) -> usize {
    if paths.is_empty() {
        return 0;
    }
    let still = match state.store.artifact_paths_still_referenced(&paths).await {
        Ok(still) => still,
        Err(e) => {
            tracing::warn!(
                error = %e,
                candidates = paths.len(),
                "could not check artifact references; keeping every blob"
            );
            return 0;
        }
    };
    // Reuse retention's selection so there is one implementation of "safe to delete",
    // and it is the one its tests cover.
    let candidates: std::collections::BTreeSet<String> = paths.into_iter().collect();
    let mut removed = 0usize;
    for path in crate::retention::unreferenced_blobs(&candidates, &still) {
        match state.artifacts.delete(path).await {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(%path, error = %e, "artifact blob delete failed"),
        }
    }
    removed
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
    let role =
        ProjectRole::parse(&req.role).ok_or_else(|| ApiError::BadRequest("invalid role".into()))?;
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
    Ok(Json(
        json!({ "ok": true, "user_id": target.id, "role": role.as_str() }),
    ))
}

async fn update_member(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path((id, user_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<UpdateMemberRequest>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Admin).await?;
    let role =
        ProjectRole::parse(&req.role).ok_or_else(|| ApiError::BadRequest("invalid role".into()))?;
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
    // Instance admins only. Project owners invite via POST /members with a password.
    crate::access::require_instance_admin(&user)?;
    let created = state
        .store
        .create_user(&req.username, &req.password)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn list_users(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_instance_admin(&user)?;
    let users = state.store.list_users().await.map_err(ApiError::from)?;
    Ok(Json(users))
}

#[derive(Debug, Deserialize)]
struct UpdateUserRequest {
    is_admin: bool,
}

async fn update_user(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateUserRequest>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_instance_admin(&user)?;
    let updated = state
        .store
        .set_instance_admin(id, req.is_admin)
        .await
        .map_err(|e| {
            if e.to_string().ends_with("not found") {
                ApiError::NotFound
            } else {
                ApiError::from(e)
            }
        })?;
    Ok(Json(updated))
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
    // Through the scheduler, not the store: that is where superseding older runs of the
    // same concurrency group happens.
    let (run, steps, _) = state
        .scheduler
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

#[derive(Debug, Deserialize)]
struct ListRunsQuery {
    limit: Option<i64>,
    /// Run id from the previous page; results continue strictly older than it.
    before: Option<Uuid>,
}

async fn list_runs(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<ListRunsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_project(&state, &user, id, ProjectRole::Reader).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let runs = state
        .store
        .list_runs(id, limit, q.before)
        .await
        .map_err(ApiError::from)?;
    // Only advertise a cursor when the page was full: an empty next page is a wasted trip.
    let next_cursor = (runs.len() as i64 == limit)
        .then(|| runs.last().map(|r| r.id))
        .flatten();
    Ok(Json(json!({ "items": runs, "next_cursor": next_cursor })))
}

#[derive(Debug, Deserialize)]
struct RetryRunRequest {
    /// Carry over steps that already succeeded and re-run only the rest.
    #[serde(default)]
    failed_only: bool,
}

async fn retry_run(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    body: Option<Json<RetryRunRequest>>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_run(&state, &user, id, ProjectRole::Writer).await?;
    let failed_only = body.map(|Json(b)| b.failed_only).unwrap_or(false);
    let (run, steps) = state
        .store
        .retry_run(id, failed_only)
        .await
        .map_err(ApiError::from)?;
    if let Err(e) = state.scheduler.enqueue_run_ready(run.id).await {
        tracing::warn!(error = %e, run_id = %run.id, "retry enqueue failed");
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({ "run": run, "steps": steps })),
    ))
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
    if step.agent_id != Some(agent.id) || step.status != "running" {
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
    let rel = crate::artifact_util::sanitize_artifact_rel_path(header_path)
        .ok_or_else(|| ApiError::BadRequest("missing or invalid X-Fiber-Artifact-Path".into()))?;
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
        .create_artifact(step.run_id, step_run_id, &rel, &stored, body.len() as i64)
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
    if step.agent_id != Some(agent.id) || step.status != "running" {
        return Err(ApiError::Unauthorized);
    }
    if body.size > crate::artifact_util::MAX_ARTIFACT_BYTES {
        return Err(ApiError::BadRequest(format!(
            "artifact exceeds {} bytes",
            crate::artifact_util::MAX_ARTIFACT_BYTES
        )));
    }
    let rel = crate::artifact_util::sanitize_artifact_rel_path(&body.path)
        .ok_or_else(|| ApiError::BadRequest("missing or invalid path".into()))?;
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
    if step.agent_id != Some(agent.id) || step.status != "running" {
        return Err(ApiError::Unauthorized);
    }
    if body.size > crate::artifact_util::MAX_ARTIFACT_BYTES {
        return Err(ApiError::BadRequest(format!(
            "artifact exceeds {} bytes",
            crate::artifact_util::MAX_ARTIFACT_BYTES
        )));
    }
    let rel = crate::artifact_util::sanitize_artifact_rel_path(&body.path)
        .ok_or_else(|| ApiError::BadRequest("missing or invalid path".into()))?;
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
            return Err(ApiError::BadRequest("object not found after upload".into()));
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

/// Restore download. An agent may only read artifacts of runs in which it currently
/// holds a running step (that is exactly the restore list it was offered); anything
/// else is 404 so existence is not disclosed.
#[derive(serde::Deserialize)]
pub struct AgentDownloadQuery {
    /// `via=api` streams the bytes through this process instead of redirecting to object
    /// storage. Presigned URLs name the storage endpoint as the outside world reaches it,
    /// which an agent kept off that network cannot use.
    #[serde(default)]
    via: Option<String>,
}

async fn agent_download_artifact(
    AuthAgent(agent): AuthAgent,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<AgentDownloadQuery>,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::Redirect;
    if !state
        .store
        .agent_may_read_artifact(agent.id, id)
        .await
        .map_err(ApiError::from)?
    {
        return Err(ApiError::NotFound);
    }
    let artifact = state
        .store
        .get_artifact(id)
        .await
        .map_err(ApiError::from)?
        .ok_or(ApiError::NotFound)?;

    if q.via.as_deref() != Some("api") {
        if let Ok(Some(url)) = state.artifacts.presign_get(&artifact.path, 600).await {
            return Ok(Redirect::temporary(&url).into_response());
        }
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

#[derive(Debug, Deserialize)]
struct ListLogsQuery {
    /// Only this attempt's output. `seq` restarts per attempt, so mixing them interleaves.
    attempt: Option<i32>,
    /// Return lines after this id (follow a live step). Without it, the newest `limit`.
    after_id: Option<i64>,
    limit: Option<i64>,
}

async fn list_logs(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<ListLogsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    crate::access::require_step(&state, &user, id, ProjectRole::Reader).await?;
    let limit = q.limit.unwrap_or(1000).clamp(1, 5000);
    let logs = state
        .store
        .list_logs(id, q.attempt, q.after_id, limit)
        .await
        .map_err(ApiError::from)?;
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
    if state
        .store
        .get_project(id)
        .await
        .map_err(ApiError::from)?
        .is_none()
    {
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
        .ok_or(ApiError::NotFound)?;
    Ok(Json(fiber))
}

/// Durable task names this build has registered.
///
/// Any authenticated user: it is a list of what the server can run, not project data. The
/// UI offered a list copied into the frontend before this existed, which went stale the
/// moment a task was added or removed.
async fn list_fiber_tasks(
    AuthUser(_user): AuthUser,
    State(state): State<AppState>,
) -> impl IntoResponse {
    Json(json!({ "tasks": state.fiber_scheduler.registry().names() }))
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
        .ok_or(ApiError::NotFound)?;
    Ok(Json(fiber))
}

async fn list_agents(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Query(q): Query<ListAgentsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    match q.project_id {
        Some(pid) => {
            crate::access::require_project(&state, &user, pid, ProjectRole::Reader).await?;
        }
        // Without a project filter this returns every agent in the instance.
        None => crate::access::require_instance_admin(&user)?,
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

/// Global agents (no project) are instance-admin only: their token leases steps —
/// and receives secrets — from every project. Project agents need project admin
/// (instance admins may manage those too).
async fn require_agent_manage(
    state: &AppState,
    user: &fiber_core::PublicUser,
    agent: &fiber_core::Agent,
) -> Result<(), ApiError> {
    require_agent_scope(state, user, agent.project_id).await
}

async fn require_agent_scope(
    state: &AppState,
    user: &fiber_core::PublicUser,
    project_id: Option<Uuid>,
) -> Result<(), ApiError> {
    match project_id {
        None => crate::access::require_instance_admin(user),
        Some(_) if user.is_admin => Ok(()),
        Some(pid) => {
            crate::access::require_project(state, user, pid, ProjectRole::Admin).await?;
            Ok(())
        }
    }
}

async fn create_agent(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateAgentRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_agent_scope(&state, &user, req.project_id).await?;
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
    let mut agent = state.store.update_agent(id, req).await.map_err(|e| {
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
    // Drop the live session too, so a deleted agent cannot keep leasing on its old socket.
    state
        .scheduler
        .force_disconnect_agent(id, "agent deleted")
        .await;
    let ok = state.store.delete_agent(id).await.map_err(ApiError::from)?;
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
    let mut resp = state.store.rotate_agent_token(id).await.map_err(|e| {
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
    // An empty key makes the HMAC publicly computable, which would silently turn the
    // fail-closed webhook back into fail-open.
    let secret = body.secret.trim();
    if secret.is_empty() {
        return Err(ApiError::BadRequest(
            "webhook secret must not be empty".into(),
        ));
    }
    state
        .store
        .upsert_webhook_secret(id, "github", secret)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "ok": true })))
}

/// A commit id we are willing to hand to `git checkout`. Anything else (an empty string,
/// a revision expression, a value starting with `-` that git would read as an option) is
/// dropped rather than passed through.
fn valid_head_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.chars().all(|c| c.is_ascii_hexdigit())
}

/// A ref safe to pass to `git fetch`. Rejects option-looking values and path tricks.
fn valid_head_ref(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 255
        && !r.starts_with('-')
        && !r.contains("..")
        && !r.contains(char::is_whitespace)
        && !r
            .chars()
            .any(|c| c.is_control() || c == '~' || c == '^' || c == ':' || c == '?')
}

async fn github_webhook(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    body: String,
) -> Result<impl IntoResponse, ApiError> {
    // Fail closed: a project with no webhook secret configured accepts nothing.
    // Otherwise anyone who guesses a project id can start runs (which execute
    // repo-supplied shell with project secrets injected).
    // A secret that cannot be read (FIBER_SECRETS_KEY missing or rotated) is treated as
    // unconfigured: reject, and keep the reason in the server log only — this endpoint
    // is unauthenticated, so it must not become a project-existence oracle.
    let stored = state
        .store
        .get_webhook_secret(id, "github")
        .await
        .map_err(|e| {
            tracing::error!(project_id = %id, error = %e, "github webhook secret unreadable");
            ApiError::Unauthorized
        })?;
    let Some(secret) = stored.filter(|s| !s.is_empty()) else {
        tracing::debug!(project_id = %id, "github webhook rejected: no secret configured");
        return Err(ApiError::Unauthorized);
    };
    let sig = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !verify_github_sig(&secret, &body, sig) {
        return Err(ApiError::Unauthorized);
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
            // The commit that was pushed, so the agent builds it rather than whatever the
            // branch points at by the time it fetches.
            let commit = fiber_core::RunCommit {
                head_sha: payload
                    .get("after")
                    .or_else(|| payload.pointer("/head_commit/id"))
                    .and_then(|v| v.as_str())
                    .filter(|s| valid_head_sha(s) && !s.chars().all(|c| c == '0'))
                    .map(str::to_string),
                head_ref: Some(branch.clone()).filter(|b| valid_head_ref(b)),
                pr_number: None,
                repo_full_name: crate::github::repo_full_name(&payload)
                    .map(|(o, r)| format!("{o}/{r}")),
                // A push landed in the project's own repository.
                untrusted: false,
            };
            let pipelines = state
                .store
                .find_pipelines_for_push(id, &branch, &changed)
                .await
                .map_err(ApiError::from)?;
            let mut run_ids = Vec::new();
            for p in pipelines {
                let (run, _, _) = state
                    .scheduler
                    .start_run_for_commit(p.id, &format!("github:push:{branch}"), commit.clone())
                    .await
                    .map_err(ApiError::from)?;
                // Off the request path: a slow GitHub must not stall the delivery past
                // its timeout and cause a redelivery (and a duplicate run).
                let store = state.store.clone();
                let run_for_status = run.clone();
                tokio::spawn(async move {
                    crate::github::report_run_status(&store, &run_for_status).await;
                });
                state
                    .scheduler
                    .enqueue_run_ready(run.id)
                    .await
                    .map_err(ApiError::from)?;
                run_ids.push(run.id);
            }
            Ok(Json(
                json!({ "started": run_ids, "changed_files": changed.len() }),
            ))
        }
        "pull_request" => {
            let action = payload.get("action").and_then(|a| a.as_str()).unwrap_or("");
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
                && project_has_pr_path_filters(&state, id)
                    .await
                    .unwrap_or(true)
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
            // `refs/pull/<n>/head` is served by the base repository, so a pull request
            // from a fork builds without any access to the fork itself.
            let commit = fiber_core::RunCommit {
                head_sha: payload
                    .pointer("/pull_request/head/sha")
                    .and_then(|v| v.as_str())
                    .filter(|s| valid_head_sha(s))
                    .map(str::to_string),
                head_ref: (number > 0).then(|| format!("refs/pull/{number}/head")),
                pr_number: (number > 0).then_some(number as i32),
                repo_full_name: crate::github::repo_full_name(&payload)
                    .map(|(o, r)| format!("{o}/{r}")),
                // A pull request whose head lives in another repository was written by
                // someone outside the project. Build it, but give it no secrets.
                untrusted: {
                    let head_repo = payload
                        .pointer("/pull_request/head/repo/full_name")
                        .and_then(|v| v.as_str());
                    let base_repo = payload
                        .pointer("/repository/full_name")
                        .and_then(|v| v.as_str());
                    match (head_repo, base_repo) {
                        (Some(h), Some(b)) => h != b,
                        // Unknown provenance is treated as untrusted.
                        _ => true,
                    }
                },
            };
            let pipelines = state
                .store
                .find_pipelines_for_pull_request(id, &base, action, &changed)
                .await
                .map_err(ApiError::from)?;
            let mut run_ids = Vec::new();
            for p in pipelines {
                let (run, _, _) = state
                    .scheduler
                    .start_run_for_commit(
                        p.id,
                        &format!("github:pr:{number}:{action}"),
                        commit.clone(),
                    )
                    .await
                    .map_err(ApiError::from)?;
                // Off the request path: a slow GitHub must not stall the delivery past
                // its timeout and cause a redelivery (and a duplicate run).
                let store = state.store.clone();
                let run_for_status = run.clone();
                tokio::spawn(async move {
                    crate::github::report_run_status(&store, &run_for_status).await;
                });
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

async fn project_has_pr_path_filters(state: &AppState, project_id: Uuid) -> Result<bool, ApiError> {
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
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
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
        // Typed validation failures are the caller's fault.
        if e.downcast_ref::<fiber_core::ValidationError>().is_some()
            || e.downcast_ref::<fiber_core::DagError>().is_some()
        {
            return ApiError::BadRequest(format!("{e:#}"));
        }
        let msg = e.to_string();
        if msg.contains("forbidden") {
            ApiError::Forbidden
        } else {
            // Full cause chain: the log line is the only place this text now appears.
            ApiError::Internal(format!("{e:#}"))
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
            ApiError::Internal(m) => {
                // Never echo sqlx / Redis / anyhow chains to clients.
                tracing::error!(error = %m, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

#[cfg(test)]
mod concurrency_funnel {
    //! Starting a run has to go through the scheduler, which is where a new run cancels
    //! the older ones in its concurrency group. Four call sites reach this code — the
    //! manual start, both webhook paths, and the schedule loop — and a fifth that called
    //! the store directly would silently opt out of concurrency with nothing to show for
    //! it. Audit the source rather than trust the next author to notice.

    /// Everything above the first `#[cfg(test)]`, so this module's own text — which
    /// necessarily contains the pattern it looks for — is not what gets audited.
    fn handler_source() -> &'static str {
        let src = include_str!("routes.rs");
        &src[..src.find("#[cfg(test)]").unwrap_or(src.len())]
    }

    #[test]
    fn every_run_start_goes_through_the_scheduler() {
        let src = handler_source();
        let lines: Vec<&str> = src.lines().map(str::trim).collect();
        let mut offenders = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if !line.starts_with(".start_run") {
                continue;
            }
            // The receiver is the previous non-blank line: `.scheduler` or `.store`.
            let receiver = lines[..i]
                .iter()
                .rev()
                .find(|l| !l.is_empty())
                .copied()
                .unwrap_or("");
            if !receiver.contains("scheduler") {
                offenders.push(format!("line {}: {receiver} {line}", i + 1));
            }
        }
        assert!(
            offenders.is_empty(),
            "these start a run without the scheduler, skipping concurrency — use \
             state.scheduler.start_run / start_run_for_commit: {offenders:?}"
        );
    }

    #[test]
    fn the_audit_can_see_the_calls_it_is_guarding() {
        // A rename that made `.start_run` unfindable would leave the test above passing
        // over nothing at all.
        let n = handler_source()
            .lines()
            .filter(|l| l.trim().starts_with(".start_run"))
            .count();
        assert!(
            n >= 3,
            "expected the manual and both webhook starts, found {n}"
        );
    }
}

#[cfg(test)]
mod tests {

    //! Static audit of the router's own source. Convention 8 says every project-scoped
    //! handler goes through `access.rs`; a handler that resolves an id and proceeds
    //! without a role check is a security bug, not a style issue. Because the gate lives
    //! inside the handler body rather than in a layer, nothing but reading the code
    //! catches a missing one — and a real request test would need Postgres. So the test
    //! reads the code.

    /// The router and every handler, as compiled into this binary.
    const SRC: &str = include_str!("routes.rs");

    /// Handlers deliberately not behind a project role gate, each with the reason it is
    /// safe. Adding a handler to this list is the moment to think hard; adding one
    /// *without* thinking makes the test fail instead.
    const UNGATED_BY_DESIGN: &[(&str, &str)] = &[
        ("health", "liveness probe, no data"),
        ("ready", "readiness probe, no data"),
        (
            "metrics",
            "gated by its own bearer token, not a project role",
        ),
        ("login", "mints the session; no user yet"),
        ("logout", "revokes the caller's own session"),
        ("me", "returns the caller's own user"),
        ("change_password", "acts on the caller's own account"),
        ("revoke_sessions", "revokes the caller's own sessions"),
        (
            "list_projects",
            "filtered to the caller's memberships by the query itself",
        ),
        ("create_project", "no project exists yet to be a member of"),
        (
            "parse_yaml",
            "pure validation of a posted document; touches no stored data",
        ),
        (
            "list_fiber_tasks",
            "the static list of registered task names",
        ),
        (
            "github_webhook",
            "authenticated by HMAC signature, not a session",
        ),
        ("agent_ws", "authenticated by agent token at the handshake"),
        (
            "run_events_ws",
            "gates on the run inside the handler after upgrade",
        ),
        (
            "agent_upload_artifact",
            "AuthAgent; scoped to a step leased to that agent",
        ),
        (
            "agent_presign_artifact",
            "AuthAgent; scoped to a step leased to that agent",
        ),
        (
            "agent_complete_artifact",
            "AuthAgent; scoped to a step leased to that agent",
        ),
        (
            "agent_download_artifact",
            "AuthAgent; scoped to a step leased to that agent",
        ),
    ];

    /// `(name, body)` for every `async fn` declared at the top level of this file.
    fn handlers() -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        let mut rest = SRC;
        while let Some(at) = rest.find("\nasync fn ") {
            let after = &rest[at + "\nasync fn ".len()..];
            let name_end = after.find('(').expect("an async fn has an argument list");
            let name = &after[..name_end];
            let body_end = after.find("\nasync fn ").unwrap_or(after.len());
            out.push((name, &after[..body_end]));
            rest = after;
        }
        assert!(
            out.len() > 40,
            "expected the whole HTTP surface, got {}",
            out.len()
        );
        out
    }

    fn router_block() -> &'static str {
        let start = SRC.find("pub fn router(").expect("router");
        let end = SRC[start..].find("\nasync fn ").expect("end of router") + start;
        &SRC[start..end]
    }

    /// HTTP methods this handler is wired to, empty if it is not routed at all.
    fn methods_for(handler: &str) -> Vec<&'static str> {
        let router = router_block();
        ["get", "post", "put", "patch", "delete"]
            .into_iter()
            .filter(|m| router.contains(&format!("{m}({handler})")))
            .collect()
    }

    /// Every handler the router wires up, including ones defined in another module
    /// (`ws.rs`). Walking the router rather than this file's `async fn`s is what stops a
    /// handler from escaping the audit by living elsewhere.
    fn routed_handler_names() -> Vec<&'static str> {
        let router = router_block();
        let mut out: Vec<&'static str> = Vec::new();
        for verb in ["get(", "post(", "put(", "patch(", "delete("] {
            let mut rest = router;
            while let Some(at) = rest.find(verb) {
                let after = &rest[at + verb.len()..];
                if let Some(close) = after.find(')') {
                    let name = &after[..close];
                    let plausible = !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                    if plausible {
                        out.push(name);
                    }
                }
                rest = after;
            }
        }
        out.sort_unstable();
        out.dedup();
        assert!(
            out.len() > 40,
            "expected the whole routed surface, got {}: {out:?}",
            out.len()
        );
        out
    }

    /// The handler's body, when it is defined in this file. `None` means it lives in
    /// another module and this audit cannot read it — which is why such handlers have to
    /// be justified in `UNGATED_BY_DESIGN` explicitly.
    fn body_of(handler: &str) -> Option<&'static str> {
        handlers()
            .into_iter()
            .find(|(n, _)| *n == handler)
            .map(|(_, b)| b)
    }

    fn is_gated(body: &str) -> bool {
        body.contains("require_project")
            || body.contains("require_pipeline")
            || body.contains("require_run")
            || body.contains("require_step")
            || body.contains("require_artifact")
            || body.contains("require_fiber")
            || body.contains("require_instance_admin")
            || body.contains("require_agent_manage")
            || body.contains("require_agent_scope")
    }

    #[test]
    fn every_routed_handler_is_gated_or_listed_as_deliberately_ungated() {
        let allowed: Vec<&str> = UNGATED_BY_DESIGN.iter().map(|(n, _)| *n).collect();
        let mut ungated = Vec::new();
        for name in routed_handler_names() {
            if allowed.contains(&name) {
                continue;
            }
            // A handler defined outside this file cannot be read here, so it counts as
            // ungated until someone justifies it.
            match body_of(name) {
                Some(body) if is_gated(body) => continue,
                _ => ungated.push(name),
            }
        }
        assert!(
            ungated.is_empty(),
            "these routed handlers reach the store with no role gate and no documented \
             reason — add the `access.rs` call, or justify it in UNGATED_BY_DESIGN: {ungated:?}"
        );
    }

    #[test]
    fn a_handler_that_takes_an_id_and_a_session_always_resolves_a_role() {
        // The dangerous shape: an authenticated caller naming someone else's resource by
        // id. Anything matching it must consult access.rs.
        let allowed: Vec<&str> = UNGATED_BY_DESIGN.iter().map(|(n, _)| *n).collect();
        let mut unchecked = Vec::new();
        for (name, body) in handlers() {
            let takes_session = body.contains("AuthUser");
            let takes_id = body.contains("Path(") && body.contains("Uuid");
            if takes_session && takes_id && !is_gated(body) && !allowed.contains(&name) {
                unchecked.push(name);
            }
        }
        assert!(
            unchecked.is_empty(),
            "id-addressed handlers with no role check: {unchecked:?}"
        );
    }

    #[test]
    fn a_mutating_project_route_never_settles_for_reader() {
        // Reader is read-only by definition; a POST/PUT/DELETE gated on it would let any
        // member of a project change it.
        let mut too_weak = Vec::new();
        for (name, body) in handlers() {
            let mutating = methods_for(name)
                .iter()
                .any(|m| matches!(*m, "post" | "put" | "patch" | "delete"));
            if !mutating {
                continue;
            }
            if body.contains("ProjectRole::Reader") {
                too_weak.push(name);
            }
        }
        assert!(
            too_weak.is_empty(),
            "mutating routes gated only on Reader: {too_weak:?}"
        );
    }

    #[test]
    fn the_ungated_allowlist_has_no_stale_entries() {
        // A handler that was deleted or has since grown a gate should leave the list, so
        // the list keeps meaning something.
        let still_needed: Vec<&str> = routed_handler_names()
            .into_iter()
            .filter(|n| !body_of(n).map(is_gated).unwrap_or(false))
            .collect();
        let stale: Vec<&str> = UNGATED_BY_DESIGN
            .iter()
            .map(|(n, _)| *n)
            .filter(|n| !still_needed.contains(n))
            .collect();
        assert!(
            stale.is_empty(),
            "UNGATED_BY_DESIGN lists handlers that no longer need to be there: {stale:?}"
        );
    }

    #[test]
    fn every_ungated_entry_carries_a_reason() {
        for (name, reason) in UNGATED_BY_DESIGN {
            assert!(
                reason.len() > 10,
                "{name} is exempted without a real justification"
            );
        }
    }
    use super::*;

    fn sign(secret: &str, body: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body.as_bytes());
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn github_signature_accepts_valid() {
        let body = r#"{"ref":"refs/heads/main"}"#;
        assert!(verify_github_sig("s3cret", body, &sign("s3cret", body)));
    }

    #[test]
    fn github_signature_rejects_wrong_secret_body_or_prefix() {
        let body = r#"{"ref":"refs/heads/main"}"#;
        let sig = sign("s3cret", body);
        assert!(!verify_github_sig("other", body, &sig));
        assert!(!verify_github_sig("s3cret", "tampered", &sig));
        assert!(!verify_github_sig(
            "s3cret",
            body,
            sig.trim_start_matches("sha256=")
        ));
        assert!(!verify_github_sig(
            "s3cret",
            body,
            &sig.replace("sha256=", "sha1=")
        ));
        assert!(!verify_github_sig("s3cret", body, ""));
    }

    #[test]
    fn github_signature_rejects_truncated_or_padded() {
        let body = "x";
        let sig = sign("k", body);
        assert!(!verify_github_sig("k", body, &sig[..sig.len() - 1]));
        assert!(!verify_github_sig("k", body, &format!("{sig}0")));
    }

    #[test]
    fn metrics_render_is_parseable_exposition() {
        let m = fiber_core::MetricsSnapshot {
            step_runs: vec![("queued".into(), 3), ("running".into(), 1)],
            runs: vec![("succeeded".into(), 10)],
            fibers: vec![("pending".into(), 2)],
            agents_online: 2,
            agents_total: 5,
            oldest_queued_step_age_secs: Some(12.5),
            ..Default::default()
        };
        let mut out = String::new();
        render_metrics(&mut out, &m);

        assert!(out.contains("fiber_step_runs{status=\"queued\"} 3"));
        assert!(out.contains("fiber_runs{status=\"succeeded\"} 10"));
        assert!(out.contains("fiber_fibers{status=\"pending\"} 2"));
        assert!(out.contains("fiber_agents{state=\"online\"} 2"));
        // Offline is derived, not stored: total minus online.
        assert!(out.contains("fiber_agents{state=\"offline\"} 3"));
        assert!(out.contains("fiber_oldest_queued_step_age_seconds 12.5"));

        // Every metric line must be `name value`, and every metric needs HELP and TYPE.
        for line in out.lines() {
            if line.starts_with('#') {
                continue;
            }
            let (name, value) = line.rsplit_once(' ').expect("metric line has a value");
            assert!(!name.is_empty(), "{line}");
            value.parse::<f64>().unwrap_or_else(|_| panic!("{line}"));
        }
        for metric in [
            "fiber_build_info",
            "fiber_step_runs",
            "fiber_runs",
            "fiber_fibers",
            "fiber_agents",
            "fiber_oldest_queued_step_age_seconds",
            "fiber_step_queue_wait_seconds",
            "fiber_step_duration_seconds",
        ] {
            assert!(out.contains(&format!("# HELP {metric} ")), "{metric}");
            assert!(out.contains(&format!("# TYPE {metric} ")), "{metric}");
        }
    }

    #[test]
    fn histograms_are_cumulative_and_carry_an_inf_bucket() {
        let m = fiber_core::MetricsSnapshot {
            step_duration: fiber_core::Histogram {
                buckets: vec![(1.0, 2), (5.0, 5), (30.0, 9)],
                count: 11,
                sum: 421.5,
            },
            ..Default::default()
        };
        let mut out = String::new();
        render_metrics(&mut out, &m);

        assert!(
            out.contains(r#"fiber_step_duration_seconds_bucket{le="1"} 2"#),
            "{out}"
        );
        assert!(out.contains(r#"fiber_step_duration_seconds_bucket{le="5"} 5"#));
        assert!(out.contains(r#"fiber_step_duration_seconds_bucket{le="30"} 9"#));
        // Two observations sit above the last finite bucket; +Inf must still be the total.
        assert!(out.contains(r#"fiber_step_duration_seconds_bucket{le="+Inf"} 11"#));
        assert!(out.contains("fiber_step_duration_seconds_sum 421.5"));
        assert!(out.contains("fiber_step_duration_seconds_count 11"));

        // Buckets must never decrease as `le` grows.
        let mut last = 0i64;
        for line in out
            .lines()
            .filter(|l| l.starts_with("fiber_step_duration_seconds_bucket"))
        {
            let n: i64 = line.rsplit(' ').next().unwrap().parse().unwrap();
            assert!(n >= last, "not cumulative: {line}");
            last = n;
        }
    }

    #[test]
    fn empty_queue_reports_zero_rather_than_nothing() {
        // A missing series and a zero series read very differently on a dashboard.
        let mut out = String::new();
        render_metrics(&mut out, &fiber_core::MetricsSnapshot::default());
        assert!(out.contains("fiber_oldest_queued_step_age_seconds 0"));
        assert!(out.contains("fiber_agents{state=\"offline\"} 0"));
    }

    #[test]
    fn label_values_that_would_break_the_exposition_are_escaped() {
        let m = fiber_core::MetricsSnapshot {
            step_runs: vec![("od\"d\nstatus".into(), 1)],
            ..Default::default()
        };
        let mut out = String::new();
        render_metrics(&mut out, &m);
        assert!(
            out.contains(r#"fiber_step_runs{status="od\"d\nstatus"} 1"#),
            "{out}"
        );
        assert_eq!(
            out.lines()
                .filter(|l| l.contains("fiber_step_runs{"))
                .count(),
            1
        );
    }

    #[test]
    fn constant_time_eq_semantics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn api_error_maps_forbidden_from_anyhow() {
        let e: ApiError = anyhow::anyhow!("forbidden: reader cannot write").into();
        assert!(matches!(e, ApiError::Forbidden));
        let e: ApiError = anyhow::anyhow!("db down").into();
        assert!(matches!(e, ApiError::Internal(_)));
    }

    #[test]
    fn api_error_maps_typed_validation_to_400() {
        let e: ApiError = anyhow::Error::new(fiber_core::ValidationError("nope".into())).into();
        assert!(matches!(e, ApiError::BadRequest(m) if m == "nope"));
        let wrapped =
            anyhow::Error::new(fiber_core::ValidationError("inner".into())).context("outer");
        let e: ApiError = wrapped.into();
        assert!(matches!(e, ApiError::BadRequest(m) if m == "outer: inner"));
    }
}
