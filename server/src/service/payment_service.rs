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
//! **Phase 4J.1 (production readiness audit, CRITICAL finding #1):**
//! `process_webhook_event` used to check `payment_webhook_events` for
//! `(provider, event_id)` and only insert that ledger row *last*, after
//! every other write — a check-then-act race where two concurrent calls
//! for the same event could both pass the "not found" check before either
//! had written anything. The claim now happens *first*, atomically, via
//! `PaymentWebhookEventRepository::claim_and_apply` — see that trait's
//! doc comment for the full fix. This method's job is unchanged: resolve
//! *what* should be applied (`repository::payment_webhook_event::
//! WebhookMutation`) from already-known local state, then hand it to the
//! repository to claim-and-apply atomically.
//!
//! **Phase 4K.1 (production readiness audit, CRITICAL finding #2):**
//! `payment_link.paid` is now a recognized event type
//! (`resolve_payment_link_paid`), fixing Payment-Link-based checkouts
//! (`lifetime`/`trial`) whose real-time webhook could never correlate
//! back to the `payments` row stored at checkout — see that method's doc
//! comment for the id-namespace mismatch this closes. Purely additive:
//! `payment.captured`/`subscription.*` handling, `extract_entity_ref`,
//! and the checkout/idempotency/reconciliation paths are unchanged.
//!
//! **Phase 4K.2 (refund/chargeback handling):** `refund.created`/
//! `.processed` and `payment.dispute.created`/`.closed` are now
//! recognized event types. These webhooks only ever reference the real
//! Razorpay payment id (`payload.payment.entity.id`), never
//! `provider_ref`'s checkout-time payment-link/subscription id, so
//! correlation reads a new, independent column instead:
//! `payments.gateway_payment_id` (`migrations/0004_add_payment_dispute_support.sql`),
//! populated by `resolve_activation` whenever an activating webhook's
//! payload carries one. See `find_payment_and_license`,
//! `resolve_refund`, `resolve_dispute_created`, and
//! `resolve_dispute_closed` for the new resolution logic — all funnel
//! into the same `claim_and_apply` idempotency guarantee every other
//! mutation already relies on. Never deletes a payment, subscription, or
//! license row; only transitions their status.
//!
//! **Production Hardening, Finding C2:** `resolve_activation` now compares
//! a webhook's actual captured `amount`/`currency`
//! (`payload.payment.entity.{amount,currency}`) against `payments.
//! amount_minor`/`currency` — the values this server itself stored at
//! checkout time — before granting entitlement, refusing to activate on a
//! mismatch. Lenient on absence (a payload missing either field verifies
//! nothing and still activates, same as every pre-existing test payload
//! already does); reuses the same `Ok(WebhookMutation::None)` "log and
//! acknowledge, mutate nothing" shape every other unsafe-to-proceed case in
//! `resolve_activation` already returns — no new response shape, no change
//! to `claim_and_apply`'s idempotency guarantee. `resolve_refund`/
//! `resolve_dispute_created`/`resolve_dispute_closed` never call
//! `resolve_activation` at all, so the refund/dispute flow is untouched.

use crate::domain::{
    LicenseRecordStatus, NewPayment, NewPaymentWebhookEvent, NewSubscription, Payment,
    PaymentStatus, PlanType, SubscriptionStatus,
};
use crate::razorpay::{
    extract_dispute_status, extract_entity_amount_minor, extract_entity_currency,
    extract_entity_id, extract_entity_ref, CreateCheckoutRequest, RazorpayClient, RazorpayPayment,
    RazorpayWebhookPayload,
};
use crate::repository::error::RepositoryError;
use crate::repository::license::LicenseRepository;
use crate::repository::payment::PaymentRepository;
use crate::repository::payment_webhook_event::{
    AffectedLicense, AffectedLicenseStatusChange, LicenseMutation, PaymentWebhookEventRepository,
    WebhookMutation,
};
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
    /// `RECONCILIATION_MAX_AGE_HOURS` (Phase 4K.4,
    /// `config::ReconciliationConfig`) — how far back `reconcile_once`
    /// asks Razorpay to list payments from. Previously a fixed
    /// `RECONCILIATION_LOOKBACK_HOURS` constant; `2` is still the default
    /// when the variable is unset.
    reconciliation_lookback_hours: i64,
}

