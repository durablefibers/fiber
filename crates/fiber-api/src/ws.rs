use crate::artifacts::ArtifactBackend;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, close_code};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use fiber_core::models::StepRun;
use fiber_proto::limits::truncate_log_line;
use fiber_proto::{
    AgentMessage, ArtifactRestore, LogEventLine, LogLineWire, RunEvent, ServerMessage, StepStatus,
    WorkspaceOffer,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument;
use tracing::{error, info, warn};
use uuid::Uuid;

/// Server-initiated liveness for agent sockets. An agent whose host vanished — NAT
/// table timeout, power loss, a VM snapshot — leaves a half-open TCP connection that
/// the kernel can keep for hours. Until now that agent stayed `online` and kept
/// receiving offers nobody would run. Any frame counts as life (the agent's own
/// heartbeat comes every 10 s), so a healthy agent never gets near the deadline.
const AGENT_PING_INTERVAL: Duration = Duration::from_secs(15);
/// Two pings unanswered.
const AGENT_LIVENESS_TIMEOUT: Duration = Duration::from_secs(45);
/// How long the writer gets to put the Close frame on the wire before it is dropped.
const CLOSE_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

fn close_frame(code: u16, reason: &'static str) -> CloseFrame {
    CloseFrame {
        code,
        reason: reason.into(),
    }
}

/// Resolves once the process is shutting down (immediately if it already is). The
/// `watch::Ref` that `wait_for` hands back is dropped in here rather than in a
/// `select!` arm, where holding it across an `await` would make the session future
/// `!Send`.
async fn shutting_down(rx: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = rx.wait_for(|stop| *stop).await;
}

#[derive(Deserialize)]
pub struct AgentQs {
    /// Deprecated. A query string ends up in proxy and server access logs, where an agent
    /// token — which leases steps and receives project secrets — has no business being.
    /// Kept so an agent older than this server can still connect.
    #[serde(default)]
    pub token: Option<String>,
}

pub async fn agent_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(qs): Query<AgentQs>,
) -> impl IntoResponse {
    let from_header = crate::auth::bearer_from_headers(&headers);
    if from_header.is_none() && qs.token.is_some() {
        warn!(
            "agent authenticated with a token in the query string; upgrade the agent so it \
             sends an Authorization header instead"
        );
    }
    let Some(token) = from_header.or_else(|| qs.token.clone()) else {
        return (axum::http::StatusCode::UNAUTHORIZED, "missing token").into_response();
    };
    // 401 is fatal for the agent (it exits rather than retrying a revoked token), so a
    // store error must not be mistaken for one.
    let agent = match state.store.agent_by_token(&token).await {
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
    let token_hash = fiber_core::tokens::hash_token(&token);
    // Subscribed before the upgrade, not inside the session: a shutdown that lands
    // between the handshake response and the session's first poll must still wait for it.
    let session = state.sessions.subscribe();
    ws.on_upgrade(move |socket| handle_agent(socket, state, agent.id, token_hash, session))
}

/// Build the offer for a leased step from the run's definition snapshot **only**.
///
/// The snapshot (`CompiledDag`, stored on the run at start) is immutable for that
/// execution: editing the pipeline while a run is in flight must not change the
/// workspace, command, artifacts or env an agent receives. Nothing here reads the
/// live pipeline row.
/// W3C `traceparent` for the current span, so the agent's execution span joins this trace.
///
/// Empty unless OpenTelemetry is on: the propagator is a no-op by default, and a span with
/// no OTel context injects nothing. Returning `None` then is correct — a bogus traceparent
/// would make the agent parent its work to a trace that does not exist.
fn current_traceparent() -> Option<String> {
    use opentelemetry::global;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let cx = tracing::Span::current().context();
    let mut carrier = std::collections::HashMap::new();
    global::get_text_map_propagator(|p| p.inject_context(&cx, &mut carrier));
    let tp = carrier.remove("traceparent").filter(|v| !v.is_empty());
    // Debug rather than info: this is how you find out why a run's spans did not join up,
    // and it is noise otherwise.
    tracing::debug!(
        traceparent = tp.as_deref().unwrap_or("<none>"),
        "offer trace context"
    );
    tp
}

/// A workspace-relative path that cannot climb out of it. Mirrors the compile-time check in
/// `fiber-core`; kept here so a snapshot written by an older or edited definition still
/// cannot send an agent outside its workspace.
fn is_contained_relative_path(p: &str) -> bool {
    let p = p.trim();
    !p.is_empty()
        && !p.starts_with('/')
        && !p.starts_with('\\')
        && !p.contains(':')
        && !p.split(['/', '\\']).any(|seg| seg == "..")
}

/// A bare program name, not a command line.
fn is_bare_program_name(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && s.len() <= 32
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        && s != "."
        && s != ".."
}

/// Why an offer could not be built, sorted by what to do about the lease. An offer sent
/// without its workspace, secrets or restore list would run the step in an empty
/// directory, fail it, and spend a retry — so none is sent; the question is only
/// whether trying again later could work.
#[derive(Debug)]
enum OfferError {
    /// Might clear: a store error, or the run row gone from under the lease. The step
    /// goes back to the queue with a backoff.
    Transient(anyhow::Error),
    /// Will not clear on its own — a project secret that cannot be decrypted. The step
    /// is failed with this reason; retrying would lease and back it out every heartbeat
    /// and never run anything behind it.
    Permanent(String),
}

impl std::fmt::Display for OfferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OfferError::Transient(e) => write!(f, "{e:#}"),
            OfferError::Permanent(reason) => f.write_str(reason),
        }
    }
}

impl From<anyhow::Error> for OfferError {
    /// The one permanent cause is typed by the store; everything else is assumed to be
    /// the database, which is the only other thing the offer path talks to.
    fn from(e: anyhow::Error) -> Self {
        match e.downcast_ref::<fiber_core::SecretDecryptError>() {
            Some(d) => OfferError::Permanent(d.to_string()),
            None => OfferError::Transient(e),
        }
    }
}

