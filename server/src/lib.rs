//! `license-server` — Phase 4 licensing + payment server.
//!
//! **Phase 4F scope** (`PHASE4_DESIGN.md` §13 phase 3): the payment
//! domain (`domain::payment`/`domain::payment_webhook_event`), a Razorpay
//! client abstraction (`razorpay`), real checkout-creation and webhook-
//! processing business logic in `PaymentService` (`service`), and the HTTP
//! endpoints in front of it (`routes::payment`) — `POST
//! /create-checkout-session` (protected, reuses Phase 4E's
//! `require_session` middleware) and `POST /webhooks/razorpay` (public,
//! HMAC-verified instead — `auth::webhook_signature`). Builds on Phase
//! 4D/4E's endpoints, all unchanged. No automatic reconciliation job,
//! background workers, or desktop changes — those land in later,
//! separately approved phases.

pub mod auth;
pub mod config;
pub mod db;
pub mod domain;
pub mod razorpay;
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
