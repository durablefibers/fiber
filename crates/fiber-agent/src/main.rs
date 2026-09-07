use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use fiber_proto::{AgentMessage, ArtifactRestore, ServerMessage, StepStatus, WorkspaceOffer};
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;
use uuid::Uuid;

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Parser, Debug)]
#[command(name = "fiber-agent", version)]
struct Args {
    #[arg(long, env = "FIBER_API_URL", default_value = "ws://127.0.0.1:18080")]
    api_url: String,

    #[arg(long, env = "FIBER_AGENT_TOKEN")]
    token: String,

    #[arg(long, env = "FIBER_AGENT_NAME", default_value = "local")]
    name: String,

    #[arg(
        long,
        env = "FIBER_AGENT_LABELS",
        default_value = "os=linux,docker=true"
    )]
    labels: String,

    #[arg(long, env = "FIBER_AGENT_CONCURRENCY", default_value_t = 1)]
    concurrency: u32,

    #[arg(long, env = "FIBER_AGENT_USE_DOCKER", default_value_t = true)]
    use_docker: bool,

    #[arg(
        long,
        env = "FIBER_AGENT_WORKSPACE_DIR",
        default_value = "./data/workspaces"
    )]
    workspace_dir: PathBuf,

    /// Extra environment variables to pass from the agent's own environment into steps,
    /// comma-separated (e.g. `SSH_AUTH_SOCK,CARGO_HOME`). Everything else is cleared.
    #[arg(long, env = "FIBER_AGENT_ENV_PASSTHROUGH", default_value = "")]
    env_passthrough: String,

    /// Delete per-run workspaces older than this many hours. 0 disables the sweep.
    #[arg(long, env = "FIBER_AGENT_WORKSPACE_TTL_HOURS", default_value_t = 24)]
    workspace_ttl_hours: u64,

    /// `--user` for step containers (e.g. `1000:1000`). Empty = the image default.
    #[arg(long, env = "FIBER_AGENT_DOCKER_USER", default_value = "")]
    docker_user: String,

    /// `--network` for step containers. `none` isolates them from the network entirely.
    #[arg(long, env = "FIBER_AGENT_DOCKER_NETWORK", default_value = "bridge")]
    docker_network: String,

    /// `--memory` for step containers (e.g. `2g`). Empty = unlimited, so an existing
    /// build is not silently OOM-killed after an upgrade.
    #[arg(long, env = "FIBER_AGENT_DOCKER_MEMORY", default_value = "")]
    docker_memory: String,

    /// `--cpus` for step containers (e.g. `2`). Empty = unlimited.
    #[arg(long, env = "FIBER_AGENT_DOCKER_CPUS", default_value = "")]
    docker_cpus: String,

    /// `--pids-limit` for step containers. 0 = unlimited.
    #[arg(long, env = "FIBER_AGENT_DOCKER_PIDS_LIMIT", default_value_t = 512)]
    docker_pids_limit: i64,
}

/// Environment a step inherits from the agent process. Everything else is cleared, so
/// repo-supplied shell cannot read `FIBER_AGENT_TOKEN` (which would let it lease steps
/// and read other projects' secrets).
const ENV_ALLOWLIST: &[&str] = &[
    // Shell basics.
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "TZ",
    "TERM",
    "TMPDIR",
    // Egress on networks that require a proxy or an internal CA. Without these a
    // corporate agent cannot reach anything, which is a worse failure than the
    // (small) chance of a credential embedded in a proxy URL.
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
];

/// Variables the `docker` client itself needs to find and talk to a daemon.
const DOCKER_CLIENT_ENV: &[&str] = &[
    "DOCKER_HOST",
    "DOCKER_CONFIG",
    "DOCKER_CERT_PATH",
    "DOCKER_TLS_VERIFY",
    "DOCKER_CONTEXT",
    "XDG_RUNTIME_DIR",
];

