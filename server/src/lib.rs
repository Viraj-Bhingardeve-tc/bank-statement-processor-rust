//! `license-server` — Phase 4 licensing + payment server.
//!
//! **Phase 4B scope only** (`PHASE4_DESIGN.md` §13 phase 2): axum + tokio
//! wiring, structured logging, environment-based configuration, dependency-
//! injection scaffolding (`AppState`), and a health endpoint. No license,
//! payment, or Razorpay logic — those land in later, separately approved
//! phases, added as new modules under `routes/` following the same
//! `router() -> Router<AppState>` pattern `routes::health` already
//! establishes.

pub mod config;
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
        .with_state(state)
}
