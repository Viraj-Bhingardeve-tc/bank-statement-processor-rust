//! Integration tests for `PaymentService::reconcile_once`
//! (`PHASE4_DESIGN.md` §12), against the real `Pg*Repository`
//! implementations and a mocked `RazorpayClient` (there is no live
//! Razorpay account to test against — same limitation `razorpay/client.rs`
//! itself documents).
//!
//! All tests here need a real, reachable, migrated Postgres — not
//! available in this sandbox — and are `#[ignore]`d, same pattern as every
//! prior phase's integration tests. The service-level correctness these
//! tests check (idempotency, "no silent healing," status-based gap
//! detection) is *also* covered without a database by
//! `service::payment_service`'s own unit tests; these integration tests
//! additionally prove the real `Pg*Repository` SQL agrees with that
//! logic.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use license_server::db;
use license_server::razorpay::{
    CreateCheckoutRequest, CreateCheckoutResponse, RazorpayClient, RazorpayError, RazorpayPayment,
};
use license_server::repository::license::PgLicenseRepository;
use license_server::repository::payment::{PaymentRepository, PgPaymentRepository};
use license_server::repository::payment_webhook_event::PgPaymentWebhookEventRepository;
use license_server::repository::subscription::PgSubscriptionRepository;
use license_server::service::{PaymentService, ReconciliationSummary};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

/// A `RazorpayClient` that only ever answers `list_payments_since` — every
/// reconciliation test drives the service through the mock, never through
/// a real Razorpay account.
struct MockRazorpayClient {
    payments: Vec<RazorpayPayment>,
}

#[async_trait]
impl RazorpayClient for MockRazorpayClient {
    async fn create_checkout(
        &self,
        _req: CreateCheckoutRequest,
    ) -> Result<CreateCheckoutResponse, RazorpayError> {
        unimplemented!("not exercised by reconciliation tests")
    }

    async fn list_payments_since(
        &self,
        _since: DateTime<Utc>,
    ) -> Result<Vec<RazorpayPayment>, RazorpayError> {
        Ok(self.payments.clone())
    }
}

async fn connected_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .expect("set DATABASE_URL to a reachable Postgres to run this ignored test");
    let pool = db::build_pool(&database_url, 5).expect("DATABASE_URL must be well-formed");
    db::run_migrations(&pool)
        .await
        .expect("migrations must apply cleanly");
    pool
}

fn payment_service_with(pool: PgPool, razorpay_payments: Vec<RazorpayPayment>) -> PaymentService {
    PaymentService::new(
        Arc::new(PgPaymentRepository::new(pool.clone())),
        Arc::new(PgPaymentWebhookEventRepository::new(pool.clone())),
        Arc::new(PgSubscriptionRepository::new(pool.clone())),
        Arc::new(PgLicenseRepository::new(pool)),
        Arc::new(MockRazorpayClient {
            payments: razorpay_payments,
        }),
        2, // matches config::ReconciliationConfig's default max_age_hours
    )
}

/// Seeds a user + `pending_payment` subscription + `pending` payment,
/// returning `(user_id, subscription_id, provider_ref)` for the test to
/// use and clean up afterwards.
async fn seed_pending_purchase(pool: &PgPool) -> (i64, i64, String) {
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
         VALUES ($1, 'yearly', 'pending_payment', now()) RETURNING id",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .unwrap();

    let order_ref = format!("order_{}", Uuid::new_v4());
    sqlx::query(
        "INSERT INTO payments (subscription_id, amount_minor, currency, provider, provider_ref, status) \
         VALUES ($1, 499900, 'INR', 'razorpay', $2, 'pending')",
    )
    .bind(subscription_id)
    .bind(&order_ref)
    .execute(pool)
    .await
    .unwrap();

    (user_id, subscription_id, order_ref)
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
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .ok();
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn reconcile_once_heals_a_payment_no_webhook_ever_arrived_for() {
    let pool = connected_pool().await;
    let (user_id, subscription_id, order_ref) = seed_pending_purchase(&pool).await;

    let service = payment_service_with(
        pool.clone(),
        vec![RazorpayPayment {
            id: "pay_xyz".to_string(),
            order_id: Some(order_ref),
            status: "captured".to_string(),
        }],
    );

    let summary = service.reconcile_once().await.unwrap();
    assert_eq!(
        summary,
        ReconciliationSummary {
            checked: 1,
            healed: 1
        }
    );

    let subscription_status: String =
        sqlx::query_scalar("SELECT status FROM subscriptions WHERE id = $1")
            .bind(subscription_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(subscription_status, "active");

    let license_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM licenses WHERE subscription_id = $1")
            .bind(subscription_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(license_count, 1);

    cleanup_user(&pool, user_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn reconcile_once_is_idempotent_across_repeated_runs() {
    let pool = connected_pool().await;
    let (user_id, subscription_id, order_ref) = seed_pending_purchase(&pool).await;

    let service = payment_service_with(
        pool.clone(),
        vec![RazorpayPayment {
            id: "pay_xyz".to_string(),
            order_id: Some(order_ref),
            status: "captured".to_string(),
        }],
    );

    let first = service.reconcile_once().await.unwrap();
    assert_eq!(
        first,
        ReconciliationSummary {
            checked: 1,
            healed: 1
        }
    );

    // A second run 15 minutes later would see the exact same Razorpay
    // payment (still inside the 2-hour lookback window) — must be a
    // pure no-op against the real database too.
    let second = service.reconcile_once().await.unwrap();
    assert_eq!(
        second,
        ReconciliationSummary {
            checked: 1,
            healed: 0
        }
    );

    let license_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM licenses WHERE subscription_id = $1")
            .bind(subscription_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        license_count, 1,
        "a replayed reconciliation run must not create a second license"
    );

    cleanup_user(&pool, user_id).await;
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn reconcile_once_does_not_guess_at_a_payment_with_no_local_record() {
    let pool = connected_pool().await;

    let service = payment_service_with(
        pool.clone(),
        vec![RazorpayPayment {
            id: "pay_never_seen".to_string(),
            order_id: Some(format!("order_{}", Uuid::new_v4())),
            status: "captured".to_string(),
        }],
    );

    let summary = service.reconcile_once().await.unwrap();
    assert_eq!(
        summary,
        ReconciliationSummary {
            checked: 1,
            healed: 0
        }
    );
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn reconcile_once_syncs_a_failed_payment_status_via_the_real_repository() {
    let pool = connected_pool().await;
    let (user_id, subscription_id, order_ref) = seed_pending_purchase(&pool).await;

    let payment_repository = PgPaymentRepository::new(pool.clone());
    let service = payment_service_with(
        pool.clone(),
        vec![RazorpayPayment {
            id: "pay_xyz".to_string(),
            order_id: Some(order_ref.clone()),
            status: "failed".to_string(),
        }],
    );

    let summary = service.reconcile_once().await.unwrap();
    assert_eq!(
        summary,
        ReconciliationSummary {
            checked: 1,
            healed: 1
        }
    );

    let payment = payment_repository
        .find_by_provider_ref(&order_ref)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(payment.status.as_str(), "failed");

    let subscription_status: String =
        sqlx::query_scalar("SELECT status FROM subscriptions WHERE id = $1")
            .bind(subscription_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        subscription_status, "pending_payment",
        "a failed payment must never activate a subscription"
    );

    cleanup_user(&pool, user_id).await;
}
