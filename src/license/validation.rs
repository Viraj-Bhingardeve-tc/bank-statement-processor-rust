// validation.rs — LicenseStatus and its derivation logic.
//
// See LICENSE_SYSTEM_DESIGN.md §4 (validation flow) and
// LICENSE_SECURITY_REVIEW.md §1 (clock rollback) and §6 (fail-closed rule).
//
// This module is deliberately pure — no database, no clock reads, no I/O —
// so every branch of the flow diagram is directly unit-testable by passing
// in whatever `now`/stored values a test wants, including adversarial ones
// (clock moved backward, corrupted/missing data). `license::mod` is the
// thin, untested-by-necessity glue that reads real data and calls a real
// clock before handing values to these functions.

use chrono::{DateTime, Utc};

/// Mirrors the task's required status set exactly. No catch-all variant —
/// every caller must handle each case explicitly, and every derivation
/// function below resolves errors/missing-data to `GracePeriodExpired` or
/// `NotActivated`, never silently to `Active` (LICENSE_SECURITY_REVIEW.md §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LicenseStatus {
    /// No local license record at all — never activated on this device.
    NotActivated,
    /// Confirmed active by a real, successful server validation.
    Active,
    /// No successful server contact within the grace period, but still
    /// within it — allowed to keep working offline.
    ActiveOfflineGrace { days_remaining: i64 },
    /// Offline grace period has elapsed with no successful revalidation.
    GracePeriodExpired,
    /// Server explicitly reported this license as suspended (e.g. failed
    /// payment) — distinct from expired: recoverable without reactivation.
    Suspended,
    /// Server explicitly reported expired/revoked, or the stored
    /// `expires_at` has passed.
    Expired,
}

impl LicenseStatus {
    /// Whether the application should currently be considered licensed.
    /// The single predicate `main.rs` would gate on, once enforcement is
    /// turned on (LICENSE_SYSTEM_DESIGN.md §7) — kept as one function so
    /// there is exactly one place that encodes "which statuses count as
    /// licensed," not one per call site.
    pub fn is_licensed(&self) -> bool {
        matches!(
            self,
            LicenseStatus::Active | LicenseStatus::ActiveOfflineGrace { .. }
        )
    }
}

