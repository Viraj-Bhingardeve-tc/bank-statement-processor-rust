//! Integration tests for the Admin API (Module 3): `GET /admin/users`,
//! `GET /admin/licenses`, `GET /admin/devices`,
//! `GET /admin/audit/login-history`, `GET /admin/audit/license-validations`,
//! and the four `POST /admin/{license,device}/:id/{revoke,restore,
//! deactivate,activate}` mutations.
//!
//! Same split as `auth_flow.rs`: the "missing/malformed bearer token" case
//! needs no database at all (the `require_admin` guard rejects before any
//! repository call) and runs by default for every one of the nine routes —
//! proving each one is actually wired behind that middleware, not
//! re-proving the middleware's own role logic (already exhaustively unit-
//! tested in `service::auth_service`'s `require_admin` tests). Everything
//! that needs a real row (a real admin/customer account, a real license,
//! a real device) needs a real, reachable, migrated Postgres and is
//! `#[ignore]`d, same reasoning as `auth_flow.rs`/`license_flow.rs`:
//! `DATABASE_URL=postgres://... cargo test -p license-server --test admin_api_flow -- --ignored`

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use license_protocol::LoginRequest;
use license_server::auth::password::hash_password;
use license_server::state::AppState;
use license_server::{build_router, db};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

async fn request_with_auth(
    app: &axum::Router,
    method: &str,
    uri: &str,
    auth_header: Option<&str>,
) -> StatusCode {
    let mut builder = Request::builder().method(method).uri(uri);
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

async fn get_json_with_auth(
    app: &axum::Router,
    uri: &str,
    auth_header: &str,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .header("authorization", auth_header)
                .body(Body::empty())
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

async fn post_json_with_auth(
    app: &axum::Router,
    uri: &str,
    auth_header: &str,
    body: &Value,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", auth_header)
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

fn app_without_db() -> axum::Router {
    let config = common::test_config();
    let pool = db::build_pool(
        config.database.url.expose_secret(),
        config.database.max_connections,
    )
    .unwrap();
    build_router(AppState::new(config, pool))
}

const ADMIN_GET_ROUTES: &[&str] = &[
    "/admin/users",
    "/admin/licenses",
    "/admin/devices",
    "/admin/audit/login-history",
    "/admin/audit/license-validations",
];

const ADMIN_POST_ROUTES: &[&str] = &[
    "/admin/license/1/revoke",
    "/admin/license/1/restore",
    "/admin/device/1/deactivate",
    "/admin/device/1/activate",
];

#[tokio::test]
async fn every_admin_get_route_without_an_authorization_header_is_unauthorized() {
    let app = app_without_db();
    for route in ADMIN_GET_ROUTES {
        let status = request_with_auth(&app, "GET", route, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "route: {route}");
    }
}

#[tokio::test]
async fn every_admin_post_route_without_an_authorization_header_is_unauthorized() {
    let app = app_without_db();
    for route in ADMIN_POST_ROUTES {
        let status = request_with_auth(&app, "POST", route, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "route: {route}");
    }
}

#[tokio::test]
async fn every_admin_route_with_a_malformed_authorization_header_is_unauthorized() {
    let app = app_without_db();
    for route in ADMIN_GET_ROUTES {
        let status = request_with_auth(&app, "GET", route, Some("NotBearer sometoken")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "route: {route}");
    }
    for route in ADMIN_POST_ROUTES {
        let status = request_with_auth(&app, "POST", route, Some("NotBearer sometoken")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "route: {route}");
    }
}

#[tokio::test]
async fn every_admin_route_with_a_well_formed_token_but_unreachable_database_is_a_server_error() {
    // Same distinction `auth_flow.rs` makes for `/logout`/`/subscription`:
    // a DB-connectivity failure must stay distinguishable from "this token
    // (or role) is genuinely invalid".
    let app = app_without_db();
    for route in ADMIN_GET_ROUTES {
        let status = request_with_auth(&app, "GET", route, Some("Bearer not-a-real-token")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "route: {route}");
    }
    for route in ADMIN_POST_ROUTES {
        let status = request_with_auth(&app, "POST", route, Some("Bearer not-a-real-token")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "route: {route}");
    }
}

// ── Real-database flow tests ──────────────────────────────────────────────

async fn connected_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .expect("set DATABASE_URL to a reachable Postgres to run this ignored test");
    let pool = db::build_pool(&database_url, 5).expect("DATABASE_URL must be well-formed");
    db::run_migrations(&pool)
        .await
        .expect("migrations must apply cleanly");
    pool
}

async fn seed_user(pool: &PgPool, password: &str, role: &str) -> (String, i64) {
    let email = format!("test-admin-api-{}@example.com", Uuid::new_v4());
    let password_hash = hash_password(password).unwrap();
    let user_id: i64 = sqlx::query_scalar(
        "INSERT INTO users (email, password_hash, role) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(&email)
    .bind(&password_hash)
    .bind(role)
    .fetch_one(pool)
    .await
    .unwrap();
    (email, user_id)
}

async fn login(app: &axum::Router, email: &str, password: &str) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&LoginRequest {
                        email: email.to_string(),
                        password: password.to_string(),
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    format!("Bearer {}", body["session_token"].as_str().unwrap())
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
async fn a_customer_session_hitting_an_admin_route_is_forbidden_not_unauthorized() {
    let pool = connected_pool().await;
    let (email, user_id) = seed_user(&pool, "correct-password", "customer").await;
    let app = build_router(AppState::new(common::test_config(), pool.clone()));

    let bearer = login(&app, &email, "correct-password").await;
    let status = request_with_auth(&app, "GET", "/admin/users", Some(&bearer)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    cleanup_user(&pool, user_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn admin_can_list_users_and_find_a_seeded_account_by_email_search() {
    let pool = connected_pool().await;
    let (admin_email, admin_id) = seed_user(&pool, "correct-password", "admin").await;
    let (customer_email, customer_id) = seed_user(&pool, "correct-password", "customer").await;
    let app = build_router(AppState::new(common::test_config(), pool.clone()));

    let bearer = login(&app, &admin_email, "correct-password").await;
    let search_term = &customer_email[..customer_email.find('@').unwrap()];
    let uri = format!("/admin/users?search={search_term}");
    let (status, body) = get_json_with_auth(&app, &uri, &bearer).await;

    assert_eq!(status, StatusCode::OK, "response: {body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "response: {body}");
    assert_eq!(items[0]["email"], customer_email);
    assert_eq!(items[0]["role"], "customer");

    cleanup_user(&pool, admin_id).await;
    cleanup_user(&pool, customer_id).await;
}

async fn seed_license(pool: &PgPool, user_id: i64, status: &str) -> (i64, i64) {
    let subscription_id: i64 = sqlx::query_scalar(
        "INSERT INTO subscriptions (user_id, plan_type, status, started_at, auto_renew) \
         VALUES ($1, 'yearly', 'active', now(), true) RETURNING id",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .unwrap();

    let license_id: i64 = sqlx::query_scalar(
        "INSERT INTO licenses (subscription_id, license_key, status, max_devices) \
         VALUES ($1, $2, $3, 2) RETURNING id",
    )
    .bind(subscription_id)
    .bind(format!("TEST-ADMIN-{}", Uuid::new_v4()))
    .bind(status)
    .fetch_one(pool)
    .await
    .unwrap();

    (subscription_id, license_id)
}

async fn cleanup_license(pool: &PgPool, subscription_id: i64, license_id: i64) {
    sqlx::query("DELETE FROM devices WHERE license_id = $1")
        .bind(license_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM licenses WHERE id = $1")
        .bind(license_id)
        .execute(pool)
        .await
        .ok();
    sqlx::query("DELETE FROM subscriptions WHERE id = $1")
        .bind(subscription_id)
        .execute(pool)
        .await
        .ok();
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn admin_can_revoke_then_restore_a_license() {
    let pool = connected_pool().await;
    let (admin_email, admin_id) = seed_user(&pool, "correct-password", "admin").await;
    let (_customer_email, customer_id) = seed_user(&pool, "correct-password", "customer").await;
    let (subscription_id, license_id) = seed_license(&pool, customer_id, "active").await;
    let app = build_router(AppState::new(common::test_config(), pool.clone()));
    let bearer = login(&app, &admin_email, "correct-password").await;

    let (status, body) = post_json_with_auth(
        &app,
        &format!("/admin/license/{license_id}/revoke"),
        &bearer,
        &serde_json::json!({ "reason": "fraud" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "revoke response: {body}");
    assert_eq!(body["status"], "revoked");

    let stored_reason: Option<String> =
        sqlx::query_scalar("SELECT revoked_reason FROM licenses WHERE id = $1")
            .bind(license_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored_reason.as_deref(), Some("fraud"));

    let (status, body) = post_json_with_auth(
        &app,
        &format!("/admin/license/{license_id}/restore"),
        &bearer,
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "restore response: {body}");
    assert_eq!(body["status"], "active");

    let stored_reason: Option<String> =
        sqlx::query_scalar("SELECT revoked_reason FROM licenses WHERE id = $1")
            .bind(license_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored_reason, None);

    cleanup_license(&pool, subscription_id, license_id).await;
    cleanup_user(&pool, admin_id).await;
    cleanup_user(&pool, customer_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn restoring_a_license_that_is_not_revoked_is_a_conflict() {
    let pool = connected_pool().await;
    let (admin_email, admin_id) = seed_user(&pool, "correct-password", "admin").await;
    let (_customer_email, customer_id) = seed_user(&pool, "correct-password", "customer").await;
    let (subscription_id, license_id) = seed_license(&pool, customer_id, "active").await;
    let app = build_router(AppState::new(common::test_config(), pool.clone()));
    let bearer = login(&app, &admin_email, "correct-password").await;

    let (status, body) = post_json_with_auth(
        &app,
        &format!("/admin/license/{license_id}/restore"),
        &bearer,
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "response: {body}");
    assert_eq!(body["error"]["code"], "LICENSE_NOT_REVOKED");

    cleanup_license(&pool, subscription_id, license_id).await;
    cleanup_user(&pool, admin_id).await;
    cleanup_user(&pool, customer_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn revoking_an_unknown_license_id_is_not_found() {
    let pool = connected_pool().await;
    let (admin_email, admin_id) = seed_user(&pool, "correct-password", "admin").await;
    let app = build_router(AppState::new(common::test_config(), pool.clone()));
    let bearer = login(&app, &admin_email, "correct-password").await;

    let (status, body) = post_json_with_auth(
        &app,
        "/admin/license/999999999/revoke",
        &bearer,
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "response: {body}");
    assert_eq!(body["error"]["code"], "LICENSE_NOT_FOUND");

    cleanup_user(&pool, admin_id).await;
}

async fn seed_device(pool: &PgPool, license_id: i64) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO devices (license_id, device_id, machine_fingerprint) \
         VALUES ($1, $2, 'fp') RETURNING id",
    )
    .bind(license_id)
    .bind(Uuid::new_v4())
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn admin_can_deactivate_then_activate_a_device() {
    let pool = connected_pool().await;
    let (admin_email, admin_id) = seed_user(&pool, "correct-password", "admin").await;
    let (_customer_email, customer_id) = seed_user(&pool, "correct-password", "customer").await;
    let (subscription_id, license_id) = seed_license(&pool, customer_id, "active").await;
    let device_id = seed_device(&pool, license_id).await;
    let app = build_router(AppState::new(common::test_config(), pool.clone()));
    let bearer = login(&app, &admin_email, "correct-password").await;

    let (status, body) = post_json_with_auth(
        &app,
        &format!("/admin/device/{device_id}/deactivate"),
        &bearer,
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "deactivate response: {body}");
    assert_eq!(body["status"], "deactivated");

    let (status, body) = get_json_with_auth(
        &app,
        &format!("/admin/devices?license_id={license_id}"),
        &bearer,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list response: {body}");
    assert_eq!(body["items"][0]["is_active"], false, "response: {body}");

    let (status, body) = post_json_with_auth(
        &app,
        &format!("/admin/device/{device_id}/activate"),
        &bearer,
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "activate response: {body}");
    assert_eq!(body["status"], "activated");

    let (status, body) = get_json_with_auth(
        &app,
        &format!("/admin/devices?license_id={license_id}"),
        &bearer,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list response: {body}");
    assert_eq!(body["items"][0]["is_active"], true, "response: {body}");

    cleanup_license(&pool, subscription_id, license_id).await;
    cleanup_user(&pool, admin_id).await;
    cleanup_user(&pool, customer_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn deactivating_an_unknown_device_id_is_not_found() {
    let pool = connected_pool().await;
    let (admin_email, admin_id) = seed_user(&pool, "correct-password", "admin").await;
    let app = build_router(AppState::new(common::test_config(), pool.clone()));
    let bearer = login(&app, &admin_email, "correct-password").await;

    let (status, body) = post_json_with_auth(
        &app,
        "/admin/device/999999999/deactivate",
        &bearer,
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "response: {body}");
    assert_eq!(body["error"]["code"], "DEVICE_NOT_FOUND");

    cleanup_user(&pool, admin_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn admin_can_list_licenses_filtered_by_status() {
    let pool = connected_pool().await;
    let (admin_email, admin_id) = seed_user(&pool, "correct-password", "admin").await;
    let (_customer_email, customer_id) = seed_user(&pool, "correct-password", "customer").await;
    let (active_sub_id, active_license_id) = seed_license(&pool, customer_id, "active").await;
    let (revoked_sub_id, revoked_license_id) = seed_license(&pool, customer_id, "revoked").await;
    let app = build_router(AppState::new(common::test_config(), pool.clone()));
    let bearer = login(&app, &admin_email, "correct-password").await;

    let (status, body) = get_json_with_auth(&app, "/admin/licenses?status=revoked", &bearer).await;
    assert_eq!(status, StatusCode::OK, "response: {body}");
    let items = body["items"].as_array().unwrap();
    let revoked_license_id_str = revoked_license_id.to_string();
    assert!(
        items
            .iter()
            .all(|i| i["license_id"] == revoked_license_id_str),
        "response: {body}"
    );
    assert!(
        items
            .iter()
            .any(|i| i["license_id"] == revoked_license_id_str),
        "response: {body}"
    );

    cleanup_license(&pool, active_sub_id, active_license_id).await;
    cleanup_license(&pool, revoked_sub_id, revoked_license_id).await;
    cleanup_user(&pool, admin_id).await;
    cleanup_user(&pool, customer_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn admin_can_view_login_history_for_a_specific_user() {
    let pool = connected_pool().await;
    let (admin_email, admin_id) = seed_user(&pool, "correct-password", "admin").await;
    let (customer_email, customer_id) = seed_user(&pool, "correct-password", "customer").await;
    let app = build_router(AppState::new(common::test_config(), pool.clone()));

    // A real login for the customer writes one `login_history` row
    // (fire-and-forget — Module 1) before the admin queries it back.
    login(&app, &customer_email, "correct-password").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let admin_bearer = login(&app, &admin_email, "correct-password").await;
    let (status, body) = get_json_with_auth(
        &app,
        &format!("/admin/audit/login-history?user_id={customer_id}"),
        &admin_bearer,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "response: {body}");
    let items = body["items"].as_array().unwrap();
    assert!(!items.is_empty(), "response: {body}");
    assert_eq!(items[0]["user_id"], customer_id.to_string());
    assert_eq!(items[0]["success"], true);

    cleanup_user(&pool, admin_id).await;
    cleanup_user(&pool, customer_id).await;
}
