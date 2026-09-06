use crate::artifacts::ArtifactBackend;
use crate::state::AppState;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use fiber_core::models::StepRun;
use fiber_proto::{
    AgentMessage, ArtifactRestore, RunEvent, ServerMessage, StepStatus, WorkspaceOffer,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
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
    // 401 is fatal for the agent (it exits rather than retrying a revoked token), so a
    // store error must not be mistaken for one.
    let agent = match state.store.agent_by_token(&qs.token).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            return (axum::http::StatusCode::UNAUTHORIZED, "invalid token").into_response();
        }
        Err(e) => {
            warn!(error = %e, "agent auth lookup failed");
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "auth unavailable",
            )
                .into_response();
        }
    };
    let token_hash = fiber_core::tokens::hash_token(&qs.token);
    ws.on_upgrade(move |socket| handle_agent(socket, state, agent.id, token_hash))
}

/// Build the offer for a leased step from the run's definition snapshot **only**.
///
/// The snapshot (`CompiledDag`, stored on the run at start) is immutable for that
/// execution: editing the pipeline while a run is in flight must not change the
/// workspace, command, artifacts or env an agent receives. Nothing here reads the
/// live pipeline row.
async fn offer_for_step(state: &AppState, step: StepRun) -> ServerMessage {
    let mut env = vec![
        ("FIBER_RUN_ID".into(), step.run_id.to_string()),
        ("FIBER_STEP_ID".into(), step.step_id.clone()),
    ];
    let mut artifacts = Vec::new();
    let mut workspace = None;
    let mut timeout_minutes = None;
    let mut secret_keys = Vec::new();
    let mut needs_closure = None;
    match state.store.get_run(step.run_id).await {
        Ok(Some(run)) => {
            let snapshot = &run.definition_snapshot;
            workspace = workspace_from_snapshot(snapshot).map(|mut ws| {
                // The webhook recorded what to build; the snapshot only knows the
                // pipeline's default ref.
                if let Some(r) = run.head_ref.clone() {
                    ws.git_ref = r;
                }
                ws.sha = run.head_sha.clone();
                ws
            });
            let mut secret_allow: Option<Vec<String>> = None;
            match snapshot_step(snapshot, &step.step_id) {
                Some(s) => {
                    artifacts = snapshot_str_list(s.get("artifacts"));
                    env.extend(snapshot_env(s.get("env")));
                    timeout_minutes = s
                        .get("timeout_minutes")
                        .and_then(|v| v.as_u64())
                        .map(|m| m as u32);
                    // Absent (or null) = every project secret; a list = only those names.
                    secret_allow = s
                        .get("secrets")
                        .filter(|v| !v.is_null())
                        .map(|v| snapshot_str_list(Some(v)));
                    needs_closure = Some(needs_closure_for(snapshot, &step.step_id));
                }
                None => warn!(
                    run_id = %step.run_id, step = %step.step_id,
                    "step missing from run snapshot; offering without artifacts/env"
                ),
            }
            if run.untrusted {
                warn!(
                    run_id = %run.id,
                    "run builds code from outside the project (fork pull request); injecting no secrets"
                );
            } else if let Ok(secrets) = state.store.list_secret_values(run.project_id).await {
                let mut available: Vec<String> = Vec::new();
                for (k, v) in secrets {
                    available.push(k.clone());
                    if secret_allow.as_ref().is_some_and(|a| !a.contains(&k)) {
                        continue;
                    }
                    secret_keys.push(k.clone());
                    env.push((k, v));
                }
                if let Some(allow) = &secret_allow {
                    for name in allow {
                        if !available.contains(name) {
                            warn!(
                                run_id = %step.run_id, step = %step.step_id, secret = %name,
                                "step requests a secret the project does not define"
                            );
                        }
                    }
                }
            }
        }
        Ok(None) => warn!(run_id = %step.run_id, "run missing while building offer"),
        Err(e) => warn!(run_id = %step.run_id, error = %e, "loading run for offer"),
    }
    let restore = restore_list(state, step.run_id, step.id, needs_closure.as_ref()).await;
    // Agents always get a limit: the step's own, else the server default.
    let default_minutes = fiber_scheduler::TimeoutConfig::from_env().default_minutes as u32;
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
        timeout_minutes: Some(timeout_minutes.unwrap_or(default_minutes)),
        secret_keys,
    }
}

