use chrono::{DateTime, Utc};
use fiber_proto::{RunStatus, StepStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Pipeline {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub definition: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_scheduled_at: Option<DateTime<Utc>>,
    /// When the interval schedule should next fire (`NULL` = no schedule).
    pub next_due_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct StepAttempt {
    pub id: Uuid,
    pub step_run_id: Uuid,
    pub attempt: i32,
    pub agent_id: Option<Uuid>,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: String,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Run {
    pub id: Uuid,
    pub pipeline_id: Uuid,
    pub project_id: Uuid,
    pub status: String,
    pub trigger: String,
    pub definition_snapshot: Value,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl Run {
    pub fn status_enum(&self) -> RunStatus {
        match self.status.as_str() {
            "running" => RunStatus::Running,
            "succeeded" => RunStatus::Succeeded,
            "failed" => RunStatus::Failed,
            "cancelled" => RunStatus::Cancelled,
            _ => RunStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct StepRun {
    pub id: Uuid,
    pub run_id: Uuid,
    pub step_id: String,
    pub step_name: String,
    pub status: String,
    pub image: Option<String>,
    pub run_cmd: String,
    pub labels: Value,
    pub needs: Value,
    pub retries: i32,
    pub attempt: i32,
    pub agent_id: Option<Uuid>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl StepRun {
    pub fn status_enum(&self) -> StepStatus {
        match self.status.as_str() {
            "queued" => StepStatus::Queued,
            "running" => StepStatus::Running,
            "succeeded" => StepStatus::Succeeded,
            "failed" => StepStatus::Failed,
            "cancelled" => StepStatus::Cancelled,
            "skipped" => StepStatus::Skipped,
            _ => StepStatus::Pending,
        }
    }

    pub fn labels_vec(&self) -> Vec<String> {
        serde_json::from_value(self.labels.clone()).unwrap_or_default()
    }

    pub fn needs_vec(&self) -> Vec<String> {
        serde_json::from_value(self.needs.clone()).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Agent {
    pub id: Uuid,
    /// `None` = global pool (any project). `Some` = only that project's steps.
    pub project_id: Option<Uuid>,
    pub name: String,
    pub labels: Value,
    pub concurrency: i32,
    pub token_hash: String,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub online: bool,
    pub created_at: DateTime<Utc>,
}

impl Agent {
    pub fn labels_vec(&self) -> Vec<String> {
        serde_json::from_value(self.labels.clone()).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct LogLine {
    pub id: i64,
    pub run_id: Uuid,
    pub step_run_id: Uuid,
    pub stream: String,
    pub data: String,
    pub seq: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Artifact {
    pub id: Uuid,
    pub run_id: Uuid,
    pub step_run_id: Uuid,
    pub name: String,
    pub path: String,
    pub size: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProjectRequest {
    pub name: String,
    pub slug: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatePipelineRequest {
    pub name: String,
    pub definition: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePipelineRequest {
    pub name: Option<String>,
    pub definition: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentRequest {
    pub name: String,
    pub labels: Vec<String>,
    pub concurrency: Option<u32>,
    /// Omit or null for a global agent; set to bind the agent to one project.
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAgentRequest {
    pub name: Option<String>,
    pub labels: Option<Vec<String>>,
    pub concurrency: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentResponse {
    pub agent: Agent,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartRunRequest {
    pub trigger: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub password_hash: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PublicUser {
    pub id: Uuid,
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub user: PublicUser,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProjectSecretMeta {
    pub id: Uuid,
    pub project_id: Uuid,
    pub key: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertSecretRequest {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ProjectMember {
    pub project_id: Uuid,
    pub user_id: Uuid,
    pub role: String,
    pub username: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddMemberRequest {
    pub username: String,
    pub role: String,
    /// If the user does not exist, create with this password (owner/admin only).
    #[serde(default)]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateMemberRequest {
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
}
