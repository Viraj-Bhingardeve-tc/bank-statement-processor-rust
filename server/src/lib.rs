//! `license-server` — Phase 4 licensing + payment server.
//!
//! **Phase 4C.1 scope** (`PHASE4_DESIGN.md` §13 phase 2): axum + tokio
//! wiring, structured logging, environment-based configuration, dependency-
//! injection scaffolding (`AppState`), a Postgres connection pool +
//! migration runner (`db`), repository-layer scaffolding, and health/
//! readiness endpoints. No license, payment, or Razorpay logic — those land
//! in later, separately approved phases, added as new modules under
//! `routes/`/`repository/` following the same patterns `health`/`ready`
//! already establish.

pub mod config;
pub mod db;
pub mod repository;
pub mod routes;
pub mod state;

use axum::Router;
use state::AppState;

/// Builds the full router. Split out from `main` so tests can exercise it
/// directly via `tower::ServiceExt::oneshot`, without binding a real socket
/// or starting a Tokio runtime driven by `main`'s own `#[tokio::main]`.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .merge(routes::health::router())
        .merge(routes::ready::router())
        .with_state(state)
}
