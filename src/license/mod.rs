//! license/ — Phase 3A subscription/licensing architecture.
//!
//! See LICENSE_SYSTEM_DESIGN.md for the full design, LICENSE_DATABASE_SCHEMA.md
//! for the schema (migration 6, `db/mod.rs`), API_SPECIFICATION.md for the
//! server contract `client::LicenseApiClient` is built against, and
//! LICENSE_SECURITY_REVIEW.md for the threat model every decision here
//! answers to.
//!
//! **This module does not gate application access in this phase** — see
//! `should_enforce()`. It is fully implemented and tested, wired into
//! startup (logs and records status on every launch), but not load-bearing
//! yet: no real server exists to validate against, and no payment flow
//! exists to let a blocked user unblock themselves.

pub mod client;
pub mod fingerprint;
pub mod storage;
pub mod validation;

pub use client::{ApiError, LicenseApiClient, OfflineClient};
pub use validation::LicenseStatus;

use chrono::Utc;
use rusqlite::Connection;

/// The single switch controlling whether `LicenseStatus::is_licensed() ==
/// false` should actually block the application. See LICENSE_SYSTEM_DESIGN.md
/// §7 for the full reasoning — kept as one trivial, obviously-named function
/// specifically so flipping it on later (once a real server and payment path
/// exist) is a one-line change with no other call site to hunt down.
pub fn should_enforce() -> bool {
    false
}

/// Runs the full validation flow (LICENSE_SYSTEM_DESIGN.md §4): reads local
/// state, attempts an online check via `client`, falls back to the offline
/// grace-period computation on any failure or when there's simply no server
/// configured (`OfflineClient`, today's only implementation), persists the
/// outcome, and logs it. Never panics, never returns `Err` — every failure
/// mode (unreadable local state, network error, malformed server response)
/// resolves to a concrete, conservative `LicenseStatus` per
/// LICENSE_SECURITY_REVIEW.md §6's fail-closed rule, so callers never need
/// a separate error-handling path on top of matching the status itself.
pub fn check_status(conn: &Connection, api: &dyn LicenseApiClient) -> LicenseStatus {
    let now = Utc::now();

    // Advance the rollback watermark unconditionally, first — must happen
    // even if everything below fails to read/write anything else.
    let _ = storage::advance_clock_watermark(conn, now);

    let record = match storage::load_local_license(conn) {
        Ok(Some(r)) => r,
        Ok(None) => {
            let _ = storage::log_validation(conn, "NotActivated", false, "no local license record");
            return LicenseStatus::NotActivated;
        }
        Err(e) => {
            let _ = storage::log_validation(
                conn,
                "GracePeriodExpired",
                false,
                &format!("failed to read local license state: {e}"),
            );
            return LicenseStatus::GracePeriodExpired;
        }
    };

    let Some(license_id) = record.license_id.clone() else {
        let _ = storage::log_validation(conn, "NotActivated", false, "record exists but was never activated");
        return LicenseStatus::NotActivated;
    };

    let device = match storage::get_or_create_device_info(conn) {
        Ok(d) => d,
        Err(e) => {
            let _ = storage::log_validation(
                conn,
                "GracePeriodExpired",
                false,
                &format!("failed to read device identity: {e}"),
            );
            return LicenseStatus::GracePeriodExpired;
        }
    };

    let online_result = api.validate_license(&client::ValidateLicenseRequest {
        license_id,
        device_id: device.device_id,
        machine_fingerprint: device.machine_fingerprint,
        client_clock: now.to_rfc3339(),
    });

    match online_result {
        Ok(resp) => {
            let status = validation::status_from_server_response(&resp.status);
            let mut updated = record;
            updated.status = resp.status;
            updated.expires_at = parse_rfc3339(resp.expires_at.as_deref());
            updated.grace_period_days = resp.grace_period_days;
            updated.last_validated_at = Some(now);
            let _ = storage::save_local_license(conn, &updated);
            let _ = storage::log_validation(conn, &format!("{status:?}"), true, "server validation succeeded");
            status
        }
        Err(_) => {
            let status = validation::derive_offline_status(
                record.grace_period_days,
                record.last_validated_at,
                record.highest_seen_clock,
                now,
            );
            let _ = storage::log_validation(conn, &format!("{status:?}"), false, "offline or server unreachable");
            status
        }
    }
}

