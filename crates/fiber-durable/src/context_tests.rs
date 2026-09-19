//! The durability primitives, driven against an in-memory store.
//!
//! These are the guarantees the whole runtime rests on: a resumed fiber must not re-run a
//! step it already completed, and must not re-serve a sleep it already served. Neither is
//! observable without watching what the context *doesn't* do, hence the recording double.

use crate::context::FiberContext;
use crate::persistence::FiberPersistence;
use crate::types::{FiberRecord, FiberState, FiberStatus};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// One `create` call: the task name, its input, and when it should wake.
type SpawnedFiber = (String, Value, Option<DateTime<Utc>>);

/// Records every write so a test can assert what was persisted and in which order.
#[derive(Default)]
struct RecordingStore {
    checkpoints: Mutex<Vec<FiberState>>,
    appended: Mutex<Vec<(String, Value)>>,
    heartbeats: AtomicUsize,
    created: Mutex<Vec<SpawnedFiber>>,
    /// When set, `append_step` fails — the crash-after-effect-before-checkpoint case.
    fail_append: bool,
}

impl RecordingStore {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn failing_append() -> Arc<Self> {
        Arc::new(Self {
            fail_append: true,
            ..Default::default()
        })
    }

    fn appended_keys(&self) -> Vec<String> {
        self.appended
            .lock()
            .unwrap()
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    }
}

#[async_trait]
impl FiberPersistence for RecordingStore {
    async fn save_checkpoint(&self, record: &FiberRecord) -> Result<()> {
        self.checkpoints.lock().unwrap().push(record.state.clone());
        Ok(())
    }

    async fn append_step(
        &self,
        _fiber_id: Uuid,
        key: &str,
        value: &Value,
        _heartbeat_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        if self.fail_append {
            anyhow::bail!("append_step failed");
        }
        self.appended
            .lock()
            .unwrap()
            .push((key.to_string(), value.clone()));
        Ok(())
    }

    async fn touch_heartbeat(&self, _id: Uuid) -> Result<()> {
        self.heartbeats.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn create(
        &self,
        _project_id: Uuid,
        name: &str,
        input: Value,
        wake_at: Option<DateTime<Utc>>,
    ) -> Result<FiberRecord> {
        self.created
            .lock()
            .unwrap()
            .push((name.to_string(), input.clone(), wake_at));
        Ok(record_with(FiberState::default()))
    }
}

fn record_with(state: FiberState) -> FiberRecord {
    let now = Utc::now();
    FiberRecord {
        id: Uuid::new_v4(),
        project_id: Uuid::new_v4(),
        name: "test_task".into(),
        status: FiberStatus::Running,
        input: json!({"n": 1}),
        state,
        result: None,
        error: None,
        attempts: 1,
        wake_at: None,
        heartbeat_at: None,
        created_at: now,
        updated_at: now,
    }
}

fn ctx_with(state: FiberState, store: &Arc<RecordingStore>) -> FiberContext {
    FiberContext::new(record_with(state), store.clone())
}

/// A `FiberState` as it comes back from the store on resume, with `steps` hydrated.
fn resumed(steps: &[(&str, Value)], sleeps_done: i32) -> FiberState {
    FiberState {
        steps: steps
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        data: Default::default(),
        sleeps_done,
    }
}

// --- step memoization ----------------------------------------------------------------

#[tokio::test]
async fn a_fresh_step_runs_its_body_and_is_persisted() {
    let store = RecordingStore::new();
    let mut ctx = ctx_with(FiberState::default(), &store);
    let calls = AtomicUsize::new(0);

    let out = ctx
        .step("charge", || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"charged": true}))
        })
        .await
        .unwrap();

    assert_eq!(out, json!({"charged": true}));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.appended_keys(), vec!["charge"]);
}

#[tokio::test]
async fn a_completed_step_is_not_run_again_on_resume() {
    // The entire point of the runtime: after a crash, the effect must not repeat.
    let store = RecordingStore::new();
    let mut ctx = ctx_with(resumed(&[("charge", json!({"charged": true}))], 0), &store);
    let calls = AtomicUsize::new(0);

    let out = ctx
        .step("charge", || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"charged": "AGAIN"}))
        })
        .await
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 0, "the body must not re-run");
    assert_eq!(
        out,
        json!({"charged": true}),
        "the memoized value is returned"
    );
    assert!(
        store.appended_keys().is_empty(),
        "a skipped step must not write a second row"
    );
}

