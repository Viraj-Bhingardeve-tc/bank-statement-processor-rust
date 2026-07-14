//! `payment_webhook_events` table access — the idempotency ledger
//! `PHASE4_DESIGN.md` §4 step 2 checks before any webhook-triggered write.
//!
//! **Phase 4J.1 fix (production readiness audit, CRITICAL finding #1):**
//! the previous design checked `(provider, event_id)` for existence and
//! only inserted the ledger row *last*, after every other write — a
//! classic check-then-act race: two concurrent calls for the same event
//! (a live webhook delivery and a concurrent reconciliation pass
//! discovering the same payment, exactly the two triggers
//! `PHASE4_DESIGN.md` §12.1 designs to call the same processing path)
//! could both pass the "not found" check before either had written
//! anything, and both proceed to double-apply the event. `claim_and_apply`
//! below closes that race: the claim (`INSERT ... ON CONFLICT DO NOTHING`)
//! happens *first*, inside the same transaction as every mutation the
//! event implies — only one concurrent caller can ever win the claim, and
//! only the winner ever writes anything else.

use crate::domain::{
    LicenseRecordStatus, NewPaymentWebhookEvent, PaymentStatus, SubscriptionStatus,
};
use crate::repository::error::RepositoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// The database effect that should be applied — atomically, alongside the
/// idempotency claim — if a given call is the first to see this event.
/// Computed by `service::payment_service` from already-resolved local
/// state (a payment/subscription/license looked up by an immutable
/// provider reference) *before* attempting the claim: those lookups are
/// read-only and safe to run outside the transaction; only the writes
/// described here run inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookMutation {
    /// `payment.captured` / `payment_link.paid` / `subscription.activated`/`.charged`.
    ActivateSubscriptionAndLicense {
        payment_id: i64,
        subscription_id: i64,
        period_end: Option<DateTime<Utc>>,
        license: LicenseMutation,
        /// The real Razorpay payment id, when this activation's payload
        /// carried one (Phase 4K.2) — recorded on `payments` so a later
        /// refund/dispute webhook (which never carries `provider_ref`'s
        /// checkout-time reference) can still be correlated back to this
        /// row. `None` leaves any previously recorded value untouched
        /// (never clears a known id back to unknown).
        gateway_payment_id: Option<String>,
    },
    /// `payment.failed`.
    MarkPaymentFailed { payment_id: i64 },
    /// `subscription.cancelled`/`.halted`.
    UpdateSubscriptionStatus {
        subscription_id: i64,
        status: SubscriptionStatus,
        current_period_end: Option<DateTime<Utc>>,
    },
    /// `refund.created` / `refund.processed` (Phase 4K.2). Marks the
    /// payment refunded and, if an active-or-suspended license still
    /// exists for its subscription, revokes it — a refund is a terminal,
    /// non-reversible outcome, unlike a dispute's `Suspended` interim
    /// state.
    RefundPayment {
        payment_id: i64,
        license: Option<AffectedLicense>,
    },
    /// `payment.dispute.created` (Phase 4K.2). Marks the payment disputed
    /// and suspends the license, if one exists — reversible, pending
    /// `payment.dispute.closed`.
    MarkPaymentDisputed {
        payment_id: i64,
        license: Option<AffectedLicense>,
    },
    /// `payment.dispute.closed` (Phase 4K.2). `merchant_won` comes from
    /// Razorpay's own dispute outcome (`"won"` vs `"lost"`), never
    /// guessed: `true` restores the payment to `succeeded` and the
    /// license to `active`; `false` marks the payment `refunded` (money
    /// left the merchant) and leaves the license `revoked`.
    ResolveDispute {
        payment_id: i64,
        license: Option<AffectedLicense>,
        merchant_won: bool,
    },
    /// Unrecognized event type, a webhook missing a usable entity
    /// reference, or a reference to a payment/subscription this database
    /// has no record of (`PHASE4_DESIGN.md` §12.3's fail-closed, no-
    /// guessing posture) — the event is still claimed, so a later
    /// redelivery is recognized as a duplicate, but nothing else changes.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseMutation {
    /// Reuse an existing license row (a renewal).
    Extend { license_id: i64 },
    /// Issue a fresh license row (first successful payment on this
    /// subscription).
    Insert {
        license_key: String,
        max_devices: i32,
        grace_period_days: i32,
    },
}

