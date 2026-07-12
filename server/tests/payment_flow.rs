//! Integration tests for `POST /create-checkout-session` and
//! `POST /webhooks/razorpay`.
//!
//! Signature verification and the auth-requirement on
//! `/create-checkout-session` need no database at all (both fail before
//! any repository call) and run by default. Tests that need a real
//! Razorpay-webhook-to-license flow need a real, reachable, migrated
//! Postgres — not available in this sandbox — and are `#[ignore]`d, same
//! pattern as every prior phase's integration tests.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hmac::{Hmac, Mac};
use license_protocol::{CreateCheckoutSessionRequest, LoginRequest, LoginResponse};
use license_server::auth::password::hash_password;
use license_server::state::AppState;
use license_server::{build_router, db};
use serde_json::json;
use sha2::Sha256;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn app_without_db() -> axum::Router {
    let config = common::test_config();
    let pool = db::build_pool(&config.database_url, config.database_max_connections).unwrap();
    build_router(AppState::new(config, pool))
}

async fn post_webhook(app: &axum::Router, body: &[u8], signature: Option<&str>) -> StatusCode {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/webhooks/razorpay")
        .header("content-type", "application/json");
    if let Some(sig) = signature {
        builder = builder.header("x-razorpay-signature", sig);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_vec())).unwrap())
        .await
        .unwrap();
    response.status()
}

#[tokio::test]
async fn webhook_without_a_signature_header_is_unauthorized() {
    let app = app_without_db();
    let body = br#"{"event":"payment.captured","payload":{}}"#;
    let status = post_webhook(&app, body, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webhook_with_no_configured_secret_is_unauthorized_even_with_a_signature() {
    // `common::test_config()` leaves razorpay_webhook_secret unset — every
    // webhook call must fail closed, not silently accept anything, when
    // there's no secret to verify against (PHASE4_DESIGN.md §5).
    let app = app_without_db();
    let body = br#"{"event":"payment.captured","payload":{}}"#;
    let fake_signature = sign("whatever", body);
    let status = post_webhook(&app, body, Some(&fake_signature)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webhook_with_a_tampered_body_is_unauthorized() {
    let secret = "whsec_test_secret";
    let config = license_server::config::AppConfig {
        razorpay_webhook_secret: Some(secret.to_string()),
        ..common::test_config()
    };
    let pool = db::build_pool(&config.database_url, config.database_max_connections).unwrap();
    let app = build_router(AppState::new(config, pool));

    let signed_body = br#"{"event":"payment.captured","payload":{}}"#;
    let signature = sign(secret, signed_body);
    let tampered_body = br#"{"event":"payment.failed","payload":{}}"#;

    let status = post_webhook(&app, tampered_body, Some(&signature)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn create_checkout_session_without_a_bearer_token_is_unauthorized() {
    let app = app_without_db();
    let req = CreateCheckoutSessionRequest {
        plan_type: "yearly".to_string(),
    };
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/create-checkout-session")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ── Tests requiring a real Postgres ─────────────────────────────────────

async fn connected_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .expect("set DATABASE_URL to a reachable Postgres to run this ignored test");
    let pool = db::build_pool(&database_url, 5).expect("DATABASE_URL must be well-formed");
    db::run_migrations(&pool)
        .await
        .expect("migrations must apply cleanly");
    pool
}

async fn seed_user_and_login(app: &axum::Router, pool: &PgPool, password: &str) -> (i64, String) {
    let email = format!("test-{}@example.com", Uuid::new_v4());
    let password_hash = hash_password(password).unwrap();
    let user_id: i64 =
        sqlx::query_scalar("INSERT INTO users (email, password_hash) VALUES ($1, $2) RETURNING id")
            .bind(&email)
            .bind(&password_hash)
            .fetch_one(pool)
            .await
            .unwrap();

    let login_req = LoginRequest {
        email,
        password: password.to_string(),
    };
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&login_req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let login_resp: LoginResponse = serde_json::from_slice(&bytes).unwrap();

    (user_id, login_resp.session_token)
}

async fn cleanup_user(pool: &PgPool, user_id: i64) {
    sqlx::query(
        "DELETE FROM licenses WHERE subscription_id IN (SELECT id FROM subscriptions WHERE user_id = $1)",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .ok();
    sqlx::query(
        "DELETE FROM payments WHERE subscription_id IN (SELECT id FROM subscriptions WHERE user_id = $1)",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .ok();
    sqlx::query("DELETE FROM subscriptions WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
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
async fn create_checkout_session_without_razorpay_configured_returns_502_provider_error() {
    let pool = connected_pool().await;
    let config = common::test_config();
    let app = build_router(AppState::new(config, pool.clone()));

    let (user_id, token) = seed_user_and_login(&app, &pool, "correct-password").await;

    let req = CreateCheckoutSessionRequest {
        plan_type: "yearly".to_string(),
    };
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/create-checkout-session")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(serde_json::to_vec(&req).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], "PROVIDER_ERROR");

    // create_checkout_session still creates the subscription row before
    // calling out to Razorpay — clean it up alongside the user.
    cleanup_user(&pool, user_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn a_valid_webhook_is_processed_and_a_replay_is_idempotent() {
    let pool = connected_pool().await;
    let secret = "whsec_integration_test";
    let config = license_server::config::AppConfig {
        razorpay_webhook_secret: Some(secret.to_string()),
        ..common::test_config()
    };
    let app = build_router(AppState::new(config, pool.clone()));

    let email = format!("test-{}@example.com", Uuid::new_v4());
    let user_id: i64 =
        sqlx::query_scalar("INSERT INTO users (email, password_hash) VALUES ($1, $2) RETURNING id")
            .bind(&email)
            .bind("hash")
            .fetch_one(&pool)
            .await
            .unwrap();
    let subscription_id: i64 = sqlx::query_scalar(
        "INSERT INTO subscriptions (user_id, plan_type, status, started_at) \
         VALUES ($1, 'yearly', 'pending_payment', now()) RETURNING id",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let order_ref = format!("order_{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO payments (subscription_id, amount_minor, currency, provider, provider_ref, status) \
         VALUES ($1, 499900, 'INR', 'razorpay', $2, 'pending')",
    )
    .bind(subscription_id)
    .bind(&order_ref)
    .execute(&pool)
    .await
    .unwrap();

    let body = json!({
        "event": "payment.captured",
        "payload": { "payment": { "entity": { "id": "pay_xyz", "order_id": order_ref } } }
    });
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let signature = sign(secret, &body_bytes);

    let status_first = post_webhook(&app, &body_bytes, Some(&signature)).await;
    assert_eq!(status_first, StatusCode::OK);

    let subscription_status: String =
        sqlx::query_scalar("SELECT status FROM subscriptions WHERE id = $1")
            .bind(subscription_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(subscription_status, "active");

    let license_count_after_first: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM licenses WHERE subscription_id = $1")
            .bind(subscription_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(license_count_after_first, 1);

    // Replay the identical webhook delivery (same event_id, since no
    // X-Razorpay-Event-Id header means the fallback hash is derived from
    // this exact body) — must be a pure no-op.
    let status_second = post_webhook(&app, &body_bytes, Some(&signature)).await;
    assert_eq!(status_second, StatusCode::OK);

    let license_count_after_second: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM licenses WHERE subscription_id = $1")
            .bind(subscription_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        license_count_after_second, 1,
        "replay must not create a second license"
    );

    cleanup_user(&pool, user_id).await;
}
