//! `devices` table access (`LICENSE_DATABASE_SCHEMA.md` §1).
//!
//! **Phase 4J.3 fix (production readiness audit, HIGH finding #3):**
//! `/activate-license` used to check `count_active_by_license` and only
//! `insert`/`reactivate` a device row afterward — a check-then-act race:
//! two concurrent activations for *different* `device_id`s on the same
//! license could both observe a free slot before either had written
//! anything, together exceeding `max_devices`. `activate_device` below
//! closes that race by performing the whole check-then-act sequence
//! atomically in one transaction.

use crate::domain::Device;
use crate::repository::error::RepositoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// The outcome of atomically resolving `/activate-license` for a given
/// `(license_id, device_id)` pair against the current device count.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceActivationOutcome {
    /// The device is now active on this license — already active (just
    /// refreshed), reactivated from a previously-deactivated row, or
    /// freshly inserted.
    Activated,
    /// Activating this device would have exceeded `max_devices`; nothing
    /// was mutated. Carries the current active-device list for the
    /// `409 DEVICE_LIMIT_REACHED` response (`API_SPECIFICATION.md`:
    /// "response includes the existing device list so the customer/admin
    /// can deactivate one").
    LimitReached(Vec<Device>),
}

#[async_trait]
pub trait DeviceRepository: Send + Sync {
    async fn find_by_license_and_device_id(
        &self,
        license_id: i64,
        device_id: Uuid,
    ) -> Result<Option<Device>, RepositoryError>;

    /// Devices with `deactivated_at IS NULL` — what `max_devices` is
    /// actually checked against on `/activate-license`
    /// (`LICENSE_DATABASE_SCHEMA.md` §1's comment on `licenses.max_devices`).
    async fn count_active_by_license(&self, license_id: i64) -> Result<i64, RepositoryError>;

    /// Bumps `last_seen_at` to now — called on every successful
    /// `/validate-license` and `/heartbeat` for an already-activated
    /// device.
    async fn touch_last_seen(&self, id: i64) -> Result<(), RepositoryError>;

    /// Sets `deactivated_at` — `/deactivate-license`. Soft-delete, never a
    /// row removal, same reasoning as every other status transition in
    /// this schema.
    async fn deactivate(&self, id: i64) -> Result<(), RepositoryError>;

    /// `/activate-license`'s entire decide-then-mutate step, atomically:
    /// locks the `licenses` row for `license_id` first (always exists,
    /// unlike a `devices` row for a brand-new license with zero prior
    /// activations — locking only `devices` rows would miss exactly that
    /// case, since a `SELECT ... FOR UPDATE` over zero rows locks
    /// nothing), then locks every existing `devices` row for this license
    /// too, then — still inside the same transaction — decides whether
    /// `device_id` is already active (idempotent refresh), can reuse a
    /// deactivated row, needs a fresh row, or would exceed `max_devices`,
    /// and either performs exactly that one mutation and commits, or
    /// rolls back and returns `LimitReached`. A concurrent call for a
    /// *different* `device_id` on the same license blocks on the license-
    /// row lock until this transaction ends, so it always re-evaluates
    /// against the true, post-commit device count rather than a stale one.
    async fn activate_device(
        &self,
        license_id: i64,
        max_devices: i32,
        device_id: Uuid,
        machine_fingerprint: &str,
        device_label: &str,
    ) -> Result<DeviceActivationOutcome, RepositoryError>;
}

pub struct PgDeviceRepository {
    pool: PgPool,
}

impl PgDeviceRepository {
    pub fn new(pool: PgPool) -> Self {
        PgDeviceRepository { pool }
    }
}

#[derive(sqlx::FromRow)]
struct DeviceRow {
    id: i64,
    license_id: i64,
    device_id: Uuid,
    machine_fingerprint: String,
    device_label: Option<String>,
    first_seen_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    deactivated_at: Option<DateTime<Utc>>,
}

impl From<DeviceRow> for Device {
    fn from(row: DeviceRow) -> Self {
        Device {
            id: row.id,
            license_id: row.license_id,
            device_id: row.device_id,
            machine_fingerprint: row.machine_fingerprint,
            device_label: row.device_label,
            first_seen_at: row.first_seen_at,
            last_seen_at: row.last_seen_at,
            deactivated_at: row.deactivated_at,
        }
    }
}

