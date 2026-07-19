//! license/ — Phase 3A subscription/licensing architecture, load-bearing
//! since Phase 4K.3.
//!
//! See LICENSE_SYSTEM_DESIGN.md for the full design, LICENSE_DATABASE_SCHEMA.md
//! for the schema (migration 6, `db/mod.rs`), API_SPECIFICATION.md for the
//! server contract `client::LicenseApiClient` is built against, and
//! LICENSE_SECURITY_REVIEW.md for the threat model every decision here
//! answers to.
//!
//! **Phase 4K.3 (desktop license enforcement):** `should_enforce()` now
//! returns `true` — the real licensing server (Phase 4F–4K.2) and a real
//! `client::HttpLicenseClient` both now exist, closing the two gaps
//! LICENSE_SYSTEM_DESIGN.md §7 named as the reason enforcement had to stay
//! off. `enforce()` below is the single entry point `main.rs`'s startup
//! gate and periodic revalidation timer both call — see its doc comment.

pub mod client;
pub mod fingerprint;
pub mod storage;
pub mod validation;

pub use client::{ApiError, LicenseApiClient, OfflineClient};

#[cfg(feature = "ai")]
pub use client::HttpLicenseClient;
pub use validation::LicenseStatus;

use chrono::Utc;
use rusqlite::Connection;

/// The single switch controlling whether `LicenseStatus::is_licensed() ==
/// false` should actually block the application. See LICENSE_SYSTEM_DESIGN.md
/// §7 for the full reasoning this was kept as one trivial, obviously-named
/// function for — flipped on in Phase 4K.3 now that a real server and a
/// real `HttpLicenseClient` both exist. Still a single named function
/// (rather than inlined into `enforce()`) so a future kill-switch (a
/// hidden debug flag, or a controlled rollback) is still the same
/// one-line, no-call-site-hunting change this was always designed for.
pub fn should_enforce() -> bool {
    true
}

/// What `main.rs`'s enforcement call sites (the post-login startup gate,
/// and the 24-hour periodic revalidation timer) need to decide UI state
/// from — computed in exactly one place so both call sites agree on what
/// "blocked" means and why, closing the "which call site actually gates
/// access" bypass surface (LICENSE_SYSTEM_DESIGN.md §7's other named gap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnforcementOutcome {
    /// The application may proceed (or keep running, for the periodic
    /// check) — `should_enforce()` is off, or the license is valid
    /// (`LicenseStatus::is_licensed()`).
    Allowed,
    /// The application must not continue into (or must immediately leave)
    /// the full application — `reason` is a ready-to-display message
    /// (`describe()`'s output). `revoked` is `true` only when the server
    /// explicitly reported this license as revoked (not merely expired) —
    /// see `clear_local_activation`'s doc comment for why that distinction
    /// changes what `enforce()` does to local state before returning.
    Blocked { reason: String, revoked: bool },
}

/// The one function `main.rs` calls to decide whether the application may
/// proceed — at the post-login startup gate, and again on every 24-hour
/// periodic revalidation tick while already running. Runs the full
/// `check_status` flow (online validation with offline-grace fallback,
/// unchanged), then applies `should_enforce()`'s kill-switch and, on a
/// server-confirmed revocation specifically, clears the local activation
/// before returning — so a caller that gets `Blocked { revoked: true, .. }`
/// never needs to remember to do that itself.
pub fn enforce(conn: &Connection, api: &dyn LicenseApiClient) -> EnforcementOutcome {
    let status = check_status(conn, api);

    if !should_enforce() || status.is_licensed() {
        return EnforcementOutcome::Allowed;
    }

    let record = storage::load_local_license(conn).ok().flatten();
    let revoked = record
        .as_ref()
        .map(|r| r.status == "revoked")
        .unwrap_or(false);
    if revoked {
        // Not silently swallowed (Phase 4K.3 follow-up): a failure here
        // used to be invisible. It no longer needs to be load-bearing for
        // correctness on its own — `check_status`'s offline branch above
        // already refuses ActiveOfflineGrace for a cached `"revoked"`
        // status regardless of whether this write ever lands — but a
        // repeatedly-failing local-cache write (e.g. disk full, a
        // permissions problem) is still a real operational condition
        // worth surfacing, since it means this installation's local state
        // is stuck out of sync with the server's.
        if let Err(e) = clear_local_activation(conn) {
            log::error!("[license] failed to clear local activation for a revoked license: {e}");
        }
    }

    EnforcementOutcome::Blocked {
        reason: describe(status, record.as_ref()),
        revoked,
    }
}

