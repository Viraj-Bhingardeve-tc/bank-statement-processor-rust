//! `license-server` — Phase 4 licensing + payment server.
//!
//! **Phase 4I.2 scope** (`PHASE4_DESIGN.md` §8.3 "Operational properties"):
//! production monitoring — a Prometheus-compatible `GET /metrics`
//! (`routes::metrics`) exposing HTTP request count/duration/in-flight
//! (`observability::track_http_metrics`, layered onto the router below),
//! webhook counts (`routes::payment`), reconciliation job run counts
//! (`reconciliation.rs`), and database pool gauges (computed at scrape
//! time). Builds on Phase 4I.1's logging/request-IDs/graceful shutdown, all
//! unchanged — no new behavior on any existing endpoint, `/healthz`
//! untouched.
//!
//! **Phase 4I.1 scope** (`PHASE4_DESIGN.md` §8.3 "Logging"): production
//! observability — every request gets a propagated `x-request-id` and a
//! `tracing` span (`build_router`, below) logged at INFO alongside
//! `main.rs`'s startup/shutdown logging and the JSON-vs-pretty log format
//! split. Builds on Phase 4B-4H's endpoints/reconciliation job/deployment,
//! all unchanged — no new routes, no behavior change to any existing
//! response.
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
pub mod observability;
pub mod rate_limit;
pub mod razorpay;
pub mod reconciliation;
pub mod repository;
pub mod routes;
pub mod service;
pub mod state;

use axum::extract::Request;
use axum::http::HeaderName;
use axum::Router;
use state::AppState;
use tower::ServiceBuilder;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::{DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;

/// Header carrying the per-request id — generated on the way in
/// (`SetRequestIdLayer`), read into the tracing span below, and copied back
/// onto the response (`PropagateRequestIdLayer`) so a client/operator can
/// correlate a response with its server-side log lines.
static REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Builds the full router. Split out from `main` so tests can exercise it
/// directly via `tower::ServiceExt::oneshot`, without binding a real socket
/// or starting a Tokio runtime driven by `main`'s own `#[tokio::main]`.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .merge(routes::health::router())
        .merge(routes::ready::router())
        .merge(routes::metrics::router())
        .merge(routes::license::router(state.clone()))
        .merge(routes::auth::router(state.clone()))
        .merge(routes::payment::router(state.clone()))
        .layer(
            // Order matters: `SetRequestIdLayer` must run before
            // `TraceLayer` so its span-builder can read the id that was
            // just generated, and `PropagateRequestIdLayer` must sit
            // innermost so it copies the id onto the *response* that comes
            // back out of the whole stack (tower-http's own documented
            // pattern for combining the two).
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::new(
                    REQUEST_ID_HEADER.clone(),
                    MakeRequestUuid,
                ))
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(|request: &Request| {
                            let request_id = request
                                .headers()
                                .get(&REQUEST_ID_HEADER)
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("-");
                            tracing::info_span!(
                                "http_request",
                                method = %request.method(),
                                path = %request.uri().path(),
                                request_id = %request_id,
                            )
                        })
                        // Default levels are DEBUG; bump to INFO so this
                        // shows up under the default `RUST_LOG` filter
                        // (`config::AppConfig::from_env`'s
                        // `"license_server=info,tower_http=info"` default
                        // already anticipates this).
                        .on_request(DefaultOnRequest::new().level(Level::INFO))
                        .on_response(DefaultOnResponse::new().level(Level::INFO)),
                )
                .layer(PropagateRequestIdLayer::new(REQUEST_ID_HEADER.clone())),
        )
        // Phase 4I.2: HTTP request-count/duration/in-flight metrics — a
        // separate `.layer()` call (rather than folded into the
        // `ServiceBuilder` above) since it has no ordering dependency on
        // request-id/tracing, only on `MatchedPath` already being in the
        // request's extensions, which axum guarantees for any middleware
        // added via `Router::layer` regardless of relative order.
        .layer(axum::middleware::from_fn(observability::track_http_metrics))
        .with_state(state)
}
