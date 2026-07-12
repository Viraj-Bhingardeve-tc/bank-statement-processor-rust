//! Integration tests for `POST /login`, `POST /logout`, and the
//! `require_session` protected-route middleware.
//!
//! The "missing/malformed bearer token" tests need no database at all
//! (header extraction fails before any repository call) and run by
//! default. The full login→logout flow needs a real, reachable, migrated
//! Postgres — not available in this sandbox — and is `#[ignore]`d with the
//! same reasoning as `license_flow.rs` (Phase 4D) and `ready.rs`
//! (Phase 4C.1). Run explicitly once one exists:
//! `DATABASE_URL=postgres://... cargo test -p license-server --test auth_flow -- --ignored`

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use license_protocol::{LoginRequest, LoginResponse};
use license_server::auth::password::hash_password;
use license_server::state::AppState;
use license_server::{build_router, db};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

async fn post_json<Req: serde::Serialize>(
    app: &axum::Router,
    uri: &str,
    body: &Req,
) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

async fn post_with_auth(app: &axum::Router, uri: &str, auth_header: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().method("POST").uri(uri);
    if let Some(value) = auth_header {
        builder = builder.header("authorization", value);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    response.status()
}

fn app_without_db() -> axum::Router {
    let config = common::test_config();
    let pool = db::build_pool(&config.database_url, config.database_max_connections).unwrap();
    build_router(AppState::new(config, pool))
}

#[tokio::test]
async fn logout_without_an_authorization_header_is_unauthorized() {
    let app = app_without_db();
    let status = post_with_auth(&app, "/logout", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn logout_with_a_malformed_authorization_header_is_unauthorized() {
    let app = app_without_db();
    let status = post_with_auth(&app, "/logout", Some("NotBearer sometoken")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn logout_with_a_well_formed_token_but_unreachable_database_is_a_server_error_not_unauthorized(
) {
    // A DB-connectivity failure must stay distinguishable from "this token
    // is genuinely invalid" — collapsing the two into the same 401 would
    // mislead a caller into thinking their session was rejected rather
    // than that the server couldn't check it. The real "unknown token"
    // case (DB reachable, no matching row) is covered by the `#[ignore]`d
    // `login_then_logout_...` flow below, which needs a real database to
    // exercise that distinction meaningfully.
    let app = app_without_db();
    let status = post_with_auth(&app, "/logout", Some("Bearer not-a-real-token")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

/// Connects, runs migrations, and returns a pool — shared setup for every
/// ignored test in this file.
async fn connected_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .expect("set DATABASE_URL to a reachable Postgres to run this ignored test");
    let pool = db::build_pool(&database_url, 5).expect("DATABASE_URL must be well-formed");
    db::run_migrations(&pool)
        .await
        .expect("migrations must apply cleanly");
    pool
}

async fn seed_user(pool: &PgPool, password: &str) -> (String, i64) {
    let email = format!("test-{}@example.com", Uuid::new_v4());
    let password_hash = hash_password(password).unwrap();
    let user_id: i64 =
        sqlx::query_scalar("INSERT INTO users (email, password_hash) VALUES ($1, $2) RETURNING id")
            .bind(&email)
            .bind(&password_hash)
            .fetch_one(pool)
            .await
            .unwrap();
    (email, user_id)
}

async fn cleanup_user(pool: &PgPool, user_id: i64) {
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn login_then_logout_then_the_old_token_no_longer_works() {
    let pool = connected_pool().await;
    let (email, user_id) = seed_user(&pool, "correct-password").await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));

    let login_req = LoginRequest {
        email: email.clone(),
        password: "correct-password".to_string(),
    };
    let (status, body) = post_json(&app, "/login", &login_req).await;
    assert_eq!(status, StatusCode::OK, "login response: {body}");
    let login_resp: LoginResponse = serde_json::from_value(body).unwrap();
    assert_eq!(login_resp.user_id, user_id.to_string());

    let bearer = format!("Bearer {}", login_resp.session_token);
    let status = post_with_auth(&app, "/logout", Some(&bearer)).await;
    assert_eq!(status, StatusCode::OK);

    // The same token must no longer authenticate a second /logout call.
    let status = post_with_auth(&app, "/logout", Some(&bearer)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    cleanup_user(&pool, user_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn login_with_wrong_password_returns_401_invalid_credentials() {
    let pool = connected_pool().await;
    let (email, user_id) = seed_user(&pool, "correct-password").await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));

    let login_req = LoginRequest {
        email,
        password: "wrong-password".to_string(),
    };
    let (status, body) = post_json(&app, "/login", &login_req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "INVALID_CREDENTIALS");

    cleanup_user(&pool, user_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn login_with_unknown_email_returns_the_same_401_invalid_credentials() {
    let pool = connected_pool().await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));

    let login_req = LoginRequest {
        email: format!("nobody-{}@example.com", Uuid::new_v4()),
        password: "x".to_string(),
    };
    let (status, body) = post_json(&app, "/login", &login_req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "INVALID_CREDENTIALS");
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn logout_with_a_token_the_database_has_never_seen_is_unauthorized() {
    let pool = connected_pool().await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool));

    let status = post_with_auth(
        &app,
        "/logout",
        Some("Bearer a-token-nobody-was-ever-issued"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
