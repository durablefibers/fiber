use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
}

impl StepStatus {
    /// Succeeded / Failed / Cancelled / Skipped — nothing further will happen to the step.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            StepStatus::Succeeded
                | StepStatus::Failed
                | StepStatus::Cancelled
                | StepStatus::Skipped
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    /// Succeeded / Failed / Cancelled — the run will not change again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepDefinition {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default = "default_retries")]
    pub retries: u32,
    /// Environment for this step, overriding any key of the same name set on the pipeline.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Directory to run in, relative to the workspace root. Artifact paths stay relative to
    /// the root, not to this, so moving a step's `working_directory` does not silently
    /// change what it publishes.
    #[serde(default)]
    pub working_directory: Option<String>,
    /// Interpreter for `run`, invoked as `<shell> -c`. Default `sh`. It has to exist in the
    /// image, or on the host for a shell step.
    #[serde(default)]
    pub shell: Option<String>,
    /// Let the run carry on when this step fails: the step is still recorded as failed, but
    /// it does not fail the run and its dependents still go ahead.
    #[serde(default)]
    pub continue_on_error: bool,
    /// Workspace-relative paths to upload as artifacts after a successful step.
    #[serde(default)]
    pub artifacts: Vec<String>,
    /// GitHub Actions-style matrix: axis name → values. Expanded at compile time.
    #[serde(default)]
    pub matrix: Option<std::collections::BTreeMap<String, Vec<String>>>,
    /// Condition: `success()` (default), `always()`, `never()`, or `matrix.os == 'linux'`.
    #[serde(default, rename = "if")]
    pub if_expr: Option<String>,
    /// Wall-clock limit for one attempt of this step, in minutes (workspace prep and
    /// artifact restore included). Unset = the server default
    /// (`FIBER_STEP_TIMEOUT_DEFAULT_MINUTES`, 60).
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
    /// Project secrets to inject, by name. Omitted = every project secret (the historical
    /// behaviour); an empty list = none. Naming them keeps credentials out of steps that
    /// have no use for them — notably steps running a third-party `image:`.
    #[serde(default)]
    pub secrets: Option<Vec<String>>,
}

