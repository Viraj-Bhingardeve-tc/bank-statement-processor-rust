//! Integration tests for `/readyz`. See `routes::ready`'s doc comment for
//! why only the failure path runs by default in this environment.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use license_server::config::{AppConfig, DatabaseConfig, Secret};
use license_server::db;
use license_server::state::AppState;
use tower::ServiceExt;

#[tokio::test]
async fn readyz_returns_503_when_database_is_unreachable() {
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
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "not_ready");
    assert!(!json["reason"].as_str().unwrap().is_empty());
}

/// Needs a real, reachable Postgres at `DATABASE_URL` — not run by default
/// in this environment (no local Postgres). Run explicitly once one is
/// available: `DATABASE_URL=postgres://... cargo test -p license-server -- --ignored`.
#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn readyz_returns_200_when_database_is_reachable() {
    let database_url = std::env::var("DATABASE_URL")
        .expect("set DATABASE_URL to a reachable Postgres to run this ignored test");
    let config = AppConfig {
        database: DatabaseConfig {
            url: Secret::new(database_url),
            ..common::test_config().database
        },
        ..common::test_config()
    };
    let pool = db::build_pool(
        config.database.url.expose_secret(),
        config.database.max_connections,
    )
    .unwrap();
    let app = license_server::build_router(AppState::new(config, pool));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}