/// Every step `step_id` transitively depends on, from the run snapshot. A step may only
/// see artifacts produced by these — the whole run's output would let unrelated parallel
/// steps drop files into its workspace.
fn needs_closure_for(snapshot: &serde_json::Value, step_id: &str) -> HashSet<String> {
    let mut needs_of: HashMap<&str, Vec<&str>> = HashMap::new();
    if let Some(steps) = snapshot.get("steps").and_then(|v| v.as_array()) {
        for s in steps {
            let Some(id) = s.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let needs = s
                .get("needs")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|n| n.as_str()).collect())
                .unwrap_or_default();
            needs_of.insert(id, needs);
        }
    }
    let mut closure = HashSet::new();
    let mut stack: Vec<&str> = needs_of.get(step_id).cloned().unwrap_or_default();
    while let Some(id) = stack.pop() {
        if !closure.insert(id.to_string()) {
            continue;
        }
        if let Some(parents) = needs_of.get(id) {
            stack.extend(parents.iter().copied());
        }
    }
    closure
}

/// The compiled step entry for `step_id` inside a `CompiledDag` snapshot.
fn snapshot_step<'a>(
    snapshot: &'a serde_json::Value,
    step_id: &str,
) -> Option<&'a serde_json::Value> {
    snapshot
        .get("steps")?
        .as_array()?
        .iter()
        .find(|s| s.get("id").and_then(|v| v.as_str()) == Some(step_id))
}