/// Resets the local license cache back to its pre-activation state — used
/// only when the server has explicitly reported this license as
/// **revoked** (distinct from merely expired/suspended, both of which
/// leave the cached record alone so the activation/renewal screen can
/// still reference the prior plan/expiry as context). Never deletes the
/// `local_license` row itself (there is exactly one, `CHECK (id = 1)`) —
/// only overwrites it with `LocalLicenseRecord::not_activated()`, the same
/// "history via status transitions, not row deletion" convention the
/// server side (`repository::payment_webhook_event`) already follows.
pub fn clear_local_activation(conn: &Connection) -> anyhow::Result<()> {
    storage::save_local_license(conn, &storage::LocalLicenseRecord::not_activated())
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
        let _ = storage::log_validation(
            conn,
            "NotActivated",
            false,
            "record exists but was never activated",
        );
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
            let _ = storage::log_validation(
                conn,
                &format!("{status:?}"),
                true,
                "server validation succeeded",
            );
            status
        }
        Err(_) => {
            // Fail-closed guard, independent of `derive_offline_status`'s
            // own grace-period arithmetic (unchanged, still the only thing
            // that computes ActiveOfflineGrace/GracePeriodExpired for
            // every other status): a cached `status == "revoked"` must
            // never resolve to ActiveOfflineGrace, even if the write that
            // `enforce()`'s revocation handling normally performs
            // (`clear_local_activation`, which nulls `license_id` so this
            // branch would never even be reached) failed to persist. A
            // fresh `last_validated_at` from the very check that reported
            // "revoked" would otherwise read as "well within grace" here —
            // this is the one exception `derive_offline_status` itself
            // still knows nothing about, by design (see that function's
            // doc comment: it's deliberately pure, time-only arithmetic).
            let status = if record.status == "revoked" {
                LicenseStatus::GracePeriodExpired
            } else {
                validation::derive_offline_status(
                    record.grace_period_days,
                    record.last_validated_at,
                    record.highest_seen_clock,
                    now,
                )
            };
            let _ = storage::log_validation(
                conn,
                &format!("{status:?}"),
                false,
                "offline or server unreachable",
            );
            status
        }
    }
}

