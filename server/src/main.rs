//! Binary entrypoint. Kept minimal on purpose — all real wiring lives in
//! `lib.rs` so it stays testable without a running process; this file only
//! loads config, sets up logging, connects to the database, runs
//! migrations, and starts serving.

use license_server::config::AppConfig;
use license_server::state::AppState;
use license_server::{build_router, db, rate_limit_cleanup, reconciliation};

/// Debug builds get the default human-readable `.pretty()` formatter
/// (multi-line, easy to read in a terminal during local development);
/// release builds get `.json()` (one object per line, the shape a log
/// aggregator/`docker logs` pipeline actually wants — `PHASE4_DESIGN.md`
/// §8.3's "Logging" note). Gated on `cfg(debug_assertions)`, i.e. Cargo's
/// own debug/release split, not a separate env var — nothing to
/// misconfigure in production.
fn init_logging(log_filter: &str) {
    let env_filter = tracing_subscriber::EnvFilter::new(log_filter);

    #[cfg(debug_assertions)]
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .pretty()
        .init();

    #[cfg(not(debug_assertions))]
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .json()
        .init();
}

/// Resolves once both Ctrl+C and (on Unix) `SIGTERM` are handled, so
/// `axum::serve`'s graceful shutdown drains in-flight requests instead of
/// dropping them mid-response on a `docker compose stop`/`restart`
/// (`PHASE4_DESIGN.md` §8.3's "a `docker compose restart server`... doesn't
/// lose committed data, only in-flight requests" — graceful shutdown is
/// what keeps that true for requests that are in flight *at* the moment of
/// restart, not just ones that arrive after).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl+C; starting graceful shutdown"),
        _ = terminate => tracing::info!("received SIGTERM; starting graceful shutdown"),
    }
}

#[tokio::main]
async fn main() {
    let config = AppConfig::from_env().unwrap_or_else(|e| {
        eprintln!("invalid configuration: {e}");
        std::process::exit(1);
    });

    init_logging(&config.server.log_filter);

    let bind_addr = config.server.bind_addr;
    tracing::info!(
        addr = %bind_addr,
        version = env!("CARGO_PKG_VERSION"),
        "starting license-server"
    );

    let pool = db::build_pool(
        config.database.url.expose_secret(),
        config.database.max_connections,
    )
    .unwrap_or_else(|e| {
        // `e` (a `sqlx::Error`) is safe to log here: a malformed-URL parse
        // failure describes *what's wrong* (e.g. "invalid port number"),
        // never echoes the connection string itself — verified directly
        // against `sqlx`'s actual error output, not assumed.
        tracing::error!(error = %e, "malformed DATABASE_URL");
        std::process::exit(1);
    });

    // Migrations run at startup, before the server accepts traffic — a
    // failed migration must stop the process, not leave it serving against
    // a schema it doesn't actually have. This does require the database to
    // be reachable at boot (unlike the lazy pool itself); a database that's
    // merely slow to come up should be retried at the deployment/orchestration
    // level, not silently skipped here.
    if let Err(e) = db::run_migrations(&pool).await {
        tracing::error!(error = %e, "database migration failed");
        // Phase 4L.3 (production validation, CRITICAL): a schema-altering
        // migration (e.g. migrations/0004_add_payment_dispute_support.sql's
        // `ALTER TABLE`) requires table *ownership*, which the restricted
        // `license_server_app` role (migrations/0003_least_privilege_app_role.sql)
        // deliberately never has — by design, not an oversight; granting it
        // would undo the whole point of that migration. An operator who
        // already switched `DATABASE_URL` to that role (the documented,
        // recommended sequence) hits exactly this the moment they deploy a
        // build containing a new ALTER-shaped migration: the raw Postgres
        // "permission denied for table ..." error above is accurate but not
        // self-explanatory, so it's paired with the actual remediation here
        // rather than leaving an operator to rediscover it during an outage.
        if db::is_insufficient_privilege_error(&e) {
            tracing::error!(
                "this looks like a schema-altering migration failing against the restricted \
                 least-privilege role — see server/README.md's \"Database roles and least \
                 privilege\" section: temporarily point DATABASE_URL at the admin/superuser \
                 account, redeploy once to apply pending migrations, then switch back"
            );
        }
        std::process::exit(1);
    }
    tracing::info!("database migrations applied");

    let state = AppState::new(config, pool);

    // Runs for the lifetime of the process, independent of HTTP traffic —
    // dropping this handle does not stop the task (it isn't `.abort()`ed),
    // it just means nothing here awaits its (never-returning) completion.
    let _reconciliation_handle = reconciliation::spawn(state.clone());

    // Production Hardening, Finding H4: bounds the keyed rate limiters'
    // memory by periodically evicting entries idle long enough to be
    // indistinguishable from a fresh one (`rate_limit::RateLimiters::
    // cleanup`, `RATE_LIMIT_ENTRY_TTL_SECONDS`-configured interval). Same
    // never-awaited background-task pattern as reconciliation above.
    let _rate_limit_cleanup_handle = rate_limit_cleanup::spawn(state.clone());

    let app = build_router(state);

    let listener = match tokio::net::TcpListener::bind(bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, addr = %bind_addr, "failed to bind");
            std::process::exit(1);
        }
    };

    tracing::info!(addr = %bind_addr, "license-server ready, accepting connections");

    // `into_make_service_with_connect_info::<SocketAddr>()` (rather than
    // passing `app` directly): populates the `ConnectInfo<SocketAddr>`
    // extension `rate_limit::login_rate_limit` (Phase 4J.6) reads to key
    // `/login`'s per-IP rate limiter. Without this, that extension would
    // never be present on real traffic and the limiter would silently
    // fail open on every request (see that middleware's own doc comment
    // on why it degrades that way instead of erroring).
    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    {
        tracing::error!(error = %e, "server exited with error");
        std::process::exit(1);
    }

    tracing::info!("license-server shut down cleanly");
}
