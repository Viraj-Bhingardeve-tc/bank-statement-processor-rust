//! Razorpay API client — order/payment-link/subscription creation
//! (`PHASE4_DESIGN.md` §1.3/§2).
//!
//! **A correction to `PHASE4_DESIGN.md` §2's exact wording, made here and
//! flagged rather than silently resolved either way:** that section says
//! "Lifetime plan → Razorpay Orders API" but also that the checkout
//! surface is "Razorpay's own hosted checkout page, opened in the user's
//! system default browser." Razorpay's plain Orders API does not itself
//! produce a hosted, browser-openable URL — that flow expects the
//! *client* to embed Razorpay's Checkout.js widget, which a native Slint
//! desktop app with no browser engine cannot do (the same constraint the
//! design document itself cites for choosing a hosted-page approach at
//! all). The Razorpay product that actually produces a hosted URL for a
//! one-time payment is **Payment Links** (`POST /v1/payment_links`); this
//! client uses that for `lifetime` and the real Subscriptions API
//! (`POST /v1/subscriptions`, whose response includes a genuine
//! `short_url`) for `monthly`/`yearly`. The `RazorpayClient` trait itself
//! stays product-agnostic (`create_checkout` → `{checkout_url,
//! provider_ref}`) precisely so this internal detail can be corrected
//! without touching `service::payment_service` or its tests. **Exact
//! Razorpay request/response field names here are written against
//! Razorpay's public API documentation, not verified against a live
//! account — confirm during the manual staging pass `PHASE4_DESIGN.md` §9
//! already calls out as the one thing automated tests can't substitute
//! for.**

use crate::domain::PlanType;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RazorpayError {
    /// No API key/plan-id configured for this operation — a configuration
    /// state, not a network failure (mirrors `license_protocol::ApiError`'s
    /// `NoServerConfigured` sentinel, same "honest, not a fake success"
    /// principle carried over from Phase 3).
    NotConfigured(String),
    /// The HTTP call itself failed, or Razorpay returned a non-2xx.
    Http(String),
}

impl fmt::Display for RazorpayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RazorpayError::NotConfigured(msg) => write!(f, "razorpay not configured: {msg}"),
            RazorpayError::Http(msg) => write!(f, "razorpay request failed: {msg}"),
        }
    }
}

impl std::error::Error for RazorpayError {}

#[derive(Debug, Clone)]
pub struct CreateCheckoutRequest {
    pub plan_type: PlanType,
    /// Smallest currency unit (paise) — only used for the `lifetime`
    /// (Payment Links) path; Subscriptions pricing lives on the
    /// Razorpay-side Plan itself.
    pub amount_minor: i64,
    pub currency: String,
    /// Our own reference string (e.g. `"sub_42"`), passed through so a
    /// Razorpay dashboard operator can trace a payment back to our
    /// `subscriptions` row without needing database access.
    pub receipt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateCheckoutResponse {
    pub checkout_url: String,
    pub provider_ref: String,
}

/// A payment as Razorpay's `GET /v1/payments` list reports it — the
/// reconciliation job's (`PHASE4_DESIGN.md` §12) only source of truth
/// about what actually happened at Razorpay, independent of whether a
/// webhook for it ever arrived. Deliberately thin: `status` is Razorpay's
/// own string (`"captured"`, `"failed"`, `"authorized"`, ...), interpreted
/// by `service::payment_service` the same way a webhook payload's status
/// would be — not re-typed as our own `PaymentStatus` here, since this
/// type describes what Razorpay said, not our own domain model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RazorpayPayment {
    pub id: String,
    /// Populated for Payment-Link/Order-originated payments; absent for
    /// most Subscription-originated recurring charges. Correlated against
    /// stored `payments.provider_ref` the same way inbound webhooks are
    /// (`razorpay::extract_entity_ref`'s order_id-then-id preference).
    pub order_id: Option<String>,
    pub status: String,
}

#[async_trait::async_trait]
pub trait RazorpayClient: Send + Sync {
    async fn create_checkout(
        &self,
        req: CreateCheckoutRequest,
    ) -> Result<CreateCheckoutResponse, RazorpayError>;

    /// Lists payments created at or after `since` — the reconciliation
    /// job's pull-based backstop for webhooks that never arrived
    /// (`PHASE4_DESIGN.md` §12.2 step 1). **Single page only (Razorpay's
    /// default/max `count=100`), no pagination** — a real production
    /// deployment handling more than 100 payments inside one 2-hour
    /// lookback window would silently miss the overflow; flagged here
    /// rather than silently assumed complete, since `PHASE4_DESIGN.md`
    /// §12 doesn't specify pagination and this phase doesn't add it.
    async fn list_payments_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Vec<RazorpayPayment>, RazorpayError>;
}

/// The real implementation — HTTP calls against `api.razorpay.com`.
pub struct HttpRazorpayClient {
    http: reqwest::Client,
    key_id: Option<String>,
    key_secret: Option<String>,
    monthly_plan_id: Option<String>,
    yearly_plan_id: Option<String>,
}

impl HttpRazorpayClient {
    pub fn new(
        key_id: Option<String>,
        key_secret: Option<String>,
        monthly_plan_id: Option<String>,
        yearly_plan_id: Option<String>,
    ) -> Self {
        HttpRazorpayClient {
            http: reqwest::Client::new(),
            key_id,
            key_secret,
            monthly_plan_id,
            yearly_plan_id,
        }
    }

