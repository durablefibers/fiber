use crate::context::{FiberContext, FiberSuspended};
use crate::registry::FiberRegistry;
use crate::store::FiberStore;
use crate::types::{FiberRecord, FiberStatus};
use chrono::Utc;
use serde::{Deserialize, Serialize};

const RETRY_BACKOFF_SECS: i64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FiberOutcome {
    Completed,
    Suspended,
    Failed,
    Retry,
}

/// Execute (or resume) one fiber, persisting its outcome.
pub async fn run_fiber(
    store: &FiberStore,
    registry: &FiberRegistry,
    mut record: FiberRecord,
    max_attempts: i32,
) -> anyhow::Result<FiberOutcome> {
    let Some(handler) = registry.get(&record.name) else {
        record.status = FiberStatus::Failed;
        record.error = Some(format!(
            "no durable task handler registered for '{}'",
            record.name
        ));
        store.save(&record).await?;
        return Ok(FiberOutcome::Failed);
    };

    record.status = FiberStatus::Running;
    record.attempts += 1;
    record.heartbeat_at = Some(Utc::now());
    record.wake_at = None;
    store.save(&record).await?;

    let mut ctx = FiberContext::new(record.clone(), store.clone());
    let result = handler.run(&mut ctx).await;
    let mut record = ctx.record.clone();
    record.state = ctx.take_state();
    // Steps live in fiber_steps; clear blob steps before save.
    record.state.steps.clear();

    match result {
        Ok(value) => {
            record.status = FiberStatus::Completed;
            record.result = Some(value);
            record.error = None;
            record.wake_at = None;
            record.heartbeat_at = None;
            store.save(&record).await?;
            Ok(FiberOutcome::Completed)
        }
        Err(e) => {
            if let Some(suspended) = e.downcast_ref::<FiberSuspended>() {
                let wake_at = suspended.wake_at;
                // Re-read sleeps_done from context state already assigned
                record.status = FiberStatus::Suspended;
                record.wake_at = Some(wake_at);
                record.heartbeat_at = None;
                // Preserve data/sleeps_done from ctx (already in record.state)
                store.save(&record).await?;
                return Ok(FiberOutcome::Suspended);
            }
            record.error = Some(format!("{e:#}"));
            record.heartbeat_at = None;
            if record.attempts >= max_attempts {
                record.status = FiberStatus::Failed;
                store.save(&record).await?;
                Ok(FiberOutcome::Failed)
            } else {
                record.status = FiberStatus::Suspended;
                record.wake_at = Some(Utc::now() + chrono::Duration::seconds(RETRY_BACKOFF_SECS));
                store.save(&record).await?;
                Ok(FiberOutcome::Retry)
            }
        }
    }
}
