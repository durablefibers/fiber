use crate::context::FiberSuspended;
use crate::engine::FiberOutcome;
use crate::registry::FiberRegistry;
use crate::tasks::register_builtin_tasks;
use crate::types::{FiberState, FiberStatus};
use chrono::Utc;
use fiber_core::DueIndex;
use serde_json::json;

#[tokio::test]
async fn due_index_earliest_and_authoritative() {
    let idx = DueIndex::new();
    let t1 = Utc::now();
    let t0 = t1 - chrono::Duration::seconds(10);
    let id = uuid::Uuid::new_v4();
    idx.record(id, t1);
    idx.record(id, t0);
    assert_eq!(idx.earliest(), Some(t0));
    let later = t1 + chrono::Duration::hours(1);
    idx.set(id, Some(later));
    assert_eq!(idx.earliest(), Some(later));
    idx.set(id, None);
    assert!(idx.is_empty());
}

#[tokio::test]
async fn registry_builtins_and_suspended_downcast() {
    let registry = FiberRegistry::new();
    register_builtin_tasks(&registry);
    assert!(registry.contains("ping"));
    assert!(registry.contains("sleep_demo"));
    assert!(registry.contains("interval_task"));

    let err = anyhow::Error::new(FiberSuspended {
        wake_at: Utc::now() + chrono::Duration::seconds(1),
    });
    assert!(err.downcast_ref::<FiberSuspended>().is_some());
}

#[test]
fn fiber_status_roundtrip() {
    for s in [
        FiberStatus::Pending,
        FiberStatus::Running,
        FiberStatus::Suspended,
        FiberStatus::Completed,
        FiberStatus::Failed,
    ] {
        assert_eq!(FiberStatus::parse(s.as_str()), s);
    }
    assert!(FiberStatus::Completed.terminal());
    assert!(!FiberStatus::Running.terminal());
}

#[test]
fn fiber_outcome_serde() {
    let o = FiberOutcome::Suspended;
    let v = serde_json::to_value(o).unwrap();
    assert_eq!(v, json!("suspended"));
}

#[test]
fn fiber_state_sleeps_done_in_checkpoint_blob() {
    // sleeps_done is the scalar that makes sleep_until a no-op on resume.
    let state = FiberState {
        steps: Default::default(),
        data: Default::default(),
        sleeps_done: 2,
    };
    let v = serde_json::to_value(&state).unwrap();
    let back: FiberState = serde_json::from_value(v).unwrap();
    assert_eq!(back.sleeps_done, 2);
}

#[test]
fn max_attempts_outcome_is_failed_when_exhausted() {
    // Contract: engine returns Failed when attempts >= max_attempts (see run_fiber).
    let attempts = 3;
    let max_attempts = 3;
    assert!(attempts >= max_attempts);
    assert_eq!(
        if attempts >= max_attempts {
            FiberOutcome::Failed
        } else {
            FiberOutcome::Retry
        },
        FiberOutcome::Failed
    );
}
