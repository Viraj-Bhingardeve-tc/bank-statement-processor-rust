//! `license-server` — Phase 4 licensing + payment server.
//!
//! **Phase 4G scope** (`PHASE4_DESIGN.md` §13 phase 5): the payment
//! reconciliation scheduler (`reconciliation`) — a `tokio::time::interval`
//! background task, spawned at startup (`main.rs`), that runs
//! `PaymentService::reconcile_once` every 15 minutes as the pull-based
//! backstop for webhooks that never arrived (`PHASE4_DESIGN.md` §12).
//! Reuses the exact same `process_webhook_event` path `routes::payment`'s
//! webhook handler uses, so there is exactly one code path that ever
//! mutates payment/subscription/license state from a Razorpay event,
//! reachable from two triggers. Builds on Phase 4D/4E/4F's endpoints, all
//! unchanged. No new payment features, no desktop changes — those land in
//! later, separately approved phases.

pub mod auth;
pub mod config;
pub mod db;
pub mod domain;
pub mod razorpay;
pub mod reconciliation;
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
        .merge(routes::payment::router(state.clone()))
        .with_state(state)
}
