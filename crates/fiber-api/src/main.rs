mod access;
mod artifact_util;
mod artifacts;
mod auth;
mod github;
mod github_status;
mod login_guard;
mod metrics_cache;
mod otel;
mod retention;
mod routes;
mod state;
mod supervisor;
mod ws;

use anyhow::Result;
use artifacts::ArtifactBackend;
use clap::{Parser, Subcommand};
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

/// How long in-flight requests and WebSocket sessions get to finish after SIGTERM before
/// the process exits anyway. Compose's `stop_grace_period` is 30 s and the OTel
/// providers take up to 5 s to flush on drop, so 20 s keeps the exit ours, not SIGKILL's.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(20);
/// Redis client bounds. The manager's defaults retry a lost connection six times with
/// exponential backoff — around 13 s — and `publish_event` is awaited inside handlers
/// that sit under the 30 s request timeout, so with Redis down a cancel or a completion
/// could burn most of its budget on one publish. One retry with these timeouts makes a
/// publish fail in about two seconds instead.
const REDIS_RETRIES: usize = 1;
const REDIS_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const REDIS_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the boot-time Redis probe waits before the process starts without it.
const REDIS_BOOT_PROBE: Duration = Duration::from_secs(5);

/// Loopback: a bare `fiber-api` comes up with a bootstrap admin whose password is in the
/// README, so the address it answers on by default must not be one a network can reach.
/// A deployment sets `FIBER_LISTEN` (Compose and the image do).
const DEFAULT_LISTEN: &str = "127.0.0.1:18080";
/// The bootstrap admin's password when `FIBER_ADMIN_PASSWORD` is not set.
const DEFAULT_ADMIN_PASSWORD: &str = "fiber";

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

    #[arg(long, env = "FIBER_LISTEN", default_value = DEFAULT_LISTEN)]
    listen: SocketAddr,

    #[arg(long, env = "FIBER_ARTIFACTS_DIR", default_value = "./data/artifacts")]
    artifacts_dir: String,

    #[arg(long, env = "FIBER_ADMIN_USER", default_value = "admin")]
    admin_user: String,

    #[arg(long, env = "FIBER_ADMIN_PASSWORD", default_value = DEFAULT_ADMIN_PASSWORD)]
    admin_password: String,

    /// `1` to start on a non-loopback address even though the bootstrap admin can still
    /// log in with the default password. Without it that combination refuses to boot.
    #[arg(long, env = "FIBER_ALLOW_DEFAULT_ADMIN")]
    allow_default_admin: Option<String>,

    /// Seed the Showcase demo project: `auto` (only when the instance has no users yet),
    /// `1` to seed on this boot, `0` never.
    #[arg(long, env = "FIBER_SEED_SHOWCASE", default_value = "auto")]
    seed_showcase: String,

    /// Most expanded steps one pipeline may compile to (matrix cells included).
    #[arg(long, env = "FIBER_MAX_STEPS")]
    max_steps: Option<String>,

    /// Run a maintenance command against the database instead of serving.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Set a user's password and sign them out everywhere, for an operator locked out of
    /// the UI. Needs only FIBER_DATABASE_URL; the server does not have to be running.
    ResetPassword {
        username: String,
        /// Read the new password from stdin (first line). Without it, one is generated
        /// and printed.
        #[arg(long)]
        password_stdin: bool,
    },
}

/// What to do about a bootstrap admin that can log in with the default password.
#[derive(Debug, PartialEq, Eq)]
enum DefaultAdmin {
    /// Nobody can log in with it.
    Absent,
    /// Usable, but only from this machine, or the operator said to start anyway.
    Warn,
    /// Usable from the network. `admin` / `fiber` is in the README.
    Refuse,
}

fn default_admin_exposure(
    listen: std::net::IpAddr,
    default_login_works: bool,
    allowed: bool,
) -> DefaultAdmin {
    if !default_login_works {
        DefaultAdmin::Absent
    } else if listen.is_loopback() || allowed {
        DefaultAdmin::Warn
    } else {
        DefaultAdmin::Refuse
    }
}

/// `FIBER_ALLOW_DEFAULT_ADMIN`: on for `1` or `true`. Compose passes an empty string for
/// an unset variable, so anything else — including empty — is off.
fn flag_is_on(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("1" | "true"))
}