#[async_trait]
impl DeviceRepository for PgDeviceRepository {
    async fn find_by_license_and_device_id(
        &self,
        license_id: i64,
        device_id: Uuid,
    ) -> Result<Option<Device>, RepositoryError> {
        let row = sqlx::query_as::<_, DeviceRow>(
            "SELECT id, license_id, device_id, machine_fingerprint, device_label, \
                    first_seen_at, last_seen_at, deactivated_at \
             FROM devices WHERE license_id = $1 AND device_id = $2",
        )
        .bind(license_id)
        .bind(device_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Device::from))
    }

    async fn count_active_by_license(&self, license_id: i64) -> Result<i64, RepositoryError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM devices WHERE license_id = $1 AND deactivated_at IS NULL",
        )
        .bind(license_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(count)
    }

    async fn touch_last_seen(&self, id: i64) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE devices SET last_seen_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn deactivate(&self, id: i64) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE devices SET deactivated_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn activate_device(
        &self,
        license_id: i64,
        max_devices: i32,
        device_id: Uuid,
        machine_fingerprint: &str,
        device_label: &str,
    ) -> Result<DeviceActivationOutcome, RepositoryError> {
        let mut tx = self.pool.begin().await?;

        // Lock the license row first — it always exists (the caller
        // already looked it up by key/id moments earlier), unlike a
        // `devices` row on a brand-new license with zero prior
        // activations. This is the actual serialization primitive: a
        // concurrent `activate_device` call for a *different* device_id
        // on the same license_id blocks here until this transaction
        // commits or rolls back, so it can never observe a stale count.
        sqlx::query("SELECT id FROM licenses WHERE id = $1 FOR UPDATE")
            .bind(license_id)
            .fetch_one(&mut *tx)
            .await?;

        // Also lock every existing `devices` row for this license — not
        // required for correctness once the license row above is locked,
        // but keeps the read below from racing a concurrent transaction
        // that (incorrectly) only locks devices rows, and matches the
        // audit's own stated remediation shape.
        let rows = sqlx::query_as::<_, DeviceRow>(
            "SELECT id, license_id, device_id, machine_fingerprint, device_label, \
                    first_seen_at, last_seen_at, deactivated_at \
             FROM devices WHERE license_id = $1 FOR UPDATE",
        )
        .bind(license_id)
        .fetch_all(&mut *tx)
        .await?;
        let devices: Vec<Device> = rows.into_iter().map(Device::from).collect();

        let existing = devices.iter().find(|d| d.device_id == device_id);

        if let Some(existing) = existing {
            if existing.deactivated_at.is_none() {
                // Already active — idempotent refresh, no slot consumed.
                sqlx::query("UPDATE devices SET last_seen_at = now() WHERE id = $1")
                    .bind(existing.id)
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await?;
                return Ok(DeviceActivationOutcome::Activated);
            }

            // Reactivating a previously-deactivated row still needs a
            // free slot, evaluated against the now-locked device set.
            let active_count = devices
                .iter()
                .filter(|d| d.deactivated_at.is_none())
                .count() as i32;
            if active_count >= max_devices {
                let active = devices
                    .into_iter()
                    .filter(|d| d.deactivated_at.is_none())
                    .collect();
                tx.rollback().await?;
                return Ok(DeviceActivationOutcome::LimitReached(active));
            }

            sqlx::query(
                "UPDATE devices SET deactivated_at = NULL, last_seen_at = now() WHERE id = $1",
            )
            .bind(existing.id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(DeviceActivationOutcome::Activated);
        }

        // No row for this device — a fresh insert also needs a free slot.
        let active_count = devices
            .iter()
            .filter(|d| d.deactivated_at.is_none())
            .count() as i32;
        if active_count >= max_devices {
            let active = devices
                .into_iter()
                .filter(|d| d.deactivated_at.is_none())
                .collect();
            tx.rollback().await?;
            return Ok(DeviceActivationOutcome::LimitReached(active));
        }

        sqlx::query(
            "INSERT INTO devices (license_id, device_id, machine_fingerprint, device_label) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(license_id)
        .bind(device_id)
        .bind(machine_fingerprint)
        .bind(device_label)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(DeviceActivationOutcome::Activated)
    }
}
