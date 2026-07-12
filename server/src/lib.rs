//! `license-server` — Phase 4 licensing + payment server.
//!
//! **Phase 4D scope** (`PHASE4_DESIGN.md` §13 phase 2): the real Postgres
//! schema for the non-payment license domain (`db`'s `migrations/`), a
//! `LicenseService` with real activate/validate/deactivate business logic
//! (`service`), and the HTTP endpoints in front of it
//! (`routes::license`) — `POST /activate-license`, `POST
//! /validate-license`, `POST /deactivate-license`, alongside the existing
//! `/healthz`/`/readyz`. No Razorpay, payment, webhook, or reconciliation
//! logic — those land in later, separately approved phases.

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
        .merge(routes::license::router())
        .with_state(state)
}
