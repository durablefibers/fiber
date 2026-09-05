//! Built-in demo durable tasks.

use crate::context::FiberContext;
use crate::registry::{FiberHandler, FiberRegistry};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};

/// Register demo handlers: `ping`, `sleep_demo`, and `interval_task`.
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

        let project_store = ctx.store().clone();
        let project_id = ctx.record.project_id;
        let input = ctx.input.clone();
        let next_id = ctx
            .step("reschedule", move || {
                let store = project_store;
                let input = input;
                async move {
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
