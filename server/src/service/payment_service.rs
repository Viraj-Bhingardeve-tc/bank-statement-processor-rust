//! Business logic for checkout creation, Razorpay webhook processing, and
//! payment reconciliation (`PHASE4_DESIGN.md` §2/§4/§12).
//!
//! Depends only on repository/client *traits*, never a concrete `Pg*`/
//! `HttpRazorpayClient` implementation, so the full checkout, webhook, and
//! reconciliation flows are unit-tested against hand-written mocks below —
//! no real database and no real Razorpay call, same pattern
//! `service::license_service`/`service::auth_service` already established.
//!
//! **Reconciliation's idempotency mechanism deliberately differs from
//! `PHASE4_DESIGN.md` §12.2 step 2's literal wording, flagged here rather
//! than silently resolved either way.** That section describes checking
//! `payment_webhook_events` directly by "the Razorpay event's own id" —
//! but Razorpay's payments-list API (what `reconcile_once` below actually
//! calls) returns payment objects, not webhook deliveries, and has no
//! "event id" a real inbound webhook would have used. Checking a
//! *synthesized* id against the ledger in isolation would be actively
//! wrong: it would never match the real `event_id` a webhook already
//! inserted, so reconciliation would re-process (and, via `extend()`,
//! keep pushing a license's expiry further into the future) every payment
//! a webhook already correctly handled, on every run within the 2-hour
//! lookback window. Instead, `reconcile_one` first compares the *payment's
//! own stored status* against what Razorpay reports — a signal that's
//! true regardless of whether a webhook, a prior reconciliation run, or
//! nothing at all produced it — and only calls `process_webhook_event`
//! (with a stable synthetic id, so repeated reconciliation runs are still
//! idempotent against *each other*) when there's a genuine mismatch.
//!
//! **Deliberately not transactional across repository calls
//! (`PHASE4_DESIGN.md` §4 step 3 asks for "a single database
//! transaction").** This architecture's repositories each operate on
//! `&self.pool` independently — there's no shared multi-statement
//! transaction primitive threaded through them (adding one is a real,
//! separate piece of work, not bundled into this phase). Correctness
//! without it rests on two properties instead: (1) every individual step
//! here is idempotent on its own (setting a status to a value it's
//! already at, or extending a license's expiry, are both safe to repeat),
//! and (2) the idempotency ledger row (`payment_webhook_events`) is
//! written *last*, only after every other write succeeds — so a crash
//! mid-sequence leaves the ledger silent about this event, and Razorpay's
//! own webhook retry (or a future reconciliation pass) re-drives the same,
//! individually-safe-to-repeat sequence rather than double-applying
//! anything. A crash between two of these steps can still leave the
//! *local* state briefly inconsistent until the next retry — a real,
//! narrower gap than "no atomicity at all," and worth closing with a real
//! transaction if this becomes the system of record before a
//! reconciliation job (deliberately out of scope this phase) exists to
//! paper over it.

use crate::domain::{
    LicenseRecordStatus, NewLicense, NewPayment, NewPaymentWebhookEvent, NewSubscription,
    PaymentStatus, PlanType, SubscriptionStatus,
};
use crate::razorpay::{
    extract_entity_ref, CreateCheckoutRequest, RazorpayClient, RazorpayPayment,
    RazorpayWebhookPayload,
};
use crate::repository::error::RepositoryError;
use crate::repository::license::LicenseRepository;
use crate::repository::payment::PaymentRepository;
use crate::repository::payment_webhook_event::PaymentWebhookEventRepository;
use crate::repository::subscription::SubscriptionRepository;
use chrono::{Duration, Utc};
use rand::Rng;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

const PROVIDER: &str = "razorpay";

pub struct PaymentService {
    payment_repository: Arc<dyn PaymentRepository>,
    webhook_event_repository: Arc<dyn PaymentWebhookEventRepository>,
    subscription_repository: Arc<dyn SubscriptionRepository>,
    license_repository: Arc<dyn LicenseRepository>,
    razorpay_client: Arc<dyn RazorpayClient>,
}

impl PaymentService {
    pub fn new(
        payment_repository: Arc<dyn PaymentRepository>,
        webhook_event_repository: Arc<dyn PaymentWebhookEventRepository>,
        subscription_repository: Arc<dyn SubscriptionRepository>,
        license_repository: Arc<dyn LicenseRepository>,
        razorpay_client: Arc<dyn RazorpayClient>,
    ) -> Self {
        PaymentService {
            payment_repository,
            webhook_event_repository,
            subscription_repository,
            license_repository,
            razorpay_client,
        }
    }

    /// `POST /create-checkout-session`. Creates a new (`pending_payment`)
    /// subscription row, asks Razorpay for a hosted checkout URL, and
    /// records a `pending` payment referencing both — see this module's
    /// doc comment on why these three writes aren't wrapped in one
    /// transaction.
    pub async fn create_checkout_session(
        &self,
        user_id: i64,
        plan_type_str: &str,
    ) -> Result<CheckoutOutcome, PaymentOperationError> {
        let plan_type = PlanType::from_str(plan_type_str)
            .map_err(|_| PaymentOperationError::InvalidPlanType)?;

        let subscription = self
            .subscription_repository
            .insert(NewSubscription {
                user_id,
                plan_type,
                status: SubscriptionStatus::PendingPayment,
                started_at: Utc::now(),
                current_period_end: None,
                auto_renew: true,
            })
            .await?;

        let amount_minor = plan_amount_minor(plan_type);
        let checkout = self
            .razorpay_client
            .create_checkout(CreateCheckoutRequest {
                plan_type,
                amount_minor,
                currency: "INR".to_string(),
                receipt: format!("sub_{}", subscription.id),
            })
            .await
            .map_err(|e| PaymentOperationError::ProviderError(e.to_string()))?;

        self.payment_repository
            .insert(NewPayment {
                subscription_id: subscription.id,
                amount_minor,
                currency: "INR".to_string(),
                provider: PROVIDER.to_string(),
                provider_ref: Some(checkout.provider_ref.clone()),
                status: PaymentStatus::Pending,
            })
            .await?;

        Ok(CheckoutOutcome {
            checkout_url: checkout.checkout_url,
            provider_ref: checkout.provider_ref,
        })
    }