/// A license affected by a refund/dispute mutation (Phase 4K.2) — carries
/// its current `expires_at` through untouched, since the underlying
/// `UPDATE licenses SET status = $2, expires_at = $3` statement (shared
/// with `LicenseRepository::extend`'s own query shape) requires it
/// explicitly, and a status-only transition must never clear a
/// still-valid expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffectedLicense {
    pub license_id: i64,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// This call claimed the event and applied `mutation`.
    Applied,
    /// Another call — concurrent or earlier — already claimed this event;
    /// this call applied nothing.
    AlreadyProcessed,
}

#[async_trait]
pub trait PaymentWebhookEventRepository: Send + Sync {
    /// Atomically, in one database transaction: (1) attempts
    /// `INSERT ... ON CONFLICT (provider, event_id) DO NOTHING` to claim
    /// the ledger row; (2) if no row was inserted (the event was already
    /// claimed by a concurrent or earlier call), rolls back and returns
    /// `AlreadyProcessed` — no mutation to `payments`/`subscriptions`/
    /// `licenses`; (3) otherwise applies `mutation` and commits, returning
    /// `Applied`. A crash or error between the claim and the commit rolls
    /// the whole transaction back, including the claim itself, so a
    /// genuine retry (webhook redelivery or a later reconciliation pass)
    /// still sees this event as unprocessed rather than being stuck
    /// half-applied forever.
    async fn claim_and_apply(
        &self,
        new_event: NewPaymentWebhookEvent,
        mutation: WebhookMutation,
    ) -> Result<ClaimOutcome, RepositoryError>;
}

pub struct PgPaymentWebhookEventRepository {
    pool: PgPool,
}

impl PgPaymentWebhookEventRepository {
    pub fn new(pool: PgPool) -> Self {
        PgPaymentWebhookEventRepository { pool }
    }
}

