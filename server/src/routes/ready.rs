//! `GET /readyz` — readiness probe: verifies the process is not just alive
//! (`/healthz`) but can actually reach and query the database.
//!
//! Testing note: the **failure** path (database unreachable → `503`) is
//! fully testable without a real Postgres — `db::build_pool` is lazy, so
//! pointing it at a syntactically valid but unreachable address only fails
//! once a query is attempted, exactly what this endpoint does. The
//! **success** path (database reachable → `200`) needs a real Postgres and
//! is marked `#[ignore]` in this crate's integration tests, to be run
//! explicitly (`cargo test -- --ignored`) against one — see
//! `PHASE4_DESIGN.md` §9's staged testing strategy.

use crate::repository;
use crate::state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

#[derive(Debug, Serialize, PartialEq, Eq)]
struct ReadyResponse {
    status: &'static str,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct NotReadyResponse {
    status: &'static str,
    reason: String,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/readyz", get(ready))
}

/// Production Hardening, Finding H5: this used to put `e.to_string()` — the
/// raw `sqlx::Error` display text — directly into the public JSON body.
/// `/readyz` is unauthenticated by design (an orchestrator/uptime monitor
/// endpoint, same precedent `routes::metrics` documents for itself), so
/// that leaked schema/connection detail to anyone who happened to hit it
/// while the database was degraded — exactly when a real attacker would be
/// probing. The real error is now only ever logged server-side; every
/// caller of this endpoint gets the same fixed, generic reason string
/// regardless of what actually went wrong.
async fn ready(State(state): State<AppState>) -> Response {
    match repository::health::ping(&state.db_pool).await {
        Ok(()) => (StatusCode::OK, Json(ReadyResponse { status: "ready" })).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "database readiness check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(NotReadyResponse {
                    status: "not_ready",
                    reason: "database unreachable".to_string(),
                }),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_response_serializes_to_the_documented_shape() {
        let json = serde_json::to_value(ReadyResponse { status: "ready" }).unwrap();
        assert_eq!(json, serde_json::json!({ "status": "ready" }));
    }

    #[test]
    fn not_ready_response_includes_a_reason() {
        let json = serde_json::to_value(NotReadyResponse {
            status: "not_ready",
            reason: "database unreachable".to_string(),
        })
        .unwrap();
        assert_eq!(json["status"], "not_ready");
        assert_eq!(json["reason"], "database unreachable");
    }
}
