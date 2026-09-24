use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use fiber_proto::limits::{MAX_LOG_LINE_BYTES, MAX_RAW_LINE_BYTES, TRUNCATION_MARKER};
use fiber_proto::{
    AgentMessage, ArtifactRestore, LogLineWire, ServerMessage, StepStatus, WorkspaceOffer,
};
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::header;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;

use tracing::Instrument;

mod otel;

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
    /// `--label`s on every step container: this agent, and this process of it. The
    /// startup sweep uses them to find its own orphans without touching another agent's
    /// running containers.
    container_labels: ContainerLabels,
    env_passthrough: Vec<String>,
    docker_user: String,
    docker_network: String,
    docker_memory: String,
    docker_cpus: String,
    docker_pids_limit: i64,
}

impl ExecConfig {
    fn from_args(args: &Args, container_labels: ContainerLabels) -> Self {
        Self {
            use_docker: args.use_docker,
            container_labels,
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

/// Every spelling of one secret that a step is likely to print.
///
/// Matching the raw bytes only is what makes redaction look like it works and then not:
/// `base64 <<< "$TOKEN"`, a token in a `curl --trace` URL, and a value inside a JSON body
/// are all the secret, and none of them contains its literal bytes. Each form here is one
/// a step produces without trying to — the point is the accident, not the adversary, who
/// can always encrypt.
fn redaction_forms(value: &str) -> Vec<String> {
    use base64::Engine as _;
    // The floor is on the secret, not on its encodings: base64 of a three-character
    // value is eight characters, and registering that would mask unrelated output for a
    // value too short to be worth protecting.
    if value.len() < Redactor::MIN_LEN {
        return Vec::new();
    }
    let std_b64 = base64::engine::general_purpose::STANDARD;
    let url_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut out = vec![
        value.to_string(),
        std_b64.encode(value.as_bytes()),
        url_b64.encode(value.as_bytes()),
        // `base64 <<< "$TOKEN"` and `echo "$TOKEN" | base64` encode a trailing newline,
        // which is the form the finding was written against.
        std_b64.encode(format!("{value}\n").as_bytes()),
        percent_encoded(value),
        json_escaped(value),
    ];
    out.retain(|v| v.len() >= Redactor::MIN_LEN);
    out
}

/// RFC 3986 percent-encoding of everything outside the unreserved set — what a query
/// string, a form body, or a client tracing a request writes.
fn percent_encoded(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The value as it appears inside a JSON string: quotes, backslashes, control characters
/// and newlines escaped, without the surrounding quotes.
fn json_escaped(value: &str) -> String {
    let quoted = serde_json::Value::String(value.to_string()).to_string();
    quoted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(&quoted)
        .to_string()
}

/// Replaces every occurrence of a secret value in log output with `***`.
///
/// One Aho-Corasick automaton over every registered form, not a loop of `contains` per
/// pattern: this runs on every log line of every step, and the pattern count is set by
/// the project's secrets, not by us. Three 30-line PEM keys are 558 patterns — a
/// per-pattern scan of a 64 KiB line is tens of milliseconds, which backpressures the log
/// channel into the step's own pipes and slows the build. The automaton is O(line length)
/// whatever the pattern count, and `LeftmostLongest` gives the "a secret containing
/// another is masked whole" rule directly, where the old code got it by sorting patterns
/// longest-first and hoping.
#[derive(Clone, Default)]
struct Redactor {
    matcher: Option<Arc<AhoCorasick>>,
}

impl Redactor {
    /// Values shorter than this are skipped: masking a two-character secret would blank
    /// out unrelated output without protecting much. Applied to each encoded form too, so
    /// a short secret does not come back through a longer encoding of itself.
    const MIN_LEN: usize = 8;

    fn new(env: &[(String, String)], secret_keys: &[String]) -> Self {
        let mut bases: Vec<String> = Vec::new();
        for (k, v) in env.iter().filter(|(k, _)| secret_keys.contains(k)) {
            let _ = k;
            bases.push(v.clone());
            // Logs arrive a line at a time, so a multi-line secret (a PEM key, a service
            // account JSON) would never match as a whole. Mask its lines individually.
            if v.contains('\n') {
                bases.extend(v.lines().map(str::trim_end).map(str::to_string));
            }
        }
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut values: Vec<String> = Vec::new();
        for base in bases {
            for form in redaction_forms(&base) {
                if seen.insert(form.clone()) {
                    values.push(form);
                }
            }
        }
        Self::from_patterns(values)
    }

    fn from_patterns(values: Vec<String>) -> Self {
        if values.is_empty() {
            return Self { matcher: None };
        }
        // `LeftmostLongest`: where two registered forms overlap at the same position, the
        // longer one wins, so a secret that contains another is masked whole.
        let matcher = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&values);
        match matcher {
            Ok(m) => Self {
                matcher: Some(Arc::new(m)),
            },
            // Refusing to redact would be worse than the cost of the fallback, and there
            // is no fallback left — so say so loudly and mask nothing rather than
            // pretending. In practice this cannot fail for literal patterns.
            Err(e) => {
                error!(error = %e, "could not build the log redactor; secrets will NOT be masked");
                Self { matcher: None }
            }
        }
    }

    fn apply(&self, line: &str) -> String {
        let Some(m) = &self.matcher else {
            return line.to_string();
        };
        // One replacement for every pattern, written by a closure rather than a vector of
        // as many `"***"`s as there are patterns.
        let mut out = String::with_capacity(line.len());
        m.replace_all_with(line, &mut out, |_, _, dst| {
            dst.push_str("***");
            true
        });
        out
    }

    /// Patterns registered, for tests.
    #[cfg(test)]
    fn pattern_count(&self) -> usize {
        self.matcher.as_ref().map_or(0, |m| m.patterns_len())
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

/// Agent heartbeat period. The server renews every lease the agent holds on each one.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// Messages held for the server while the socket is down. Past this the oldest log
/// line is dropped for each new message; completions and artifacts are never dropped.
/// At a few hundred bytes a line this is a few megabytes, and a step that produces
/// more than this during a five-minute outage loses its oldest output, with a system
/// line saying how much.
const OUTBOX_CAP: usize = 10_000;
/// Log bytes held for the server while the socket is down. A line cap alone does not
/// bound memory: 10 000 lines of a step printing 64 KiB each is 640 MB. Whichever of the
/// two budgets runs out first evicts the oldest output.
const OUTBOX_LOG_BYTES: usize = 8 * 1024 * 1024;
/// Lines in flight between a step's pipes and its batcher.
///
/// Bounded, and the readers `send().await` on it: `yes | head -n 10000000` has to be made
/// to wait somewhere, and the right place is the pipe, where the kernel buffer fills and
/// the step's own `write` blocks. Unbounded, the agent buffered the whole of a runaway
/// step's output in memory before any cap on the server applied.
const LINE_CHANNEL_CAP: usize = 10_000;
/// Longest a line waits in a batch before it is sent, so a step that prints one line a
/// second still shows up live.
const LOG_FLUSH_INTERVAL: Duration = Duration::from_millis(50);
/// Lines per batch. A cap on the WebSocket frame and on the server's multi-row insert.
const LOG_FLUSH_LINES: usize = 500;
/// Bytes of line data per batch.
const LOG_FLUSH_BYTES: usize = 64 * 1024;
/// Longest a batcher waits for room in the outbox before it gives up and lets the
/// drop-oldest policy take over. A bound, not a target: it exists so a wedged socket
/// that has not yet been noticed cannot hold a step's pipes shut forever.
const LOG_BACKPRESSURE_MAX: Duration = Duration::from_secs(30);
/// Longest a killed step waits for its buffered output to reach the server before the
/// rest is dropped. A cancel has to be reported promptly: the permit is held and the run
/// reads "cancelling" until it is.
const KILL_DRAIN_BUDGET: Duration = Duration::from_secs(10);
/// Longest a step that exited on its own waits for its buffered output to reach the
/// server. Generous, because all of this output belongs in the log and the child is
/// already gone — but not unbounded: a connected socket that is not draining makes the
/// batcher wait `LOG_BACKPRESSURE_MAX` per flush, and ten thousand buffered lines is
/// minutes of holding the concurrency permit for a step that has finished.
const EXIT_DRAIN_BUDGET: Duration = Duration::from_secs(60);
/// The protocol revision that introduced `LogBatch`. A server below it gets one
/// `LogChunk` per line.
const LOG_BATCH_PROTOCOL: u32 = 2;

/// What the outbox did with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Enqueue {
    Keep,
    /// Admitted, and the oldest log line in the queue was dropped to make room.
    DropOldestLog,
    /// Admitted past the hard bound, and the oldest message that is not a completion
    /// was dropped to make room.
    DropOldest,
    /// Not admitted: the queue is at its hard bound and holds nothing but completions.
    Refused,
}

/// The step a queued message is about, if any.
fn step_of(m: &AgentMessage) -> Option<Uuid> {
    match m {
        AgentMessage::Claim { step_run_id, .. }
        | AgentMessage::LogChunk { step_run_id, .. }
        | AgentMessage::LogBatch { step_run_id, .. }
        | AgentMessage::Artifact { step_run_id, .. }
        | AgentMessage::StepComplete { step_run_id, .. } => Some(*step_run_id),
        AgentMessage::Hello { .. }
        | AgentMessage::Heartbeat { .. }
        | AgentMessage::Goodbye { .. } => None,
    }
}

/// Outbound messages to the API, in order, kept across WebSocket sessions.
///
/// A session is not an attempt: a step keeps running through a reconnect and its lease
/// is still live on the server, so its lines and its completion have to reach the server
/// when the socket is back — in the order they happened. The bound protects the
/// process, and only log lines pay for it.
/// A queued message and the token identifying it while the writer has it in flight.
#[derive(Debug)]
struct Queued {
    token: u64,
    msg: AgentMessage,
}

#[derive(Debug)]
struct Outbox {
    queue: std::collections::VecDeque<Queued>,
    /// Log **lines** the queue may hold, not messages: one batch is many lines, and a
    /// message count would let one chatty step evict another step's whole output.
    cap: usize,
    /// Bytes of log data the queue may hold, whatever the line count.
    max_bytes: usize,
    /// Handed out by `push`, never reused: `pop_sent` compares it so a message that
    /// was purged mid-send cannot make the writer discard the one behind it.
    next_token: u64,
    /// Lines dropped per (step, attempt) since the last flush reported them.
    dropped: HashMap<(Uuid, Option<i32>), Dropped>,
    queued_lines: usize,
    queued_bytes: usize,
}

/// Lines and bytes of log data a message accounts for. Everything that is not output
/// weighs nothing: completions and artifacts are bounded by the steps in flight.
fn log_weight(m: &AgentMessage) -> (usize, usize) {
    match m {
        AgentMessage::LogChunk { data, .. } => (1, data.len()),
        AgentMessage::LogBatch { lines, .. } => {
            (lines.len(), lines.iter().map(|l| l.data.len()).sum())
        }
        _ => (0, 0),
    }
}

/// Output lost from the queue for one (step, attempt), and where it was lost.
///
/// `at_seq` is the `seq` of the newest line dropped, so the notice reporting the gap
/// sorts into the log where the gap is rather than wherever it happens to be stored.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Dropped {
    count: u64,
    at_seq: u64,
}

impl Dropped {
    fn record(&mut self, count: u64, at_seq: u64) {
        self.count += count;
        self.at_seq = self.at_seq.max(at_seq);
    }
}

fn is_log(m: &AgentMessage) -> bool {
    matches!(
        m,
        AgentMessage::LogChunk { .. } | AgentMessage::LogBatch { .. }
    )
}

impl Outbox {
    #[cfg(test)]
    fn with_capacity(cap: usize) -> Self {
        Self::with_limits(cap, usize::MAX)
    }

    fn with_limits(cap: usize, max_bytes: usize) -> Self {
        Self {
            queue: std::collections::VecDeque::new(),
            cap,
            max_bytes,
            next_token: 0,
            dropped: HashMap::new(),
            queued_lines: 0,
            queued_bytes: 0,
        }
    }

    fn over_log_budget(&self) -> bool {
        self.queued_lines > self.cap || self.queued_bytes > self.max_bytes
    }

    /// Room for a whole batch, not just for one more line. Waiting on `has_room()`
    /// alone let a 500-line batch into a queue one line under the budget, and the
    /// eviction that followed took up to 500 lines off the front — possibly another
    /// step's — while the socket was up and could have been waited for.
    fn has_room_for(&self, lines: usize, bytes: usize) -> bool {
        self.queued_lines + lines <= self.cap && self.queued_bytes + bytes <= self.max_bytes
    }

    /// Recount from the queue. Cheap enough at the cap, and the alternative is two
    /// counters that a later `retain` forgets to adjust.
    fn recount(&mut self) {
        let (lines, bytes) = self
            .queue
            .iter()
            .map(|q| log_weight(&q.msg))
            .fold((0, 0), |(l, b), (dl, db)| (l + dl, b + db));
        self.queued_lines = lines;
        self.queued_bytes = bytes;
    }

    /// Admit `msg`, dropping the oldest log line first when the queue is full. Past the
    /// cap a queue of completions and artifacts still grows — they are bounded by the
    /// steps in flight, not by their output — up to a hard bound of twice the cap, past
    /// which the oldest message that is not a completion goes, and a queue of nothing
    /// but completions refuses the newcomer rather than lose a result already held.
    fn push(&mut self, msg: AgentMessage) -> Enqueue {
        let token = self.next_token;
        self.next_token += 1;
        let (lines, bytes) = log_weight(&msg);
        self.queued_lines += lines;
        self.queued_bytes += bytes;
        self.queue.push_back(Queued { token, msg });
        let mut evicted = false;
        while self.over_log_budget() {
            let Some(i) = self.queue.iter().position(|q| is_log(&q.msg)) else {
                break;
            };
            // The newcomer is the only output left: keeping the newest line and losing
            // the oldest is the whole point, so it stays even if it alone is over.
            if i + 1 == self.queue.len() {
                break;
            }
            let Some(q) = self.queue.remove(i) else { break };
            let (lines, bytes) = log_weight(&q.msg);
            self.queued_lines -= lines;
            self.queued_bytes -= bytes;
            match q.msg {
                AgentMessage::LogChunk {
                    step_run_id,
                    attempt,
                    seq,
                    ..
                } => self
                    .dropped
                    .entry((step_run_id, attempt))
                    .or_default()
                    .record(1, seq),
                AgentMessage::LogBatch {
                    step_run_id,
                    attempt,
                    lines,
                    ..
                } => {
                    let last = lines.last().map(|l| l.seq).unwrap_or(0);
                    self.dropped
                        .entry((step_run_id, attempt))
                        .or_default()
                        .record(lines.len() as u64, last);
                }
                _ => {}
            }
            evicted = true;
        }
        if evicted {
            return Enqueue::DropOldestLog;
        }
        if self.queue.len() <= self.cap {
            return Enqueue::Keep;
        }
        if self.queue.len() <= self.cap * 2 {
            return Enqueue::Keep;
        }
        let oldest_other = self
            .queue
            .iter()
            .position(|q| !matches!(q.msg, AgentMessage::StepComplete { .. }));
        let outcome = match oldest_other {
            // The newcomer is the only non-completion: it is the one not admitted.
            Some(i) if i + 1 == self.queue.len() => {
                self.queue.pop_back();
                Enqueue::Refused
            }
            Some(i) => {
                self.queue.remove(i);
                Enqueue::DropOldest
            }
            None => {
                self.queue.pop_back();
                Enqueue::Refused
            }
        };
        self.recount();
        outcome
    }

    /// The frames the next message becomes on this session, with its token and without
    /// removing it: it leaves the queue only once the socket has taken all of them, so
    /// a writer that dies mid-send cannot lose it.
    ///
    /// Usually one frame. When `server_batches` is false — the server predates
    /// `LogBatch`, which a rolling deploy makes possible for a message queued while
    /// talking to a newer replica — a batch is unpacked here into one `LogChunk` per
    /// line, each keeping its original `seq`. This is the last point at which the
    /// session's capabilities are known, which is why the expansion lives here and not
    /// where the batch was built.
    fn peek_front_frames(
        &self,
        server_batches: bool,
    ) -> Option<(u64, Result<Vec<String>, serde_json::Error>)> {
        let q = self.queue.front()?;
        if let AgentMessage::LogBatch {
            agent_id,
            step_run_id,
            attempt,
            lines,
        } = &q.msg
            && !server_batches
        {
            let frames = lines
                .iter()
                .map(|l| {
                    serde_json::to_string(&AgentMessage::LogChunk {
                        agent_id: *agent_id,
                        step_run_id: *step_run_id,
                        stream: l.stream.clone(),
                        data: l.data.clone(),
                        seq: l.seq,
                        attempt: *attempt,
                    })
                })
                .collect::<Result<Vec<_>, _>>();
            return Some((q.token, frames));
        }
        Some((q.token, serde_json::to_string(&q.msg).map(|t| vec![t])))
    }

    /// Drop the front message, but only if it is still the one `token` named. The
    /// writer releases the lock while the socket takes a message, and a purge in that
    /// window shifts the queue: popping blind would discard a message never sent.
    fn pop_sent(&mut self, token: u64) {
        if self.queue.front().is_some_and(|q| q.token == token)
            && let Some(q) = self.queue.pop_front()
        {
            // The budget is what `send_batch` waits on: leaving it charged for a message
            // already on the wire makes the outbox look permanently full, and every
            // later batch waits out `LOG_BACKPRESSURE_MAX` for room that is already there.
            let (lines, bytes) = log_weight(&q.msg);
            self.queued_lines -= lines;
            self.queued_bytes -= bytes;
        }
    }

    #[cfg(test)]
    fn queued(&self) -> usize {
        self.queue.len()
    }

    #[cfg(test)]
    fn msgs(&self) -> impl Iterator<Item = &AgentMessage> {
        self.queue.iter().map(|q| &q.msg)
    }

    /// Per-(step, attempt) lines dropped since the last call.
    fn take_dropped(&mut self) -> HashMap<(Uuid, Option<i32>), Dropped> {
        std::mem::take(&mut self.dropped)
    }

    /// Put counts back after their notices failed to reach the server. Taking them out
    /// and then losing them is worse than never counting: the gap in the log becomes
    /// unmarked, and a flapping reconnect — which is what produces the gaps — is
    /// exactly when the send fails.
    fn restore_dropped(&mut self, counts: HashMap<(Uuid, Option<i32>), Dropped>) {
        for (key, d) in counts {
            self.dropped
                .entry(key)
                .or_default()
                .record(d.count, d.at_seq);
        }
    }

    /// Forget everything queued about a step — lines, claim, artifacts, and its
    /// completion. Used when the step was given up on (its lease has ended or is about
    /// to) and when a new offer for the same `step_run_id` arrives: anything still held
    /// is about an earlier attempt, and delivered late it would either be dropped by
    /// the server or, without the attempt check, close the attempt that replaced it.
    fn purge_step(&mut self, step_run_id: Uuid) {
        self.queue.retain(|q| step_of(&q.msg) != Some(step_run_id));
        self.dropped.retain(|(s, _), _| *s != step_run_id);
        self.recount();
    }

    /// Forget a step's buffered output, keeping everything that reports on it. Used
    /// when a step is given up: its lines are worth nothing to an attempt the server
    /// has moved past, while the report that the attempt is over is the whole point.
    fn purge_step_logs(&mut self, step_run_id: Uuid) {
        self.queue
            .retain(|q| !(is_log(&q.msg) && step_of(&q.msg) == Some(step_run_id)));
        self.dropped.retain(|(s, _), _| *s != step_run_id);
        self.recount();
    }
}

/// Shared handle to the outbox for step tasks; `send` never blocks and never fails.
#[derive(Clone)]
struct Outbound {
    outbox: Arc<Mutex<Outbox>>,
    /// Woken on every push; the session's writer drains the queue on it.
    wake: Arc<tokio::sync::Notify>,
    /// Woken when the writer has put the queue on the wire, so a producer waiting for
    /// room knows to look again.
    drained: Arc<tokio::sync::Notify>,
    /// Whether a session is established (Hello sent) right now. Step tasks read it to
    /// decide whether an HTTP failure is worth waiting out.
    connected: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the server on the other end of the current session can parse
    /// `LogBatch`, from `Welcome.protocol_version`. False for a server that predates it
    /// — and for no session at all — so the writer expands a batch into one `LogChunk`
    /// per line rather than have the server answer `Error { "invalid message" }` and
    /// lose the build's entire log. Set per session, cleared with `connected`.
    server_batches: Arc<std::sync::atomic::AtomicBool>,
}

impl Outbound {
    fn new() -> Self {
        Self {
            outbox: Arc::new(Mutex::new(Outbox::with_limits(
                OUTBOX_CAP,
                OUTBOX_LOG_BYTES,
            ))),
            wake: Arc::new(tokio::sync::Notify::new()),
            drained: Arc::new(tokio::sync::Notify::new()),
            connected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            server_batches: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn server_takes_batches(&self) -> bool {
        self.server_batches.load(Ordering::SeqCst)
    }

    fn send(&self, msg: AgentMessage) -> Enqueue {
        let step = step_of(&msg);
        let outcome = self
            .outbox
            .lock()
            .map(|mut o| o.push(msg))
            .unwrap_or(Enqueue::Keep);
        match outcome {
            Enqueue::DropOldest => warn!(
                ?step,
                "outbox past its hard bound; dropped the oldest message"
            ),
            Enqueue::Refused => warn!(?step, "outbox full of completions; message refused"),
            Enqueue::Keep | Enqueue::DropOldestLog => {}
        }
        self.wake.notify_one();
        outcome
    }

    fn has_room_for(&self, msg: &AgentMessage) -> bool {
        let (lines, bytes) = log_weight(msg);
        self.outbox
            .lock()
            .map(|o| o.has_room_for(lines, bytes))
            .unwrap_or(true)
    }

    /// Queue a batch of output, waiting for room instead of dropping the oldest lines.
    ///
    /// This is where a runaway step is made to wait. The batcher stops taking lines, the
    /// pipe's channel fills, the readers stop reading, the kernel pipe buffer fills, and
    /// the step's own `write` blocks — which is the only back-pressure that reaches the
    /// thing actually producing the output. Dropping instead (what `send` does) turned a
    /// 50 000-line step into 10 000 stored lines and no way to get the rest.
    ///
    /// Only while a session is up: with the socket down there is nothing to wait for, and
    /// holding a step's pipes shut for the length of an outage is worse than losing its
    /// oldest output, which is what the outbox bound is for. Bounded either way, so a
    /// socket that is up but not draining cannot wedge the step.
    async fn send_batch(&self, msg: AgentMessage) -> Enqueue {
        let deadline = tokio::time::Instant::now() + LOG_BACKPRESSURE_MAX;
        while self.is_connected()
            && !self.has_room_for(&msg)
            && tokio::time::Instant::now() < deadline
        {
            // Polled rather than purely notified, and the 25 ms is load-bearing:
            // `notify_waiters` stores no permit, so a wake that lands between the
            // check above and the registration below is lost, and `connected` can go
            // false while this waits. Without the timeout either would hold the step's
            // pipes shut until the outer bound.
            let _ = tokio::time::timeout(Duration::from_millis(25), self.drained.notified()).await;
        }
        self.send(msg)
    }

    fn purge_step(&self, step_run_id: Uuid) {
        if let Ok(mut o) = self.outbox.lock() {
            o.purge_step(step_run_id);
        }
    }

    fn purge_step_logs(&self, step_run_id: Uuid) {
        if let Ok(mut o) = self.outbox.lock() {
            o.purge_step_logs(step_run_id);
        }
    }
}

/// The `&'static str` for a stream name the step task uses.
///
/// `RawLine` carries `&'static str` because the pumps only ever produce two values;
/// the step task's own notes are always `system`, and anything else it invents would be
/// a bug rather than a new stream.
fn system_stream(stream: &str) -> &'static str {
    match stream {
        "stdout" => "stdout",
        "stderr" => "stderr",
        _ => "system",
    }
}

/// One line read from a step's pipe, on its way to a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawLine {
    /// `stdout` or `stderr`; system lines do not come through a pipe.
    stream: &'static str,
    /// Assigned where the line was read, so stdout, stderr and the system lines the step
    /// task emits directly stay in the order they happened however they are batched.
    seq: u64,
    data: String,
}

/// One raw line from a pipe, decoded for the wire.
///
/// Three things happen here, and each one is a bug that has bitten this path:
/// the trailing newline (and a CR before it) is not part of the line; the bytes are
/// decoded **lossily**, because a step that prints a Latin-1 filename is not an error;
/// and a line longer than the cap is cut with a visible marker rather than sent whole.
///
/// `raw` is expected to include its newline when the line was complete. A buffer that
/// ends without one and is at the raw cap is a line the reader stopped early — it says so
/// in the output.
fn truncate_line(raw: &[u8]) -> String {
    let (body, complete) = match raw.strip_suffix(b"\n") {
        Some(b) => (b.strip_suffix(b"\r").unwrap_or(b), true),
        None => (raw, false),
    };
    let cut = !complete && body.len() >= MAX_RAW_LINE_BYTES;
    let text = String::from_utf8_lossy(&body[..body.len().min(MAX_RAW_LINE_BYTES)]);
    // Lossy decoding expands: every invalid byte becomes a three-byte replacement
    // character, so even a raw buffer inside the cap can come out over it.
    if !cut && text.len() <= MAX_LOG_LINE_BYTES {
        return text.into_owned();
    }
    let keep = fiber_proto::limits::floor_char_boundary(
        &text,
        MAX_LOG_LINE_BYTES - TRUNCATION_MARKER.len(),
    );
    let mut out = String::with_capacity(MAX_LOG_LINE_BYTES);
    out.push_str(&text[..keep]);
    out.push_str(TRUNCATION_MARKER);
    out
}

/// Read `reader` to EOF, sending one [`RawLine`] per line.
///
/// Not `lines()`: that decodes UTF-8 and returns `InvalidData` on the first byte that is
/// not, and the loop around it treated that as end-of-stream — so one Latin-1 byte in an
/// `ls` listing dropped the pipe, the child got SIGPIPE on its next write, and the step
/// ended at exit 141 with an empty log. Reading bytes has no decode step to fail, so the
/// only way out of this loop is EOF, a real I/O error on the pipe, or the batcher going
/// away.
///
/// `send().await` is the back-pressure: when the channel is full this stops reading, the
/// pipe buffer fills, and the step's own `write` blocks. That is the only thing standing
/// between `yes | head -n 10000000` and the agent's whole address space.
async fn pump_lines<R>(
    reader: R,
    stream: &'static str,
    seq: Arc<AtomicU64>,
    tx: mpsc::Sender<RawLine>,
) where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = {
            // Bounded per read: `read_until` on a 100 MB line with no newline would
            // allocate all of it before anything looked at the length.
            let mut limited = (&mut reader).take(MAX_RAW_LINE_BYTES as u64);
            match limited.read_until(b'\n', &mut buf).await {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    // In the build log too, not only the agent's: a step whose output
                    // simply stops, with the agent's own journal on another host, is
                    // the kind of thing nobody diagnoses.
                    warn!(error = %e, stream, "reading step output failed");
                    let _ = tx
                        .send(RawLine {
                            stream: "system",
                            seq: seq.fetch_add(1, Ordering::Relaxed),
                            data: format!("reading the step's {stream} failed: {e}"),
                        })
                        .await;
                    break;
                }
            }
        };
        if n == 0 {
            break;
        }
        let over_long = n >= MAX_RAW_LINE_BYTES && !buf.ends_with(b"\n");
        let line = RawLine {
            stream,
            seq: seq.fetch_add(1, Ordering::Relaxed),
            data: truncate_line(&buf),
        };
        if tx.send(line).await.is_err() {
            break;
        }
        if over_long && !discard_rest_of_line(&mut reader).await {
            break;
        }
    }
}

