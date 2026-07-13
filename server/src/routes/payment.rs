//! `POST /create-checkout-session` (protected — Bearer session token),
//! `POST /webhooks/razorpay` (public — HMAC-signed instead;
//! `PHASE4_DESIGN.md` §3/§4).
//!
//! Handlers are thin: verify/parse, call one `PaymentService` method, map
//! the `Result` onto a response — all real logic lives in
//! `service::payment_service`, not here, same pattern
//! `routes::license`/`routes::auth` already established.

use crate::auth::token::hash_token;
use crate::auth::webhook_signature::verify_webhook_signature;
use crate::observability::{WEBHOOK_EVENTS_TOTAL, WEBHOOK_REQUESTS_TOTAL};
use crate::razorpay::RazorpayWebhookPayload;
use crate::routes::auth::{require_session, AuthenticatedSession};
use crate::routes::error::ApiError;
use crate::state::AppState;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::middleware;
use axum::routing::post;
use axum::{Json, Router};
use license_protocol::{CreateCheckoutSessionRequest, CreateCheckoutSessionResponse};
use serde::Serialize;

/// Takes `state` directly, same reason and same pattern as
/// `routes::auth::router` — `/create-checkout-session` needs
/// `require_session` wired up with a concrete `AppState`, which
/// `axum::middleware::from_fn` alone can't provide.
pub fn router(state: AppState) -> Router<AppState> {
    let protected = Router::new()
        .route("/create-checkout-session", post(create_checkout_session))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ))
        .with_state(state);

    let public = Router::new().route("/webhooks/razorpay", post(webhook));

    Router::new().merge(protected).merge(public)
}

async fn create_checkout_session(
    State(state): State<AppState>,
    axum::Extension(AuthenticatedSession(session)): axum::Extension<AuthenticatedSession>,
    Json(req): Json<CreateCheckoutSessionRequest>,
) -> Result<Json<CreateCheckoutSessionResponse>, ApiError> {
    let outcome = state
        .payment_service
        .create_checkout_session(session.user_id, &req.plan_type)
        .await?;

    Ok(Json(CreateCheckoutSessionResponse {
        checkout_url: outcome.checkout_url,
        provider_ref: outcome.provider_ref,
    }))
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct WebhookAck {
    status: &'static str,
}

/// Razorpay's own idempotency key, when it sends one
/// (`X-Razorpay-Event-Id`); falls back to a hash of the raw body so an
/// event can still be deduplicated even without that header — the same
/// payload always derives the same fallback id, so a genuine retry of the
/// *same* delivery still hits the idempotency check in
/// `PaymentService::process_webhook_event`.
fn resolve_event_id(headers: &HeaderMap, raw_body: &[u8]) -> String {
    headers
        .get("x-razorpay-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| hash_token(&String::from_utf8_lossy(raw_body)))
}

async fn webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<WebhookAck>, ApiError> {
    let Some(signature) = headers
        .get("x-razorpay-signature")
        .and_then(|v| v.to_str().ok())
    else {
        metrics::counter!(WEBHOOK_REQUESTS_TOTAL, "outcome" => "missing_signature").increment(1);
        return Err(ApiError::Unauthorized);
    };

    let Some(secret) = state.config.payment.razorpay_webhook_secret.as_ref() else {
        // A missing secret is a real configuration problem — logged loudly
        // server-side — but the HTTP response to Razorpay stays a plain
        // rejection, same as any other signature failure: whether the
        // secret is wrong or absent isn't Razorpay's concern, and telling
        // them apart in the response would be a minor information leak.
        tracing::error!("RAZORPAY_WEBHOOK_SECRET is not configured; rejecting all webhook calls");
        metrics::counter!(WEBHOOK_REQUESTS_TOTAL, "outcome" => "not_configured").increment(1);
        return Err(ApiError::Unauthorized);
    };

    // `.expose_secret()`: the one deliberate, explicit read of the actual
    // secret value — needed here to compute the HMAC, never logged.
    if !verify_webhook_signature(secret.expose_secret(), &body, signature) {
        tracing::warn!("razorpay webhook signature verification failed; rejecting");
        metrics::counter!(WEBHOOK_REQUESTS_TOTAL, "outcome" => "invalid_signature").increment(1);
        return Err(ApiError::Unauthorized);
    }

    let payload: RazorpayWebhookPayload = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(e) => {
            metrics::counter!(WEBHOOK_REQUESTS_TOTAL, "outcome" => "invalid_payload").increment(1);
            return Err(ApiError::InvalidRequest(e.to_string()));
        }
    };
    let event_id = resolve_event_id(&headers, &body);

    // Logged post-signature-verification only — event id/type are Razorpay
    // metadata, not secrets, but logging them before the HMAC check would
    // mean logging attacker-controlled input from an unauthenticated call.
    tracing::info!(event_id = %event_id, event_type = %payload.event, "received razorpay webhook");

    let event_type = payload.event.clone();
    if let Err(e) = state
        .payment_service
        .process_webhook_event(&event_id, payload)
        .await
    {
        metrics::counter!(WEBHOOK_REQUESTS_TOTAL, "outcome" => "processing_error").increment(1);
        return Err(e.into());
    }

    tracing::info!(event_id = %event_id, "razorpay webhook processed");
    metrics::counter!(WEBHOOK_REQUESTS_TOTAL, "outcome" => "processed").increment(1);
    metrics::counter!(WEBHOOK_EVENTS_TOTAL, "event_type" => event_type).increment(1);

    Ok(Json(WebhookAck { status: "ok" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_ack_serializes_to_the_documented_shape() {
        let json = serde_json::to_value(WebhookAck { status: "ok" }).unwrap();
        assert_eq!(json, serde_json::json!({ "status": "ok" }));
    }

    #[test]
    fn resolve_event_id_prefers_the_header_when_present() {
        let mut headers = HeaderMap::new();
        headers.insert("x-razorpay-event-id", "evt_123".parse().unwrap());
        assert_eq!(resolve_event_id(&headers, b"irrelevant body"), "evt_123");
    }

    #[test]
    fn resolve_event_id_falls_back_to_a_body_hash_when_the_header_is_absent() {
        let headers = HeaderMap::new();
        let a = resolve_event_id(&headers, b"same body");
        let b = resolve_event_id(&headers, b"same body");
        let c = resolve_event_id(&headers, b"different body");
        assert_eq!(a, b, "same body must derive the same fallback id");
        assert_ne!(a, c);
    }
}
