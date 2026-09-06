//! Reports run outcomes to GitHub as commit statuses.
//!
//! Driven by the scheduler's event bus rather than the completion path, so a slow or
//! failing GitHub API can never hold up a run. Runs without a commit (manual, scheduled)
//! are ignored.

use fiber_core::Store;
use fiber_proto::RunEvent;
use std::sync::Arc;
use tracing::warn;

pub async fn report_loop(store: Store, scheduler: Arc<fiber_scheduler::Scheduler>) {
    let mut events = scheduler.subscribe();
    loop {
        let payload = match events.recv().await {
            Ok(p) => p,
            // Lagged: the missed events were status transitions we will see again on the
            // next one, and a run's terminal event is what matters.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                warn!(missed = n, "commit status reporter lagged");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let Ok(RunEvent::RunUpdated { run_id, status }) = serde_json::from_str(&payload) else {
            continue;
        };
        if !status.is_terminal() {
            continue;
        }
        match store.get_run(run_id).await {
            Ok(Some(run)) => crate::github::report_run_status(&store, &run).await,
            Ok(None) => {}
            Err(e) => warn!(error = %e, %run_id, "load run for commit status"),
        }
    }
}
