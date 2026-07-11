//! `Subscription` — a purchasable plan a user is subscribed to
//! (`subscriptions` table, `LICENSE_DATABASE_SCHEMA.md` §1). History is
//! kept via status transitions on new rows, never by mutating a past row —
//! see that document's comment on the table.

use chrono::{DateTime, Utc};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanType {
    Trial,
    Monthly,
    Yearly,
    Lifetime,
}

impl PlanType {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanType::Trial => "trial",
            PlanType::Monthly => "monthly",
            PlanType::Yearly => "yearly",
            PlanType::Lifetime => "lifetime",
        }
    }
}

impl fmt::Display for PlanType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PlanType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "trial" => Ok(PlanType::Trial),
            "monthly" => Ok(PlanType::Monthly),
            "yearly" => Ok(PlanType::Yearly),
            "lifetime" => Ok(PlanType::Lifetime),
            other => Err(format!("unrecognized plan_type {other:?}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionStatus {
    Active,
    Expired,
    Cancelled,
    Suspended,
    PendingPayment,
}

impl SubscriptionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SubscriptionStatus::Active => "active",
            SubscriptionStatus::Expired => "expired",
            SubscriptionStatus::Cancelled => "cancelled",
            SubscriptionStatus::Suspended => "suspended",
            SubscriptionStatus::PendingPayment => "pending_payment",
        }
    }
}

impl fmt::Display for SubscriptionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SubscriptionStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(SubscriptionStatus::Active),
            "expired" => Ok(SubscriptionStatus::Expired),
            "cancelled" => Ok(SubscriptionStatus::Cancelled),
            "suspended" => Ok(SubscriptionStatus::Suspended),
            "pending_payment" => Ok(SubscriptionStatus::PendingPayment),
            other => Err(format!("unrecognized subscription status {other:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Subscription {
    pub id: i64,
    pub user_id: i64,
    pub plan_type: PlanType,
    pub status: SubscriptionStatus,
    pub started_at: DateTime<Utc>,
    /// `None` for `lifetime` plans.
    pub current_period_end: Option<DateTime<Utc>>,
    pub auto_renew: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_type_round_trips_through_its_string_form() {
        for pt in [
            PlanType::Trial,
            PlanType::Monthly,
            PlanType::Yearly,
            PlanType::Lifetime,
        ] {
            assert_eq!(PlanType::from_str(pt.as_str()).unwrap(), pt);
        }
    }

    #[test]
    fn plan_type_rejects_an_unrecognized_string() {
        assert!(PlanType::from_str("platinum").is_err());
    }

    #[test]
    fn subscription_status_round_trips_through_its_string_form() {
        for status in [
            SubscriptionStatus::Active,
            SubscriptionStatus::Expired,
            SubscriptionStatus::Cancelled,
            SubscriptionStatus::Suspended,
            SubscriptionStatus::PendingPayment,
        ] {
            assert_eq!(
                SubscriptionStatus::from_str(status.as_str()).unwrap(),
                status
            );
        }
    }

    #[test]
    fn subscription_status_rejects_an_unrecognized_string() {
        assert!(SubscriptionStatus::from_str("bogus").is_err());
    }
}