/// Activates a license key on this device (`POST /activate-license`,
/// API_SPECIFICATION.md). On success, persists the returned license terms
/// locally so subsequent launches can use `check_status`'s offline path.
/// Returns the `ApiError` unmodified on failure — nothing is persisted, so
/// a failed activation attempt leaves any prior local state untouched.
pub fn activate(conn: &Connection, api: &dyn LicenseApiClient, license_key: &str) -> Result<LicenseStatus, ApiError> {
    let now = Utc::now();
    let device = storage::get_or_create_device_info(conn)
        .map_err(|e| ApiError::ServerError(format!("failed to read device identity: {e}")))?;
    let device_label = fingerprint::FingerprintInputs::collect().computer_name;

    let resp = api.activate_license(&client::ActivateLicenseRequest {
        license_key: license_key.to_string(),
        device_id: device.device_id,
        machine_fingerprint: device.machine_fingerprint,
        device_label,
    })?;

    let status = validation::status_from_server_response(&resp.status);
    let record = storage::LocalLicenseRecord {
        customer_id: Some(resp.customer_id),
        license_id: Some(resp.license_id),
        license_key: Some(license_key.to_string()),
        subscription_type: Some(resp.subscription_type),
        status: resp.status,
        expires_at: parse_rfc3339(resp.expires_at.as_deref()),
        last_validated_at: Some(now),
        grace_period_days: resp.grace_period_days,
        highest_seen_clock: Some(now),
    };
    storage::save_local_license(conn, &record)
        .map_err(|e| ApiError::ServerError(format!("activation succeeded but failed to persist locally: {e}")))?;
    let _ = storage::log_validation(conn, &format!("{status:?}"), true, "activation succeeded");
    Ok(status)
}

