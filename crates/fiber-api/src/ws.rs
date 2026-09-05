use crate::artifacts::ArtifactBackend;
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use fiber_core::models::StepRun;
use fiber_core::store::value_to_definition;
use fiber_proto::{
    AgentMessage, ArtifactRestore, RunEvent, ServerMessage, StepStatus, WorkspaceOffer,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Deserialize)]
pub struct AgentQs {
    pub token: String,
}

pub async fn agent_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(qs): Query<AgentQs>,
) -> impl IntoResponse {
    let agent = match state.store.agent_by_token(&qs.token).await {
        Ok(Some(a)) => a,
        _ => {
            return (axum::http::StatusCode::UNAUTHORIZED, "invalid token").into_response();
        }
    };
    ws.on_upgrade(move |socket| handle_agent(socket, state, agent.id))
}

async fn offer_for_step(state: &AppState, step: StepRun) -> ServerMessage {
    let workspace = match state.store.get_run(step.run_id).await {
        Ok(Some(run)) => workspace_from_snapshot(&run.definition_snapshot),
        _ => None,
    };
    let restore = restore_list(state, step.run_id, step.id).await;
    let step_id = step.step_id.clone();
    ServerMessage::Offer {
        step_run_id: step.id,
        run_id: step.run_id,
        step_id: step_id.clone(),
        step_name: step.step_name,
        image: step.image,
        run: step.run_cmd,
        workspace,
        env: vec![
            ("FIBER_RUN_ID".into(), step.run_id.to_string()),
            ("FIBER_STEP_ID".into(), step_id),
        ],
        artifacts: vec![],
        restore,
    }
}

fn workspace_from_snapshot(snapshot: &serde_json::Value) -> Option<WorkspaceOffer> {
    // Snapshot is CompiledDag — workspace lives on original definition.
    // Prefer nested `workspace` if present; also accept top-level from pipeline def JSON.
    if let Some(ws) = snapshot.get("workspace") {
        let repo = ws.get("repo")?.as_str()?.to_string();
        let git_ref = ws
            .get("ref")
            .or_else(|| ws.get("git_ref"))
            .and_then(|v| v.as_str())
            .unwrap_or("main")
            .to_string();
        return Some(WorkspaceOffer { repo, git_ref });
    }
    // Fallback: try parsing as full pipeline definition embedded
    if let Ok(def) = value_to_definition(snapshot) {
        if let Some(ws) = def.workspace {
            return Some(WorkspaceOffer {
                repo: ws.repo,
                git_ref: ws.git_ref.unwrap_or_else(|| "main".into()),
            });
        }
    }
    None
}

async fn restore_list(
    state: &AppState,
    run_id: Uuid,
    current_step_run_id: Uuid,
) -> Vec<ArtifactRestore> {
    let Ok(arts) = state.store.list_artifacts(run_id).await else {
        return vec![];
    };
    // Prefer latest artifact per workspace path (name); skip this step's own (none yet).
    let mut by_name = std::collections::BTreeMap::new();
    for a in arts {
        if a.step_run_id == current_step_run_id {
            continue;
        }
        by_name.insert(
            a.name.clone(),
            ArtifactRestore {
                id: a.id,
                name: a.name,
                size: a.size.max(0) as u64,
            },
        );
    }
    by_name.into_values().collect()
}

/// Prefer workspace from the live pipeline definition (source of truth).
async fn offer_for_step_with_pipeline(state: &AppState, step: StepRun) -> ServerMessage {
    let mut workspace = None;
    let mut artifacts = Vec::new();
    let mut env = vec![
        ("FIBER_RUN_ID".into(), step.run_id.to_string()),
        ("FIBER_STEP_ID".into(), step.step_id.clone()),
    ];
    if let Ok(Some(run)) = state.store.get_run(step.run_id).await {
        if let Ok(secrets) = state.store.list_secret_values(run.project_id).await {
            for (k, v) in secrets {
                env.push((k, v));
            }
        }
        // Prefer compiled snapshot (matrix-expanded ids + artifacts + env).
        if let Some(steps) = run
            .definition_snapshot
            .get("steps")
            .and_then(|v| v.as_array())
        {
            for s in steps {
                if s.get("id").and_then(|v| v.as_str()) != Some(step.step_id.as_str()) {
                    continue;
                }
                if let Some(arr) = s.get("artifacts").and_then(|v| v.as_array()) {
                    artifacts = arr
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                }
                if let Some(pairs) = s.get("env").and_then(|v| v.as_array()) {
                    for p in pairs {
                        if let (Some(k), Some(v)) = (
                            p.get(0).and_then(|x| x.as_str()),
                            p.get(1).and_then(|x| x.as_str()),
                        ) {
                            env.push((k.to_string(), v.to_string()));
                        }
                    }
                }
                break;
            }
        }
        if let Ok(Some(pipe)) = state.store.get_pipeline(run.pipeline_id).await {
            if let Ok(def) = value_to_definition(&pipe.definition) {
                if let Some(ws) = def.workspace {
                    workspace = Some(WorkspaceOffer {
                        repo: ws.repo,
                        git_ref: ws.git_ref.unwrap_or_else(|| "main".into()),
                    });
                }
                if artifacts.is_empty() {
                    if let Some(s) = def.steps.iter().find(|s| s.id == step.step_id) {
                        artifacts = s.artifacts.clone();
                    }
                }
            }
        }
        if workspace.is_none() {
            workspace = workspace_from_snapshot(&run.definition_snapshot);
        }
    }
    let restore = restore_list(state, step.run_id, step.id).await;
    ServerMessage::Offer {
        step_run_id: step.id,
        run_id: step.run_id,
        step_id: step.step_id.clone(),
        step_name: step.step_name,
        image: step.image,
        run: step.run_cmd,
        workspace,
        env,
        artifacts,
        restore,
    }
}

