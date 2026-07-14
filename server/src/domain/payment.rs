//! `Payment` — payment ledger entry (`payments` table,
//! `LICENSE_DATABASE_SCHEMA.md` §1). A future Razorpay integration only
//! needs to start writing rows here and updating `subscriptions.status` —
//! Phase 4F is exactly that "future."

use chrono::{DateTime, Utc};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentStatus {
    Pending,
    Succeeded,
    Failed,
    Refunded,
    /// An open Razorpay dispute/chargeback on this payment (Phase 4K.2,
    /// `migrations/0004_add_payment_dispute_support.sql`) — distinct from
    /// `Succeeded` (no dispute, or one already resolved for the merchant)
    /// and `Refunded` (money definitively returned to the customer).
    Disputed,
}

impl PaymentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PaymentStatus::Pending => "pending",
            PaymentStatus::Succeeded => "succeeded",
            PaymentStatus::Failed => "failed",
            PaymentStatus::Refunded => "refunded",
            PaymentStatus::Disputed => "disputed",
        }
    }
}

impl fmt::Display for PaymentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PaymentStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(PaymentStatus::Pending),
            "succeeded" => Ok(PaymentStatus::Succeeded),
            "failed" => Ok(PaymentStatus::Failed),
            "refunded" => Ok(PaymentStatus::Refunded),
            "disputed" => Ok(PaymentStatus::Disputed),
            other => Err(format!("unrecognized payment status {other:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Payment {
    pub id: i64,
    pub subscription_id: i64,
    /// Smallest currency unit (paise) — never a float
    /// (`LICENSE_DATABASE_SCHEMA.md` §1's comment on this column).
    pub amount_minor: i64,
    pub currency: String,
    pub provider: String,
    /// The gateway's own payment/order/subscription id — how a webhook
    /// event is correlated back to this row (`repository::payment`).
    pub provider_ref: Option<String>,
    /// The real Razorpay payment id (`payload.payment.entity.id`),
    /// recorded once known at activation time — the *only* field
    /// refund/dispute correlation reads (Phase 4K.2), since those webhooks
    /// never carry `provider_ref`'s checkout-time payment-link/subscription
    /// id. `None` until an activating webhook supplies it.
    pub gateway_payment_id: Option<String>,
    pub status: PaymentStatus,
    pub created_at: DateTime<Utc>,
}

/// Fields needed to create a new `Payment` row — no `id`/`created_at`,
/// since those are database-generated.
#[derive(Debug, Clone, PartialEq)]
pub struct NewPayment {
    pub subscription_id: i64,
    pub amount_minor: i64,
    pub currency: String,
    pub provider: String,
    pub provider_ref: Option<String>,
    pub status: PaymentStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_status_round_trips_through_its_string_form() {
        for status in [
            PaymentStatus::Pending,
            PaymentStatus::Succeeded,
            PaymentStatus::Failed,
            PaymentStatus::Refunded,
            PaymentStatus::Disputed,
        ] {
            assert_eq!(PaymentStatus::from_str(status.as_str()).unwrap(), status);
        }
    }

    #[test]
    fn payment_status_rejects_an_unrecognized_string() {
        assert!(PaymentStatus::from_str("bogus").is_err());
    }
}
