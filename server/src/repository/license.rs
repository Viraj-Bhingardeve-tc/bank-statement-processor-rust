//! `licenses` table access (`LICENSE_DATABASE_SCHEMA.md` §1).

use crate::domain::{License, LicenseRecordStatus, NewLicense};
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
    /// The most recently issued non-revoked license for a subscription —
    /// used by `service::payment_service` to decide "extend this on
    /// renewal" vs. "issue a fresh one" (a subscription can, in principle,
    /// have more than one license over its life — `LICENSE_DATABASE_SCHEMA.md`
    /// §1's comment on `licenses`).
    async fn find_latest_by_subscription(
        &self,
        subscription_id: i64,
    ) -> Result<Option<License>, RepositoryError>;
    /// Issued on a successful payment, not on `/activate-license`.
    async fn insert(&self, new_license: NewLicense) -> Result<License, RepositoryError>;
    /// Updates status/expiry on a renewal or plan-status change — never
    /// touches `license_key`/`max_devices`/`grace_period_days`.
    async fn extend(
        &self,
        id: i64,
        status: LicenseRecordStatus,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(), RepositoryError>;
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

    async fn find_latest_by_subscription(
        &self,
        subscription_id: i64,
    ) -> Result<Option<License>, RepositoryError> {
        let row = sqlx::query_as::<_, LicenseRow>(
            "SELECT id, subscription_id, license_key, status, expires_at, max_devices, \
                    grace_period_days, issued_at, revoked_at, revoked_reason \
             FROM licenses WHERE subscription_id = $1 AND status != 'revoked' \
             ORDER BY issued_at DESC LIMIT 1",
        )
        .bind(subscription_id)
        .fetch_optional(&self.pool)
        .await?;

        row.map(License::try_from).transpose()
    }

    async fn insert(&self, new_license: NewLicense) -> Result<License, RepositoryError> {
        let row = sqlx::query_as::<_, LicenseRow>(
            "INSERT INTO licenses (subscription_id, license_key, status, expires_at, max_devices, grace_period_days) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             RETURNING id, subscription_id, license_key, status, expires_at, max_devices, \
                       grace_period_days, issued_at, revoked_at, revoked_reason",
        )
        .bind(new_license.subscription_id)
        .bind(&new_license.license_key)
        .bind(new_license.status.as_str())
        .bind(new_license.expires_at)
        .bind(new_license.max_devices)
        .bind(new_license.grace_period_days)
        .fetch_one(&self.pool)
        .await?;

        License::try_from(row)
    }

    async fn extend(
        &self,
        id: i64,
        status: LicenseRecordStatus,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE licenses SET status = $2, expires_at = $3 WHERE id = $1")
            .bind(id)
            .bind(status.as_str())
            .bind(expires_at)
            .execute(&self.pool)
            .await?;

        Ok(())
    }
}
