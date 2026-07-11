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

    async fn insert(&self, new_device: NewDevice) -> Result<Device, RepositoryError>;
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
}