async fn handle_agent(socket: WebSocket, state: AppState, mut agent_id: Uuid) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

    let welcome = ServerMessage::Welcome { agent_id };
    if sink
        .send(Message::Text(
            serde_json::to_string(&welcome).unwrap_or_default().into(),
        ))
        .await
        .is_err()
    {
        return;
    }

    state
        .scheduler
        .register_connection(agent_id, tx.clone())
        .await;

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let Ok(text) = serde_json::to_string(&msg) else {
                continue;
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let _ = state.store.set_agent_online(agent_id, true).await;

    while let Some(Ok(msg)) = stream.next().await {
        let Message::Text(text) = msg else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<AgentMessage>(&text) else {
            let _ = tx.send(ServerMessage::Error {
                message: "invalid message".into(),
            });
            continue;
        };

        match parsed {
            AgentMessage::Hello {
                name,
                labels,
                concurrency,
            } => {
                info!(%agent_id, %name, ?labels, "agent hello");
                // Reload from DB so pool scope is authoritative (not client-supplied).
                let project_id = state
                    .store
                    .get_agent(agent_id)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|a| a.project_id);
                state
                    .scheduler
                    .register_agent(agent_id, labels, concurrency, project_id)
                    .await;
                // Re-bind connection under confirmed agent_id (same as token)
                state
                    .scheduler
                    .register_connection(agent_id, tx.clone())
                    .await;
                let _ = state.store.touch_agent(agent_id).await;
                if let Ok(Some(step)) = state.scheduler.offer_for_agent(agent_id).await {
                    let _ = tx.send(offer_for_step_with_pipeline(&state, step).await);
                }
            }
            AgentMessage::Heartbeat { agent_id: aid } => {
                if !state.scheduler.has_connection(aid).await {
                    let _ = tx.send(ServerMessage::Error {
                        message: "session ended — reconnect".into(),
                    });
                    break;
                }
                agent_id = aid;
                let _ = state.store.touch_agent(aid).await;
                let _ = state.scheduler.renew_leases(aid).await;
                if let Ok(Some(step)) = state.scheduler.offer_for_agent(aid).await {
                    let _ = tx.send(offer_for_step_with_pipeline(&state, step).await);
                }
            }
            AgentMessage::Claim {
                agent_id: _,
                step_run_id: _,
            } => {}
            AgentMessage::LogChunk {
                agent_id: _,
                step_run_id,
                stream: stream_name,
                data,
                seq,
            } => {
                if let Ok(Some(step)) = state.store.get_step_run(step_run_id).await {
                    if let Ok(line) = state
                        .store
                        .append_log(step.run_id, step_run_id, &stream_name, &data, seq)
                        .await
                    {
                        let ev = RunEvent::Log {
                            run_id: step.run_id,
                            step_run_id,
                            stream: stream_name,
                            data,
                            seq,
                            at: line.created_at,
                        };
                        if let Ok(payload) = serde_json::to_string(&ev) {
                            state.scheduler.publish_event(&payload).await;
                        }
                    }
                }
            }
            AgentMessage::Artifact {
                agent_id: _,
                step_run_id,
                name,
                path: rel_path,
                size,
                content_base64,
            } => {
                // Prefer HTTP upload; keep WS base64 as a small-file fallback.
                let Some(b64) = content_base64 else {
                    continue;
                };
                if let Ok(Some(step)) = state.store.get_step_run(step_run_id).await {
                    let rel = crate::artifact_util::sanitize_artifact_rel_path(&rel_path)
                        .or_else(|| crate::artifact_util::sanitize_artifact_rel_path(&name));
                    let Some(rel) = rel else {
                        continue;
                    };
                    if size > crate::artifact_util::MAX_WS_ARTIFACT_BYTES {
                        warn!(%step_run_id, size, "artifact too large for WS; use HTTP upload");
                        continue;
                    }
                    if let Ok(bytes) = base64_decode(&b64) {
                        if bytes.len() as u64 > crate::artifact_util::MAX_WS_ARTIFACT_BYTES {
                            warn!(%step_run_id, "artifact payload too large; skipping");
                            continue;
                        }
                        let key = ArtifactBackend::object_key(
                            &step.run_id.to_string(),
                            &step_run_id.to_string(),
                            &rel.replace('/', "__"),
                        );
                        match state.artifacts.put(&key, &bytes).await {
                            Ok(stored_path) => {
                                let _ = state
                                    .store
                                    .create_artifact(
                                        step.run_id,
                                        step_run_id,
                                        &rel,
                                        &stored_path,
                                        bytes.len() as i64,
                                    )
                                    .await;
                            }
                            Err(e) => warn!(error = %e, %step_run_id, "artifact store failed"),
                        }
                    }
                }
            }
            AgentMessage::StepComplete {
                agent_id: aid,
                step_run_id,
                status,
                exit_code,
                error,
            } => {
                match state
                    .scheduler
                    .on_step_complete(aid, step_run_id, status, exit_code, error)
                    .await
                {
                    Ok(_) => {
                        if let Ok(Some(step)) = state.scheduler.offer_for_agent(aid).await {
                            let _ = tx.send(offer_for_step_with_pipeline(&state, step).await);
                        }
                    }
                    Err(e) => warn!(error = %e, "step complete failed"),
                }
            }
        }
    }

    let _ = state.store.set_agent_online(agent_id, false).await;
    if let Err(e) = state.scheduler.on_agent_disconnect(agent_id).await {
        warn!(error = %e, %agent_id, "agent disconnect cleanup failed");
    }
    writer.abort();
}

fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|_| ())
}

#[derive(Deserialize)]
pub struct RunEventsQs {
    pub token: String,
}

pub async fn run_events_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(run_id): Path<Uuid>,
    Query(qs): Query<RunEventsQs>,
) -> impl IntoResponse {
    let user = match state.store.user_by_session_token(&qs.token).await {
        Ok(Some(u)) => u,
        _ => {
            return (axum::http::StatusCode::UNAUTHORIZED, "invalid token").into_response();
        }
    };
    let project_id = match state.store.project_id_for_run(run_id).await {
        Ok(Some(pid)) => pid,
        Ok(None) => {
            return (axum::http::StatusCode::NOT_FOUND, "run not found").into_response();
        }
        Err(_) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "lookup failed",
            )
                .into_response();
        }
    };
    if state
        .store
        .require_role(project_id, user.id, fiber_core::ProjectRole::Reader)
        .await
        .is_err()
    {
        return (axum::http::StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    ws.on_upgrade(move |socket| handle_run_events(socket, state, run_id))
}

async fn handle_run_events(socket: WebSocket, state: AppState, run_id: Uuid) {
    let (mut sink, mut stream) = socket.split();
    let mut sub = state.scheduler.subscribe();

    if let Ok(Some(run)) = state.store.get_run(run_id).await {
        let ev = RunEvent::RunUpdated {
            run_id,
            status: run.status_enum(),
        };
        if let Ok(text) = serde_json::to_string(&ev) {
            let _ = sink.send(Message::Text(text.into())).await;
        }
        if let Ok(steps) = state.store.list_step_runs(run_id).await {
            for s in steps {
                let ev = RunEvent::StepUpdated {
                    run_id,
                    step_run_id: s.id,
                    step_id: s.step_id.clone(),
                    status: s.status_enum(),
                };
                if let Ok(text) = serde_json::to_string(&ev) {
                    let _ = sink.send(Message::Text(text.into())).await;
                }
            }
        }
    }

    loop {
        tokio::select! {
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(p))) => {
                        let _ = sink.send(Message::Pong(p)).await;
                    }
                    _ => {}
                }
            }
            ev = sub.recv() => {
                match ev {
                    Ok(payload) => {
                        if let Ok(parsed) = serde_json::from_str::<RunEvent>(&payload) {
                            let matches = match &parsed {
                                RunEvent::RunUpdated { run_id: rid, .. } => *rid == run_id,
                                RunEvent::StepUpdated { run_id: rid, .. } => *rid == run_id,
                                RunEvent::Log { run_id: rid, .. } => *rid == run_id,
                            };
                            if matches {
                                if sink.send(Message::Text(payload.into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

#[allow(dead_code)]
async fn _unused_offer(state: &AppState, step: StepRun) -> ServerMessage {
    offer_for_step(state, step).await
}

#[allow(dead_code)]
fn _status_ok() -> StepStatus {
    StepStatus::Succeeded
}
