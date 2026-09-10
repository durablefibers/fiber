//! Durable fiber runtime (memoturn-style step/stash/sleep), Postgres-backed.
//!
//! Fibers are a parallel primitive to CI DAG `step_runs` — long-running control-plane
//! work with memoized checkpoints. Semantics are **at-least-once**: a step that finishes
//! but crashes before checkpoint may re-run; handlers should make effects idempotent.

pub mod context;
pub mod durability;
pub mod engine;
pub mod http_task;
pub mod registry;
pub mod scheduler;
pub mod store;
pub mod tasks;
pub mod types;

pub use context::{FiberContext, FiberSuspended};
pub use durability::Durability;
pub use engine::{FiberOutcome, run_fiber};
pub use registry::FiberRegistry;
pub use scheduler::FiberScheduler;
pub use store::FiberStore;
pub use types::{FiberRecord, FiberState, FiberStatus};

#[cfg(test)]
mod tests;