/// Swallow what is left of a line already sent truncated, up to and including its
/// newline. `false` means the pipe ended or failed while doing it.
async fn discard_rest_of_line<R>(reader: &mut BufReader<R>) -> bool
where
    R: AsyncRead + Unpin,
{
    let mut sink = Vec::new();
    loop {
        sink.clear();
        let mut limited = reader.take(MAX_RAW_LINE_BYTES as u64);
        match limited.read_until(b'\n', &mut sink).await {
            Ok(0) => return false,
            Ok(_) if sink.ends_with(b"\n") => return true,
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
}

/// Whether the lines held so far should go out now.
///
/// Three triggers, whichever comes first: enough lines that the batch is worth its own
/// insert, enough bytes that the frame is big enough, or enough time that a step printing
/// one line a second would otherwise look stalled in the UI. Nothing is a flush when
/// nothing is pending — an empty batch is legal on the wire but costs a round trip.
fn should_flush(lines: usize, bytes: usize, elapsed: Duration) -> bool {
    lines > 0
        && (lines >= LOG_FLUSH_LINES || bytes >= LOG_FLUSH_BYTES || elapsed >= LOG_FLUSH_INTERVAL)
}

/// Coalesce one step's output into `LogBatch` messages until its pipes close.
///
/// Batching is what takes the server's ingest from two database round trips and a publish
/// per line to one insert and one publish per batch. It does not renumber anything: each
/// line keeps the `seq` it was given when it was read, so a batch re-sent after a
/// reconnect carries the same numbers it did the first time.
async fn batch_lines(
    mut rx: mpsc::Receiver<RawLine>,
    out: Outbound,
    agent_id: Uuid,
    step_run_id: Uuid,
    attempt: Option<i32>,
    redactor: Redactor,
) {
    let mut pending: Vec<LogLineWire> = Vec::new();
    let mut bytes = 0usize;
    let mut oldest: Option<tokio::time::Instant> = None;
    loop {
        let deadline = oldest.map(|t| t + LOG_FLUSH_INTERVAL);
        let next = tokio::select! {
            line = rx.recv() => line,
            _ = sleep_until_opt(deadline) => {
                flush_batch(&out, agent_id, step_run_id, attempt, &mut pending, &mut bytes)
                    .await;
                oldest = None;
                continue;
            }
        };
        let Some(line) = next else { break };
        let data = redactor.apply(&line.data);
        bytes += data.len();
        pending.push(LogLineWire {
            stream: line.stream.to_string(),
            data,
            seq: line.seq,
        });
        oldest.get_or_insert_with(tokio::time::Instant::now);
        let elapsed = oldest.map(|t| t.elapsed()).unwrap_or_default();
        if should_flush(pending.len(), bytes, elapsed) {
            flush_batch(
                &out,
                agent_id,
                step_run_id,
                attempt,
                &mut pending,
                &mut bytes,
            )
            .await;
            oldest = None;
        }
    }
    flush_batch(
        &out,
        agent_id,
        step_run_id,
        attempt,
        &mut pending,
        &mut bytes,
    )
    .await;
}

async fn flush_batch(
    out: &Outbound,
    agent_id: Uuid,
    step_run_id: Uuid,
    attempt: Option<i32>,
    pending: &mut Vec<LogLineWire>,
    bytes: &mut usize,
) {
    if pending.is_empty() {
        return;
    }
    *bytes = 0;
    out.send_batch(AgentMessage::LogBatch {
        agent_id,
        step_run_id,
        attempt,
        lines: std::mem::take(pending),
    })
    .await;
}

/// How long a step keeps running after the socket is lost, before the agent gives it up.
///
/// The server reclaims a lease `lease_secs` after the last heartbeat it processed —
/// which may be a heartbeat interval before the socket died, with one more heartbeat
/// possibly lost in flight — and the first heartbeat after a reconnect needs a moment to
/// land. Three intervals of margin cover that, so a step still running here is one the
/// server still counts as this agent's. Without `lease_secs` the server is older than
/// this contract and requeues on close; a step kept running here would then race the
/// re-leased attempt, so the grace is zero and the step is stopped at once, as before.
fn grace_after_disconnect(lease_secs: Option<u64>, heartbeat: Duration) -> Duration {
    match lease_secs {
        // Clamped: an absurd value would overflow `Instant + Duration` in the watchdog
        // and, by panicking that task, quietly disable giving up at all.
        Some(secs) => Duration::from_secs(secs.min(MAX_LEASE_SECS)).saturating_sub(heartbeat * 3),
        None => Duration::ZERO,
    }
}

/// Ceilings on one artifact transfer. Without them a connection the kernel never
/// gives up on outlasts the lease grace entirely: the retry budget is only consulted
/// between attempts, so one hung request is unbounded. Generous enough for a 64 MB
/// artifact over a slow link.
const ARTIFACT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const ARTIFACT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// An HTTP client for artifact transfers that cannot hang for ever.
fn artifact_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(ARTIFACT_CONNECT_TIMEOUT)
        .timeout(ARTIFACT_REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .context("http client")
}

/// Upper bound on a `lease_secs` the agent will honour (a day).
const MAX_LEASE_SECS: u64 = 86_400;
/// Frames the server sends unprompted (a ping every 15 s from servers that carry
/// `lease_secs`); silence this long means the socket is black-holed, and the session
/// ends so the watchdog is armed rather than the lease outlived.
const SERVER_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

/// When a disconnected agent gives up its steps: `grace` after the last frame the server
/// sent — the lease runs from work the server's loop actually did, and a frame back is
/// that loop proving it is turning — or, if none was ever seen, after the disconnect
/// itself. Anchored, not re-armed: a reconnect that fails, or one that connects and
/// drops before the server says anything, does not move it.
fn give_up_at(
    last_server_frame: Option<tokio::time::Instant>,
    disconnected_at: tokio::time::Instant,
    grace: Duration,
) -> tokio::time::Instant {
    last_server_frame.unwrap_or(disconnected_at) + grace
}

/// Whether a session's end should start a lease watchdog: one per outage, and only
/// while there is something to give up. A watchdog already running keeps its anchor;
/// a second one per failed reconnect would fire on a session that has since healed.
fn should_arm_watchdog(watchdog_alive: bool, steps_in_flight: u64) -> bool {
    !watchdog_alive && steps_in_flight > 0
}

/// Everything that outlives one WebSocket session: the steps in flight, their cancel
/// handles, the outbound queue, the per-run workspace state, and the concurrency cap.
struct AgentState {
    /// Process-wide concurrency cap: survives reconnects, so a flapping connection
    /// cannot run more than --concurrency steps at once.
    slots: Arc<tokio::sync::Semaphore>,
    outbound: Outbound,
    prepared: Arc<Mutex<HashSet<Uuid>>>,
    workspaces: Arc<Workspaces>,
    /// Cancel handles, keyed by the attempt they belong to: attempt 1's task can still
    /// be winding down as attempt 2 is offered, and a key of `step_run_id` alone let
    /// the older task's cleanup take the newer one's handle out.
    cancels: Arc<Mutex<HashMap<StepAttemptKey, oneshot::Sender<()>>>>,
    in_flight: Arc<AtomicU64>,
    /// Attempts whose own outcome must not be reported: they were given up and the
    /// report was queued here instead, so their task ends quietly.
    abandoned: Arc<Mutex<HashSet<StepAttemptKey>>>,
    /// Graceful drain: while set, finished step tasks do not report — the server is told
    /// Goodbye instead and requeues the steps to another agent.
    draining: Arc<std::sync::atomic::AtomicBool>,
    /// From the last `Welcome`. `None` until a server has said, or when the server is
    /// older than the field.
    lease_secs: Option<u64>,
    /// Labels every step container of this process carries. Made once, in `main`, so
    /// the boot id in them is the one the startup sweep excluded — a second set would
    /// make a later sweep treat this process's own containers as a previous boot's.
    container_labels: ContainerLabels,
    /// This agent's id, as the last `Welcome` gave it. Only informational on the wire
    /// (the server binds identity from the token), but a report the agent synthesises
    /// still carries it so it does not read as spoofed.
    agent_id: Arc<Mutex<Uuid>>,
    /// When a frame was last *received*. The lease watchdog counts from here rather
    /// than from the agent's last written heartbeat: a write only reaches a buffer,
    /// while a frame back — the server pings every 15 s — is the server's event loop
    /// proving it is still turning, which is what renews the lease.
    last_server_frame_at: Arc<Mutex<Option<tokio::time::Instant>>>,
    /// The start of the current outage, if in one. Set once per outage, cleared when a
    /// session is established.
    disconnected_at: Option<tokio::time::Instant>,
    /// The one lease watchdog for the current outage.
    watchdog: Option<tokio::task::JoinHandle<()>>,
}

impl AgentState {
    fn new(concurrency: u32, container_labels: ContainerLabels) -> Self {
        Self {
            container_labels,
            slots: Arc::new(tokio::sync::Semaphore::new(concurrency.max(1) as usize)),
            outbound: Outbound::new(),
            prepared: Arc::new(Mutex::new(HashSet::new())),
            workspaces: Arc::new(Workspaces::default()),
            cancels: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(AtomicU64::new(0)),
            abandoned: Arc::new(Mutex::new(HashSet::new())),
            draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lease_secs: None,
            agent_id: Arc::new(Mutex::new(Uuid::nil())),
            last_server_frame_at: Arc::new(Mutex::new(None)),
            disconnected_at: None,
            watchdog: None,
        }
    }

    fn last_server_frame(&self) -> Option<tokio::time::Instant> {
        self.last_server_frame_at.lock().ok().and_then(|g| *g)
    }

    /// A frame arrived: the server is alive and processing, which is what the lease
    /// watchdog measures from.
    fn note_server_frame(&self) {
        if let Ok(mut g) = self.last_server_frame_at.lock() {
            *g = Some(tokio::time::Instant::now());
        }
    }

    fn agent_id(&self) -> Uuid {
        self.agent_id
            .lock()
            .map(|g| *g)
            .unwrap_or_else(|_| Uuid::nil())
    }

    /// A session is up (Hello sent): the outage, if any, is over. The watchdog goes;
    /// but if its deadline had already passed, the server has reclaimed the steps and
    /// they are given up here rather than run on for a lease that is gone.
    ///
    /// `anchor` is the last frame seen *before* this session — reading it here would
    /// find the `Welcome` this session just took, and the deadline would never be past.
    fn on_session_established(&mut self, anchor: Option<tokio::time::Instant>) {
        self.outbound.connected.store(true, Ordering::SeqCst);
        if let Some(w) = self.watchdog.take() {
            w.abort();
        }
        let Some(started) = self.disconnected_at.take() else {
            return;
        };
        let grace = grace_after_disconnect(self.lease_secs, HEARTBEAT_INTERVAL);
        let now = tokio::time::Instant::now();
        if now >= give_up_at(anchor, started, grace) {
            let n = give_up_steps(
                self.agent_id(),
                &self.cancels,
                &self.abandoned,
                &self.outbound,
            );
            warn!(
                steps = n,
                "reconnected after the lease ran out; stopping in-flight steps and reporting them lost"
            );
        } else if self.steps_in_flight() > 0 {
            info!(
                steps = self.steps_in_flight(),
                "reconnected with steps still running; resuming"
            );
        }
    }

    fn steps_in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Stop every step for shutdown. They are not reported either, but their lines are
    /// kept: the socket is still up and Goodbye follows them.
    fn cancel_all_for_shutdown(&self) {
        self.draining.store(true, Ordering::SeqCst);
        if let Ok(mut g) = self.cancels.lock() {
            for (_, tx) in g.drain() {
                let _ = tx.send(());
            }
        }
    }
}

/// A session ended. While the agent is reconnecting, a step from the last session keeps
/// running until the lease it holds on the server can no longer be counted on; then it
/// is stopped. One watchdog per outage, anchored at the last heartbeat before it, and
/// aborted by `on_session_established`.
fn arm_watchdog(st: &mut AgentState, disconnected_at: tokio::time::Instant) {
    let alive = st.watchdog.as_ref().is_some_and(|w| !w.is_finished());
    if !should_arm_watchdog(alive, st.steps_in_flight()) {
        return;
    }
    let grace = grace_after_disconnect(st.lease_secs, HEARTBEAT_INTERVAL);
    let cancels = Arc::clone(&st.cancels);
    let abandoned = Arc::clone(&st.abandoned);
    let outbound = st.outbound.clone();
    let in_flight = Arc::clone(&st.in_flight);
    let last_frame = Arc::clone(&st.last_server_frame_at);
    let agent_id = st.agent_id();
    info!(
        steps = st.steps_in_flight(),
        grace_secs = grace.as_secs(),
        "disconnected with steps in flight; they keep running while the agent reconnects"
    );
    st.watchdog = Some(tokio::spawn(async move {
        loop {
            let last = last_frame.lock().ok().and_then(|g| *g);
            let deadline = give_up_at(last, disconnected_at, grace);
            tokio::time::sleep_until(deadline).await;
            if in_flight.load(Ordering::SeqCst) == 0 {
                return;
            }
            // Belt and braces against a stale watchdog: a session that is up keeps the
            // leases renewed, and it is not this task's call to stop anything then.
            if outbound.is_connected() {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                continue;
            }
            let n = give_up_steps(agent_id, &cancels, &abandoned, &outbound);
            if n > 0 {
                warn!(
                    steps = n,
                    grace_secs = grace.as_secs(),
                    "not reconnected within the lease; stopping in-flight steps and queueing a lost-lease report for each"
                );
            }
            return;
        }
    }));
}

/// A step and the attempt it was offered as. Messages are accepted per attempt, and a
/// `step_run_id` is the same for every attempt of the step, so nothing the agent keys
/// on a step alone survives a re-lease.
type StepAttemptKey = (Uuid, Option<i32>);

/// What the agent reports for an attempt it is giving up.
///
/// Going silent was worse than it looks: the row stayed `running` under this agent, and
/// the first heartbeat after reconnecting renewed its lease — and every one after that
/// — so no lease ever expired, the reclaim loop never saw it, nothing reported it, and
/// the run hung until the step-timeout backstop while the agent kept the slot for a
/// process it had already killed. A report ends the attempt on whichever side of the
/// race the server is: if it has reclaimed and re-leased the step, the attempt on this
/// message no longer matches the row and the server drops it; if it has not, the step
/// fails now and retries under its own budget.
fn give_up_report(agent_id: Uuid, step_run_id: Uuid, attempt: Option<i32>) -> AgentMessage {
    AgentMessage::StepComplete {
        agent_id,
        step_run_id,
        status: StepStatus::Failed,
        exit_code: None,
        error: Some("lease lost while the agent was disconnected".into()),
        attempt,
    }
}

/// Stop every step and report it lost. Its buffered *lines* go — they describe an
/// attempt the server has finished with — but the report does not. Returns how many
/// were stopped.
fn give_up_steps(
    agent_id: Uuid,
    cancels: &Mutex<HashMap<StepAttemptKey, oneshot::Sender<()>>>,
    abandoned: &Mutex<HashSet<StepAttemptKey>>,
    outbound: &Outbound,
) -> usize {
    let Ok(mut g) = cancels.lock() else {
        return 0;
    };
    let n = g.len();
    for ((step_run_id, attempt), tx) in g.drain() {
        // The task must not also report: this is the one report for the attempt.
        if let Ok(mut a) = abandoned.lock() {
            a.insert((step_run_id, attempt));
        }
        outbound.purge_step_logs(step_run_id);
        outbound.send(give_up_report(agent_id, step_run_id, attempt));
        let _ = tx.send(());
    }
    n
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = Args::parse();
    // Held to the end of main: dropping it flushes whatever has not been exported.
    let _otel = otel::init(&args.name)?;

    if args.token.trim().is_empty() {
        error!("no agent token: set FIBER_AGENT_TOKEN (create one with `fiber agents create …`)");
        std::process::exit(2);
    }
    std::fs::create_dir_all(&args.workspace_dir)?;
    // Absolute from here on. The documented default is `./data/workspaces`, and a
    // relative path becomes the source of a `docker run -v` bind, which the daemon
    // refuses outright ("must be an absolute path"): docker mode did not work with the
    // documented default. Resolved once at startup rather than per step, so every
    // workspace path the agent logs, sweeps and mounts is the same one.
    match std::fs::canonicalize(&args.workspace_dir) {
        Ok(abs) => args.workspace_dir = abs,
        Err(e) => {
            error!(
                path = %args.workspace_dir.display(), error = %e,
                "cannot resolve the workspace directory"
            );
            std::process::exit(2);
        }
    }
    info!(workspace_dir = %args.workspace_dir.display(), "workspace root");
    // Anything left from a previous process (crash, kill -9) is nobody's to finish.
    sweep_stale_workspaces(&args.workspace_dir, args.workspace_ttl_hours).await;
    // One set for the life of the process: the sweep below excludes this boot id, and
    // every container started from here carries it.
    let container_labels = ContainerLabels::new(&args.name);
    if args.use_docker {
        sweep_orphaned_containers(&container_labels, &args.name).await;
    }
    let labels: Vec<String> = args
        .labels
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // SIGTERM / SIGINT → cancel in-flight steps, tell the server, then exit.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        warn!("shutdown signal received; cancelling in-flight steps");
        let _ = shutdown_tx.send(true);
    });

    let mut st = AgentState::new(args.concurrency, container_labels);
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = std::time::Instant::now();
        match run_session(&args, &labels, shutdown_rx.clone(), &mut st).await {
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
        // The socket is gone; the steps are not. They run on under their leases until
        // the next session renews them or the grace runs out. The outage is dated from
        // its first session loss, not from each failed reconnect.
        let now = tokio::time::Instant::now();
        let since = *st.disconnected_at.get_or_insert(now);
        arm_watchdog(&mut st, since);
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
                // No socket to say Goodbye on: stop the steps and let the leases expire.
                st.cancel_all_for_shutdown();
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                while st.steps_in_flight() > 0 && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
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

/// Owns the sink for one session: drains the outbox in order, sends heartbeats, and —
/// if asked — a final message followed by a Close frame.
async fn write_session(
    mut sink: futures_util::stream::SplitSink<Ws, Message>,
    outbound: Outbound,
    agent_id: Uuid,
    mut last: oneshot::Receiver<AgentMessage>,
) -> Result<()> {
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    // The first tick is immediate: a reconnect renews the leases straight after Hello.
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let hb = AgentMessage::Heartbeat { agent_id };
                sink.send(Message::Text(serde_json::to_string(&hb)?.into())).await?;
            }
            _ = outbound.wake.notified() => {
                flush_outbox(&mut sink, &outbound, agent_id).await?;
            }
            msg = &mut last => {
                if let Ok(msg) = msg {
                    // Whatever the steps said while stopping goes first.
                    flush_outbox(&mut sink, &outbound, agent_id).await?;
                    sink.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
                    let _ = sink.send(Message::Close(None)).await;
                }
                return Ok(());
            }
        }
    }
}