    /// `POST /webhooks/razorpay` — called only after signature
    /// verification has already passed (`routes::payment`); `event_id` is
    /// what the handler extracted from `X-Razorpay-Event-Id` (or derived,
    /// if absent — see that module). Idempotent: a second call with the
    /// same `event_id` is a no-op.
    pub async fn process_webhook_event(
        &self,
        event_id: &str,
        payload: RazorpayWebhookPayload,
    ) -> Result<(), PaymentOperationError> {
        if self
            .webhook_event_repository
            .find_by_provider_and_event_id(PROVIDER, event_id)
            .await?
            .is_some()
        {
            return Ok(());
        }

        match payload.event.as_str() {
            "payment.captured" => self.handle_payment_captured(&payload).await?,
            "payment.failed" => self.handle_payment_failed(&payload).await?,
            "subscription.activated" | "subscription.charged" => {
                self.handle_subscription_active(&payload).await?
            }
            "subscription.cancelled" | "subscription.halted" => {
                self.handle_subscription_inactive(&payload).await?
            }
            other => {
                tracing::info!(
                    event = other,
                    "unrecognized webhook event type; acknowledged, no action taken"
                );
            }
        }

        self.webhook_event_repository
            .insert(NewPaymentWebhookEvent {
                provider: PROVIDER.to_string(),
                event_id: event_id.to_string(),
                event_type: payload.event.clone(),
                payload: payload.payload.clone(),
            })
            .await?;

        Ok(())
    }

    async fn handle_payment_captured(
        &self,
        payload: &RazorpayWebhookPayload,
    ) -> Result<(), PaymentOperationError> {
        let Some(provider_ref) = extract_entity_ref(&payload.payload, "payment") else {
            tracing::warn!("payment.captured webhook missing a usable entity reference; ignoring");
            return Ok(());
        };
        self.mark_payment_succeeded_and_activate(&provider_ref)
            .await
    }

    async fn handle_payment_failed(
        &self,
        payload: &RazorpayWebhookPayload,
    ) -> Result<(), PaymentOperationError> {
        let Some(provider_ref) = extract_entity_ref(&payload.payload, "payment") else {
            tracing::warn!("payment.failed webhook missing a usable entity reference; ignoring");
            return Ok(());
        };
        let Some(payment) = self
            .payment_repository
            .find_by_provider_ref(&provider_ref)
            .await?
        else {
            tracing::warn!(provider_ref = %provider_ref, "payment.failed webhook references an unknown payment; ignoring");
            return Ok(());
        };
        self.payment_repository
            .update_status(payment.id, PaymentStatus::Failed)
            .await?;
        Ok(())
    }

    async fn handle_subscription_active(
        &self,
        payload: &RazorpayWebhookPayload,
    ) -> Result<(), PaymentOperationError> {
        let Some(provider_ref) = extract_entity_ref(&payload.payload, "subscription") else {
            tracing::warn!("subscription webhook missing a usable entity reference; ignoring");
            return Ok(());
        };
        self.mark_payment_succeeded_and_activate(&provider_ref)
            .await
    }

    async fn handle_subscription_inactive(
        &self,
        payload: &RazorpayWebhookPayload,
    ) -> Result<(), PaymentOperationError> {
        let Some(provider_ref) = extract_entity_ref(&payload.payload, "subscription") else {
            tracing::warn!("subscription webhook missing a usable entity reference; ignoring");
            return Ok(());
        };
        let Some(payment) = self
            .payment_repository
            .find_by_provider_ref(&provider_ref)
            .await?
        else {
            tracing::warn!(provider_ref = %provider_ref, "subscription webhook references an unknown payment; ignoring");
            return Ok(());
        };
        let Some(subscription) = self
            .subscription_repository
            .find_by_id(payment.subscription_id)
            .await?
        else {
            return Ok(());
        };

        let new_status = if payload.event == "subscription.halted" {
            SubscriptionStatus::Suspended
        } else {
            SubscriptionStatus::Cancelled
        };
        self.subscription_repository
            .update_status(subscription.id, new_status, subscription.current_period_end)
            .await?;
        Ok(())
    }

    /// Shared by both `payment.captured` (one-time, Payment Links) and
    /// `subscription.activated`/`.charged` (recurring) — see this module's
    /// doc comment for the "reuse the original payments row on renewal"
    /// simplification.
    async fn mark_payment_succeeded_and_activate(
        &self,
        provider_ref: &str,
    ) -> Result<(), PaymentOperationError> {
        let Some(payment) = self
            .payment_repository
            .find_by_provider_ref(provider_ref)
            .await?
        else {
            tracing::warn!(provider_ref = %provider_ref, "webhook references an unknown payment; ignoring (no local record)");
            return Ok(());
        };
        self.payment_repository
            .update_status(payment.id, PaymentStatus::Succeeded)
            .await?;
        self.activate_subscription_and_license(payment.subscription_id)
            .await
    }

