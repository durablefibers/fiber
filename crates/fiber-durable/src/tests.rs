use crate::context::FiberSuspended;
use crate::engine::{FiberOutcome, failure_outcome};
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
    for (o, expected) in [
        (FiberOutcome::Completed, "completed"),
        (FiberOutcome::Suspended, "suspended"),
        (FiberOutcome::Failed, "failed"),
        (FiberOutcome::Retry, "retry"),
    ] {
        assert_eq!(serde_json::to_value(o).unwrap(), json!(expected));
        assert_eq!(
            serde_json::from_value::<FiberOutcome>(json!(expected)).unwrap(),
            o
        );
    }
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
fn failure_outcome_retries_until_max_attempts() {
    // run_fiber increments attempts before calling the handler, so the first failure
    // arrives with attempts == 1.
    assert_eq!(failure_outcome(1, 3), FiberOutcome::Retry);
    assert_eq!(failure_outcome(2, 3), FiberOutcome::Retry);
    assert_eq!(failure_outcome(3, 3), FiberOutcome::Failed);
    assert_eq!(failure_outcome(4, 3), FiberOutcome::Failed);
    // max_attempts <= 1 means no retries at all.
    assert_eq!(failure_outcome(1, 1), FiberOutcome::Failed);
    assert_eq!(failure_outcome(1, 0), FiberOutcome::Failed);
}
