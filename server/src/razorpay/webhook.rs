//! Razorpay webhook payload shape — the inbound counterpart to
//! `client.rs`'s outbound request/response types.
//!
//! Deserializes only the two fields this server actually branches on
//! (`event`, `payload`); the nested `payload.<entity>.entity.*` structure
//! is read on demand via `serde_json::Value` indexing in
//! `service::payment_service`, not fully typed here — Razorpay's exact
//! nested shape per event type is public API documentation this crate
//! hasn't been verified against a live payload (see `client.rs`'s module
//! doc comment for the same caveat on the outbound side).

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
pub struct RazorpayWebhookPayload {
    pub event: String,
    #[serde(default)]
    pub payload: Value,
}

/// Reads `payload.<entity_key>.entity.order_id`, falling back to
/// `payload.<entity_key>.entity.id` — the id a webhook event correlates
/// back to a stored `payments.provider_ref` with. `order_id` is preferred
/// when present: for a one-time Payment Link, `entity.id` is the
/// *payment's own* id (freshly generated per attempt), while
/// `entity.order_id` matches the stable reference this server stored at
/// checkout time (`repository::payment`'s doc comment).
pub fn extract_entity_ref(payload: &Value, entity_key: &str) -> Option<String> {
    let entity = payload.get(entity_key)?.get("entity")?;
    entity
        .get("order_id")
        .and_then(Value::as_str)
        .or_else(|| entity.get("id").and_then(Value::as_str))
        .map(str::to_string)
}

/// Reads `payload.<entity_key>.entity.id` directly — unlike
/// `extract_entity_ref`, no `order_id` preference. Used where the caller
/// specifically needs the entity's own id: `payload.payment.entity.id` is
/// the real Razorpay payment id recorded as `payments.gateway_payment_id`
/// at activation time (Phase 4K.2), and is the *only* thing a later
/// `refund.*`/`payment.dispute.*` webhook ever carries to correlate back
/// to that row — those webhooks never carry `provider_ref`'s checkout-time
/// payment-link/subscription id.
pub fn extract_entity_id(payload: &Value, entity_key: &str) -> Option<String> {
    payload
        .get(entity_key)?
        .get("entity")?
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Reads `payload.<entity_key>.entity.amount` (Razorpay's actual captured
/// amount, integer minor units — paise) — used to verify a webhook's real
/// captured amount against `payments.amount_minor`, the value stored at
/// checkout time, before granting entitlement (Production Hardening,
/// Finding C2). `None` for a missing/non-integer field is treated the same
/// as `extract_entity_id`'s own absence case — nothing to verify against,
/// not itself a mismatch; only an actual present-and-different value
/// blocks activation (`service::payment_service::resolve_activation`).
pub fn extract_entity_amount_minor(payload: &Value, entity_key: &str) -> Option<i64> {
    payload
        .get(entity_key)?
        .get("entity")?
        .get("amount")
        .and_then(Value::as_i64)
}

/// Reads `payload.<entity_key>.entity.currency` — paired with
/// `extract_entity_amount_minor` for the same Finding C2 verification.
pub fn extract_entity_currency(payload: &Value, entity_key: &str) -> Option<String> {
    payload
        .get(entity_key)?
        .get("entity")?
        .get("currency")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Reads `payload.dispute.entity.status` — Razorpay's own dispute status
/// string (`"open"`, `"under_review"`, `"won"`, `"lost"`, ...). Only
/// `"won"`/`"lost"` are terminal outcomes `payment.dispute.closed`
/// handling recognizes; any other value (including a missing/malformed
/// field) is treated as unrecognized, never guessed at.
pub fn extract_dispute_status(payload: &Value) -> Option<String> {
    payload
        .get("dispute")?
        .get("entity")?
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_entity_ref_prefers_order_id_when_present() {
        let payload = json!({
            "payment": { "entity": { "id": "pay_123", "order_id": "order_456" } }
        });
        assert_eq!(
            extract_entity_ref(&payload, "payment"),
            Some("order_456".to_string())
        );
    }

    #[test]
    fn extract_entity_ref_falls_back_to_id_when_order_id_absent() {
        let payload = json!({
            "subscription": { "entity": { "id": "sub_123" } }
        });
        assert_eq!(
            extract_entity_ref(&payload, "subscription"),
            Some("sub_123".to_string())
        );
    }

    #[test]
    fn extract_entity_ref_returns_none_for_a_missing_entity_key() {
        let payload = json!({ "payment": { "entity": { "id": "pay_123" } } });
        assert_eq!(extract_entity_ref(&payload, "subscription"), None);
    }

    #[test]
    fn webhook_payload_deserializes_the_event_field() {
        let json = json!({ "event": "payment.captured", "payload": {} });
        let parsed: RazorpayWebhookPayload = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.event, "payment.captured");
    }

    #[test]
    fn extract_entity_id_reads_the_entitys_own_id_ignoring_order_id() {
        let payload = json!({
            "payment": { "entity": { "id": "pay_123", "order_id": "order_456" } }
        });
        assert_eq!(
            extract_entity_id(&payload, "payment"),
            Some("pay_123".to_string())
        );
    }

    #[test]
    fn extract_entity_id_returns_none_for_a_missing_entity_key() {
        let payload = json!({ "payment": { "entity": { "id": "pay_123" } } });
        assert_eq!(extract_entity_id(&payload, "refund"), None);
    }

    #[test]
    fn extract_entity_amount_minor_reads_the_entitys_amount() {
        let payload = json!({ "payment": { "entity": { "amount": 499900 } } });
        assert_eq!(
            extract_entity_amount_minor(&payload, "payment"),
            Some(499900)
        );
    }

    #[test]
    fn extract_entity_amount_minor_returns_none_for_a_missing_entity_key() {
        let payload = json!({ "payment": { "entity": { "amount": 499900 } } });
        assert_eq!(extract_entity_amount_minor(&payload, "refund"), None);
    }

    #[test]
    fn extract_entity_amount_minor_returns_none_for_a_non_integer_amount() {
        let payload = json!({ "payment": { "entity": { "amount": "not-a-number" } } });
        assert_eq!(extract_entity_amount_minor(&payload, "payment"), None);
    }

    #[test]
    fn extract_entity_currency_reads_the_entitys_currency() {
        let payload = json!({ "payment": { "entity": { "currency": "INR" } } });
        assert_eq!(
            extract_entity_currency(&payload, "payment"),
            Some("INR".to_string())
        );
    }

    #[test]
    fn extract_entity_currency_returns_none_for_a_missing_entity_key() {
        let payload = json!({ "payment": { "entity": { "currency": "INR" } } });
        assert_eq!(extract_entity_currency(&payload, "refund"), None);
    }

    #[test]
    fn extract_dispute_status_reads_the_dispute_entitys_status() {
        let payload = json!({ "dispute": { "entity": { "status": "won" } } });
        assert_eq!(extract_dispute_status(&payload), Some("won".to_string()));
    }

    #[test]
    fn extract_dispute_status_returns_none_when_absent() {
        assert_eq!(extract_dispute_status(&json!({})), None);
    }
}
