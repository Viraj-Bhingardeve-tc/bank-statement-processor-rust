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
    RazorpayError,
};
pub use webhook::{extract_entity_ref, RazorpayWebhookPayload};
