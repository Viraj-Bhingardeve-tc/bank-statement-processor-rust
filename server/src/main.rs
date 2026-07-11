//! Binary entrypoint. Kept minimal on purpose — all real wiring lives in
//! `lib.rs` so it stays testable without a running process; this file only
//! loads config, sets up logging, and starts serving.

use license_server::config::AppConfig;
use license_server::state::AppState;

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

    let app = license_server::build_router(AppState::new(config));

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
