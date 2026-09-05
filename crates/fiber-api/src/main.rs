mod access;
mod artifacts;
mod artifact_util;
mod auth;
mod github;
mod otel;
mod retention;
mod routes;
mod state;
mod ws;

use anyhow::Result;
use artifacts::ArtifactBackend;
use clap::Parser;
use fiber_core::{db, Store};
use fiber_durable::{tasks, FiberRegistry, FiberScheduler, FiberStore};
use fiber_scheduler::Scheduler;
use redis::aio::ConnectionManager;
use state::AppState;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

#[derive(Parser, Debug)]
#[command(name = "fiber-api")]
struct Args {
    #[arg(long, env = "FIBER_DATABASE_URL", default_value = "postgres://fiber:fiber@localhost:15432/fiber")]
    database_url: String,

    #[arg(long, env = "FIBER_REDIS_URL", default_value = "redis://localhost:16379")]
    redis_url: String,

    #[arg(long, env = "FIBER_LISTEN", default_value = "0.0.0.0:18080")]
    listen: SocketAddr,

    #[arg(long, env = "FIBER_ARTIFACTS_DIR", default_value = "./data/artifacts")]
    artifacts_dir: String,

    #[arg(long, env = "FIBER_ADMIN_USER", default_value = "admin")]
    admin_user: String,

    #[arg(long, env = "FIBER_ADMIN_PASSWORD", default_value = "fiber")]
    admin_password: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _otel = otel::init()?;

    let args = Args::parse();
    std::fs::create_dir_all(&args.artifacts_dir)?;
    fiber_core::secrets::init_from_env();

    let pool = db::connect(&args.database_url).await?;
    db::migrate(&pool).await?;
    let store = Store::new(pool);
    let n = store.backfill_schedule_dues().await?;
    if n > 0 {
        tracing::info!(count = n, "backfilled pipeline next_due_at");
    }
    let admin = store
        .ensure_admin_user(&args.admin_user, &args.admin_password)
        .await?;
    fiber_core::ensure_showcase(&store, admin.id).await?;

    let client = redis::Client::open(args.redis_url.as_str())?;
    let redis = ConnectionManager::new(client).await?;
    let scheduler = Arc::new(Scheduler::new(store.clone(), redis));

    let fiber_store = FiberStore::new(store.pool.clone());
    let registry = FiberRegistry::new();
    tasks::register_builtin_tasks(&registry);
    let fiber_scheduler = Arc::new(FiberScheduler::new(fiber_store, registry));

    let reclaim = scheduler.clone();
    tokio::spawn(async move {
        reclaim.reclaim_loop().await;
    });
    let schedules = scheduler.clone();
    tokio::spawn(async move {
        schedules.schedule_loop().await;
    });
    let events_bus = scheduler.clone();
    let redis_url = args.redis_url.clone();
    tokio::spawn(async move {
        events_bus.events_loop(redis_url).await;
    });
    let fibers = fiber_scheduler.clone();
    tokio::spawn(async move {
        fibers.run_loop().await;
    });

    let artifacts = ArtifactBackend::from_env(&args.artifacts_dir).await?;
    let retention_cfg = retention::RetentionConfig::from_env();
    let retention_store = store.clone();
    let retention_artifacts = artifacts.clone();
    tokio::spawn(async move {
        retention::retention_loop(retention_store, retention_artifacts, retention_cfg).await;
    });

    let state = AppState {
        store,
        scheduler,
        fiber_scheduler,
        artifacts,
    };

    let app = routes::router(state)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .layer(TraceLayer::new_for_http());

    tracing::info!(%args.listen, "fiber-api listening");
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
