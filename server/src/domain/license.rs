//! `License` — the actual activatable credential (`licenses` table,
//! `LICENSE_DATABASE_SCHEMA.md` §1), kept separate from `Subscription` so
//! "this exact key was activated on these devices" is independently
//! auditable from "this customer's billing status."

use chrono::{DateTime, Utc};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LicenseRecordStatus {
    Active,
    Revoked,
    Expired,
    Suspended,
}

impl LicenseRecordStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            LicenseRecordStatus::Active => "active",
            LicenseRecordStatus::Revoked => "revoked",
            LicenseRecordStatus::Expired => "expired",
            LicenseRecordStatus::Suspended => "suspended",
        }
    }
}

impl fmt::Display for LicenseRecordStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LicenseRecordStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(LicenseRecordStatus::Active),
            "revoked" => Ok(LicenseRecordStatus::Revoked),
            "expired" => Ok(LicenseRecordStatus::Expired),
            "suspended" => Ok(LicenseRecordStatus::Suspended),
            other => Err(format!("unrecognized license status {other:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct License {
    pub id: i64,
    pub subscription_id: i64,
    pub license_key: String,
    pub status: LicenseRecordStatus,
    /// `None` for `lifetime` plans.
    pub expires_at: Option<DateTime<Utc>>,
    pub max_devices: i32,
    pub grace_period_days: i32,
    pub issued_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revoked_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn license_status_round_trips_through_its_string_form() {
        for status in [
            LicenseRecordStatus::Active,
            LicenseRecordStatus::Revoked,
            LicenseRecordStatus::Expired,
            LicenseRecordStatus::Suspended,
        ] {
            assert_eq!(
                LicenseRecordStatus::from_str(status.as_str()).unwrap(),
                status
            );
        }
    }

    #[test]
    fn license_status_rejects_an_unrecognized_string() {
        assert!(LicenseRecordStatus::from_str("bogus").is_err());
    }
}
