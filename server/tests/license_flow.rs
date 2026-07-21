//! End-to-end integration tests for the licensing HTTP endpoints, run
//! against a real Postgres — exercises the actual `Pg*` repository
//! implementations and the real migrated schema, not mocks.
//!
//! Not run by default in this sandbox (no local Postgres available — see
//! `PHASE4_DESIGN.md` §9's staged testing strategy, same limitation already
//! noted for `routes::ready`'s tests in Phase 4C.1). Run explicitly once a
//! Postgres is reachable:
//! `DATABASE_URL=postgres://... cargo test -p license-server --test license_flow -- --ignored`

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use license_protocol::{
    ActivateLicenseRequest, ActivateLicenseResponse, DeactivateLicenseRequest,
    DeactivateLicenseResponse, HeartbeatRequest, HeartbeatResponse, LoginRequest, LoginResponse,
    ValidateLicenseRequest, ValidateLicenseResponse,
};
use license_server::auth::password::hash_password;
use license_server::state::AppState;
use license_server::{build_router, db};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

/// Connects, runs migrations, and returns a pool — shared setup for every
/// test in this file.
async fn connected_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .expect("set DATABASE_URL to a reachable Postgres to run this ignored test");
    let pool = db::build_pool(&database_url, 5).expect("DATABASE_URL must be well-formed");
    db::run_migrations(&pool)
        .await
        .expect("migrations must apply cleanly");
    pool
}

