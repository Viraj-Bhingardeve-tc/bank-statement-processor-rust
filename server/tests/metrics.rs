//! Integration tests for `GET /metrics` (Phase 4I.2 —
//! `PHASE4_DESIGN.md` §8.3 "Operational properties"). Exercises the
//! assembled router exactly as `main.rs` builds it, via
//! `tower::ServiceExt::oneshot` — no real socket bound, same pattern
//! `tests/health.rs`/`tests/ready.rs` already established.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use license_server::db;
use license_server::state::AppState;
use tower::ServiceExt;

#[tokio::test]
async fn metrics_endpoint_returns_200_with_the_prometheus_content_type() {
    let config = common::test_config();
    let pool = db::build_pool(
        config.database.url.expose_secret(),
        config.database.max_connections,
    )
    .unwrap();
    let app = license_server::build_router(AppState::new(config, pool));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap();
    assert_eq!(content_type, "text/plain; version=0.0.4; charset=utf-8");
}

/// The database pool gauges are computed synchronously inside the
/// `/metrics` handler itself (`routes::metrics::metrics`), so — unlike the
/// HTTP-request counters below — they're guaranteed present on the very
/// first scrape, with no warmup request needed.
#[tokio::test]
async fn metrics_endpoint_exposes_database_pool_gauges() {
    let config = common::test_config();
    let pool = db::build_pool(
        config.database.url.expose_secret(),
        config.database.max_connections,
    )
    .unwrap();
    let app = license_server::build_router(AppState::new(config, pool));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    assert!(
        text.contains("db_pool_connections"),
        "expected db_pool_connections in:\n{text}"
    );
    assert!(
        text.contains("db_pool_idle_connections"),
        "expected db_pool_idle_connections in:\n{text}"
    );
}

/// HTTP request metrics are only registered once a request has actually
/// completed a full round trip through `observability::track_http_metrics`
/// — a warmup request (`/healthz`) exercises that before the scrape, so
/// this test doesn't depend on `/metrics` counting itself.
#[tokio::test]
async fn metrics_endpoint_exposes_http_request_metrics_after_a_warmup_request() {
    let config = common::test_config();
    let pool = db::build_pool(
        config.database.url.expose_secret(),
        config.database.max_connections,
    )
    .unwrap();
    let app = license_server::build_router(AppState::new(config, pool));

    let warmup = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(warmup.status(), StatusCode::OK);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    assert!(
        text.contains("http_requests_total"),
        "expected http_requests_total in:\n{text}"
    );
    assert!(
        text.contains("http_request_duration_seconds"),
        "expected http_request_duration_seconds in:\n{text}"
    );
    assert!(
        text.contains("http_requests_in_flight"),
        "expected http_requests_in_flight in:\n{text}"
    );
    // The warmup request matched `/healthz` specifically — confirms labels
    // are the matched route pattern, not just any bare metric name.
    assert!(
        text.contains(r#"path="/healthz""#),
        "expected a /healthz-labeled series in:\n{text}"
    );
}

#[tokio::test]
async fn healthz_is_unchanged_by_the_metrics_middleware() {
    let config = common::test_config();
    let pool = db::build_pool(
        config.database.url.expose_secret(),
        config.database.max_connections,
    )
    .unwrap();
    let app = license_server::build_router(AppState::new(config, pool));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json, serde_json::json!({ "status": "ok" }));
}