#[tokio::test]
async fn steps_are_memoized_per_key_not_per_position() {
    // Two steps with different keys both run; a resume replays only the missing one.
    let store = RecordingStore::new();
    let mut ctx = ctx_with(resumed(&[("first", json!(1))], 0), &store);
    let first_calls = AtomicUsize::new(0);
    let second_calls = AtomicUsize::new(0);

    let a = ctx
        .step("first", || async {
            first_calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!(99))
        })
        .await
        .unwrap();
    let b = ctx
        .step("second", || async {
            second_calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!(2))
        })
        .await
        .unwrap();

    assert_eq!(a, json!(1), "replayed from the checkpoint");
    assert_eq!(b, json!(2), "genuinely executed");
    assert_eq!(first_calls.load(Ordering::SeqCst), 0);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.appended_keys(), vec!["second"]);
}

#[tokio::test]
async fn repeating_a_key_within_one_run_serves_the_first_result() {
    // Same-key reuse inside a single pass memoizes immediately, without waiting for a
    // resume — so a handler that calls step("x") in a loop does the work once.
    let store = RecordingStore::new();
    let mut ctx = ctx_with(FiberState::default(), &store);
    let calls = AtomicUsize::new(0);

    for _ in 0..3 {
        let v = ctx
            .step("once", || async {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                Ok(json!(n))
            })
            .await
            .unwrap();
        assert_eq!(v, json!(0));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.appended_keys(), vec!["once"]);
}

#[tokio::test]
async fn a_failing_step_is_not_memoized_so_the_retry_runs_it() {
    let store = RecordingStore::new();
    let mut ctx = ctx_with(FiberState::default(), &store);

    let err = ctx
        .step("flaky", || async { anyhow::bail!("boom") })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("boom"));
    assert!(
        store.appended_keys().is_empty(),
        "a failed step must not be checkpointed as done"
    );

    // The same key runs again rather than replaying the failure.
    let out = ctx
        .step("flaky", || async { Ok(json!("second time")) })
        .await
        .unwrap();
    assert_eq!(out, json!("second time"));
    assert_eq!(store.appended_keys(), vec!["flaky"]);
}

#[tokio::test(start_paused = true)]
async fn a_panicking_step_takes_its_heartbeat_down_with_it() {
    // The heartbeat keeps a running fiber from looking stale. A handler that panics
    // unwinds past the point that would have stopped it; left running, it keeps the row
    // fresh forever — never reclaimed, never failed, attempts never spent.
    let store = RecordingStore::new();
    let mut ctx = ctx_with(FiberState::default(), &store);
    let task = tokio::spawn(async move {
        ctx.step("boom", || async { panic!("handler bug") })
            .await
            .map(|_| ())
    });
    assert!(task.await.is_err(), "the step must have panicked");

    // Paused time auto-advances when the runtime is idle; a surviving heartbeat task
    // would tick several times in this window.
    tokio::time::sleep(std::time::Duration::from_secs(120)).await;
    assert_eq!(
        store.heartbeats.load(Ordering::SeqCst),
        0,
        "no heartbeat may outlive the step that spawned it"
    );
}

#[tokio::test]
async fn a_step_whose_checkpoint_fails_to_persist_surfaces_the_error() {
    // At-least-once: the effect happened but the checkpoint did not, so the step will run
    // again on resume. The failure must propagate rather than be swallowed — a silent
    // success here would mark the fiber complete with no record of the step.
    let store = RecordingStore::failing_append();
    let mut ctx = ctx_with(FiberState::default(), &store);

    let err = ctx
        .step("effect", || async { Ok(json!("done")) })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("append_step failed"), "{err}");
}

// --- sleep ordinals ------------------------------------------------------------------

