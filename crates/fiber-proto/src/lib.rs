use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod validate;

/// The agent protocol revision this crate speaks. An agent sends it in `Hello`; a server
/// reads it to decide what the agent can be relied on to do, and a later server may
/// refuse a revision it no longer supports.
///
/// - `0` — never sent: an agent older than this field. It cancels its steps on any
///   socket close, so the server requeues them at once on disconnect.
/// - `1` — the agent keeps its step tasks and its outbound queue across sessions, resumes
///   heartbeats (lease renewal) after a reconnect, sends `Goodbye` before a deliberate
///   exit, and reads `lease_secs` from `Welcome`.
pub const PROTOCOL_VERSION: u32 = 1;

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
    /// One run at a time per group, newest wins. Unset = no limit.
    #[serde(default)]
    pub concurrency: Option<ConcurrencyConfig>,
}

/// Keep one run per group in flight, cancelling the older ones.
///
/// Absent means unlimited, which stays the default: a project that wants every push built
/// should not have to say so. Present with `cancel_in_progress: false` is also unlimited
/// today — queueing rather than cancelling is a separate behaviour, and refusing to guess
/// which one an author meant is cheaper than silently picking one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConcurrencyConfig {
    /// Runs sharing this resolved string contend. `{pipeline}` and `{ref}` expand;
    /// anything else is literal. Defaults to `{pipeline}-{ref}`, which is what makes a
    /// second push to one branch cancel the first without touching any other branch.
    #[serde(default)]
    pub group: Option<String>,
    /// Cancel older runs in the group when a new one starts.
    #[serde(default)]
    pub cancel_in_progress: bool,
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
        /// [`PROTOCOL_VERSION`] of the agent. Absent (`0`) from agents older than the
        /// field; see the constant for what each revision promises.
        #[serde(default)]
        protocol_version: u32,
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
    /// The agent is exiting on purpose (SIGTERM) and has already stopped its steps: the
    /// server requeues everything it holds now instead of waiting for the leases to
    /// expire. Sent after the steps are stopped, so no two attempts overlap. A server
    /// older than this variant answers `Error { "invalid message" }` and requeues on the
    /// close that follows, which is the same outcome.
    Goodbye {
        agent_id: Uuid,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // Offer carries workspace/env/artifacts for the agent WS.
pub enum ServerMessage {
    Welcome {
        agent_id: Uuid,
        /// How long a step lease lives without renewal, in seconds. An agent that loses
        /// its socket keeps running its steps for this long (less a margin) while it
        /// reconnects; past it the server has reclaimed them. Absent from older servers,
        /// which requeue on disconnect — the agent then stops its steps on any close.
        #[serde(default)]
        lease_secs: Option<u64>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every variant, so adding one without deciding its terminality fails to compile.
    const ALL_STEP: [StepStatus; 7] = [
        StepStatus::Pending,
        StepStatus::Queued,
        StepStatus::Running,
        StepStatus::Succeeded,
        StepStatus::Failed,
        StepStatus::Cancelled,
        StepStatus::Skipped,
    ];
    const ALL_RUN: [RunStatus; 5] = [
        RunStatus::Pending,
        RunStatus::Running,
        RunStatus::Succeeded,
        RunStatus::Failed,
        RunStatus::Cancelled,
    ];

    #[test]
    fn only_finished_step_statuses_are_terminal() {
        for s in ALL_STEP {
            let expected = !matches!(
                s,
                StepStatus::Pending | StepStatus::Queued | StepStatus::Running
            );
            assert_eq!(s.is_terminal(), expected, "{s:?}");
        }
    }

    #[test]
    fn only_finished_run_statuses_are_terminal() {
        for s in ALL_RUN {
            let expected = !matches!(s, RunStatus::Pending | RunStatus::Running);
            assert_eq!(s.is_terminal(), expected, "{s:?}");
        }
    }

    #[test]
    fn status_wire_spellings_are_snake_case_and_stable() {
        // apps/ui/src/lib/api.ts compares against these strings by hand (convention 9);
        // renaming a variant without updating it there is silent until runtime.
        assert_eq!(
            serde_json::to_value(StepStatus::Succeeded).unwrap(),
            json!("succeeded")
        );
        assert_eq!(
            serde_json::to_value(StepStatus::Cancelled).unwrap(),
            json!("cancelled")
        );
        assert_eq!(
            serde_json::to_value(StepStatus::Skipped).unwrap(),
            json!("skipped")
        );
        assert_eq!(
            serde_json::to_value(RunStatus::Pending).unwrap(),
            json!("pending")
        );
        assert_eq!(
            serde_json::from_value::<StepStatus>(json!("running")).unwrap(),
            StepStatus::Running
        );
        // British spelling only: "canceled" is not accepted.
        assert!(serde_json::from_value::<StepStatus>(json!("canceled")).is_err());
    }

    #[test]
    fn agent_message_tags_are_stable() {
        let hello = AgentMessage::Hello {
            name: "agent-1".into(),
            labels: vec!["os=linux".into()],
            concurrency: 2,
            protocol_version: PROTOCOL_VERSION,
        };
        assert_eq!(
            serde_json::to_value(&hello).unwrap()["type"],
            json!("hello")
        );

        let complete = AgentMessage::StepComplete {
            agent_id: Uuid::nil(),
            step_run_id: Uuid::nil(),
            status: StepStatus::Succeeded,
            exit_code: Some(0),
            error: None,
        };
        let v = serde_json::to_value(&complete).unwrap();
        assert_eq!(v["type"], json!("step_complete"));
        assert_eq!(v["status"], json!("succeeded"));
    }

    #[test]
    fn server_message_tags_are_stable() {
        assert_eq!(
            serde_json::to_value(ServerMessage::Cancel {
                step_run_id: Uuid::nil()
            })
            .unwrap()["type"],
            json!("cancel")
        );
        assert_eq!(
            serde_json::to_value(ServerMessage::Welcome {
                agent_id: Uuid::nil(),
                lease_secs: Some(300),
            })
            .unwrap()["type"],
            json!("welcome")
        );
        assert_eq!(
            serde_json::to_value(AgentMessage::Goodbye {
                agent_id: Uuid::nil()
            })
            .unwrap()["type"],
            json!("goodbye")
        );
    }

    #[test]
    fn a_hello_without_protocol_version_is_revision_zero() {
        // An agent older than the field: the server must read it as the legacy contract
        // (cancels on close), not reject it.
        let v: AgentMessage = serde_json::from_value(json!({
            "type": "hello", "name": "old", "labels": [], "concurrency": 1
        }))
        .unwrap();
        assert!(matches!(
            v,
            AgentMessage::Hello {
                protocol_version: 0,
                ..
            }
        ));
        // 0 is reserved for agents without the field.
        const { assert!(PROTOCOL_VERSION >= 1) };
    }

    #[test]
    fn a_welcome_without_lease_secs_is_an_old_server() {
        let v: ServerMessage = serde_json::from_value(json!({
            "type": "welcome", "agent_id": Uuid::nil()
        }))
        .unwrap();
        assert!(matches!(
            v,
            ServerMessage::Welcome {
                lease_secs: None,
                ..
            }
        ));
    }

    #[test]
    fn run_event_tags_are_stable() {
        // The UI event stream (/ws/runs/{id}) switches on these.
        let v = serde_json::to_value(RunEvent::StepUpdated {
            run_id: Uuid::nil(),
            step_run_id: Uuid::nil(),
            step_id: "build".into(),
            status: StepStatus::Running,
        })
        .unwrap();
        assert_eq!(v["type"], json!("step_updated"));
        assert_eq!(
            serde_json::to_value(RunEvent::RunUpdated {
                run_id: Uuid::nil(),
                status: RunStatus::Failed,
            })
            .unwrap()["type"],
            json!("run_updated")
        );
    }

    #[test]
    fn an_offer_from_an_older_server_omits_the_newer_fields() {
        // timeout_minutes / secret_keys / traceparent / working_directory / shell are all
        // `#[serde(default)]` precisely so a mixed-version deploy keeps working.
        let minimal = json!({
            "type": "offer",
            "step_run_id": Uuid::nil(),
            "run_id": Uuid::nil(),
            "step_id": "build",
            "step_name": "Build",
            "image": null,
            "run": "make",
            "workspace": null,
            "env": [],
        });
        let msg: ServerMessage = serde_json::from_value(minimal).unwrap();
        let ServerMessage::Offer {
            timeout_minutes,
            secret_keys,
            traceparent,
            working_directory,
            shell,
            artifacts,
            restore,
            ..
        } = msg
        else {
            panic!("expected an offer");
        };
        assert_eq!(timeout_minutes, None);
        assert!(secret_keys.is_empty());
        assert_eq!(traceparent, None);
        assert_eq!(working_directory, None);
        assert_eq!(shell, None);
        assert!(artifacts.is_empty());
        assert!(restore.is_empty());
    }

    #[test]
    fn an_unknown_field_from_a_newer_peer_is_ignored_rather_than_fatal() {
        let v = json!({
            "type": "heartbeat",
            "agent_id": Uuid::nil(),
            "something_we_have_not_shipped_yet": 1,
        });
        assert!(serde_json::from_value::<AgentMessage>(v).is_ok());
    }

    #[test]
    fn the_yaml_facing_renames_are_ref_and_if() {
        // `ref` and `if` are Rust keywords; fiber.yml spells them plainly, and
        // docs/pipeline-yaml.md documents them that way.
        let ws: WorkspaceConfig =
            serde_json::from_value(json!({"repo": "git@example.com:o/r.git", "ref": "main"}))
                .unwrap();
        assert_eq!(ws.git_ref.as_deref(), Some("main"));
        assert_eq!(
            serde_json::to_value(&ws).unwrap()["ref"],
            json!("main"),
            "must serialise back as `ref`, not `git_ref`"
        );

        let step: StepDefinition = serde_json::from_value(json!({
            "id": "test", "name": "test", "if": "always()"
        }))
        .unwrap();
        assert_eq!(step.if_expr.as_deref(), Some("always()"));
        assert_eq!(
            serde_json::to_value(&step).unwrap()["if"],
            json!("always()")
        );
    }

    #[test]
    fn a_step_definition_needs_only_an_id_and_a_name() {
        let step: StepDefinition = serde_json::from_value(json!({"id": "a", "name": "A"})).unwrap();
        assert_eq!(step.retries, 0);
        assert!(step.needs.is_empty());
        assert!(!step.continue_on_error);
        assert_eq!(step.run, None);
        // None means "every project secret"; an empty Vec means none. The distinction is
        // load-bearing, so it has to survive the default.
        assert_eq!(step.secrets, None);
    }

    #[test]
    fn an_empty_secrets_list_stays_distinct_from_an_absent_one() {
        let named: StepDefinition =
            serde_json::from_value(json!({"id": "a", "name": "A", "secrets": []})).unwrap();
        assert_eq!(named.secrets, Some(vec![]));
    }
}
