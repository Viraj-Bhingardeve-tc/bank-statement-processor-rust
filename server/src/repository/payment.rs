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
    /// `domain::Payment`.
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

    async fn find_by_provider_ref(
        &self,
        provider_ref: &str,
    ) -> Result<Option<Payment>, RepositoryError> {
        let row = sqlx::query_as::<_, PaymentRow>(
            "SELECT id, subscription_id, amount_minor, currency, provider, provider_ref, gateway_payment_id, status, created_at \
             FROM payments WHERE provider_ref = $1 \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(provider_ref)
        .fetch_optional(&self.pool)
        .await?;

        row.map(Payment::try_from).transpose()
    }

    async fn find_by_gateway_payment_id(
        &self,
        gateway_payment_id: &str,
    ) -> Result<Option<Payment>, RepositoryError> {
        let row = sqlx::query_as::<_, PaymentRow>(
            "SELECT id, subscription_id, amount_minor, currency, provider, provider_ref, gateway_payment_id, status, created_at \
             FROM payments WHERE gateway_payment_id = $1 \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(gateway_payment_id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(Payment::try_from).transpose()
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