/// Send everything queued, oldest first. A message leaves the queue only after the
/// socket has taken it, so a writer aborted mid-send (the session ending underneath
/// it) leaves the message for the next session. The server may or may not have read
/// it by then, and a duplicate is what the server's own checks exist for (a second
/// completion is ignored, a second artifact replaces the first, a repeated line is one
/// line twice).
async fn flush_outbox(
    sink: &mut futures_util::stream::SplitSink<Ws, Message>,
    outbound: &Outbound,
    agent_id: Uuid,
) -> Result<()> {
    // Read before anything is built: the notice is a one-line batch, and against a
    // server that cannot parse one it would be rejected — on exactly the sessions the
    // fallback exists for, and exactly when the outbox has been dropping lines. It
    // would also raise the "upgrade the API before the agents" error on a session
    // whose real output is being delivered perfectly well as chunks.
    let batches = outbound.server_takes_batches();
    let dropped = outbound
        .outbox
        .lock()
        .map(|mut o| o.take_dropped())
        .unwrap_or_default();
    for (i, ((step_run_id, attempt), d)) in dropped.iter().enumerate() {
        // `at_seq` puts the notice where the gap is: readers order a step's log by
        // `seq`, not by when a row was stored.
        let data = format!(
            "{} log lines dropped here: the agent's outbound buffer was full \
             (the API was unreachable, or was not keeping up)",
            d.count
        );
        let notice = if batches {
            AgentMessage::LogBatch {
                agent_id,
                step_run_id: *step_run_id,
                attempt: *attempt,
                lines: vec![LogLineWire {
                    stream: "system".into(),
                    data,
                    seq: d.at_seq,
                }],
            }
        } else {
            AgentMessage::LogChunk {
                agent_id,
                step_run_id: *step_run_id,
                stream: "system".into(),
                data,
                seq: d.at_seq,
                attempt: *attempt,
            }
        };
        let frame = match serde_json::to_string(&notice) {
            Ok(f) => f,
            Err(_) => continue,
        };
        if sink.send(Message::Text(frame.into())).await.is_err() {
            // The counts are already out of the map, and a flapping reconnect — the
            // thing that produces the gaps — is exactly when this fails. Losing them
            // here leaves an unmarked hole in the log, so put back everything not yet
            // reported and let the next session say it.
            if let Ok(mut o) = outbound.outbox.lock() {
                o.restore_dropped(dropped.iter().skip(i).map(|(k, v)| (*k, *v)).collect());
            }
            bail!("sending a dropped-lines notice failed");
        }
    }
    loop {
        let next = outbound
            .outbox
            .lock()
            .ok()
            .and_then(|o| o.peek_front_frames(batches));
        let (token, frames) = match next {
            None => return Ok(()),
            Some((token, Ok(frames))) => (token, frames),
            Some((token, Err(_))) => {
                // Unserialisable: nothing the next session could do better with it.
                if let Ok(mut o) = outbound.outbox.lock() {
                    o.pop_sent(token);
                }
                continue;
            }
        };
        for frame in frames {
            sink.send(Message::Text(frame.into())).await?;
        }
        // By token, and only once every frame is away: the lock was released while the
        // socket took them, and a purge in that window would otherwise make this
        // discard a message never sent.
        if let Ok(mut o) = outbound.outbox.lock() {
            o.pop_sent(token);
        }
        // A step held back by `send_batch` is waiting on exactly this.
        outbound.drained.notify_waiters();
    }
}