#[async_trait]
impl PaymentWebhookEventRepository for PgPaymentWebhookEventRepository {
    async fn claim_and_apply(
        &self,
        new_event: NewPaymentWebhookEvent,
        mutation: WebhookMutation,
    ) -> Result<ClaimOutcome, RepositoryError> {
        let mut tx = self.pool.begin().await?;

        // The claim — first statement in the transaction, exactly as
        // required: only one of any number of concurrent callers can ever
        // have this `INSERT` actually affect a row, since `(provider,
        // event_id)` is `UNIQUE` (migration `0002_create_payment_schema.sql`).
        let claim = sqlx::query(
            "INSERT INTO payment_webhook_events (provider, event_id, event_type, payload) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (provider, event_id) DO NOTHING",
        )
        .bind(&new_event.provider)
        .bind(&new_event.event_id)
        .bind(&new_event.event_type)
        .bind(&new_event.payload)
        .execute(&mut *tx)
        .await?;

        if claim.rows_affected() == 0 {
            // Nothing to undo, but explicit rather than relying on `tx`'s
            // Drop impl to roll back — this is the "return success
            // immediately without mutating any payment/license state"
            // path.
            tx.rollback().await?;
            return Ok(ClaimOutcome::AlreadyProcessed);
        }

        // Every statement below runs on the same connection/transaction as
        // the claim above, so a failure here rolls the claim back too —
        // this duplicates a handful of short statements already present in
        // `repository::payment`/`repository::subscription`/
        // `repository::license` (those repositories are separate trait
        // objects operating on their own pool reference, so they can't
        // participate in this transaction); kept intentionally small and
        // inline rather than threading a shared transaction type through
        // every repository trait in the crate.
        match mutation {
            WebhookMutation::None => {}
            WebhookMutation::MarkPaymentFailed { payment_id } => {
                sqlx::query("UPDATE payments SET status = $2 WHERE id = $1")
                    .bind(payment_id)
                    .bind(PaymentStatus::Failed.as_str())
                    .execute(&mut *tx)
                    .await?;
            }
            WebhookMutation::UpdateSubscriptionStatus {
                subscription_id,
                status,
                current_period_end,
            } => {
                sqlx::query(
                    "UPDATE subscriptions SET status = $2, current_period_end = $3, updated_at = now() \
                     WHERE id = $1",
                )
                .bind(subscription_id)
                .bind(status.as_str())
                .bind(current_period_end)
                .execute(&mut *tx)
                .await?;
            }
            WebhookMutation::ActivateSubscriptionAndLicense {
                payment_id,
                subscription_id,
                period_end,
                license,
                gateway_payment_id,
            } => {
                // `COALESCE`: a `None` here (an activation payload with no
                // `payment` entity, e.g. some `subscription.activated`
                // deliveries) must never clear a `gateway_payment_id`
                // already recorded by an earlier activation of this same
                // row (renewals reuse it — `repository::payment`'s doc
                // comment).
                sqlx::query(
                    "UPDATE payments SET status = $2, gateway_payment_id = COALESCE($3, gateway_payment_id) \
                     WHERE id = $1",
                )
                .bind(payment_id)
                .bind(PaymentStatus::Succeeded.as_str())
                .bind(&gateway_payment_id)
                .execute(&mut *tx)
                .await?;

                sqlx::query(
                    "UPDATE subscriptions SET status = $2, current_period_end = $3, updated_at = now() \
                     WHERE id = $1",
                )
                .bind(subscription_id)
                .bind(SubscriptionStatus::Active.as_str())
                .bind(period_end)
                .execute(&mut *tx)
                .await?;

                match license {
                    LicenseMutation::Extend { license_id } => {
                        sqlx::query(
                            "UPDATE licenses SET status = $2, expires_at = $3 WHERE id = $1",
                        )
                        .bind(license_id)
                        .bind(LicenseRecordStatus::Active.as_str())
                        .bind(period_end)
                        .execute(&mut *tx)
                        .await?;
                    }
                    LicenseMutation::Insert {
                        license_key,
                        max_devices,
                        grace_period_days,
                    } => {
                        sqlx::query(
                            "INSERT INTO licenses (subscription_id, license_key, status, expires_at, max_devices, grace_period_days) \
                             VALUES ($1, $2, $3, $4, $5, $6)",
                        )
                        .bind(subscription_id)
                        .bind(&license_key)
                        .bind(LicenseRecordStatus::Active.as_str())
                        .bind(period_end)
                        .bind(max_devices)
                        .bind(grace_period_days)
                        .execute(&mut *tx)
                        .await?;
                    }
                }
            }
            WebhookMutation::RefundPayment {
                payment_id,
                license,
            } => {
                sqlx::query("UPDATE payments SET status = $2 WHERE id = $1")
                    .bind(payment_id)
                    .bind(PaymentStatus::Refunded.as_str())
                    .execute(&mut *tx)
                    .await?;

                if let Some(license) = license {
                    sqlx::query("UPDATE licenses SET status = $2, expires_at = $3 WHERE id = $1")
                        .bind(license.license_id)
                        .bind(LicenseRecordStatus::Revoked.as_str())
                        .bind(license.expires_at)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            WebhookMutation::MarkPaymentDisputed {
                payment_id,
                license,
            } => {
                sqlx::query("UPDATE payments SET status = $2 WHERE id = $1")
                    .bind(payment_id)
                    .bind(PaymentStatus::Disputed.as_str())
                    .execute(&mut *tx)
                    .await?;

                if let Some(license) = license {
                    sqlx::query("UPDATE licenses SET status = $2, expires_at = $3 WHERE id = $1")
                        .bind(license.license_id)
                        .bind(LicenseRecordStatus::Suspended.as_str())
                        .bind(license.expires_at)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            WebhookMutation::ResolveDispute {
                payment_id,
                license,
                merchant_won,
            } => {
                let payment_status = if merchant_won {
                    PaymentStatus::Succeeded
                } else {
                    PaymentStatus::Refunded
                };
                sqlx::query("UPDATE payments SET status = $2 WHERE id = $1")
                    .bind(payment_id)
                    .bind(payment_status.as_str())
                    .execute(&mut *tx)
                    .await?;

                if let Some(license) = license {
                    let license_status = if merchant_won {
                        LicenseRecordStatus::Active
                    } else {
                        LicenseRecordStatus::Revoked
                    };
                    sqlx::query("UPDATE licenses SET status = $2, expires_at = $3 WHERE id = $1")
                        .bind(license.license_id)
                        .bind(license_status.as_str())
                        .bind(license.expires_at)
                        .execute(&mut *tx)
                        .await?;
                }
            }
        }

        tx.commit().await?;
        Ok(ClaimOutcome::Applied)
    }
}
