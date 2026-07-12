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
    /// Correlates an inbound webhook event back to the `payments` row it
    /// concerns — see this module's doc comment for exactly what
    /// `provider_ref` holds for each plan type.
    async fn find_by_provider_ref(
        &self,
        provider_ref: &str,
    ) -> Result<Option<Payment>, RepositoryError>;
    async fn update_status(&self, id: i64, status: PaymentStatus) -> Result<(), RepositoryError>;
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
             RETURNING id, subscription_id, amount_minor, currency, provider, provider_ref, status, created_at",
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
            "SELECT id, subscription_id, amount_minor, currency, provider, provider_ref, status, created_at \
             FROM payments WHERE provider_ref = $1 \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(provider_ref)
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
}