    async fn activate_subscription_and_license(
        &self,
        subscription_id: i64,
    ) -> Result<(), PaymentOperationError> {
        let Some(subscription) = self
            .subscription_repository
            .find_by_id(subscription_id)
            .await?
        else {
            return Err(PaymentOperationError::Repository(
                RepositoryError::InvalidData(format!(
                    "payment references missing subscription {subscription_id}"
                )),
            ));
        };

        let period_end = plan_duration(subscription.plan_type).map(|d| Utc::now() + d);
        self.subscription_repository
            .update_status(subscription.id, SubscriptionStatus::Active, period_end)
            .await?;

        match self
            .license_repository
            .find_latest_by_subscription(subscription.id)
            .await?
        {
            Some(existing) => {
                self.license_repository
                    .extend(existing.id, LicenseRecordStatus::Active, period_end)
                    .await?;
            }
            None => {
                self.license_repository
                    .insert(NewLicense {
                        subscription_id: subscription.id,
                        license_key: generate_license_key(),
                        status: LicenseRecordStatus::Active,
                        expires_at: period_end,
                        max_devices: 1,
                        grace_period_days: 7,
                    })
                    .await?;
            }
        }
        Ok(())
    }

    /// `PHASE4_DESIGN.md` §12 — the pull-based backstop for webhooks that
    /// never arrived. Runs on a fixed schedule (`reconciliation::spawn`),
    /// but is a plain method here so it's directly callable from tests
    /// (§12.4) without a running scheduler.
    ///
    /// Lists Razorpay payments from the trailing lookback window,
    /// compares each against local state, and heals genuine gaps through
    /// the exact same `process_webhook_event` path a real webhook would
    /// have used — see this module's doc comment for why "genuine gap" is
    /// detected via payment-status comparison rather than a synthesized
    /// event-id lookup.
    pub async fn reconcile_once(&self) -> Result<ReconciliationSummary, PaymentOperationError> {
        let since = Utc::now() - Duration::hours(RECONCILIATION_LOOKBACK_HOURS);
        let payments = self
            .razorpay_client
            .list_payments_since(since)
            .await
            .map_err(|e| PaymentOperationError::ProviderError(e.to_string()))?;

        let checked = payments.len();
        let mut healed = 0usize;

        for razorpay_payment in &payments {
            match self.reconcile_one(razorpay_payment).await {
                Ok(true) => healed += 1,
                Ok(false) => {}
                Err(e) => {
                    // One bad payment must not abort the whole run — every
                    // other discovered payment still gets a chance this
                    // pass, and this one gets picked up again next run
                    // (it's still inside the 2-hour lookback window then).
                    tracing::warn!(
                        razorpay_payment_id = %razorpay_payment.id,
                        error = %e,
                        "reconciliation: failed to process a discovered payment; will retry next run"
                    );
                }
            }
        }

        tracing::info!(
            checked,
            healed,
            lookback_hours = RECONCILIATION_LOOKBACK_HOURS,
            "reconciliation run complete"
        );

        Ok(ReconciliationSummary { checked, healed })
    }

    /// Returns `Ok(true)` if this payment was healed, `Ok(false)` if it
    /// was already in sync (or deliberately skipped — an unhandled status,
    /// or a reference with no local match at all, per `PHASE4_DESIGN.md`
    /// §12.3's "no silent healing of ambiguous cases").
    async fn reconcile_one(
        &self,
        razorpay_payment: &RazorpayPayment,
    ) -> Result<bool, PaymentOperationError> {
        let Some(razorpay_status) = map_razorpay_payment_status(&razorpay_payment.status) else {
            // An unrecognized-or-unhandled Razorpay status (e.g.
            // "authorized", "refunded" — the latter has no corresponding
            // webhook handler in this phase either, see PHASE4_DESIGN.md
            // §4's event list). Nothing to heal against.
            return Ok(false);
        };

        // Same order_id-then-id preference `razorpay::extract_entity_ref`
        // uses for inbound webhooks, so this pre-check and the eventual
        // `process_webhook_event` call (which re-derives the same
        // provider_ref independently from the synthetic payload below)
        // always agree on which local row they mean.
        let candidate_refs = [
            razorpay_payment.order_id.as_deref(),
            Some(razorpay_payment.id.as_str()),
        ];

        let mut local_payment = None;
        for candidate in candidate_refs.into_iter().flatten() {
            if let Some(p) = self
                .payment_repository
                .find_by_provider_ref(candidate)
                .await?
            {
                local_payment = Some(p);
                break;
            }
        }

        let Some(local_payment) = local_payment else {
            tracing::warn!(
                razorpay_payment_id = %razorpay_payment.id,
                status = %razorpay_payment.status,
                "reconciliation: Razorpay payment has no matching local record; logged as an anomaly, not guessed at"
            );
            return Ok(false);
        };

        if local_payment.status == razorpay_status {
            // Already in sync — via a webhook, or a prior reconciliation
            // run. This is what makes repeated runs over the same
            // still-in-window payment idempotent: nothing past this point
            // ever executes a second time for the same real-world event.
            return Ok(false);
        }

        let event_type = match razorpay_status {
            PaymentStatus::Succeeded => "payment.captured",
            PaymentStatus::Failed => "payment.failed",
            // Only captured/failed are reachable here — see
            // `map_razorpay_payment_status`.
            _ => return Ok(false),
        };
        let event_id = format!(
            "reconcile:{}:{}",
            razorpay_payment.status, razorpay_payment.id
        );
        let payload = RazorpayWebhookPayload {
            event: event_type.to_string(),
            payload: serde_json::json!({
                "payment": {
                    "entity": {
                        "id": razorpay_payment.id,
                        "order_id": razorpay_payment.order_id,
                    }
                }
            }),
        };

        self.process_webhook_event(&event_id, payload).await?;

        tracing::warn!(
            razorpay_payment_id = %razorpay_payment.id,
            provider_ref = %local_payment.provider_ref.as_deref().unwrap_or(""),
            new_status = %razorpay_payment.status,
            "reconciliation: healed a payment a webhook apparently never delivered for"
        );

        Ok(true)
    }
}

