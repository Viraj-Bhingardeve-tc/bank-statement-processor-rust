//! Audit-log domain models — `login_history`/`license_validation_logs`
//! (`LICENSE_DATABASE_SCHEMA.md` §1, migrations `0005`/`0006`). Write-only
//! for now: `service::license_service`/`service::auth_service` produce
//! these, `repository::audit::AuditRepository` persists them. Nothing
//! reads them back yet (no admin API exists), so there's no corresponding
//! "stored row" domain type — only the `New*` shape each insert needs.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// One `login_history` row. `user_id` is a real, already-resolved account
/// id — see `AuditRepository::record_login`'s doc comment for why a login
/// attempt against an unrecognized email is never represented here at all.
///
/// `login_history.ip_address` (`INET`, migration `0005`) has no field here:
/// `sqlx`'s Postgres driver needs the `ipnetwork` feature/crate to bind
/// `std::net::IpAddr` against an `INET` column, and this codebase pulls in
/// neither today. `repository::audit::PgAuditRepository::record_login`
/// writes a literal `NULL` for that column until a later module both adds
/// that dependency and threads a real client IP down from the HTTP layer.
#[derive(Debug, Clone, PartialEq)]
pub struct NewLoginHistoryEntry {
    pub user_id: i64,
    pub device_id: Option<Uuid>,
    pub success: bool,
}

/// The five outcomes `license_validation_logs.result` accepts (its `CHECK`
/// constraint, migration `0006`) — matches `LICENSE_DATABASE_SCHEMA.md` §1
/// exactly. Deliberately narrower than
/// `service::license_service::LicenseOperationError`: variants with no
/// resolved `license_id` to attribute a row to (`LicenseNotFound`) or that
/// aren't a license-state result at all (`DeviceLimitReached`, a capacity
/// error) have no case here — see migration `0006`'s own doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationLogResult {
    Valid,
    Expired,
    Suspended,
    Revoked,
    DeviceMismatch,
}

impl ValidationLogResult {
    pub fn as_str(&self) -> &'static str {
        match self {
            ValidationLogResult::Valid => "valid",
            ValidationLogResult::Expired => "expired",
            ValidationLogResult::Suspended => "suspended",
            ValidationLogResult::Revoked => "revoked",
            ValidationLogResult::DeviceMismatch => "device_mismatch",
        }
    }
}

/// One `license_validation_logs` row. Like [`NewLoginHistoryEntry`], has no
/// `ip_address` field for the same `sqlx`/`ipnetwork` reason. `client_clock`
/// is part of the original schema design (for future clock-rollback
/// detection, `LICENSE_SECURITY_REVIEW.md`) but always `None` for now —
/// populating it would require threading the client's claimed timestamp
/// from the HTTP request down into `LicenseService`, whose methods are
/// deliberately HTTP-framework-agnostic (`PHASE4_DESIGN.md` §1.2); out of
/// scope for this module, which only appends audit calls using values those
/// methods already compute.
#[derive(Debug, Clone, PartialEq)]
pub struct NewLicenseValidationLogEntry {
    pub license_id: i64,
    pub device_id: Uuid,
    pub result: ValidationLogResult,
    pub client_clock: Option<DateTime<Utc>>,
}
