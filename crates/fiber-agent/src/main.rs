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
                error!(error = %e, "session error");
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
                                let use_docker = args.use_docker;
                                let http_api = http_base(&args.api_url);
                                let token = args.token.clone();
                                let slots = Arc::clone(&slots);
                                let draining = Arc::clone(&draining);
                                let in_flight = Arc::clone(&in_flight);
                                // The attempt's clock starts now, not when a local permit frees up.
                                let offered_at = tokio::time::Instant::now();
                                in_flight.fetch_add(1, Ordering::SeqCst);
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
                                        use_docker,
                                        &workspace_dir,
                                        &prepared,
                                        cancel_rx,
                                        timeout_minutes,
                                        offered_at,
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
                                                error: Some(e.to_string()),
                                            }
                                        }
                                        Err(e) => AgentMessage::StepComplete {
                                            agent_id,
                                            step_run_id,
                                            status: StepStatus::Failed,
                                            exit_code: None,
                                            error: Some(e.to_string()),
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
    use_docker: bool,
    workspace_root: &Path,
    prepared: &Mutex<HashSet<Uuid>>,
    mut cancel: oneshot::Receiver<()>,
    timeout_minutes: Option<u32>,
    offered_at: tokio::time::Instant,
) -> Result<i32> {
    // One sequence for system/stdout/stderr so the server's `ORDER BY seq` interleaves
    // streams in emission order (the old per-stream bases collided after 1000 lines).
    let seq = Arc::new(AtomicU64::new(0));
    let log_seq = Arc::clone(&seq);
    let mut log = |stream: &str, data: String| {
        let _ = out_tx.send(AgentMessage::LogChunk {
            agent_id,
            step_run_id,
            stream: stream.into(),
            data,
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

    let work_dir = workspace_root.join(run_id.to_string());
    tokio::fs::create_dir_all(&work_dir).await?;

    // Workspace prep can be interrupted by cancel.
    let prep = async {
        if let Some(ws) = workspace {
            let already = prepared
                .lock()
                .map(|g| g.contains(&run_id))
                .unwrap_or(false);
            if !already {
                log(
                    "system",
                    format!("preparing workspace from {} @ {}", ws.repo, ws.git_ref),
                );
                prepare_git_workspace(&work_dir, ws, &mut log).await?;
                if let Ok(mut g) = prepared.lock() {
                    g.insert(run_id);
                }
                log(
                    "system",
                    format!("workspace ready at {}", work_dir.display()),
                );
            }
        } else {
            log(
                "system",
                format!("no git workspace; cwd={}", work_dir.display()),
            );
        }
        if !restore.is_empty() {
            restore_artifacts(http_api, token, &work_dir, restore, &mut log).await?;
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
    let mut child = if let Some(img) = image.filter(|i| !i.is_empty()).filter(|_| use_docker) {
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
        ]);
        for (k, v) in env {
            cmd.args(["-e", &format!("{k}={v}")]);
        }
        cmd.args([img, "sh", "-lc", run])
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
        cmd.args(["-lc", run])
            .current_dir(&work_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
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
    let out_handle = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let _ = out_tx2.send(AgentMessage::LogChunk {
                agent_id,
                step_run_id,
                stream: "stdout".into(),
                data: l,
                seq: out_seq.fetch_add(1, Ordering::Relaxed),
            });
        }
    });

    let err_tx = out_tx.clone();
    let err_seq = Arc::clone(&seq);
    let err_handle = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let _ = err_tx.send(AgentMessage::LogChunk {
                agent_id,
                step_run_id,
                stream: "stderr".into(),
                data: l,
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

    if code == 0 && !artifacts.is_empty() {
        upload_artifacts(http_api, token, step_run_id, &work_dir, artifacts, &mut log).await;
    }

    Ok(code)
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
        let resp = match client.get(&url).bearer_auth(token).send().await {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("RESTORE FAILED {}: network error: {e}", art.name);
                log("system", msg.clone());
                bail!("{msg}");
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

async fn upload_artifacts(
    http_api: &str,
    token: &str,
    step_run_id: Uuid,
    work_dir: &Path,
    artifacts: &[String],
    log: &mut impl FnMut(&str, String),
) {
    let client = reqwest::Client::new();
    let proxy_url = format!("{http_api}/api/agent/steps/{step_run_id}/artifacts");
    let presign_url = format!("{http_api}/api/agent/steps/{step_run_id}/artifacts/presign");
    let complete_url = format!("{http_api}/api/agent/steps/{step_run_id}/artifacts/complete");
    for rel in artifacts {
        let rel = rel.trim();
        if rel.is_empty() || rel.contains("..") {
            log("system", format!("skipping unsafe artifact path: {rel}"));
            continue;
        }
        let path = work_dir.join(rel);
        match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.is_file() => {
                if meta.len() > MAX_ARTIFACT_BYTES {
                    log(
                        "system",
                        format!(
                            "artifact {} too large ({} bytes); skipping",
                            rel,
                            meta.len()
                        ),
                    );
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
                            bytes,
                        )
                        .await
                        {
                            Ok(mode) => {
                                log("system", format!("uploaded artifact {rel} via {mode}"))
                            }
                            Err(msg) => log("system", msg),
                        }
                    }
                    Err(e) => log("system", format!("failed to read artifact {rel}: {e}")),
                }
            }
            Ok(_) => log("system", format!("artifact {rel} is not a file; skipping")),
            Err(e) => log("system", format!("artifact {rel} missing: {e}")),
        }
    }
}

async fn upload_one_artifact(
    client: &reqwest::Client,
    token: &str,
    presign_url: &str,
    complete_url: &str,
    proxy_url: &str,
    rel: &str,
    bytes: Vec<u8>,
) -> Result<&'static str, String> {
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
        let put = client
            .put(upload_url)
            .body(bytes)
            .send()
            .await
            .map_err(|e| format!("s3 put {rel}: {e}"))?;
        if !put.status().is_success() {
            return Err(format!("s3 put {rel} failed: HTTP {}", put.status()));
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
        return Ok("presign");
    }

    // Local backend (or presign unavailable): proxy bytes through the API.
    let resp = client
        .put(proxy_url)
        .bearer_auth(token)
        .header("X-Fiber-Artifact-Path", rel)
        .body(bytes)
        .send()
        .await
        .map_err(|e| format!("upload {rel} failed: {e}"))?;
    if resp.status().is_success() {
        Ok("proxy")
    } else {
        Err(format!("upload {rel} failed: HTTP {}", resp.status()))
    }
}

async fn prepare_git_workspace(
    work_dir: &Path,
    ws: &WorkspaceOffer,
    log: &mut impl FnMut(&str, String),
) -> Result<()> {
    let git_dir = work_dir.join(".git");
    if git_dir.exists() {
        log("system", "fetching updates…".into());
        run_git(work_dir, &["fetch", "--all", "--prune"], log).await?;
        if run_git(work_dir, &["checkout", "--force", &ws.git_ref], log)
            .await
            .is_err()
        {
            run_git(
                work_dir,
                &[
                    "checkout",
                    "--force",
                    "-B",
                    &ws.git_ref,
                    &format!("origin/{}", ws.git_ref),
                ],
                log,
            )
            .await?;
        }
        let _ = run_git(work_dir, &["reset", "--hard", "HEAD"], log).await;
        let _ = run_git(work_dir, &["clean", "-fdx"], log).await;
    } else {
        log("system", format!("cloning {} …", ws.repo));
        let status = Command::new("git")
            .args([
                "clone",
                "--branch",
                &ws.git_ref,
                "--single-branch",
                "--depth",
                "50",
                &ws.repo,
                &work_dir.to_string_lossy(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .status()
            .await
            .context("git clone")?;
        if !status.success() {
            log(
                "system",
                "branch clone failed; cloning default then checking out ref".into(),
            );
            let _ = tokio::fs::remove_dir_all(work_dir).await;
            tokio::fs::create_dir_all(work_dir).await?;
            let status = Command::new("git")
                .args([
                    "clone",
                    "--depth",
                    "50",
                    &ws.repo,
                    &work_dir.to_string_lossy(),
                ])
                .kill_on_drop(true)
                .status()
                .await
                .context("git clone default")?;
            if !status.success() {
                bail!("git clone failed");
            }
            run_git(work_dir, &["checkout", "--force", &ws.git_ref], log).await?;
        }
    }
    Ok(())
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
