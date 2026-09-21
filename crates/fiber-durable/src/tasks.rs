//! Built-in demo durable tasks.

use crate::context::FiberContext;
use crate::registry::{FiberHandler, FiberRegistry};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};

/// Register the built-in handlers: `http_request`, plus the `ping`, `sleep_demo` and
/// `interval_task` demos.
pub fn register_builtin_tasks(registry: &FiberRegistry) {
    registry.register_fn("ping", |ctx| {
        let input = ctx.input.clone();
        async move {
            let msg = input
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("pong");
            Ok(json!({ "message": msg, "at": Utc::now().to_rfc3339() }))
        }
    });

    registry.register("http_request", crate::http_task::HttpRequestTask);
    registry.register("sleep_demo", SleepDemo);
    registry.register("interval_task", IntervalTask);
}

struct SleepDemo;

#[async_trait]
impl FiberHandler for SleepDemo {
    async fn run(&self, ctx: &mut FiberContext) -> Result<Value> {
        let secs = ctx
            .input
            .get("seconds")
            .and_then(|v| v.as_i64())
            .unwrap_or(2)
            .clamp(1, 60);

        let a = ctx
            .step("before", || async { Ok(json!({ "phase": "before" })) })
            .await?;

        ctx.sleep(secs).await.map_err(anyhow::Error::new)?;

        let b = ctx
            .step("after", || async {
                Ok(json!({ "phase": "after", "slept": secs }))
            })
            .await?;

        Ok(json!({ "before": a, "after": b }))
    }
}

/// Live `interval_task` chains allowed per project, unless `FIBER_INTERVAL_MAX_PER_PROJECT`
/// says otherwise.
///
/// Ten perpetual tickers is more than any real use of the demo task and small enough that
/// a project writer cannot turn "create a fiber" — a `writer` operation — into unbounded
/// background work on the API process. The chain stops rather than failing: the fibers
/// already running are legitimate.
const DEFAULT_INTERVAL_MAX_PER_PROJECT: i64 = 10;

/// The cap, parsed and floored. Nonsense and "0" both land on the default rather than on
/// a value that would silently stop every chain (or allow every one).
fn interval_cap(raw: Option<&str>) -> i64 {
    raw.and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_INTERVAL_MAX_PER_PROJECT)
}

/// Whether a chain may extend itself, given how many other live chains its project has.
fn may_reschedule(live_siblings: i64, cap: i64) -> bool {
    live_siblings < cap
}

/// Self-rescheduling chain (memoturn `cron_turn` pattern): each run creates the next fiber.
struct IntervalTask;

