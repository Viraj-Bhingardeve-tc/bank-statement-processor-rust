//! `PaymentWebhookEvent` — idempotency ledger for inbound provider
//! webhooks (`payment_webhook_events` table, `PHASE4_DESIGN.md` §7).
//! `(provider, event_id)` is checked before any other write a webhook
//! triggers; if already present, nothing further happens
//! (`PHASE4_DESIGN.md` §4 step 2).

use chrono::{DateTime, Utc};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct PaymentWebhookEvent {
    pub id: i64,
    pub provider: String,
    pub event_id: String,
    pub event_type: String,
    pub payload: Value,
    pub processed_at: DateTime<Utc>,
}

/// Fields needed to record a new webhook event — no `id`/`processed_at`,
/// since those are database-generated.
#[derive(Debug, Clone, PartialEq)]
pub struct NewPaymentWebhookEvent {
    pub provider: String,
    pub event_id: String,
    pub event_type: String,
    pub payload: Value,
}