    fn credentials(&self) -> Result<(&str, &str), RazorpayError> {
        let key_id = self.key_id.as_deref().ok_or_else(|| {
            RazorpayError::NotConfigured("RAZORPAY_KEY_ID is not set".to_string())
        })?;
        let key_secret = self.key_secret.as_deref().ok_or_else(|| {
            RazorpayError::NotConfigured("RAZORPAY_KEY_SECRET is not set".to_string())
        })?;
        Ok((key_id, key_secret))
    }

    fn plan_id_for(&self, plan_type: PlanType) -> Result<&str, RazorpayError> {
        match plan_type {
            PlanType::Monthly => self.monthly_plan_id.as_deref(),
            PlanType::Yearly => self.yearly_plan_id.as_deref(),
            PlanType::Trial | PlanType::Lifetime => None,
        }
        .ok_or_else(|| {
            RazorpayError::NotConfigured(format!("no Razorpay plan id configured for {plan_type}"))
        })
    }
}

#[derive(Debug, Serialize)]
struct CreatePaymentLinkRequest<'a> {
    amount: i64,
    currency: &'a str,
    reference_id: &'a str,
}

#[derive(Debug, Deserialize)]
struct CreatePaymentLinkResponse {
    id: String,
    short_url: String,
}

#[derive(Debug, Serialize)]
struct CreateSubscriptionRequest<'a> {
    plan_id: &'a str,
    #[serde(rename = "total_count")]
    total_count: u32,
    #[serde(rename = "customer_notify")]
    customer_notify: u8,
    notes: SubscriptionNotes<'a>,
}

#[derive(Debug, Serialize)]
struct SubscriptionNotes<'a> {
    receipt: &'a str,
}

#[derive(Debug, Deserialize)]
struct CreateSubscriptionResponse {
    id: String,
    short_url: String,
}

#[async_trait::async_trait]
impl RazorpayClient for HttpRazorpayClient {
    async fn create_checkout(
        &self,
        req: CreateCheckoutRequest,
    ) -> Result<CreateCheckoutResponse, RazorpayError> {
        let (key_id, key_secret) = self.credentials()?;

        match req.plan_type {
            PlanType::Lifetime | PlanType::Trial => {
                let body = CreatePaymentLinkRequest {
                    amount: req.amount_minor,
                    currency: &req.currency,
                    reference_id: &req.receipt,
                };
                let response = self
                    .http
                    .post("https://api.razorpay.com/v1/payment_links")
                    .basic_auth(key_id, Some(key_secret))
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| RazorpayError::Http(e.to_string()))?;

                if !response.status().is_success() {
                    return Err(RazorpayError::Http(format!(
                        "payment_links returned {}",
                        response.status()
                    )));
                }

                let parsed: CreatePaymentLinkResponse = response
                    .json()
                    .await
                    .map_err(|e| RazorpayError::Http(e.to_string()))?;

                Ok(CreateCheckoutResponse {
                    checkout_url: parsed.short_url,
                    provider_ref: parsed.id,
                })
            }
            PlanType::Monthly | PlanType::Yearly => {
                let plan_id = self.plan_id_for(req.plan_type)?;
                let body = CreateSubscriptionRequest {
                    plan_id,
                    total_count: 120, // 10 years of billing cycles — Razorpay requires a bound, not literally unlimited.
                    customer_notify: 1,
                    notes: SubscriptionNotes {
                        receipt: &req.receipt,
                    },
                };
                let response = self
                    .http
                    .post("https://api.razorpay.com/v1/subscriptions")
                    .basic_auth(key_id, Some(key_secret))
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| RazorpayError::Http(e.to_string()))?;

                if !response.status().is_success() {
                    return Err(RazorpayError::Http(format!(
                        "subscriptions returned {}",
                        response.status()
                    )));
                }

                let parsed: CreateSubscriptionResponse = response
                    .json()
                    .await
                    .map_err(|e| RazorpayError::Http(e.to_string()))?;

                Ok(CreateCheckoutResponse {
                    checkout_url: parsed.short_url,
                    provider_ref: parsed.id,
                })
            }
        }
    }

    async fn list_payments_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Vec<RazorpayPayment>, RazorpayError> {
        let (key_id, key_secret) = self.credentials()?;

        let response = self
            .http
            .get("https://api.razorpay.com/v1/payments")
            .basic_auth(key_id, Some(key_secret))
            .query(&[
                ("from", since.timestamp().to_string()),
                ("count", "100".to_string()),
            ])
            .send()
            .await
            .map_err(|e| RazorpayError::Http(e.to_string()))?;

        if !response.status().is_success() {
            return Err(RazorpayError::Http(format!(
                "payments list returned {}",
                response.status()
            )));
        }

        let parsed: ListPaymentsResponse = response
            .json()
            .await
            .map_err(|e| RazorpayError::Http(e.to_string()))?;

        Ok(parsed
            .items
            .into_iter()
            .map(|item| RazorpayPayment {
                id: item.id,
                order_id: item.order_id,
                status: item.status,
            })
            .collect())
    }
}

#[derive(Debug, Deserialize)]
struct ListPaymentsResponse {
    items: Vec<PaymentListItem>,
}

#[derive(Debug, Deserialize)]
struct PaymentListItem {
    id: String,
    order_id: Option<String>,
    status: String,
}