/// Clears the per-session flags however `run_session` returns — an error, a close, or
/// a drop. `server_batches` is one of them: the next session may be a different
/// replica, and assuming the old one's capabilities is how a rolling deploy loses logs.
struct SessionGuard {
    connected: Arc<std::sync::atomic::AtomicBool>,
    server_batches: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.connected.store(false, Ordering::SeqCst);
        self.server_batches.store(false, Ordering::SeqCst);
    }
}

/// One WebSocket session. Steps started here outlive it: on close they carry on under
/// the state in `st`, and the next session picks them up.
async fn run_session(
    args: &Args,
    labels: &[String],
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    st: &mut AgentState,
) -> Result<()> {
    let _guard = SessionGuard {
        connected: Arc::clone(&st.outbound.connected),
        server_batches: Arc::clone(&st.outbound.server_batches),
    };
    let url = Url::parse(&format!("{}/ws/agent", args.api_url.trim_end_matches('/')))?;
    // The token goes in a header, not the query string: a URL ends up in proxy and server
    // access logs, and this token leases steps and receives project secrets. Servers older
    // than this still accept `?token=`, so an old server and a new agent do not connect —
    // upgrade the server first.
    let request = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
        url.as_str(),
    )
    .map(|mut r| {
        if let Ok(v) = format!("Bearer {}", args.token).parse() {
            r.headers_mut().insert(header::AUTHORIZATION, v);
        }
        r
    })
    .context("build websocket request")?;

    // Taken before the handshake: `Welcome` moves the anchor, and the give-up decision
    // below is about how long this agent went without hearing from any session.
    let anchor_before_session = st.last_server_frame();
    info!(api = %args.api_url, "connecting");
    let (ws, _) = connect_async(request).await.context("connect websocket")?;
    let (mut sink, mut stream) = ws.split();

    let mut agent_id = Uuid::nil();

    if let Some(Ok(Message::Text(text))) = stream.next().await {
        if let Ok(ServerMessage::Welcome {
            agent_id: id,
            lease_secs,
            protocol_version,
        }) = serde_json::from_str(&text)
        {
            agent_id = id;
            st.lease_secs = lease_secs;
            if let Ok(mut g) = st.agent_id.lock() {
                *g = agent_id;
            }
            // Absent, or older than the revision that introduced it: this server cannot
            // parse a batch, so the writer unpacks them for it.
            let batches = protocol_version.is_some_and(|v| v >= LOG_BATCH_PROTOCOL);
            st.outbound.server_batches.store(batches, Ordering::SeqCst);
            if !batches {
                warn!(
                    ?protocol_version,
                    "server does not accept batched logs; sending one line per message"
                );
            }
            st.note_server_frame();
            info!(%agent_id, ?lease_secs, ?protocol_version, "registered");
        }
    }

    let hello = AgentMessage::Hello {
        name: args.name.clone(),
        labels: labels.to_vec(),
        // The local semaphore already clamps; report the same number, or the server
        // would register an online agent that is never offered anything.
        concurrency: args.concurrency.max(1),
        protocol_version: fiber_proto::PROTOCOL_VERSION,
    };
    sink.send(Message::Text(serde_json::to_string(&hello)?.into()))
        .await?;
    // Reconnected: the steps from the last session keep their leases (the writer's
    // first heartbeat renews them) and their buffered output goes out now — unless the
    // outage outlasted the lease, in which case they are stopped here.
    st.on_session_established(anchor_before_session);

    let (last_tx, last_rx) = oneshot::channel::<AgentMessage>();
    let mut writer = tokio::spawn(write_session(sink, st.outbound.clone(), agent_id, last_rx));
    // A server that carries lease_secs also pings every 15 s; one that does not sends
    // nothing between offers, so silence means nothing there.
    let server_pings = st.lease_secs.is_some();
    let mut idle = tokio::time::Instant::now() + SERVER_IDLE_TIMEOUT;
    let grace = grace_after_disconnect(st.lease_secs, HEARTBEAT_INTERVAL);
    // Anything queued while there was no socket goes out first.
    st.outbound.wake.notify_one();

    loop {
        tokio::select! {
            res = &mut writer => {
                // The socket rejected a write; the read side ends on its own soon, but
                // there is no point waiting for it.
                match res {
                    Ok(Err(e)) => return Err(e).context("session write"),
                    _ => return Ok(()),
                }
            }
            _ = shutdown.changed() => {
                let n = st.steps_in_flight();
                warn!(in_flight = n, "shutting down: stopping in-flight steps; the server will requeue them");
                // Do not report terminal status: a restart must not fail the build. The
                // server is told Goodbye once the processes are gone, so it requeues the
                // steps now instead of when their leases expire.
                st.cancel_all_for_shutdown();
                // Wait until every step task has killed its process (bounded).
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                while st.steps_in_flight() > 0 && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                let _ = last_tx.send(AgentMessage::Goodbye { agent_id });
                if tokio::time::timeout(Duration::from_secs(2), &mut writer).await.is_err() {
                    writer.abort();
                }
                return Ok(());
            }
            _ = tokio::time::sleep_until(idle), if server_pings => {
                writer.abort();
                bail!(
                    "no frame from the server for {} s; treating the socket as lost",
                    SERVER_IDLE_TIMEOUT.as_secs()
                );
            }
            msg = stream.next() => {
                if matches!(msg, Some(Ok(_))) {
                    idle = tokio::time::Instant::now() + SERVER_IDLE_TIMEOUT;
                    // The server's loop is turning, so it is renewing leases too.
                    st.note_server_frame();
                }
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
                                traceparent,
                                working_directory,
                                shell,
                                attempt,
                            }) => {
                                info!(%step_id, %step_name, %run_id, ?attempt, "offered step");
                                // Anything still queued about this step_run_id is from an
                                // earlier attempt the server has moved past.
                                st.outbound.purge_step(step_run_id);
                                let _ = st.outbound.send(AgentMessage::Claim { agent_id, step_run_id });

                                let (cancel_tx, cancel_rx) = oneshot::channel();
                                if let Ok(mut g) = st.cancels.lock() {
                                    g.insert((step_run_id, attempt), cancel_tx);
                                }

                                let outbound = st.outbound.clone();
                                let prepared = Arc::clone(&st.prepared);
                                let cancels = Arc::clone(&st.cancels);
                                let abandoned = Arc::clone(&st.abandoned);
                                let workspace_dir = args.workspace_dir.clone();
                                let http_api = http_base(&args.api_url);
                                let token = args.token.clone();
                                let slots = Arc::clone(&st.slots);
                                let draining = Arc::clone(&st.draining);
                                let in_flight = Arc::clone(&st.in_flight);
                                // The attempt's clock starts now, not when a local permit frees up.
                                let offered_at = tokio::time::Instant::now();
                                let redactor = Redactor::new(&env, &secret_keys);
                                let exec = ExecConfig::from_args(args, st.container_labels.clone());
                                in_flight.fetch_add(1, Ordering::SeqCst);
                                let workspaces = Arc::clone(&st.workspaces);
                                tokio::spawn(async move {
                                    let _permit = slots.acquire_owned().await;
                                    let result = execute_step(
                                        &outbound,
                                        agent_id,
                                        step_run_id,
                                        attempt,
                                        grace,
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
                                        traceparent.as_deref(),
                                        working_directory.as_deref(),
                                        shell.as_deref(),
                                    ).await;

                                    if let Ok(mut g) = cancels.lock() {
                                        g.remove(&(step_run_id, attempt));
                                    }
                                    in_flight.fetch_sub(1, Ordering::SeqCst);
                                    if draining.load(Ordering::SeqCst) {
                                        return;
                                    }
                                    // Given up on (lease gone): the server would ignore
                                    // this report, or worse, apply it to a re-leased attempt.
                                    if abandoned
                                        .lock()
                                        .map(|mut a| a.remove(&(step_run_id, attempt)))
                                        .unwrap_or(false)
                                    {
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
                                            attempt,
                                        },
                                        Err(e) if e.to_string().contains("cancelled") => {
                                            AgentMessage::StepComplete {
                                                agent_id,
                                                step_run_id,
                                                status: StepStatus::Cancelled,
                                                exit_code: None,
                                                error: Some("cancelled".into()),
                                                attempt,
                                            }
                                        }
                                        Err(e) if e.to_string().starts_with("timed out") => {
                                            AgentMessage::StepComplete {
                                                agent_id,
                                                step_run_id,
                                                status: StepStatus::Failed,
                                                exit_code: None,
                                                error: Some(redactor.apply(&e.to_string())),
                                                attempt,
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
                                            attempt,
                                        },
                                    };
                                    let _ = outbound.send(complete);
                                });
                            }
                            Ok(ServerMessage::Cancel { step_run_id }) => {
                                warn!(%step_run_id, "cancel requested");
                                // The message names no attempt, so every attempt of the
                                // step running here is stopped — in practice one.
                                if let Ok(mut g) = st.cancels.lock() {
                                    let keys: Vec<StepAttemptKey> = g
                                        .keys()
                                        .filter(|(s, _)| *s == step_run_id)
                                        .copied()
                                        .collect();
                                    for k in keys {
                                        if let Some(tx) = g.remove(&k) {
                                            let _ = tx.send(());
                                        }
                                    }
                                }
                            }
                            Ok(ServerMessage::Error { message }) => {
                                if message == "invalid message" {
                                    // The message is already gone: the writer pops on a
                                    // successful `send`, not on an ack. Loud, because
                                    // the visible symptom is a green build with an
                                    // empty log and nothing pointing at the cause.
                                    error!(
                                        %message,
                                        "the server rejected a message it could not parse — \
                                         it is probably older than this agent; upgrade the \
                                         API before the agents"
                                    );
                                } else {
                                    warn!(%message, "server error");
                                }
                            }
                            Ok(ServerMessage::Welcome { .. }) => {}
                            Err(e) => warn!(error = %e, "bad server message"),
                        }
                    }
                    // The session is over; the steps are not. They keep running under
                    // their leases, their output queues in the outbox, and the next
                    // session — or the lease watchdog — decides what becomes of them.
                    Some(Ok(Message::Close(_))) | None => {
                        writer.abort();
                        return Ok(());
                    }
                    Some(Err(e)) => {
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
    out_tx: &Outbound,
    agent_id: Uuid,
    step_run_id: Uuid,
    attempt: Option<i32>,
    retry_budget: Duration,
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
    traceparent: Option<&str>,
    working_directory: Option<&str>,
    shell: Option<&str>,
) -> Result<i32> {
    let run_dir = workspace_root.join(run_id.to_string());
    let work_dir = run_dir.join(step_run_id.to_string());
    workspaces.enter(run_id);
    let span = tracing::info_span!(
        "fiber.step",
        run_id = %run_id,
        step_run_id = %step_run_id,
        kind = if image.is_some() { "docker" } else { "shell" },
        outcome = tracing::field::Empty,
    );
    // Join the server's trace when it sent one, so a run reads as one trace across both
    // processes instead of a server span and an unrelated agent root.
    otel::adopt_remote_parent(&span, traceparent);
    let result = execute_step_inner(
        out_tx,
        agent_id,
        step_run_id,
        attempt,
        retry_budget,
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
        working_directory,
        shell,
    )
    // `.instrument`, not `span.enter()`: a guard held across an await point attributes
    // whatever else the runtime schedules on this thread to this step.
    .instrument(span.clone())
    .await;
    // Cancel, timeout and prep failures all land here: a leaked checkout per aborted
    // attempt would fill the disk faster than successful runs do.
    cleanup_workspace(workspaces, run_id, &run_dir, &work_dir, prepared).await;
    // Measured from the offer, not from the first byte of output: queue-to-green is what
    // anyone waiting on a build actually experiences.
    span.record("outcome", otel::outcome_label(&result));
    otel::record_step(&result, offered_at.elapsed().as_secs_f64(), image.is_some());
    result
}

