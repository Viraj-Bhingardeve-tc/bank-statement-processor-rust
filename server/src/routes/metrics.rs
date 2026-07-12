//! `GET /metrics` — Prometheus-compatible scrape endpoint
//! (`PHASE4_DESIGN.md` §8.3 "Operational properties", Phase 4I.2).
//!
//! Deliberately unauthenticated, matching `/healthz`/`/readyz`'s existing
//! precedent — a Prometheus scraper is another piece of infrastructure, not
//! a customer-facing client, and every value this endpoint exposes is
//! already an aggregate count/duration/gauge, never a per-customer secret
//! or PII (see `observability`'s own doc comment for exactly what is
//! emitted).

use crate::observability::{DB_POOL_CONNECTIONS, DB_POOL_IDLE_CONNECTIONS};
use crate::state::AppState;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

pub fn router() -> Router<AppState> {
    Router::new().route("/metrics", get(metrics))
}

/// Prometheus's own documented content type for the text exposition
/// format — distinct from plain `text/plain` so scrapers that check it
/// strictly still accept this response.
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

async fn metrics(State(state): State<AppState>) -> Response {
    // Computed at scrape time rather than instrumented per-query —
    // `PgPool::size`/`num_idle` are cheap synchronous reads of the pool's
    // own internal counters, not a database round trip (`PHASE4_DESIGN.md`
    // §8.3's "database connection pool metrics if practical" — this is the
    // practical version: no background poller, no extra query against the
    // database itself).
    metrics::gauge!(DB_POOL_CONNECTIONS).set(state.db_pool.size() as f64);
    metrics::gauge!(DB_POOL_IDLE_CONNECTIONS).set(state.db_pool.num_idle() as f64);

    let body = state.metrics_handle.render();
    ([(CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_content_type_matches_the_documented_exposition_format() {
        assert_eq!(
            PROMETHEUS_CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8"
        );
    }
}