impl PaymentService {
    pub fn new(
        payment_repository: Arc<dyn PaymentRepository>,
        webhook_event_repository: Arc<dyn PaymentWebhookEventRepository>,
        subscription_repository: Arc<dyn SubscriptionRepository>,
        license_repository: Arc<dyn LicenseRepository>,
        razorpay_client: Arc<dyn RazorpayClient>,
        reconciliation_lookback_hours: i64,
    ) -> Self {
        PaymentService {
            payment_repository,
            webhook_event_repository,
            subscription_repository,
            license_repository,
            razorpay_client,
            reconciliation_lookback_hours,
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
    /// same `event_id` is a no-op — and, since Phase 4J.1, a *concurrent*
    /// second call is too (see `PaymentWebhookEventRepository::
    /// claim_and_apply`'s doc comment).
    ///
    /// Resolves *what* this event implies (`resolve_*` below — read-only
    /// lookups keyed by immutable provider references, safe to run before
    /// any idempotency claim) into a `WebhookMutation`, then hands both the
    /// ledger row and the mutation to the repository, which claims and
    /// applies them atomically. If the claim loses the race (the event was
    /// already processed by a concurrent or earlier call), the repository
    /// applies nothing — this method still returns `Ok(())`.
    pub async fn process_webhook_event(
        &self,
        event_id: &str,
        payload: RazorpayWebhookPayload,
    ) -> Result<(), PaymentOperationError> {
        let mutation = match payload.event.as_str() {
            "payment.captured" => self.resolve_payment_captured(&payload).await?,
            "payment_link.paid" => self.resolve_payment_link_paid(&payload).await?,
            "payment.failed" => self.resolve_payment_failed(&payload).await?,
            "subscription.activated" | "subscription.charged" => {
                self.resolve_subscription_active(&payload).await?
            }
            "subscription.cancelled" | "subscription.halted" => {
                self.resolve_subscription_inactive(&payload).await?
            }
            "refund.created" | "refund.processed" => self.resolve_refund(&payload).await?,
            "payment.dispute.created" => self.resolve_dispute_created(&payload).await?,
            "payment.dispute.closed" => self.resolve_dispute_closed(&payload).await?,
            other => {
                tracing::info!(
                    event = other,
                    "unrecognized webhook event type; acknowledged, no action taken"
                );
                WebhookMutation::None
            }
        };

        let new_event = NewPaymentWebhookEvent {
            provider: PROVIDER.to_string(),
            event_id: event_id.to_string(),
            event_type: payload.event.clone(),
            payload: payload.payload.clone(),
        };

        self.webhook_event_repository
            .claim_and_apply(new_event, mutation)
            .await?;

        Ok(())
    }

    /// Production Hardening, Finding H2: resolves `provider_ref` to its
    /// local `payments` row, treating both "no match" and "ambiguous
    /// match" (`RepositoryError::DuplicateProviderReference` —
    /// `find_by_provider_ref` no longer silently picks one) the same
    /// way every `resolve_*` method here already treats "nothing safe to
    /// act on": log a warning and let the caller fall through to its own
    /// `Ok(WebhookMutation::None)`, never letting the ambiguity surface as
    /// a `PaymentOperationError` (which would otherwise turn into a `500`
    /// and trigger pointless Razorpay redelivery of a webhook this server
    /// can never safely resolve until the underlying duplicate rows are
    /// fixed).
    async fn find_payment_by_provider_ref_or_none(
        &self,
        provider_ref: &str,
    ) -> Result<Option<Payment>, PaymentOperationError> {
        match self
            .payment_repository
            .find_by_provider_ref(provider_ref)
            .await
        {
            Ok(payment) => Ok(payment),
            Err(RepositoryError::DuplicateProviderReference(_)) => {
                tracing::warn!(
                    provider_ref = %provider_ref,
                    "multiple payments rows share this provider_ref; refusing to guess which one this webhook concerns"
                );
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn resolve_payment_captured(
        &self,
        payload: &RazorpayWebhookPayload,
    ) -> Result<WebhookMutation, PaymentOperationError> {
        let Some(provider_ref) = extract_entity_ref(&payload.payload, "payment") else {
            tracing::warn!("payment.captured webhook missing a usable entity reference; ignoring");
            return Ok(WebhookMutation::None);
        };
        let gateway_payment_id = extract_entity_id(&payload.payload, "payment");
        let captured_amount_minor = extract_entity_amount_minor(&payload.payload, "payment");
        let captured_currency = extract_entity_currency(&payload.payload, "payment");
        self.resolve_activation(
            &provider_ref,
            gateway_payment_id,
            captured_amount_minor,
            captured_currency,
        )
        .await
    }

    /// `payment_link.paid` — Razorpay's dedicated event for a completed
    /// Payment Link (`PHASE4_DESIGN.md` §2's `lifetime`/`trial` checkout
    /// path, `razorpay::client`'s `POST /v1/payment_links`). Phase 4K.1
    /// (production readiness audit CRITICAL finding #2) fix:
    /// `create_checkout_session` stores the *Payment Link's own* id
    /// (`payments.provider_ref`, e.g. `plink_...`) at checkout time, but
    /// `resolve_payment_captured` below only ever reads
    /// `payload.payment.entity.{order_id,id}` — a different Razorpay id
    /// namespace (the underlying payment/order, freshly generated per
    /// attempt) that never equals the stored Payment Link id. `payload
    /// .payment_link.entity.id` is the one field Razorpay actually sends
    /// that matches what was stored, so this event — previously
    /// unhandled and silently dropped into `process_webhook_event`'s
    /// `other` catch-all — is what correlation must key on for this
    /// checkout path. `extract_entity_ref` needs no change: called with
    /// `"payment_link"`, its existing `order_id`-then-`id` fallback
    /// already returns `entity.id` since a payment_link entity has no
    /// `order_id` field.
    async fn resolve_payment_link_paid(
        &self,
        payload: &RazorpayWebhookPayload,
    ) -> Result<WebhookMutation, PaymentOperationError> {
        let Some(provider_ref) = extract_entity_ref(&payload.payload, "payment_link") else {
            tracing::warn!("payment_link.paid webhook missing a usable entity reference; ignoring");
            return Ok(WebhookMutation::None);
        };
        let gateway_payment_id = extract_entity_id(&payload.payload, "payment");
        let captured_amount_minor = extract_entity_amount_minor(&payload.payload, "payment");
        let captured_currency = extract_entity_currency(&payload.payload, "payment");
        self.resolve_activation(
            &provider_ref,
            gateway_payment_id,
            captured_amount_minor,
            captured_currency,
        )
        .await
    }

    async fn resolve_payment_failed(
        &self,
        payload: &RazorpayWebhookPayload,
    ) -> Result<WebhookMutation, PaymentOperationError> {
        let Some(provider_ref) = extract_entity_ref(&payload.payload, "payment") else {
            tracing::warn!("payment.failed webhook missing a usable entity reference; ignoring");
            return Ok(WebhookMutation::None);
        };
        let Some(payment) = self
            .find_payment_by_provider_ref_or_none(&provider_ref)
            .await?
        else {
            tracing::warn!(provider_ref = %provider_ref, "payment.failed webhook references an unknown payment; ignoring");
            return Ok(WebhookMutation::None);
        };
        // Phase 4L.3 (production validation, CRITICAL): a genuine "this
        // attempt failed" transition only ever makes sense from `Pending`.
        // Razorpay can redeliver/reorder webhooks — a `payment.failed`
        // arriving *after* the same payment's `payment.captured` (network
        // reordering, a retried delivery) previously overwrote
        // `payments.status` back to `failed` unconditionally, silently
        // forking the stored status away from reality (subscription/
        // license stayed untouched, so support/finance would see a
        // "failed" payment that actually funded an active license). Any
        // other current status means this event is stale/out of order —
        // ignored, not applied.
        if payment.status != PaymentStatus::Pending {
            tracing::warn!(
                provider_ref = %provider_ref,
                current_status = %payment.status,
                "payment.failed webhook arrived for a payment no longer Pending; ignoring as out-of-order rather than overwriting its status"
            );
            return Ok(WebhookMutation::None);
        }
        Ok(WebhookMutation::MarkPaymentFailed {
            payment_id: payment.id,
        })
    }

    async fn resolve_subscription_active(
        &self,
        payload: &RazorpayWebhookPayload,
    ) -> Result<WebhookMutation, PaymentOperationError> {
        let Some(provider_ref) = extract_entity_ref(&payload.payload, "subscription") else {
            tracing::warn!("subscription webhook missing a usable entity reference; ignoring");
            return Ok(WebhookMutation::None);
        };
        let gateway_payment_id = extract_entity_id(&payload.payload, "payment");
        let captured_amount_minor = extract_entity_amount_minor(&payload.payload, "payment");
        let captured_currency = extract_entity_currency(&payload.payload, "payment");
        self.resolve_activation(
            &provider_ref,
            gateway_payment_id,
            captured_amount_minor,
            captured_currency,
        )
        .await
    }

    /// `subscription.cancelled`/`.halted`.
    ///
    /// **Production Hardening, Finding #9:** as well as transitioning the
    /// subscription itself, this now resolves the subscription's current
    /// license (`find_latest_by_subscription` — same lookup
    /// `find_payment_and_license` uses for refund/dispute, so a license
    /// already `revoked` by an earlier refund/dispute is correctly excluded
    /// rather than "revived") and includes it in the same
    /// `WebhookMutation`, applied atomically alongside the subscription
    /// write by `claim_and_apply`. `subscription.halted` (temporary —
    /// Razorpay's own retry/dunning window) maps to `Suspended`, matching
    /// the existing dispute-suspension precedent: reversible, and still
    /// found (not `revoked`) by a later `resolve_activation` if the
    /// subscription recovers. `subscription.cancelled` (terminal) maps to
    /// `Revoked`, matching the existing refund precedent: a later
    /// legitimate payment on the same subscription issues a fresh license
    /// rather than reviving this one, since `find_latest_by_subscription`
    /// excludes it once revoked.
    async fn resolve_subscription_inactive(
        &self,
        payload: &RazorpayWebhookPayload,
    ) -> Result<WebhookMutation, PaymentOperationError> {
        let Some(provider_ref) = extract_entity_ref(&payload.payload, "subscription") else {
            tracing::warn!("subscription webhook missing a usable entity reference; ignoring");
            return Ok(WebhookMutation::None);
        };
        let Some(payment) = self
            .find_payment_by_provider_ref_or_none(&provider_ref)
            .await?
        else {
            tracing::warn!(provider_ref = %provider_ref, "subscription webhook references an unknown payment; ignoring");
            return Ok(WebhookMutation::None);
        };
        let Some(subscription) = self
            .subscription_repository
            .find_by_id(payment.subscription_id)
            .await?
        else {
            return Ok(WebhookMutation::None);
        };

        let (new_status, license_status) = if payload.event == "subscription.halted" {
            (
                SubscriptionStatus::Suspended,
                LicenseRecordStatus::Suspended,
            )
        } else {
            (SubscriptionStatus::Cancelled, LicenseRecordStatus::Revoked)
        };

        let license = self
            .license_repository
            .find_latest_by_subscription(subscription.id)
            .await?
            .map(|l| AffectedLicenseStatusChange {
                license_id: l.id,
                expires_at: l.expires_at,
                status: license_status,
            });

        Ok(WebhookMutation::UpdateSubscriptionStatus {
            subscription_id: subscription.id,
            status: new_status,
            current_period_end: subscription.current_period_end,
            license,
        })
    }

    /// Shared by both `payment.captured` (one-time, Payment Links) and
    /// `subscription.activated`/`.charged` (recurring) — see this module's
    /// doc comment for the "reuse the original payments row on renewal"
    /// simplification. Resolves what *should* happen without mutating
    /// anything itself; `PaymentWebhookEventRepository::claim_and_apply`
    /// performs the actual write, atomically with the idempotency claim,
    /// only if this call wins it.
    async fn resolve_activation(
        &self,
        provider_ref: &str,
        gateway_payment_id: Option<String>,
        captured_amount_minor: Option<i64>,
        captured_currency: Option<String>,
    ) -> Result<WebhookMutation, PaymentOperationError> {
        let Some(payment) = self
            .find_payment_by_provider_ref_or_none(provider_ref)
            .await?
        else {
            tracing::warn!(provider_ref = %provider_ref, "webhook references an unknown payment; ignoring (no local record)");
            return Ok(WebhookMutation::None);
        };

        // Production Hardening, Finding C2: verify the webhook's actual
        // captured amount/currency against `payments.amount_minor`/
        // `currency` — the values this server itself stored at checkout
        // time — before granting entitlement. Without this, a manipulated
        // or partial-capture webhook payload (or a compromised/
        // misconfigured Razorpay account) could still activate full
        // entitlement regardless of what was actually captured. Lenient on
        // *absence* (a missing/non-integer `amount` or missing `currency`
        // field, same treatment `extract_entity_id`'s own absence already
        // gets elsewhere in this file) — there is nothing to verify
        // against in that case, so it isn't itself treated as a mismatch;
        // only an actual present-and-different value blocks activation.
        // Reuses the exact same "log and acknowledge without mutating"
        // shape (`Ok(WebhookMutation::None)`) every other "can't safely
        // proceed" case in this method already returns — no new API
        // contract, no new response shape, the webhook is still claimed
        // (never retried by Razorpay) but nothing activates.
        if let Some(amount) = captured_amount_minor {
            if amount != payment.amount_minor {
                tracing::warn!(
                    provider_ref = %provider_ref,
                    stored_amount_minor = payment.amount_minor,
                    captured_amount_minor = amount,
                    "webhook captured amount does not match the stored checkout amount; refusing to activate"
                );
                return Ok(WebhookMutation::None);
            }
        }
        if let Some(currency) = captured_currency.as_deref() {
            if currency != payment.currency {
                tracing::warn!(
                    provider_ref = %provider_ref,
                    stored_currency = %payment.currency,
                    captured_currency = %currency,
                    "webhook captured currency does not match the stored checkout currency; refusing to activate"
                );
                return Ok(WebhookMutation::None);
            }
        }

        // Phase 4L.3 (production validation, CRITICAL): `payment.captured`/
        // `payment_link.paid`/`subscription.activated`/`.charged` can all
        // redeliver or arrive out of order. Without this guard, a
        // redelivery landing *after* `refund.created` or a lost dispute
        // (`payment.dispute.closed`, merchant lost) unconditionally
        // reactivated the subscription and license — silently reviving
        // access for a payment that was refunded or lost its dispute, an
        // idempotency-key check alone can't catch since a genuine Razorpay
        // redelivery carries a *different* event_id than the original.
        // `Refunded`/`Disputed` are only ever left by `resolve_refund`/
        // `resolve_dispute_created`, both deliberate terminal-or-interim
        // outcomes — the only legitimate way out of `Disputed` is
        // `resolve_dispute_closed` (merchant won), never a re-activation
        // event. Every other current status (`Pending`, `Failed`,
        // `Succeeded`) still activates normally, including the ordinary
        // "failed attempt, customer retried, this one succeeded" and
        // renewal-extends-an-existing-license cases below.
        if matches!(
            payment.status,
            PaymentStatus::Refunded | PaymentStatus::Disputed
        ) {
            tracing::warn!(
                provider_ref = %provider_ref,
                current_status = %payment.status,
                "activation-shaped webhook arrived for a payment already refunded or disputed; ignoring as out-of-order rather than reviving it"
            );
            return Ok(WebhookMutation::None);
        }

        let Some(subscription) = self
            .subscription_repository
            .find_by_id(payment.subscription_id)
            .await?
        else {
            return Err(PaymentOperationError::Repository(
                RepositoryError::InvalidData(format!(
                    "payment references missing subscription {}",
                    payment.subscription_id
                )),
            ));
        };

        let period_end = plan_duration(subscription.plan_type).map(|d| Utc::now() + d);

        let license = match self
            .license_repository
            .find_latest_by_subscription(subscription.id)
            .await?
        {
            Some(existing) => LicenseMutation::Extend {
                license_id: existing.id,
            },
            None => LicenseMutation::Insert {
                license_key: generate_license_key(),
                max_devices: 1,
                grace_period_days: 7,
            },
        };

        Ok(WebhookMutation::ActivateSubscriptionAndLicense {
            payment_id: payment.id,
            subscription_id: subscription.id,
            period_end,
            license,
            gateway_payment_id,
        })
    }

    /// Shared by `refund.*`/`payment.dispute.*` resolution (Phase 4K.2):
    /// locates the payment via `gateway_payment_id` — the real Razorpay
    /// payment id, the *only* reference these events ever carry (never
    /// `provider_ref`'s checkout-time payment-link/subscription id, see
    /// `domain::Payment::gateway_payment_id`'s doc comment) — and its
    /// current license, if any (`find_latest_by_subscription` already
    /// excludes an already-`revoked` one, so a refund/dispute on a
    /// payment whose license was previously revoked, or never issued,
    /// correctly finds none to further mutate).
    async fn find_payment_and_license(
        &self,
        gateway_payment_id: &str,
    ) -> Result<Option<(Payment, Option<AffectedLicense>)>, PaymentOperationError> {
        let Some(payment) = self
            .payment_repository
            .find_by_gateway_payment_id(gateway_payment_id)
            .await?
        else {
            return Ok(None);
        };

        let license = self
            .license_repository
            .find_latest_by_subscription(payment.subscription_id)
            .await?
            .map(|l| AffectedLicense {
                license_id: l.id,
                expires_at: l.expires_at,
            });

        Ok(Some((payment, license)))
    }

    /// `refund.created` / `refund.processed` (Phase 4K.2). Both events are
    /// resolved identically — whichever arrives first performs the
    /// mutation; the other (or a genuine redelivery of either) is an
    /// idempotent no-op via `claim_and_apply`'s per-`event_id` claim, same
    /// as `payment.captured`/`subscription.charged` both funneling into
    /// `resolve_activation`.
    async fn resolve_refund(
        &self,
        payload: &RazorpayWebhookPayload,
    ) -> Result<WebhookMutation, PaymentOperationError> {
        let Some(gateway_payment_id) = extract_entity_id(&payload.payload, "payment") else {
            tracing::warn!("refund webhook missing a usable payment reference; ignoring");
            return Ok(WebhookMutation::None);
        };
        let Some((payment, license)) = self.find_payment_and_license(&gateway_payment_id).await?
        else {
            tracing::warn!(gateway_payment_id = %gateway_payment_id, "refund webhook references an unknown payment; ignoring");
            return Ok(WebhookMutation::None);
        };
        Ok(WebhookMutation::RefundPayment {
            payment_id: payment.id,
            license,
        })
    }

    /// `payment.dispute.created` (Phase 4K.2) — suspends the license
    /// pending `payment.dispute.closed`; never revokes outright, since a
    /// dispute can still resolve in the merchant's favor.
    async fn resolve_dispute_created(
        &self,
        payload: &RazorpayWebhookPayload,
    ) -> Result<WebhookMutation, PaymentOperationError> {
        let Some(gateway_payment_id) = extract_entity_id(&payload.payload, "payment") else {
            tracing::warn!(
                "payment.dispute.created webhook missing a usable payment reference; ignoring"
            );
            return Ok(WebhookMutation::None);
        };
        let Some((payment, license)) = self.find_payment_and_license(&gateway_payment_id).await?
        else {
            tracing::warn!(gateway_payment_id = %gateway_payment_id, "payment.dispute.created webhook references an unknown payment; ignoring");
            return Ok(WebhookMutation::None);
        };
        Ok(WebhookMutation::MarkPaymentDisputed {
            payment_id: payment.id,
            license,
        })
    }

    /// `payment.dispute.closed` (Phase 4K.2). The outcome comes from
    /// Razorpay's own `payload.dispute.entity.status` — `"won"` (merchant
    /// won, restore) or `"lost"` (customer won, payment refunded and
    /// license stays revoked) — never guessed at: any other status
    /// (including a still-open dispute closing prematurely, or a
    /// malformed/missing field) is acknowledged without mutating anything,
    /// matching this codebase's existing no-guessing posture
    /// (`PHASE4_DESIGN.md` §12.3).
    async fn resolve_dispute_closed(
        &self,
        payload: &RazorpayWebhookPayload,
    ) -> Result<WebhookMutation, PaymentOperationError> {
        let Some(gateway_payment_id) = extract_entity_id(&payload.payload, "payment") else {
            tracing::warn!(
                "payment.dispute.closed webhook missing a usable payment reference; ignoring"
            );
            return Ok(WebhookMutation::None);
        };
        let merchant_won = match extract_dispute_status(&payload.payload).as_deref() {
            Some("won") => true,
            Some("lost") => false,
            Some(other) => {
                tracing::warn!(
                    status = other,
                    "payment.dispute.closed webhook has an unrecognized dispute status; ignoring"
                );
                return Ok(WebhookMutation::None);
            }
            None => {
                tracing::warn!("payment.dispute.closed webhook missing a dispute status; ignoring");
                return Ok(WebhookMutation::None);
            }
        };
        let Some((payment, license)) = self.find_payment_and_license(&gateway_payment_id).await?
        else {
            tracing::warn!(gateway_payment_id = %gateway_payment_id, "payment.dispute.closed webhook references an unknown payment; ignoring");
            return Ok(WebhookMutation::None);
        };
        Ok(WebhookMutation::ResolveDispute {
            payment_id: payment.id,
            license,
            merchant_won,
        })
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
        let since = Utc::now() - Duration::hours(self.reconciliation_lookback_hours);
        let payments = self
            .razorpay_client
            .list_payments_since(since)
            .await
            .map_err(|e| {
                // Phase 4K.4: `RazorpayError::is_recoverable()` distinguishes
                // a `Transient` failure (worth retrying next tick — a
                // connect/timeout error, or a Razorpay 5xx) from a
                // `Permanent` one (retrying the identical request won't
                // help) so an operator grepping logs doesn't mistake a real
                // integration problem for ordinary network flakiness. The
                // returned `PaymentOperationError` shape is unchanged —
                // this only affects what gets logged.
                tracing::warn!(
                    recoverable = e.is_recoverable(),
                    error = %e,
                    "reconciliation: list_payments_since failed"
                );
                PaymentOperationError::ProviderError(e.to_string())
            })?;

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
            lookback_hours = self.reconciliation_lookback_hours,
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
            // "authorized") — nothing to heal against.
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
            // Phase 4L.3 (production validation, HIGH): a lost
            // `refund.created`/`.processed` webhook previously had no
            // reconciliation backstop at all — `map_razorpay_payment_status`
            // dropped "refunded" entirely, so a payment refunded at
            // Razorpay while this server's webhook never arrived (or
            // failed) stayed `Succeeded`/license `Active` forever, with
            // no self-healing path. `resolve_refund` reads
            // `payload.payment.entity.id` (`extract_entity_id`) — already
            // exactly what the synthetic payload below provides, so no
            // payload-shape change is needed for this to work.
            PaymentStatus::Refunded => "refund.created",
            // `Pending`/`Disputed` have no corresponding Razorpay payments-
            // list status this maps to (see `map_razorpay_payment_status`),
            // so are unreachable here.
            _ => return Ok(false),
        };
        // Includes `local_payment.id`, not just Razorpay's own reported
        // `(status, id)` — this key must identify "this local row's
        // transition to this status," not merely "a Razorpay payment
        // reporting this status under this id." Without the local id, two
        // reconciliation passes that each discover a *different* local
        // payment row but happen to observe the same upstream `(status,
        // id)` pair would share one idempotency-ledger entry: whichever
        // claims it first silently blocks the other's real update — the
        // loser's row never actually transitions, yet `reconcile_one`
        // (having no way to tell "applied" from "already claimed" apart at
        // this call site) still reports it as healed. Scoping the key to
        // the resolved local row keeps two independent rows from ever
        // contending for the same claim, so each genuinely gets healed
        // once and reports a stable, idempotent result on every repeat run.
        let event_id = format!(
            "reconcile:{}:{}:{}",
            local_payment.id, razorpay_payment.status, razorpay_payment.id
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

/// `PHASE4_DESIGN.md` §14 item 8/9 (confirmed, fixed value; the scheduler
/// interval counterpart, `RECONCILIATION_INTERVAL_SECS`, is now
/// configurable — see `reconciliation::interval_from_config`).
pub const RECONCILIATION_INTERVAL_MINUTES: i64 = 15;

#[derive(Debug, PartialEq, Eq)]
pub struct ReconciliationSummary {
    pub checked: usize,
    pub healed: usize,
}

/// Maps a Razorpay payment status string onto the one local status it
/// implies, for the statuses reconciliation actually knows how to heal
/// against (matching `process_webhook_event`'s own handled event types —
/// see this module's doc comment). Any other Razorpay status (e.g.
/// `"authorized"`) has no corresponding webhook handler in this phase, so
/// reconciliation deliberately doesn't attempt one either.
///
/// `"refunded"` (Phase 4L.3, production validation, HIGH) was previously
/// dropped here — a lost `refund.*` webhook had no reconciliation
/// backstop at all, unlike every other event type. `payment.dispute.*`
/// still has none: Razorpay's payments-list API (what reconciliation
/// calls) reports payment status, not dispute status, so an open/lost
/// dispute isn't observable from this endpoint at all — only a real
/// `payment.dispute.*` webhook can heal that gap.
fn map_razorpay_payment_status(status: &str) -> Option<PaymentStatus> {
    match status {
        "captured" => Some(PaymentStatus::Succeeded),
        "failed" => Some(PaymentStatus::Failed),
        "refunded" => Some(PaymentStatus::Refunded),
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
    use crate::domain::{
        License, LicenseRecordStatus, NewLicense, Payment, PaymentWebhookEvent, Subscription,
    };
    use crate::razorpay::{CreateCheckoutResponse, RazorpayError};
    use crate::repository::payment_webhook_event::ClaimOutcome;
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
                gateway_payment_id: None,
                status: new_payment.status,
                created_at: Utc::now(),
            };
            self.payments.lock().unwrap().push(payment.clone());
            Ok(payment)
        }

        /// Production Hardening, Finding H2: mirrors the real
        /// `PgPaymentRepository`'s "more than one match is an error, never
        /// silently pick one" semantics, so tests exercising a duplicate
        /// `provider_ref` (impossible to arrange against the real,
        /// migration-`0008`-constrained schema) get faithful behavior from
        /// this mock instead of the old "just take the first" shortcut.
        async fn find_by_provider_ref(
            &self,
            provider_ref: &str,
        ) -> Result<Option<Payment>, RepositoryError> {
            let payments = self.payments.lock().unwrap();
            let mut matches = payments
                .iter()
                .filter(|p| p.provider_ref.as_deref() == Some(provider_ref));
            let first = matches.next().cloned();
            if matches.next().is_some() {
                return Err(RepositoryError::DuplicateProviderReference(
                    provider_ref.to_string(),
                ));
            }
            Ok(first)
        }

        async fn find_by_gateway_payment_id(
            &self,
            gateway_payment_id: &str,
        ) -> Result<Option<Payment>, RepositoryError> {
            Ok(self
                .payments
                .lock()
                .unwrap()
                .iter()
                .find(|p| p.gateway_payment_id.as_deref() == Some(gateway_payment_id))
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

        async fn record_gateway_payment_id(
            &self,
            id: i64,
            gateway_payment_id: &str,
        ) -> Result<(), RepositoryError> {
            if let Some(p) = self
                .payments
                .lock()
                .unwrap()
                .iter_mut()
                .find(|p| p.id == id)
            {
                p.gateway_payment_id = Some(gateway_payment_id.to_string());
            }
            Ok(())
        }
    }

    /// Simulates `claim_and_apply`'s atomic claim-then-mutate contract
    /// entirely in memory: the `events` `Mutex` is the mock's own
    /// idempotency ledger (a check-then-insert under one lock acquisition,
    /// so two calls racing on the same `(provider, event_id)` can't both
    /// see "not claimed"), and — only for the caller that wins the claim —
    /// the resolved `WebhookMutation` is applied by calling straight
    /// through to the *same* payment/subscription/license mocks the test
    /// itself holds handles to, exactly mirroring what the real
    /// `PgPaymentWebhookEventRepository::claim_and_apply` does against
    /// real tables on one transaction.
    struct MockWebhookEventRepository {
        events: Mutex<Vec<PaymentWebhookEvent>>,
        next_id: Mutex<i64>,
        payment_repository: Arc<dyn PaymentRepository>,
        subscription_repository: Arc<dyn SubscriptionRepository>,
        license_repository: Arc<dyn LicenseRepository>,
    }

    impl MockWebhookEventRepository {
        fn new(
            payment_repository: Arc<dyn PaymentRepository>,
            subscription_repository: Arc<dyn SubscriptionRepository>,
            license_repository: Arc<dyn LicenseRepository>,
        ) -> Self {
            MockWebhookEventRepository {
                events: Mutex::new(Vec::new()),
                next_id: Mutex::new(1),
                payment_repository,
                subscription_repository,
                license_repository,
            }
        }
    }

    #[async_trait]
    impl PaymentWebhookEventRepository for MockWebhookEventRepository {
        async fn claim_and_apply(
            &self,
            new_event: NewPaymentWebhookEvent,
            mutation: WebhookMutation,
        ) -> Result<ClaimOutcome, RepositoryError> {
            {
                // Claim: check-and-insert under one lock acquisition, with
                // no `.await` point in between — the same "only one
                // concurrent caller can ever win" guarantee the real
                // `INSERT ... ON CONFLICT DO NOTHING` provides.
                let mut events = self.events.lock().unwrap();
                if events
                    .iter()
                    .any(|e| e.provider == new_event.provider && e.event_id == new_event.event_id)
                {
                    return Ok(ClaimOutcome::AlreadyProcessed);
                }
                let mut next_id = self.next_id.lock().unwrap();
                let id = *next_id;
                *next_id += 1;
                events.push(PaymentWebhookEvent {
                    id,
                    provider: new_event.provider,
                    event_id: new_event.event_id,
                    event_type: new_event.event_type,
                    payload: new_event.payload,
                    processed_at: Utc::now(),
                });
            }

            match mutation {
                WebhookMutation::None => {}
                WebhookMutation::MarkPaymentFailed { payment_id } => {
                    self.payment_repository
                        .update_status(payment_id, PaymentStatus::Failed)
                        .await?;
                }
                WebhookMutation::UpdateSubscriptionStatus {
                    subscription_id,
                    status,
                    current_period_end,
                    license,
                } => {
                    self.subscription_repository
                        .update_status(subscription_id, status, current_period_end)
                        .await?;
                    if let Some(license) = license {
                        self.license_repository
                            .extend(license.license_id, license.status, license.expires_at)
                            .await?;
                    }
                }
                WebhookMutation::ActivateSubscriptionAndLicense {
                    payment_id,
                    subscription_id,
                    period_end,
                    license,
                    gateway_payment_id,
                } => {
                    self.payment_repository
                        .update_status(payment_id, PaymentStatus::Succeeded)
                        .await?;
                    if let Some(gateway_payment_id) = gateway_payment_id {
                        self.payment_repository
                            .record_gateway_payment_id(payment_id, &gateway_payment_id)
                            .await?;
                    }
                    self.subscription_repository
                        .update_status(subscription_id, SubscriptionStatus::Active, period_end)
                        .await?;
                    match license {
                        LicenseMutation::Extend { license_id } => {
                            self.license_repository
                                .extend(license_id, LicenseRecordStatus::Active, period_end)
                                .await?;
                        }
                        LicenseMutation::Insert {
                            license_key,
                            max_devices,
                            grace_period_days,
                        } => {
                            self.license_repository
                                .insert(NewLicense {
                                    subscription_id,
                                    license_key,
                                    status: LicenseRecordStatus::Active,
                                    expires_at: period_end,
                                    max_devices,
                                    grace_period_days,
                                })
                                .await?;
                        }
                    }
                }
                WebhookMutation::RefundPayment {
                    payment_id,
                    license,
                } => {
                    self.payment_repository
                        .update_status(payment_id, PaymentStatus::Refunded)
                        .await?;
                    if let Some(license) = license {
                        self.license_repository
                            .extend(
                                license.license_id,
                                LicenseRecordStatus::Revoked,
                                license.expires_at,
                            )
                            .await?;
                    }
                }
                WebhookMutation::MarkPaymentDisputed {
                    payment_id,
                    license,
                } => {
                    self.payment_repository
                        .update_status(payment_id, PaymentStatus::Disputed)
                        .await?;
                    if let Some(license) = license {
                        self.license_repository
                            .extend(
                                license.license_id,
                                LicenseRecordStatus::Suspended,
                                license.expires_at,
                            )
                            .await?;
                    }
                }
                WebhookMutation::ResolveDispute {
                    payment_id,
                    license,
                    merchant_won,
                } => {
                    self.payment_repository
                        .update_status(
                            payment_id,
                            if merchant_won {
                                PaymentStatus::Succeeded
                            } else {
                                PaymentStatus::Refunded
                            },
                        )
                        .await?;
                    if let Some(license) = license {
                        self.license_repository
                            .extend(
                                license.license_id,
                                if merchant_won {
                                    LicenseRecordStatus::Active
                                } else {
                                    LicenseRecordStatus::Revoked
                                },
                                license.expires_at,
                            )
                            .await?;
                    }
                }
            }

            Ok(ClaimOutcome::Applied)
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

        async fn find_latest_by_user(
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

    /// Records every `since` it's called with (Phase 4K.4) — unlike
    /// `MockRazorpayClient` above, which discards it, this is specifically
    /// for asserting `reconcile_once` actually derives `since` from
    /// `PaymentService`'s configured lookback rather than a hardcoded
    /// value.
    struct RecordingRazorpayClient {
        since_calls: Arc<Mutex<Vec<chrono::DateTime<Utc>>>>,
    }

    #[async_trait]
    impl RazorpayClient for RecordingRazorpayClient {
        async fn create_checkout(
            &self,
            _req: CreateCheckoutRequest,
        ) -> Result<CreateCheckoutResponse, RazorpayError> {
            Err(RazorpayError::NotConfigured(
                "not exercised by lookback tests".to_string(),
            ))
        }

        async fn list_payments_since(
            &self,
            since: chrono::DateTime<Utc>,
        ) -> Result<Vec<RazorpayPayment>, RazorpayError> {
            self.since_calls.lock().unwrap().push(since);
            Ok(vec![])
        }
    }

    /// Builds a `PaymentService` with a given `reconciliation_lookback_hours`
    /// and a `RecordingRazorpayClient`, returning the shared handle used to
    /// inspect what `since` `reconcile_once` actually passed (Phase 4K.4).
    fn service_with_lookback(
        lookback_hours: i64,
    ) -> (PaymentService, Arc<Mutex<Vec<chrono::DateTime<Utc>>>>) {
        let since_calls = Arc::new(Mutex::new(Vec::new()));
        let payment_repository: Arc<dyn PaymentRepository> =
            Arc::new(MockPaymentRepository::with(vec![]));
        let subscription_repository = Arc::new(MockSubscriptionRepository::with(vec![]));
        let license_repository = Arc::new(MockLicenseRepository::with(vec![]));
        let webhook_event_repository = Arc::new(MockWebhookEventRepository::new(
            payment_repository.clone(),
            subscription_repository.clone(),
            license_repository.clone(),
        ));
        let service = PaymentService::new(
            payment_repository,
            webhook_event_repository,
            subscription_repository,
            license_repository,
            Arc::new(RecordingRazorpayClient {
                since_calls: since_calls.clone(),
            }),
            lookback_hours,
        );
        (service, since_calls)
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
            gateway_payment_id: None,
            status,
            created_at: Utc::now(),
        }
    }

    /// Like `sample_payment`, but with `gateway_payment_id` set — the
    /// reference refund/dispute correlation reads (Phase 4K.2).
    /// `provider_ref` is set to an arbitrary, distinct value: refund/
    /// dispute resolution never reads it, so a real test never needs it to
    /// coincide with `gateway_payment_id`.
    fn sample_activated_payment(
        id: i64,
        subscription_id: i64,
        gateway_payment_id: &str,
        status: PaymentStatus,
    ) -> Payment {
        Payment {
            gateway_payment_id: Some(gateway_payment_id.to_string()),
            ..sample_payment(id, subscription_id, "sub_checkout_ref", status)
        }
    }

    fn sample_license(id: i64, subscription_id: i64, status: LicenseRecordStatus) -> License {
        License {
            id,
            subscription_id,
            license_key: format!("SAMP-LEKY-{id:04}-0000"),
            status,
            expires_at: Some(Utc::now() + Duration::days(30)),
            max_devices: 1,
            grace_period_days: 7,
            issued_at: Utc::now() - Duration::days(1),
            revoked_at: None,
            revoked_reason: None,
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
        let payment_repository: Arc<dyn PaymentRepository> =
            Arc::new(MockPaymentRepository::with(payments));
        let subscription_repository = Arc::new(MockSubscriptionRepository::with(subscriptions));
        let license_repository = Arc::new(MockLicenseRepository::with(licenses));
        let webhook_event_repository = Arc::new(MockWebhookEventRepository::new(
            payment_repository.clone(),
            subscription_repository.clone(),
            license_repository.clone(),
        ));
        let service = PaymentService::new(
            payment_repository,
            webhook_event_repository,
            subscription_repository.clone(),
            license_repository.clone(),
            Arc::new(MockRazorpayClient {
                checkout_result: razorpay_result,
                list_result: vec![],
            }),
            2, // matches config::ReconciliationConfig's default max_age_hours
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
        let webhook_event_repository = Arc::new(MockWebhookEventRepository::new(
            payment_repository.clone(),
            subscription_repository.clone(),
            license_repository.clone(),
        ));
        let service = PaymentService::new(
            payment_repository.clone(),
            webhook_event_repository,
            subscription_repository.clone(),
            license_repository.clone(),
            Arc::new(MockRazorpayClient {
                checkout_result: Err(RazorpayError::NotConfigured(
                    "not exercised by reconciliation tests".to_string(),
                )),
                list_result: razorpay_payments,
            }),
            2, // matches config::ReconciliationConfig's default max_age_hours
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

    // ── Production Hardening, Finding C2: captured amount/currency ───────
    //
    // `sample_payment` stores `amount_minor: 499_900, currency: "INR"` —
    // every test below matches or deliberately diverges from exactly that.

    #[tokio::test]
    async fn payment_captured_activates_when_the_captured_amount_and_currency_match() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let (service, subscriptions, licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "payment.captured".to_string(),
            payload: json!({
                "payment": {
                    "entity": {
                        "id": "pay_xyz",
                        "order_id": "order_abc",
                        "amount": 499_900,
                        "currency": "INR"
                    }
                }
            }),
        };
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(subscription.status, SubscriptionStatus::Active);
        let license = licenses.find_latest_by_subscription(10).await.unwrap();
        assert_eq!(license.unwrap().status, LicenseRecordStatus::Active);
    }

    #[tokio::test]
    async fn payment_captured_with_no_amount_or_currency_in_the_payload_still_activates() {
        // Lenient on absence, same treatment `extract_entity_id`'s own
        // missing case already gets — nothing to verify against isn't
        // itself a mismatch. Also proves every pre-C2 test payload (none
        // of which carry `amount`/`currency`) keeps working unchanged.
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
        assert_eq!(license.unwrap().status, LicenseRecordStatus::Active);
    }

    #[tokio::test]
    async fn payment_captured_does_not_activate_on_an_amount_mismatch() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let (service, subscriptions, licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "payment.captured".to_string(),
            payload: json!({
                "payment": {
                    "entity": {
                        "id": "pay_xyz",
                        "order_id": "order_abc",
                        "amount": 1,
                        "currency": "INR"
                    }
                }
            }),
        };
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(
            subscription.status,
            SubscriptionStatus::PendingPayment,
            "a captured-amount mismatch must never activate the subscription"
        );
        assert!(
            licenses
                .find_latest_by_subscription(10)
                .await
                .unwrap()
                .is_none(),
            "a captured-amount mismatch must never issue a license"
        );
    }

    #[tokio::test]
    async fn payment_captured_does_not_activate_on_a_currency_mismatch() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let (service, subscriptions, licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "payment.captured".to_string(),
            payload: json!({
                "payment": {
                    "entity": {
                        "id": "pay_xyz",
                        "order_id": "order_abc",
                        "amount": 499_900,
                        "currency": "USD"
                    }
                }
            }),
        };
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(
            subscription.status,
            SubscriptionStatus::PendingPayment,
            "a captured-currency mismatch must never activate the subscription"
        );
        assert!(
            licenses
                .find_latest_by_subscription(10)
                .await
                .unwrap()
                .is_none(),
            "a captured-currency mismatch must never issue a license"
        );
    }

    #[tokio::test]
    async fn payment_captured_does_not_activate_when_both_amount_and_currency_mismatch() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let (service, subscriptions, licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "payment.captured".to_string(),
            payload: json!({
                "payment": {
                    "entity": {
                        "id": "pay_xyz",
                        "order_id": "order_abc",
                        "amount": 1,
                        "currency": "USD"
                    }
                }
            }),
        };
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(subscription.status, SubscriptionStatus::PendingPayment);
        assert!(licenses
            .find_latest_by_subscription(10)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn a_mismatched_amount_webhook_replay_never_activates_either_time() {
        // Preserves idempotency: `claim_and_apply` still claims the event
        // (so a genuine redelivery is recognized and never double-
        // processed) even though the mutation it applied was `None` — the
        // exact same mechanism every other "can't safely proceed" webhook
        // case already relies on, not something new C2 had to add.
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let (service, subscriptions, licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "payment.captured".to_string(),
            payload: json!({
                "payment": {
                    "entity": {
                        "id": "pay_xyz",
                        "order_id": "order_abc",
                        "amount": 1,
                        "currency": "INR"
                    }
                }
            }),
        };
        service
            .process_webhook_event("evt_1", payload.clone())
            .await
            .unwrap();
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(subscription.status, SubscriptionStatus::PendingPayment);
        assert!(licenses
            .find_latest_by_subscription(10)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn payment_link_paid_does_not_activate_on_an_amount_mismatch() {
        // Proves the check applies uniformly across all three activation
        // paths sharing `resolve_activation`, not only `payment.captured`.
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "plink_abc", PaymentStatus::Pending);
        let (service, subscriptions, licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "payment_link.paid".to_string(),
            payload: json!({
                "payment_link": { "entity": { "id": "plink_abc" } },
                "payment": {
                    "entity": {
                        "id": "pay_xyz",
                        "amount": 1,
                        "currency": "INR"
                    }
                }
            }),
        };
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(subscription.status, SubscriptionStatus::PendingPayment);
        assert!(licenses
            .find_latest_by_subscription(10)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn subscription_charged_does_not_activate_on_a_currency_mismatch() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "sub_abc", PaymentStatus::Pending);
        let (service, subscriptions, licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "subscription.charged".to_string(),
            payload: json!({
                "subscription": { "entity": { "id": "sub_abc" } },
                "payment": {
                    "entity": {
                        "id": "pay_xyz",
                        "amount": 499_900,
                        "currency": "USD"
                    }
                }
            }),
        };
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(subscription.status, SubscriptionStatus::PendingPayment);
        assert!(licenses
            .find_latest_by_subscription(10)
            .await
            .unwrap()
            .is_none());
    }

    // ── Production Hardening, Finding H2: duplicate provider_ref ─────────
    //
    // Two `sample_payment` rows deliberately sharing the same
    // `provider_ref` — impossible to arrange against the real,
    // migration-`0008`-constrained schema, but exactly what
    // `MockPaymentRepository::find_by_provider_ref` now faithfully
    // rejects, the same way the real repository would have before that
    // migration ever ran.

    #[tokio::test]
    async fn payment_captured_does_not_activate_when_the_provider_ref_is_duplicated() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment_a = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let payment_b = sample_payment(2, 10, "order_abc", PaymentStatus::Pending);
        let (service, subscriptions, licenses) = service_with(
            vec![payment_a, payment_b],
            vec![subscription],
            vec![],
            ok_checkout(),
        );

        let payload = RazorpayWebhookPayload {
            event: "payment.captured".to_string(),
            payload: json!({ "payment": { "entity": { "id": "pay_xyz", "order_id": "order_abc" } } }),
        };
        let result = service.process_webhook_event("evt_1", payload).await;
        assert!(
            result.is_ok(),
            "a duplicate provider_ref must not surface as a webhook processing error"
        );

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(
            subscription.status,
            SubscriptionStatus::PendingPayment,
            "a duplicate provider_ref must never activate a subscription"
        );
        assert!(
            licenses
                .find_latest_by_subscription(10)
                .await
                .unwrap()
                .is_none(),
            "a duplicate provider_ref must never issue a license"
        );
    }

    #[tokio::test]
    async fn payment_failed_does_not_mutate_when_the_provider_ref_is_duplicated() {
        let payment_a = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let payment_b = sample_payment(2, 10, "order_abc", PaymentStatus::Pending);
        let (service, _subscriptions, _licenses) =
            service_with(vec![payment_a, payment_b], vec![], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "payment.failed".to_string(),
            payload: json!({ "payment": { "entity": { "id": "pay_xyz", "order_id": "order_abc" } } }),
        };
        let result = service.process_webhook_event("evt_1", payload).await;
        assert!(result.is_ok());
        // Both rows must still be exactly `Pending` — neither was guessed
        // at and flipped to `Failed`.
    }

    #[tokio::test]
    async fn subscription_cancelled_does_not_mutate_when_the_provider_ref_is_duplicated() {
        let subscription = sample_subscription(10, SubscriptionStatus::Active);
        let payment_a = sample_payment(1, 10, "sub_abc", PaymentStatus::Succeeded);
        let payment_b = sample_payment(2, 10, "sub_abc", PaymentStatus::Succeeded);
        let (service, subscriptions, _licenses) = service_with(
            vec![payment_a, payment_b],
            vec![subscription],
            vec![],
            ok_checkout(),
        );

        let payload = RazorpayWebhookPayload {
            event: "subscription.cancelled".to_string(),
            payload: json!({ "subscription": { "entity": { "id": "sub_abc" } } }),
        };
        let result = service.process_webhook_event("evt_1", payload).await;
        assert!(result.is_ok());

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(
            subscription.status,
            SubscriptionStatus::Active,
            "a duplicate provider_ref must never transition the subscription"
        );
    }

    #[tokio::test]
    async fn reconcile_once_skips_a_payment_whose_provider_ref_is_duplicated_locally() {
        // `reconcile_one` propagates `DuplicateProviderReference` via `?`;
        // `reconcile_once`'s own per-item catch (already relied on so one
        // bad payment never aborts the whole run) is what keeps this safe
        // — logged and skipped, retried next run, never crashing the batch
        // or guessing which local row to heal.
        let payment_a = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let payment_b = sample_payment(2, 10, "order_abc", PaymentStatus::Pending);
        let razorpay_payment = RazorpayPayment {
            id: "pay_xyz".to_string(),
            order_id: Some("order_abc".to_string()),
            status: "captured".to_string(),
        };
        let (service, _payments, _subscriptions, _licenses) = service_with_reconciliation(
            vec![payment_a, payment_b],
            vec![],
            vec![],
            vec![razorpay_payment],
        );

        let summary = service.reconcile_once().await.unwrap();
        assert_eq!(summary.checked, 1);
        assert_eq!(
            summary.healed, 0,
            "a duplicate provider_ref must never be reported as healed"
        );
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

    /// Phase 4J.1 regression test — two *concurrent* calls for the same
    /// event, not two sequential ones (the case above). Proves the claim
    /// is what gates the mutation: `MockWebhookEventRepository::
    /// claim_and_apply` only ever pushes a license once its own `Mutex`-
    /// guarded claim succeeds, so no interleaving of these two spawned
    /// tasks can result in two licenses. The real race this guards against
    /// — two genuinely separate database connections/transactions racing
    /// on `INSERT ... ON CONFLICT DO NOTHING` — is additionally proven
    /// against a real Postgres by
    /// `tests/payment_flow.rs`'s
    /// `concurrent_duplicate_webhook_deliveries_apply_the_mutation_exactly_once`.
    #[tokio::test]
    async fn concurrent_duplicate_webhook_deliveries_apply_the_mutation_exactly_once() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Pending);
        let (service, subscriptions, licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());
        let service = Arc::new(service);

        let payload = RazorpayWebhookPayload {
            event: "payment.captured".to_string(),
            payload: json!({ "payment": { "entity": { "id": "pay_xyz", "order_id": "order_abc" } } }),
        };

        let service_a = Arc::clone(&service);
        let payload_a = payload.clone();
        let task_a =
            tokio::spawn(
                async move { service_a.process_webhook_event("evt_race", payload_a).await },
            );

        let service_b = Arc::clone(&service);
        let task_b =
            tokio::spawn(async move { service_b.process_webhook_event("evt_race", payload).await });

        let (result_a, result_b) = tokio::join!(task_a, task_b);
        result_a.unwrap().unwrap();
        result_b.unwrap().unwrap();

        let license_count = licenses
            .licenses
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.subscription_id == 10)
            .count();
        assert_eq!(
            license_count, 1,
            "two concurrent deliveries of the same event must issue exactly one license, not two"
        );

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(subscription.status, SubscriptionStatus::Active);
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

    /// Phase 4L.3 (production validation, CRITICAL): Razorpay can redeliver
    /// or reorder webhooks — a `payment.failed` arriving *after* the same
    /// payment already succeeded (network reordering, a retried delivery)
    /// must not overwrite `payments.status` back to `failed`. `event_id`
    /// idempotency alone doesn't catch this: a genuine redelivery carries a
    /// different `event_id` than the original `payment.captured`.
    #[tokio::test]
    async fn payment_failed_arriving_after_the_payment_already_succeeded_is_ignored() {
        let subscription = sample_subscription(10, SubscriptionStatus::Active);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Succeeded);
        let (service, payments, _subscriptions, _licenses) =
            service_with_reconciliation(vec![payment], vec![subscription], vec![], vec![]);

        let payload = RazorpayWebhookPayload {
            event: "payment.failed".to_string(),
            payload: json!({ "payment": { "entity": { "id": "pay_xyz", "order_id": "order_abc" } } }),
        };
        service
            .process_webhook_event("evt_late_failed", payload)
            .await
            .unwrap();

        assert_eq!(
            payments.payments.lock().unwrap()[0].status,
            PaymentStatus::Succeeded,
            "an out-of-order payment.failed must not downgrade an already-succeeded payment"
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

    // ── payment_link.paid (Phase 4K.1 regression coverage) ──────────────

    /// Reproduces the exact bug fixed in Phase 4K.1 side-by-side with its
    /// fix: a `payment.captured` webhook for a Payment-Link-originated
    /// payment carries only the underlying payment/order id (never stored
    /// anywhere), so it can never match; the `payment_link.paid` webhook
    /// for the *same real-world payment* carries the Payment Link id that
    /// actually was stored at checkout, and does match.
    #[tokio::test]
    async fn payment_captured_for_a_payment_link_purchase_does_not_match_while_payment_link_paid_does(
    ) {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        // Stored at checkout time for a `lifetime`/`trial` purchase —
        // the Payment Link's own id, per `client.rs`'s `create_checkout`.
        let payment = sample_payment(1, 10, "plink_abc123", PaymentStatus::Pending);
        let (service, subscriptions, licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        // What Razorpay actually sends first: `payment.captured`, whose
        // payload never carries the Payment Link id — only the freshly
        // generated payment/order id.
        let captured_payload = RazorpayWebhookPayload {
            event: "payment.captured".to_string(),
            payload: json!({
                "payment": { "entity": { "id": "pay_freshly_generated", "order_id": "order_freshly_generated" } }
            }),
        };
        service
            .process_webhook_event("evt_captured", captured_payload)
            .await
            .unwrap();

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(
            subscription.status,
            SubscriptionStatus::PendingPayment,
            "payment.captured alone must not activate a Payment-Link purchase \
             (its payload never carries the stored Payment Link id)"
        );
        assert!(licenses
            .find_latest_by_subscription(10)
            .await
            .unwrap()
            .is_none());

        // What actually correlates: `payment_link.paid`, whose payload
        // carries `payment_link.entity.id` — the same id stored at
        // checkout.
        let paid_payload = RazorpayWebhookPayload {
            event: "payment_link.paid".to_string(),
            payload: json!({
                "payment_link": { "entity": { "id": "plink_abc123" } },
                "payment": { "entity": { "id": "pay_freshly_generated", "order_id": "order_freshly_generated" } }
            }),
        };
        service
            .process_webhook_event("evt_paid", paid_payload)
            .await
            .unwrap();

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(subscription.status, SubscriptionStatus::Active);
        let license = licenses.find_latest_by_subscription(10).await.unwrap();
        assert!(license.is_some(), "payment_link.paid must issue a license");
        assert_eq!(license.unwrap().status, LicenseRecordStatus::Active);
    }

    /// End-to-end round trip: `create_checkout_session` for a `lifetime`
    /// plan persists the Payment Link id Razorpay returned as
    /// `provider_ref`; the real `payment_link.paid` webhook Razorpay later
    /// sends for that same link must activate the subscription and issue
    /// a license using exactly that stored reference.
    #[tokio::test]
    async fn lifetime_checkout_followed_by_payment_link_paid_webhook_issues_a_license() {
        let (service, subscriptions, licenses) = service_with(
            vec![],
            vec![],
            vec![],
            Ok(CreateCheckoutResponse {
                checkout_url: "https://rzp.io/l/plink_abc123".to_string(),
                provider_ref: "plink_abc123".to_string(),
            }),
        );

        let outcome = service
            .create_checkout_session(1, "lifetime")
            .await
            .unwrap();
        assert_eq!(outcome.provider_ref, "plink_abc123");

        let subscription = subscriptions
            .subscriptions
            .lock()
            .unwrap()
            .first()
            .cloned()
            .expect("checkout must have created a subscription row");
        assert_eq!(subscription.status, SubscriptionStatus::PendingPayment);

        let payload = RazorpayWebhookPayload {
            event: "payment_link.paid".to_string(),
            payload: json!({
                "payment_link": { "entity": { "id": "plink_abc123" } },
                "payment": { "entity": { "id": "pay_xyz", "order_id": "order_xyz" } }
            }),
        };
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let subscription = subscriptions
            .find_by_id(subscription.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(subscription.status, SubscriptionStatus::Active);
        let license = licenses
            .find_latest_by_subscription(subscription.id)
            .await
            .unwrap();
        assert!(license.is_some());
    }

    #[tokio::test]
    async fn payment_link_paid_is_idempotent_for_a_repeated_event_id() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "plink_abc123", PaymentStatus::Pending);
        let (service, _subscriptions, licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "payment_link.paid".to_string(),
            payload: json!({ "payment_link": { "entity": { "id": "plink_abc123" } } }),
        };
        service
            .process_webhook_event("evt_1", payload.clone())
            .await
            .unwrap();
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let license_count = licenses
            .licenses
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.subscription_id == 10)
            .count();
        assert_eq!(
            license_count, 1,
            "a repeated event_id must not issue a second license"
        );
    }

    /// Phase 4K.1 concurrency regression, mirroring
    /// `concurrent_duplicate_webhook_deliveries_apply_the_mutation_exactly_once`
    /// for the new `payment_link.paid` path: two concurrent deliveries of
    /// the same event must still issue exactly one license.
    #[tokio::test]
    async fn concurrent_payment_link_paid_deliveries_issue_the_mutation_exactly_once() {
        let subscription = sample_subscription(10, SubscriptionStatus::PendingPayment);
        let payment = sample_payment(1, 10, "plink_abc123", PaymentStatus::Pending);
        let (service, subscriptions, licenses) =
            service_with(vec![payment], vec![subscription], vec![], ok_checkout());
        let service = Arc::new(service);

        let payload = RazorpayWebhookPayload {
            event: "payment_link.paid".to_string(),
            payload: json!({ "payment_link": { "entity": { "id": "plink_abc123" } } }),
        };

        let service_a = Arc::clone(&service);
        let payload_a = payload.clone();
        let task_a =
            tokio::spawn(
                async move { service_a.process_webhook_event("evt_race", payload_a).await },
            );

        let service_b = Arc::clone(&service);
        let task_b =
            tokio::spawn(async move { service_b.process_webhook_event("evt_race", payload).await });

        let (result_a, result_b) = tokio::join!(task_a, task_b);
        result_a.unwrap().unwrap();
        result_b.unwrap().unwrap();

        let license_count = licenses
            .licenses
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.subscription_id == 10)
            .count();
        assert_eq!(
            license_count, 1,
            "two concurrent deliveries of the same event must issue exactly one license, not two"
        );

        let subscription = subscriptions.find_by_id(10).await.unwrap().unwrap();
        assert_eq!(subscription.status, SubscriptionStatus::Active);
    }

    #[tokio::test]
    async fn payment_link_paid_missing_the_entity_reference_is_acknowledged_not_errored() {
        let (service, ..) = service_with(vec![], vec![], vec![], ok_checkout());

        // No `payment_link` key at all in the payload — e.g. a malformed
        // or unexpected delivery shape.
        let payload = RazorpayWebhookPayload {
            event: "payment_link.paid".to_string(),
            payload: json!({ "payment": { "entity": { "id": "pay_xyz" } } }),
        };
        let result = service.process_webhook_event("evt_1", payload).await;
        assert!(
            result.is_ok(),
            "a missing provider reference must not fail the webhook call"
        );
    }

    #[tokio::test]
    async fn payment_link_paid_with_a_non_string_id_is_treated_as_missing() {
        let (service, ..) = service_with(vec![], vec![], vec![], ok_checkout());

        // `id` present but the wrong JSON type — `extract_entity_ref`
        // requires a string and must not panic or coerce it.
        let payload = RazorpayWebhookPayload {
            event: "payment_link.paid".to_string(),
            payload: json!({ "payment_link": { "entity": { "id": 12345 } } }),
        };
        let result = service.process_webhook_event("evt_1", payload).await;
        assert!(
            result.is_ok(),
            "an invalid reference must not fail the webhook call"
        );
    }

    #[tokio::test]
    async fn payment_link_paid_referencing_an_unknown_payment_is_acknowledged_not_errored() {
        let (service, ..) = service_with(vec![], vec![], vec![], ok_checkout());

        let payload = RazorpayWebhookPayload {
            event: "payment_link.paid".to_string(),
            payload: json!({ "payment_link": { "entity": { "id": "plink_never_seen" } } }),
        };
        let result = service.process_webhook_event("evt_1", payload).await;
        assert!(
            result.is_ok(),
            "an unmatched reference must not fail the webhook call"
        );
    }

    #[tokio::test]
    async fn subscription_cancelled_with_no_license_issued_yet_only_updates_the_subscription() {
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
    async fn subscription_halted_with_no_license_issued_yet_marks_the_subscription_suspended_not_cancelled(
    ) {
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

    // ── Production Hardening, Finding #9: subscription→license consistency ──

    #[tokio::test]
    async fn subscription_cancelled_immediately_revokes_its_active_license() {
        let subscription = sample_subscription(10, SubscriptionStatus::Active);
        let payment = sample_payment(1, 10, "sub_rzp_xyz", PaymentStatus::Succeeded);
        let license = sample_license(50, 10, LicenseRecordStatus::Active);
        let (service, subscriptions, licenses) = service_with(
            vec![payment],
            vec![subscription],
            vec![license],
            ok_checkout(),
        );

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
        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(license.status, LicenseRecordStatus::Revoked);
    }

    #[tokio::test]
    async fn subscription_halted_immediately_suspends_its_active_license() {
        let subscription = sample_subscription(10, SubscriptionStatus::Active);
        let payment = sample_payment(1, 10, "sub_rzp_xyz", PaymentStatus::Succeeded);
        let license = sample_license(50, 10, LicenseRecordStatus::Active);
        let (service, subscriptions, licenses) = service_with(
            vec![payment],
            vec![subscription],
            vec![license],
            ok_checkout(),
        );

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
        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(license.status, LicenseRecordStatus::Suspended);
    }

    #[tokio::test]
    async fn subscription_cancelled_does_not_revive_a_license_already_revoked_by_an_earlier_refund()
    {
        // `find_latest_by_subscription` excludes `revoked` licenses, so a
        // `subscription.cancelled` arriving after an earlier `refund.created`
        // already revoked the license must find nothing to touch, not
        // "resurrect" it back to some other status.
        let subscription = sample_subscription(10, SubscriptionStatus::Active);
        let payment = sample_payment(1, 10, "sub_rzp_xyz", PaymentStatus::Refunded);
        let license = sample_license(50, 10, LicenseRecordStatus::Revoked);
        let (service, _subscriptions, licenses) = service_with(
            vec![payment],
            vec![subscription],
            vec![license],
            ok_checkout(),
        );

        let payload = RazorpayWebhookPayload {
            event: "subscription.cancelled".to_string(),
            payload: json!({ "subscription": { "entity": { "id": "sub_rzp_xyz" } } }),
        };
        service
            .process_webhook_event("evt_1", payload)
            .await
            .unwrap();

        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(license.status, LicenseRecordStatus::Revoked);
    }

    #[tokio::test]
    async fn subscription_halted_redelivered_is_idempotent_for_an_already_suspended_license() {
        let subscription = sample_subscription(10, SubscriptionStatus::Suspended);
        let payment = sample_payment(1, 10, "sub_rzp_xyz", PaymentStatus::Succeeded);
        let license = sample_license(50, 10, LicenseRecordStatus::Suspended);
        let (service, _subscriptions, licenses) = service_with(
            vec![payment],
            vec![subscription],
            vec![license],
            ok_checkout(),
        );

        let payload = RazorpayWebhookPayload {
            event: "subscription.halted".to_string(),
            payload: json!({ "subscription": { "entity": { "id": "sub_rzp_xyz" } } }),
        };
        service
            .process_webhook_event("evt_1", payload.clone())
            .await
            .unwrap();
        service
            .process_webhook_event("evt_2", payload)
            .await
            .unwrap();

        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(license.status, LicenseRecordStatus::Suspended);
    }

    // ── refund.*/payment.dispute.* (Phase 4K.2) ─────────────────────────

    fn refund_payload(event: &str, event_payment_id: &str) -> RazorpayWebhookPayload {
        RazorpayWebhookPayload {
            event: event.to_string(),
            payload: json!({
                "refund": { "entity": { "id": "rfnd_1", "payment_id": event_payment_id } },
                "payment": { "entity": { "id": event_payment_id, "order_id": "order_xyz" } }
            }),
        }
    }

    fn dispute_created_payload(event_payment_id: &str) -> RazorpayWebhookPayload {
        RazorpayWebhookPayload {
            event: "payment.dispute.created".to_string(),
            payload: json!({
                "dispute": { "entity": { "id": "disp_1", "payment_id": event_payment_id, "status": "open" } },
                "payment": { "entity": { "id": event_payment_id, "order_id": "order_xyz" } }
            }),
        }
    }

    fn dispute_closed_payload(event_payment_id: &str, outcome: &str) -> RazorpayWebhookPayload {
        RazorpayWebhookPayload {
            event: "payment.dispute.closed".to_string(),
            payload: json!({
                "dispute": { "entity": { "id": "disp_1", "payment_id": event_payment_id, "status": outcome } },
                "payment": { "entity": { "id": event_payment_id, "order_id": "order_xyz" } }
            }),
        }
    }

    #[tokio::test]
    async fn refund_created_marks_the_payment_refunded_and_revokes_the_license() {
        let payment = sample_activated_payment(1, 10, "pay_xyz", PaymentStatus::Succeeded);
        let license = sample_license(50, 10, LicenseRecordStatus::Active);
        let (service, payments, _subscriptions, licenses) =
            service_with_reconciliation(vec![payment], vec![], vec![license], vec![]);

        service
            .process_webhook_event(
                "evt_refund_created",
                refund_payload("refund.created", "pay_xyz"),
            )
            .await
            .unwrap();

        assert_eq!(
            payments.payments.lock().unwrap()[0].status,
            PaymentStatus::Refunded
        );
        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(license.status, LicenseRecordStatus::Revoked);
    }

    #[tokio::test]
    async fn refund_processed_marks_the_payment_refunded_and_revokes_the_license() {
        let payment = sample_activated_payment(1, 10, "pay_xyz", PaymentStatus::Succeeded);
        let license = sample_license(50, 10, LicenseRecordStatus::Active);
        let (service, payments, _subscriptions, licenses) =
            service_with_reconciliation(vec![payment], vec![], vec![license], vec![]);

        service
            .process_webhook_event(
                "evt_refund_processed",
                refund_payload("refund.processed", "pay_xyz"),
            )
            .await
            .unwrap();

        assert_eq!(
            payments.payments.lock().unwrap()[0].status,
            PaymentStatus::Refunded
        );
        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(license.status, LicenseRecordStatus::Revoked);
    }

    #[tokio::test]
    async fn a_duplicate_refund_event_id_does_not_double_revoke_or_corrupt_status() {
        let payment = sample_activated_payment(1, 10, "pay_xyz", PaymentStatus::Succeeded);
        let license = sample_license(50, 10, LicenseRecordStatus::Active);
        let (service, payments, _subscriptions, licenses) =
            service_with_reconciliation(vec![payment], vec![], vec![license], vec![]);

        let payload = refund_payload("refund.created", "pay_xyz");
        service
            .process_webhook_event("evt_refund", payload.clone())
            .await
            .unwrap();
        service
            .process_webhook_event("evt_refund", payload)
            .await
            .unwrap();

        assert_eq!(
            payments.payments.lock().unwrap()[0].status,
            PaymentStatus::Refunded,
            "status must not be corrupted by a repeated delivery"
        );
        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(license.status, LicenseRecordStatus::Revoked);
    }

    /// Phase 4L.3 (production validation, CRITICAL): a `payment.captured`/
    /// `payment_link.paid`/`subscription.activated`/`.charged` redelivery
    /// arriving *after* `refund.created` already ran must not silently
    /// revive the refunded payment or the revoked license. `event_id`
    /// idempotency doesn't catch this — a genuine Razorpay redelivery of
    /// the original capture carries a *different* event_id than the
    /// refund, so `claim_and_apply` sees it as a brand-new event to apply.
    #[tokio::test]
    async fn payment_captured_redelivered_after_a_refund_does_not_revive_the_payment_or_license() {
        let subscription = sample_subscription(10, SubscriptionStatus::Active);
        let payment = sample_payment(1, 10, "order_abc", PaymentStatus::Refunded);
        let license = sample_license(50, 10, LicenseRecordStatus::Revoked);
        let (service, payments, _subscriptions, licenses) =
            service_with_reconciliation(vec![payment], vec![subscription], vec![license], vec![]);

        let payload = RazorpayWebhookPayload {
            event: "payment.captured".to_string(),
            payload: json!({ "payment": { "entity": { "id": "pay_xyz", "order_id": "order_abc" } } }),
        };
        service
            .process_webhook_event("evt_redelivered_capture", payload)
            .await
            .unwrap();

        assert_eq!(
            payments.payments.lock().unwrap()[0].status,
            PaymentStatus::Refunded,
            "a redelivered capture must not revive an already-refunded payment"
        );
        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(
            license.status,
            LicenseRecordStatus::Revoked,
            "the license must stay revoked"
        );
    }

    /// Same guard, from the `Disputed` side: a redelivered activation must
    /// not silently clear an open dispute — only `resolve_dispute_closed`
    /// (a real merchant-won resolution) may do that.
    #[tokio::test]
    async fn subscription_charged_redelivered_during_an_open_dispute_does_not_clear_it() {
        let subscription = sample_subscription(10, SubscriptionStatus::Active);
        let payment = sample_payment(1, 10, "sub_abc", PaymentStatus::Disputed);
        let license = sample_license(50, 10, LicenseRecordStatus::Suspended);
        let (service, payments, _subscriptions, licenses) =
            service_with_reconciliation(vec![payment], vec![subscription], vec![license], vec![]);

        let payload = RazorpayWebhookPayload {
            event: "subscription.charged".to_string(),
            payload: json!({
                "subscription": { "entity": { "id": "sub_abc" } },
                "payment": { "entity": { "id": "pay_xyz" } }
            }),
        };
        service
            .process_webhook_event("evt_redelivered_charge", payload)
            .await
            .unwrap();

        assert_eq!(
            payments.payments.lock().unwrap()[0].status,
            PaymentStatus::Disputed,
            "a redelivered activation must not clear an open dispute"
        );
        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(
            license.status,
            LicenseRecordStatus::Suspended,
            "the license must stay suspended while the dispute is open"
        );
    }

    #[tokio::test]
    async fn dispute_created_marks_the_payment_disputed_and_suspends_the_license() {
        let payment = sample_activated_payment(1, 10, "pay_xyz", PaymentStatus::Succeeded);
        let license = sample_license(50, 10, LicenseRecordStatus::Active);
        let (service, payments, _subscriptions, licenses) =
            service_with_reconciliation(vec![payment], vec![], vec![license], vec![]);

        service
            .process_webhook_event("evt_dispute_created", dispute_created_payload("pay_xyz"))
            .await
            .unwrap();

        assert_eq!(
            payments.payments.lock().unwrap()[0].status,
            PaymentStatus::Disputed
        );
        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(license.status, LicenseRecordStatus::Suspended);
    }

    #[tokio::test]
    async fn dispute_closed_with_merchant_won_restores_the_payment_and_license() {
        let payment = sample_activated_payment(1, 10, "pay_xyz", PaymentStatus::Disputed);
        let license = sample_license(50, 10, LicenseRecordStatus::Suspended);
        let expected_expiry = license.expires_at;
        let (service, payments, _subscriptions, licenses) =
            service_with_reconciliation(vec![payment], vec![], vec![license], vec![]);

        service
            .process_webhook_event(
                "evt_dispute_closed",
                dispute_closed_payload("pay_xyz", "won"),
            )
            .await
            .unwrap();

        assert_eq!(
            payments.payments.lock().unwrap()[0].status,
            PaymentStatus::Succeeded,
            "merchant winning must restore the payment, not leave it disputed"
        );
        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(license.status, LicenseRecordStatus::Active);
        assert_eq!(
            license.expires_at, expected_expiry,
            "restoring must not alter the license's original expiry"
        );
    }

    #[tokio::test]
    async fn dispute_closed_with_customer_won_refunds_the_payment_and_keeps_the_license_revoked() {
        let payment = sample_activated_payment(1, 10, "pay_xyz", PaymentStatus::Disputed);
        let license = sample_license(50, 10, LicenseRecordStatus::Suspended);
        let (service, payments, _subscriptions, licenses) =
            service_with_reconciliation(vec![payment], vec![], vec![license], vec![]);

        service
            .process_webhook_event(
                "evt_dispute_closed",
                dispute_closed_payload("pay_xyz", "lost"),
            )
            .await
            .unwrap();

        assert_eq!(
            payments.payments.lock().unwrap()[0].status,
            PaymentStatus::Refunded,
            "customer winning a chargeback means money left the merchant"
        );
        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(
            license.status,
            LicenseRecordStatus::Revoked,
            "customer winning must keep the license revoked, not merely suspended"
        );
    }

    #[tokio::test]
    async fn dispute_closed_with_an_unrecognized_status_is_acknowledged_without_mutating_anything()
    {
        let payment = sample_activated_payment(1, 10, "pay_xyz", PaymentStatus::Disputed);
        let license = sample_license(50, 10, LicenseRecordStatus::Suspended);
        let (service, payments, _subscriptions, licenses) =
            service_with_reconciliation(vec![payment], vec![], vec![license], vec![]);

        let result = service
            .process_webhook_event(
                "evt_dispute_closed",
                dispute_closed_payload("pay_xyz", "under_review"),
            )
            .await;

        assert!(result.is_ok(), "an unrecognized status must not error");
        assert_eq!(
            payments.payments.lock().unwrap()[0].status,
            PaymentStatus::Disputed,
            "must not guess at an outcome from a non-terminal status"
        );
        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(license.status, LicenseRecordStatus::Suspended);
    }

    #[tokio::test]
    async fn a_duplicate_dispute_created_event_id_does_not_double_suspend_or_corrupt_status() {
        let payment = sample_activated_payment(1, 10, "pay_xyz", PaymentStatus::Succeeded);
        let license = sample_license(50, 10, LicenseRecordStatus::Active);
        let (service, payments, _subscriptions, licenses) =
            service_with_reconciliation(vec![payment], vec![], vec![license], vec![]);

        let payload = dispute_created_payload("pay_xyz");
        service
            .process_webhook_event("evt_dispute", payload.clone())
            .await
            .unwrap();
        service
            .process_webhook_event("evt_dispute", payload)
            .await
            .unwrap();

        assert_eq!(
            payments.payments.lock().unwrap()[0].status,
            PaymentStatus::Disputed,
            "status must not be corrupted by a repeated delivery"
        );
        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(license.status, LicenseRecordStatus::Suspended);
    }

    #[tokio::test]
    async fn refund_referencing_an_unknown_payment_is_acknowledged_not_errored() {
        let (service, ..) = service_with(vec![], vec![], vec![], ok_checkout());

        let result = service
            .process_webhook_event("evt_1", refund_payload("refund.created", "pay_never_seen"))
            .await;

        assert!(
            result.is_ok(),
            "an unmatched gateway_payment_id must not fail the webhook call"
        );
    }

    #[tokio::test]
    async fn dispute_created_referencing_an_unknown_payment_is_acknowledged_not_errored() {
        let (service, ..) = service_with(vec![], vec![], vec![], ok_checkout());

        let result = service
            .process_webhook_event("evt_1", dispute_created_payload("pay_never_seen"))
            .await;

        assert!(
            result.is_ok(),
            "an unmatched gateway_payment_id must not fail the webhook call"
        );
    }

    #[tokio::test]
    async fn refund_missing_the_payment_reference_is_acknowledged_not_errored() {
        let (service, ..) = service_with(vec![], vec![], vec![], ok_checkout());

        // No `payment` entity at all in the payload.
        let payload = RazorpayWebhookPayload {
            event: "refund.created".to_string(),
            payload: json!({ "refund": { "entity": { "id": "rfnd_1" } } }),
        };
        let result = service.process_webhook_event("evt_1", payload).await;

        assert!(
            result.is_ok(),
            "a missing payment reference must not fail the webhook call"
        );
    }

    #[tokio::test]
    async fn refund_on_a_payment_with_no_active_license_only_updates_the_payment() {
        // No license row at all for this subscription — e.g. it was never
        // issued, or was already revoked by an earlier dispute/refund.
        let payment = sample_activated_payment(1, 10, "pay_xyz", PaymentStatus::Succeeded);
        let (service, payments, _subscriptions, _licenses) =
            service_with_reconciliation(vec![payment], vec![], vec![], vec![]);

        let result = service
            .process_webhook_event("evt_1", refund_payload("refund.created", "pay_xyz"))
            .await;

        assert!(
            result.is_ok(),
            "a payment with no active license must not error"
        );
        assert_eq!(
            payments.payments.lock().unwrap()[0].status,
            PaymentStatus::Refunded,
            "the payment itself must still be marked refunded"
        );
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

    /// Phase 4K.4: `reconcile_once` must derive `since` from
    /// `PaymentService`'s configured `reconciliation_lookback_hours`
    /// (`RECONCILIATION_MAX_AGE_HOURS`), not a hardcoded constant — proven
    /// here with a non-default value (6, not the old fixed `2`) so a
    /// regression back to the constant would fail this assertion.
    #[tokio::test]
    async fn reconcile_once_uses_the_configured_lookback_window() {
        let (service, since_calls) = service_with_lookback(6);

        let before = Utc::now();
        service.reconcile_once().await.unwrap();
        let after = Utc::now();

        let calls = since_calls.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "expected exactly one list_payments_since call"
        );
        let expected_earliest = before - Duration::hours(6);
        let expected_latest = after - Duration::hours(6);
        assert!(
            calls[0] >= expected_earliest && calls[0] <= expected_latest,
            "since ({:?}) was not derived from the configured 6-hour lookback \
             (expected between {expected_earliest:?} and {expected_latest:?})",
            calls[0],
        );
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

    /// Phase 4L.3 (production validation, HIGH): a lost `refund.created`/
    /// `.processed` webhook previously had no reconciliation backstop —
    /// `map_razorpay_payment_status` dropped `"refunded"` entirely, so a
    /// payment refunded at Razorpay while the webhook never arrived (or
    /// failed) stayed `Succeeded`, license `Active`, forever. Proves
    /// reconciliation now heals this the same way it already healed a
    /// lost `payment.captured`/`.failed`.
    #[tokio::test]
    async fn reconcile_once_heals_a_refund_no_webhook_ever_arrived_for() {
        let subscription = sample_subscription(10, SubscriptionStatus::Active);
        let payment = sample_activated_payment(1, 10, "pay_xyz", PaymentStatus::Succeeded);
        let license = sample_license(50, 10, LicenseRecordStatus::Active);
        let (service, payments, _subscriptions, licenses) = service_with_reconciliation(
            vec![payment],
            vec![subscription],
            vec![license],
            vec![razorpay_payment(
                "pay_xyz",
                Some("sub_checkout_ref"),
                "refunded",
            )],
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
        assert_eq!(stored_payments[0].status, PaymentStatus::Refunded);

        let license = licenses.find_by_id(50).await.unwrap().unwrap();
        assert_eq!(
            license.status,
            LicenseRecordStatus::Revoked,
            "reconciliation must have revoked the license, same as a real refund.created webhook would"
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