#[allow(clippy::too_many_arguments)]
async fn execute_step_inner(
    out_tx: &Outbound,
    agent_id: Uuid,
    step_run_id: Uuid,
    attempt: Option<i32>,
    retry_budget: Duration,
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
    working_directory: Option<&str>,
    shell: Option<&str>,
) -> Result<i32> {
    // One sequence for system/stdout/stderr so the server's `ORDER BY seq` interleaves
    // streams in emission order (the old per-stream bases collided after 1000 lines).
    let seq = Arc::new(AtomicU64::new(0));
    let log_seq = Arc::clone(&seq);
    let log_redactor = redactor.clone();
    // Filled while the step's pipes are being read, so the agent's own notes take the
    // same route as the step's output and reach the table in the order they happened.
    // Storage order is what every reader uses — `list_logs` pages by `id`, and the
    // cursor a follower resumes from is the last row of a page — so a note that
    // overtakes the output it describes is stored above it, and no amount of sorting
    // on read can fix that without breaking the cursor.
    let log_lines_tx: Arc<Mutex<Option<mpsc::Sender<RawLine>>>> = Arc::new(Mutex::new(None));
    let closure_tx = Arc::clone(&log_lines_tx);
    let mut log = |stream: &str, data: String| {
        let seq = log_seq.fetch_add(1, Ordering::Relaxed);
        // `try_send`, because this is a sync closure called from a dozen places and
        // some of them hold no runtime slot to yield. Full means ten thousand lines are
        // already queued, in which case going straight to the outbox can put this note
        // ahead of them — the alternative is losing it, and a note about a timeout or a
        // kill is the one line you want most.
        if let Ok(g) = closure_tx.lock()
            && let Some(tx) = g.as_ref()
            && tx
                .try_send(RawLine {
                    stream: system_stream(stream),
                    seq,
                    data: data.clone(),
                })
                .is_ok()
        {
            return;
        }
        // Before the pipes exist (workspace prep) and after they are drained: nothing
        // is buffered behind this, so straight to the outbox is in order by definition.
        //
        // Masked here and only here: everything that reaches the channel above is masked
        // by the batcher, and doing it in both places ran the matcher twice over every
        // system line the agent writes.
        let data = log_redactor.apply(&data);
        let _ = out_tx.send(AgentMessage::LogBatch {
            agent_id,
            step_run_id,
            attempt,
            lines: vec![LogLineWire {
                stream: stream.into(),
                data,
                seq,
            }],
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
                format!(
                    "preparing workspace from {} @ {}",
                    without_userinfo(&ws.repo),
                    ws.git_ref
                ),
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
            restore_artifacts(
                http_api,
                token,
                work_dir,
                restore,
                out_tx,
                retry_budget,
                &mut log,
            )
            .await?;
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
    // The server validated these and validated them again when building the offer. The
    // agent is the last place that can be wrong about them, and it is the one that would
    // actually leave the workspace, so it checks too.
    let subdir = working_directory
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .filter(|d| is_contained_relative_path(d));
    if working_directory.is_some() && subdir.is_none() {
        bail!("working_directory is not a path inside the workspace");
    }
    let shell_prog = shell
        .map(str::trim)
        .filter(|sh| !sh.is_empty())
        .filter(|sh| is_bare_program_name(sh))
        .unwrap_or("sh");
    if shell.is_some() && shell.map(str::trim) != Some(shell_prog) {
        bail!("shell must be a bare program name");
    }
    // This is the boundary: the value is about to become a `docker run` argument. The
    // server checked it at compile time, but a snapshot from an older server did not.
    if let Some(img) = image.map(str::trim)
        && !img.is_empty()
        && !fiber_proto::validate::image_reference_ok(img)
    {
        bail!("image `{img}` is not a docker image reference");
    }
    let host_cwd = match subdir {
        Some(d) => {
            let full = work_dir.join(d);
            // Belt and braces: resolve and confirm it is still under the workspace, which
            // catches a symlink the repository itself planted.
            let root = tokio::fs::canonicalize(work_dir)
                .await
                .unwrap_or_else(|_| work_dir.to_path_buf());
            match tokio::fs::canonicalize(&full).await {
                Ok(real) if real.starts_with(&root) => real,
                Ok(_) => bail!("working_directory resolves outside the workspace"),
                Err(e) => bail!("working_directory {d}: {e}"),
            }
        }
        None => work_dir.to_path_buf(),
    };
    if let Some(d) = subdir {
        log("system", format!("working directory: {d}"));
    }
    if shell_prog != "sh" {
        log("system", format!("shell: {shell_prog}"));
    }

    let mut env_file: Option<tempfile::NamedTempFile> = None;
    // Trimmed here as at compile time, so the value docker sees is the value that was
    // checked. Empty means "no image".
    let mut child = if let Some(img) = image
        .map(str::trim)
        .filter(|i| !i.is_empty())
        .filter(|_| exec.use_docker)
    {
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
            "--label",
            &exec.container_labels.agent,
            "--label",
            &exec.container_labels.boot,
            "-v",
            &mount,
        ]);
        let container_cwd = match subdir {
            Some(d) => format!("/workspace/{d}"),
            None => "/workspace".to_string(),
        };
        cmd.args([
            "-w",
            &container_cwd,
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
        // `--` ends docker's own options, so whatever the image string is, it is an image.
        // No stdin: a step that waits on a terminal should fail now, not at the timeout.
        cmd.args(["--", img, shell_prog, "-c", run])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        put_in_own_process_group(&mut cmd);
        cmd.spawn().context("spawn docker")?
    } else {
        log(
            "system",
            format!("running on host shell in {}", host_cwd.display()),
        );
        let mut cmd = Command::new(shell_prog);
        // Not `-lc`, for the same reason as the container: /etc/profile would overwrite
        // the environment assembled just below, PATH included.
        cmd.args(["-c", run])
            .current_dir(&host_cwd)
            .stdin(Stdio::null())
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

    // Both pipes feed one bounded channel, and one batcher turns it into `LogBatch`
    // messages. The channel is where a runaway step is made to wait; the batcher is what
    // keeps the server's ingest off one insert and one publish per line.
    let (line_tx, line_rx) = mpsc::channel::<RawLine>(LINE_CHANNEL_CAP);
    let batcher = tokio::spawn(batch_lines(
        line_rx,
        out_tx.clone(),
        agent_id,
        step_run_id,
        attempt,
        redactor.clone(),
    ));
    let out_handle = tokio::spawn(pump_lines(
        stdout,
        "stdout",
        Arc::clone(&seq),
        line_tx.clone(),
    ));
    let err_handle = tokio::spawn(pump_lines(
        stderr,
        "stderr",
        Arc::clone(&seq),
        line_tx.clone(),
    ));
    // From here the step task's own notes go down the same channel as the output, so
    // they are stored between the lines they were written between. `line_tx` itself is
    // dropped: the slot's clone and the pumps' are the only senders, and the slot has
    // to be cleared before the drain or the batcher never sees the channel close.
    if let Ok(mut g) = log_lines_tx.lock() {
        *g = Some(line_tx);
    }
    let stop_logging = || {
        if let Ok(mut g) = log_lines_tx.lock() {
            g.take();
        }
    };

    // Both pumps, then the batcher: the pumps hold the only senders, so awaiting them
    // closes the channel and the batcher flushes whatever is left and returns. Every
    // line the step wrote is queued before anything below reports on the step.
    /// Wait for the pipes to end and the batcher to flush, at most `budget`.
    ///
    /// Unbounded on a normal exit: the child is gone, so the output is finite and all
    /// of it belongs in the log. Bounded after a kill: the batcher can wait
    /// `LOG_BACKPRESSURE_MAX` per flush for room in the outbox, and a socket that is up
    /// but not draining turns ten thousand buffered lines into minutes of waiting —
    /// with the concurrency permit still held and the run still saying "cancelling".
    async fn drain(
        out: tokio::task::JoinHandle<()>,
        err: tokio::task::JoinHandle<()>,
        batcher: tokio::task::JoinHandle<()>,
        budget: Option<Duration>,
    ) -> bool {
        let all = async {
            let _ = out.await;
            let _ = err.await;
            let _ = batcher.await;
        };
        match budget {
            None => {
                all.await;
                true
            }
            Some(b) => tokio::time::timeout(b, all).await.is_ok(),
        }
    }
    let code = tokio::select! {
        status = child.wait() => {
            stop_logging();
            // Drained before the `?`: an error from `wait` must not enqueue the step's
            // completion ahead of the output that explains it. Bounded, generously: a
            // socket that is up but not draining could otherwise hold the concurrency
            // permit for as long as the buffered output takes, which at
            // `LOG_BACKPRESSURE_MAX` per flush is minutes after the child is gone.
            let flushed = drain(out_handle, err_handle, batcher, Some(EXIT_DRAIN_BUDGET)).await;
            let code = status?.code().unwrap_or(1);
            if !flushed {
                log("system", format!(
                    "log buffer not flushed within {}s of the step exiting; the rest of this step's output was dropped",
                    EXIT_DRAIN_BUDGET.as_secs()
                ));
            }
            code
        }
        _ = &mut cancel => {
            // Said before the kill, so it is worth reading; the last few milliseconds of
            // the step's own output may still be in a batch behind it.
            log("system", "killing step process".into());
            stop_logging();
            kill_step(&mut child, docker_container.as_deref()).await;
            if !drain(out_handle, err_handle, batcher, Some(KILL_DRAIN_BUDGET)).await {
                log("system", "log buffer not flushed within the cancel deadline; the rest of this step's output was dropped".into());
            }
            bail!("step cancelled");
        }
        _ = sleep_until_opt(deadline) => {
            let msg = timed_out_msg();
            log("system", format!("{msg}; killing step process"));
            stop_logging();
            kill_step(&mut child, docker_container.as_deref()).await;
            if !drain(out_handle, err_handle, batcher, Some(KILL_DRAIN_BUDGET)).await {
                log("system", "log buffer not flushed within the timeout deadline; the rest of this step's output was dropped".into());
            }
            bail!("{msg}");
        }
    };

    // A declared artifact that never reached storage must not leave the step green: a
    // dependent step restores it and would fail later with a missing file instead.
    let mut artifact_failures = Vec::new();
    if code == 0 && !artifacts.is_empty() {
        // Cancellable like the prep phase: an upload retrying through an outage would
        // otherwise hold the concurrency permit — and keep the step counted as in
        // flight — long after the step was given up.
        let cancelled = tokio::select! {
            failures = upload_artifacts(
                http_api,
                token,
                step_run_id,
                attempt,
                work_dir,
                artifacts,
                out_tx,
                retry_budget,
                &mut log,
            ) => {
                artifact_failures = failures;
                false
            }
            _ = &mut cancel => true,
        };
        if cancelled {
            log("system", "step cancelled during artifact upload".into());
            bail!("step cancelled");
        }
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

/// A workspace-relative path that cannot climb out of it. The server checks this twice
/// already; this is the copy that guards the process actually being spawned.
fn is_contained_relative_path(p: &str) -> bool {
    let p = p.trim();
    !p.is_empty()
        && !p.starts_with('/')
        && !p.starts_with('\\')
        && !p.contains(':')
        && !p.split(['/', '\\']).any(|seg| seg == "..")
}

/// A bare program name, not a command line: `bash`, never `/bin/bash` or `bash -e`.
fn is_bare_program_name(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && s.len() <= 32
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        && s != "."
        && s != ".."
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
    link: &Outbound,
    retry_budget: Duration,
    log: &mut impl FnMut(&str, String),
) -> Result<()> {
    let client = artifact_client()?;
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
        let what = format!("restore {}", art.name);
        let bytes = match with_api_retry(link, retry_budget, &what, log, || {
            fetch_artifact(client.clone(), url.clone(), token.to_string())
        })
        .await
        {
            Ok(b) => b,
            Err(why) => {
                let msg = format!("RESTORE FAILED {}: {why}", art.name);
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

/// One download of a prior artifact. A failure on the direct URL is usually the 307 to
/// object storage, not the API itself: the presigned URL names the storage endpoint as
/// the outside world reaches it. Ask the API for the bytes instead, the same way the
/// upload falls back.
async fn fetch_artifact(
    client: reqwest::Client,
    url: String,
    token: String,
) -> Result<bytes::Bytes, ApiFailure> {
    let resp = match client.get(&url).bearer_auth(&token).send().await {
        Ok(r) => r,
        Err(_) => client
            .get(format!("{url}?via=api"))
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| ApiFailure::Transient(format!("network error: {e}")))?,
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(200).collect();
        let msg = if snippet.is_empty() {
            format!("HTTP {status}")
        } else {
            format!("HTTP {status} — {snippet}")
        };
        return Err(ApiFailure::from_status(status, msg));
    }
    resp.bytes()
        .await
        .map_err(|e| ApiFailure::Transient(format!("read body: {e}")))
}

/// Whether a failed call gets another go: only a transient failure, and only while the
/// budget has time left in it. A `4xx` is a definite answer and retrying it would spend
/// the whole grace to arrive at the same place.
fn retry_again(transient: bool, elapsed: Duration, budget: Duration) -> bool {
    transient && elapsed < budget
}

/// Why a call to the API failed, and whether it is worth another try.
#[derive(Debug)]
enum ApiFailure {
    /// The API was unreachable or answered 5xx: the kind of failure an outage causes.
    Transient(String),
    /// A definite answer (4xx, a malformed body): trying again changes nothing.
    Fatal(String),
}

impl ApiFailure {
    fn from_status(status: reqwest::StatusCode, msg: String) -> Self {
        if status.is_server_error() {
            ApiFailure::Transient(msg)
        } else {
            ApiFailure::Fatal(msg)
        }
    }

    fn message(&self) -> &str {
        match self {
            ApiFailure::Transient(m) | ApiFailure::Fatal(m) => m,
        }
    }

    fn is_transient(&self) -> bool {
        matches!(self, ApiFailure::Transient(_))
    }
}

/// Retry `op` on transient failures for up to `budget` from the first one — the same
/// grace the step's lease allows, since a step that cannot reach the API is usually one
/// whose agent is in the middle of an outage, and failing it would spend the run on the
/// deploy that caused it. Without a session established the wait is a short poll for
/// one (the socket and the HTTP path fail together); with one, an exponential backoff.
async fn with_api_retry<T, F, Fut>(
    link: &Outbound,
    budget: Duration,
    what: &str,
    log: &mut impl FnMut(&str, String),
    mut op: F,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ApiFailure>>,
{
    let started = tokio::time::Instant::now();
    let mut backoff = Duration::from_secs(1);
    let mut announced = false;
    loop {
        // Each try is also bounded by what is left of the budget, so a request the
        // client's own timeout would let run to 120 s cannot overrun the grace. A zero
        // budget is not "no time to call": it is a server that named no lease, so there
        // is nothing to overrun. Bounding the try by it would fail every artifact
        // transfer against an older API without putting a request on the wire.
        let remaining = budget.saturating_sub(started.elapsed());
        let failure = if remaining.is_zero() {
            match op().await {
                Ok(v) => return Ok(v),
                Err(f) => f,
            }
        } else {
            match tokio::time::timeout(remaining, op()).await {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(f)) => f,
                Err(_) => {
                    ApiFailure::Transient(format!("no answer within {} s", remaining.as_secs()))
                }
            }
        };
        let elapsed = started.elapsed();
        if !retry_again(failure.is_transient(), elapsed, budget) {
            if failure.is_transient() {
                return Err(format!(
                    "{} (gave up after {} s of retries)",
                    failure.message(),
                    elapsed.as_secs()
                ));
            }
            return Err(failure.message().to_string());
        }
        if !announced {
            announced = true;
            log(
                "system",
                format!(
                    "{what}: {}; retrying for up to {} s",
                    failure.message(),
                    budget.as_secs()
                ),
            );
        }
        let wait = if link.is_connected() {
            backoff
        } else {
            Duration::from_secs(1)
        };
        tokio::time::sleep(wait.min(budget - elapsed)).await;
        backoff = (backoff * 2).min(Duration::from_secs(15));
    }
}

/// The server's explanation for a refused request, as a short suffix for a step's log.
///
/// Bounded and single-line: this ends up in `log_lines` and in the step's `error`, and an
/// error page or a stack trace pasted there helps nobody.
async fn error_detail(resp: reqwest::Response) -> String {
    let body = resp.text().await.unwrap_or_default();
    let trimmed: String = body
        .trim()
        .chars()
        .filter(|c| *c != '\n' && *c != '\r')
        .take(300)
        .collect();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(" ({trimmed})")
    }
}

/// The plain components of a declared artifact path, or `None` when it is not a simple
/// relative path.
///
/// `..`, an absolute path and a Windows prefix are all refused here rather than by a
/// substring check: `work_dir.join("/etc/passwd")` discards the workspace entirely, so
/// an absolute declaration read a file outside the checkout and uploaded it.
fn artifact_components(rel: &str) -> Option<Vec<std::ffi::OsString>> {
    let mut out = Vec::new();
    for c in Path::new(rel).components() {
        match c {
            std::path::Component::Normal(n) => out.push(n.to_os_string()),
            _ => return None,
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The first path element of `rel` under `work_dir` that is a symlink, if any.
///
/// Every component is checked, not just the leaf: `out -> /` with `artifacts: [out/etc/shadow]`
/// is the same escape one level up. A component that does not exist ends the walk — the
/// caller's own metadata call reports it as missing.
async fn symlink_in(work_dir: &Path, rel: &str) -> Option<PathBuf> {
    let mut cur = work_dir.to_path_buf();
    for part in artifact_components(rel)? {
        cur.push(part);
        match tokio::fs::symlink_metadata(&cur).await {
            Ok(m) if m.is_symlink() => return Some(cur),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

/// Leaf name for the `tar czf …` suggestion on a directory artifact.
fn artifact_archive_hint(rel: &str) -> String {
    Path::new(rel)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("artifact")
        .to_string()
}

/// Upload each declared artifact, returning the ones that could not be stored.
///
/// A path that does not exist is a warning, not a failure: a pipeline may legitimately
/// declare an artifact its step only sometimes produces. Anything that exists but could
/// not be stored — unreadable, over the size cap, or a failed transfer — is returned, and
/// the caller fails the step.
#[allow(clippy::too_many_arguments)]
async fn upload_artifacts(
    http_api: &str,
    token: &str,
    step_run_id: Uuid,
    attempt: Option<i32>,
    work_dir: &Path,
    artifacts: &[String],
    link: &Outbound,
    retry_budget: Duration,
    log: &mut impl FnMut(&str, String),
) -> Vec<String> {
    let mut failures = Vec::new();
    let client = match artifact_client() {
        Ok(c) => c,
        Err(e) => return vec![format!("http client: {e}")],
    };
    let proxy_url = format!("{http_api}/api/agent/steps/{step_run_id}/artifacts");
    let presign_url = format!("{http_api}/api/agent/steps/{step_run_id}/artifacts/presign");
    let complete_url = format!("{http_api}/api/agent/steps/{step_run_id}/artifacts/complete");
    for rel in artifacts {
        let rel = rel.trim();
        if artifact_components(rel).is_none() {
            let msg = format!("unsafe artifact path: {rel}");
            log("system", msg.clone());
            failures.push(msg);
            continue;
        }
        let path = work_dir.join(rel);
        // Checked before the open, on every component. A repository controls its own
        // working tree, so `out/build.log -> ~/.ssh/id_rsa` (or `out -> /`) is a file the
        // step never produced being uploaded to a store every project reader can read.
        if let Some(link) = symlink_in(work_dir, rel).await {
            let msg = format!(
                "artifact {rel} is or is under a symlink ({}); refusing to upload it",
                link.display()
            );
            log("system", msg.clone());
            failures.push(msg);
            continue;
        }
        match tokio::fs::symlink_metadata(&path).await {
            Ok(meta) if meta.is_file() => {
                // Opened `O_NOFOLLOW`, and judged on the handle rather than on a second
                // stat of the path. The check above and the read are two syscalls apart,
                // and a step can leave a process running behind it (the group is only
                // killed on cancel or timeout), so the regular file that was checked can
                // be a symlink to someone's key by the time it is opened. `O_NOFOLLOW`
                // refuses that outright, and the size below is the size of the thing
                // actually being read.
                let opened = tokio::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)
                    .await;
                let mut file = match opened {
                    Ok(f) => f,
                    Err(e) => {
                        let msg = format!("failed to open artifact {rel}: {e}");
                        log("system", msg.clone());
                        failures.push(msg);
                        continue;
                    }
                };
                let meta = match file.metadata().await {
                    Ok(m) if m.is_file() => m,
                    Ok(_) => {
                        let msg =
                            format!("artifact {rel} is not a regular file; refusing to upload it");
                        log("system", msg.clone());
                        failures.push(msg);
                        continue;
                    }
                    Err(e) => {
                        let msg = format!("failed to stat artifact {rel}: {e}");
                        log("system", msg.clone());
                        failures.push(msg);
                        continue;
                    }
                };
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
                let mut buf = Vec::with_capacity(meta.len() as usize);
                match file.read_to_end(&mut buf).await.map(|_| buf) {
                    Ok(bytes) => {
                        log(
                            "system",
                            format!("uploading artifact {rel} ({} bytes)", bytes.len()),
                        );
                        let bytes = bytes::Bytes::from(bytes);
                        let what = format!("upload {rel}");
                        match with_api_retry(link, retry_budget, &what, log, || {
                            upload_one_artifact(
                                client.clone(),
                                token.to_string(),
                                presign_url.clone(),
                                complete_url.clone(),
                                proxy_url.clone(),
                                rel.to_string(),
                                attempt,
                                bytes.clone(),
                            )
                        })
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
            // A directory used to be skipped with a note, leaving the step green and the
            // artifact absent — the dependent step then failed at restore time, or worse,
            // ran without it. `artifacts: [dist/]` is a mistake worth reporting where it
            // was made. Archive the tree into one file in the step instead:
            // `tar czf dist.tgz dist` and declare `dist.tgz`.
            Ok(meta) if meta.is_dir() => {
                let msg = format!(
                    "artifact {rel} is a directory; declare the files individually or \
                     archive it first (tar czf {}.tgz {rel})",
                    artifact_archive_hint(rel)
                );
                log("system", msg.clone());
                failures.push(msg);
            }
            Ok(_) => {
                let msg = format!("artifact {rel} is not a regular file; refusing to upload it");
                log("system", msg.clone());
                failures.push(msg);
            }
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
#[allow(clippy::too_many_arguments)]
async fn upload_one_artifact(
    client: reqwest::Client,
    token: String,
    presign_url: String,
    complete_url: String,
    proxy_url: String,
    rel: String,
    attempt: Option<i32>,
    bytes: bytes::Bytes,
) -> Result<String, ApiFailure> {
    let size = bytes.len() as u64;
    let mode = match client
        .post(&presign_url)
        .bearer_auth(&token)
        .json(&serde_json::json!({ "path": rel, "size": size, "attempt": attempt }))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp
            .json::<serde_json::Value>()
            .await
            .map_err(|e| ApiFailure::Fatal(format!("presign parse {rel}: {e}")))?,
        Ok(resp) => {
            // The body carries the server's reason — an artifact cap, an unusable path,
            // an attempt that has moved on. Without it the step's only clue is
            // "HTTP 400", and the operator has to read the API's log to learn why.
            let status = resp.status();
            let why = error_detail(resp).await;
            return Err(ApiFailure::from_status(
                status,
                format!("presign {rel} failed: HTTP {status}{why}"),
            ));
        }
        Err(e) => return Err(ApiFailure::Transient(format!("presign {rel} failed: {e}"))),
    };

    if mode.get("mode").and_then(|v| v.as_str()) == Some("presign") {
        let upload_url = mode
            .get("upload_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ApiFailure::Fatal(format!("presign {rel}: missing upload_url")))?;
        let stored_path = mode
            .get("stored_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ApiFailure::Fatal(format!("presign {rel}: missing stored_path")))?;
        // Cheap to clone: `Bytes` is refcounted, so the fallback costs no second copy.
        let unreachable = match client.put(upload_url).body(bytes.clone()).send().await {
            Ok(put) if put.status().is_success() => None,
            Ok(put) => Some(format!("HTTP {}", put.status())),
            Err(e) => Some(e.to_string()),
        };
        if let Some(why) = unreachable {
            let via = upload_via_proxy(&client, &token, &proxy_url, &rel, attempt, bytes).await?;
            return Ok(format!("{via} (presigned upload unreachable: {why})"));
        }
        let done = client
            .post(&complete_url)
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "path": rel,
                "size": size,
                "stored_path": stored_path,
                "attempt": attempt,
            }))
            .send()
            .await
            .map_err(|e| ApiFailure::Transient(format!("complete {rel}: {e}")))?;
        if !done.status().is_success() {
            return Err(ApiFailure::from_status(
                done.status(),
                format!("complete {rel} failed: HTTP {}", done.status()),
            ));
        }
        return Ok("presign".into());
    }

    // Local backend, or object storage the API would rather proxy for.
    upload_via_proxy(&client, &token, &proxy_url, &rel, attempt, bytes).await
}

/// Send the bytes through the API, which stores them with whatever backend it has.
async fn upload_via_proxy(
    client: &reqwest::Client,
    token: &str,
    proxy_url: &str,
    rel: &str,
    attempt: Option<i32>,
    bytes: bytes::Bytes,
) -> Result<String, ApiFailure> {
    let mut req = client
        .put(proxy_url)
        .bearer_auth(token)
        .header("X-Fiber-Artifact-Path", rel);
    // Same contract as the `attempt` on the WebSocket messages: the server drops an
    // upload for an attempt its row has moved past.
    if let Some(a) = attempt {
        req = req.header("X-Fiber-Attempt", a.to_string());
    }
    let resp = req
        .body(bytes)
        .send()
        .await
        .map_err(|e| ApiFailure::Transient(format!("upload {rel} failed: {e}")))?;
    if resp.status().is_success() {
        Ok("proxy".into())
    } else {
        // The body names the cap and its limit; without it a refused upload reads as a
        // bare `HTTP 400` and the reason is only in the server log.
        let status = resp.status();
        let why = error_detail(resp).await;
        Err(ApiFailure::from_status(
            status,
            format!("upload {rel} failed: HTTP {status}{why}"),
        ))
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
    // This is the boundary: the value is about to become a `git remote add` argument, on
    // the host, before any container exists. `ext::<command>` would run the command.
    // Trimmed, so the value checked is the value git gets.
    let repo = ws.repo.trim();
    if !fiber_proto::validate::repo_url_ok(repo) {
        bail!("workspace repo is not a fetchable URL");
    }
    log(
        "system",
        format!("fetching {} @ {target}", without_userinfo(repo)),
    );
    run_git(reference, &["init", "--quiet"], log).await?;
    run_git(reference, &["remote", "add", "origin", repo], log).await?;

    // Depth 50 keeps the fetch small while leaving room to check out a commit slightly
    // behind the ref tip (a push that lands while the run is queued).
    // `--` so a ref or sha can never be read as a git option.
    run_git(
        reference,
        &["fetch", "--depth", "50", "origin", "--", &ws.git_ref],
        log,
    )
    .await
    // This string is persisted as the step's error and shown in the UI: no credential.
    .with_context(|| format!("fetch {} from {}", ws.git_ref, without_userinfo(&ws.repo)))?;

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
    let dest = work_dir.to_string_lossy();
    // Through `run_git` like every other invocation: no stdin, no prompt, pinned
    // transports. A local clone cannot prompt, but the rule is easier to keep than the
    // exception.
    // From the agent's own directory, as before: `src` and `dest` may be relative to it.
    run_git(
        Path::new("."),
        &["clone", "--quiet", "--depth", "1", &src, &dest],
        log,
    )
    .await
    .context("could not create the step workspace from the run's clone")
}

/// Delete run workspaces — and orphaned step env files — left behind by a crash.
/// The default `FIBER_AGENT_NAME`. Two agents left on it share a label, which is the one
/// configuration where the sweep below must not run.
const DEFAULT_AGENT_NAME: &str = "local";

/// The labels every step container of this agent carries: which agent started it, and
/// which *process* of that agent.
///
/// Two labels because the sweep has to tell "a container my previous process left behind"
/// from "a container another live agent is running". The boot id answers the second
/// question for one agent restarting; the name keeps two differently-named agents on one
/// host out of each other's way entirely.
#[derive(Clone)]
struct ContainerLabels {
    agent: String,
    boot: String,
}

impl ContainerLabels {
    fn new(agent_name: &str) -> Self {
        Self {
            agent: format!("fiber.agent={}", label_safe(agent_name)),
            boot: format!("fiber.boot={}", Uuid::new_v4()),
        }
    }
}

/// An agent name as a docker label value. The name comes from configuration rather than
/// from a pipeline, but this is still the one place it becomes an argv token.
fn label_safe(agent_name: &str) -> String {
    let safe: String = agent_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    if safe.is_empty() {
        "agent".to_string()
    } else {
        safe
    }
}

/// Whether the startup sweep may run for this agent name.
///
/// Not on the unmodified default. `make agent` and the documented quickstart both leave
/// `FIBER_AGENT_NAME` at `local`, so two agents on one host share a label — and a sweep
/// keyed on that label would `docker rm -f` the other's *running* step containers,
/// killing live builds. Naming an agent is the cheap half of the fix and the thing an
/// operator running two of them has to do anyway.
fn sweep_allowed(agent_name: &str) -> bool {
    agent_name.trim() != DEFAULT_AGENT_NAME && !agent_name.trim().is_empty()
}

/// Container ids to remove, from `docker ps` rows of `(id, boot label)`.
///
/// Only containers from another boot: whatever this process started is by definition
/// still running, and a row with no boot label predates the label and is treated as
/// another boot's, which is what it is.
fn containers_to_sweep(rows: &[(String, String)], my_boot: &str) -> Vec<String> {
    rows.iter()
        .filter(|(_, boot)| boot != my_boot)
        .map(|(id, _)| id.clone())
        .collect()
}

/// Parse `docker ps --format '{{.ID}} {{.Label "fiber.boot"}}'` output into rows.
///
/// Separated from the process call so the parsing is testable: a blank line handed to
/// `docker rm -f` is an argument error that would abort the whole sweep.
fn container_rows(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let id = parts.next()?;
            // A container id is hex and at least a short id long. Anything else on this
            // stream is prose ("Cannot connect to the Docker daemon"), and this list is
            // about to be handed to `docker rm -f`.
            if id.len() < 12 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            Some((id.to_string(), parts.next().unwrap_or_default().to_string()))
        })
        .collect()
}

/// Remove step containers an earlier process of *this* agent left running.
///
/// A `kill -9` (or an OOM-killed agent) leaves `docker run --rm` children alive: the
/// container keeps the step's workspace bind-mounted and its `--env-file` secrets in the
/// process environment, and nothing ever reaps it — the step is reclaimed and re-run
/// somewhere else while the orphan holds CPU, memory and the credentials.
async fn sweep_orphaned_containers(labels: &ContainerLabels, agent_name: &str) {
    if !sweep_allowed(agent_name) {
        info!(
            "not sweeping orphaned step containers: FIBER_AGENT_NAME is the default \
             `{DEFAULT_AGENT_NAME}`, and two agents on one host would then share a label. \
             Give this agent a name to enable the sweep."
        );
        return;
    }
    let out = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label={}", labels.agent),
            "--format",
            "{{.ID}} {{.Label \"fiber.boot\"}}",
        ])
        .output()
        .await;
    let Ok(out) = out else {
        // No docker client, or no daemon: `use_docker` steps will fail with their own
        // message; a missing sweep is not worth a startup failure.
        return;
    };
    if !out.status.success() {
        return;
    }
    let rows = container_rows(&String::from_utf8_lossy(out.stdout.as_slice()));
    let boot_value = labels.boot.trim_start_matches("fiber.boot=");
    let ids = containers_to_sweep(&rows, boot_value);
    if ids.is_empty() {
        return;
    }
    warn!(
        containers = ids.len(),
        "removing step containers left by a previous agent process"
    );
    let mut cmd = Command::new("docker");
    cmd.args(["rm", "-f"]);
    cmd.args(&ids);
    let _ = cmd.output().await;
}

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

/// Transports git may use here. `file` is the per-step clone from the run's reference
/// copy; the rest are what a `workspace.repo` can name. Anything else — `ext::`, which
/// runs a command, above all — is refused by git itself, whatever the URL says.
const GIT_ALLOW_PROTOCOL: &str = "file:git:http:https:ssh";

async fn run_git(cwd: &Path, args: &[&str], log: &mut impl FnMut(&str, String)) -> Result<()> {
    // The remote URL may carry a token; the log line must not.
    let shown: Vec<String> = args.iter().map(|a| without_userinfo(a)).collect();
    log("system", format!("git {}", shown.join(" ")));
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        // A prompt for credentials or a host key would hang until the step timeout when
        // the agent has a terminal, and fail at once when it does not. Make it the latter.
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    // An operator who set this deliberately keeps their setting.
    if std::env::var_os("GIT_ALLOW_PROTOCOL").is_none() {
        cmd.env("GIT_ALLOW_PROTOCOL", GIT_ALLOW_PROTOCOL);
    }
    let output = cmd.output().await.context("git")?;
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

/// `scheme://user:secret@host/...` with the userinfo replaced, for log lines. Anything
/// without a `://` userinfo is returned as written.
fn without_userinfo(s: &str) -> String {
    let Some((scheme, rest)) = s.split_once("://") else {
        return s.to_string();
    };
    // Userinfo lives in the authority, which ends at the first `/`. The *last* `@` in it
    // is the separator: a password may itself contain `@`, and the tail of one is still
    // a secret.
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let Some(at) = rest[..authority_end].rfind('@') else {
        return s.to_string();
    };
    format!("{scheme}://***@{}", &rest[at + 1..])
}

#[allow(dead_code)]
fn _ws_ty(_: Ws) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_batch(step: Uuid, seqs: &[u64]) -> AgentMessage {
        AgentMessage::LogBatch {
            agent_id: Uuid::nil(),
            step_run_id: step,
            attempt: Some(1),
            lines: seqs
                .iter()
                .map(|seq| LogLineWire {
                    stream: "stdout".into(),
                    data: format!("line {seq}"),
                    seq: *seq,
                })
                .collect(),
        }
    }

    #[test]
    fn a_normal_line_loses_its_newline_and_nothing_else() {
        assert_eq!(truncate_line(b"hello world\n"), "hello world");
        assert_eq!(truncate_line(b"crlf\r\n"), "crlf");
        // EOF without a trailing newline is still a line.
        assert_eq!(truncate_line(b"no newline"), "no newline");
        assert_eq!(truncate_line(b"\n"), "");
    }

    #[test]
    fn invalid_utf8_is_decoded_lossily_rather_than_dropped() {
        // `ls` of a Latin-1 filename. The old reader took this as end-of-stream and the
        // rest of the step's output was never seen.
        let raw = b"caf\xe9 \xff\xfe latin\n";
        let out = truncate_line(raw);
        assert!(out.starts_with("caf"), "{out}");
        assert!(out.ends_with(" latin"), "{out}");
        assert!(
            out.contains('\u{fffd}'),
            "invalid bytes become U+FFFD: {out}"
        );
    }

    #[test]
    fn a_line_with_no_newline_is_cut_at_the_cap_with_a_marker() {
        let raw = vec![b'x'; 1024 * 1024];
        let out = truncate_line(&raw);
        assert!(
            out.ends_with(TRUNCATION_MARKER),
            "{}",
            &out[out.len() - 40..]
        );
        assert_eq!(out.len(), MAX_LOG_LINE_BYTES);
    }

    #[test]
    fn a_full_length_line_that_did_end_is_not_marked_truncated() {
        // Exactly the raw cap, newline included: complete, so no marker.
        let mut raw = vec![b'x'; MAX_RAW_LINE_BYTES - 1];
        raw.push(b'\n');
        let out = truncate_line(&raw);
        assert!(!out.contains(TRUNCATION_MARKER));
        assert_eq!(out.len(), MAX_RAW_LINE_BYTES - 1);
    }

    #[tokio::test]
    async fn the_reader_keeps_going_past_a_line_it_cannot_decode() {
        let input: Vec<u8> = b"first\ncaf\xe9\nthird\n".to_vec();
        let (tx, mut rx) = mpsc::channel(16);
        let seq = Arc::new(AtomicU64::new(0));
        pump_lines(&input[..], "stdout", seq, tx).await;
        let mut got = Vec::new();
        while let Ok(l) = rx.try_recv() {
            got.push(l);
        }
        assert_eq!(
            got.len(),
            3,
            "the bad line must not end the stream: {got:?}"
        );
        assert_eq!(got[0].data, "first");
        assert_eq!(got[2].data, "third");
        // seq is assigned where the line is read, and counts up.
        assert_eq!(got.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn a_megabyte_without_a_newline_becomes_one_capped_line() {
        let mut input = vec![b'x'; 1024 * 1024];
        input.extend_from_slice(b"\nafter\n");
        let (tx, mut rx) = mpsc::channel(16);
        pump_lines(&input[..], "stdout", Arc::new(AtomicU64::new(0)), tx).await;
        let mut got = Vec::new();
        while let Ok(l) = rx.try_recv() {
            got.push(l);
        }
        // One line for the oversized one (the rest of it discarded), then the next line
        // — reading has to resynchronise on the newline or everything after is garbage.
        assert_eq!(
            got.len(),
            2,
            "{:?}",
            got.iter().map(|l| l.data.len()).collect::<Vec<_>>()
        );
        assert_eq!(got[0].data.len(), MAX_LOG_LINE_BYTES);
        assert!(got[0].data.ends_with(TRUNCATION_MARKER));
        assert_eq!(got[1].data, "after");
    }

    #[test]
    fn a_batch_flushes_on_lines_bytes_or_time_and_never_when_empty() {
        let none = Duration::from_millis(0);
        assert!(!should_flush(0, 0, Duration::from_secs(60)));
        assert!(!should_flush(1, 10, none));
        assert!(should_flush(LOG_FLUSH_LINES, 10, none));
        assert!(should_flush(1, LOG_FLUSH_BYTES, none));
        assert!(should_flush(1, 10, LOG_FLUSH_INTERVAL));
        // Just under each threshold is not a flush.
        assert!(!should_flush(
            LOG_FLUSH_LINES - 1,
            LOG_FLUSH_BYTES - 1,
            LOG_FLUSH_INTERVAL - Duration::from_millis(1)
        ));
    }

    #[tokio::test]
    async fn the_batcher_preserves_order_and_sends_everything_before_it_exits() {
        let out = Outbound::new();
        let step = Uuid::new_v4();
        let (tx, rx) = mpsc::channel(64);
        let handle = tokio::spawn(batch_lines(
            rx,
            out.clone(),
            Uuid::nil(),
            step,
            Some(3),
            Redactor::new(&[], &[]),
        ));
        for seq in 0..LOG_FLUSH_LINES as u64 + 7 {
            tx.send(RawLine {
                stream: if seq % 2 == 0 { "stdout" } else { "stderr" },
                seq,
                data: format!("line {seq}"),
            })
            .await
            .unwrap();
        }
        drop(tx);
        handle.await.unwrap();
        let o = out.outbox.lock().unwrap();
        let seqs: Vec<u64> = o
            .msgs()
            .flat_map(|m| match m {
                AgentMessage::LogBatch { lines, attempt, .. } => {
                    assert_eq!(*attempt, Some(3));
                    lines.iter().map(|l| l.seq).collect::<Vec<_>>()
                }
                other => panic!("expected batches, got {other:?}"),
            })
            .collect();
        assert_eq!(
            seqs,
            (0..LOG_FLUSH_LINES as u64 + 7).collect::<Vec<_>>(),
            "batching must not reorder or renumber"
        );
        assert!(
            o.queued() > 1,
            "the line threshold has to have split this into more than one batch"
        );
    }

    #[test]
    fn a_dropped_count_survives_a_failed_notice_and_marks_where_the_gap_is() {
        let step = Uuid::new_v4();
        let mut o = Outbox::with_capacity(2);
        o.push(log_batch(step, &[10, 11]));
        o.push(log_batch(step, &[12, 13]));
        let taken = o.take_dropped();
        let d = taken[&(step, Some(1))];
        assert_eq!(d.count, 2);
        // The newest dropped line: the notice sorts into the log where the gap is,
        // which is what readers ordering by `seq` need from it.
        assert_eq!(d.at_seq, 11);
        assert!(o.take_dropped().is_empty(), "taken means taken");
        // The notice did not reach the server; the count must not go with it.
        o.restore_dropped(taken);
        let again = o.take_dropped();
        assert_eq!(again[&(step, Some(1))].count, 2);
        assert_eq!(again[&(step, Some(1))].at_seq, 11);
    }

    #[test]
    fn an_old_server_gets_one_chunk_per_line_under_one_token() {
        // A server that predates `LogBatch` answers `Error { "invalid message" }` and
        // the line is already gone — the writer pops on a successful send, not on an
        // ack — so a whole build's log disappears with nothing to point at. The batch
        // is unpacked here, at the last point the session's capabilities are known,
        // because a rolling deploy can hand a batch queued against a new replica to an
        // old one.
        let step = Uuid::new_v4();
        let mut o = Outbox::with_capacity(100);
        o.push(log_batch(step, &[7, 8, 9]));

        let (new_token, new_frames) = o.peek_front_frames(true).expect("a message");
        assert_eq!(new_frames.unwrap().len(), 1, "a new server takes the batch");

        let (token, frames) = o.peek_front_frames(false).expect("a message");
        assert_eq!(token, new_token, "one token: the batch is one queue entry");
        let frames = frames.expect("serialisable");
        assert_eq!(frames.len(), 3);
        let parsed: Vec<AgentMessage> = frames
            .iter()
            .map(|f| serde_json::from_str(f).unwrap())
            .collect();
        let seqs: Vec<u64> = parsed
            .iter()
            .map(|m| match m {
                AgentMessage::LogChunk {
                    seq,
                    step_run_id: s,
                    attempt,
                    stream,
                    ..
                } => {
                    assert_eq!(*s, step);
                    assert_eq!(*attempt, Some(1));
                    assert_eq!(stream, "stdout");
                    *seq
                }
                other => panic!("expected chunks, got {other:?}"),
            })
            .collect();
        assert_eq!(seqs, vec![7, 8, 9], "the original seqs, in order");
        // Nothing leaves the queue until the writer says so.
        assert_eq!(o.queued(), 1);
        o.pop_sent(token);
        assert_eq!(o.queued(), 0);
    }

    #[test]
    fn sending_a_message_gives_its_room_back() {
        // The budget has to follow the queue. Charged for messages already on the wire,
        // the outbox looks full forever and every batch waits out the back-pressure
        // bound before it is admitted — 500 lines per 30 s, which is what this cost.
        let step = Uuid::new_v4();
        let mut o = Outbox::with_limits(10, 1_000);
        o.push(log_batch(step, &(0..10).collect::<Vec<_>>()));
        assert!(!o.has_room_for(1, 0), "the whole 10-line budget is queued");
        let (token, _) = o.peek_front_frames(true).expect("a message to send");
        o.pop_sent(token);
        assert!(o.has_room_for(1, 0), "the queue is empty; so is the budget");
        assert_eq!((o.queued_lines, o.queued_bytes), (0, 0));
    }

    #[tokio::test]
    async fn a_full_outbox_holds_the_producer_back_while_the_socket_is_up() {
        // The bounded channel between the pipes and the batcher is not enough on its
        // own: the batcher drains it as fast as it can, so the loss moves to the outbox.
        // While a session is up the batcher has to wait for room instead, which is what
        // stops the step's own writes.
        let out = Outbound::new();
        let step = Uuid::new_v4();
        out.connected.store(true, Ordering::SeqCst);
        // Fill it to the line budget.
        while out.has_room_for(&log_batch(step, &[0])) {
            out.send(log_batch(step, &[0]));
        }
        let waiting = out.clone();
        let handle = tokio::spawn(async move { waiting.send_batch(log_batch(step, &[1])).await });
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(!handle.is_finished(), "a connected, full outbox must wait");
        // Room appears: the writer put something on the wire.
        if let Ok(mut o) = out.outbox.lock() {
            let token = o.queue.front().map(|q| q.token).unwrap();
            o.pop_sent(token);
            o.recount();
        }
        out.drained.notify_waiters();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the batch goes out once there is room")
            .unwrap();
    }

    #[tokio::test]
    async fn a_disconnected_outbox_takes_the_batch_at_once_and_drops_the_oldest() {
        // Nothing to wait for with the socket down: holding a step's pipes shut for the
        // length of an outage is worse than losing its oldest output.
        let out = Outbound::new();
        let step = Uuid::new_v4();
        while out.has_room_for(&log_batch(step, &[0])) {
            out.send(log_batch(step, &[0]));
        }
        let outcome = tokio::time::timeout(
            Duration::from_millis(250),
            out.send_batch(log_batch(step, &[1])),
        )
        .await
        .expect("must not wait while disconnected");
        assert_eq!(outcome, Enqueue::DropOldestLog);
    }

    #[test]
    fn the_outbox_budget_counts_lines_not_messages() {
        // Otherwise one step's buffered output evicts another's: a batch of 500 lines
        // and a batch of one would cost the same.
        let step = Uuid::new_v4();
        let mut o = Outbox::with_capacity(10);
        o.push(log_batch(step, &(0..8).collect::<Vec<_>>()));
        assert_eq!(o.queued(), 1);
        o.push(log_batch(step, &(8..12).collect::<Vec<_>>()));
        // 12 lines over a 10-line budget: the older batch goes, whole.
        assert_eq!(o.queued(), 1);
        assert_eq!(
            o.take_dropped().get(&(step, Some(1))).map(|d| d.count),
            Some(8)
        );
    }

    #[test]
    fn the_outbox_also_bounds_bytes() {
        // A step printing 64 KiB lines would hold 640 MB inside a 10 000-line budget.
        let step = Uuid::new_v4();
        let mut o = Outbox::with_limits(1_000, 4_096);
        let big = |seq: u64| AgentMessage::LogBatch {
            agent_id: Uuid::nil(),
            step_run_id: step,
            attempt: Some(1),
            lines: vec![LogLineWire {
                stream: "stdout".into(),
                data: "x".repeat(3_000),
                seq,
            }],
        };
        o.push(big(0));
        o.push(big(1));
        assert_eq!(o.queued(), 1, "6 000 bytes over a 4 096-byte budget");
        assert_eq!(
            o.take_dropped().get(&(step, Some(1))).map(|d| d.count),
            Some(1)
        );
    }

    fn log_chunk(step: Uuid, seq: u64) -> AgentMessage {
        AgentMessage::LogChunk {
            agent_id: Uuid::nil(),
            step_run_id: step,
            stream: "stdout".into(),
            data: format!("line {seq}"),
            seq,
            attempt: Some(1),
        }
    }

    fn complete(step: Uuid) -> AgentMessage {
        AgentMessage::StepComplete {
            agent_id: Uuid::nil(),
            step_run_id: step,
            status: StepStatus::Succeeded,
            exit_code: Some(0),
            error: None,
            attempt: Some(1),
        }
    }

    fn artifact(step: Uuid) -> AgentMessage {
        AgentMessage::Artifact {
            agent_id: Uuid::nil(),
            step_run_id: step,
            name: "a".into(),
            path: "a".into(),
            size: 1,
            content_base64: Some("AA==".into()),
            attempt: Some(1),
        }
    }

    fn seqs(o: &Outbox) -> Vec<u64> {
        o.msgs()
            .filter_map(|m| match m {
                AgentMessage::LogChunk { seq, .. } => Some(*seq),
                _ => None,
            })
            .collect()
    }

    // --- outbox: what survives a disconnect -------------------------------------------

    #[test]
    fn the_outbox_keeps_everything_below_its_cap_in_order() {
        let step = Uuid::new_v4();
        let mut o = Outbox::with_capacity(3);
        assert_eq!(o.push(log_chunk(step, 0)), Enqueue::Keep);
        assert_eq!(o.push(log_chunk(step, 1)), Enqueue::Keep);
        assert_eq!(o.push(complete(step)), Enqueue::Keep);
        assert_eq!(seqs(&o), vec![0, 1]);
        assert!(matches!(
            o.msgs().last(),
            Some(AgentMessage::StepComplete { .. })
        ));
    }

    #[test]
    fn past_the_cap_the_oldest_log_line_goes_first() {
        let step = Uuid::new_v4();
        let mut o = Outbox::with_capacity(3);
        o.push(log_chunk(step, 0));
        o.push(log_chunk(step, 1));
        o.push(log_chunk(step, 2));
        assert_eq!(o.push(log_chunk(step, 3)), Enqueue::DropOldestLog);
        assert_eq!(
            seqs(&o),
            vec![1, 2, 3],
            "the newest line is kept, the oldest dropped"
        );
        assert_eq!(
            o.take_dropped().get(&(step, Some(1))).map(|d| d.count),
            Some(1)
        );
    }

    #[test]
    fn completions_and_artifacts_grow_past_the_cap_and_never_reorder() {
        let step = Uuid::new_v4();
        let mut o = Outbox::with_capacity(2);
        o.push(complete(step));
        o.push(artifact(step));
        // Full of messages that must reach the server: the cap yields, not the queue.
        assert_eq!(o.push(complete(step)), Enqueue::Keep);
        assert_eq!(o.queued(), 3);
        // The cap is a budget for *output*: completions do not spend it, so a line
        // arriving alongside three of them is admitted rather than dropped on arrival.
        assert_eq!(o.push(log_chunk(step, 0)), Enqueue::Keep);
        assert_eq!(seqs(&o), vec![0]);
        assert!(matches!(
            o.msgs().next(),
            Some(AgentMessage::StepComplete { .. })
        ));
    }

    #[test]
    fn past_the_hard_bound_the_oldest_non_completion_goes_then_admission_is_refused() {
        let step = Uuid::new_v4();
        let mut o = Outbox::with_capacity(2);
        o.push(artifact(step));
        o.push(complete(step));
        o.push(complete(step));
        o.push(complete(step));
        // Twice the cap is the hard bound: the artifact is the oldest non-completion.
        assert_eq!(o.push(complete(step)), Enqueue::DropOldest);
        assert_eq!(o.queued(), 4);
        assert!(
            o.msgs()
                .all(|m| matches!(m, AgentMessage::StepComplete { .. }))
        );
        // Nothing but completions left: a result already held outranks the newcomer.
        assert_eq!(o.push(artifact(step)), Enqueue::Refused);
        assert_eq!(o.queued(), 4);
    }

    #[test]
    fn purging_a_step_removes_everything_about_it_and_nothing_else() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut o = Outbox::with_capacity(10);
        o.push(log_chunk(a, 0));
        o.push(log_chunk(b, 0));
        o.push(complete(b));
        o.push(complete(a));
        o.push(artifact(a));
        o.push(AgentMessage::Claim {
            agent_id: Uuid::nil(),
            step_run_id: a,
        });
        o.purge_step(a);
        // Its completion too: a new offer for the step means the server has moved past
        // the attempt that completion reports on.
        assert_eq!(o.queued(), 2);
        assert!(o.msgs().all(|m| step_of(m) == Some(b)));
    }

    #[test]
    fn purging_a_step_s_logs_keeps_what_reports_on_it() {
        // What a given-up step leaves behind: its lines are worthless to an attempt the
        // server has finished with, its result is not.
        let a = Uuid::new_v4();
        let mut o = Outbox::with_capacity(10);
        o.push(log_chunk(a, 0));
        o.push(complete(a));
        o.push(log_chunk(a, 1));
        o.purge_step_logs(a);
        assert_eq!(o.queued(), 1);
        assert!(matches!(
            o.msgs().next(),
            Some(AgentMessage::StepComplete { .. })
        ));
    }

    #[test]
    fn a_message_purged_mid_send_does_not_take_the_next_one_with_it() {
        // The writer holds no lock while the socket takes a message. If a purge empties
        // the front in that window, popping blind would discard the message behind it —
        // which was never sent, and may be a completion.
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let mut o = Outbox::with_capacity(10);
        o.push(log_chunk(a, 0));
        o.push(complete(b));
        let (token, _) = o.peek_front_frames(true).expect("a message to send");
        o.purge_step(a);
        o.pop_sent(token);
        assert_eq!(o.queued(), 1, "b's completion is still queued");
        assert!(matches!(
            o.msgs().next(),
            Some(AgentMessage::StepComplete { .. })
        ));
        // The ordinary path still pops what it sent.
        let (token, _) = o.peek_front_frames(true).expect("a message to send");
        o.pop_sent(token);
        assert_eq!(o.queued(), 0);
    }

    // --- giving up a step -------------------------------------------------------------

    #[test]
    fn a_given_up_step_is_reported_lost_on_its_own_attempt() {
        let step = Uuid::new_v4();
        let agent = Uuid::new_v4();
        match give_up_report(agent, step, Some(2)) {
            AgentMessage::StepComplete {
                agent_id,
                step_run_id,
                status,
                exit_code,
                error,
                attempt,
            } => {
                assert_eq!((agent_id, step_run_id), (agent, step));
                // Failed, not cancelled: a cancel is a person's decision and would not
                // retry. This spends an attempt from the step's own budget.
                assert_eq!(status, StepStatus::Failed);
                assert_eq!(exit_code, None);
                assert_eq!(
                    error.as_deref(),
                    Some("lease lost while the agent was disconnected")
                );
                // The attempt is what makes it safe to send after a reclaim.
                assert_eq!(attempt, Some(2));
            }
            other => panic!("expected a completion, got {other:?}"),
        }
    }

    #[test]
    fn giving_up_reports_every_attempt_and_drops_only_their_lines() {
        // Going silent left the row running under this agent, and the next heartbeat
        // renewed its lease for ever.
        let one = Uuid::new_v4();
        let two = Uuid::new_v4();
        let cancels: Mutex<HashMap<StepAttemptKey, oneshot::Sender<()>>> =
            Mutex::new(HashMap::new());
        let abandoned: Mutex<HashSet<StepAttemptKey>> = Mutex::new(HashSet::new());
        let outbound = Outbound::new();
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        cancels.lock().unwrap().insert((one, Some(1)), tx1);
        cancels.lock().unwrap().insert((two, Some(3)), tx2);
        outbound.send(log_chunk(one, 0));
        outbound.send(log_chunk(two, 0));

        assert_eq!(
            give_up_steps(Uuid::new_v4(), &cancels, &abandoned, &outbound),
            2
        );

        // Both tasks were told to stop, and neither may report for itself.
        assert!(rx1.is_terminated() || rx1.blocking_recv().is_ok());
        assert!(rx2.is_terminated() || rx2.blocking_recv().is_ok());
        let a = abandoned.lock().unwrap();
        assert!(a.contains(&(one, Some(1))) && a.contains(&(two, Some(3))));
        // Two reports, no lines.
        let o = outbound.outbox.lock().unwrap();
        assert_eq!(o.queued(), 2);
        assert!(
            o.msgs()
                .all(|m| matches!(m, AgentMessage::StepComplete { .. }))
        );
        let attempts: Vec<Option<i32>> = o
            .msgs()
            .filter_map(|m| match m {
                AgentMessage::StepComplete { attempt, .. } => Some(*attempt),
                _ => None,
            })
            .collect();
        assert!(attempts.contains(&Some(1)) && attempts.contains(&Some(3)));
    }

    // --- how long to keep running after a disconnect ---------------------------------

    #[test]
    fn a_lease_from_the_server_gives_a_grace_short_of_the_lease() {
        // 300 s lease, 10 s heartbeats: the last renewal the server saw may be an
        // interval old, one more may be in flight, and the first one after a reconnect
        // needs a moment to land — three intervals of margin.
        assert_eq!(
            grace_after_disconnect(Some(300), Duration::from_secs(10)),
            Duration::from_secs(270)
        );
    }

    #[test]
    fn an_old_server_gets_no_grace() {
        // No lease_secs in Welcome: that server requeues on close, so a step kept
        // running here would race the re-leased attempt. Cancel at once, as before.
        assert_eq!(
            grace_after_disconnect(None, Duration::from_secs(10)),
            Duration::ZERO
        );
    }

    #[test]
    fn a_lease_shorter_than_the_margin_saturates_to_zero() {
        assert_eq!(
            grace_after_disconnect(Some(20), Duration::from_secs(10)),
            Duration::ZERO
        );
    }

    // --- retrying the API through an outage ------------------------------------------

    #[test]
    fn a_definite_answer_is_not_retried_however_much_budget_is_left() {
        // A 4xx is the server's decision; retrying spends the whole grace to arrive at
        // the same place, and the step is failed at the end of it either way.
        assert!(!retry_again(
            false,
            Duration::ZERO,
            Duration::from_secs(270)
        ));
        assert!(matches!(
            ApiFailure::from_status(reqwest::StatusCode::BAD_REQUEST, "x".into()),
            ApiFailure::Fatal(_)
        ));
        assert!(matches!(
            ApiFailure::from_status(reqwest::StatusCode::UNAUTHORIZED, "x".into()),
            ApiFailure::Fatal(_)
        ));
    }

    #[test]
    fn a_server_error_is_retried_until_the_budget_runs_out() {
        let budget = Duration::from_secs(270);
        assert!(matches!(
            ApiFailure::from_status(reqwest::StatusCode::BAD_GATEWAY, "x".into()),
            ApiFailure::Transient(_)
        ));
        assert!(ApiFailure::Transient("down".into()).is_transient());
        assert!(retry_again(true, Duration::from_secs(269), budget));
        assert!(!retry_again(true, budget, budget));
        assert!(!retry_again(true, Duration::from_secs(300), budget));
    }

    #[test]
    fn an_absurd_lease_is_clamped_so_the_deadline_cannot_overflow() {
        let grace = grace_after_disconnect(Some(u64::MAX), Duration::from_secs(10));
        assert_eq!(grace, Duration::from_secs(MAX_LEASE_SECS - 30));
        // The watchdog adds this to an Instant; it must not panic.
        let _ = give_up_at(None, tokio::time::Instant::now(), grace);
    }

    // --- the watchdog: one per outage, anchored at the last heartbeat ----------------

    #[test]
    fn the_deadline_counts_from_the_last_heartbeat_the_socket_took() {
        let grace = Duration::from_secs(270);
        let now = tokio::time::Instant::now();
        let heartbeat = now - Duration::from_secs(8);
        // The server's lease clock started at that heartbeat, not at the close.
        assert_eq!(give_up_at(Some(heartbeat), now, grace), heartbeat + grace);
        // Never wrote one: the close is all there is to count from.
        assert_eq!(give_up_at(None, now, grace), now + grace);
    }

    #[test]
    fn a_failed_reconnect_does_not_move_the_deadline() {
        // The anchor is the outage's first loss; later attempts pass the same value.
        let grace = Duration::from_secs(270);
        let first_loss = tokio::time::Instant::now();
        let later_attempt = first_loss + Duration::from_secs(25);
        assert_eq!(
            give_up_at(None, first_loss, grace),
            first_loss + grace,
            "the deadline is a function of the first loss alone"
        );
        assert_ne!(give_up_at(None, later_attempt, grace), first_loss + grace);
    }

    #[test]
    fn one_watchdog_per_outage_and_only_with_something_to_give_up() {
        assert!(should_arm_watchdog(false, 1));
        // A second one per failed reconnect would fire on a session that has healed.
        assert!(!should_arm_watchdog(true, 1));
        assert!(!should_arm_watchdog(false, 0));
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn a_token_in_the_remote_url_never_reaches_the_log() {
        assert_eq!(
            without_userinfo("https://x-access-token:ghs_abc@github.com/org/repo.git"),
            "https://***@github.com/org/repo.git"
        );
        assert_eq!(
            without_userinfo("https://github.com/org/repo.git"),
            "https://github.com/org/repo.git"
        );
        assert_eq!(
            without_userinfo("git@github.com:org/repo.git"),
            "git@github.com:org/repo.git"
        );
        assert_eq!(
            without_userinfo("https://host/path/with@sign"),
            "https://host/path/with@sign"
        );
        assert_eq!(without_userinfo("fetch"), "fetch");
        // A password containing `@`: nothing of it may survive.
        assert_eq!(
            without_userinfo("https://user:p@ss@w0rd@github.com/org/repo.git"),
            "https://***@github.com/org/repo.git"
        );
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
    fn redactor_masks_a_base64_encoding_of_a_secret() {
        use base64::Engine as _;
        let value = "supersecretvalue";
        let r = Redactor::new(&env(&[("NPM_TOKEN", value)]), &["NPM_TOKEN".into()]);
        let std_b64 = base64::engine::general_purpose::STANDARD.encode(value.as_bytes());
        let url_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.as_bytes());
        // `base64 <<< "$TOKEN"` encodes the trailing newline the here-string adds.
        let with_nl =
            base64::engine::general_purpose::STANDARD.encode(format!("{value}\n").as_bytes());
        assert_eq!(r.apply(&format!("body={std_b64}")), "body=***");
        assert_eq!(r.apply(&url_b64), "***");
        assert_eq!(r.apply(&with_nl), "***");
    }

    #[test]
    fn redactor_masks_a_percent_encoded_secret() {
        // What a query string or a `curl --trace` line carries.
        let r = Redactor::new(&env(&[("TOKEN", "p@ss w0rd/1234")]), &["TOKEN".into()]);
        assert_eq!(
            r.apply("GET /x?t=p%40ss%20w0rd%2F1234 HTTP/1.1"),
            "GET /x?t=*** HTTP/1.1"
        );
    }

    #[test]
    fn redactor_masks_a_json_escaped_secret() {
        let r = Redactor::new(&env(&[("TOKEN", "a\"b\\c\tlongenough")]), &["TOKEN".into()]);
        // Serialized into a request body, the value carries its escapes, not its bytes.
        assert_eq!(
            r.apply(r#"{"token":"a\"b\\c\tlongenough"}"#),
            r#"{"token":"***"}"#
        );
    }

    #[test]
    fn redactor_masks_encoded_forms_of_each_line_of_a_multi_line_secret() {
        use base64::Engine as _;
        let key = "-----BEGIN KEY-----\nabcdefghijklmnop\nqrstuvwxyz123456\n-----END KEY-----";
        let r = Redactor::new(&env(&[("DEPLOY_KEY", key)]), &["DEPLOY_KEY".into()]);
        let line_b64 = base64::engine::general_purpose::STANDARD.encode(b"abcdefghijklmnop");
        assert_eq!(r.apply(&line_b64), "***");
        // And the whole key as it appears inside a JSON payload.
        let escaped = key.replace('\n', "\\n");
        assert_eq!(
            r.apply(&format!("{{\"key\":\"{escaped}\"}}")),
            "{\"key\":\"***\"}"
        );
    }

    #[test]
    fn redaction_cost_does_not_grow_with_the_number_of_secrets() {
        // Twenty 30-line PEM keys is ~3 700 registered patterns. Scanning a 64 KiB line
        // once per pattern is a quarter of a second, and the log channel backpressures
        // into the step's own pipes — the build itself slows down because someone stored
        // a few keys. One automaton is O(line length) whatever the pattern count.
        let mut env_pairs: Vec<(String, String)> = Vec::new();
        let mut keys: Vec<String> = Vec::new();
        for k in 0..20 {
            let body: String = (0..30)
                .map(|i| format!("secretline{k:02}{i:02}abcdefghijklmnop"))
                .collect::<Vec<_>>()
                .join("\n");
            let name = format!("KEY_{k}");
            env_pairs.push((name.clone(), body));
            keys.push(name);
        }
        let r = Redactor::new(&env_pairs, &keys);
        assert!(
            r.pattern_count() > 1_500,
            "expected a large pattern set, got {}",
            r.pattern_count()
        );

        let line = "x".repeat(64 * 1024);
        let started = std::time::Instant::now();
        for _ in 0..20 {
            let out = r.apply(&line);
            assert_eq!(out.len(), line.len());
        }
        let elapsed = started.elapsed();
        // Deliberately loose (a debug build, on whatever CI runs): the per-pattern loop
        // this replaced needs about five seconds for the same work.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "redaction took {elapsed:?} for 20 lines against {} patterns",
            r.pattern_count()
        );

        // Still correct at that size.
        assert_eq!(
            r.apply("head secretline0500abcdefghijklmnop tail"),
            "head *** tail"
        );
    }

    #[test]
    fn redactor_does_not_widen_a_short_secret_through_its_encodings() {
        // "abc" is below MIN_LEN; its base64 ("YWJj") must not be registered either, and
        // neither may any longer encoding of it — masking those blanks out unrelated
        // output for a value too short to be worth protecting.
        let r = Redactor::new(&env(&[("SHORT", "abc")]), &["SHORT".into()]);
        assert_eq!(r.pattern_count(), 0, "a short secret registered a pattern");
    }

    #[test]
    fn an_artifact_path_must_be_plain_and_relative() {
        assert!(artifact_components("out/VERSION").is_some());
        assert!(artifact_components("dist").is_some());
        // `work_dir.join("/etc/passwd")` is `/etc/passwd`: an absolute declaration would
        // read outside the workspace entirely.
        assert!(artifact_components("/etc/passwd").is_none());
        assert!(artifact_components("../../etc/passwd").is_none());
        assert!(artifact_components("out/../../etc/passwd").is_none());
        assert!(artifact_components("").is_none());
        assert!(artifact_components(".").is_none());
    }

    #[tokio::test]
    async fn opening_an_artifact_refuses_a_symlink_swapped_in_after_the_check() {
        // The leaf race: the path was a regular file when it was checked, and is a
        // symlink by the time it is opened. A step can leave a process behind to do
        // exactly that, so the open itself has to refuse rather than the stat before it.
        let root = std::env::temp_dir().join(format!("fiber-nofollow-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let secret = root.join("id_rsa");
        std::fs::write(&secret, b"PRIVATE").unwrap();
        let target = root.join("out.txt");
        std::os::unix::fs::symlink(&secret, &target).unwrap();
        let opened = tokio::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&target)
            .await;
        assert!(opened.is_err(), "O_NOFOLLOW opened a symlink");
        // The same open on the real file still works.
        assert!(
            tokio::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&secret)
                .await
                .is_ok()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_symlinked_artifact_is_found_at_any_depth() {
        let root = std::env::temp_dir().join(format!("fiber-art-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("out/nested")).unwrap();
        std::fs::write(root.join("out/nested/real.txt"), b"ok").unwrap();
        let secret = root.join("id_rsa");
        std::fs::write(&secret, b"PRIVATE").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("out/leaf.txt")).unwrap();
        std::os::unix::fs::symlink(root.join("out"), root.join("alias")).unwrap();

        assert_eq!(symlink_in(&root, "out/nested/real.txt").await, None);
        assert_eq!(
            symlink_in(&root, "out/leaf.txt").await,
            Some(root.join("out/leaf.txt")),
            "the leaf itself is a symlink"
        );
        assert_eq!(
            symlink_in(&root, "alias/nested/real.txt").await,
            Some(root.join("alias")),
            "a symlinked parent is the same escape one level up"
        );
        // Nothing there yet is not a symlink; the caller reports it as missing.
        assert_eq!(symlink_in(&root, "out/absent").await, None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn container_rows_are_taken_only_from_well_formed_lines() {
        // A blank line handed to `docker rm -f` is an argument error that aborts the
        // whole sweep, and anything else in that output is not an id.
        assert_eq!(
            container_rows("a1b2c3d4e5f6 boot-a\n\n  0f1e2d3c4b5a6   boot-b \n"),
            vec![
                ("a1b2c3d4e5f6".to_string(), "boot-a".to_string()),
                ("0f1e2d3c4b5a6".to_string(), "boot-b".to_string())
            ]
        );
        // A container from before the boot label has none; it is still a previous boot's.
        assert_eq!(
            container_rows("a1b2c3d4e5f6\n"),
            vec![("a1b2c3d4e5f6".to_string(), String::new())]
        );
        assert!(container_rows("").is_empty());
        assert!(container_rows("Cannot connect to the Docker daemon\n").is_empty());
        // A short id is not one; neither is a word that happens to be hex.
        assert!(container_rows("abc123\n").is_empty());
        assert!(container_rows("deadbeef\n").is_empty());
    }

    #[test]
    fn the_container_labels_name_this_agent_and_this_process() {
        let l = ContainerLabels::new("build-01");
        assert_eq!(l.agent, "fiber.agent=build-01");
        assert!(l.boot.starts_with("fiber.boot="));
        // Two processes of the same agent differ.
        assert_ne!(l.boot, ContainerLabels::new("build-01").boot);
        // The name reaches `docker` as an argv token.
        assert_eq!(
            ContainerLabels::new("a b;rm -rf /").agent,
            "fiber.agent=a_b_rm_-rf__"
        );
        assert_eq!(ContainerLabels::new("").agent, "fiber.agent=agent");
    }

    #[test]
    fn the_sweep_never_touches_this_processes_own_containers() {
        // The whole hazard: `docker rm -f` on a container another live agent is running
        // kills a build in progress. Only another boot's containers may go.
        let rows = vec![
            ("mine".to_string(), "boot-me".to_string()),
            ("previous".to_string(), "boot-old".to_string()),
            ("unlabelled".to_string(), String::new()),
        ];
        assert_eq!(
            containers_to_sweep(&rows, "boot-me"),
            vec!["previous".to_string(), "unlabelled".to_string()]
        );
        assert!(containers_to_sweep(&[], "boot-me").is_empty());
    }

    #[test]
    fn the_sweep_stands_down_on_the_default_agent_name() {
        // `make agent` and the quickstart both leave the name at `local`, so two agents
        // on one host share a label and the sweep would kill the other's running steps.
        assert!(!sweep_allowed("local"));
        assert!(!sweep_allowed("  local  "));
        assert!(!sweep_allowed(""));
        assert!(sweep_allowed("build-01"));
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