/// A name usable as a shell/`--env-file` variable. Anything else is refused: docker
/// reads a line *without* `=` as "take this variable from my own environment", so a key
/// carrying a newline could make the docker client hand a step its own environment —
/// including the agent token.
fn is_valid_env_key(k: &str) -> bool {
    !k.is_empty()
        && k.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Execution policy for one step, resolved once from the agent's flags.
#[derive(Clone)]
struct ExecConfig {
    use_docker: bool,
    env_passthrough: Vec<String>,
    docker_user: String,
    docker_network: String,
    docker_memory: String,
    docker_cpus: String,
    docker_pids_limit: i64,
}

impl ExecConfig {
    fn from_args(args: &Args) -> Self {
        Self {
            use_docker: args.use_docker,
            env_passthrough: args
                .env_passthrough
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            docker_user: args.docker_user.clone(),
            docker_network: args.docker_network.clone(),
            docker_memory: args.docker_memory.clone(),
            docker_cpus: args.docker_cpus.clone(),
            docker_pids_limit: args.docker_pids_limit,
        }
    }
}

/// Per-run workspace refcount: the last step of a run to finish on this agent deletes
/// the run's tree. Without it every run leaks a checkout for the agent's lifetime.
#[derive(Default)]
struct Workspaces {
    live: Mutex<HashMap<Uuid, usize>>,
    /// One lock per run, held while its reference clone is created: two steps of the
    /// same run starting together would otherwise each delete the other's in-flight clone.
    prep: Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>,
}

impl Workspaces {
    fn run_lock(&self, run_id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        let mut g = self.prep.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(g.entry(run_id).or_default())
    }

    fn enter(&self, run_id: Uuid) {
        if let Ok(mut g) = self.live.lock() {
            *g.entry(run_id).or_insert(0) += 1;
        }
    }

    /// True when this was the run's last step here, so its tree can go.
    fn leave(&self, run_id: Uuid) -> bool {
        let Ok(mut g) = self.live.lock() else {
            return false;
        };
        match g.get_mut(&run_id) {
            Some(n) if *n > 1 => {
                *n -= 1;
                false
            }
            Some(_) => {
                g.remove(&run_id);
                if let Ok(mut p) = self.prep.lock() {
                    p.remove(&run_id);
                }
                true
            }
            None => false,
        }
    }
}

/// Replaces every occurrence of a secret value in log output with `***`.
#[derive(Clone, Default)]
struct Redactor {
    values: Vec<String>,
}

impl Redactor {
    /// Values shorter than this are skipped: masking a two-character secret would blank
    /// out unrelated output without protecting much.
    const MIN_LEN: usize = 8;

    fn new(env: &[(String, String)], secret_keys: &[String]) -> Self {
        let mut values: Vec<String> = Vec::new();
        for (k, v) in env.iter().filter(|(k, _)| secret_keys.contains(k)) {
            let _ = k;
            if v.len() >= Self::MIN_LEN {
                values.push(v.clone());
            }
            // Logs arrive a line at a time, so a multi-line secret (a PEM key, a service
            // account JSON) would never match as a whole. Mask its lines individually.
            if v.contains('\n') {
                values.extend(
                    v.lines()
                        .map(str::trim_end)
                        .filter(|l| l.len() >= Self::MIN_LEN)
                        .map(str::to_string),
                );
            }
        }
        // Longest first, so a secret containing another is masked whole.
        values.sort_by_key(|v| std::cmp::Reverse(v.len()));
        values.dedup();
        Self { values }
    }

    fn apply(&self, line: &str) -> String {
        if self.values.is_empty() {
            return line.to_string();
        }
        let mut out = line.to_string();
        for v in &self.values {
            if out.contains(v.as_str()) {
                out = out.replace(v.as_str(), "***");
            }
        }
        out
    }
}

fn http_base(api_url: &str) -> String {
    let u = api_url.trim_end_matches('/');
    if let Some(rest) = u.strip_prefix("ws://") {
        format!("http://{rest}")
    } else if let Some(rest) = u.strip_prefix("wss://") {
        format!("https://{rest}")
    } else {
        u.to_string()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("fiber_agent=info".parse()?))
        .init();

    let args = Args::parse();
    if args.token.trim().is_empty() {
        error!("no agent token: set FIBER_AGENT_TOKEN (create one with `fiber agents create …`)");
        std::process::exit(2);
    }
    std::fs::create_dir_all(&args.workspace_dir)?;
    // Anything left from a previous process (crash, kill -9) is nobody's to finish.
    sweep_stale_workspaces(&args.workspace_dir, args.workspace_ttl_hours).await;
    let labels: Vec<String> = args
        .labels
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // SIGTERM / SIGINT → cancel in-flight steps, report them, then exit.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        warn!("shutdown signal received; cancelling in-flight steps");
        let _ = shutdown_tx.send(true);
    });

    // Process-wide concurrency cap: survives reconnects, so a flapping connection cannot
    // run more than --concurrency steps at once.
    let slots = Arc::new(tokio::sync::Semaphore::new(args.concurrency.max(1) as usize));
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = std::time::Instant::now();
        match run_session(&args, &labels, shutdown_rx.clone(), Arc::clone(&slots)).await {
            Ok(()) => info!("session ended"),
            Err(e) => {
                if is_unauthorized(&e) {
                    error!(
                        "agent token rejected (401); not retrying — rotate or re-issue the token"
                    );
                    std::process::exit(2);
                }
                error!(error = format!("{e:#}"), "session error");
            }
        }
        if *shutdown_rx.borrow() {
            info!("agent stopped");
            return Ok(());
        }
        // A session that lasted a while was healthy: start the backoff over.
        if started.elapsed() > Duration::from_secs(30) {
            backoff = Duration::from_secs(1);
        }
        let delay = with_jitter(backoff);
        info!(delay_ms = delay.as_millis() as u64, "reconnecting");
        let mut sd = shutdown_rx.clone();
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = sd.changed() => {
                info!("agent stopped");
                return Ok(());
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn is_unauthorized(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<tokio_tungstenite::tungstenite::Error>(),
            Some(tokio_tungstenite::tungstenite::Error::Http(r)) if r.status().as_u16() == 401
        )
    })
}

/// ±25% jitter so a fleet of agents does not reconnect in lockstep after an API restart.
fn with_jitter(d: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos())
        .unwrap_or(0) as u64;
    let pct = 75 + (nanos % 51); // 75..=125
    d * pct as u32 / 100
}