/// Inserts a user + active yearly subscription + active license with the
/// given `max_devices`, returning `(license_key, license_id, subscription_id, user_id)`
/// for the test to use and clean up afterwards.
async fn seed_license(pool: &PgPool, max_devices: i32) -> (String, i64, i64, i64) {
    let email = format!("test-{}@example.com", Uuid::new_v4());
    let user_id: i64 =
        sqlx::query_scalar("INSERT INTO users (email, password_hash) VALUES ($1, $2) RETURNING id")
            .bind(&email)
            .bind("hash")
            .fetch_one(pool)
            .await
            .unwrap();

    let subscription_id: i64 = sqlx::query_scalar(
        "INSERT INTO subscriptions (user_id, plan_type, status, started_at) \
         VALUES ($1, 'yearly', 'active', now()) RETURNING id",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .unwrap();

    let license_key = format!("TEST-{}", Uuid::new_v4());
    let license_id: i64 = sqlx::query_scalar(
        "INSERT INTO licenses (subscription_id, license_key, status, max_devices) \
         VALUES ($1, $2, 'active', $3) RETURNING id",
    )
    .bind(subscription_id)
    .bind(&license_key)
    .bind(max_devices)
    .fetch_one(pool)
    .await
    .unwrap();

    (license_key, license_id, subscription_id, user_id)
}

async fn cleanup(pool: &PgPool, license_id: i64, subscription_id: i64, user_id: i64) {
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
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
}

async fn post_json<Req: serde::Serialize>(
    app: &axum::Router,
    uri: &str,
    body: &Req,
) -> (StatusCode, serde_json::Value) {
    post_json_with_auth(app, uri, body, None).await
}

/// Production Hardening, Finding C1: like `post_json`, plus an optional
/// `Authorization` header — used by the optional-session ownership tests
/// below. `post_json` itself is unchanged (delegates here with `None`) so
/// every existing anonymous-call test keeps exercising exactly the same
/// request it always has.
async fn post_json_with_auth<Req: serde::Serialize>(
    app: &axum::Router,
    uri: &str,
    body: &Req,
    auth_header: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(value) = auth_header {
        builder = builder.header("authorization", value);
    }
    let response = app
        .clone()
        .oneshot(
            builder
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

/// Sets a real, loginable password for a user `seed_license` already
/// created (that helper writes a literal, non-hashed `"hash"` placeholder,
/// fine for every existing anonymous-call test but not usable to actually
/// log in) and returns their email, so an ownership test can obtain a real
/// session token for the account that owns a seeded license.
async fn make_loginable(pool: &PgPool, user_id: i64, password: &str) -> String {
    let email: String = sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap();
    let password_hash = hash_password(password).unwrap();
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(user_id)
        .bind(&password_hash)
        .execute(pool)
        .await
        .unwrap();
    email
}

/// A second, unrelated account — used by the "wrong owner" tests below to
/// present a genuinely valid session that just doesn't own the license
/// under test.
async fn seed_other_user(pool: &PgPool, password: &str) -> (i64, String) {
    let email = format!("test-{}@example.com", Uuid::new_v4());
    let password_hash = hash_password(password).unwrap();
    let user_id: i64 =
        sqlx::query_scalar("INSERT INTO users (email, password_hash) VALUES ($1, $2) RETURNING id")
            .bind(&email)
            .bind(&password_hash)
            .fetch_one(pool)
            .await
            .unwrap();
    (user_id, email)
}

async fn cleanup_bare_user(pool: &PgPool, user_id: i64) {
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

async fn login_bearer(app: &axum::Router, email: &str, password: &str) -> String {
    let login_req = LoginRequest {
        email: email.to_string(),
        password: password.to_string(),
    };
    let (status, body) = post_json(app, "/login", &login_req).await;
    assert_eq!(status, StatusCode::OK, "login response: {body}");
    let login_resp: LoginResponse = serde_json::from_value(body).unwrap();
    format!("Bearer {}", login_resp.session_token)
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn activate_then_validate_then_deactivate_full_flow() {
    let pool = connected_pool().await;
    let (license_key, license_id, subscription_id, user_id) = seed_license(&pool, 1).await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));
    let device_id = Uuid::new_v4().to_string();

    let activate_req = ActivateLicenseRequest {
        license_key: license_key.clone(),
        device_id: device_id.clone(),
        machine_fingerprint: "fp-1".to_string(),
        device_label: "test-device".to_string(),
    };
    let (status, body) = post_json(&app, "/activate-license", &activate_req).await;
    assert_eq!(status, StatusCode::OK, "activate response: {body}");
    let activate_resp: ActivateLicenseResponse = serde_json::from_value(body).unwrap();
    assert_eq!(activate_resp.license_id, license_id.to_string());
    assert_eq!(activate_resp.status, "active");
    assert_eq!(activate_resp.subscription_type, "yearly");

    let validate_req = ValidateLicenseRequest {
        license_id: activate_resp.license_id.clone(),
        device_id: device_id.clone(),
        machine_fingerprint: "fp-1".to_string(),
        client_clock: chrono::Utc::now().to_rfc3339(),
    };
    let (status, body) = post_json(&app, "/validate-license", &validate_req).await;
    assert_eq!(status, StatusCode::OK, "validate response: {body}");
    let validate_resp: ValidateLicenseResponse = serde_json::from_value(body).unwrap();
    assert_eq!(validate_resp.status, "active");
    assert!(validate_resp.fingerprint_matched);

    let deactivate_req = DeactivateLicenseRequest {
        license_id: activate_resp.license_id.clone(),
        device_id,
        machine_fingerprint: "fp-1".to_string(),
    };
    let (status, body) = post_json(&app, "/deactivate-license", &deactivate_req).await;
    assert_eq!(status, StatusCode::OK, "deactivate response: {body}");
    let deactivate_resp: DeactivateLicenseResponse = serde_json::from_value(body).unwrap();
    assert_eq!(deactivate_resp.devices_active, 0);

    cleanup(&pool, license_id, subscription_id, user_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn activate_with_unknown_key_returns_404_license_not_found() {
    let pool = connected_pool().await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool));

    let req = ActivateLicenseRequest {
        license_key: format!("NOPE-{}", Uuid::new_v4()),
        device_id: Uuid::new_v4().to_string(),
        machine_fingerprint: "fp".to_string(),
        device_label: "device".to_string(),
    };
    let (status, body) = post_json(&app, "/activate-license", &req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "LICENSE_NOT_FOUND");
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn activate_beyond_max_devices_returns_409_with_existing_device_list() {
    let pool = connected_pool().await;
    let (license_key, license_id, subscription_id, user_id) = seed_license(&pool, 1).await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));

    let first = ActivateLicenseRequest {
        license_key: license_key.clone(),
        device_id: Uuid::new_v4().to_string(),
        machine_fingerprint: "fp-1".to_string(),
        device_label: "device-one".to_string(),
    };
    let (status, _) = post_json(&app, "/activate-license", &first).await;
    assert_eq!(status, StatusCode::OK);

    let second = ActivateLicenseRequest {
        license_key,
        device_id: Uuid::new_v4().to_string(),
        machine_fingerprint: "fp-2".to_string(),
        device_label: "device-two".to_string(),
    };
    let (status, body) = post_json(&app, "/activate-license", &second).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "DEVICE_LIMIT_REACHED");
    assert_eq!(body["error"]["devices"].as_array().unwrap().len(), 1);

    cleanup(&pool, license_id, subscription_id, user_id).await;
}

/// Phase 4J.3 — the regression test for the production readiness audit's
/// HIGH finding #3: two *genuinely concurrent* activation requests for two
/// *different* new `device_id`s, with the license already at
/// `max_devices - 1` active devices (exactly one free slot), must result
/// in exactly one success and one `409 DEVICE_LIMIT_REACHED` — never both
/// succeeding. Before the fix, `ensure_device_slot_available`'s `SELECT
/// COUNT` and the later `INSERT` were separate round trips, so both
/// concurrent requests could observe the same free slot before either had
/// written anything.
#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn concurrent_activation_for_different_devices_at_the_device_limit_allows_exactly_one() {
    let pool = connected_pool().await;
    // max_devices = 2, one device already active ⇒ exactly one free slot,
    // matching the audit's own example (max_devices=2, current active=1).
    let (license_key, license_id, subscription_id, user_id) = seed_license(&pool, 2).await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));

    let first = ActivateLicenseRequest {
        license_key: license_key.clone(),
        device_id: Uuid::new_v4().to_string(),
        machine_fingerprint: "fp-existing".to_string(),
        device_label: "existing-device".to_string(),
    };
    let (status, _) = post_json(&app, "/activate-license", &first).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "seeding the first active device must succeed"
    );

    // Two concurrent requests for two different, brand-new device_ids,
    // racing for the single remaining slot.
    let app_a = app.clone();
    let req_a = ActivateLicenseRequest {
        license_key: license_key.clone(),
        device_id: Uuid::new_v4().to_string(),
        machine_fingerprint: "fp-a".to_string(),
        device_label: "device-a".to_string(),
    };
    let task_a = tokio::spawn(async move { post_json(&app_a, "/activate-license", &req_a).await });

    let app_b = app.clone();
    let req_b = ActivateLicenseRequest {
        license_key: license_key.clone(),
        device_id: Uuid::new_v4().to_string(),
        machine_fingerprint: "fp-b".to_string(),
        device_label: "device-b".to_string(),
    };
    let task_b = tokio::spawn(async move { post_json(&app_b, "/activate-license", &req_b).await });

    let (result_a, result_b) = tokio::join!(task_a, task_b);
    let (status_a, body_a) = result_a.unwrap();
    let (status_b, body_b) = result_b.unwrap();

    let statuses = [status_a, status_b];
    let successes = statuses.iter().filter(|s| **s == StatusCode::OK).count();
    let conflicts = statuses
        .iter()
        .filter(|s| **s == StatusCode::CONFLICT)
        .count();
    assert_eq!(
        successes, 1,
        "expected exactly one success, got statuses {statuses:?} (bodies: {body_a} / {body_b})"
    );
    assert_eq!(
        conflicts, 1,
        "expected exactly one 409 DEVICE_LIMIT_REACHED, got statuses {statuses:?} (bodies: {body_a} / {body_b})"
    );

    let error_body = if status_a == StatusCode::CONFLICT {
        &body_a
    } else {
        &body_b
    };
    assert_eq!(error_body["error"]["code"], "DEVICE_LIMIT_REACHED");

    // The database must agree: exactly 2 active devices, never 3.
    let active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM devices WHERE license_id = $1 AND deactivated_at IS NULL",
    )
    .bind(license_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        active_count, 2,
        "max_devices must never be exceeded, even under concurrent activation"
    );

    cleanup(&pool, license_id, subscription_id, user_id).await;
}

