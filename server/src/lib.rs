//! `license-server` — Phase 4 licensing + payment server.
//!
//! **Phase 4E scope** (`PHASE4_DESIGN.md` §13 phase 2): server-account
//! authentication — Argon2 password hashing and secure session tokens
//! (`auth`), real login/session-validation/logout business logic in
//! `AuthService` (`service`), and the HTTP endpoints in front of it
//! (`routes::auth`) — `POST /login`, `POST /logout`, plus the
//! `require_session` protected-route middleware every future account-
//! scoped endpoint will reuse. Builds on Phase 4D's
//! `/activate-license`/`/validate-license`/`/deactivate-license` and
//! `/healthz`/`/readyz`, all unchanged. No Razorpay, payment, webhook, or
//! reconciliation logic — those land in later, separately approved phases.

pub mod auth;
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
        .merge(routes::auth::router(state.clone()))
        .with_state(state)
}