async fn run_session(
    args: &Args,
    labels: &[String],
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    slots: Arc<tokio::sync::Semaphore>,
) -> Result<()> {
    let mut url = Url::parse(&format!("{}/ws/agent", args.api_url.trim_end_matches('/')))?;
    url.query_pairs_mut().append_pair("token", &args.token);

    // Never log the token-bearing URL.
    info!(api = %args.api_url, "connecting");
    let (ws, _) = connect_async(url.as_str())
        .await
        .context("connect websocket")?;
    let (mut sink, mut stream) = ws.split();

    let mut agent_id = Uuid::nil();

    if let Some(Ok(Message::Text(text))) = stream.next().await {
        if let Ok(ServerMessage::Welcome { agent_id: id }) = serde_json::from_str(&text) {
            agent_id = id;
            info!(%agent_id, "registered");
        }
    }

    let hello = AgentMessage::Hello {
        name: args.name.clone(),
        labels: labels.to_vec(),
        concurrency: args.concurrency,
    };
    sink.send(Message::Text(serde_json::to_string(&hello)?.into()))
        .await?;

    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<AgentMessage>();

    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let Ok(text) = serde_json::to_string(&msg) else {
                continue;
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let prepared: Arc<Mutex<HashSet<Uuid>>> = Arc::new(Mutex::new(HashSet::new()));
    let workspaces: Arc<Workspaces> = Arc::new(Workspaces::default());
    let cancels: Arc<Mutex<HashMap<Uuid, oneshot::Sender<()>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let in_flight = Arc::new(AtomicU64::new(0));
    // Graceful drain: while set, finished step tasks do not report — the socket is
    // closed instead, and the server requeues the steps to another agent.
    let draining = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut heartbeat = tokio::time::interval(Duration::from_secs(10));

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let _ = out_tx.send(AgentMessage::Heartbeat { agent_id });
            }
            _ = shutdown.changed() => {
                let n = cancels.lock().map(|g| g.len()).unwrap_or(0);
                warn!(in_flight = n, "shutting down: stopping in-flight steps; the server will requeue them");
                // Do not report terminal status: closing the socket makes the server
                // requeue these steps (a restart must not fail the build).
                draining.store(true, Ordering::SeqCst);
                if let Ok(mut g) = cancels.lock() {
                    for (_, tx) in g.drain() {
                        let _ = tx.send(());
                    }
                }
                // Wait until every step task has killed its process (bounded).
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                while in_flight.load(Ordering::SeqCst) > 0 && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                writer.abort();
                return Ok(());
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ServerMessage>(&text) {
                            Ok(ServerMessage::Offer {
                                step_run_id,
                                run_id,
                                step_id,
                                step_name,
                                image,
                                run,
                                workspace,
                                env,
                                artifacts,
                                restore,
                                timeout_minutes,
                                secret_keys,
                            }) => {
                                info!(%step_id, %step_name, %run_id, "offered step");
                                let _ = out_tx.send(AgentMessage::Claim { agent_id, step_run_id });

                                let (cancel_tx, cancel_rx) = oneshot::channel();
                                if let Ok(mut g) = cancels.lock() {
                                    g.insert(step_run_id, cancel_tx);
                                }

                                let out_tx = out_tx.clone();
                                let prepared = Arc::clone(&prepared);
                                let cancels = Arc::clone(&cancels);
                                let workspace_dir = args.workspace_dir.clone();
                                let http_api = http_base(&args.api_url);
                                let token = args.token.clone();
                                let slots = Arc::clone(&slots);
                                let draining = Arc::clone(&draining);
                                let in_flight = Arc::clone(&in_flight);
                                // The attempt's clock starts now, not when a local permit frees up.
                                let offered_at = tokio::time::Instant::now();
                                let redactor = Redactor::new(&env, &secret_keys);
                                let exec = ExecConfig::from_args(args);
                                in_flight.fetch_add(1, Ordering::SeqCst);
                                let workspaces = Arc::clone(&workspaces);
                                tokio::spawn(async move {
                                    let _permit = slots.acquire_owned().await;
                                    let result = execute_step(
                                        &out_tx,
                                        agent_id,
                                        step_run_id,
                                        run_id,
                                        image.as_deref(),
                                        &run,
                                        workspace.as_ref(),
                                        &env,
                                        &artifacts,
                                        &restore,
                                        &http_api,
                                        &token,
                                        &workspace_dir,
                                        &prepared,
                                        cancel_rx,
                                        timeout_minutes,
                                        offered_at,
                                        &redactor,
                                        &exec,
                                        &workspaces,
                                    ).await;

                                    if let Ok(mut g) = cancels.lock() {
                                        g.remove(&step_run_id);
                                    }
                                    in_flight.fetch_sub(1, Ordering::SeqCst);
                                    if draining.load(Ordering::SeqCst) {
                                        return;
                                    }

                                    let complete = match result {
                                        Ok(code) => AgentMessage::StepComplete {
                                            agent_id,
                                            step_run_id,
                                            status: if code == 0 {
                                                StepStatus::Succeeded
                                            } else {
                                                StepStatus::Failed
                                            },
                                            exit_code: Some(code),
                                            error: None,
                                        },
                                        Err(e) if e.to_string().contains("cancelled") => {
                                            AgentMessage::StepComplete {
                                                agent_id,
                                                step_run_id,
                                                status: StepStatus::Cancelled,
                                                exit_code: None,
                                                error: Some("cancelled".into()),
                                            }
                                        }
                                        Err(e) if e.to_string().starts_with("timed out") => {
                                            AgentMessage::StepComplete {
                                                agent_id,
                                                step_run_id,
                                                status: StepStatus::Failed,
                                                exit_code: None,
                                                error: Some(redactor.apply(&e.to_string())),
                                            }
                                        }
                                        // Redacted like log lines: an error string can pick
                                        // up a value through `.context(...)`.
                                        Err(e) => AgentMessage::StepComplete {
                                            agent_id,
                                            step_run_id,
                                            status: StepStatus::Failed,
                                            exit_code: None,
                                            error: Some(redactor.apply(&e.to_string())),
                                        },
                                    };
                                    let _ = out_tx.send(complete);
                                });
                            }
                            Ok(ServerMessage::Cancel { step_run_id }) => {
                                warn!(%step_run_id, "cancel requested");
                                if let Ok(mut g) = cancels.lock() {
                                    if let Some(tx) = g.remove(&step_run_id) {
                                        let _ = tx.send(());
                                    }
                                }
                            }
                            Ok(ServerMessage::Error { message }) => {
                                warn!(%message, "server error");
                            }
                            Ok(ServerMessage::Welcome { .. }) => {}
                            Err(e) => warn!(error = %e, "bad server message"),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        // Signal all in-flight steps to stop.
                        if let Ok(mut g) = cancels.lock() {
                            for (_, tx) in g.drain() {
                                let _ = tx.send(());
                            }
                        }
                        writer.abort();
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        if let Ok(mut g) = cancels.lock() {
                            for (_, tx) in g.drain() {
                                let _ = tx.send(());
                            }
                        }
                        writer.abort();
                        return Err(e.into());
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Run one step, then release its workspace whatever the outcome.
///
/// Each step gets its own directory under the run's tree: steps of one run can be offered
/// to this agent concurrently, and a shared directory means they overwrite each other's
/// build output. Files move between steps as artifacts, not by sharing a checkout.
#[allow(clippy::too_many_arguments)]
async fn execute_step(
    out_tx: &tokio::sync::mpsc::UnboundedSender<AgentMessage>,
    agent_id: Uuid,
    step_run_id: Uuid,
    run_id: Uuid,
    image: Option<&str>,
    run: &str,
    workspace: Option<&WorkspaceOffer>,
    env: &[(String, String)],
    artifacts: &[String],
    restore: &[ArtifactRestore],
    http_api: &str,
    token: &str,
    workspace_root: &Path,
    prepared: &Mutex<HashSet<Uuid>>,
    cancel: oneshot::Receiver<()>,
    timeout_minutes: Option<u32>,
    offered_at: tokio::time::Instant,
    redactor: &Redactor,
    exec: &ExecConfig,
    workspaces: &Workspaces,
) -> Result<i32> {
    let run_dir = workspace_root.join(run_id.to_string());
    let work_dir = run_dir.join(step_run_id.to_string());
    workspaces.enter(run_id);
    let result = execute_step_inner(
        out_tx,
        agent_id,
        step_run_id,
        run_id,
        image,
        run,
        workspace,
        env,
        artifacts,
        restore,
        http_api,
        token,
        &run_dir,
        &work_dir,
        prepared,
        cancel,
        timeout_minutes,
        offered_at,
        redactor,
        exec,
        workspaces,
    )
    .await;
    // Cancel, timeout and prep failures all land here: a leaked checkout per aborted
    // attempt would fill the disk faster than successful runs do.
    cleanup_workspace(workspaces, run_id, &run_dir, &work_dir, prepared).await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn execute_step_inner(
    out_tx: &tokio::sync::mpsc::UnboundedSender<AgentMessage>,
    agent_id: Uuid,
    step_run_id: Uuid,
    run_id: Uuid,
    image: Option<&str>,
    run: &str,
    workspace: Option<&WorkspaceOffer>,
    env: &[(String, String)],
    artifacts: &[String],
    restore: &[ArtifactRestore],
    http_api: &str,
    token: &str,
    run_dir: &Path,
    work_dir: &Path,
    prepared: &Mutex<HashSet<Uuid>>,
    mut cancel: oneshot::Receiver<()>,
    timeout_minutes: Option<u32>,
    offered_at: tokio::time::Instant,
    redactor: &Redactor,
    exec: &ExecConfig,
    workspaces: &Workspaces,
) -> Result<i32> {
    // One sequence for system/stdout/stderr so the server's `ORDER BY seq` interleaves
    // streams in emission order (the old per-stream bases collided after 1000 lines).
    let seq = Arc::new(AtomicU64::new(0));
    let log_seq = Arc::clone(&seq);
    let log_redactor = redactor.clone();
    let mut log = |stream: &str, data: String| {
        let _ = out_tx.send(AgentMessage::LogChunk {
            agent_id,
            step_run_id,
            stream: stream.into(),
            data: log_redactor.apply(&data),
            seq: log_seq.fetch_add(1, Ordering::Relaxed),
        });
    };
    let deadline = timeout_minutes
        .filter(|m| *m > 0)
        .map(|m| offered_at + Duration::from_secs(u64::from(m) * 60));
    let timed_out_msg = || {
        format!(
            "timed out after {} min",
            timeout_minutes.unwrap_or_default()
        )
    };
    if let Some(m) = timeout_minutes {
        log("system", format!("step timeout: {m} min"));
    }

    tokio::fs::create_dir_all(work_dir).await?;

    // Workspace prep can be interrupted by cancel.
    let prep = async {
        if let Some(ws) = workspace {
            log(
                "system",
                format!("preparing workspace from {} @ {}", ws.repo, ws.git_ref),
            );
            // One network clone per run, then a local copy per step. The lock keeps
            // concurrent steps of this run from racing to create the reference.
            let reference = run_dir.join(".repo");
            let lock = workspaces.run_lock(run_id);
            {
                let _guard = lock.lock().await;
                prepare_reference_clone(&reference, ws, run_id, prepared, &mut log).await?;
            }
            clone_step_workspace(&reference, work_dir, &mut log).await?;
            tokio::fs::create_dir_all(work_dir).await?;
            log(
                "system",
                format!("workspace ready at {}", work_dir.display()),
            );
        } else {
            log(
                "system",
                format!("no git workspace; cwd={}", work_dir.display()),
            );
        }
        if !restore.is_empty() {
            restore_artifacts(http_api, token, work_dir, restore, &mut log).await?;
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        r = prep => r?,
        _ = &mut cancel => {
            log("system", "step cancelled during workspace prep".into());
            bail!("step cancelled");
        }
        _ = sleep_until_opt(deadline) => {
            let msg = timed_out_msg();
            log("system", format!("{msg} (during workspace prep)"));
            bail!("{msg}");
        }
    }

    // Killing the `docker run` client leaves the container running; name it so
    // cancel / timeout can `docker kill` it.
    let container_name = format!("fiber-step-{}", Uuid::new_v4());
    let mut docker_container: Option<String> = None;
    // Kept alive until the child exits: dropping it deletes the file docker reads.
    let mut env_file: Option<tempfile::NamedTempFile> = None;
    let mut child = if let Some(img) = image.filter(|i| !i.is_empty()).filter(|_| exec.use_docker) {
        let mount = format!("{}:/workspace", work_dir.display());
        log(
            "system",
            format!("running in docker image {img} (mount /workspace)"),
        );
        docker_container = Some(container_name.clone());
        let mut cmd = Command::new("docker");
        cmd.args([
            "run",
            "--rm",
            "--name",
            &container_name,
            "-v",
            &mount,
            "-w",
            "/workspace",
            // A step cannot gain privileges beyond the user it starts as.
            "--security-opt",
            "no-new-privileges",
        ]);
        if !exec.docker_user.is_empty() {
            cmd.args(["--user", &exec.docker_user]);
        }
        if !exec.docker_network.is_empty() {
            cmd.args(["--network", &exec.docker_network]);
        }
        if !exec.docker_memory.is_empty() {
            cmd.args(["--memory", &exec.docker_memory]);
        }
        if !exec.docker_cpus.is_empty() {
            cmd.args(["--cpus", &exec.docker_cpus]);
        }
        if exec.docker_pids_limit > 0 {
            cmd.args(["--pids-limit", &exec.docker_pids_limit.to_string()]);
        }
        // Say what was applied: a step killed for exceeding a limit exits 137 with no
        // other clue.
        let mut limits = Vec::new();
        if !exec.docker_memory.is_empty() {
            limits.push(format!("memory={}", exec.docker_memory));
        }
        if !exec.docker_cpus.is_empty() {
            limits.push(format!("cpus={}", exec.docker_cpus));
        }
        if exec.docker_pids_limit > 0 {
            limits.push(format!("pids={}", exec.docker_pids_limit));
        }
        limits.push(format!("network={}", exec.docker_network));
        log("system", format!("container limits: {}", limits.join(" ")));
        // The docker client is spawned from the agent, whose environment holds
        // FIBER_AGENT_TOKEN. Start it from nothing so an env-file line without `=`
        // (which tells docker to copy a variable from its own environment) has nothing
        // worth copying, then add back only what the client needs to reach a daemon.
        cmd.env_clear();
        for name in ENV_ALLOWLIST
            .iter()
            .copied()
            .chain(DOCKER_CLIENT_ENV.iter().copied())
            .chain(exec.env_passthrough.iter().map(String::as_str))
        {
            if let Ok(v) = std::env::var(name) {
                cmd.env(name, v);
            }
        }
        // `-e K=V` would put every project secret in the host's process list. An
        // env-file is read by docker and never appears in anyone's argv.
        let (file, from_client_env) = write_env_file(env, &mut log)?;
        cmd.args(["--env-file", &file.path().to_string_lossy()]);
        // Values an env-file cannot express (they contain newlines) are handed over as
        // `-e NAME`, which makes docker read NAME from the client environment we set here
        // — still never in argv.
        for (k, v) in &from_client_env {
            cmd.args(["-e", k]);
            cmd.env(k, v);
        }
        env_file = Some(file);
        // `sh -c`, never `sh -lc`: a login shell sources /etc/profile, which on Debian
        // resets PATH and throws away what the image put there — `rust:*` keeps cargo on
        // /usr/local/cargo/bin, so `-l` turns a plain `cargo build` into "cargo: not found".
        cmd.args([img, "sh", "-c", run])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        put_in_own_process_group(&mut cmd);
        cmd.spawn().context("spawn docker")?
    } else {
        log(
            "system",
            format!("running on host shell in {}", work_dir.display()),
        );
        let mut cmd = Command::new("sh");
        // Not `-lc`, for the same reason as the container: /etc/profile would overwrite
        // the environment assembled just below, PATH included.
        cmd.args(["-c", run])
            .current_dir(work_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Start from nothing: the agent's own environment holds FIBER_AGENT_TOKEN, which
        // repo-supplied shell must never see.
        cmd.env_clear();
        for name in ENV_ALLOWLIST
            .iter()
            .copied()
            .chain(exec.env_passthrough.iter().map(String::as_str))
        {
            if let Ok(v) = std::env::var(name) {
                cmd.env(name, v);
            }
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        put_in_own_process_group(&mut cmd);
        cmd.spawn().context("spawn shell")?
    };

    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

    let out_tx2 = out_tx.clone();
    let out_seq = Arc::clone(&seq);
    let out_redactor = redactor.clone();
    let out_handle = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let _ = out_tx2.send(AgentMessage::LogChunk {
                agent_id,
                step_run_id,
                stream: "stdout".into(),
                data: out_redactor.apply(&l),
                seq: out_seq.fetch_add(1, Ordering::Relaxed),
            });
        }
    });

    let err_tx = out_tx.clone();
    let err_seq = Arc::clone(&seq);
    let err_redactor = redactor.clone();
    let err_handle = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let _ = err_tx.send(AgentMessage::LogChunk {
                agent_id,
                step_run_id,
                stream: "stderr".into(),
                data: err_redactor.apply(&l),
                seq: err_seq.fetch_add(1, Ordering::Relaxed),
            });
        }
    });

    let code = tokio::select! {
        status = child.wait() => {
            let status = status?;
            let _ = out_handle.await;
            let _ = err_handle.await;
            status.code().unwrap_or(1)
        }
        _ = &mut cancel => {
            log("system", "killing step process".into());
            kill_step(&mut child, docker_container.as_deref()).await;
            let _ = out_handle.await;
            let _ = err_handle.await;
            bail!("step cancelled");
        }
        _ = sleep_until_opt(deadline) => {
            let msg = timed_out_msg();
            log("system", format!("{msg}; killing step process"));
            kill_step(&mut child, docker_container.as_deref()).await;
            let _ = out_handle.await;
            let _ = err_handle.await;
            bail!("{msg}");
        }
    };

    // A declared artifact that never reached storage must not leave the step green: a
    // dependent step restores it and would fail later with a missing file instead.
    let mut artifact_failures = Vec::new();
    if code == 0 && !artifacts.is_empty() {
        artifact_failures =
            upload_artifacts(http_api, token, step_run_id, work_dir, artifacts, &mut log).await;
    }
    drop(env_file);
    if !artifact_failures.is_empty() {
        bail!("artifact upload failed: {}", artifact_failures.join("; "));
    }

    Ok(code)
}

/// Drop this step's directory, and the whole run tree once its last step here is done.
async fn cleanup_workspace(
    workspaces: &Workspaces,
    run_id: Uuid,
    run_dir: &Path,
    work_dir: &Path,
    prepared: &Mutex<HashSet<Uuid>>,
) {
    let _ = tokio::fs::remove_dir_all(work_dir).await;
    if workspaces.leave(run_id) {
        if let Ok(mut g) = prepared.lock() {
            g.remove(&run_id);
        }
        // Rename first: deleting a large tree takes time, and a new step of this run
        // could otherwise start creating its directory inside the one being removed.
        let trash = run_dir.with_extension(format!("trash-{}", Uuid::new_v4()));
        match tokio::fs::rename(run_dir, &trash).await {
            Ok(()) => {
                if let Err(e) = tokio::fs::remove_dir_all(&trash).await {
                    warn!(path = %trash.display(), error = %e, "could not remove run workspace");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(path = %run_dir.display(), error = %e, "could not rename run workspace")
            }
        }
    }
}

/// Write `KEY=VALUE` lines for `docker --env-file`, mode 0600.
///
/// Returns the file plus the pairs it could not express (values containing a newline),
/// which the caller passes as `-e NAME` so docker reads them from the client environment.
/// Keys are validated: a line without `=` means "copy this from my own environment",
/// so an attacker-chosen key containing a newline could otherwise smuggle one in.
fn write_env_file(
    env: &[(String, String)],
    log: &mut impl FnMut(&str, String),
) -> Result<(tempfile::NamedTempFile, Vec<(String, String)>)> {
    use std::io::Write;
    let mut file = tempfile::Builder::new().prefix("fiber-env-").tempfile()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let mut deferred = Vec::new();
    for (k, v) in env {
        if !is_valid_env_key(k) {
            log(
                "system",
                format!("ignoring environment variable with an unusable name: {k:?}"),
            );
            continue;
        }
        if v.contains('\n') {
            deferred.push((k.clone(), v.clone()));
            continue;
        }
        writeln!(file, "{k}={v}")?;
    }
    file.flush()?;
    Ok((file, deferred))
}

/// Stop a step: the container (if any) first, then the client's process group.
async fn kill_step(child: &mut tokio::process::Child, container: Option<&str>) {
    if let Some(name) = container {
        let _ = Command::new("docker")
            .args(["kill", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    kill_process_group(child);
    let _ = child.wait().await;
}

/// Resolves at `deadline`, or never when there is none.
async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending::<()>().await,
    }
}

/// Put the child in its own process group so cancel can kill grandchildren.
fn put_in_own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        unsafe {
            cmd.pre_exec(|| {
                // SAFETY: called in the child after fork, before exec.
                libc::setpgid(0, 0);
                Ok(())
            });
        }
    }
    let _ = cmd;
}

fn kill_process_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // SAFETY: kill the child's process group; pid is the group leader we set.
            unsafe {
                let _ = libc::killpg(pid as i32, libc::SIGKILL);
            }
        }
    }
    let _ = child.start_kill();
}