async fn offer_for_step(state: &AppState, step: StepRun) -> Result<ServerMessage, OfferError> {
    let mut env = vec![
        ("FIBER_RUN_ID".into(), step.run_id.to_string()),
        ("FIBER_STEP_ID".into(), step.step_id.clone()),
    ];
    let mut artifacts = Vec::new();
    let mut timeout_minutes = None;
    let mut working_directory = None;
    let mut shell = None;
    let mut secret_keys = Vec::new();
    let mut needs_closure = None;
    let run = state
        .store
        .get_run(step.run_id)
        .await?
        .ok_or_else(|| OfferError::Transient(anyhow::anyhow!("run missing")))?;
    let snapshot = &run.definition_snapshot;
    let workspace = workspace_from_snapshot(snapshot).map(|mut ws| {
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
            // Re-checked here rather than trusted from the snapshot: it was
            // validated when the pipeline compiled, but a snapshot is a stored
            // document and this is the last point before it reaches an agent.
            working_directory = s
                .get("working_directory")
                .and_then(|v| v.as_str())
                .filter(|d| is_contained_relative_path(d))
                .map(str::to_string);
            shell = s
                .get("shell")
                .and_then(|v| v.as_str())
                .filter(|sh| is_bare_program_name(sh))
                .map(str::to_string);
            // Absent (or null) = every project secret; a list = only those names.
            secret_allow = s
                .get("secrets")
                .filter(|v| !v.is_null())
                .map(|v| snapshot_str_list(Some(v)));
            needs_closure = Some(needs_closure_for(snapshot, &step.step_id));
        }
        // A snapshot without this step is a stored document that has gone wrong, not
        // a transient failure: requeueing would lease and unlease it every heartbeat.
        // Offered as-is, loudly, so the step fails in front of someone.
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
    } else {
        // One undecryptable row fails the whole read, and the offer with it: a step
        // that ran without its secrets would fail in a way that looks like the build's
        // fault. The store types that failure; `OfferError::from` fails the step on it.
        let secrets = state.store.list_secret_values(run.project_id).await?;
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
    let restore = restore_list(state, step.run_id, step.id, needs_closure.as_ref()).await?;
    // Both values were checked when the pipeline compiled, and the agent refuses them
    // again before use. A snapshot from a server older than that check can still hold one;
    // the agent will fail the step, and this is the line that says why.
    if let Some(img) = step.image.as_deref().map(str::trim)
        && !img.is_empty()
        && !fiber_proto::validate::image_reference_ok(img)
    {
        warn!(
            run_id = %step.run_id, step = %step.step_id, image = %img,
            "snapshot image is not a docker image reference; the agent will refuse it"
        );
    }
    if let Some(ws) = &workspace
        && !fiber_proto::validate::repo_url_ok(&ws.repo)
    {
        warn!(
            run_id = %step.run_id, step = %step.step_id,
            "snapshot workspace repo is not a fetchable URL; the agent will refuse it"
        );
    }
    // Agents always get a limit: the step's own, else the server default.
    let default_minutes = fiber_scheduler::TimeoutConfig::from_env().default_minutes as u32;
    Ok(ServerMessage::Offer {
        step_run_id: step.id,
        run_id: step.run_id,
        step_id: step.step_id.clone(),
        step_name: step.step_name,
        image: step.image,
        run: step.run_cmd,
        workspace,
        attempt: Some(step.attempt),
        env,
        artifacts,
        restore,
        timeout_minutes: Some(timeout_minutes.unwrap_or(default_minutes)),
        working_directory,
        shell,
        secret_keys,
        traceparent: current_traceparent(),
    })
}

/// Offer the agent steps until it has no free slot or nothing matches. Each offer is
/// leased first (that is what `offer_for_agent` does) and built second; one that cannot
/// be built is backed out (or failed, when the cause is permanent), nothing is sent for
/// it, and the pass moves on to the next candidate — skipping the ones it already
/// backed out, and giving up after a few failures so a store that is down costs one
/// heartbeat a bounded amount of work.
async fn fill_agent(state: &AppState, agent_id: Uuid, tx: &mpsc::UnboundedSender<ServerMessage>) {
    let mut cursor = fiber_scheduler::FillCursor::new();
    loop {
        let step = match state
            .scheduler
            .offer_for_agent(agent_id, cursor.skip())
            .await
        {
            Ok(Some(step)) => step,
            Ok(None) => return,
            Err(e) => {
                warn!(%agent_id, error = %e, "offer lookup failed");
                return;
            }
        };
        let span = tracing::info_span!(
            "fiber.offer",
            %agent_id,
            run_id = %step.run_id,
            step_id = %step.step_id,
        );
        let built = offer_for_step(state, step.clone()).instrument(span).await;
        // The attempt opens only for an offer that is about to go out; if even that
        // write fails the offer is treated like any other transient failure.
        let built = match built {
            Ok(offer) => match state.scheduler.record_offer_sent(&step).await {
                Ok(()) => Ok(offer),
                Err(e) => Err(OfferError::Transient(e)),
            },
            Err(e) => Err(e),
        };
        match built {
            Ok(offer) => {
                if tx.send(offer).is_err() {
                    // The writer is gone; the disconnect path reclaims the lease.
                    return;
                }
            }
            Err(OfferError::Permanent(reason)) => {
                if let Err(e) = state.scheduler.fail_unsent_offer(&step, &reason).await {
                    warn!(%agent_id, run_id = %step.run_id, step = %step.step_id, error = %e,
                        "could not fail the step; the reclaim loop will requeue it");
                }
                if !cursor.note_failure(step.id) {
                    return;
                }
            }
            Err(OfferError::Transient(e)) => {
                state
                    .scheduler
                    .release_offer(&step, &format!("{e:#}"))
                    .await;
                if !cursor.note_failure(step.id) {
                    return;
                }
            }
        }
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
///
/// A store error is an error: restoring nothing would hand the step an empty workspace
/// and a green build a red one, for a database blip.
async fn restore_list(
    state: &AppState,
    run_id: Uuid,
    current_step_run_id: Uuid,
    needs_closure: Option<&HashSet<String>>,
) -> anyhow::Result<Vec<ArtifactRestore>> {
    let arts = state.store.list_artifacts(run_id).await?;
    // step_run_id -> step_id, so artifacts can be attributed to the step that produced them.
    let producer: HashMap<Uuid, String> = state
        .store
        .list_step_runs(run_id)
        .await?
        .into_iter()
        .map(|s| (s.id, s.step_id))
        .collect();
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
    Ok(by_name.into_values().collect())
}

/// The step a message refers to, but only if `agent_id` is the agent it was last
/// leased to and `attempt` (echoed from the offer; `None` from older agents) is the
/// attempt the row is on. Used for log lines: output that arrives after a cancel or
/// reclaim is still this agent's output for its own attempt, and is what an operator
/// reads to learn why the step stopped — but output an agent held through a reclaim
/// and delivered after it leased the same step again belongs to the earlier attempt,
/// and is dropped rather than filed under the new one.
async fn owned_step(
    state: &AppState,
    agent_id: Uuid,
    step_run_id: Uuid,
    attempt: Option<i32>,
) -> Option<StepRun> {
    let step = state.store.get_step_run(step_run_id).await.ok().flatten()?;
    if step.agent_id != Some(agent_id) {
        tracing::debug!(
            %agent_id, %step_run_id, owner = ?step.agent_id,
            "dropping agent message for a step it does not own"
        );
        return None;
    }
    if !fiber_scheduler::attempt_is_current(attempt, step.attempt) {
        warn!(
            %agent_id, %step_run_id, row_attempt = step.attempt, reported_attempt = ?attempt,
            "dropping agent message for an earlier attempt of the step"
        );
        return None;
    }
    Some(step)
}

/// What the step says when the server could not store one of its artifacts.
///
/// The name is the part the pipeline author needs — it says which `artifacts:` entry is
/// missing. The cause is deliberately not here: this string is persisted as the step's
/// `error` and any project `reader` can fetch it, and the cause is a backend or sqlx
/// chain that can name a bucket, an endpoint or a database. That goes to the server log,
/// the same split `ApiError::Internal` makes for HTTP responses.
fn artifact_store_failure(rel: &str) -> String {
    format!("artifact {rel} could not be stored; see the server log")
}

/// Like `owned_step`, but the lease must still be live. Used for artifacts (and,
/// via the scheduler, completions): once a step was reclaimed or re-leased, the
/// re-leased attempt reports its own outputs — this attempt's are dropped.
async fn leased_step(
    state: &AppState,
    agent_id: Uuid,
    step_run_id: Uuid,
    attempt: Option<i32>,
) -> Option<StepRun> {
    let step = owned_step(state, agent_id, step_run_id, attempt).await?;
    if step.status_enum() != StepStatus::Running {
        tracing::debug!(
            %agent_id, %step_run_id, status = %step.status,
            "dropping agent message for a step whose lease ended"
        );
        return None;
    }
    Some(step)
}

/// What one agent socket remembers about one attempt of one step, for the log path.
///
/// Both halves used to cost a query per line: `owned_step` was a `SELECT` on every
/// `LogChunk`, and the line cap was seeded from `count_log_lines`. Neither answer can
/// change under a live socket — an attempt is reclaimed only when its lease expires,
/// which needs `LEASE_SECS` without a heartbeat, and this socket is closed after 45 s
/// without a frame — so the socket resolves each `(step_run_id, attempt)` once.
///
/// Dropped on `StepComplete` and when a `Cancel` goes out to this agent, and with the
/// socket. The map used to be the line counter alone and was never pruned, so a
/// long-lived agent accumulated an entry per step it had ever run.
#[derive(Debug)]
struct StepLog {
    run_id: Uuid,
    /// Lines an earlier batch of this attempt could not be stored for. Reported on the
    /// next batch that does land: the budget was spent when they were planned, so
    /// without this the gap is silent and the counter has already moved past it.
    ///
    /// A failure on a step's **last** batch is therefore never reported — the entry is
    /// dropped on `StepComplete` and no later batch comes. Reporting it from the
    /// completion path would mean an insert on the way out of a step, for a case that
    /// needs Postgres to fail on the final batch specifically; the `warn!` on the
    /// failing insert is the record of it.
    unstored: u64,
    /// Lines already stored for this attempt, including by an earlier session: the cap
    /// is per attempt, not per socket.
    stored: u64,
}

/// Shared with the socket's writer, which is where an outgoing `Cancel` is seen.
type StepLogs = std::sync::Arc<std::sync::Mutex<HashMap<(Uuid, i32), StepLog>>>;

fn forget_step(logs: &StepLogs, step_run_id: Uuid) {
    if let Ok(mut m) = logs.lock() {
        m.retain(|(s, _), _| *s != step_run_id);
    }
}

/// Lines one `LogBatch` may carry.
///
/// The agent flushes at 500, so four times that is generous for anything honest. A
/// hostile one can put a 64 MiB frame on the wire, which parses to on the order of a
/// million lines and becomes three arrays of them before a single row is written.
const MAX_BATCH_LINES: usize = 4 * 500;

/// Which lines of a batch to store, and what the attempt's count becomes.
///
/// A runaway step could otherwise write until the disk filled. Past the cap the lines are
/// dropped, with one line saying so — silence would look like the step stopped producing
/// output. Every line is also cut to `MAX_LOG_LINE_BYTES`: the agent caps its own, but a
/// hostile or old one does not, and a 60 MB line is a 60 MB row and a 60 MB broadcast.
fn plan_log_batch(
    mut lines: Vec<LogLineWire>,
    stored: u64,
    cap: u64,
    unstored: u64,
) -> (Vec<LogLineWire>, u64, bool) {
    let mut seen = stored;
    let mut told = unstored == 0;
    let mut over = 0usize;
    if lines.len() > MAX_BATCH_LINES {
        over = lines.len() - MAX_BATCH_LINES;
        lines.truncate(MAX_BATCH_LINES);
    }
    let mut out = Vec::with_capacity(lines.len() + 2);
    // Ahead of the batch, and inside the cap like any other line: a gap the server made
    // is worth a row, and saying nothing is what made the last one invisible.
    if unstored > 0 {
        let seq = lines.first().map(|l| l.seq).unwrap_or(0);
        seen += 1;
        if seen <= cap {
            out.push(LogLineWire {
                stream: "system".into(),
                data: format!("{unstored} log lines could not be stored and were lost"),
                seq,
            });
            told = true;
        }
    }
    if over > 0 {
        let seq = lines.first().map(|l| l.seq).unwrap_or(0);
        seen += 1;
        if seen <= cap {
            out.push(LogLineWire {
                stream: "system".into(),
                data: format!(
                    "{over} log lines dropped: the agent sent more than {MAX_BATCH_LINES} in one batch"
                ),
                seq,
            });
        }
    }
    for line in lines {
        seen += 1;
        if seen > cap {
            continue;
        }
        if seen == cap {
            out.push(LogLineWire {
                stream: "system".into(),
                data: format!(
                    "log truncated at {cap} lines for this attempt \
                     (FIBER_STEP_LOG_MAX_LINES); the step is still running"
                ),
                seq: line.seq,
            });
            continue;
        }
        out.push(LogLineWire {
            stream: line.stream,
            data: truncate_log_line(&line.data).into_owned(),
            seq: line.seq,
        });
    }
    (out, seen, told)
}

/// Store one batch of a step's output and publish it once.
///
/// `LogChunk` from an older agent comes through here as a batch of one, so there is a
/// single ingest path whatever the agent speaks.
async fn ingest_log_batch(
    state: &AppState,
    logs: &StepLogs,
    agent_id: Uuid,
    step_run_id: Uuid,
    attempt: Option<i32>,
    lines: Vec<LogLineWire>,
    cap: u64,
) {
    if lines.is_empty() {
        return;
    }
    // A cached entry is only ever reached with the attempt named, so a message from an
    // agent older than the `attempt` field takes the `owned_step` path every time — the
    // row is the only thing that can say which attempt it meant.
    let cached = attempt.and_then(|a| {
        logs.lock().ok().and_then(|m| {
            m.get(&(step_run_id, a))
                .map(|e| (a, e.run_id, e.stored, e.unstored))
        })
    });
    let (row_attempt, run_id, stored, unstored) = match cached {
        Some(hit) => hit,
        None => {
            let Some(step) = owned_step(state, agent_id, step_run_id, attempt).await else {
                return;
            };
            let key = (step_run_id, step.attempt);
            let known = logs
                .lock()
                .ok()
                .and_then(|m| m.get(&key).map(|e| (e.stored, e.unstored)));
            let (stored, unstored) = match known {
                Some(hit) => hit,
                None => (
                    state
                        .store
                        .count_log_lines(step_run_id, step.attempt)
                        .await
                        .map(|n| u64::try_from(n).unwrap_or(0))
                        .unwrap_or(0),
                    0,
                ),
            };
            if let Ok(mut m) = logs.lock() {
                m.entry(key).or_insert(StepLog {
                    run_id: step.run_id,
                    stored,
                    unstored,
                });
            }
            (step.attempt, step.run_id, stored, unstored)
        }
    };

    let lost = lines.len() as u64;
    let (to_store, seen, told) = plan_log_batch(lines, stored, cap, unstored);
    // Spent whether or not the insert lands: a failing insert retried by the agent must
    // not get a fresh budget.
    if let Ok(mut m) = logs.lock()
        && let Some(e) = m.get_mut(&(step_run_id, row_attempt))
    {
        e.stored = seen;
        // Cleared only if the notice for it is in the batch just planned: past the
        // per-attempt line cap it is not, and clearing then would lose the count
        // without ever having said it. If the insert below fails, the whole batch —
        // this notice included — is charged back.
        if told {
            e.unstored = 0;
        }
    }
    if to_store.is_empty() {
        return;
    }
    let rows = match state
        .store
        .append_logs(run_id, step_run_id, row_attempt, &to_store)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            // The budget is already spent and the lines are gone. Carry the count so
            // the next batch that does land reports the hole rather than leaving it
            // silent.
            warn!(error = %e, %step_run_id, lines = lost, "storing step output failed");
            if let Ok(mut m) = logs.lock()
                && let Some(entry) = m.get_mut(&(step_run_id, row_attempt))
            {
                // Everything this batch was carrying, which during a database outage
                // is this batch *and* whatever earlier batches it was reporting for:
                // adding only `lost` pinned the figure at one batch however much was
                // actually lost.
                entry.unstored = entry.unstored.saturating_add(lost).saturating_add(unstored);
            }
            return;
        }
    };
    // One event for the batch, not one per line: the run-event bus is a 1024-slot
    // broadcast shared by every viewer of every run, and a chatty step used to spend a
    // slot — and a Redis round trip — per line.
    let ev = RunEvent::LogBatch {
        run_id,
        step_run_id,
        attempt: Some(row_attempt),
        lines: rows
            .into_iter()
            .map(|r| LogEventLine {
                id: r.id,
                stream: r.stream,
                data: r.data,
                seq: r.seq.max(0) as u64,
                at: r.created_at,
            })
            .collect(),
    };
    if let Ok(payload) = serde_json::to_string(&ev) {
        state.scheduler.publish_event(&payload).await;
    }
}

/// `agent_id` is bound once from the authenticated token and never rebound from a
/// client-supplied field: an agent may only ever act as itself.
async fn handle_agent(
    socket: WebSocket,
    state: AppState,
    agent_id: Uuid,
    token_hash: String,
    session: tokio::sync::watch::Receiver<()>,
) {
    // Held to the end of the function: shutdown waits for it to drop.
    let _session = session;
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

    let welcome = ServerMessage::Welcome {
        agent_id,
        // The agent keeps a step running for this long after losing its socket; past
        // it the reclaim loop has requeued the step and the agent stops its copy.
        lease_secs: u64::try_from(fiber_scheduler::LEASE_SECS).ok(),
        // What this server can parse. The agent falls back to one `LogChunk` per line
        // when it is absent, so an older replica in a rolling deploy still gets output.
        protocol_version: Some(fiber_proto::PROTOCOL_VERSION),
    };
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

    // Lines already stored per (step, attempt), and the step's run, so the cap and the
    // ownership check cost a counter rather than two queries on every line. Shared with
    // the writer because a `Cancel` reaches the agent through it, and a cancelled step
    // may be requeued and leased back to this same agent as a new attempt.
    let step_logs: StepLogs = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));

    // The writer owns the sink: scheduler messages, the liveness pings, and — last —
    // the Close frame this side sends when it ends the session, so the agent learns
    // why instead of seeing a reset.
    let (close_tx, mut close_rx) = oneshot::channel::<CloseFrame>();
    let writer_logs = std::sync::Arc::clone(&step_logs);
    let mut writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(AGENT_PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await; // the first tick is immediate; the agent just said hello
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    if let ServerMessage::Cancel { step_run_id } = &msg {
                        forget_step(&writer_logs, *step_run_id);
                    }
                    let Ok(text) = serde_json::to_string(&msg) else {
                        continue;
                    };
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Bytes::new())).await.is_err() {
                        break;
                    }
                }
                frame = &mut close_rx => {
                    if let Ok(frame) = frame {
                        let _ = sink.send(Message::Close(Some(frame))).await;
                    }
                    break;
                }
            }
        }
    });

    // Log a spoofed agent_id once per session, not once per log line.
    let mut spoof_logged = false;
    // Set when this side ends the session because the token no longer authorises the
    // agent: its steps are requeued at once, whatever it declared in Hello.
    let mut revoked = false;
    let log_cap = step_log_cap();
    // What the agent declared in Hello (`None` until it does): decides whether a close
    // ends its attempts or just this session. `goodbye` is set when the agent says it
    // is exiting on purpose. Not persisted — `agents` has no column for the revision
    // yet; the Hello log line carries it.
    let mut agent_protocol: Option<u32> = None;
    let mut goodbye = false;
    let mut shutdown = state.shutdown.clone();
    let mut liveness = tokio::time::Instant::now() + AGENT_LIVENESS_TIMEOUT;
    // Why this side ended the session, if it did. `None` means the agent went first.
    let mut close: Option<CloseFrame> = None;

    loop {
        let msg = tokio::select! {
            msg = stream.next() => msg,
            _ = tokio::time::sleep_until(liveness) => {
                warn!(
                    %agent_id,
                    secs = AGENT_LIVENESS_TIMEOUT.as_secs(),
                    "agent sent nothing (not even a pong); closing its socket"
                );
                close = Some(close_frame(close_code::POLICY, "liveness timeout"));
                // Deliberately KeepLeases (via the tail): a socket the agent stopped
                // answering on may be black-holed while the agent itself is fine and
                // reconnecting; if it is gone, the leases expire on their own.
                break;
            }
            _ = shutting_down(&mut shutdown) => {
                info!(%agent_id, "shutdown: closing agent session");
                close = Some(close_frame(close_code::RESTART, "server shutting down"));
                break;
            }
        };
        let Some(Ok(msg)) = msg else {
            break;
        };
        liveness = tokio::time::Instant::now() + AGENT_LIVENESS_TIMEOUT;
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
                protocol_version,
            } => {
                // A force-disconnected session (token rotated, agent deleted) must not be
                // able to re-register itself by replaying Hello.
                if !state.scheduler.has_connection(agent_id).await {
                    let _ = tx.send(ServerMessage::Error {
                        message: "session ended — reconnect".into(),
                    });
                    break;
                }
                agent_protocol = Some(protocol_version);
                info!(%agent_id, %name, ?labels, protocol_version, "agent hello");
                // Reload from DB so pool scope is authoritative (not client-supplied).
                // A missing row ends the session rather than defaulting: `.flatten()`
                // into `and_then(project_id)` made "agent deleted" indistinguishable from
                // "global agent", so a deleted project-scoped agent would re-register
                // itself into the *global* pool — and the global pool is offered every
                // project's steps, with their secrets.
                let Ok(Some(agent_row)) = state.store.get_agent(agent_id).await else {
                    let _ = tx.send(ServerMessage::Error {
                        message: "agent no longer exists".into(),
                    });
                    break;
                };
                let project_id = agent_row.project_id;
                state
                    .scheduler
                    .register_agent(agent_id, labels, concurrency, project_id)
                    .await;
                // Re-bind connection under confirmed agent_id (same as token)
                state
                    .scheduler
                    .register_connection(agent_id, tx.clone())
                    .await;
                let _ = state.store.set_agent_online(agent_id, true).await;
                let _ = state.store.touch_agent(agent_id).await;
                fill_agent(&state, agent_id, &tx).await;
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
                    revoked = true;
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
                fill_agent(&state, agent_id, &tx).await;
            }
            AgentMessage::Claim {
                agent_id: claimed,
                step_run_id: _,
            } => {
                warn_if_spoofed(agent_id, claimed, &mut spoof_logged);
            }
            // An agent older than `LogBatch` sends one of these per line. Same path,
            // batch of one: the ingest rules must not depend on the agent's revision.
            AgentMessage::LogChunk {
                agent_id: claimed,
                step_run_id,
                stream: stream_name,
                data,
                seq,
                attempt,
            } => {
                warn_if_spoofed(agent_id, claimed, &mut spoof_logged);
                ingest_log_batch(
                    &state,
                    &step_logs,
                    agent_id,
                    step_run_id,
                    attempt,
                    vec![LogLineWire {
                        stream: stream_name,
                        data,
                        seq,
                    }],
                    log_cap,
                )
                .await;
            }
            AgentMessage::LogBatch {
                agent_id: claimed,
                step_run_id,
                attempt,
                lines,
            } => {
                warn_if_spoofed(agent_id, claimed, &mut spoof_logged);
                ingest_log_batch(
                    &state,
                    &step_logs,
                    agent_id,
                    step_run_id,
                    attempt,
                    lines,
                    log_cap,
                )
                .await;
            }
            AgentMessage::Artifact {
                agent_id: claimed,
                step_run_id,
                name,
                path: rel_path,
                size,
                content_base64,
                attempt,
            } => {
                warn_if_spoofed(agent_id, claimed, &mut spoof_logged);
                // Prefer HTTP upload; keep WS base64 as a small-file fallback.
                let Some(b64) = content_base64 else {
                    continue;
                };
                if let Some(step) = leased_step(&state, agent_id, step_run_id, attempt).await {
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
                        // Storing it is part of the step, not a side effect of it: a
                        // step whose artifacts were dropped is not a step that passed,
                        // and its dependents would restore nothing and fail somewhere
                        // else. This used to `warn!` and carry on, so a root-owned
                        // artifact directory (or a full disk, or a dead bucket) lost
                        // every artifact into the API's own log while every build stayed
                        // green. The HTTP upload path already fails loudly.
                        let stored = match state.artifacts.put(&key, &bytes).await {
                            Ok(stored_path) => state
                                .store
                                .create_artifact(
                                    step.run_id,
                                    step_run_id,
                                    &rel,
                                    &stored_path,
                                    bytes.len() as i64,
                                )
                                .await
                                .map(|_| ()),
                            Err(e) => Err(e),
                        };
                        if let Err(e) = stored {
                            let reason = artifact_store_failure(&rel);
                            // `{e:#}` for the whole chain: the outer context alone is
                            // "s3 put_object", which says nothing an operator can act on.
                            error!(
                                %step_run_id, %agent_id, artifact = %rel, error = format!("{e:#}"),
                                "could not store an artifact; failing the step"
                            );
                            // The step is over for this socket either way.
                            forget_step(&step_logs, step_run_id);
                            match state
                                .scheduler
                                .on_step_complete(
                                    agent_id,
                                    step_run_id,
                                    attempt,
                                    StepStatus::Failed,
                                    None,
                                    Some(reason),
                                )
                                .await
                            {
                                Ok(_) => {
                                    // Tell the agent to stop, as the timeout backstop
                                    // does. Without it the step keeps running while the
                                    // server offers its next attempt — onto the same
                                    // per-step workspace path, whose cleanup when the
                                    // first process finally exits deletes the second
                                    // attempt's checkout underneath it.
                                    state
                                        .scheduler
                                        .cancel_step_on_agent(agent_id, step_run_id)
                                        .await;
                                    fill_agent(&state, agent_id, &tx).await;
                                }
                                Err(e) => {
                                    warn!(error = %e, %step_run_id, "failing the step for an unstored artifact failed")
                                }
                            }
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
                attempt,
            } => {
                warn_if_spoofed(agent_id, claimed, &mut spoof_logged);
                // The step is over for this agent: nothing more of its output can be
                // admitted under the cached attempt, and an agent that runs thousands of
                // steps would otherwise keep an entry for every one of them.
                forget_step(&step_logs, step_run_id);
                // on_step_complete additionally rejects completions for steps not leased
                // to agent_id, or reporting on an attempt the row has moved past.
                match state
                    .scheduler
                    .on_step_complete(agent_id, step_run_id, attempt, status, exit_code, error)
                    .await
                {
                    Ok(_) => fill_agent(&state, agent_id, &tx).await,
                    Err(e) => warn!(error = %e, "step complete failed"),
                }
            }
            AgentMessage::Goodbye { agent_id: claimed } => {
                warn_if_spoofed(agent_id, claimed, &mut spoof_logged);
                // The agent has stopped its steps and is exiting: end the session here
                // and requeue in the tail, rather than wait LEASE_SECS for leases it
                // will not renew. Nothing after this is offered to it.
                info!(%agent_id, "agent goodbye; requeueing its steps");
                goodbye = true;
                break;
            }
        }
    }

    // The online flag belongs to the session that set it, in the Hello arm. A socket
    // that closed before Hello never set it — it is a reconnect racing its own
    // predecessor, or a second process on the same token — and clearing it from here
    // would report the *live* session's agent as gone. The registry entry and the
    // connection are dropped either way, by `on_agent_disconnect` below: this socket
    // took the connection slot when it opened, so it owns the cleanup.
    if agent_protocol.is_some() {
        let _ = state.store.set_agent_online(agent_id, false).await;
    }
    // A close is no longer the end of the agent's attempts: from protocol revision 1 it
    // keeps its steps running and renews their leases when it is back, so the rows are
    // left as they are and the reclaim loop requeues whatever expires. An older agent,
    // or one that said Goodbye, has stopped its steps, and they are requeued now. A
    // session that never said Hello learned nothing about the agent and touches nothing:
    // the agent's other session may be holding the leases.
    // A revoked token is decided here, not by the agent: with Redis down the
    // `force_disconnect_agent` fan-out never arrives, and this break is the only path.
    let policy = if revoked {
        fiber_scheduler::DisconnectPolicy::RequeueNow {
            reason: "agent token revoked",
        }
    } else {
        fiber_scheduler::disconnect_policy(agent_protocol, goodbye)
    };
    if let Err(e) = state.scheduler.on_agent_disconnect(agent_id, policy).await {
        warn!(error = %e, %agent_id, "agent disconnect cleanup failed");
    }
    if let Some(frame) = close {
        let _ = close_tx.send(frame);
        if tokio::time::timeout(CLOSE_FLUSH_TIMEOUT, &mut writer)
            .await
            .is_err()
        {
            writer.abort();
        }
    } else {
        writer.abort();
    }
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

