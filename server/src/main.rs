//! Binary entrypoint. Kept minimal on purpose — all real wiring lives in
//! `lib.rs` so it stays testable without a running process; this file only
//! loads config, sets up logging, connects to the database, runs
//! migrations, and starts serving.

use license_server::config::AppConfig;
use license_server::state::AppState;
use license_server::{build_router, db};

#[tokio::main]
async fn main() {
    let config = AppConfig::from_env().unwrap_or_else(|e| {
        eprintln!("invalid configuration: {e}");
        std::process::exit(1);
    });

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            config.log_filter.clone(),
        ))
        .init();

    let bind_addr = config.bind_addr;
    tracing::info!(addr = %bind_addr, "starting license-server");

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

    let app = build_router(AppState::new(config, pool));

    let listener = match tokio::net::TcpListener::bind(bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(error = %e, addr = %bind_addr, "failed to bind");
            std::process::exit(1);
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!(error = %e, "server exited with error");
        std::process::exit(1);
    }
}