#[async_trait]
impl FiberHandler for IntervalTask {
    async fn run(&self, ctx: &mut FiberContext) -> Result<Value> {
        let secs = ctx
            .input
            .get("interval_seconds")
            .and_then(|v| v.as_i64())
            .unwrap_or(60)
            .clamp(5, 86_400);

        let tick = ctx
            .step("tick", || async {
                Ok(json!({
                    "ticked_at": Utc::now().to_rfc3339(),
                }))
            })
            .await?;

        let project_store = ctx.persistence();
        let project_id = ctx.record.project_id;
        let fiber_id = ctx.record.id;
        let input = ctx.input.clone();
        let cap = interval_cap(
            std::env::var("FIBER_INTERVAL_MAX_PER_PROJECT")
                .ok()
                .as_deref(),
        );
        // Inside the memoized step, so the decision is made once per link and a resumed
        // fiber does not create a second successor.
        let next_id = ctx
            .step("reschedule", move || {
                let store = project_store;
                let input = input;
                async move {
                    let live = store
                        .count_live_siblings(project_id, "interval_task", fiber_id)
                        .await?;
                    if !may_reschedule(live, cap) {
                        tracing::warn!(
                            %project_id, live, cap,
                            "interval_task chain stopped: the project is at its live-chain cap \
                             (FIBER_INTERVAL_MAX_PER_PROJECT)"
                        );
                        return Ok(json!(null));
                    }
                    let wake = Utc::now() + chrono::Duration::seconds(secs);
                    let next = store
                        .create(project_id, "interval_task", input, Some(wake))
                        .await?;
                    Ok(json!(next.id.to_string()))
                }
            })
            .await?;

        Ok(json!({ "tick": tick, "next_fiber_id": next_id }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::FiberPersistence;
    use crate::types::{FiberRecord, FiberState, FiberStatus};
    use chrono::DateTime;
    use std::sync::Mutex;
    use uuid::Uuid;

    /// Counts what the chain tried to create, and answers the sibling count a test sets.
    struct CapStore {
        live: i64,
        created: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl FiberPersistence for CapStore {
        async fn save_checkpoint(&self, _record: &FiberRecord) -> Result<()> {
            Ok(())
        }
        async fn append_step(
            &self,
            _fiber_id: Uuid,
            _key: &str,
            _value: &Value,
            _heartbeat_at: Option<DateTime<Utc>>,
        ) -> Result<()> {
            Ok(())
        }
        async fn touch_heartbeat(&self, _id: Uuid) -> Result<()> {
            Ok(())
        }
        async fn create(
            &self,
            _project_id: Uuid,
            name: &str,
            _input: Value,
            _wake_at: Option<DateTime<Utc>>,
        ) -> Result<FiberRecord> {
            self.created.lock().unwrap().push(name.to_string());
            Ok(record())
        }
        async fn count_live_siblings(
            &self,
            _project_id: Uuid,
            _name: &str,
            _exclude: Uuid,
        ) -> Result<i64> {
            Ok(self.live)
        }
    }

    fn record() -> FiberRecord {
        let now = Utc::now();
        FiberRecord {
            id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            name: "interval_task".into(),
            status: FiberStatus::Running,
            input: json!({ "interval_seconds": 60 }),
            state: FiberState::default(),
            result: None,
            error: None,
            attempts: 0,
            wake_at: None,
            heartbeat_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    async fn run_with(live: i64) -> (Value, usize) {
        let store = std::sync::Arc::new(CapStore {
            live,
            created: Mutex::new(Vec::new()),
        });
        let mut ctx = FiberContext::new(record(), store.clone());
        let out = IntervalTask.run(&mut ctx).await.expect("tick");
        let n = store.created.lock().unwrap().len();
        (out, n)
    }

    #[test]
    fn the_cap_falls_back_rather_than_disabling_the_chain() {
        assert_eq!(interval_cap(None), DEFAULT_INTERVAL_MAX_PER_PROJECT);
        assert_eq!(interval_cap(Some("lots")), DEFAULT_INTERVAL_MAX_PER_PROJECT);
        // 0 and negatives would stop every chain on the instance, silently.
        assert_eq!(interval_cap(Some("0")), DEFAULT_INTERVAL_MAX_PER_PROJECT);
        assert_eq!(interval_cap(Some("-3")), DEFAULT_INTERVAL_MAX_PER_PROJECT);
        assert_eq!(interval_cap(Some(" 3 ")), 3);
    }

    #[test]
    fn a_chain_does_not_count_itself_against_the_cap() {
        // Otherwise a cap of one would stop the only chain a project has.
        assert!(may_reschedule(0, 1));
        assert!(!may_reschedule(1, 1));
        assert!(may_reschedule(9, 10));
        assert!(!may_reschedule(10, 10));
        assert!(!may_reschedule(99, 10));
    }

    #[tokio::test]
    async fn a_chain_below_the_cap_schedules_its_successor() {
        let (out, created) = run_with(0).await;
        assert_eq!(created, 1, "the next link must be created");
        assert!(out["next_fiber_id"].is_string(), "{out}");
    }

    #[tokio::test]
    async fn a_project_at_the_cap_stops_the_chain_instead_of_extending_it() {
        let (out, created) = run_with(DEFAULT_INTERVAL_MAX_PER_PROJECT).await;
        assert_eq!(created, 0, "no successor may be created at the cap");
        // The tick itself still succeeded; the chain just ends here.
        assert!(out["tick"].is_object(), "{out}");
        assert!(out["next_fiber_id"].is_null(), "{out}");
    }
}