// ── Phase 4J.7: /heartbeat, /refresh-license ─────────────────────────────

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn heartbeat_after_activation_reports_the_current_license_status() {
    let pool = connected_pool().await;
    let (license_key, license_id, subscription_id, user_id) = seed_license(&pool, 1).await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));
    let device_id = Uuid::new_v4().to_string();

    let activate_req = ActivateLicenseRequest {
        license_key,
        device_id: device_id.clone(),
        machine_fingerprint: "fp-1".to_string(),
        device_label: "test-device".to_string(),
    };
    let (status, body) = post_json(&app, "/activate-license", &activate_req).await;
    assert_eq!(status, StatusCode::OK, "activate response: {body}");
    let activate_resp: ActivateLicenseResponse = serde_json::from_value(body).unwrap();

    let heartbeat_req = HeartbeatRequest {
        license_id: activate_resp.license_id.clone(),
        device_id,
    };
    let (status, body) = post_json(&app, "/heartbeat", &heartbeat_req).await;
    assert_eq!(status, StatusCode::OK, "heartbeat response: {body}");
    let heartbeat_resp: HeartbeatResponse = serde_json::from_value(body).unwrap();
    assert_eq!(heartbeat_resp.status, "active");

    cleanup(&pool, license_id, subscription_id, user_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn heartbeat_for_a_device_never_activated_returns_404_device_not_activated() {
    let pool = connected_pool().await;
    let (_license_key, license_id, subscription_id, user_id) = seed_license(&pool, 1).await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));

    let heartbeat_req = HeartbeatRequest {
        license_id: license_id.to_string(),
        device_id: Uuid::new_v4().to_string(),
    };
    let (status, body) = post_json(&app, "/heartbeat", &heartbeat_req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "DEVICE_NOT_ACTIVATED");

    cleanup(&pool, license_id, subscription_id, user_id).await;
}