/// Lines stored per step attempt before the rest are dropped.
///
/// `0` disables the cap, for whoever would rather risk the disk than lose output.
fn step_log_cap() -> u64 {
    std::env::var("FIBER_STEP_LOG_MAX_LINES")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(|n: u64| if n == 0 { u64::MAX } else { n })
        .unwrap_or(50_000)
}

/// The session token a browser sent as a WebSocket subprotocol.
///
/// A browser cannot set headers on a WebSocket, so `Sec-WebSocket-Protocol` is the only
/// place a token can travel that is not the URL. Query strings end up in proxy and server
/// access logs; this does not.
fn token_from_subprotocol(headers: &HeaderMap) -> Option<String> {
    headers
        .get("sec-websocket-protocol")?
        .to_str()
        .ok()?
        .split(',')
        .map(str::trim)
        .find_map(|p| p.strip_prefix("fiber.token.").map(str::to_string))
}

pub async fn run_events_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Path(run_id): Path<Uuid>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Subprotocol only. A session token is the whole user's authority for two weeks, and
    // a query string is written to every access log between the browser and this process.
    // The agent socket still takes `?token=` for older agents; nothing in-tree needs it here.
    let Some(token) = token_from_subprotocol(&headers) else {
        return (axum::http::StatusCode::UNAUTHORIZED, "missing token").into_response();
    };
    let user = match state.store.user_by_session_token(&token).await {
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
    // The client offered a subprotocol, so the handshake has to name it back. Without
    // this the browser closes the socket immediately.
    let session = state.sessions.subscribe();
    ws.protocols([format!("fiber.token.{token}")])
        .on_upgrade(move |socket| handle_run_events(socket, state, run_id, session))
}

