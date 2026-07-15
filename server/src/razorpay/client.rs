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

use crate::config::Secret;
use crate::domain::PlanType;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// Conservative, production-safe bounds so a single hung Razorpay request
/// can never block a caller (`POST /create-checkout-session`) or the
/// reconciliation scheduler forever (production readiness audit HIGH
/// finding #5). `connect_timeout` bounds the TCP+TLS handshake;
/// `timeout` bounds the whole request including the response body.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RazorpayError {
    /// No API key/plan-id configured for this operation — a configuration
    /// state, not a network failure (mirrors `license_protocol::ApiError`'s
    /// `NoServerConfigured` sentinel, same "honest, not a fake success"
    /// principle carried over from Phase 3).
    NotConfigured(String),
    /// The HTTP call itself failed, or Razorpay returned a non-2xx. Kept
    /// as-is for `create_checkout` (unchanged — checkout is out of scope
    /// for Phase 4K.4); `list_payments_since` now constructs the two more
    /// specific variants below instead, so the reconciliation job can
    /// classify what happened without re-parsing this string.
    Http(String),
    /// (Phase 4K.4) `list_payments_since` failed in a way a later retry
    /// can reasonably be expected to fix on its own — a connect/timeout
    /// error, or Razorpay returning HTTP 5xx. Reconciliation already
    /// retries the whole run on the next scheduled tick; this variant just
    /// makes that "safe to retry" judgment explicit in logs/metrics
    /// instead of implicit in a generic error string.
    Transient(String),
    /// (Phase 4K.4) `list_payments_since` failed in a way that retrying
    /// the identical request will not fix — a well-formed non-5xx error
    /// response, or a response body that doesn't parse as the documented
    /// shape. Still surfaced as a failed run (there's no partial list to
    /// fall back to), but classified distinctly so an operator doesn't
    /// mistake a real integration problem for ordinary network flakiness.
    Permanent(String),
}

impl fmt::Display for RazorpayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RazorpayError::NotConfigured(msg) => write!(f, "razorpay not configured: {msg}"),
            RazorpayError::Http(msg) => write!(f, "razorpay request failed: {msg}"),
            RazorpayError::Transient(msg) => {
                write!(f, "razorpay request failed (transient, will retry): {msg}")
            }
            RazorpayError::Permanent(msg) => {
                write!(f, "razorpay request failed (permanent): {msg}")
            }
        }
    }
}