/// Derives status purely from locally-cached state — the path taken when
/// the server was not reached this check (no internet, or the request
/// failed). See LICENSE_SYSTEM_DESIGN.md §4's flow diagram, the branch
/// below "Check offline grace period".
///
/// `last_validated_at: None` means this device has never once successfully
/// validated (either truly never activated, or activated but the very
/// first validation hasn't succeeded yet) — treated as `NotActivated`
/// rather than guessing a grace period from nothing.
pub fn derive_offline_status(
    grace_period_days: i64,
    last_validated_at: Option<DateTime<Utc>>,
    highest_seen_clock: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> LicenseStatus {
    let Some(last_validated_at) = last_validated_at else {
        return LicenseStatus::NotActivated;
    };

    // Clock-rollback defense: fail closed the instant "now" is behind the
    // highest timestamp this installation has ever observed, regardless of
    // what the grace-period arithmetic below would otherwise say. See
    // LICENSE_SECURITY_REVIEW.md §1 for the exact threat and honest
    // limitation.
    if let Some(watermark) = highest_seen_clock {
        if now < watermark {
            return LicenseStatus::GracePeriodExpired;
        }
    }

    // A negative elapsed time here would mean `now < last_validated_at`
    // without having tripped the watermark check above (e.g. no watermark
    // stored yet) — still not a valid "days remaining" and must not be
    // treated as more time than the full grace period. Clamped to 0 rather
    // than allowed to produce a nonsensical negative `elapsed`.
    let elapsed_days = (now - last_validated_at).num_days().max(0);

    if elapsed_days <= grace_period_days {
        LicenseStatus::ActiveOfflineGrace {
            days_remaining: (grace_period_days - elapsed_days).max(0),
        }
    } else {
        LicenseStatus::GracePeriodExpired
    }
}

/// Maps a `/validate-license`-style server response status string directly
/// to a `LicenseStatus` — used only when the server was actually reached
/// (LICENSE_SYSTEM_DESIGN.md §4's "succeeds" branches). An unrecognized
/// string (e.g. a future server introducing a new status this client
/// build doesn't know about) resolves to `Expired`, not `Active` — the
/// same fail-closed rule as the offline path, applied to the online path
/// too.
pub fn status_from_server_response(status: &str) -> LicenseStatus {
    match status {
        "active" => LicenseStatus::Active,
        "suspended" => LicenseStatus::Suspended,
        "expired" | "revoked" | "device_mismatch" => LicenseStatus::Expired,
        _ => LicenseStatus::Expired,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn no_last_validated_at_means_not_activated() {
        let status = derive_offline_status(7, None, None, now());
        assert_eq!(status, LicenseStatus::NotActivated);
    }

    #[test]
    fn within_grace_period_is_active_offline_grace_with_correct_days_remaining() {
        let last = now() - Duration::days(3);
        let status = derive_offline_status(7, Some(last), Some(last), now());
        assert_eq!(
            status,
            LicenseStatus::ActiveOfflineGrace { days_remaining: 4 }
        );
    }

    #[test]
    fn exactly_at_the_grace_boundary_is_still_active() {
        let last = now() - Duration::days(7);
        let status = derive_offline_status(7, Some(last), Some(last), now());
        assert_eq!(
            status,
            LicenseStatus::ActiveOfflineGrace { days_remaining: 0 }
        );
    }

    #[test]
    fn one_day_past_the_grace_boundary_is_expired() {
        let last = now() - Duration::days(8);
        let status = derive_offline_status(7, Some(last), Some(last), now());
        assert_eq!(status, LicenseStatus::GracePeriodExpired);
    }

    #[test]
    fn clock_rolled_back_before_the_watermark_fails_closed_even_within_naive_grace_window() {
        // Naively, `now - last_validated_at` would say "0 days elapsed,
        // well within grace" — but `now` is *behind* a timestamp this
        // installation already observed, which is exactly the rollback
        // attack LICENSE_SECURITY_REVIEW.md §1 describes.
        let last = now();
        let watermark = now() + Duration::days(5); // app was run 5 days "later" before the clock was wound back
        let rolled_back_now = now(); // attacker sets clock back to "now" (before the watermark)
        let status = derive_offline_status(7, Some(last), Some(watermark), rolled_back_now);
        assert_eq!(status, LicenseStatus::GracePeriodExpired);
    }

    #[test]
    fn no_watermark_stored_yet_does_not_falsely_trigger_rollback_detection() {
        // First-ever check on a freshly activated license: no watermark
        // recorded yet. Must not be treated as a rollback.
        let last = now() - Duration::days(1);
        let status = derive_offline_status(7, Some(last), None, now());
        assert_eq!(
            status,
            LicenseStatus::ActiveOfflineGrace { days_remaining: 6 }
        );
    }

    #[test]
    fn is_licensed_is_true_only_for_active_and_offline_grace() {
        assert!(LicenseStatus::Active.is_licensed());
        assert!(LicenseStatus::ActiveOfflineGrace { days_remaining: 1 }.is_licensed());
        assert!(!LicenseStatus::NotActivated.is_licensed());
        assert!(!LicenseStatus::GracePeriodExpired.is_licensed());
        assert!(!LicenseStatus::Suspended.is_licensed());
        assert!(!LicenseStatus::Expired.is_licensed());
    }

    #[test]
    fn server_response_mapping_matches_the_documented_enum() {
        assert_eq!(status_from_server_response("active"), LicenseStatus::Active);
        assert_eq!(
            status_from_server_response("suspended"),
            LicenseStatus::Suspended
        );
        assert_eq!(
            status_from_server_response("expired"),
            LicenseStatus::Expired
        );
        assert_eq!(
            status_from_server_response("revoked"),
            LicenseStatus::Expired
        );
        assert_eq!(
            status_from_server_response("device_mismatch"),
            LicenseStatus::Expired
        );
    }

    #[test]
    fn unrecognized_server_status_fails_closed_not_open() {
        // A hypothetical future server status this client build doesn't
        // know about must never be treated as licensed.
        assert_eq!(
            status_from_server_response("some_future_status"),
            LicenseStatus::Expired
        );
        assert!(!status_from_server_response("some_future_status").is_licensed());
    }
}
