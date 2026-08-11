//! `payments` table access (`LICENSE_DATABASE_SCHEMA.md` §1).
//!
//! **Known simplification (Phase 4F):** a recurring `subscription.charged`
//! webhook (a monthly/yearly plan's renewal charge) updates and reuses the
//! *same* `payments` row created at initial checkout (matched by
//! `provider_ref` = the Razorpay subscription id) rather than inserting a
//! new ledger row per billing cycle. `LICENSE_DATABASE_SCHEMA.md` §1
//! doesn't have a column linking a `payments` row to "which Razorpay
//! subscription this recurring charge belongs to" independent of a fresh
//! per-charge payment id, and adding one wasn't part of this phase's
//! scope — a full per-cycle payment history is a reasonable follow-up, not
//! implemented here. Flagged in `PHASE4_IMPLEMENTATION`-equivalent notes
//! for this phase, not silently assumed complete.

use crate::domain::{NewPayment, Payment, PaymentStatus};
use crate::repository::error::RepositoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::str::FromStr;

#[async_trait]
pub trait PaymentRepository: Send + Sync {
    async fn insert(&self, new_payment: NewPayment) -> Result<Payment, RepositoryError>;
    /// Correlates an inbound activation webhook event back to the
    /// `payments` row it concerns — see this module's doc comment for
    /// exactly what `provider_ref` holds for each plan type.
    async fn find_by_provider_ref(
        &self,
        provider_ref: &str,
    ) -> Result<Option<Payment>, RepositoryError>;
    /// Correlates an inbound refund/dispute webhook event back to the
    /// `payments` row it concerns (Phase 4K.2). `refund.*`/
    /// `payment.dispute.*` webhooks only ever carry the real Razorpay
    /// payment id, never `provider_ref`'s checkout-time payment-link/
    /// subscription id — see `gateway_payment_id`'s doc comment on
    /// `domain::Payment`. Same "more than one match is an error, never
    /// silently pick one" contract `find_by_provider_ref` has (Finding
    /// H2) — see [`single_payment_or_duplicate_gateway_error`] and
    /// migration `0009`.
    async fn find_by_gateway_payment_id(
        &self,
        gateway_payment_id: &str,
    ) -> Result<Option<Payment>, RepositoryError>;
    async fn update_status(&self, id: i64, status: PaymentStatus) -> Result<(), RepositoryError>;
    /// Records the real Razorpay payment id once an activating webhook's
    /// payload supplies one (Phase 4K.2) — not called by
    /// `PaymentWebhookEventRepository::claim_and_apply`'s real Postgres
    /// path, which sets it inline within its own transaction for the same
    /// reason `update_status` isn't called from there either (see that
    /// trait's doc comment); exists so this repository's contract stays
    /// complete and independently testable.
    async fn record_gateway_payment_id(
        &self,
        id: i64,
        gateway_payment_id: &str,
    ) -> Result<(), RepositoryError>;
}

pub struct PgPaymentRepository {
    pool: PgPool,
}

impl PgPaymentRepository {
    pub fn new(pool: PgPool) -> Self {
        PgPaymentRepository { pool }
    }
}

#[derive(sqlx::FromRow)]
struct PaymentRow {
    id: i64,
    subscription_id: i64,
    amount_minor: i64,
    currency: String,
    provider: String,
    provider_ref: Option<String>,
    gateway_payment_id: Option<String>,
    status: String,
    created_at: DateTime<Utc>,
}

impl TryFrom<PaymentRow> for Payment {
    type Error = RepositoryError;

    fn try_from(row: PaymentRow) -> Result<Self, Self::Error> {
        Ok(Payment {
            id: row.id,
            subscription_id: row.subscription_id,
            amount_minor: row.amount_minor,
            currency: row.currency,
            provider: row.provider,
            provider_ref: row.provider_ref,
            gateway_payment_id: row.gateway_payment_id,
            status: PaymentStatus::from_str(&row.status).map_err(RepositoryError::InvalidData)?,
            created_at: row.created_at,
        })
    }
}

/// Interprets however many rows matched a `provider_ref` lookup: zero →
/// `None`, exactly one → that row, more than one →
/// `RepositoryError::DuplicateProviderReference` (Production Hardening,
/// Finding H2) — never silently picks one. Split out as its own pure,
/// `sqlx`-free function so this decision is unit-testable directly: once
/// migration `0008`'s partial `UNIQUE` index is applied, a real duplicate
/// row can never exist in a freshly migrated database, so a test can't
/// arrange one through the real repository at all — this is the only
/// practical way to exercise the "duplicate" branch.
fn single_payment_or_duplicate_error(
    rows: Vec<PaymentRow>,
    provider_ref: &str,
) -> Result<Option<Payment>, RepositoryError> {
    match rows.len() {
        0 => Ok(None),
        1 => rows.into_iter().next().map(Payment::try_from).transpose(),
        _ => Err(RepositoryError::DuplicateProviderReference(
            provider_ref.to_string(),
        )),
    }
}