const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;

async fn restore_artifacts(
    http_api: &str,
    token: &str,
    work_dir: &Path,
    restore: &[ArtifactRestore],
    log: &mut impl FnMut(&str, String),
) -> Result<()> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .context("http client")?;
    for art in restore {
        if art.name.contains("..") {
            log(
                "system",
                format!("skipping unsafe restore path: {}", art.name),
            );
            continue;
        }
        let url = format!("{http_api}/api/agent/artifacts/{}/download", art.id);
        log(
            "system",
            format!("restoring artifact {} ({} bytes)", art.name, art.size),
        );
        // A failure here is usually the 307 to object storage, not the API itself: the
        // presigned URL names the storage endpoint as the outside world reaches it. Ask
        // the API for the bytes instead, the same way the upload falls back.
        let resp = match client.get(&url).bearer_auth(token).send().await {
            Ok(r) => r,
            Err(e) => {
                log(
                    "system",
                    format!("{}: fetching through the API ({e})", art.name),
                );
                match client
                    .get(format!("{url}?via=api"))
                    .bearer_auth(token)
                    .send()
                    .await
                {
                    Ok(r) => r,
                    Err(e2) => {
                        let msg = format!("RESTORE FAILED {}: network error: {e2}", art.name);
                        log("system", msg.clone());
                        bail!("{msg}");
                    }
                }
            }
        };
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            let msg = if snippet.is_empty() {
                format!("RESTORE FAILED {}: HTTP {status}", art.name)
            } else {
                format!("RESTORE FAILED {}: HTTP {status} — {snippet}", art.name)
            };
            log("system", msg.clone());
            bail!("{msg}");
        }
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                let msg = format!("RESTORE FAILED {}: read body: {e}", art.name);
                log("system", msg.clone());
                bail!("{msg}");
            }
        };
        let dest = work_dir.join(&art.name);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&dest, &bytes)
            .await
            .with_context(|| format!("write restored artifact {}", art.name))?;
        log(
            "system",
            format!("restored artifact {} → {}", art.name, dest.display()),
        );
    }
    Ok(())
}