/// Activates a license key on this device (`POST /activate-license`,
/// API_SPECIFICATION.md). On success, persists the returned license terms
/// locally so subsequent launches can use `check_status`'s offline path.
/// Returns the `ApiError` unmodified on failure — nothing is persisted, so
/// a failed activation attempt leaves any prior local state untouched.
pub fn activate(
    conn: &Connection,
    api: &dyn LicenseApiClient,
    license_key: &str,
) -> Result<LicenseStatus, ApiError> {
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
    storage::save_local_license(conn, &record).map_err(|e| {
        ApiError::ServerError(format!(
            "activation succeeded but failed to persist locally: {e}"
        ))
    })?;
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
    use std::sync::Mutex;

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
    fn should_enforce_is_true_since_phase_4k3() {
        // Guards against silently flipping this back off by accident — see
        // this module's doc comment for why it's safe to be on now (a real
        // server and a real HttpLicenseClient both exist as of Phase 4K.3).
        assert!(should_enforce());
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
        validate_response: Mutex<Option<Result<ValidateLicenseResponse, ApiError>>>,
        activate_response: Mutex<Option<Result<ActivateLicenseResponse, ApiError>>>,
    }

    impl MockClient {
        fn validate_ok(resp: ValidateLicenseResponse) -> Self {
            MockClient {
                validate_response: Mutex::new(Some(Ok(resp))),
                activate_response: Mutex::new(None),
            }
        }
        fn activate_ok(resp: ActivateLicenseResponse) -> Self {
            MockClient {
                validate_response: Mutex::new(None),
                activate_response: Mutex::new(Some(Ok(resp))),
            }
        }
    }

    impl LicenseApiClient for MockClient {
        fn login(&self, _req: &LoginRequest) -> Result<LoginResponse, ApiError> {
            Err(ApiError::NoServerConfigured)
        }
        fn activate_license(
            &self,
            _req: &ActivateLicenseRequest,
        ) -> Result<ActivateLicenseResponse, ApiError> {
            self.activate_response
                .lock()
                .unwrap()
                .take()
                .expect("unexpected activate_license call")
        }
        fn validate_license(
            &self,
            _req: &ValidateLicenseRequest,
        ) -> Result<ValidateLicenseResponse, ApiError> {
            self.validate_response
                .lock()
                .unwrap()
                .take()
                .expect("unexpected validate_license call")
        }
        fn refresh_license(
            &self,
            req: &ValidateLicenseRequest,
        ) -> Result<ValidateLicenseResponse, ApiError> {
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

        let record = storage::load_local_license(&conn)
            .unwrap()
            .expect("must be persisted");
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
        storage::save_local_license(
            &conn,
            &storage::LocalLicenseRecord {
                license_id: Some("lic_1".to_string()),
                status: "active".to_string(),
                last_validated_at: Some(stale),
                grace_period_days: 7,
                highest_seen_clock: Some(stale),
                ..Default::default()
            },
        )
        .unwrap();

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
        assert!(
            record.last_validated_at.unwrap() > stale,
            "last_validated_at must be refreshed on successful online validation"
        );
    }

    #[test]
    fn check_status_with_server_reporting_suspended_returns_suspended() {
        let conn = open_migrated();
        storage::save_local_license(
            &conn,
            &storage::LocalLicenseRecord {
                license_id: Some("lic_1".to_string()),
                status: "active".to_string(),
                last_validated_at: Some(Utc::now()),
                grace_period_days: 7,
                highest_seen_clock: Some(Utc::now()),
                ..Default::default()
            },
        )
        .unwrap();

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
                chrono::DateTime::parse_from_rfc3339("2027-07-09T00:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            ..Default::default()
        };
        let text = describe(LicenseStatus::Active, Some(&record));
        assert!(text.contains("yearly"), "expected tier in: {text}");
        assert!(text.contains("2027"), "expected expiry year in: {text}");
    }

    #[test]
    fn describe_offline_grace_shows_days_remaining() {
        let text = describe(
            LicenseStatus::ActiveOfflineGrace { days_remaining: 3 },
            None,
        );
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
        assert!(
            !status.is_licensed(),
            "corrupted timestamp must never resolve to a licensed status"
        );
    }

    #[test]
    fn check_status_recreates_a_missing_device_info_row_without_erroring() {
        // Reproduces a partially-deleted database: local_license survived
        // but device_info did not (e.g. a manual row delete, or a future
        // migration bug). get_or_create_device_info must transparently
        // recreate it rather than check_status erroring out.
        let conn = open_migrated();
        storage::save_local_license(
            &conn,
            &storage::LocalLicenseRecord {
                license_id: Some("lic_1".to_string()),
                status: "active".to_string(),
                last_validated_at: Some(Utc::now()),
                grace_period_days: 7,
                highest_seen_clock: Some(Utc::now()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            storage::load_device_info(&conn).unwrap(),
            None,
            "precondition: no device_info row yet"
        );

        let status = check_status(&conn, &OfflineClient);
        assert!(matches!(status, LicenseStatus::ActiveOfflineGrace { .. }));
        assert!(
            storage::load_device_info(&conn).unwrap().is_some(),
            "device_info must have been created on demand"
        );
    }

    #[test]
    fn repeated_check_status_calls_are_idempotent_on_device_identity() {
        // A "second launch" (or, in the future, a periodic heartbeat) must
        // never regenerate the device identity — the server would see a
        // stream of "new" devices for what is really one installation.
        let conn = open_migrated();
        storage::save_local_license(
            &conn,
            &storage::LocalLicenseRecord {
                license_id: Some("lic_1".to_string()),
                status: "active".to_string(),
                last_validated_at: Some(Utc::now()),
                grace_period_days: 7,
                highest_seen_clock: Some(Utc::now()),
                ..Default::default()
            },
        )
        .unwrap();

        check_status(&conn, &OfflineClient);
        let device_after_first = storage::load_device_info(&conn).unwrap().unwrap();
        check_status(&conn, &OfflineClient);
        check_status(&conn, &OfflineClient);
        let device_after_third = storage::load_device_info(&conn).unwrap().unwrap();

        assert_eq!(device_after_first.device_id, device_after_third.device_id);

        let log_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM license_validation_log", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            log_count, 3,
            "each check_status call must append its own audit-log row"
        );
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
            storage::load_local_license(&conn).unwrap(),
            None,
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

    // ── enforce() (Phase 4K.3 desktop enforcement) ──────────────────────

    fn active_record(last_validated_at: chrono::DateTime<Utc>) -> storage::LocalLicenseRecord {
        storage::LocalLicenseRecord {
            license_id: Some("lic_1".to_string()),
            status: "active".to_string(),
            last_validated_at: Some(last_validated_at),
            grace_period_days: 7,
            highest_seen_clock: Some(last_validated_at),
            ..Default::default()
        }
    }

    #[test]
    fn enforce_blocks_startup_when_there_is_no_local_license_record() {
        let conn = open_migrated();
        let outcome = enforce(&conn, &OfflineClient);
        assert!(matches!(
            outcome,
            EnforcementOutcome::Blocked { revoked: false, .. }
        ));
    }

    #[test]
    fn enforce_allows_a_valid_cached_license_via_online_validation() {
        let conn = open_migrated();
        storage::save_local_license(&conn, &active_record(Utc::now())).unwrap();
        let mock = MockClient::validate_ok(ValidateLicenseResponse {
            status: "active".to_string(),
            expires_at: Some((Utc::now() + chrono::Duration::days(300)).to_rfc3339()),
            grace_period_days: 7,
            server_time: Utc::now().to_rfc3339(),
            fingerprint_matched: true,
        });

        assert_eq!(enforce(&conn, &mock), EnforcementOutcome::Allowed);
    }

    #[test]
    fn enforce_blocks_and_clears_local_activation_when_the_server_reports_revoked() {
        let conn = open_migrated();
        storage::save_local_license(&conn, &active_record(Utc::now())).unwrap();
        let mock = MockClient::validate_ok(ValidateLicenseResponse {
            status: "revoked".to_string(),
            expires_at: None,
            grace_period_days: 7,
            server_time: Utc::now().to_rfc3339(),
            fingerprint_matched: true,
        });

        let outcome = enforce(&conn, &mock);
        assert!(matches!(
            outcome,
            EnforcementOutcome::Blocked { revoked: true, .. }
        ));
        let record = storage::load_local_license(&conn).unwrap();
        assert_eq!(
            record.map(|r| r.license_id),
            Some(None),
            "a revoked license must have its local activation cleared, not merely be reported blocked"
        );
    }

    #[test]
    fn check_status_never_grants_offline_grace_to_a_cached_revoked_status() {
        // Simulates the exact gap this test guards against: a record
        // already marked "revoked" (as if a prior `clear_local_activation`
        // had failed to persist) with a `last_validated_at` from moments
        // ago — well within `grace_period_days`, which would otherwise
        // resolve to `ActiveOfflineGrace` via `derive_offline_status`'s
        // purely time-based arithmetic (unchanged, still correct for
        // every other status).
        let conn = open_migrated();
        storage::save_local_license(
            &conn,
            &storage::LocalLicenseRecord {
                license_id: Some("lic_1".to_string()),
                status: "revoked".to_string(),
                last_validated_at: Some(Utc::now()),
                grace_period_days: 7,
                highest_seen_clock: Some(Utc::now()),
                ..Default::default()
            },
        )
        .unwrap();

        let status = check_status(&conn, &OfflineClient);
        assert_eq!(
            status,
            LicenseStatus::GracePeriodExpired,
            "a cached revoked status must never resolve to ActiveOfflineGrace, regardless of how recent last_validated_at is"
        );
        assert!(!status.is_licensed());
    }

    #[test]
    fn enforce_does_not_panic_when_clearing_a_revoked_record_fails_and_the_next_offline_check_still_blocks(
    ) {
        let conn = open_migrated();
        // Pre-seeds a record already marked revoked (as if an earlier
        // clear attempt had already failed once) with a fresh
        // last_validated_at — the exact condition that would otherwise
        // read as "well within grace".
        storage::save_local_license(
            &conn,
            &storage::LocalLicenseRecord {
                license_id: Some("lic_1".to_string()),
                status: "revoked".to_string(),
                last_validated_at: Some(Utc::now()),
                grace_period_days: 7,
                highest_seen_clock: Some(Utc::now()),
                ..Default::default()
            },
        )
        .unwrap();

        // Forces the write inside clear_local_activation (and every other
        // local_license write on this connection) to fail, simulating a
        // disk/permissions problem — without this, SQLite would happily
        // let the clear succeed and this test would prove nothing new.
        conn.pragma_update(None, "query_only", true).unwrap();

        let outcome = enforce(&conn, &OfflineClient);
        assert!(
            matches!(outcome, EnforcementOutcome::Blocked { revoked: true, .. }),
            "must still report Blocked{{revoked: true}} even when the clearing write itself fails"
        );

        conn.pragma_update(None, "query_only", false).unwrap();

        // Confirms the write genuinely failed (the record still reads
        // "revoked", proving this test actually exercised the failure
        // path, not a no-op).
        let record = storage::load_local_license(&conn).unwrap().unwrap();
        assert_eq!(record.status, "revoked");

        // The next offline check must still refuse to grant offline
        // grace, thanks to check_status's own revoked short-circuit —
        // never dependent on the earlier clear having actually succeeded.
        let status = check_status(&conn, &OfflineClient);
        assert!(
            !status.is_licensed(),
            "a revoked license must stay blocked on a later offline launch even if it was never successfully cleared"
        );
    }

    #[test]
    fn enforce_blocks_without_clearing_local_activation_when_the_server_reports_expired() {
        let conn = open_migrated();
        storage::save_local_license(&conn, &active_record(Utc::now())).unwrap();
        let mock = MockClient::validate_ok(ValidateLicenseResponse {
            status: "expired".to_string(),
            expires_at: None,
            grace_period_days: 7,
            server_time: Utc::now().to_rfc3339(),
            fingerprint_matched: true,
        });

        let outcome = enforce(&conn, &mock);
        assert!(matches!(
            outcome,
            EnforcementOutcome::Blocked { revoked: false, .. }
        ));
        let record = storage::load_local_license(&conn).unwrap().unwrap();
        assert_eq!(
            record.license_id.as_deref(),
            Some("lic_1"),
            "an expired (not revoked) license must keep its local record for renew-screen context"
        );
    }

    #[test]
    fn enforce_allows_when_offline_inside_the_grace_period() {
        let conn = open_migrated();
        storage::save_local_license(
            &conn,
            &active_record(Utc::now() - chrono::Duration::days(2)),
        )
        .unwrap();

        assert_eq!(enforce(&conn, &OfflineClient), EnforcementOutcome::Allowed);
    }

    #[test]
    fn enforce_blocks_when_offline_after_the_grace_period() {
        let conn = open_migrated();
        storage::save_local_license(
            &conn,
            &active_record(Utc::now() - chrono::Duration::days(30)),
        )
        .unwrap();

        let outcome = enforce(&conn, &OfflineClient);
        assert!(matches!(
            outcome,
            EnforcementOutcome::Blocked { revoked: false, .. }
        ));
    }

    #[test]
    fn enforce_treats_a_network_error_the_same_as_offline_not_a_hard_failure() {
        struct FailingClient;
        impl LicenseApiClient for FailingClient {
            fn login(&self, _: &LoginRequest) -> Result<LoginResponse, ApiError> {
                Err(ApiError::NetworkError("connection refused".to_string()))
            }
            fn activate_license(
                &self,
                _: &ActivateLicenseRequest,
            ) -> Result<ActivateLicenseResponse, ApiError> {
                Err(ApiError::NetworkError("connection refused".to_string()))
            }
            fn validate_license(
                &self,
                _: &ValidateLicenseRequest,
            ) -> Result<ValidateLicenseResponse, ApiError> {
                Err(ApiError::NetworkError("connection refused".to_string()))
            }
            fn refresh_license(
                &self,
                req: &ValidateLicenseRequest,
            ) -> Result<ValidateLicenseResponse, ApiError> {
                self.validate_license(req)
            }
            fn logout(&self) -> Result<(), ApiError> {
                Err(ApiError::NetworkError("connection refused".to_string()))
            }
            fn get_subscription(&self) -> Result<SubscriptionSummary, ApiError> {
                Err(ApiError::NetworkError("connection refused".to_string()))
            }
            fn heartbeat(&self, _: &HeartbeatRequest) -> Result<HeartbeatResponse, ApiError> {
                Err(ApiError::NetworkError("connection refused".to_string()))
            }
        }

        let conn = open_migrated();
        storage::save_local_license(&conn, &active_record(Utc::now())).unwrap();

        // A recent validation plus a network failure must fall back to the
        // offline-grace computation (still within grace here), not a hard
        // block — a network blip must never be indistinguishable from a
        // confirmed-invalid license.
        assert_eq!(enforce(&conn, &FailingClient), EnforcementOutcome::Allowed);
    }

    #[test]
    fn enforce_blocks_when_the_local_cache_is_corrupted() {
        let conn = open_migrated();
        conn.execute(
            "INSERT INTO local_license (id, license_id, status, last_validated_at, grace_period_days)
             VALUES (1, 'lic_1', 'active', 'not-a-real-timestamp', 7)",
            [],
        ).unwrap();

        let outcome = enforce(&conn, &OfflineClient);
        assert!(matches!(
            outcome,
            EnforcementOutcome::Blocked { revoked: false, .. }
        ));
    }

    #[test]
    fn enforce_blocks_when_the_local_cache_fails_its_integrity_check() {
        let conn = open_migrated();
        storage::save_local_license(&conn, &active_record(Utc::now())).unwrap();
        // Simulates a plain SQLite-row edit, bypassing save_local_license.
        conn.execute("UPDATE local_license SET status = 'active', expires_at = '2099-01-01T00:00:00Z' WHERE id = 1", []).unwrap();

        let outcome = enforce(&conn, &OfflineClient);
        assert!(
            matches!(outcome, EnforcementOutcome::Blocked { .. }),
            "a tampered local cache must never resolve to Allowed"
        );
    }

    #[test]
    fn enforce_blocks_after_activation_is_removed_between_checks() {
        // Simulates the periodic revalidation timer's re-check catching an
        // activation that disappeared mid-session (support tooling, a
        // manual local_license wipe, `clear_local_activation` from an
        // earlier revocation) without needing a restart.
        let conn = open_migrated();
        storage::save_local_license(&conn, &active_record(Utc::now())).unwrap();
        assert_eq!(
            enforce(&conn, &OfflineClient),
            EnforcementOutcome::Allowed,
            "precondition: a fresh activation is allowed"
        );

        clear_local_activation(&conn).unwrap();

        let outcome = enforce(&conn, &OfflineClient);
        assert!(matches!(
            outcome,
            EnforcementOutcome::Blocked { revoked: false, .. }
        ));
    }
}