/// `/refresh-license` reuses `/validate-license`'s exact handler
/// (`routes::license::router`) — this proves it end-to-end over HTTP, not
/// just via the shared `LicenseService::validate` unit tests.
#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn refresh_license_after_activation_behaves_identically_to_validate_license() {
    let pool = connected_pool().await;
    let (license_key, license_id, subscription_id, user_id) = seed_license(&pool, 1).await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));
    let device_id = Uuid::new_v4().to_string();

    let activate_req = ActivateLicenseRequest {
        license_key,
        device_id: device_id.clone(),
        machine_fingerprint: "fp-1".to_string(),
        device_label: "test-device".to_string(),
    };
    let (status, body) = post_json(&app, "/activate-license", &activate_req).await;
    assert_eq!(status, StatusCode::OK, "activate response: {body}");
    let activate_resp: ActivateLicenseResponse = serde_json::from_value(body).unwrap();

    let refresh_req = ValidateLicenseRequest {
        license_id: activate_resp.license_id.clone(),
        device_id,
        machine_fingerprint: "fp-1".to_string(),
        client_clock: chrono::Utc::now().to_rfc3339(),
    };
    let (status, body) = post_json(&app, "/refresh-license", &refresh_req).await;
    assert_eq!(status, StatusCode::OK, "refresh-license response: {body}");
    let refresh_resp: ValidateLicenseResponse = serde_json::from_value(body).unwrap();
    assert_eq!(refresh_resp.status, "active");
    assert!(refresh_resp.fingerprint_matched);

    cleanup(&pool, license_id, subscription_id, user_id).await;
}

/// Regression coverage for the audit's `/heartbeat` requirement that a
/// non-active status is returned as data, not an error — seeds a license
/// that's already `expired` in the database (bypassing `/activate-license`,
/// which itself rejects an already-expired license — this simulates a
/// license that expired *after* the device was activated on it, the
/// realistic path to this state) and confirms both `/heartbeat` and
/// `/validate-license` report `"expired"` with a normal `200`, never a
/// `410`.
#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn heartbeat_and_validate_report_an_expired_license_status_without_erroring() {
    let pool = connected_pool().await;
    let (license_key, license_id, subscription_id, user_id) = seed_license(&pool, 1).await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));
    let device_id = Uuid::new_v4().to_string();

    let activate_req = ActivateLicenseRequest {
        license_key,
        device_id: device_id.clone(),
        machine_fingerprint: "fp-1".to_string(),
        device_label: "test-device".to_string(),
    };
    let (status, _) = post_json(&app, "/activate-license", &activate_req).await;
    assert_eq!(status, StatusCode::OK);

    sqlx::query("UPDATE licenses SET status = 'expired' WHERE id = $1")
        .bind(license_id)
        .execute(&pool)
        .await
        .unwrap();

    let heartbeat_req = HeartbeatRequest {
        license_id: license_id.to_string(),
        device_id: device_id.clone(),
    };
    let (status, body) = post_json(&app, "/heartbeat", &heartbeat_req).await;
    assert_eq!(status, StatusCode::OK, "heartbeat response: {body}");
    let heartbeat_resp: HeartbeatResponse = serde_json::from_value(body).unwrap();
    assert_eq!(heartbeat_resp.status, "expired");

    let validate_req = ValidateLicenseRequest {
        license_id: license_id.to_string(),
        device_id,
        machine_fingerprint: "fp-1".to_string(),
        client_clock: chrono::Utc::now().to_rfc3339(),
    };
    let (status, body) = post_json(&app, "/validate-license", &validate_req).await;
    assert_eq!(status, StatusCode::OK, "validate response: {body}");
    let validate_resp: ValidateLicenseResponse = serde_json::from_value(body).unwrap();
    assert_eq!(validate_resp.status, "expired");

    cleanup(&pool, license_id, subscription_id, user_id).await;
}

