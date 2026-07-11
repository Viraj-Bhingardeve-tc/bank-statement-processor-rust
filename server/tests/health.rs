//! Integration test for `/healthz`: exercises the assembled router exactly
//! as `main.rs` builds it, via `tower::ServiceExt::oneshot` — no real
//! socket bound, no Tokio server loop, just the router as a
//! `tower::Service`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use license_server::db;
use license_server::state::AppState;
use tower::ServiceExt;

#[tokio::test]
async fn healthz_returns_200_with_ok_status_body() {
    let config = common::test_config();
    let pool = db::build_pool(&config.database_url, config.database_max_connections).unwrap();
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

#[tokio::test]
async fn unknown_route_returns_404_not_500() {
    let config = common::test_config();
    let pool = db::build_pool(&config.database_url, config.database_max_connections).unwrap();
    let app = license_server::build_router(AppState::new(config, pool));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