#[tokio::test]
async fn a_first_sleep_suspends_and_advances_its_ordinal_in_memory_only() {
    let store = RecordingStore::new();
    let mut ctx = ctx_with(FiberState::default(), &store);
    let wake = Utc::now() + Duration::seconds(30);

    let suspended = ctx.sleep_until(wake).await.unwrap_err();
    assert_eq!(suspended.wake_at, wake);
    assert_eq!(ctx.state().sleeps_done, 1, "the engine's save carries this");

    assert!(
        store.checkpoints.lock().unwrap().is_empty(),
        "the ordinal must not be persisted ahead of the status and wake_at the engine \
         writes with it"
    );
}

#[tokio::test]
async fn a_save_that_fails_after_a_sleep_does_not_skip_the_sleep_on_resume() {
    // The engine's save (status = suspended, wake_at, sleeps_done, in one UPDATE) is
    // what makes a sleep durable. If it fails, the row stays `running` with no wake_at,
    // is reclaimed as stale, and the handler runs again from the top. Whatever the
    // store holds at that point must still make the first sleep suspend — otherwise a
    // 24-hour sleep becomes a 60-second one.
    let store = RecordingStore::new();
    let mut first = ctx_with(FiberState::default(), &store);
    let wake = Utc::now() + Duration::hours(24);
    first.sleep_until(wake).await.unwrap_err();
    // The save never happens (crash, database error); nothing else is written.

    // Resume from what the store actually has.
    let persisted = store
        .checkpoints
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap_or_default();
    let mut resumed_ctx = ctx_with(persisted, &store);
    assert!(
        resumed_ctx.sleep_until(wake).await.is_err(),
        "a sleep whose save never landed must suspend again, not be skipped"
    );
}

#[tokio::test]
async fn a_sleep_already_served_returns_immediately_on_resume() {
    let store = RecordingStore::new();
    let mut ctx = ctx_with(resumed(&[], 1), &store);

    // The handler re-executes from the top; its first sleep is the one already served.
    ctx.sleep_until(Utc::now() + Duration::seconds(30))
        .await
        .expect("the first sleep must not suspend again");
    assert!(
        store.checkpoints.lock().unwrap().is_empty(),
        "replaying a served sleep must not rewrite the checkpoint"
    );
}

#[tokio::test]
async fn sleeps_are_counted_by_position_so_a_resume_stops_at_the_next_one() {
    // Three sleeps in a handler, resumed with one done: the first replays through, the
    // second suspends, and the third is never reached.
    let store = RecordingStore::new();
    let mut ctx = ctx_with(resumed(&[], 1), &store);

    ctx.sleep_until(Utc::now() + Duration::seconds(1))
        .await
        .expect("sleep 1 already served");
    let second = ctx.sleep_until(Utc::now() + Duration::seconds(2)).await;
    assert!(second.is_err(), "sleep 2 must suspend");
    assert_eq!(ctx.state().sleeps_done, 2);
}

#[tokio::test]
async fn each_resume_advances_exactly_one_sleep() {
    // Walk a three-sleep handler through its resumes and assert it terminates after
    // three, rather than looping on the last one or skipping past it.
    let store = RecordingStore::new();
    let mut sleeps_done = 0;
    for expected in 1..=3 {
        let mut ctx = ctx_with(resumed(&[], sleeps_done), &store);
        let mut suspended_at = None;
        for i in 1..=3 {
            if ctx
                .sleep_until(Utc::now() + Duration::seconds(i))
                .await
                .is_err()
            {
                suspended_at = Some(i);
                break;
            }
        }
        assert_eq!(suspended_at, Some(expected), "resume {expected}");
        sleeps_done = ctx.state().sleeps_done;
        assert_eq!(sleeps_done, expected as i32);
    }

    // Fourth pass: every sleep is served, so the handler runs to completion.
    let mut ctx = ctx_with(resumed(&[], sleeps_done), &store);
    for i in 1..=3 {
        ctx.sleep_until(Utc::now() + Duration::seconds(i))
            .await
            .expect("all three sleeps are done");
    }
}