/// One text frame to a run-stream subscriber, bounded by `RUN_EVENTS_SEND_TIMEOUT`.
/// `false` means the socket is gone or not reading and the session should end.
async fn send_bounded(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    text: String,
) -> bool {
    matches!(
        tokio::time::timeout(
            RUN_EVENTS_SEND_TIMEOUT,
            sink.send(Message::Text(text.into()))
        )
        .await,
        Ok(Ok(()))
    )
}

/// A browser that cannot take a frame in this long is not reading; a stalled tab must not
/// hold a session open through a deploy's drain.
const RUN_EVENTS_SEND_TIMEOUT: Duration = Duration::from_secs(10);

async fn handle_run_events(
    socket: WebSocket,
    state: AppState,
    run_id: Uuid,
    session: tokio::sync::watch::Receiver<()>,
) {
    // Held to the end of the function: shutdown waits for it to drop.
    let _session = session;
    let (mut sink, mut stream) = socket.split();
    let mut sub = state.scheduler.subscribe();

    if let Ok(Some(run)) = state.store.get_run(run_id).await {
        let ev = RunEvent::RunUpdated {
            run_id,
            status: run.status_enum(),
        };
        if let Ok(text) = serde_json::to_string(&ev)
            && !send_bounded(&mut sink, text).await
        {
            return;
        }
        if let Ok(steps) = state.store.list_step_runs(run_id).await {
            for s in steps {
                let ev = RunEvent::StepUpdated {
                    run_id,
                    step_run_id: s.id,
                    step_id: s.step_id.clone(),
                    status: s.status_enum(),
                };
                if let Ok(text) = serde_json::to_string(&ev)
                    && !send_bounded(&mut sink, text).await
                {
                    return;
                }
            }
        }
    }

    let mut shutdown = state.shutdown.clone();
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
            _ = shutting_down(&mut shutdown) => {
                // The UI retries 2 s after any close; the frame matters through a proxy
                // that would otherwise hold the browser's socket half-open.
                let frame = close_frame(close_code::RESTART, "server shutting down");
                let _ = tokio::time::timeout(
                    CLOSE_FLUSH_TIMEOUT,
                    sink.send(Message::Close(Some(frame))),
                )
                .await;
                break;
            }
            ev = sub.recv() => {
                match ev {
                    Ok(payload) => {
                        if let Ok(parsed) = serde_json::from_str::<RunEvent>(&payload) {
                            let matches = match &parsed {
                                RunEvent::RunUpdated { run_id: rid, .. } => *rid == run_id,
                                RunEvent::StepUpdated { run_id: rid, .. } => *rid == run_id,
                                RunEvent::Log { run_id: rid, .. } => *rid == run_id,
                                RunEvent::LogBatch { run_id: rid, .. } => *rid == run_id,
                            };
                            if matches && !send_bounded(&mut sink, payload).await {
                                break;
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
    fn an_unstored_artifact_names_itself_without_leaking_its_cause() {
        // The pipeline author needs to know which artifact is missing; the cause is the
        // operator's and goes to the server log.
        let reason = artifact_store_failure("dist/app.tgz");
        assert!(reason.contains("dist/app.tgz"), "{reason}");
        // The cause belongs in the server log: a reader of this project must not be
        // handed a bucket name, an endpoint or a database error.
        for leaked in ["Permission denied", "os error", "s3", "bucket", "database"] {
            assert!(
                !reason.to_ascii_lowercase().contains(leaked),
                "{reason} leaks {leaked}"
            );
        }
    }

    #[test]
    fn a_secret_that_cannot_be_decrypted_fails_the_step_rather_than_the_offer() {
        // The one cause that never clears on its own. Backing the step out would lease
        // and unlease it every heartbeat, at the head of the queue, forever.
        let e = anyhow::Error::from(fiber_core::SecretDecryptError {
            name: "API_KEY".into(),
            source: anyhow::anyhow!("decrypt secret failed (wrong key?)"),
        });
        match OfferError::from(e) {
            OfferError::Permanent(reason) => {
                assert!(reason.contains("API_KEY"), "{reason}");
                assert!(reason.contains("FIBER_SECRETS_KEY"), "{reason}");
            }
            OfferError::Transient(e) => panic!("must be permanent, got transient: {e}"),
        }
    }

    #[test]
    fn any_other_failure_is_transient_and_backs_the_step_out() {
        // A wrapped decrypt error still classifies; a bare store error does not.
        let wrapped = anyhow::Error::from(fiber_core::SecretDecryptError {
            name: "TOKEN".into(),
            source: anyhow::anyhow!("boom"),
        })
        .context("reading project secrets");
        assert!(matches!(
            OfferError::from(wrapped),
            OfferError::Permanent(_)
        ));
        let db = anyhow::anyhow!("connection reset by peer");
        assert!(matches!(OfferError::from(db), OfferError::Transient(_)));
    }

    fn wire(seq: u64, data: &str) -> LogLineWire {
        LogLineWire {
            stream: "stdout".into(),
            data: data.into(),
            seq,
        }
    }

    #[test]
    fn a_batch_keeps_its_lines_in_order_with_their_own_seq() {
        let lines = (0..4).map(|i| wire(10 + i, "x")).collect();
        let (out, seen, _) = plan_log_batch(lines, 0, 1_000, 0);
        assert_eq!(
            out.iter().map(|l| l.seq).collect::<Vec<_>>(),
            vec![10, 11, 12, 13],
            "the agent numbers the lines; the server stores what it was given"
        );
        assert_eq!(seen, 4);
        assert!(out.iter().all(|l| l.stream == "stdout"));
    }

    #[test]
    fn a_line_past_the_wire_cap_is_cut_before_it_is_stored() {
        // A hostile or old agent does not cap its own lines, and a 60 MB line is a 60 MB
        // row and a 60 MB broadcast payload.
        let huge = "a".repeat(fiber_proto::limits::MAX_LOG_LINE_BYTES * 2);
        let (out, _, _) = plan_log_batch(vec![wire(0, &huge)], 0, 1_000, 0);
        assert_eq!(out[0].data.len(), fiber_proto::limits::MAX_LOG_LINE_BYTES);
        assert!(
            out[0]
                .data
                .ends_with(fiber_proto::limits::TRUNCATION_MARKER)
        );
    }

    #[test]
    fn the_line_cap_is_counted_across_the_batch_and_says_so_once() {
        // The cap is per attempt, and a batch may cross it in the middle. The line *at*
        // the cap becomes the notice; everything after it is dropped silently.
        let lines = (0..10).map(|i| wire(i, "out")).collect();
        let (out, seen, _) = plan_log_batch(lines, 0, 5, 0);
        assert_eq!(out.len(), 5);
        assert_eq!(out[4].stream, "system");
        assert!(
            out[4].data.contains("FIBER_STEP_LOG_MAX_LINES"),
            "{:?}",
            out[4]
        );
        assert!(out[..4].iter().all(|l| l.data == "out"));
        // The counter keeps climbing past the cap so a later batch is not readmitted.
        assert_eq!(seen, 10);
        let (later, _, _) = plan_log_batch(vec![wire(99, "more")], seen, 5, 0);
        assert!(later.is_empty());
    }

    #[test]
    fn a_batch_that_starts_at_the_cap_stores_nothing() {
        let (out, seen, _) = plan_log_batch(vec![wire(0, "x"), wire(1, "y")], 50_000, 50_000, 0);
        assert!(out.is_empty());
        assert_eq!(seen, 50_002);
    }

    #[test]
    fn an_oversized_batch_is_cut_and_the_cut_is_reported() {
        // A 64 MiB frame parses to on the order of a million lines, and every one of
        // them is cloned into an array before a row is written.
        let lines = (0..MAX_BATCH_LINES as u64 + 50)
            .map(|i| wire(i, "x"))
            .collect();
        let (out, _, _) = plan_log_batch(lines, 0, 1_000_000, 0);
        assert_eq!(
            out.len(),
            MAX_BATCH_LINES + 1,
            "the kept lines plus one notice"
        );
        assert_eq!(out[0].stream, "system");
        assert!(out[0].data.contains("50 log lines dropped"), "{:?}", out[0]);
    }

    #[test]
    fn lines_a_failed_insert_lost_are_reported_on_the_next_batch() {
        // The budget was spent when they were planned, so without this the hole is
        // silent — and the attempt's counter has already moved past it.
        let (out, seen, told) = plan_log_batch(vec![wire(9, "next")], 4, 1_000, 500);
        assert!(
            told,
            "the notice is in this batch, so the count may be cleared"
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].stream, "system");
        assert!(
            out[0].data.contains("500 log lines could not be stored"),
            "{:?}",
            out[0]
        );
        assert_eq!(out[0].seq, 9, "it sorts where the gap is");
        assert_eq!(out[1].data, "next");
        assert_eq!(
            seen, 6,
            "the notice costs a line of the cap, like any other"
        );
    }

    #[test]
    fn a_notice_the_cap_swallowed_does_not_clear_the_count() {
        // Past the per-attempt cap the notice is not stored. Clearing the count then
        // would lose it having never said it.
        let (out, _, told) = plan_log_batch(vec![wire(0, "x")], 50_000, 50_000, 900);
        assert!(out.is_empty());
        assert!(!told, "nothing was said, so nothing may be forgotten");
    }

    #[test]
    fn the_step_log_cache_is_keyed_by_attempt_and_pruned_by_step() {
        // The count and the ownership answer are per attempt: a retry is a new attempt
        // with its own budget, and must never read the previous attempt's counter.
        let logs: StepLogs = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let step = Uuid::new_v4();
        let other = Uuid::new_v4();
        let run = Uuid::new_v4();
        {
            let mut m = logs.lock().unwrap();
            m.insert(
                (step, 1),
                StepLog {
                    run_id: run,
                    stored: 7,
                    unstored: 0,
                },
            );
            m.insert(
                (step, 2),
                StepLog {
                    run_id: run,
                    stored: 0,
                    unstored: 0,
                },
            );
            m.insert(
                (other, 1),
                StepLog {
                    run_id: run,
                    stored: 3,
                    unstored: 0,
                },
            );
        }
        assert_eq!(logs.lock().unwrap()[&(step, 2)].stored, 0);
        // StepComplete (and an outgoing Cancel) drop every attempt of that step, and
        // nothing else: the map used to grow for the life of the socket.
        forget_step(&logs, step);
        let m = logs.lock().unwrap();
        assert!(m.keys().all(|(s, _)| *s == other));
        assert_eq!(m.len(), 1);
    }

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
