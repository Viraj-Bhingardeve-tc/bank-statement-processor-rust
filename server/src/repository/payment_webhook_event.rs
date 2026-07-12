//! `payment_webhook_events` table access — the idempotency ledger
//! `PHASE4_DESIGN.md` §4 step 2 checks before any webhook-triggered write.

use crate::domain::{NewPaymentWebhookEvent, PaymentWebhookEvent};
use crate::repository::error::RepositoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

#[async_trait]
pub trait PaymentWebhookEventRepository: Send + Sync {
    async fn find_by_provider_and_event_id(
        &self,
        provider: &str,
        event_id: &str,
    ) -> Result<Option<PaymentWebhookEvent>, RepositoryError>;
    async fn insert(
        &self,
        new_event: NewPaymentWebhookEvent,
    ) -> Result<PaymentWebhookEvent, RepositoryError>;
}

pub struct PgPaymentWebhookEventRepository {
    pool: PgPool,
}

impl PgPaymentWebhookEventRepository {
    pub fn new(pool: PgPool) -> Self {
        PgPaymentWebhookEventRepository { pool }
    }
}

#[derive(sqlx::FromRow)]
struct PaymentWebhookEventRow {
    id: i64,
    provider: String,
    event_id: String,
    event_type: String,
    payload: Value,
    processed_at: DateTime<Utc>,
}

impl From<PaymentWebhookEventRow> for PaymentWebhookEvent {
    fn from(row: PaymentWebhookEventRow) -> Self {
        PaymentWebhookEvent {
            id: row.id,
            provider: row.provider,
            event_id: row.event_id,
            event_type: row.event_type,
            payload: row.payload,
            processed_at: row.processed_at,
        }
    }
}

#[async_trait]
impl PaymentWebhookEventRepository for PgPaymentWebhookEventRepository {
    async fn find_by_provider_and_event_id(
        &self,
        provider: &str,
        event_id: &str,
    ) -> Result<Option<PaymentWebhookEvent>, RepositoryError> {
        let row = sqlx::query_as::<_, PaymentWebhookEventRow>(
            "SELECT id, provider, event_id, event_type, payload, processed_at \
             FROM payment_webhook_events WHERE provider = $1 AND event_id = $2",
        )
        .bind(provider)
        .bind(event_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(PaymentWebhookEvent::from))
    }

    async fn insert(
        &self,
        new_event: NewPaymentWebhookEvent,
    ) -> Result<PaymentWebhookEvent, RepositoryError> {
        let row = sqlx::query_as::<_, PaymentWebhookEventRow>(
            "INSERT INTO payment_webhook_events (provider, event_id, event_type, payload) \
             VALUES ($1, $2, $3, $4) \
             RETURNING id, provider, event_id, event_type, payload, processed_at",
        )
        .bind(&new_event.provider)
        .bind(&new_event.event_id)
        .bind(&new_event.event_type)
        .bind(&new_event.payload)
        .fetch_one(&self.pool)
        .await?;

        Ok(PaymentWebhookEvent::from(row))
    }
}
