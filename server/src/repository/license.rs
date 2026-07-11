//! `licenses` table access (`LICENSE_DATABASE_SCHEMA.md` §1).

use crate::domain::{License, LicenseRecordStatus};
use crate::repository::error::RepositoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use std::str::FromStr;

#[async_trait]
pub trait LicenseRepository: Send + Sync {
    /// Looks up the customer-facing activation code (`POST /activate-license`).
    async fn find_by_key(&self, license_key: &str) -> Result<Option<License>, RepositoryError>;
    async fn find_by_id(&self, id: i64) -> Result<Option<License>, RepositoryError>;
}

pub struct PgLicenseRepository {
    pool: PgPool,
}

impl PgLicenseRepository {
    pub fn new(pool: PgPool) -> Self {
        PgLicenseRepository { pool }
    }
}

#[derive(sqlx::FromRow)]
struct LicenseRow {
    id: i64,
    subscription_id: i64,
    license_key: String,
    status: String,
    expires_at: Option<DateTime<Utc>>,
    max_devices: i32,
    grace_period_days: i32,
    issued_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
    revoked_reason: Option<String>,
}

impl TryFrom<LicenseRow> for License {
    type Error = RepositoryError;

    fn try_from(row: LicenseRow) -> Result<Self, Self::Error> {
        Ok(License {
            id: row.id,
            subscription_id: row.subscription_id,
            license_key: row.license_key,
            status: LicenseRecordStatus::from_str(&row.status)
                .map_err(RepositoryError::InvalidData)?,
            expires_at: row.expires_at,
            max_devices: row.max_devices,
            grace_period_days: row.grace_period_days,
            issued_at: row.issued_at,
            revoked_at: row.revoked_at,
            revoked_reason: row.revoked_reason,
        })
    }
}

#[async_trait]
impl LicenseRepository for PgLicenseRepository {
    async fn find_by_key(&self, license_key: &str) -> Result<Option<License>, RepositoryError> {
        let row = sqlx::query_as::<_, LicenseRow>(
            "SELECT id, subscription_id, license_key, status, expires_at, max_devices, \
                    grace_period_days, issued_at, revoked_at, revoked_reason \
             FROM licenses WHERE license_key = $1",
        )
        .bind(license_key)
        .fetch_optional(&self.pool)
        .await?;

        row.map(License::try_from).transpose()
    }

    async fn find_by_id(&self, id: i64) -> Result<Option<License>, RepositoryError> {
        let row = sqlx::query_as::<_, LicenseRow>(
            "SELECT id, subscription_id, license_key, status, expires_at, max_devices, \
                    grace_period_days, issued_at, revoked_at, revoked_reason \
             FROM licenses WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(License::try_from).transpose()
    }
}
