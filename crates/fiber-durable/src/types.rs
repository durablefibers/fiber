use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FiberStatus {
    Pending,
    Running,
    Suspended,
    Completed,
    Failed,
    /// Stopped by a person. Distinct from `Failed`, which is the task's own outcome — a
    /// dashboard that cannot tell them apart cannot answer "is anything actually broken".
    Cancelled,
}

impl FiberStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Suspended => "suspended",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "suspended" => Self::Suspended,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Pending,
        }
    }

    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FiberState {
    /// Memoized step results (hydrated from `fiber_steps` on load).
    #[serde(default)]
    pub steps: std::collections::HashMap<String, Value>,
    /// Arbitrary stash data.
    #[serde(default)]
    pub data: std::collections::HashMap<String, Value>,
    #[serde(default)]
    pub sleeps_done: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FiberRecord {
    pub id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub status: FiberStatus,
    pub input: Value,
    pub state: FiberState,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub attempts: i32,
    pub wake_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