/// Upload each declared artifact, returning the ones that could not be stored.
///
/// A path that does not exist is a warning, not a failure: a pipeline may legitimately
/// declare an artifact its step only sometimes produces. Anything that exists but could
/// not be stored — unreadable, over the size cap, or a failed transfer — is returned, and
/// the caller fails the step.
async fn upload_artifacts(
    http_api: &str,
    token: &str,
    step_run_id: Uuid,
    work_dir: &Path,
    artifacts: &[String],
    log: &mut impl FnMut(&str, String),
) -> Vec<String> {
    let mut failures = Vec::new();
    let client = reqwest::Client::new();
    let proxy_url = format!("{http_api}/api/agent/steps/{step_run_id}/artifacts");
    let presign_url = format!("{http_api}/api/agent/steps/{step_run_id}/artifacts/presign");
    let complete_url = format!("{http_api}/api/agent/steps/{step_run_id}/artifacts/complete");
    for rel in artifacts {
        let rel = rel.trim();
        if rel.is_empty() || rel.contains("..") {
            let msg = format!("unsafe artifact path: {rel}");
            log("system", msg.clone());
            failures.push(msg);
            continue;
        }
        let path = work_dir.join(rel);
        match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.is_file() => {
                if meta.len() > MAX_ARTIFACT_BYTES {
                    let msg = format!(
                        "artifact {} too large ({} bytes, limit {MAX_ARTIFACT_BYTES})",
                        rel,
                        meta.len()
                    );
                    log("system", msg.clone());
                    failures.push(msg);
                    continue;
                }
                match tokio::fs::read(&path).await {
                    Ok(bytes) => {
                        log(
                            "system",
                            format!("uploading artifact {rel} ({} bytes)", bytes.len()),
                        );
                        match upload_one_artifact(
                            &client,
                            token,
                            &presign_url,
                            &complete_url,
                            &proxy_url,
                            rel,
                            bytes::Bytes::from(bytes),
                        )
                        .await
                        {
                            Ok(mode) => {
                                log("system", format!("uploaded artifact {rel} via {mode}"))
                            }
                            Err(msg) => {
                                log("system", msg.clone());
                                failures.push(msg);
                            }
                        }
                    }
                    Err(e) => {
                        let msg = format!("failed to read artifact {rel}: {e}");
                        log("system", msg.clone());
                        failures.push(msg);
                    }
                }
            }
            Ok(_) => log("system", format!("artifact {rel} is not a file; skipping")),
            Err(e) => log("system", format!("artifact {rel} missing: {e}")),
        }
    }
    failures
}