impl RazorpayError {
    /// Whether a caller should expect an identical retry to have a real
    /// chance of succeeding (Phase 4K.4) — `NotConfigured`/`Http` (the
    /// pre-existing, `create_checkout`-only variants) default to `true`,
    /// preserving today's "always retry next tick" reconciliation
    /// behavior for any call site that hasn't been updated to construct
    /// the more specific variants.
    pub fn is_recoverable(&self) -> bool {
        !matches!(self, RazorpayError::Permanent(_))
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
///
/// Deliberately does not derive `Debug` — even though `key_id`/`key_secret`
/// are already [`Secret`]-wrapped (redacted regardless), there is no
/// present need to print this struct, and not deriving `Debug` here is one
/// more layer keeping a future accidental `{:?}` of the whole client from
/// ever becoming a question of "did the wrapper actually redact it," since
/// the derive doesn't exist to answer wrong.
pub struct HttpRazorpayClient {
    http: reqwest::Client,
    key_id: Option<Secret<String>>,
    key_secret: Option<Secret<String>>,
    monthly_plan_id: Option<String>,
    yearly_plan_id: Option<String>,
    /// `RECONCILIATION_BATCH_SIZE` (Phase 4K.4, `config::ReconciliationConfig`)
    /// — the `count` query param `list_payments_since` sends Razorpay.
    /// Previously hardcoded to `100`; that's still the default when unset.
    reconciliation_batch_size: u32,
}

impl HttpRazorpayClient {
    pub fn new(
        key_id: Option<Secret<String>>,
        key_secret: Option<Secret<String>>,
        monthly_plan_id: Option<String>,
        yearly_plan_id: Option<String>,
        reconciliation_batch_size: u32,
    ) -> Self {
        HttpRazorpayClient {
            http: build_http_client(CONNECT_TIMEOUT, REQUEST_TIMEOUT),
            key_id,
            key_secret,
            monthly_plan_id,
            yearly_plan_id,
            reconciliation_batch_size,
        }
    }

    fn credentials(&self) -> Result<(&str, &str), RazorpayError> {
        let key_id = self
            .key_id
            .as_ref()
            .map(|s| s.expose_secret().as_str())
            .ok_or_else(|| {
                RazorpayError::NotConfigured("RAZORPAY_KEY_ID is not set".to_string())
            })?;
        let key_secret = self
            .key_secret
            .as_ref()
            .map(|s| s.expose_secret().as_str())
            .ok_or_else(|| {
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

/// Builds the underlying HTTP client with bounded connect/request
/// timeouts — factored out of `HttpRazorpayClient::new` so a test can
/// build one with tiny durations against a local, deliberately-
/// unresponsive server and prove the bound is actually enforced, without
/// touching the real production values.
///
/// `.expect(...)`: building a `reqwest::Client` only fails on a
/// catastrophic TLS-backend initialization problem, never on the timeout
/// values themselves. This runs once at server startup (`AppState::new`),
/// so failing loudly here — rather than silently falling back to an
/// unbounded client, which would defeat the entire point of this function
/// — matches this crate's existing "fail fast at boot" convention
/// (`main.rs`'s signal-handler installs, `observability::handle`'s
/// Prometheus recorder install).
fn build_http_client(connect_timeout: Duration, request_timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .timeout(request_timeout)
        .build()
        .expect("failed to build the Razorpay HTTP client")
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
            .query(&list_payments_query_params(
                since,
                self.reconciliation_batch_size,
            ))
            .send()
            .await
            .map_err(|e| classify_send_error(&e))?;

        if !response.status().is_success() {
            return Err(classify_status_error(response.status()));
        }

        let parsed: ListPaymentsResponse = response
            .json()
            .await
            .map_err(|e| RazorpayError::Permanent(e.to_string()))?;

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

/// The `from`/`count` query params `list_payments_since` sends Razorpay —
/// factored out of the method body (Phase 4K.4) so the now-configurable
/// `count` (previously always `"100"`) is testable without a network call.
fn list_payments_query_params(
    since: DateTime<Utc>,
    batch_size: u32,
) -> [(&'static str, String); 2] {
    [
        ("from", since.timestamp().to_string()),
        ("count", batch_size.to_string()),
    ]
}

/// Classifies a `reqwest::Error` from `list_payments_since`'s `.send()`
/// call (Phase 4K.4) — a connect/timeout failure is worth retrying next
/// scheduled tick (`Transient`); anything else (a malformed request, a
/// redirect failure, ...) will fail identically on retry (`Permanent`).
fn classify_send_error(e: &reqwest::Error) -> RazorpayError {
    if e.is_timeout() || e.is_connect() {
        RazorpayError::Transient(e.to_string())
    } else {
        RazorpayError::Permanent(e.to_string())
    }
}

/// Classifies a non-2xx `list_payments_since` response status (Phase
/// 4K.4) — a 5xx is Razorpay's own transient failure (worth retrying);
/// anything else (4xx, ...) is a well-formed rejection an identical retry
/// will not fix.
fn classify_status_error(status: reqwest::StatusCode) -> RazorpayError {
    let msg = format!("payments list returned {status}");
    if status.is_server_error() {
        RazorpayError::Transient(msg)
    } else {
        RazorpayError::Permanent(msg)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::net::TcpListener;

    #[test]
    fn timeouts_match_the_documented_conservative_defaults() {
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(5));
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(10));
    }

    /// The actual behavior Phase 4J.4 fixes: a request to a server that
    /// accepts the TCP connection but never sends a response must still
    /// fail within roughly the configured timeout, not hang indefinitely
    /// (`create_checkout`/`list_payments_since` both call through
    /// `self.http`, built the same way as `client` here).
    #[tokio::test]
    async fn a_request_past_the_configured_timeout_fails_instead_of_hanging_forever() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Accepts the connection and then never responds — simulates a
        // Razorpay request that hangs forever.
        tokio::spawn(async move {
            if let Ok((_socket, _)) = listener.accept().await {
                std::future::pending::<()>().await;
            }
        });

        let client = build_http_client(Duration::from_millis(200), Duration::from_millis(200));

        let started = Instant::now();
        let result = client.get(format!("http://{addr}/")).send().await;
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "a request to an unresponsive server must fail, not hang until it succeeds"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the request must fail within roughly the configured timeout, not block \
             indefinitely (took {elapsed:?})"
        );
    }

    // ── Reconciliation batch size (Phase 4K.4) ──────────────────────────

    #[test]
    fn list_payments_query_params_uses_the_configured_batch_size() {
        let params = list_payments_query_params(Utc::now(), 25);
        assert_eq!(params[1], ("count", "25".to_string()));
    }

    #[test]
    fn list_payments_query_params_defaults_match_the_previously_hardcoded_count() {
        let params = list_payments_query_params(Utc::now(), 100);
        assert_eq!(params[1], ("count", "100".to_string()));
    }

    #[test]
    fn list_payments_query_params_sends_since_as_a_unix_timestamp() {
        let since = Utc::now();
        let params = list_payments_query_params(since, 100);
        assert_eq!(params[0], ("from", since.timestamp().to_string()));
    }

    #[test]
    fn http_razorpay_client_stores_the_configured_reconciliation_batch_size() {
        let client = HttpRazorpayClient::new(None, None, None, None, 42);
        assert_eq!(client.reconciliation_batch_size, 42);
    }

    // ── Transient/Permanent error classification (Phase 4K.4) ───────────

    #[test]
    fn classify_status_error_treats_5xx_as_transient() {
        assert!(matches!(
            classify_status_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
            RazorpayError::Transient(_)
        ));
        assert!(matches!(
            classify_status_error(reqwest::StatusCode::SERVICE_UNAVAILABLE),
            RazorpayError::Transient(_)
        ));
    }

    #[test]
    fn classify_status_error_treats_non_5xx_as_permanent() {
        assert!(matches!(
            classify_status_error(reqwest::StatusCode::BAD_REQUEST),
            RazorpayError::Permanent(_)
        ));
        assert!(matches!(
            classify_status_error(reqwest::StatusCode::UNAUTHORIZED),
            RazorpayError::Permanent(_)
        ));
        assert!(matches!(
            classify_status_error(reqwest::StatusCode::NOT_FOUND),
            RazorpayError::Permanent(_)
        ));
    }

    /// Same connect/timeout distinction the existing timeout test above
    /// proves at the transport layer — here proving `classify_send_error`
    /// maps that kind of `reqwest::Error` to `Transient`.
    #[tokio::test]
    async fn classify_send_error_treats_a_connect_timeout_as_transient() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((_socket, _)) = listener.accept().await {
                std::future::pending::<()>().await;
            }
        });

        let client = build_http_client(Duration::from_millis(100), Duration::from_millis(100));
        let err = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap_err();

        assert!(matches!(
            classify_send_error(&err),
            RazorpayError::Transient(_)
        ));
    }

    #[tokio::test]
    async fn classify_send_error_treats_a_malformed_request_as_permanent() {
        let client = build_http_client(CONNECT_TIMEOUT, REQUEST_TIMEOUT);
        let err = client.get("not a valid url").send().await.unwrap_err();

        assert!(matches!(
            classify_send_error(&err),
            RazorpayError::Permanent(_)
        ));
    }

    #[test]
    fn is_recoverable_is_true_for_every_variant_except_permanent() {
        assert!(RazorpayError::NotConfigured("x".to_string()).is_recoverable());
        assert!(RazorpayError::Http("x".to_string()).is_recoverable());
        assert!(RazorpayError::Transient("x".to_string()).is_recoverable());
        assert!(!RazorpayError::Permanent("x".to_string()).is_recoverable());
    }
}