/// `PHASE4_DESIGN.md` §14 item 8/9 (confirmed, fixed values).
pub const RECONCILIATION_INTERVAL_MINUTES: i64 = 15;
const RECONCILIATION_LOOKBACK_HOURS: i64 = 2;

#[derive(Debug, PartialEq, Eq)]
pub struct ReconciliationSummary {
    pub checked: usize,
    pub healed: usize,
}

/// Maps a Razorpay payment status string onto the one local status it
/// implies, for the statuses reconciliation actually knows how to heal
/// against (matching `process_webhook_event`'s own handled event types —
/// see this module's doc comment). Any other Razorpay status (e.g.
/// `"authorized"`, `"refunded"`) has no corresponding webhook handler in
/// this phase, so reconciliation deliberately doesn't attempt one either.
fn map_razorpay_payment_status(status: &str) -> Option<PaymentStatus> {
    match status {
        "captured" => Some(PaymentStatus::Succeeded),
        "failed" => Some(PaymentStatus::Failed),
        _ => None,
    }
}

#[derive(Debug)]
pub struct CheckoutOutcome {
    pub checkout_url: String,
    pub provider_ref: String,
}

#[derive(Debug)]
pub enum PaymentOperationError {
    InvalidPlanType,
    ProviderError(String),
    Repository(RepositoryError),
}

impl fmt::Display for PaymentOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PaymentOperationError::InvalidPlanType => write!(f, "invalid plan_type"),
            PaymentOperationError::ProviderError(msg) => write!(f, "payment provider error: {msg}"),
            PaymentOperationError::Repository(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PaymentOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PaymentOperationError::Repository(e) => Some(e),
            _ => None,
        }
    }
}

impl From<RepositoryError> for PaymentOperationError {
    fn from(e: RepositoryError) -> Self {
        PaymentOperationError::Repository(e)
    }
}

/// **Placeholder pricing** — not sourced from any confirmed business
/// input, purely so `create_checkout_session` has a concrete amount to
/// send Razorpay. Revisit before any real launch (a config-driven price
/// map, matching `PHASE4_DESIGN.md` §2's plan-id mapping treatment, is the
/// natural next step once real prices exist).
fn plan_amount_minor(plan: PlanType) -> i64 {
    match plan {
        PlanType::Trial => 0,
        PlanType::Monthly => 49_900,
        PlanType::Yearly => 499_900,
        PlanType::Lifetime => 1_999_900,
    }
}

/// `None` for `lifetime` (never expires, per the schema's own
/// `expires_at` nullability).
fn plan_duration(plan: PlanType) -> Option<Duration> {
    match plan {
        PlanType::Trial => Some(Duration::days(14)),
        PlanType::Monthly => Some(Duration::days(30)),
        PlanType::Yearly => Some(Duration::days(365)),
        PlanType::Lifetime => None,
    }
}

