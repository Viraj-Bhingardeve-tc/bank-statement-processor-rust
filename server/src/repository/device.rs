//! `devices` table access (`LICENSE_DATABASE_SCHEMA.md` §1).

use crate::domain::{Device, NewDevice};
use crate::repository::error::RepositoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

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

    /// The full active-device list, for `409 DEVICE_LIMIT_REACHED`'s
    /// response (`API_SPECIFICATION.md`: "response includes the existing
    /// device list so the customer/admin can deactivate one").
    async fn list_active_by_license(&self, license_id: i64)
        -> Result<Vec<Device>, RepositoryError>;

    async fn insert(&self, new_device: NewDevice) -> Result<Device, RepositoryError>;

    /// Bumps `last_seen_at` to now — called on every successful
    /// `/validate-license` (and, later, `/heartbeat`) for an already-
    /// activated device.
    async fn touch_last_seen(&self, id: i64) -> Result<(), RepositoryError>;

    /// Clears `deactivated_at` and refreshes `last_seen_at` — used when
    /// `/activate-license` is called again for a device that was
    /// previously deactivated on this same license (re-activation reuses
    /// the existing row rather than inserting a second one, since
    /// `(license_id, device_id)` is unique).
    async fn reactivate(&self, id: i64) -> Result<(), RepositoryError>;

    /// Sets `deactivated_at` — `/deactivate-license`. Soft-delete, never a
    /// row removal, same reasoning as every other status transition in
    /// this schema.
    async fn deactivate(&self, id: i64) -> Result<(), RepositoryError>;
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

    async fn list_active_by_license(
        &self,
        license_id: i64,
    ) -> Result<Vec<Device>, RepositoryError> {
        let rows = sqlx::query_as::<_, DeviceRow>(
            "SELECT id, license_id, device_id, machine_fingerprint, device_label, \
                    first_seen_at, last_seen_at, deactivated_at \
             FROM devices WHERE license_id = $1 AND deactivated_at IS NULL \
             ORDER BY first_seen_at",
        )
        .bind(license_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Device::from).collect())
    }

    async fn insert(&self, new_device: NewDevice) -> Result<Device, RepositoryError> {
        let row = sqlx::query_as::<_, DeviceRow>(
            "INSERT INTO devices (license_id, device_id, machine_fingerprint, device_label) \
             VALUES ($1, $2, $3, $4) \
             RETURNING id, license_id, device_id, machine_fingerprint, device_label, \
                       first_seen_at, last_seen_at, deactivated_at",
        )
        .bind(new_device.license_id)
        .bind(new_device.device_id)
        .bind(&new_device.machine_fingerprint)
        .bind(&new_device.device_label)
        .fetch_one(&self.pool)
        .await?;

        Ok(Device::from(row))
    }

    async fn touch_last_seen(&self, id: i64) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE devices SET last_seen_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn reactivate(&self, id: i64) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE devices SET deactivated_at = NULL, last_seen_at = now() WHERE id = $1")
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
}
