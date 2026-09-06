use crate::artifacts::ArtifactBackend;
use crate::login_guard::LoginGuard;
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
    pub login_guard: Arc<LoginGuard>,
}