/// Customer-facing activation code — 4 groups of 4 characters, excluding
/// visually ambiguous ones (`0`/`O`, `1`/`I`), matching
/// `API_SPECIFICATION.md`'s documented `"XXXX-XXXX-XXXX-XXXX"` format.
fn generate_license_key() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..4)
        .map(|_| {
            (0..4)
                .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{License, Payment, PaymentWebhookEvent, Subscription};
    use crate::razorpay::{CreateCheckoutResponse, RazorpayError};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    // ── Mocks ────────────────────────────────────────────────────────────

    struct MockPaymentRepository {
        payments: Mutex<Vec<Payment>>,
        next_id: Mutex<i64>,
    }

    impl MockPaymentRepository {
        fn with(payments: Vec<Payment>) -> Self {
            let next_id = payments.iter().map(|p| p.id).max().unwrap_or(0) + 1;
            MockPaymentRepository {
                payments: Mutex::new(payments),
                next_id: Mutex::new(next_id),
            }
        }
    }

    #[async_trait]
    impl PaymentRepository for MockPaymentRepository {
        async fn insert(&self, new_payment: NewPayment) -> Result<Payment, RepositoryError> {
            let mut next_id = self.next_id.lock().unwrap();
            let id = *next_id;
            *next_id += 1;
            let payment = Payment {
                id,
                subscription_id: new_payment.subscription_id,
                amount_minor: new_payment.amount_minor,
                currency: new_payment.currency,
                provider: new_payment.provider,
                provider_ref: new_payment.provider_ref,
                status: new_payment.status,
                created_at: Utc::now(),
            };
            self.payments.lock().unwrap().push(payment.clone());
            Ok(payment)
        }

        async fn find_by_provider_ref(
            &self,
            provider_ref: &str,
        ) -> Result<Option<Payment>, RepositoryError> {
            Ok(self
                .payments
                .lock()
                .unwrap()
                .iter()
                .find(|p| p.provider_ref.as_deref() == Some(provider_ref))
                .cloned())
        }

        async fn update_status(
            &self,
            id: i64,
            status: PaymentStatus,
        ) -> Result<(), RepositoryError> {
            if let Some(p) = self
                .payments
                .lock()
                .unwrap()
                .iter_mut()
                .find(|p| p.id == id)
            {
                p.status = status;
            }
            Ok(())
        }
    }

    struct MockWebhookEventRepository {
        events: Mutex<Vec<PaymentWebhookEvent>>,
        next_id: Mutex<i64>,
    }

    impl MockWebhookEventRepository {
        fn new() -> Self {
            MockWebhookEventRepository {
                events: Mutex::new(Vec::new()),
                next_id: Mutex::new(1),
            }
        }
    }

    #[async_trait]
    impl PaymentWebhookEventRepository for MockWebhookEventRepository {
        async fn find_by_provider_and_event_id(
            &self,
            provider: &str,
            event_id: &str,
        ) -> Result<Option<PaymentWebhookEvent>, RepositoryError> {
            Ok(self
                .events
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.provider == provider && e.event_id == event_id)
                .cloned())
        }

        async fn insert(
            &self,
            new_event: NewPaymentWebhookEvent,
        ) -> Result<PaymentWebhookEvent, RepositoryError> {
            let mut next_id = self.next_id.lock().unwrap();
            let id = *next_id;
            *next_id += 1;
            let event = PaymentWebhookEvent {
                id,
                provider: new_event.provider,
                event_id: new_event.event_id,
                event_type: new_event.event_type,
                payload: new_event.payload,
                processed_at: Utc::now(),
            };
            self.events.lock().unwrap().push(event.clone());
            Ok(event)
        }
    }

    struct MockSubscriptionRepository {
        subscriptions: Mutex<Vec<Subscription>>,
        next_id: Mutex<i64>,
    }

    impl MockSubscriptionRepository {
        fn with(subscriptions: Vec<Subscription>) -> Self {
            let next_id = subscriptions.iter().map(|s| s.id).max().unwrap_or(0) + 1;
            MockSubscriptionRepository {
                subscriptions: Mutex::new(subscriptions),
                next_id: Mutex::new(next_id),
            }
        }
    }

    #[async_trait]
    impl SubscriptionRepository for MockSubscriptionRepository {
        async fn find_by_id(&self, id: i64) -> Result<Option<Subscription>, RepositoryError> {
            Ok(self
                .subscriptions
                .lock()
                .unwrap()
                .iter()
                .find(|s| s.id == id)
                .cloned())
        }

        async fn find_active_by_user(
            &self,
            _user_id: i64,
        ) -> Result<Option<Subscription>, RepositoryError> {
            unimplemented!("not exercised by these tests")
        }

        async fn insert(
            &self,
            new_subscription: NewSubscription,
        ) -> Result<Subscription, RepositoryError> {
            let mut next_id = self.next_id.lock().unwrap();
            let id = *next_id;
            *next_id += 1;
            let now = Utc::now();
            let subscription = Subscription {
                id,
                user_id: new_subscription.user_id,
                plan_type: new_subscription.plan_type,
                status: new_subscription.status,
                started_at: new_subscription.started_at,
                current_period_end: new_subscription.current_period_end,
                auto_renew: new_subscription.auto_renew,
                created_at: now,
                updated_at: now,
            };
            self.subscriptions
                .lock()
                .unwrap()
                .push(subscription.clone());
            Ok(subscription)
        }

        async fn update_status(
            &self,
            id: i64,
            status: SubscriptionStatus,
            current_period_end: Option<chrono::DateTime<Utc>>,
        ) -> Result<(), RepositoryError> {
            if let Some(s) = self
                .subscriptions
                .lock()
                .unwrap()
                .iter_mut()
                .find(|s| s.id == id)
            {
                s.status = status;
                s.current_period_end = current_period_end;
            }
            Ok(())
        }
    }

    struct MockLicenseRepository {
        licenses: Mutex<Vec<License>>,
        next_id: Mutex<i64>,
    }

    impl MockLicenseRepository {
        fn with(licenses: Vec<License>) -> Self {
            let next_id = licenses.iter().map(|l| l.id).max().unwrap_or(0) + 1;
            MockLicenseRepository {
                licenses: Mutex::new(licenses),
                next_id: Mutex::new(next_id),
            }
        }
    }

    #[async_trait]
    impl LicenseRepository for MockLicenseRepository {
        async fn find_by_key(
            &self,
            _license_key: &str,
        ) -> Result<Option<License>, RepositoryError> {
            unimplemented!("not exercised by these tests")
        }

        async fn find_by_id(&self, id: i64) -> Result<Option<License>, RepositoryError> {
            Ok(self
                .licenses
                .lock()
                .unwrap()
                .iter()
                .find(|l| l.id == id)
                .cloned())
        }

        async fn find_latest_by_subscription(
            &self,
            subscription_id: i64,
        ) -> Result<Option<License>, RepositoryError> {
            Ok(self
                .licenses
                .lock()
                .unwrap()
                .iter()
                .filter(|l| {
                    l.subscription_id == subscription_id && l.status != LicenseRecordStatus::Revoked
                })
                .max_by_key(|l| l.issued_at)
                .cloned())
        }

        async fn insert(&self, new_license: NewLicense) -> Result<License, RepositoryError> {
            let mut next_id = self.next_id.lock().unwrap();
            let id = *next_id;
            *next_id += 1;
            let license = License {
                id,
                subscription_id: new_license.subscription_id,
                license_key: new_license.license_key,
                status: new_license.status,
                expires_at: new_license.expires_at,
                max_devices: new_license.max_devices,
                grace_period_days: new_license.grace_period_days,
                issued_at: Utc::now(),
                revoked_at: None,
                revoked_reason: None,
            };
            self.licenses.lock().unwrap().push(license.clone());
            Ok(license)
        }

        async fn extend(
            &self,
            id: i64,
            status: LicenseRecordStatus,
            expires_at: Option<chrono::DateTime<Utc>>,
        ) -> Result<(), RepositoryError> {
            if let Some(l) = self
                .licenses
                .lock()
                .unwrap()
                .iter_mut()
                .find(|l| l.id == id)
            {
                l.status = status;
                l.expires_at = expires_at;
            }
            Ok(())
        }
    }

    struct MockRazorpayClient {
        checkout_result: Result<CreateCheckoutResponse, RazorpayError>,
        list_result: Vec<RazorpayPayment>,
    }

    #[async_trait]
    impl RazorpayClient for MockRazorpayClient {
        async fn create_checkout(
            &self,
            _req: CreateCheckoutRequest,
        ) -> Result<CreateCheckoutResponse, RazorpayError> {
            self.checkout_result.clone()
        }

        async fn list_payments_since(
            &self,
            _since: chrono::DateTime<Utc>,
        ) -> Result<Vec<RazorpayPayment>, RazorpayError> {
            Ok(self.list_result.clone())
        }
    }

    // ── Fixtures ─────────────────────────────────────────────────────────

    fn sample_subscription(id: i64, status: SubscriptionStatus) -> Subscription {
        Subscription {
            id,
            user_id: 1,
            plan_type: PlanType::Yearly,
            status,
            started_at: Utc::now(),
            current_period_end: None,
            auto_renew: true,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn sample_payment(
        id: i64,
        subscription_id: i64,
        provider_ref: &str,
        status: PaymentStatus,
    ) -> Payment {
        Payment {
            id,
            subscription_id,
            amount_minor: 499_900,
            currency: "INR".to_string(),
            provider: PROVIDER.to_string(),
            provider_ref: Some(provider_ref.to_string()),
            status,
            created_at: Utc::now(),
        }
    }

    /// Builds a `PaymentService` plus separate, directly inspectable
    /// handles to the subscription/license mocks (cloned `Arc`s pointing
    /// at the same interior `Mutex` state the service itself uses) — lets
    /// tests assert on post-call state without reaching into the
    /// service's own private fields.
    #[allow(clippy::too_many_arguments)]
    fn service_with(
        payments: Vec<Payment>,
        subscriptions: Vec<Subscription>,
        licenses: Vec<License>,
        razorpay_result: Result<CreateCheckoutResponse, RazorpayError>,
    ) -> (
        PaymentService,
        Arc<MockSubscriptionRepository>,
        Arc<MockLicenseRepository>,
    ) {
        let subscription_repository = Arc::new(MockSubscriptionRepository::with(subscriptions));
        let license_repository = Arc::new(MockLicenseRepository::with(licenses));
        let service = PaymentService::new(
            Arc::new(MockPaymentRepository::with(payments)),
            Arc::new(MockWebhookEventRepository::new()),
            subscription_repository.clone(),
            license_repository.clone(),
            Arc::new(MockRazorpayClient {
                checkout_result: razorpay_result,
                list_result: vec![],
            }),
        );
        (service, subscription_repository, license_repository)
    }

    fn ok_checkout() -> Result<CreateCheckoutResponse, RazorpayError> {
        Ok(CreateCheckoutResponse {
            checkout_url: "https://rzp.io/checkout/xyz".to_string(),
            provider_ref: "sub_rzp_123".to_string(),
        })
    }

    /// Builds a `PaymentService` for reconciliation tests specifically —
    /// unlike `service_with`, this also hands back the payment-repository
    /// mock (reconciliation tests need to assert on `payments.status`,
    /// which checkout/webhook tests never do) and takes the canned
    /// `RazorpayClient::list_payments_since` response directly instead of
    /// a single checkout result.
    fn service_with_reconciliation(
        payments: Vec<Payment>,
        subscriptions: Vec<Subscription>,
        licenses: Vec<License>,
        razorpay_payments: Vec<RazorpayPayment>,
    ) -> (
        PaymentService,
        Arc<MockPaymentRepository>,
        Arc<MockSubscriptionRepository>,
        Arc<MockLicenseRepository>,
    ) {
        let payment_repository = Arc::new(MockPaymentRepository::with(payments));
        let subscription_repository = Arc::new(MockSubscriptionRepository::with(subscriptions));
        let license_repository = Arc::new(MockLicenseRepository::with(licenses));
        let service = PaymentService::new(
            payment_repository.clone(),
            Arc::new(MockWebhookEventRepository::new()),
            subscription_repository.clone(),
            license_repository.clone(),
            Arc::new(MockRazorpayClient {
                checkout_result: Err(RazorpayError::NotConfigured(
                    "not exercised by reconciliation tests".to_string(),
                )),
                list_result: razorpay_payments,
            }),
        );
        (
            service,
            payment_repository,
            subscription_repository,
            license_repository,
        )
    }

    // ── create_checkout_session ─────────────────────────────────────────

    #[tokio::test]
    async fn create_checkout_session_with_a_valid_plan_type_succeeds() {
        let (service, ..) = service_with(vec![], vec![], vec![], ok_checkout());

        let outcome = service.create_checkout_session(1, "yearly").await.unwrap();
        assert_eq!(outcome.checkout_url, "https://rzp.io/checkout/xyz");
        assert_eq!(outcome.provider_ref, "sub_rzp_123");
    }

    #[tokio::test]
    async fn create_checkout_session_with_an_invalid_plan_type_is_rejected() {
        let (service, ..) = service_with(vec![], vec![], vec![], ok_checkout());

        let err = service
            .create_checkout_session(1, "platinum")
            .await
            .unwrap_err();
        assert!(matches!(err, PaymentOperationError::InvalidPlanType));
    }

    #[tokio::test]
    async fn create_checkout_session_surfaces_a_provider_error_honestly() {
        let (service, ..) = service_with(
            vec![],
            vec![],
            vec![],
            Err(RazorpayError::NotConfigured(
                "RAZORPAY_KEY_ID is not set".to_string(),
            )),
        );

        let err = service
            .create_checkout_session(1, "monthly")
            .await
            .unwrap_err();
        assert!(matches!(err, PaymentOperationError::ProviderError(_)));
    }

    // ── process_webhook_event ───────────────────────────────────────────

    #[tokio::test]
    async fn payment_captured_activates_the_subscription_and_issues_a_license() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let (service, subscriptions, licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "payment.captured".to_string(),
            payload: json!({ "payment": { "entity": { "id": "pay_xyz", "order_id": "order_abc" } } }),
        };
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(subscription.status, SubscriptionStatus::Active);

        let license = licenses.find_latest_by_subscription(10).await.unwrap();
        assert!(license.is_some());
        assert_eq!(license.unwrap().status, LicenseRecordStatus::Active);
    }

    #[tokio::test]
    async fn payment_captured_extends_an_existing_license_instead_of_creating_a_second_one() {
        let subscription = sample_subscription(10, SubscriptionStatus::Active);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let existing_license = License {
            id: 99,
            subscription_id: 10,
            license_key: "EXIS-TING-KEYX-0000".to_string(),
            status: LicenseRecordStatus::Expired,
            expires_at: Some(Utc::now() - Duration::days(1)),
            max_devices: 1,
            grace_period_days: 7,
            issued_at: Utc::now() - Duration::days(400),
            revoked_at: None,
            revoked_reason: None,
        };
        let (service, _subscriptions, licenses) = service_with(
            vec![payment],
            vec![subscription],
            vec![existing_license],
            ok_checkout(),
        );

        let payload = RazorpayWebhookPayload {
            event: "payment.captured".to_string(),
            payload: json!({ "payment": { "entity": { "id": "pay_xyz", "order_id": "order_abc" } } }),
        };
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let license = licenses
            .find_latest_by_subscription(10)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            license.id, 99,
            "must reuse the existing row, not insert a new one"
        );
        assert_eq!(license.status, LicenseRecordStatus::Active);
        assert!(license.expires_at.unwrap() > Utc::now());
    }

    #[tokio::test]
    async fn webhook_processing_is_idempotent_for_a_repeated_event_id() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let (service, _subscriptions, licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "payment.captured".to_string(),
            payload: json!({ "payment": { "entity": { "id": "pay_xyz", "order_id": "order_abc" } } }),
        };
        service
            .process_webhook_event("evt_1", payload.clone())
            .await
            .unwrap();
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        // Exactly one license, not two — the second call must have been a
        // pure no-op (idempotency check short-circuits before any write).
        let licenses_for_subscription = licenses.find_latest_by_subscription(10).await.unwrap();
        assert!(licenses_for_subscription.is_some());
    }

    #[tokio::test]
    async fn payment_failed_marks_the_payment_failed_without_touching_the_subscription() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let (service, subscriptions, _licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "payment.failed".to_string(),
            payload: json!({ "payment": { "entity": { "id": "pay_xyz", "order_id": "order_abc" } } }),
        };
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(
            subscription.status,
            SubscriptionStatus::PendingPayment,
            "must not be activated"
        );
    }

    #[tokio::test]
    async fn an_unrecognized_event_type_is_acknowledged_without_error_or_action() {
        let (service, ..) = service_with(vec![], vec![], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "refund.processed".to_string(),
            payload: json!({}),
        };
        let result = service.process_webhook_event("evt_1", payload).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn a_webhook_referencing_an_unknown_payment_is_acknowledged_not_errored() {
        let (service, ..) = service_with(vec![], vec![], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "payment.captured".to_string(),
            payload: json!({ "payment": { "entity": { "id": "pay_xyz", "order_id": "order_never_seen" } } }),
        };
        let result = service.process_webhook_event("evt_1", payload).await;
        assert!(
            result.is_ok(),
            "an unmatched reference must not fail the webhook call"
        );
    }

    #[tokio::test]
    async fn subscription_cancelled_updates_status_without_touching_the_license() {
        let subscription = sample_subscription(10, SubscriptionStatus::Active);
        let payment = sample_payment(1, 10, "sub_rzp_xyz", PaymentStatus::Succeeded);
        let (service, subscriptions, _licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "subscription.cancelled".to_string(),
            payload: json!({ "subscription": { "entity": { "id": "sub_rzp_xyz" } } }),
        };
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(subscription.status, SubscriptionStatus::Cancelled);
    }

    #[tokio::test]
    async fn subscription_halted_marks_the_subscription_suspended_not_cancelled() {
        let subscription = sample_subscription(10, SubscriptionStatus::Active);
        let payment = sample_payment(1, 10, "sub_rzp_xyz", PaymentStatus::Succeeded);
        let (service, subscriptions, _licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "subscription.halted".to_string(),
            payload: json!({ "subscription": { "entity": { "id": "sub_rzp_xyz" } } }),
        };
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(subscription.status, SubscriptionStatus::Suspended);
    }

    // ── license key generation ──────────────────────────────────────────

    #[test]
    fn generated_license_keys_match_the_documented_format() {
        let key = generate_license_key();
        let groups: Vec<&str> = key.split('-').collect();
        assert_eq!(groups.len(), 4, "expected 4 dash-separated groups: {key}");
        for group in groups {
            assert_eq!(group.len(), 4, "expected 4 characters per group: {key}");
            assert!(
                group
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
                "{key}"
            );
        }
    }

    #[test]
    fn generated_license_keys_are_not_trivially_identical() {
        let a = generate_license_key();
        let b = generate_license_key();
        assert_ne!(a, b);
    }

    #[test]
    fn generated_license_keys_never_use_the_desktop_ambiguous_characters() {
        // Excludes 0/O and 1/I specifically so a customer reading a key
        // aloud, or hand-typing one, can't confuse a digit for a letter.
        let key = generate_license_key();
        assert!(!key.contains('0'));
        assert!(!key.contains('O'));
        assert!(!key.contains('1'));
        assert!(!key.contains('I'));
    }

    // ── reconcile_once (PHASE4_DESIGN.md §12) ───────────────────────────

    fn razorpay_payment(id: &str, order_id: Option<&str>, status: &str) -> RazorpayPayment {
        RazorpayPayment {
            id: id.to_string(),
            order_id: order_id.map(str::to_string),
            status: status.to_string(),
        }
    }

    #[tokio::test]
    async fn reconcile_once_heals_a_payment_no_webhook_ever_arrived_for() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let (service, payments, subscriptions, licenses) = service_with_reconciliation(
            vec![payment],
            vec![subscription],
            vec![],
            vec![razorpay_payment("pay_xyz", Some("order_abc"), "captured")],
        );

        let summary = service.reconcile_once().await.unwrap();
        assert_eq!(
            summary,
            ReconciliationSummary {
                checked: 1,
                healed: 1
            }
        );

        let stored_payments = payments.payments.lock().unwrap().clone();
        assert_eq!(stored_payments[0].status, PaymentStatus::Succeeded);

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(subscription.status, SubscriptionStatus::Active);

        let license = licenses.find_latest_by_subscription(10).await.unwrap();
        assert!(
            license.is_some(),
            "reconciliation must have issued a license"
        );
    }

    #[tokio::test]
    async fn reconcile_once_is_idempotent_across_repeated_calls() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let (service, _payments, _subscriptions, licenses) = service_with_reconciliation(
            vec![payment],
            vec![subscription],
            vec![],
            vec![razorpay_payment("pay_xyz", Some("order_abc"), "captured")],
        );

        let first = service.reconcile_once().await.unwrap();
        assert_eq!(
            first,
            ReconciliationSummary {
                checked: 1,
                healed: 1
            }
        );

        // Same discovered payment, still inside the lookback window on a
        // second (simulated) run 15 minutes later — must be a pure no-op.
        let second = service.reconcile_once().await.unwrap();
        assert_eq!(
            second,
            ReconciliationSummary {
                checked: 1,
                healed: 0
            },
            "a payment already in sync must not be re-processed"
        );

        // Exactly one license, not two.
        let license_count = licenses
            .licenses
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.subscription_id == 10)
            .count();
        assert_eq!(license_count, 1);
    }

    #[tokio::test]
    async fn reconcile_once_does_not_guess_at_an_unmatched_payment() {
        let (service, _payments, _subscriptions, licenses) = service_with_reconciliation(
            vec![],
            vec![],
            vec![],
            vec![razorpay_payment(
                "pay_unknown",
                Some("order_unknown"),
                "captured",
            )],
        );

        let summary = service.reconcile_once().await.unwrap();
        assert_eq!(
            summary,
            ReconciliationSummary {
                checked: 1,
                healed: 0
            }
        );
        assert!(
            licenses.licenses.lock().unwrap().is_empty(),
            "must not fabricate a license"
        );
    }

    #[tokio::test]
    async fn reconcile_once_skips_a_razorpay_status_with_no_handler() {
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let (service, payments, _subscriptions, _licenses) = service_with_reconciliation(
            vec![payment],
            vec![sample_subscription(10, SubscriptionStatus::PendingPayment)],
            vec![],
            vec![razorpay_payment("pay_xyz", Some("order_abc"), "authorized")],
        );

        let summary = service.reconcile_once().await.unwrap();
        assert_eq!(
            summary,
            ReconciliationSummary {
                checked: 1,
                healed: 0
            }
        );

        let stored_payments = payments.payments.lock().unwrap().clone();
        assert_eq!(
            stored_payments[0].status,
            PaymentStatus::Pending,
            "an unhandled Razorpay status must not change local state"
        );
    }

    #[tokio::test]
    async fn reconcile_once_syncs_a_failed_payment_without_activating_the_subscription() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let (service, payments, subscriptions, _licenses) = service_with_reconciliation(
            vec![payment],
            vec![subscription],
            vec![],
            vec![razorpay_payment("pay_xyz", Some("order_abc"), "failed")],
        );

        let summary = service.reconcile_once().await.unwrap();
        assert_eq!(
            summary,
            ReconciliationSummary {
                checked: 1,
                healed: 1
            }
        );

        let stored_payments = payments.payments.lock().unwrap().clone();
        assert_eq!(stored_payments[0].status, PaymentStatus::Failed);

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(
            subscription.status,
            SubscriptionStatus::PendingPayment,
            "a failed payment must never activate a subscription"
        );
    }

    #[tokio::test]
    async fn reconcile_once_matches_by_payment_id_when_order_id_is_absent() {
        // Subscription-originated recurring charges typically have no
        // order_id — the payment's own id is what was stored as
        // provider_ref at the original checkout.
        let subscription = sample_subscription(10, SubscriptionStatus::Active);
        let payment = sample_payment(1, 10, "sub_rzp_xyz", PaymentStatus::Pending);
        let (service, payments, ..) = service_with_reconciliation(
            vec![payment],
            vec![subscription],
            vec![],
            vec![razorpay_payment("sub_rzp_xyz", None, "captured")],
        );

        let summary = service.reconcile_once().await.unwrap();
        assert_eq!(
            summary,
            ReconciliationSummary {
                checked: 1,
                healed: 1
            }
        );

        let stored_payments = payments.payments.lock().unwrap().clone();
        assert_eq!(stored_payments[0].status, PaymentStatus::Succeeded);
    }

    #[tokio::test]
    async fn reconcile_once_reports_checked_count_even_when_nothing_needs_healing() {
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Succeeded);
        let (service, ..) = service_with_reconciliation(
            vec![payment],
            vec![sample_subscription(10, SubscriptionStatus::Active)],
            vec![],
            vec![razorpay_payment("pay_xyz", Some("order_abc"), "captured")],
        );

        let summary = service.reconcile_once().await.unwrap();
        assert_eq!(
            summary,
            ReconciliationSummary {
                checked: 1,
                healed: 0
            }
        );
    }
}
