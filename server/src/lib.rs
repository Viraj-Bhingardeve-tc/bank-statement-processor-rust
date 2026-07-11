//! `license-server` — Phase 4 licensing + payment server.
//!
//! **Phase 4C.2 scope** (`PHASE4_DESIGN.md` §13 phase 2): axum and tokio
//! wiring, structured logging, environment-based configuration, dependency-
//! injection scaffolding (`AppState`), a Postgres connection pool and
//! migration runner (`db`), domain models (`domain`), repository interfaces
//! and Postgres implementations (`repository`), service-layer scaffolding
//! (`service`), and health/readiness endpoints. No Razorpay, payment, or
//! webhook logic, and no new API endpoints beyond `/healthz`/`/readyz` —
//! those land in later, separately approved phases, added as new modules
//! following the same patterns already established here.

pub mod config;
pub mod db;
pub mod domain;
pub mod repository;
pub mod routes;
pub mod service;
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