/// [`single_payment_or_duplicate_error`]'s twin for `gateway_payment_id`
/// lookups (end-to-end payment testing pass, Phase 4N). `find_by_gateway_payment_id`
/// used to `ORDER BY created_at DESC LIMIT 1` — the identical "silently
/// pick the most recent match" shortcut Finding H2 already fixed for
/// `provider_ref`, just not caught here at the time. `refund.*`/
/// `payment.dispute.*` webhooks correlate solely via `gateway_payment_id`
/// (`domain::Payment::gateway_payment_id`'s doc comment) — silently
/// mutating the wrong row's payment/license status on a collision would be
/// exactly as serious as the original H2 finding, so this gets the same
/// fail-closed treatment rather than being left as a lower-priority
/// inconsistency.
fn single_payment_or_duplicate_gateway_error(
    rows: Vec<PaymentRow>,
    gateway_payment_id: &str,
) -> Result<Option<Payment>, RepositoryError> {
    match rows.len() {
        0 => Ok(None),
        1 => rows.into_iter().next().map(Payment::try_from).transpose(),
        _ => Err(RepositoryError::DuplicateGatewayPaymentId(
            gateway_payment_id.to_string(),
        )),
    }
}

#[async_trait]
impl PaymentRepository for PgPaymentRepository {
    async fn insert(&self, new_payment: NewPayment) -> Result<Payment, RepositoryError> {
        let row = sqlx::query_as::<_, PaymentRow>(
            "INSERT INTO payments (subscription_id, amount_minor, currency, provider, provider_ref, status) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             RETURNING id, subscription_id, amount_minor, currency, provider, provider_ref, gateway_payment_id, status, created_at",
        )
        .bind(new_payment.subscription_id)
        .bind(new_payment.amount_minor)
        .bind(&new_payment.currency)
        .bind(&new_payment.provider)
        .bind(&new_payment.provider_ref)
        .bind(new_payment.status.as_str())
        .fetch_one(&self.pool)
        .await?;

        Payment::try_from(row)
    }

    /// Production Hardening, Finding H2: used to `ORDER BY created_at DESC
    /// LIMIT 1`, silently picking the most recently created match on a
    /// `provider_ref` collision — the wrong payment could be mutated by a
    /// webhook if two rows ever shared one. Now fetches every matching row
    /// and lets [`single_payment_or_duplicate_error`] decide what that
    /// means; see migration `0008` for the partial `UNIQUE` index that
    /// makes a genuine collision unreachable going forward.
    async fn find_by_provider_ref(
        &self,
        provider_ref: &str,
    ) -> Result<Option<Payment>, RepositoryError> {
        let rows = sqlx::query_as::<_, PaymentRow>(
            "SELECT id, subscription_id, amount_minor, currency, provider, provider_ref, gateway_payment_id, status, created_at \
             FROM payments WHERE provider_ref = $1",
        )
        .bind(provider_ref)
        .fetch_all(&self.pool)
        .await?;

        single_payment_or_duplicate_error(rows, provider_ref)
    }

    /// End-to-end payment testing pass (Phase 4N): fetches every matching
    /// row and lets [`single_payment_or_duplicate_gateway_error`] decide
    /// what that means — previously `ORDER BY created_at DESC LIMIT 1`,
    /// the same silent-pick shortcut Finding H2 fixed for
    /// `find_by_provider_ref`. See migration `0009` for the partial
    /// `UNIQUE` index that makes a genuine collision unreachable going
    /// forward.
    async fn find_by_gateway_payment_id(
        &self,
        gateway_payment_id: &str,
    ) -> Result<Option<Payment>, RepositoryError> {
        let rows = sqlx::query_as::<_, PaymentRow>(
            "SELECT id, subscription_id, amount_minor, currency, provider, provider_ref, gateway_payment_id, status, created_at \
             FROM payments WHERE gateway_payment_id = $1",
        )
        .bind(gateway_payment_id)
        .fetch_all(&self.pool)
        .await?;

        single_payment_or_duplicate_gateway_error(rows, gateway_payment_id)
    }

