//! `razorpay/` — the "External" layer (`PHASE4_DESIGN.md` §1.2): the one
//! place this crate talks to Razorpay's HTTP API. `service::payment_service`
//! depends on the `RazorpayClient` *trait* here, never `HttpRazorpayClient`
//! directly, so webhook/checkout business logic is testable against a mock
//! without a real network call or real credentials — same pattern every
//! repository trait already established.

pub mod client;
pub mod webhook;

pub use client::{
    CreateCheckoutRequest, CreateCheckoutResponse, HttpRazorpayClient, RazorpayClient,
    RazorpayError, RazorpayPayment,
};
pub use webhook::{
    extract_dispute_status, extract_entity_amount_minor, extract_entity_currency,
    extract_entity_id, extract_entity_ref, RazorpayWebhookPayload,
};

/// Crate-internal only (not part of this module's public API above) —
/// `config::AppConfig::from_vars` reuses this to decide whether a live key
/// requires the two Plan-id variables to also be set. See its doc comment
/// in `client.rs` for why it's shared rather than duplicated.
pub(crate) use client::is_test_mode_key;