fn default_retries() -> u32 {
    0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineTriggers {
    #[serde(default)]
    pub push: Option<PushTrigger>,
    #[serde(default)]
    pub pull_request: Option<PullRequestTrigger>,
    /// Run on an interval (minutes). Simple schedule for MVP.
    #[serde(default)]
    pub interval_minutes: Option<u32>,
    /// 6-field cron with seconds: `SEC MIN HOUR DAY MONTH DOW` (e.g. `0 */15 * * * *`).
    /// Takes precedence over `interval_minutes` when both are set.
    #[serde(default)]
    pub cron: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushTrigger {
    #[serde(default)]
    pub branches: Vec<String>,
    /// Glob patterns; fire if any changed file matches (GitHub Actions-style).
    #[serde(default)]
    pub paths: Vec<String>,
    /// Glob patterns; files matching these are ignored when deciding to fire.
    #[serde(default)]
    pub paths_ignore: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequestTrigger {
    /// Base branches (e.g. `main`). Empty = any base.
    #[serde(default)]
    pub branches: Vec<String>,
    /// PR actions: `opened`, `synchronize`, `reopened`. Empty = those three.
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub paths_ignore: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Git remote URL or local path.
    pub repo: String,
    /// Branch, tag, or commit SHA. Defaults to `main` when omitted at offer time.
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineDefinition {
    pub name: String,
    /// Environment for every step. A step's own `env` overrides a key set here, and a
    /// matrix binding overrides both, since that is what says which cell is running.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub workspace: Option<WorkspaceConfig>,
    #[serde(default)]
    pub on: Option<PipelineTriggers>,
    pub steps: Vec<StepDefinition>,
    /// Wall-clock limit for the whole run, in minutes, from run start. Unset = none.
    #[serde(default)]
    pub timeout_minutes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceOffer {
    pub repo: String,
    /// What to fetch: a branch, or `refs/pull/<n>/head` for a pull request. A pull
    /// request's head ref is fetchable from the base repository, so a fork's PR builds
    /// without any access to the fork.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// Exact commit to check out after fetching. When a webhook supplied it, the run
    /// builds that commit rather than wherever the ref has moved to since.
    #[serde(default)]
    pub sha: Option<String>,
}

/// Prior-step artifact the agent should download into the workspace before running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRestore {
    pub id: Uuid,
    /// Workspace-relative path to write (e.g. `out/VERSION`).
    pub name: String,
    pub size: u64,
}

/// Messages from an agent to `fiber-api` over `/ws/agent`.
///
/// The `agent_id` fields are informational only: the server binds the agent's identity
/// from the authenticated token at connect time and ignores (but logs) any mismatch.
/// Step-scoped messages are accepted only for steps currently leased to that agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentMessage {
    Hello {
        name: String,
        labels: Vec<String>,
        concurrency: u32,
    },
    Heartbeat {
        agent_id: Uuid,
    },
    Claim {
        agent_id: Uuid,
        step_run_id: Uuid,
    },
    LogChunk {
        agent_id: Uuid,
        step_run_id: Uuid,
        stream: String,
        data: String,
        seq: u64,
    },
    /// Legacy WS base64 upload. Prefer HTTP: presign PUT to S3 when available,
    /// else `PUT /api/agent/steps/.../artifacts`.
    Artifact {
        agent_id: Uuid,
        step_run_id: Uuid,
        name: String,
        path: String,
        size: u64,
        content_base64: Option<String>,
    },
    StepComplete {
        agent_id: Uuid,
        step_run_id: Uuid,
        status: StepStatus,
        exit_code: Option<i32>,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // Offer carries workspace/env/artifacts for the agent WS.
pub enum ServerMessage {
    Welcome {
        agent_id: Uuid,
    },
    Offer {
        step_run_id: Uuid,
        run_id: Uuid,
        step_id: String,
        step_name: String,
        image: Option<String>,
        run: String,
        workspace: Option<WorkspaceOffer>,
        env: Vec<(String, String)>,
        /// Workspace-relative paths to upload after success.
        #[serde(default)]
        artifacts: Vec<String>,
        /// Prior artifacts to restore into the workspace before the step runs.
        #[serde(default)]
        restore: Vec<ArtifactRestore>,
        /// Attempt wall-clock limit in minutes; the agent kills the step past it and
        /// the server independently fails it after a grace period. Absent from old servers.
        #[serde(default)]
        timeout_minutes: Option<u32>,
        /// Directory to run in, relative to the workspace root. Validated server-side to
        /// stay inside it; the agent checks again before use.
        #[serde(default)]
        working_directory: Option<String>,
        /// Interpreter for `run`. Absent means `sh`.
        #[serde(default)]
        shell: Option<String>,
        /// Which `env` entries are project secrets. The agent redacts their values from
        /// log lines; it does not otherwise treat them differently.
        #[serde(default)]
        secret_keys: Vec<String>,
        /// W3C `traceparent` for the span that offered this step, so the agent's execution
        /// span joins the server's trace instead of starting a root of its own. Absent
        /// unless the server exports OpenTelemetry, and ignored by older agents.
        #[serde(default)]
        traceparent: Option<String>,
    },
    Cancel {
        step_run_id: Uuid,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunEvent {
    RunUpdated {
        run_id: Uuid,
        status: RunStatus,
    },
    StepUpdated {
        run_id: Uuid,
        step_run_id: Uuid,
        step_id: String,
        status: StepStatus,
    },
    Log {
        run_id: Uuid,
        step_run_id: Uuid,
        stream: String,
        data: String,
        seq: u64,
        at: DateTime<Utc>,
    },
}