/// Store one artifact, returning how it got there.
///
/// The presigned URL points at object storage as the **outside world** reaches it
/// (`FIBER_S3_PUBLIC_ENDPOINT`). An agent that cannot reach that address — a container kept
/// off the storage network, or an agent behind a different boundary — falls back to sending
/// the bytes through the API, which it can reach by definition, since that is where its
/// offers come from. Isolating the agent should cost throughput, not artifacts.
async fn upload_one_artifact(
    client: &reqwest::Client,
    token: &str,
    presign_url: &str,
    complete_url: &str,
    proxy_url: &str,
    rel: &str,
    bytes: bytes::Bytes,
) -> Result<String, String> {
    let size = bytes.len() as u64;
    let mode = match client
        .post(presign_url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "path": rel, "size": size }))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| format!("presign parse {rel}: {e}"))?,
        Ok(resp) => {
            return Err(format!("presign {rel} failed: HTTP {}", resp.status()));
        }
        Err(e) => return Err(format!("presign {rel} failed: {e}")),
    };

    if mode.get("mode").and_then(|v| v.as_str()) == Some("presign") {
        let upload_url = mode
            .get("upload_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("presign {rel}: missing upload_url"))?;
        let stored_path = mode
            .get("stored_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("presign {rel}: missing stored_path"))?;
        // Cheap to clone: `Bytes` is refcounted, so the fallback costs no second copy.
        let unreachable = match client.put(upload_url).body(bytes.clone()).send().await {
            Ok(put) if put.status().is_success() => None,
            Ok(put) => Some(format!("HTTP {}", put.status())),
            Err(e) => Some(e.to_string()),
        };
        if let Some(why) = unreachable {
            let via = upload_via_proxy(client, token, proxy_url, rel, bytes).await?;
            return Ok(format!("{via} (presigned upload unreachable: {why})"));
        }
        let done = client
            .post(complete_url)
            .bearer_auth(token)
            .json(&serde_json::json!({
                "path": rel,
                "size": size,
                "stored_path": stored_path,
            }))
            .send()
            .await
            .map_err(|e| format!("complete {rel}: {e}"))?;
        if !done.status().is_success() {
            return Err(format!("complete {rel} failed: HTTP {}", done.status()));
        }
        return Ok("presign".into());
    }

    // Local backend, or object storage the API would rather proxy for.
    upload_via_proxy(client, token, proxy_url, rel, bytes).await
}

/// Send the bytes through the API, which stores them with whatever backend it has.
async fn upload_via_proxy(
    client: &reqwest::Client,
    token: &str,
    proxy_url: &str,
    rel: &str,
    bytes: bytes::Bytes,
) -> Result<String, String> {
    let resp = client
        .put(proxy_url)
        .bearer_auth(token)
        .header("X-Fiber-Artifact-Path", rel)
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("upload {rel} failed: {e}"))?;
    if resp.status().is_success() {
        Ok("proxy".into())
    } else {
        Err(format!("upload {rel} failed: HTTP {}", resp.status()))
    }
}

