//! Integration test for the Phase 4B skeleton: exercises the assembled
//! router exactly as `main.rs` builds it, via `tower::ServiceExt::oneshot`
//! — no real socket bound, no Tokio server loop, just the router as a
//! `tower::Service`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use license_server::config::AppConfig;
use license_server::state::AppState;
use tower::ServiceExt;

fn test_config() -> AppConfig {
    AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        log_filter: "info".to_string(),
    }
}

#[tokio::test]
async fn healthz_returns_200_with_ok_status_body() {
    let app = license_server::build_router(AppState::new(test_config()));

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
    let app = license_server::build_router(AppState::new(test_config()));

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