// ── Production Hardening, Finding C1 ──────────────────────────────────────
//
// Session/ownership validation for /validate-license, /heartbeat,
// /refresh-license, /deactivate-license — backward-compatible: every test
// above this section calls these endpoints with no Authorization header at
// all and continues to pass unchanged, proving the desktop's real,
// session-less `HttpLicenseClient` flow is untouched. These tests cover the
// two things that changed: an optional session, when present, must be
// cross-checked for ownership; and a `machine_fingerprint` mismatch now
// rejects outright instead of merely being reported.

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn validate_license_succeeds_with_a_bearer_token_that_owns_the_license() {
    let pool = connected_pool().await;
    let (license_key, license_id, subscription_id, user_id) = seed_license(&pool, 1).await;
    let email = make_loginable(&pool, user_id, "correct-password").await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));
    let device_id = Uuid::new_v4().to_string();

    let activate_req = ActivateLicenseRequest {
        license_key,
        device_id: device_id.clone(),
        machine_fingerprint: "fp-1".to_string(),
        device_label: "test-device".to_string(),
    };
    let (status, _) = post_json(&app, "/activate-license", &activate_req).await;
    assert_eq!(status, StatusCode::OK);

    let bearer = login_bearer(&app, &email, "correct-password").await;
    let validate_req = ValidateLicenseRequest {
        license_id: license_id.to_string(),
        device_id,
        machine_fingerprint: "fp-1".to_string(),
        client_clock: chrono::Utc::now().to_rfc3339(),
    };
    let (status, body) =
        post_json_with_auth(&app, "/validate-license", &validate_req, Some(&bearer)).await;
    assert_eq!(status, StatusCode::OK, "validate response: {body}");
    let validate_resp: ValidateLicenseResponse = serde_json::from_value(body).unwrap();
    assert_eq!(validate_resp.status, "active");

    cleanup(&pool, license_id, subscription_id, user_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn validate_license_is_masked_as_device_not_activated_for_a_bearer_token_from_a_different_user(
) {
    let pool = connected_pool().await;
    let (license_key, license_id, subscription_id, user_id) = seed_license(&pool, 1).await;
    let (other_user_id, other_email) = seed_other_user(&pool, "correct-password").await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));
    let device_id = Uuid::new_v4().to_string();

    let activate_req = ActivateLicenseRequest {
        license_key,
        device_id: device_id.clone(),
        machine_fingerprint: "fp-1".to_string(),
        device_label: "test-device".to_string(),
    };
    let (status, _) = post_json(&app, "/activate-license", &activate_req).await;
    assert_eq!(status, StatusCode::OK);

    let bearer = login_bearer(&app, &other_email, "correct-password").await;
    let validate_req = ValidateLicenseRequest {
        license_id: license_id.to_string(),
        device_id,
        machine_fingerprint: "fp-1".to_string(),
        client_clock: chrono::Utc::now().to_rfc3339(),
    };
    let (status, body) =
        post_json_with_auth(&app, "/validate-license", &validate_req, Some(&bearer)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "response: {body}");
    assert_eq!(body["error"]["code"], "DEVICE_NOT_ACTIVATED");

    cleanup(&pool, license_id, subscription_id, user_id).await;
    cleanup_bare_user(&pool, other_user_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn validate_license_with_a_malformed_bearer_token_is_unauthorized() {
    let pool = connected_pool().await;
    let (license_key, license_id, subscription_id, user_id) = seed_license(&pool, 1).await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));
    let device_id = Uuid::new_v4().to_string();

    let activate_req = ActivateLicenseRequest {
        license_key,
        device_id: device_id.clone(),
        machine_fingerprint: "fp-1".to_string(),
        device_label: "test-device".to_string(),
    };
    let (status, _) = post_json(&app, "/activate-license", &activate_req).await;
    assert_eq!(status, StatusCode::OK);

    let validate_req = ValidateLicenseRequest {
        license_id: license_id.to_string(),
        device_id,
        machine_fingerprint: "fp-1".to_string(),
        client_clock: chrono::Utc::now().to_rfc3339(),
    };
    let (status, body) = post_json_with_auth(
        &app,
        "/validate-license",
        &validate_req,
        Some("Bearer not-a-real-token"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "response: {body}");

    cleanup(&pool, license_id, subscription_id, user_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn validate_license_rejects_a_fingerprint_mismatch_with_403() {
    let pool = connected_pool().await;
    let (license_key, license_id, subscription_id, user_id) = seed_license(&pool, 1).await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));
    let device_id = Uuid::new_v4().to_string();

    let activate_req = ActivateLicenseRequest {
        license_key,
        device_id: device_id.clone(),
        machine_fingerprint: "fp-1".to_string(),
        device_label: "test-device".to_string(),
    };
    let (status, _) = post_json(&app, "/activate-license", &activate_req).await;
    assert_eq!(status, StatusCode::OK);

    let validate_req = ValidateLicenseRequest {
        license_id: license_id.to_string(),
        device_id,
        machine_fingerprint: "wrong-fp".to_string(),
        client_clock: chrono::Utc::now().to_rfc3339(),
    };
    let (status, body) = post_json(&app, "/validate-license", &validate_req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "response: {body}");
    assert_eq!(body["error"]["code"], "DEVICE_MISMATCH");

    cleanup(&pool, license_id, subscription_id, user_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn deactivate_license_requires_the_correct_machine_fingerprint() {
    let pool = connected_pool().await;
    let (license_key, license_id, subscription_id, user_id) = seed_license(&pool, 1).await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));
    let device_id = Uuid::new_v4().to_string();

    let activate_req = ActivateLicenseRequest {
        license_key,
        device_id: device_id.clone(),
        machine_fingerprint: "fp-1".to_string(),
        device_label: "test-device".to_string(),
    };
    let (status, _) = post_json(&app, "/activate-license", &activate_req).await;
    assert_eq!(status, StatusCode::OK);

    let wrong_fingerprint_req = DeactivateLicenseRequest {
        license_id: license_id.to_string(),
        device_id: device_id.clone(),
        machine_fingerprint: "wrong-fp".to_string(),
    };
    let (status, body) = post_json(&app, "/deactivate-license", &wrong_fingerprint_req).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "response: {body}");
    assert_eq!(body["error"]["code"], "DEVICE_MISMATCH");

    let correct_fingerprint_req = DeactivateLicenseRequest {
        license_id: license_id.to_string(),
        device_id,
        machine_fingerprint: "fp-1".to_string(),
    };
    let (status, body) = post_json(&app, "/deactivate-license", &correct_fingerprint_req).await;
    assert_eq!(status, StatusCode::OK, "response: {body}");

    cleanup(&pool, license_id, subscription_id, user_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn deactivate_license_is_masked_as_license_not_found_for_a_bearer_token_from_a_different_user(
) {
    let pool = connected_pool().await;
    let (license_key, license_id, subscription_id, user_id) = seed_license(&pool, 1).await;
    let (other_user_id, other_email) = seed_other_user(&pool, "correct-password").await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));
    let device_id = Uuid::new_v4().to_string();

    let activate_req = ActivateLicenseRequest {
        license_key,
        device_id: device_id.clone(),
        machine_fingerprint: "fp-1".to_string(),
        device_label: "test-device".to_string(),
    };
    let (status, _) = post_json(&app, "/activate-license", &activate_req).await;
    assert_eq!(status, StatusCode::OK);

    let bearer = login_bearer(&app, &other_email, "correct-password").await;
    let deactivate_req = DeactivateLicenseRequest {
        license_id: license_id.to_string(),
        device_id,
        machine_fingerprint: "fp-1".to_string(),
    };
    let (status, body) =
        post_json_with_auth(&app, "/deactivate-license", &deactivate_req, Some(&bearer)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "response: {body}");
    assert_eq!(body["error"]["code"], "LICENSE_NOT_FOUND");

    cleanup(&pool, license_id, subscription_id, user_id).await;
    cleanup_bare_user(&pool, other_user_id).await;
}
