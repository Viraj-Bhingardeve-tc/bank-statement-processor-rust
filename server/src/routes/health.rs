//! `GET /healthz` — liveness probe.
//!
//! Deliberately stateless: no database dependency, since Phase 4B has no
//! database yet. `PHASE4_DESIGN.md` §8.3 distinguishes a plain liveness
//! check (this one — "the process is running") from a readiness check that
//! also verifies DB connectivity; the readiness variant is deferred to
//! whichever later phase actually adds a database connection pool to
//! `AppState`.

use crate::state::AppState;
use axum::{routing::get, Json, Router};
use serde::Serialize;

#[derive(Debug, Serialize, PartialEq, Eq)]
struct HealthResponse {
    status: &'static str,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/healthz", get(health))
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_response_serializes_to_the_documented_shape() {
        let json = serde_json::to_value(HealthResponse { status: "ok" }).unwrap();
        assert_eq!(json, serde_json::json!({ "status": "ok" }));
    }
}
