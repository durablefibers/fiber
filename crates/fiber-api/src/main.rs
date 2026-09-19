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
mod supervisor;
mod ws;

use anyhow::Result;
use artifacts::ArtifactBackend;
use clap::Parser;
use fiber_core::{Store, db};
use fiber_durable::{FiberRegistry, FiberScheduler, FiberStore, tasks};
use fiber_scheduler::Scheduler;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use state::AppState;
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use tower_http::trace::TraceLayer;

/// How long in-flight requests get to finish after SIGTERM before the process exits
/// anyway. Below Compose's `stop_grace_period: 30s`, so the exit is ours and not SIGKILL's.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(25);
/// How long the boot-time Redis probe waits before the process starts without it.
const REDIS_BOOT_PROBE: Duration = Duration::from_secs(5);

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
    let otel = otel::init()?;

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

    // Lazy: the first command connects, and the manager reconnects on its own after
    // that. Connecting eagerly here made an unreachable Redis a crash loop, although
    // leases and scheduling need only Postgres; now the process comes up degraded
    // (`/ready` says so) and heals when Redis does.
    let client = redis::Client::open(args.redis_url.as_str())?;
    let redis = ConnectionManager::new_lazy_with_config(client, ConnectionManagerConfig::new())?;
    let scheduler = Arc::new(Scheduler::new(store.clone(), redis));
    match tokio::time::timeout(REDIS_BOOT_PROBE, scheduler.redis_ping()).await {
        Ok(Ok(())) => tracing::info!("redis reachable"),
        Ok(Err(e)) => tracing::error!(
            error = %e,
            "redis unreachable at boot; starting degraded (no live run streams or \
             cross-replica commands until it is back)"
        ),
        Err(_) => tracing::error!(
            secs = REDIS_BOOT_PROBE.as_secs(),
            "redis did not answer at boot; starting degraded (no live run streams or \
             cross-replica commands until it is back)"
        ),
    }

    let fiber_store = FiberStore::new(store.pool.clone());
    let registry = FiberRegistry::new();
    tasks::register_builtin_tasks(&registry);
    let fiber_scheduler = Arc::new(FiberScheduler::new(fiber_store, registry));

    // Supervised, not bare: a panic in any of these used to kill that task while the
    // process stayed up and `/ready` kept saying ok.
    let health = supervisor::LoopHealth::new();
    {
        let s = scheduler.clone();
        supervisor::supervise("reclaim", health.clone(), move || {
            let s = s.clone();
            async move { s.reclaim_loop().await }
        });
    }
    {
        let s = scheduler.clone();
        supervisor::supervise("schedules", health.clone(), move || {
            let s = s.clone();
            async move { s.schedule_loop().await }
        });
    }
    {
        let s = scheduler.clone();
        let url = args.redis_url.clone();
        supervisor::supervise("events", health.clone(), move || {
            let (s, url) = (s.clone(), url.clone());
            async move { s.events_loop(url).await }
        });
    }
    {
        let st = store.clone();
        let s = scheduler.clone();
        supervisor::supervise("github_status", health.clone(), move || {
            let (st, s) = (st.clone(), s.clone());
            async move { github_status::report_loop(st, s).await }
        });
    }
    {
        let s = scheduler.clone();
        let url = args.redis_url.clone();
        supervisor::supervise("agent_cmds", health.clone(), move || {
            let (s, url) = (s.clone(), url.clone());
            async move { s.agent_cmds_loop(url).await }
        });
    }
    {
        let f = fiber_scheduler.clone();
        supervisor::supervise("fibers", health.clone(), move || {
            let f = f.clone();
            async move { f.run_loop().await }
        });
    }

    let artifacts = ArtifactBackend::from_env(&args.artifacts_dir).await?;
    let retention_cfg = retention::RetentionConfig::from_env();
    {
        let st = store.clone();
        let a = artifacts.clone();
        let retention_fibers = fiber_scheduler.store().clone();
        supervisor::supervise("retention", health.clone(), move || {
            let (st, a, f, cfg) = (
                st.clone(),
                a.clone(),
                retention_fibers.clone(),
                retention_cfg.clone(),
            );
            async move { retention::retention_loop(st, a, f, cfg).await }
        });
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let state = AppState {
        store,
        scheduler,
        fiber_scheduler,
        artifacts,
        login_guard: Arc::new(login_guard::LoginGuard::new()),
        loop_health: health,
        shutdown: shutdown_rx.clone(),
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
    let server = axum::serve(listener, app).with_graceful_shutdown({
        let mut rx = shutdown_rx;
        async move {
            let _ = rx.wait_for(|stop| *stop).await;
        }
    });
    let mut server = tokio::spawn(server.into_future());

    // A deploy used to be a hard kill: no signal handler, so SIGTERM ended the process
    // mid-request and mid-upload, with the OTel batch unflushed. Now the listener
    // closes, requests in flight finish, the WebSocket sessions get a Close frame, and
    // only then does the process leave — within DRAIN_TIMEOUT either way.
    tokio::select! {
        signal = shutdown_signal() => {
            tracing::info!(%signal, "shutdown: stopped accepting connections; draining");
            let _ = shutdown_tx.send(true);
            match tokio::time::timeout(DRAIN_TIMEOUT, &mut server).await {
                Ok(Ok(Ok(()))) => tracing::info!("shutdown: drained"),
                Ok(Ok(Err(e))) => tracing::error!(error = %e, "shutdown: server ended with an error"),
                Ok(Err(e)) => tracing::error!(error = %e, "shutdown: server task failed"),
                Err(_) => {
                    tracing::warn!(
                        secs = DRAIN_TIMEOUT.as_secs(),
                        "shutdown: drain timed out; exiting with connections open"
                    );
                    server.abort();
                }
            }
        }
        ended = &mut server => {
            // Only an accept-loop failure gets here; there is nothing to drain.
            match ended {
                Ok(Ok(())) => tracing::warn!("server stopped without a signal"),
                Ok(Err(e)) => tracing::error!(error = %e, "server failed"),
                Err(e) => tracing::error!(error = %e, "server task failed"),
            }
        }
    }

    tracing::info!("shutdown: flushing telemetry");
    drop(otel);
    tracing::info!("shutdown: complete");
    Ok(())
}

/// Resolves on SIGINT (ctrl-c) everywhere, and on SIGTERM — what `docker stop`,
/// Compose, and systemd send — on unix. Returns which one, for the log.
async fn shutdown_signal() -> &'static str {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "cannot listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => "SIGINT",
        _ = terminate => "SIGTERM",
    }
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