#[tokio::test]
async fn sleep_seconds_is_relative_to_now() {
    let store = RecordingStore::new();
    let mut ctx = ctx_with(FiberState::default(), &store);
    let before = Utc::now();
    let suspended = ctx.sleep(60).await.unwrap_err();
    let delta = suspended.wake_at - before;
    assert!(
        delta >= Duration::seconds(59) && delta <= Duration::seconds(61),
        "woke at {delta:?} from now"
    );
}

// --- stash ---------------------------------------------------------------------------

#[tokio::test]
async fn stash_is_readable_immediately_and_checkpointed() {
    let store = RecordingStore::new();
    let mut ctx = ctx_with(FiberState::default(), &store);

    ctx.stash("cursor", json!(42)).await.unwrap();
    assert_eq!(ctx.get("cursor"), Some(&json!(42)));
    assert_eq!(ctx.get("missing"), None);

    let checkpoints = store.checkpoints.lock().unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].data.get("cursor"), Some(&json!(42)));
}

#[tokio::test]
async fn stash_survives_a_resume_and_the_latest_write_wins() {
    let store = RecordingStore::new();
    let mut ctx = ctx_with(FiberState::default(), &store);
    ctx.stash("cursor", json!(1)).await.unwrap();
    ctx.stash("cursor", json!(2)).await.unwrap();
    assert_eq!(ctx.get("cursor"), Some(&json!(2)));

    let last = store.checkpoints.lock().unwrap().last().unwrap().clone();
    let resumed_ctx = ctx_with(last, &store);
    assert_eq!(resumed_ctx.get("cursor"), Some(&json!(2)));
}

#[tokio::test]
async fn a_checkpoint_carries_stash_and_sleeps_but_not_step_results() {
    // Step results live in `fiber_steps`, not the blob; duplicating them would let the
    // two disagree after a partial write.
    let store = RecordingStore::new();
    let mut ctx = ctx_with(FiberState::default(), &store);

    ctx.step("a", || async { Ok(json!("done")) }).await.unwrap();
    ctx.stash("cursor", json!(7)).await.unwrap();

    let checkpoint = store.checkpoints.lock().unwrap().last().unwrap().clone();
    assert!(
        checkpoint.steps.is_empty(),
        "step results must not be written into the state blob"
    );
    assert_eq!(checkpoint.data.get("cursor"), Some(&json!(7)));
}

// --- spawn ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_fiber_creates_a_follow_up_under_the_same_project() {
    let store = RecordingStore::new();
    let ctx = ctx_with(FiberState::default(), &store);
    let wake = Utc::now() + Duration::seconds(600);

    ctx.spawn_fiber("interval_task", json!({"interval_seconds": 60}), Some(wake))
        .await
        .unwrap();

    let created = store.created.lock().unwrap();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].0, "interval_task");
    assert_eq!(created[0].1, json!({"interval_seconds": 60}));
    assert_eq!(created[0].2, Some(wake));
}

#[tokio::test]
async fn a_spawn_inside_a_step_happens_once_across_a_resume() {
    // `interval_task` reschedules itself this way; without the memo a resume would fork
    // a second chain and the interval would double every crash.
    let store = RecordingStore::new();
    let mut ctx = ctx_with(FiberState::default(), &store);
    let p = ctx.persistence();
    let pid = ctx.record.project_id;
    ctx.step("reschedule", move || async move {
        let next = p.create(pid, "interval_task", json!({}), None).await?;
        Ok(json!(next.id.to_string()))
    })
    .await
    .unwrap();
    assert_eq!(store.created.lock().unwrap().len(), 1);

    // Resume with that step already recorded.
    let done = store.appended.lock().unwrap().clone();
    let mut resumed_ctx = ctx_with(
        resumed(
            &done
                .iter()
                .map(|(k, v)| (k.as_str(), v.clone()))
                .collect::<Vec<_>>(),
            0,
        ),
        &store,
    );
    let p2 = resumed_ctx.persistence();
    resumed_ctx
        .step("reschedule", move || async move {
            let next = p2.create(pid, "interval_task", json!({}), None).await?;
            Ok(json!(next.id.to_string()))
        })
        .await
        .unwrap();
    assert_eq!(
        store.created.lock().unwrap().len(),
        1,
        "the resume must not spawn a second chain"
    );
}