/// `fiber-api reset-password`: no migrations, no server — just the one update.
async fn reset_password(database_url: &str, username: &str, from_stdin: bool) -> Result<()> {
    let (password, generated) = if from_stdin {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        let s = s.trim_end_matches(['\r', '\n']).to_string();
        anyhow::ensure!(!s.is_empty(), "stdin was empty");
        (s, false)
    } else {
        (fiber_core::tokens::generate_password(), true)
    };
    let store = Store::new(db::connect(database_url).await?);
    match store.reset_password(username, &password).await? {
        Some(_) if generated => {
            println!("password for {username} reset; sessions revoked\n{password}")
        }
        Some(_) => println!("password for {username} reset; sessions revoked"),
        None => anyhow::bail!("no user named {username:?}"),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let otel = otel::init()?;

    let args = Args::parse();
    if let Some(Command::ResetPassword {
        username,
        password_stdin,
    }) = &args.command
    {
        return reset_password(&args.database_url, username, *password_stdin).await;
    }
    // Validated here so a typo is a boot failure rather than a limit nobody chose.
    // `fiber_core::dag::max_steps` reads the same variable and warns on its own for the
    // CLI, which has no boot to fail.
    let max_steps =
        fiber_core::dag::max_steps_from(args.max_steps.as_deref()).map_err(anyhow::Error::msg)?;
    // Hand the parsed value to the compiler rather than letting it re-read the
    // environment: clap also accepts `--max-steps`, and two readers disagreeing is a
    // boot line that says one thing and a 400 that says another.
    if let Err(in_force) = fiber_core::dag::set_max_steps(max_steps) {
        tracing::warn!(
            in_force,
            requested = max_steps,
            "step cap was already fixed before it could be set"
        );
    }
    if max_steps != fiber_core::dag::DEFAULT_MAX_STEPS {
        tracing::info!(max_steps, "step cap overridden");
    }
    std::fs::create_dir_all(&args.artifacts_dir)?;
    // A key that is set but unusable stops the boot: carrying on would store secrets in
    // plaintext for an operator who believes they are encrypted.
    fiber_core::secrets::init_from_env()?;

    let pool = db::connect(&args.database_url).await?;
    db::migrate(&pool).await?;
    let store = Store::new(pool);
    // One walk: back-fill schedule dues, and report definitions that no longer compile.
    // An upgrade can tighten a validator, which turns a stored pipeline that was accepted
    // into one that cannot start — say so here rather than on the next push.
    match store.scan_pipelines_at_boot().await {
        Ok(scan) => {
            if scan.backfilled > 0 {
                tracing::info!(count = scan.backfilled, "backfilled pipeline next_due_at");
            }
            for d in &scan.defects {
                tracing::error!(
                    project = %d.project_id, pipeline = %d.pipeline_id, name = %d.name,
                    reason = %d.reason,
                    "stored pipeline no longer compiles and cannot start until it is fixed"
                );
            }
            if !scan.defects.is_empty() {
                tracing::error!(
                    count = scan.defects.len(),
                    "pipelines that will not start; see the lines above"
                );
            }
        }
        // Never worth failing the boot: the back-fill is best-effort and the audit is a
        // report, not a gate. The schedule loop re-derives a missing due time anyway.
        Err(e) => tracing::warn!(error = %e, "could not scan stored pipelines at boot"),
    }
    // Read before `ensure_admin_user`, which creates one: this is what "fresh instance"
    // means for the seeder too, so a deleted demo project stays deleted across restarts.
    let users_existed = store.count_users().await? > 0;
    // Decided before the admin is created, not after: `FIBER_ADMIN_PASSWORD` is applied
    // only to an empty database, so refusing *after* creating `admin` / `fiber` would
    // leave an instance that setting the variable could no longer fix.
    let default_login_works = if users_existed {
        store
            .password_is(&args.admin_user, DEFAULT_ADMIN_PASSWORD)
            .await?
    } else {
        args.admin_password == DEFAULT_ADMIN_PASSWORD
    };
    match default_admin_exposure(
        args.listen.ip(),
        default_login_works,
        flag_is_on(args.allow_default_admin.as_deref()),
    ) {
        DefaultAdmin::Absent => {}
        DefaultAdmin::Warn => tracing::warn!(
            user = %args.admin_user,
            "the bootstrap admin can log in with the default password (`fiber`) — change it before exposing this instance"
        ),
        DefaultAdmin::Refuse if users_existed => anyhow::bail!(
            "refusing to listen on {}: `{user}` can still log in with the default password `fiber`. \
             Change it (`fiber-api reset-password {user}` works without the server), \
             or listen on a loopback address, or set FIBER_ALLOW_DEFAULT_ADMIN=1 to start anyway",
            args.listen,
            user = args.admin_user,
        ),
        DefaultAdmin::Refuse => anyhow::bail!(
            "refusing to listen on {} with the default admin password `fiber`. \
             Set FIBER_ADMIN_PASSWORD, or listen on a loopback address, \
             or set FIBER_ALLOW_DEFAULT_ADMIN=1 to start anyway",
            args.listen,
        ),
    }
    let admin = store
        .ensure_admin_user(&args.admin_user, &args.admin_password)
        .await?;
    // Make the dummy hash now, so the first login for an unknown username does not pay
    // for it and stand out against the ones after.
    tokio::task::spawn_blocking(fiber_core::tokens::dummy_password_hash);
    if fiber_core::seed::should_seed_showcase(Some(&args.seed_showcase), users_existed)
        .map_err(anyhow::Error::msg)?
    {
        fiber_core::ensure_showcase(&store, admin.id).await?;
    }

    // Lazy: the first command connects, and the manager reconnects on its own after
    // that. Connecting eagerly here made an unreachable Redis a crash loop, although
    // leases and scheduling need only Postgres; now the process comes up degraded
    // (`/ready` says so) and heals when Redis does.
    let client = redis::Client::open(args.redis_url.as_str())?;
    let redis_cfg = ConnectionManagerConfig::new()
        .set_number_of_retries(REDIS_RETRIES)
        .set_connection_timeout(Some(REDIS_CONNECT_TIMEOUT))
        .set_response_timeout(Some(REDIS_RESPONSE_TIMEOUT));
    let redis = ConnectionManager::new_lazy_with_config(client, redis_cfg)?;
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
    let (sessions, _no_session) = tokio::sync::watch::channel(());
    // Only the sessions themselves may hold receivers, or `closed()` never resolves.
    drop(_no_session);
    let state = AppState {
        store,
        scheduler,
        fiber_scheduler,
        artifacts,
        login_guard: Arc::new(login_guard::LoginGuard::new()),
        metrics_cache: Arc::new(metrics_cache::MetricsCache::default()),
        loop_health: health,
        shutdown: shutdown_rx.clone(),
        sessions: sessions.clone(),
    };

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
    // closes, requests in flight finish, the WebSocket sessions send a Close frame and
    // end, and only then does the process leave — within DRAIN_TIMEOUT either way.
    tokio::select! {
        signal = shutdown_signal() => {
            let deadline = tokio::time::Instant::now() + DRAIN_TIMEOUT;
            tracing::info!(
                %signal,
                sessions = sessions.receiver_count(),
                "shutdown: stopped accepting connections; draining"
            );
            let _ = shutdown_tx.send(true);
            match tokio::time::timeout_at(deadline, &mut server).await {
                Ok(Ok(Ok(()))) => tracing::info!("shutdown: http drained"),
                Ok(Ok(Err(e))) => tracing::error!(error = %e, "shutdown: server ended with an error"),
                Ok(Err(e)) => tracing::error!(error = %e, "shutdown: server task failed"),
                Err(_) => {
                    tracing::warn!(
                        secs = DRAIN_TIMEOUT.as_secs(),
                        "shutdown: http drain timed out; exiting with requests in flight"
                    );
                    server.abort();
                }
            }
            // The serve future does not cover upgraded connections: each WebSocket session
            // is its own task, and dropping the runtime would cut it off anywhere — before
            // the Close frame, or between marking the agent offline and its cleanup.
            match tokio::time::timeout_at(deadline, sessions.closed()).await {
                Ok(()) => tracing::info!("shutdown: websocket sessions closed"),
                Err(_) => tracing::warn!(
                    open = sessions.receiver_count(),
                    "shutdown: drain timed out with websocket sessions still open"
                ),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_listen_address_is_loopback() {
        let addr: SocketAddr = DEFAULT_LISTEN.parse().unwrap();
        assert!(addr.ip().is_loopback(), "{addr}");
        assert_eq!(addr.port(), 18080);
    }

    #[test]
    fn a_default_admin_login_is_refused_on_any_address_a_network_can_reach() {
        use std::net::IpAddr;
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        for public in [
            "0.0.0.0",
            "::",
            "192.168.1.10",
            "10.0.0.5",
            "::ffff:127.0.0.1",
        ] {
            assert_eq!(
                default_admin_exposure(ip(public), true, false),
                DefaultAdmin::Refuse,
                "{public}"
            );
            // The operator's explicit override, and a changed password, both start.
            assert_eq!(
                default_admin_exposure(ip(public), true, true),
                DefaultAdmin::Warn
            );
            assert_eq!(
                default_admin_exposure(ip(public), false, false),
                DefaultAdmin::Absent
            );
        }
        for local in ["127.0.0.1", "127.8.8.8", "::1"] {
            assert_eq!(
                default_admin_exposure(ip(local), true, false),
                DefaultAdmin::Warn,
                "{local}"
            );
            assert_eq!(
                default_admin_exposure(ip(local), false, false),
                DefaultAdmin::Absent
            );
        }
    }

    #[test]
    fn the_override_is_on_only_when_it_says_so() {
        for on in ["1", "true", " 1 "] {
            assert!(flag_is_on(Some(on)), "{on:?}");
        }
        for off in [
            None,
            Some(""),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("yes"),
        ] {
            assert!(!flag_is_on(off), "{off:?}");
        }
    }

    #[test]
    fn with_no_subcommand_the_binary_still_serves() {
        let args = Args::try_parse_from(["fiber-api"]).unwrap();
        assert!(args.command.is_none());
    }

    #[test]
    fn reset_password_takes_a_username_and_an_optional_stdin_flag() {
        let args = Args::try_parse_from(["fiber-api", "reset-password", "admin"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::ResetPassword { ref username, password_stdin: false }) if username == "admin"
        ));
        let args =
            Args::try_parse_from(["fiber-api", "reset-password", "admin", "--password-stdin"])
                .unwrap();
        assert!(matches!(
            args.command,
            Some(Command::ResetPassword {
                password_stdin: true,
                ..
            })
        ));
        assert!(Args::try_parse_from(["fiber-api", "reset-password"]).is_err());
    }
}
