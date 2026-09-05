use crate::artifacts::ArtifactBackend;
use fiber_core::Store;
use fiber_durable::FiberScheduler;
use fiber_scheduler::Scheduler;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub scheduler: Arc<Scheduler>,
    pub fiber_scheduler: Arc<FiberScheduler>,
    pub artifacts: ArtifactBackend,
}
