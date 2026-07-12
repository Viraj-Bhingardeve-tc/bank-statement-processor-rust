//! Binary entrypoint. Kept minimal on purpose — all real wiring lives in
//! `lib.rs` so it stays testable without a running process; this file only
//! loads config, sets up logging, connects to the database, runs
//! migrations, and starts serving.

use license_server::config::AppConfig;
use license_server::state::AppState;
use license_server::{build_router, db, reconciliation};

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

    init_logging(&config.log_filter);

    let bind_addr = config.bind_addr;
    tracing::info!(
        addr = %bind_addr,
        version = env!("CARGO_PKG_VERSION"),
        "starting license-server"
    );

    let pool = db::build_pool(&config.database_url, config.database_max_connections)
        .unwrap_or_else(|e| {
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
        std::process::exit(1);
    }
    tracing::info!("database migrations applied");

    let state = AppState::new(config, pool);

    // Runs for the lifetime of the process, independent of HTTP traffic —
    // dropping this handle does not stop the task (it isn't `.abort()`ed),
    // it just means nothing here awaits its (never-returning) completion.
    let _reconciliation_handle = reconciliation::spawn(state.clone());

    let app = build_router(state);

    let listener = match tokio::net::TcpListener::bind(bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, addr = %bind_addr, "failed to bind");
            std::process::exit(1);
        }
    };

    tracing::info!(addr = %bind_addr, "license-server ready, accepting connections");

    if let Err(e) = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!(error = %e, "server exited with error");
        std::process::exit(1);
    }

    tracing::info!("license-server shut down cleanly");
}
