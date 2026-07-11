// storage.rs — local_license / device_info / license_validation_log CRUD.
//
// Follows this codebase's established db/mod.rs conventions: plain
// rusqlite, anyhow::Result, ISO-8601 text timestamps. See
// LICENSE_DATABASE_SCHEMA.md §2 for the table definitions (migration 6,
// db/mod.rs).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};

use super::fingerprint::{self, FingerprintInputs};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LocalLicenseRecord {
    pub customer_id: Option<String>,
    pub license_id: Option<String>,
    pub license_key: Option<String>,
    pub subscription_type: Option<String>,
    pub status: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_validated_at: Option<DateTime<Utc>>,
    pub grace_period_days: i64,
    pub highest_seen_clock: Option<DateTime<Utc>>,
}

impl LocalLicenseRecord {
    pub fn not_activated() -> Self {
        LocalLicenseRecord {
            status: "not_activated".to_string(),
            grace_period_days: 7,
            ..Default::default()
        }
    }
}

fn parse_ts(s: Option<String>) -> Option<DateTime<Utc>> {
    s.and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// Returns `None` only when no license has ever been persisted at all
/// (fresh install / never activated) — distinct from a record that exists
/// but is expired/suspended, which is returned as `Some` with that status.
pub fn load_local_license(conn: &Connection) -> Result<Option<LocalLicenseRecord>> {
    conn.query_row(
        "SELECT customer_id, license_id, license_key, subscription_type, status,
                expires_at, last_validated_at, grace_period_days, highest_seen_clock
         FROM local_license WHERE id = 1",
        [],
        |r| {
            Ok(LocalLicenseRecord {
                customer_id: r.get(0)?,
                license_id: r.get(1)?,
                license_key: r.get(2)?,
                subscription_type: r.get(3)?,
                status: r.get(4)?,
                expires_at: parse_ts(r.get(5)?),
                last_validated_at: parse_ts(r.get(6)?),
                grace_period_days: r.get(7)?,
                highest_seen_clock: parse_ts(r.get(8)?),
            })
        },
    )
    .optional()
    .context("load_local_license")
}

pub fn save_local_license(conn: &Connection, record: &LocalLicenseRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO local_license (
             id, customer_id, license_id, license_key, subscription_type, status,
             expires_at, last_validated_at, grace_period_days, highest_seen_clock, updated_at
         ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'))
         ON CONFLICT(id) DO UPDATE SET
             customer_id = excluded.customer_id,
             license_id = excluded.license_id,
             license_key = excluded.license_key,
             subscription_type = excluded.subscription_type,
             status = excluded.status,
             expires_at = excluded.expires_at,
             last_validated_at = excluded.last_validated_at,
             grace_period_days = excluded.grace_period_days,
             highest_seen_clock = excluded.highest_seen_clock,
             updated_at = datetime('now')",
        rusqlite::params![
            record.customer_id,
            record.license_id,
            record.license_key,
            record.subscription_type,
            record.status,
            record.expires_at.map(|d| d.to_rfc3339()),
            record.last_validated_at.map(|d| d.to_rfc3339()),
            record.grace_period_days,
            record.highest_seen_clock.map(|d| d.to_rfc3339()),
        ],
    )
    .context("save_local_license")?;
    Ok(())
}

/// Advances `highest_seen_clock` to `max(current stored value, now)` —
/// never allowed to move backward, regardless of what `now` is. This is the
/// clock-rollback watermark itself; see validation.rs's doc comment and
/// LICENSE_SECURITY_REVIEW.md §1. Safe to call even before any license is
/// activated (a no-op if `local_license` has no row yet, since there's
/// nothing meaningful to protect before activation).
pub fn advance_clock_watermark(conn: &Connection, now: DateTime<Utc>) -> Result<()> {
    let Some(mut record) = load_local_license(conn)? else {
        return Ok(());
    };
    let should_advance = match record.highest_seen_clock {
        Some(existing) => now > existing,
        None => true,
    };
    if should_advance {
        record.highest_seen_clock = Some(now);
        save_local_license(conn, &record)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub device_id: String,
    pub machine_fingerprint: String,
    pub fingerprint_inputs_json: String,
}

pub fn load_device_info(conn: &Connection) -> Result<Option<DeviceInfo>> {
    conn.query_row(
        "SELECT device_id, machine_fingerprint, fingerprint_inputs FROM device_info WHERE id = 1",
        [],
        |r| {
            Ok(DeviceInfo {
                device_id: r.get(0)?,
                machine_fingerprint: r.get(1)?,
                fingerprint_inputs_json: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        },
    )
    .optional()
    .context("load_device_info")
}

/// Returns this installation's device identity, generating and persisting
/// one on first call if none exists yet. Idempotent — every subsequent
/// call on the same database returns the same `device_id`
/// (LICENSE_SYSTEM_DESIGN.md §5: generated once, never regenerated).
pub fn get_or_create_device_info(conn: &Connection) -> Result<DeviceInfo> {
    if let Some(existing) = load_device_info(conn)? {
        return Ok(existing);
    }
    let inputs = FingerprintInputs::collect();
    let info = DeviceInfo {
        device_id: fingerprint::generate_device_id(),
        machine_fingerprint: inputs.hash(),
        fingerprint_inputs_json: inputs.to_json(),
    };
    conn.execute(
        "INSERT INTO device_info (id, device_id, machine_fingerprint, fingerprint_inputs)
         VALUES (1, ?1, ?2, ?3)",
        rusqlite::params![info.device_id, info.machine_fingerprint, info.fingerprint_inputs_json],
    )
    .context("get_or_create_device_info insert")?;
    Ok(info)
}

pub fn log_validation(conn: &Connection, result: &str, online: bool, detail: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO license_validation_log (result, online, detail) VALUES (?1, ?2, ?3)",
        rusqlite::params![result, if online { 1i64 } else { 0i64 }, detail],
    )
    .context("log_validation")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn open_migrated() -> Connection {
        crate::db::open(":memory:").expect("open in-memory db")
    }

    #[test]
    fn load_local_license_returns_none_before_any_activation() {
        let conn = open_migrated();
        assert_eq!(load_local_license(&conn).unwrap(), None);
    }

    #[test]
    fn save_then_load_round_trips_exactly() {
        let conn = open_migrated();
        let now = Utc::now();
        let record = LocalLicenseRecord {
            customer_id: Some("cus_1".to_string()),
            license_id: Some("lic_1".to_string()),
            license_key: Some("XXXX-XXXX-XXXX-XXXX".to_string()),
            subscription_type: Some("yearly".to_string()),
            status: "active".to_string(),
            expires_at: Some(now + Duration::days(365)),
            last_validated_at: Some(now),
            grace_period_days: 7,
            highest_seen_clock: Some(now),
        };
        save_local_license(&conn, &record).unwrap();
        let reloaded = load_local_license(&conn).unwrap().unwrap();
        assert_eq!(reloaded.customer_id, record.customer_id);
        assert_eq!(reloaded.license_id, record.license_id);
        assert_eq!(reloaded.status, record.status);
        assert_eq!(reloaded.grace_period_days, 7);
        // Timestamps round-trip through RFC3339 text — compare at second
        // precision since that's what's actually preserved.
        assert_eq!(
            reloaded.expires_at.unwrap().timestamp(),
            record.expires_at.unwrap().timestamp()
        );
    }

    #[test]
    fn save_local_license_upserts_not_duplicates() {
        let conn = open_migrated();
        save_local_license(&conn, &LocalLicenseRecord::not_activated()).unwrap();
        let mut second = LocalLicenseRecord::not_activated();
        second.status = "active".to_string();
        save_local_license(&conn, &second).unwrap();

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM local_license", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "must stay a single row (CHECK (id = 1) + upsert)");
        assert_eq!(load_local_license(&conn).unwrap().unwrap().status, "active");
    }

    #[test]
    fn get_or_create_device_info_is_idempotent() {
        let conn = open_migrated();
        let first = get_or_create_device_info(&conn).unwrap();
        let second = get_or_create_device_info(&conn).unwrap();
        assert_eq!(first.device_id, second.device_id, "must not regenerate on second call");
        assert_eq!(first.machine_fingerprint, second.machine_fingerprint);
    }

    #[test]
    fn advance_clock_watermark_never_moves_backward() {
        let conn = open_migrated();
        save_local_license(&conn, &LocalLicenseRecord::not_activated()).unwrap();
        let later = Utc::now() + Duration::days(10);
        advance_clock_watermark(&conn, later).unwrap();

        let earlier = Utc::now();
        advance_clock_watermark(&conn, earlier).unwrap();

        let record = load_local_license(&conn).unwrap().unwrap();
        assert_eq!(
            record.highest_seen_clock.unwrap().timestamp(),
            later.timestamp(),
            "watermark must stay at the later timestamp, not regress to the earlier one"
        );
    }

    #[test]
    fn log_validation_appends_rows() {
        let conn = open_migrated();
        log_validation(&conn, "Active", true, "server confirmed").unwrap();
        log_validation(&conn, "ActiveOfflineGrace", false, "3 days remaining").unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM license_validation_log", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 2);
    }
}