fn snapshot_str_list(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn snapshot_env(v: Option<&serde_json::Value>) -> Vec<(String, String)> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    Some((
                        p.get(0)?.as_str()?.to_string(),
                        p.get(1)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `CompiledDag.workspace` — serialized `WorkspaceConfig` (`{repo, ref}`) or `null`.
fn workspace_from_snapshot(snapshot: &serde_json::Value) -> Option<WorkspaceOffer> {
    let ws = snapshot.get("workspace")?;
    let repo = ws.get("repo")?.as_str()?.to_string();
    let git_ref = ws
        .get("ref")
        .or_else(|| ws.get("git_ref"))
        .and_then(|v| v.as_str())
        .unwrap_or("main")
        .to_string();
    Some(WorkspaceOffer {
        repo,
        git_ref,
        sha: None,
    })
}

/// Artifacts to place in the step's workspace before it runs: those produced by the
/// steps it depends on, latest wins per path.
async fn restore_list(
    state: &AppState,
    run_id: Uuid,
    current_step_run_id: Uuid,
    needs_closure: Option<&HashSet<String>>,
) -> Vec<ArtifactRestore> {
    let Ok(arts) = state.store.list_artifacts(run_id).await else {
        return vec![];
    };
    // step_run_id -> step_id, so artifacts can be attributed to the step that produced them.
    let producer: HashMap<Uuid, String> = match state.store.list_step_runs(run_id).await {
        Ok(steps) => steps.into_iter().map(|s| (s.id, s.step_id)).collect(),
        Err(e) => {
            warn!(%run_id, error = %e, "cannot map artifacts to steps; restoring none");
            return vec![];
        }
    };
    let mut by_name = std::collections::BTreeMap::new();
    for a in arts {
        if a.step_run_id == current_step_run_id {
            continue;
        }
        // No closure means the step is not in the snapshot — the offer already went out
        // without its artifacts or env, so restore nothing rather than everything.
        let Some(closure) = needs_closure else {
            continue;
        };
        if !producer
            .get(&a.step_run_id)
            .is_some_and(|id| closure.contains(id))
        {
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

/// The step a message refers to, but only if `agent_id` is the agent it was last
/// leased to. Used for log lines: output that arrives after a cancel or reclaim is
/// still this agent's output for its own attempt, and is what an operator reads to
/// learn why the step stopped.
async fn owned_step(state: &AppState, agent_id: Uuid, step_run_id: Uuid) -> Option<StepRun> {
    let step = state.store.get_step_run(step_run_id).await.ok().flatten()?;
    if step.agent_id != Some(agent_id) {
        tracing::debug!(
            %agent_id, %step_run_id, owner = ?step.agent_id,
            "dropping agent message for a step it does not own"
        );
        return None;
    }
    Some(step)
}

/// Like `owned_step`, but the lease must still be live. Used for artifacts (and,
/// via the scheduler, completions): once a step was reclaimed or re-leased, the
/// re-leased attempt reports its own outputs — this attempt's are dropped.
async fn leased_step(state: &AppState, agent_id: Uuid, step_run_id: Uuid) -> Option<StepRun> {
    let step = owned_step(state, agent_id, step_run_id).await?;
    if step.status_enum() != StepStatus::Running {
        tracing::debug!(
            %agent_id, %step_run_id, status = %step.status,
            "dropping agent message for a step whose lease ended"
        );
        return None;
    }
    Some(step)
}

/// `agent_id` is bound once from the authenticated token and never rebound from a
/// client-supplied field: an agent may only ever act as itself.
async fn handle_agent(socket: WebSocket, state: AppState, agent_id: Uuid, token_hash: String) {
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
    // Log a spoofed agent_id once per session, not once per log line.
    let mut spoof_logged = false;

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
                // A force-disconnected session (token rotated, agent deleted) must not be
                // able to re-register itself by replaying Hello.
                if !state.scheduler.has_connection(agent_id).await {
                    let _ = tx.send(ServerMessage::Error {
                        message: "session ended — reconnect".into(),
                    });
                    break;
                }
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
                    let _ = tx.send(offer_for_step(&state, step).await);
                }
            }
            AgentMessage::Heartbeat { agent_id: claimed } => {
                warn_if_spoofed(agent_id, claimed, &mut spoof_logged);
                // Revocation must not depend on Redis delivery: a rotated or deleted token
                // ends the session on its next heartbeat, whichever replica holds it.
                let still_valid = matches!(
                    state.store.get_agent(agent_id).await,
                    Ok(Some(a)) if a.token_hash == token_hash
                );
                if !still_valid {
                    warn!(%agent_id, "agent token no longer valid; ending session");
                    let _ = tx.send(ServerMessage::Error {
                        message: "agent token revoked — reconnect with a valid token".into(),
                    });
                    break;
                }
                // force_disconnect_agent drops the connection; the next heartbeat ends the session.
                if !state.scheduler.has_connection(agent_id).await {
                    let _ = tx.send(ServerMessage::Error {
                        message: "session ended — reconnect".into(),
                    });
                    break;
                }
                let _ = state.store.touch_agent(agent_id).await;
                let _ = state.scheduler.renew_leases(agent_id).await;
                if let Ok(Some(step)) = state.scheduler.offer_for_agent(agent_id).await {
                    let _ = tx.send(offer_for_step(&state, step).await);
                }
            }
            AgentMessage::Claim {
                agent_id: claimed,
                step_run_id: _,
            } => {
                warn_if_spoofed(agent_id, claimed, &mut spoof_logged);
            }
            AgentMessage::LogChunk {
                agent_id: claimed,
                step_run_id,
                stream: stream_name,
                data,
                seq,
            } => {
                warn_if_spoofed(agent_id, claimed, &mut spoof_logged);
                if let Some(step) = owned_step(&state, agent_id, step_run_id).await {
                    if let Ok(line) = state
                        .store
                        .append_log(
                            step.run_id,
                            step_run_id,
                            &stream_name,
                            &data,
                            seq,
                            step.attempt,
                        )
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
                agent_id: claimed,
                step_run_id,
                name,
                path: rel_path,
                size,
                content_base64,
            } => {
                warn_if_spoofed(agent_id, claimed, &mut spoof_logged);
                // Prefer HTTP upload; keep WS base64 as a small-file fallback.
                let Some(b64) = content_base64 else {
                    continue;
                };
                if let Some(step) = leased_step(&state, agent_id, step_run_id).await {
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
                agent_id: claimed,
                step_run_id,
                status,
                exit_code,
                error,
            } => {
                warn_if_spoofed(agent_id, claimed, &mut spoof_logged);
                // on_step_complete additionally rejects completions for steps not leased to agent_id.
                match state
                    .scheduler
                    .on_step_complete(agent_id, step_run_id, status, exit_code, error)
                    .await
                {
                    Ok(_) => {
                        if let Ok(Some(step)) = state.scheduler.offer_for_agent(agent_id).await {
                            let _ = tx.send(offer_for_step(&state, step).await);
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

/// Client-supplied `agent_id` fields are ignored; log the first disagreement per session.
fn warn_if_spoofed(bound: Uuid, claimed: Uuid, logged: &mut bool) {
    if bound != claimed && !*logged {
        *logged = true;
        warn!(%bound, %claimed, "agent message carried a different agent_id; ignoring it");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn workspace_comes_from_snapshot_only() {
        let snap = json!({"workspace": {"repo": "https://x/y.git", "ref": "v1"}, "steps": []});
        let ws = workspace_from_snapshot(&snap).unwrap();
        assert_eq!(
            (ws.repo.as_str(), ws.git_ref.as_str()),
            ("https://x/y.git", "v1")
        );
        // Missing ref defaults to main; null / absent workspace → no checkout.
        let snap = json!({"workspace": {"repo": "r"}});
        assert_eq!(workspace_from_snapshot(&snap).unwrap().git_ref, "main");
        assert!(workspace_from_snapshot(&json!({"workspace": null})).is_none());
        assert!(workspace_from_snapshot(&json!({"steps": []})).is_none());
    }

    #[test]
    fn needs_closure_is_transitive_and_excludes_siblings() {
        let snap = json!({"steps": [
            {"id": "checkout", "needs": []},
            {"id": "build", "needs": ["checkout"]},
            {"id": "test-a", "needs": ["build"]},
            {"id": "test-b", "needs": ["build"]},
            {"id": "package", "needs": ["test-a"]}
        ]});
        let c = needs_closure_for(&snap, "package");
        assert!(c.contains("test-a") && c.contains("build") && c.contains("checkout"));
        // A sibling branch is not a dependency: its artifacts must not be restored.
        assert!(!c.contains("test-b"));
        assert!(!c.contains("package"));
        assert!(needs_closure_for(&snap, "checkout").is_empty());
        assert!(needs_closure_for(&snap, "nope").is_empty());
    }

    #[test]
    fn step_artifacts_and_env_come_from_snapshot() {
        let snap = json!({"steps": [
            {"id": "a", "artifacts": ["out/a"], "env": [["MATRIX_OS", "linux"], ["bad"]]},
            {"id": "b"}
        ]});
        let a = snapshot_step(&snap, "a").unwrap();
        assert_eq!(
            snapshot_str_list(a.get("artifacts")),
            vec!["out/a".to_string()]
        );
        assert_eq!(
            snapshot_env(a.get("env")),
            vec![("MATRIX_OS".to_string(), "linux".to_string())]
        );
        let b = snapshot_step(&snap, "b").unwrap();
        assert!(snapshot_str_list(b.get("artifacts")).is_empty());
        assert!(snapshot_env(b.get("env")).is_empty());
        assert!(snapshot_step(&snap, "zzz").is_none());
    }
}
