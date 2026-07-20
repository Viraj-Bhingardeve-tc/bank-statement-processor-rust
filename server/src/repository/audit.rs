//! `login_history`/`license_validation_logs` table access
//! (`LICENSE_DATABASE_SCHEMA.md` §1, migrations `0005`/`0006`). Insert-only
//! — nothing reads these back yet (no admin API exists), so this trait has
//! no `find_*`/`list_*` methods, unlike every other repository in this
//! module.
//!
//! Called from `service::audit_service::AuditService`, itself called from
//! `service::auth_service::AuthService::login` and
//! `service::license_service::LicenseService::{activate,validate,heartbeat}`
//! — always fire-and-forget (`tokio::spawn`, see `AuditService`'s own doc
//! comment), never awaited on the request's critical path, so a slow or
//! failing write here can never add latency to, or fail, the request it's
//! recording.

use crate::domain::{NewLicenseValidationLogEntry, NewLoginHistoryEntry};
use crate::repository::error::RepositoryError;
use async_trait::async_trait;
use sqlx::PgPool;

#[async_trait]
pub trait AuditRepository: Send + Sync {
    /// Inserts one `login_history` row. Only ever called for an attempt
    /// against a real, already-resolved `users` row — `user_id` is
    /// `NOT NULL REFERENCES users(id)` (migration `0005`), so there is
    /// nothing valid to insert for an attempt against an unrecognized
    /// email; callers must not invoke this for that case.
    async fn record_login(&self, entry: NewLoginHistoryEntry) -> Result<(), RepositoryError>;

    /// Inserts one `license_validation_logs` row. `entry.license_id` must
    /// reference a real `licenses` row (`NOT NULL REFERENCES licenses(id)`,
    /// migration `0006`); `entry.device_id` is a plain UUID, not a foreign
    /// key, so an unrecognized device is still recordable.
    async fn record_validation(
        &self,
        entry: NewLicenseValidationLogEntry,
    ) -> Result<(), RepositoryError>;
}

pub struct PgAuditRepository {
    pool: PgPool,
}

impl PgAuditRepository {
    pub fn new(pool: PgPool) -> Self {
        PgAuditRepository { pool }
    }
}

#[async_trait]
impl AuditRepository for PgAuditRepository {
    async fn record_login(&self, entry: NewLoginHistoryEntry) -> Result<(), RepositoryError> {
        // `ip_address` is a literal `NULL`, not a bound parameter — see
        // `domain::audit::NewLoginHistoryEntry`'s doc comment for why that
        // field doesn't exist yet.
        sqlx::query(
            "INSERT INTO login_history (user_id, device_id, ip_address, success) \
             VALUES ($1, $2, NULL, $3)",
        )
        .bind(entry.user_id)
        .bind(entry.device_id)
        .bind(entry.success)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn record_validation(
        &self,
        entry: NewLicenseValidationLogEntry,
    ) -> Result<(), RepositoryError> {
        // Same `ip_address` note as `record_login` above.
        sqlx::query(
            "INSERT INTO license_validation_logs \
                (license_id, device_id, result, ip_address, client_clock) \
             VALUES ($1, $2, $3, NULL, $4)",
        )
        .bind(entry.license_id)
        .bind(entry.device_id)
        .bind(entry.result.as_str())
        .bind(entry.client_clock)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Always-`Ok`, writes-nothing stand-in for tests — `service::auth_service`
/// and `service::license_service`'s existing test suites construct their
/// service under test with this so `AuditService` has something to hold
/// without pulling a real database into tests that were never about
/// auditing in the first place. Mirrors the `Mock*Repository` pattern
/// those same test modules already use for their other repository
/// dependencies.
#[cfg(test)]
pub(crate) struct NoopAuditRepository;

#[cfg(test)]
#[async_trait]
impl AuditRepository for NoopAuditRepository {
    async fn record_login(&self, _entry: NewLoginHistoryEntry) -> Result<(), RepositoryError> {
        Ok(())
    }

    async fn record_validation(
        &self,
        _entry: NewLicenseValidationLogEntry,
    ) -> Result<(), RepositoryError> {
        Ok(())
    }
}
