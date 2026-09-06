mod access;
mod artifact_util;
mod artifacts;
mod auth;
mod github;
mod github_status;
mod login_guard;
mod otel;
mod retention;
mod routes;
mod state;
mod ws;

use anyhow::Result;
use artifacts::ArtifactBackend;
use clap::Parser;
use fiber_core::{Store, db};
use fiber_durable::{FiberRegistry, FiberScheduler, FiberStore, tasks};
use fiber_scheduler::Scheduler;
use redis::aio::ConnectionManager;
use state::AppState;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::trace::TraceLayer;

#[derive(Parser, Debug)]
#[command(name = "fiber-api", version)]
struct Args {
    #[arg(
        long,
        env = "FIBER_DATABASE_URL",
        default_value = "postgres://fiber:fiber@localhost:15432/fiber"
    )]
    database_url: String,

    #[arg(
        long,
        env = "FIBER_REDIS_URL",
        default_value = "redis://localhost:16379"
    )]
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
    let status_store = store.clone();
    let status_scheduler = scheduler.clone();
    tokio::spawn(async move {
        github_status::report_loop(status_store, status_scheduler).await;
    });
    let agent_cmds = scheduler.clone();
    let redis_url = args.redis_url.clone();
    tokio::spawn(async move {
        agent_cmds.agent_cmds_loop(redis_url).await;
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
        login_guard: Arc::new(login_guard::LoginGuard::new()),
    };

    if args.admin_password == "fiber" {
        tracing::warn!(
            "FIBER_ADMIN_PASSWORD is the default (`fiber`) — change it before exposing this instance"
        );
    }

    let app = routes::router(state)
        .layer(cors_layer())
        .layer(TraceLayer::new_for_http());

    tracing::info!(%args.listen, "fiber-api listening");
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Browser origins allowed to call the API. `FIBER_CORS_ORIGINS` is a comma-separated
/// list; the default covers the local web dev server. `*` opts back into any origin.
fn cors_layer() -> CorsLayer {
    let raw = std::env::var("FIBER_CORS_ORIGINS")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:3100,http://localhost:3100".into());
    // `Access-Control-Allow-Headers: *` does not cover Authorization per the Fetch spec,
    // so name the two headers the SPA actually sends.
    let base = CorsLayer::new().allow_methods(Any).allow_headers([
        axum::http::header::AUTHORIZATION,
        axum::http::header::CONTENT_TYPE,
    ]);
    let entries: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    // A wildcard anywhere in the list means "any origin" (AllowOrigin::list would panic on it).
    if entries.contains(&"*") {
        tracing::warn!("FIBER_CORS_ORIGINS contains `*` — any web origin may call this API");
        return base.allow_origin(Any);
    }
    let origins: Vec<_> = entries
        .into_iter()
        .filter_map(|s| match s.parse() {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::warn!(origin = %s, "ignoring invalid FIBER_CORS_ORIGINS entry");
                None
            }
        })
        .collect();
    tracing::info!(?origins, "CORS allowed origins");
    base.allow_origin(AllowOrigin::list(origins))
}
