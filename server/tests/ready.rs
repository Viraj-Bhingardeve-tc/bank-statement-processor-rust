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
    // Production Hardening, Finding H5: a fixed, generic reason — never
    // the raw sqlx::Error text (schema/connection detail) that used to be
    // placed directly in this unauthenticated, public response body.
    assert_eq!(json["reason"], "database unreachable");
}

/// Production Hardening, Finding H5 — the regression test proving the fix:
/// no internal SQL/connection error text (a real `sqlx::Error`'s `Display`
/// output always mentions at least one of these) appears anywhere in the
/// response body, not just that the `reason` field matches exactly.
#[tokio::test]
async fn readyz_response_body_never_contains_internal_database_error_text() {
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
    let body_text = String::from_utf8(body.to_vec()).unwrap();
    let body_lower = body_text.to_lowercase();

    // The exact, complete expected body — the strongest possible proof
    // nothing else (a stray field, extra detail) snuck in alongside it.
    let json: serde_json::Value = serde_json::from_str(&body_text).unwrap();
    assert_eq!(
        json,
        serde_json::json!({ "status": "not_ready", "reason": "database unreachable" })
    );

    // Belt-and-suspenders: none of the substrings a real sqlx/Postgres
    // connection error's Display text would contain (host, port, driver
    // name, connection-refused wording, ...) appear anywhere in the body.
    for forbidden in [
        "sqlx",
        "postgres",
        "connect",
        "refused",
        "127.0.0.1",
        "error:",
        "nonexistent_db",
    ] {
        assert!(
            !body_lower.contains(forbidden),
            "response body must never leak internal database error detail; found {forbidden:?} in {body_text:?}"
        );
    }
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