    async fn update_status(&self, id: i64, status: PaymentStatus) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE payments SET status = $2 WHERE id = $1")
            .bind(id)
            .bind(status.as_str())
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    async fn record_gateway_payment_id(
        &self,
        id: i64,
        gateway_payment_id: &str,
    ) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE payments SET gateway_payment_id = $2 WHERE id = $1")
            .bind(id)
            .bind(gateway_payment_id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(id: i64, provider_ref: &str) -> PaymentRow {
        PaymentRow {
            id,
            subscription_id: 1,
            amount_minor: 499_900,
            currency: "INR".to_string(),
            provider: "razorpay".to_string(),
            provider_ref: Some(provider_ref.to_string()),
            gateway_payment_id: None,
            status: "pending".to_string(),
            created_at: Utc::now(),
        }
    }

    // ── Production Hardening, Finding H2 ──────────────────────────────

    #[test]
    fn single_payment_or_duplicate_error_returns_none_for_zero_rows() {
        let result = single_payment_or_duplicate_error(vec![], "order_abc");
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn single_payment_or_duplicate_error_returns_the_one_match() {
        let result =
            single_payment_or_duplicate_error(vec![sample_row(1, "order_abc")], "order_abc");
        let payment = result.unwrap().unwrap();
        assert_eq!(payment.id, 1);
        assert_eq!(payment.provider_ref.as_deref(), Some("order_abc"));
    }

    #[test]
    fn single_payment_or_duplicate_error_rejects_two_rows_sharing_a_provider_ref() {
        let rows = vec![sample_row(1, "order_abc"), sample_row(2, "order_abc")];
        let err = single_payment_or_duplicate_error(rows, "order_abc").unwrap_err();
        match err {
            RepositoryError::DuplicateProviderReference(provider_ref) => {
                assert_eq!(provider_ref, "order_abc");
            }
            other => panic!("expected DuplicateProviderReference, got {other:?}"),
        }
    }

    #[test]
    fn single_payment_or_duplicate_error_rejects_more_than_two_rows_too() {
        let rows = vec![
            sample_row(1, "order_abc"),
            sample_row(2, "order_abc"),
            sample_row(3, "order_abc"),
        ];
        let err = single_payment_or_duplicate_error(rows, "order_abc").unwrap_err();
        assert!(matches!(
            err,
            RepositoryError::DuplicateProviderReference(_)
        ));
    }

    // ── End-to-end payment testing pass, Phase 4N: the same H2 fix for
    //    gateway_payment_id ────────────────────────────────────────────

    fn sample_gateway_row(id: i64, gateway_payment_id: &str) -> PaymentRow {
        PaymentRow {
            gateway_payment_id: Some(gateway_payment_id.to_string()),
            ..sample_row(id, "sub_checkout_ref")
        }
    }

    #[test]
    fn single_payment_or_duplicate_gateway_error_returns_none_for_zero_rows() {
        let result = single_payment_or_duplicate_gateway_error(vec![], "pay_abc");
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn single_payment_or_duplicate_gateway_error_returns_the_one_match() {
        let result = single_payment_or_duplicate_gateway_error(
            vec![sample_gateway_row(1, "pay_abc")],
            "pay_abc",
        );
        let payment = result.unwrap().unwrap();
        assert_eq!(payment.id, 1);
        assert_eq!(payment.gateway_payment_id.as_deref(), Some("pay_abc"));
    }

    #[test]
    fn single_payment_or_duplicate_gateway_error_rejects_two_rows_sharing_a_gateway_payment_id() {
        let rows = vec![
            sample_gateway_row(1, "pay_abc"),
            sample_gateway_row(2, "pay_abc"),
        ];
        let err = single_payment_or_duplicate_gateway_error(rows, "pay_abc").unwrap_err();
        match err {
            RepositoryError::DuplicateGatewayPaymentId(gateway_payment_id) => {
                assert_eq!(gateway_payment_id, "pay_abc");
            }
            other => panic!("expected DuplicateGatewayPaymentId, got {other:?}"),
        }
    }

    #[test]
    fn single_payment_or_duplicate_gateway_error_rejects_more_than_two_rows_too() {
        let rows = vec![
            sample_gateway_row(1, "pay_abc"),
            sample_gateway_row(2, "pay_abc"),
            sample_gateway_row(3, "pay_abc"),
        ];
        let err = single_payment_or_duplicate_gateway_error(rows, "pay_abc").unwrap_err();
        assert!(matches!(err, RepositoryError::DuplicateGatewayPaymentId(_)));
    }
}