fn parse_rfc3339(s: Option<&str>) -> Option<chrono::DateTime<Utc>> {
    s.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// Renders a `LicenseStatus` (plus the locally-cached record, when one
/// exists) as a short, human-readable line for the Settings screen. Pure
/// and side-effect-free so it's directly unit-testable — the UI layer just
/// calls this with whatever `check_status`/`load_local_license` already
/// returned, no extra DB or clock reads.
pub fn describe(status: LicenseStatus, record: Option<&storage::LocalLicenseRecord>) -> String {
    let tier = record
        .and_then(|r| r.subscription_type.as_deref())
        .unwrap_or("unknown plan");
    let expires = record
        .and_then(|r| r.expires_at)
        .map(|d| d.format("%d %b %Y").to_string());

    match status {
        LicenseStatus::NotActivated => {
            "Not activated. Enter a license key below to activate.".to_string()
        }
        LicenseStatus::Active => match expires {
            Some(e) => format!("Active ({tier}) — valid until {e}."),
            None => format!("Active ({tier})."),
        },
        LicenseStatus::ActiveOfflineGrace { days_remaining } => format!(
            "Active ({tier}) — running offline, {days_remaining} day(s) left before revalidation is required."
        ),
        LicenseStatus::GracePeriodExpired => {
            "Offline grace period has expired. Connect to the internet to revalidate, or renew your subscription.".to_string()
        }
        LicenseStatus::Suspended => {
            "Suspended (e.g. a payment issue). Contact support to resolve — no reactivation needed once resolved.".to_string()
        }
        LicenseStatus::Expired => {
            "Expired. Renew your subscription and activate the new license key below.".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use client::*;
    use std::cell::RefCell;

    fn open_migrated() -> Connection {
        crate::db::open(":memory:").expect("open in-memory db")
    }

    #[test]
    fn check_status_with_no_local_record_is_not_activated() {
        let conn = open_migrated();
        let status = check_status(&conn, &OfflineClient);
        assert_eq!(status, LicenseStatus::NotActivated);
    }

    #[test]
    fn should_enforce_is_false_in_this_phase() {
        // Guards against silently flipping this on by accident — see
        // LICENSE_SYSTEM_DESIGN.md §7 for why it must stay false until a
        // real server and payment path exist.
        assert!(!should_enforce());
    }

    #[test]
    fn check_status_with_offline_client_and_fresh_activation_is_active_offline_grace() {
        let conn = open_migrated();
        let record = storage::LocalLicenseRecord {
            license_id: Some("lic_1".to_string()),
            status: "active".to_string(),
            last_validated_at: Some(Utc::now()),
            grace_period_days: 7,
            highest_seen_clock: Some(Utc::now()),
            ..Default::default()
        };
        storage::save_local_license(&conn, &record).unwrap();

        let status = check_status(&conn, &OfflineClient);
        assert!(matches!(status, LicenseStatus::ActiveOfflineGrace { .. }));
    }

    #[test]
    fn check_status_with_offline_client_past_grace_period_is_expired() {
        let conn = open_migrated();
        let long_ago = Utc::now() - chrono::Duration::days(30);
        let record = storage::LocalLicenseRecord {
            license_id: Some("lic_1".to_string()),
            status: "active".to_string(),
            last_validated_at: Some(long_ago),
            grace_period_days: 7,
            highest_seen_clock: Some(long_ago),
            ..Default::default()
        };
        storage::save_local_license(&conn, &record).unwrap();

        let status = check_status(&conn, &OfflineClient);
        assert_eq!(status, LicenseStatus::GracePeriodExpired);
    }

    /// A minimal mock implementing `LicenseApiClient`, for tests that need
    /// to simulate a real server responding (something `OfflineClient`
    /// deliberately never does).
    struct MockClient {
        validate_response: RefCell<Option<Result<ValidateLicenseResponse, ApiError>>>,
        activate_response: RefCell<Option<Result<ActivateLicenseResponse, ApiError>>>,
    }

    impl MockClient {
        fn validate_ok(resp: ValidateLicenseResponse) -> Self {
            MockClient {
                validate_response: RefCell::new(Some(Ok(resp))),
                activate_response: RefCell::new(None),
            }
        }
        fn activate_ok(resp: ActivateLicenseResponse) -> Self {
            MockClient {
                validate_response: RefCell::new(None),
                activate_response: RefCell::new(Some(Ok(resp))),
            }
        }
    }

    impl LicenseApiClient for MockClient {
        fn login(&self, _req: &LoginRequest) -> Result<LoginResponse, ApiError> {
            Err(ApiError::NoServerConfigured)
        }
        fn activate_license(&self, _req: &ActivateLicenseRequest) -> Result<ActivateLicenseResponse, ApiError> {
            self.activate_response.borrow_mut().take().expect("unexpected activate_license call")
        }
        fn validate_license(&self, _req: &ValidateLicenseRequest) -> Result<ValidateLicenseResponse, ApiError> {
            self.validate_response.borrow_mut().take().expect("unexpected validate_license call")
        }
        fn refresh_license(&self, req: &ValidateLicenseRequest) -> Result<ValidateLicenseResponse, ApiError> {
            self.validate_license(req)
        }
        fn logout(&self) -> Result<(), ApiError> {
            Ok(())
        }
        fn get_subscription(&self) -> Result<SubscriptionSummary, ApiError> {
            Err(ApiError::NoServerConfigured)
        }
        fn heartbeat(&self, _req: &HeartbeatRequest) -> Result<HeartbeatResponse, ApiError> {
            Err(ApiError::NoServerConfigured)
        }
    }

    #[test]
    fn activate_persists_the_server_response_locally() {
        let conn = open_migrated();
        let mock = MockClient::activate_ok(ActivateLicenseResponse {
            license_id: "lic_999".to_string(),
            customer_id: "cus_999".to_string(),
            subscription_type: "monthly".to_string(),
            status: "active".to_string(),
            expires_at: Some((Utc::now() + chrono::Duration::days(30)).to_rfc3339()),
            grace_period_days: 5,
        });

        let status = activate(&conn, &mock, "TEST-KEY-0000-0000").expect("activation must succeed");
        assert_eq!(status, LicenseStatus::Active);

        let record = storage::load_local_license(&conn).unwrap().expect("must be persisted");
        assert_eq!(record.license_id.as_deref(), Some("lic_999"));
        assert_eq!(record.customer_id.as_deref(), Some("cus_999"));
        assert_eq!(record.subscription_type.as_deref(), Some("monthly"));
        assert_eq!(record.grace_period_days, 5);
        assert!(record.last_validated_at.is_some());
    }

    #[test]
    fn check_status_with_successful_online_validation_is_active_and_updates_last_validated_at() {
        let conn = open_migrated();
        let stale = Utc::now() - chrono::Duration::days(20); // would be GracePeriodExpired offline
        storage::save_local_license(&conn, &storage::LocalLicenseRecord {
            license_id: Some("lic_1".to_string()),
            status: "active".to_string(),
            last_validated_at: Some(stale),
            grace_period_days: 7,
            highest_seen_clock: Some(stale),
            ..Default::default()
        }).unwrap();

        let mock = MockClient::validate_ok(ValidateLicenseResponse {
            status: "active".to_string(),
            expires_at: Some((Utc::now() + chrono::Duration::days(300)).to_rfc3339()),
            grace_period_days: 7,
            server_time: Utc::now().to_rfc3339(),
            fingerprint_matched: true,
        });

        let status = check_status(&conn, &mock);
        assert_eq!(status, LicenseStatus::Active, "a real server confirming active must override an otherwise-expired offline grace window");

        let record = storage::load_local_license(&conn).unwrap().unwrap();
        assert!(record.last_validated_at.unwrap() > stale, "last_validated_at must be refreshed on successful online validation");
    }

    #[test]
    fn check_status_with_server_reporting_suspended_returns_suspended() {
        let conn = open_migrated();
        storage::save_local_license(&conn, &storage::LocalLicenseRecord {
            license_id: Some("lic_1".to_string()),
            status: "active".to_string(),
            last_validated_at: Some(Utc::now()),
            grace_period_days: 7,
            highest_seen_clock: Some(Utc::now()),
            ..Default::default()
        }).unwrap();

        let mock = MockClient::validate_ok(ValidateLicenseResponse {
            status: "suspended".to_string(),
            expires_at: None,
            grace_period_days: 7,
            server_time: Utc::now().to_rfc3339(),
            fingerprint_matched: true,
        });

        let status = check_status(&conn, &mock);
        assert_eq!(status, LicenseStatus::Suspended);
        assert!(!status.is_licensed());
    }

    #[test]
    fn describe_not_activated_prompts_for_a_key() {
        let text = describe(LicenseStatus::NotActivated, None);
        assert!(text.contains("Not activated"));
    }

    #[test]
    fn describe_active_includes_tier_and_expiry_when_known() {
        let record = storage::LocalLicenseRecord {
            subscription_type: Some("yearly".to_string()),
            expires_at: Some(
                chrono::DateTime::parse_from_rfc3339("2027-07-09T00:00:00Z").unwrap().with_timezone(&Utc),
            ),
            ..Default::default()
        };
        let text = describe(LicenseStatus::Active, Some(&record));
        assert!(text.contains("yearly"), "expected tier in: {text}");
        assert!(text.contains("2027"), "expected expiry year in: {text}");
    }

    #[test]
    fn describe_offline_grace_shows_days_remaining() {
        let text = describe(LicenseStatus::ActiveOfflineGrace { days_remaining: 3 }, None);
        assert!(text.contains('3'), "expected days remaining in: {text}");
    }

    #[test]
    fn check_status_survives_a_corrupted_last_validated_at_timestamp() {
        // Simulate on-disk corruption / a hand-edited row: license_id is
        // present (activated) but last_validated_at is not valid RFC-3339.
        // storage::parse_ts silently maps unparseable text to None — this
        // proves check_status then fails closed (never Active) rather than
        // panicking or, worse, treating garbage as "just validated".
        let conn = open_migrated();
        conn.execute(
            "INSERT INTO local_license (id, license_id, status, last_validated_at, grace_period_days)
             VALUES (1, 'lic_1', 'active', 'not-a-real-timestamp', 7)",
            [],
        ).unwrap();

        let status = check_status(&conn, &OfflineClient);
        assert!(!status.is_licensed(), "corrupted timestamp must never resolve to a licensed status");
    }

    #[test]
    fn check_status_recreates_a_missing_device_info_row_without_erroring() {
        // Reproduces a partially-deleted database: local_license survived
        // but device_info did not (e.g. a manual row delete, or a future
        // migration bug). get_or_create_device_info must transparently
        // recreate it rather than check_status erroring out.
        let conn = open_migrated();
        storage::save_local_license(&conn, &storage::LocalLicenseRecord {
            license_id: Some("lic_1".to_string()),
            status: "active".to_string(),
            last_validated_at: Some(Utc::now()),
            grace_period_days: 7,
            highest_seen_clock: Some(Utc::now()),
            ..Default::default()
        }).unwrap();
        assert_eq!(storage::load_device_info(&conn).unwrap(), None, "precondition: no device_info row yet");

        let status = check_status(&conn, &OfflineClient);
        assert!(matches!(status, LicenseStatus::ActiveOfflineGrace { .. }));
        assert!(storage::load_device_info(&conn).unwrap().is_some(), "device_info must have been created on demand");
    }

    #[test]
    fn repeated_check_status_calls_are_idempotent_on_device_identity() {
        // A "second launch" (or, in the future, a periodic heartbeat) must
        // never regenerate the device identity — the server would see a
        // stream of "new" devices for what is really one installation.
        let conn = open_migrated();
        storage::save_local_license(&conn, &storage::LocalLicenseRecord {
            license_id: Some("lic_1".to_string()),
            status: "active".to_string(),
            last_validated_at: Some(Utc::now()),
            grace_period_days: 7,
            highest_seen_clock: Some(Utc::now()),
            ..Default::default()
        }).unwrap();

        check_status(&conn, &OfflineClient);
        let device_after_first = storage::load_device_info(&conn).unwrap().unwrap();
        check_status(&conn, &OfflineClient);
        check_status(&conn, &OfflineClient);
        let device_after_third = storage::load_device_info(&conn).unwrap().unwrap();

        assert_eq!(device_after_first.device_id, device_after_third.device_id);

        let log_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM license_validation_log", [], |r| r.get(0),
        ).unwrap();
        assert_eq!(log_count, 3, "each check_status call must append its own audit-log row");
    }

    #[test]
    fn activate_with_no_server_configured_leaves_prior_local_state_untouched() {
        // Reinstall / fresh-activation-attempt-before-a-server-exists path:
        // a failed activation must not clobber whatever was locally cached
        // before the attempt (there is nothing to clobber to yet in this
        // phase, but this locks in the "failure persists nothing" contract
        // for when a real, sometimes-failing HttpLicenseClient lands).
        let conn = open_migrated();
        assert_eq!(storage::load_local_license(&conn).unwrap(), None);

        let result = activate(&conn, &OfflineClient, "SOME-KEY-0000");
        assert_eq!(result.unwrap_err(), ApiError::NoServerConfigured);
        assert_eq!(
            storage::load_local_license(&conn).unwrap(), None,
            "a failed activation must not create a local_license row"
        );
    }

    #[test]
    fn describe_never_panics_on_every_status_with_no_record() {
        for status in [
            LicenseStatus::NotActivated,
            LicenseStatus::Active,
            LicenseStatus::ActiveOfflineGrace { days_remaining: 0 },
            LicenseStatus::GracePeriodExpired,
            LicenseStatus::Suspended,
            LicenseStatus::Expired,
        ] {
            let text = describe(status, None);
            assert!(!text.is_empty());
        }
    }
}
