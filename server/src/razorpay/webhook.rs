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
}