/// One network fetch per run, shared by every step of that run on this agent.
///
/// Fetch rather than clone: a pull request's head (`refs/pull/<n>/head`) is not a branch,
/// and an exact commit has to be checked out after the ref is fetched — cloning with
/// `--branch` can do neither. The base repository serves a PR head ref, so a fork's pull
/// request builds without access to the fork.
async fn prepare_reference_clone(
    reference: &Path,
    ws: &WorkspaceOffer,
    run_id: Uuid,
    prepared: &Mutex<HashSet<Uuid>>,
    log: &mut impl FnMut(&str, String),
) -> Result<()> {
    let already = prepared
        .lock()
        .map(|g| g.contains(&run_id))
        .unwrap_or(false);
    if already && reference.join(".git").exists() {
        return Ok(());
    }
    if reference.exists() {
        let _ = tokio::fs::remove_dir_all(reference).await;
    }
    tokio::fs::create_dir_all(reference).await?;

    let target = ws.sha.clone().unwrap_or_else(|| ws.git_ref.clone());
    log("system", format!("fetching {} @ {target}", ws.repo));
    run_git(reference, &["init", "--quiet"], log).await?;
    run_git(reference, &["remote", "add", "origin", &ws.repo], log).await?;

    // Depth 50 keeps the fetch small while leaving room to check out a commit slightly
    // behind the ref tip (a push that lands while the run is queued).
    // `--` so a ref or sha can never be read as a git option.
    run_git(
        reference,
        &["fetch", "--depth", "50", "origin", "--", &ws.git_ref],
        log,
    )
    .await
    .with_context(|| format!("fetch {} from {}", ws.git_ref, ws.repo))?;

    // `--` goes *after* the revision: before it, git reads the argument as a pathspec.
    let checkout = match &ws.sha {
        Some(sha) => run_git(reference, &["checkout", "--force", sha, "--"], log).await,
        None => run_git(reference, &["checkout", "--force", "FETCH_HEAD", "--"], log).await,
    };
    if let Err(e) = checkout {
        // Only a sha can be outside the shallow window. Deepen in bounded steps rather
        // than pulling whole history, and never fall back to a different commit: a run
        // that cannot build what it was asked to build must fail, not build something else.
        let Some(sha) = &ws.sha else {
            return Err(e);
        };
        let mut found = false;
        for depth in ["500", "5000"] {
            log(
                "system",
                format!("{sha} not in the shallow history; deepening to {depth}"),
            );
            if run_git(
                reference,
                &[
                    "fetch",
                    &format!("--depth={depth}"),
                    "origin",
                    "--",
                    &ws.git_ref,
                ],
                log,
            )
            .await
            .is_err()
            {
                break;
            }
            if run_git(reference, &["checkout", "--force", sha, "--"], log)
                .await
                .is_ok()
            {
                found = true;
                break;
            }
        }
        if !found {
            bail!("commit {sha} is not reachable from {}", ws.git_ref);
        }
    }

    if let Ok(mut g) = prepared.lock() {
        g.insert(run_id);
    }
    Ok(())
}

/// A step's own checkout, cloned from the run's reference over `file://`.
///
/// Not `git worktree`: a worktree's `.git` is a file pointing at an absolute path inside
/// the reference, which is not mounted into a step container, so git would not work
/// there. Not `git clone --local` either: that refuses a shallow source. `file://` copies
/// objects locally (no network) and produces a self-contained repository.
async fn clone_step_workspace(
    reference: &Path,
    work_dir: &Path,
    log: &mut impl FnMut(&str, String),
) -> Result<()> {
    if work_dir.join(".git").exists() {
        // A retried attempt reuses this path: put it back to a clean tree.
        let _ = run_git(work_dir, &["reset", "--hard", "HEAD"], log).await;
        let _ = run_git(work_dir, &["clean", "-fdx"], log).await;
        return Ok(());
    }
    let _ = tokio::fs::remove_dir(work_dir).await;
    let src = format!("file://{}", reference.display());
    let out = Command::new("git")
        .args(["clone", "--depth", "1", &src])
        .arg(work_dir)
        .output()
        .await
        .context("git clone from the run reference")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        log("system", format!("workspace clone failed: {}", err.trim()));
        bail!("could not create the step workspace from the run's clone");
    }
    Ok(())
}

/// Delete run workspaces — and orphaned step env files — left behind by a crash.
async fn sweep_stale_workspaces(root: &Path, ttl_hours: u64) {
    if ttl_hours == 0 {
        return;
    }
    let ttl = Duration::from_secs(ttl_hours * 3600);
    // A `kill -9` skips NamedTempFile's cleanup, leaving a file of secrets in TMPDIR.
    if let Ok(mut tmp) = tokio::fs::read_dir(std::env::temp_dir()).await {
        while let Ok(Some(entry)) = tmp.next_entry().await {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("fiber-env-")
            {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let stale = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > ttl);
        if stale {
            info!(path = %entry.path().display(), "removing stale workspace");
            let _ = tokio::fs::remove_dir_all(entry.path()).await;
        }
    }
}

async fn run_git(cwd: &Path, args: &[&str], log: &mut impl FnMut(&str, String)) -> Result<()> {
    log("system", format!("git {}", args.join(" ")));
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .output()
        .await
        .context("git")?;
    if !output.stdout.is_empty() {
        log(
            "stdout",
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        );
    }
    if !output.stderr.is_empty() {
        log(
            "stderr",
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        );
    }
    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(())
}

#[allow(dead_code)]
fn _ws_ty(_: Ws) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn redactor_masks_secret_values_anywhere_in_a_line() {
        let e = env(&[
            ("NPM_TOKEN", "supersecretvalue"),
            ("MATRIX_OS", "linux"),
            ("SHORT", "abc"),
        ]);
        let r = Redactor::new(&e, &["NPM_TOKEN".into(), "SHORT".into()]);
        assert_eq!(r.apply("using supersecretvalue now"), "using *** now");
        assert_eq!(
            r.apply("Authorization: Bearer supersecretvalue"),
            "Authorization: Bearer ***"
        );
        // Non-secret env is untouched, and a too-short secret is not masked (it would
        // blank out unrelated output for little gain).
        assert_eq!(r.apply("os=linux abc"), "os=linux abc");
    }

    #[test]
    fn redactor_masks_the_longer_secret_when_one_contains_another() {
        let e = env(&[("A", "tokenvalue1234"), ("B", "tokenvalue")]);
        let r = Redactor::new(&e, &["A".into(), "B".into()]);
        assert_eq!(r.apply("x tokenvalue1234 y"), "x *** y");
    }

    #[test]
    fn redactor_without_secrets_is_a_passthrough() {
        let r = Redactor::new(&env(&[("A", "value123456")]), &[]);
        assert_eq!(r.apply("value123456"), "value123456");
    }

    #[test]
    fn env_file_is_private_and_defers_unrepresentable_values() {
        let e = env(&[
            ("TOKEN", "s3cret"),
            ("MULTI", "line1\nline2"),
            ("PLAIN", "ok"),
        ]);
        let mut log = |_: &str, _: String| {};
        let (f, deferred) = write_env_file(&e, &mut log).unwrap();
        let body = std::fs::read_to_string(f.path()).unwrap();
        assert!(body.contains("TOKEN=s3cret"));
        assert!(body.contains("PLAIN=ok"));
        // A newline cannot go in an env-file; it is handed over as `-e NAME` instead.
        assert!(!body.contains("MULTI"));
        assert_eq!(
            deferred,
            vec![("MULTI".to_string(), "line1\nline2".to_string())]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(f.path()).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "step env file must not be readable by others"
            );
        }
    }

    #[test]
    fn env_file_refuses_keys_that_could_smuggle_a_line() {
        // A line without `=` tells docker to copy that variable from its own environment,
        // so a key carrying a newline must never reach the file.
        let e = env(&[
            ("FIBER_AGENT_TOKEN\nX", "linux"),
            ("HAS SPACE", "v"),
            ("1LEADING_DIGIT", "v"),
            ("", "v"),
            ("GOOD_KEY", "value"),
        ]);
        let mut log = |_: &str, _: String| {};
        let (f, deferred) = write_env_file(&e, &mut log).unwrap();
        let body = std::fs::read_to_string(f.path()).unwrap();
        assert_eq!(body, "GOOD_KEY=value\n");
        assert!(deferred.is_empty());
        for line in body.lines() {
            assert!(line.contains('='), "every line must bind a value: {line:?}");
        }
    }

    #[test]
    fn env_key_validation() {
        assert!(is_valid_env_key("PATH"));
        assert!(is_valid_env_key("_x9"));
        assert!(!is_valid_env_key("A\nB"));
        assert!(!is_valid_env_key("A B"));
        assert!(!is_valid_env_key("9A"));
        assert!(!is_valid_env_key(""));
        assert!(!is_valid_env_key("A=B"));
    }

    #[test]
    fn redactor_masks_each_line_of_a_multi_line_secret() {
        let key = "-----BEGIN KEY-----\nabcdefghijklmnop\nqrstuvwxyz123456\n-----END KEY-----";
        let r = Redactor::new(&env(&[("DEPLOY_KEY", key)]), &["DEPLOY_KEY".into()]);
        // Logs arrive one line at a time, so the whole-value pattern never matches.
        assert_eq!(r.apply("abcdefghijklmnop"), "***");
        assert_eq!(
            r.apply("prefix qrstuvwxyz123456 suffix"),
            "prefix *** suffix"
        );
    }

    #[test]
    fn workspace_refcount_deletes_only_after_the_last_step() {
        let w = Workspaces::default();
        let run = Uuid::new_v4();
        w.enter(run);
        w.enter(run);
        assert!(!w.leave(run), "another step of this run is still running");
        assert!(w.leave(run), "last step out removes the run tree");
        // Unknown runs never claim ownership.
        assert!(!w.leave(Uuid::new_v4()));
    }
}
