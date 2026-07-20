//! Read/write access for the Admin API (Module 3) — every `GET /admin/*`
//! list query (`users`, `licenses`, `devices`, `login_history`,
//! `license_validation_logs`), plus the two device-row mutations
//! (`revoke`/`restore` license, `deactivate`/`activate` device) reuse
//! `LicenseRepository`/`DeviceRepository` where those already do exactly
//! what's needed (see `service::admin_service`'s doc comment for which),
//! but the ones below have no equivalent anywhere else in this crate:
//!
//! - `list_*`: every other repository's queries are single-row lookups by
//!   an already-known key (`find_by_id`, `find_by_email`, ...); nothing
//!   before this module ever needed a paginated, filterable, joined *list*
//!   query. `AuditRepository` in particular is explicitly documented as
//!   "insert-only... no find_*/list_* methods" — rather than widen that
//!   trait (a completed module, Module 1), the two read queries over
//!   `login_history`/`license_validation_logs` this module needs live here
//!   instead, against the same tables.
//! - `revoke_license`/`restore_license`: `LicenseRepository::extend` only
//!   ever touches `status`/`expires_at` (every existing caller in
//!   `service::payment_service` already has its own reason for a status
//!   change tracked elsewhere). An admin revoke is the first code path
//!   with an actual human-supplied reason to record, so these are the
//!   first two writers of `licenses.revoked_at`/`revoked_reason` — columns
//!   the original schema always had but nothing ever populated.
//! - `find_device_by_id`/`reactivate_device`: `DeviceRepository` has
//!   `deactivate(id)` (reused directly by `service::admin_service`) but no
//!   inverse — every existing activation path is `activate_device`,
//!   customer-facing, keyed by `(license_id, device_id: Uuid,
//!   fingerprint)` plus a `max_devices` check, not "flip this already-known
//!   row's `deactivated_at` back to `NULL`". An admin reactivating a
//!   specific device they can already see in `GET /admin/devices`
//!   deliberately bypasses that limit check — a trusted override action,
//!   not a second customer-facing activation path.
//!
//! Every list query uses `sqlx::QueryBuilder` (not the compile-time
//! `query!`/`query_as!` macros — no other repository in this crate uses
//! them either, since there's no `.sqlx` prepare cache or build-time
//! `DATABASE_URL` anywhere in this project) because each one has a
//! variable number of optional `WHERE` conditions; every filter value is
//! still passed through `.push_bind`, never string-interpolated, so this
//! is exactly as injection-safe as every other repository's static
//! `.bind()` calls.

use crate::domain::{
    AdminDeviceSummary, AdminLicenseSummary, AdminUserSummary, DeviceListFilter, LicenseListFilter,
    LicenseRecordStatus, LicenseValidationEntry, LicenseValidationFilter, LoginHistoryEntry,
    LoginHistoryFilter, Page, PlanType, SortOrder, SubscriptionStatus, UserListFilter, UserRole,
    ValidationLogResult,
};
use crate::repository::error::RepositoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder};
use std::str::FromStr;
use uuid::Uuid;

#[async_trait]
pub trait AdminRepository: Send + Sync {
    async fn list_users(
        &self,
        filter: &UserListFilter,
    ) -> Result<Page<AdminUserSummary>, RepositoryError>;

    async fn list_licenses(
        &self,
        filter: &LicenseListFilter,
    ) -> Result<Page<AdminLicenseSummary>, RepositoryError>;

    async fn list_devices(
        &self,
        filter: &DeviceListFilter,
    ) -> Result<Page<AdminDeviceSummary>, RepositoryError>;

    /// Existence check + read for `POST /admin/device/{id}/activate` and
    /// `.../deactivate` — `DeviceRepository::deactivate`/this module's own
    /// `reactivate_device` are both blind `UPDATE ... WHERE id = $1`s that
    /// silently affect zero rows for an unknown id; `service::admin_service`
    /// calls this first so an unknown device id 404s instead.
    async fn find_device_by_id(
        &self,
        id: i64,
    ) -> Result<Option<AdminDeviceSummary>, RepositoryError>;

    /// Sets `deactivated_at = NULL` — the inverse of
    /// `DeviceRepository::deactivate`, with no `max_devices` check (see
    /// this module's own doc comment for why).
    async fn reactivate_device(&self, id: i64) -> Result<(), RepositoryError>;

    async fn list_login_history(
        &self,
        filter: &LoginHistoryFilter,
    ) -> Result<Page<LoginHistoryEntry>, RepositoryError>;

    async fn list_license_validations(
        &self,
        filter: &LicenseValidationFilter,
    ) -> Result<Page<LicenseValidationEntry>, RepositoryError>;

    /// Sets `status = 'revoked'`, `revoked_at = now()`, and
    /// `revoked_reason` — idempotent (safe to call on an
    /// already-revoked license). `service::admin_service` looks the
    /// license up via `LicenseRepository::find_by_id` first so an unknown
    /// `license_id` 404s before this ever runs.
    async fn revoke_license(&self, id: i64, reason: Option<&str>) -> Result<(), RepositoryError>;

    /// Sets `status = 'active'` and clears `revoked_at`/`revoked_reason`.
    /// `service::admin_service` only calls this after confirming the
    /// license is currently `Revoked`, so this itself doesn't re-check.
    async fn restore_license(&self, id: i64) -> Result<(), RepositoryError>;
}

pub struct PgAdminRepository {
    pool: PgPool,
}

impl PgAdminRepository {
    pub fn new(pool: PgPool) -> Self {
        PgAdminRepository { pool }
    }
}

#[derive(sqlx::FromRow)]
struct AdminUserRow {
    id: i64,
    email: String,
    company_name: Option<String>,
    role: String,
    subscription_status: Option<String>,
    created_at: DateTime<Utc>,
}

impl TryFrom<AdminUserRow> for AdminUserSummary {
    type Error = RepositoryError;

    fn try_from(row: AdminUserRow) -> Result<Self, Self::Error> {
        Ok(AdminUserSummary {
            id: row.id,
            email: row.email,
            company_name: row.company_name,
            role: UserRole::from_str(&row.role).map_err(RepositoryError::InvalidData)?,
            subscription_status: row
                .subscription_status
                .map(|s| SubscriptionStatus::from_str(&s))
                .transpose()
                .map_err(RepositoryError::InvalidData)?,
            created_at: row.created_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct AdminLicenseRow {
    id: i64,
    license_key: String,
    status: String,
    plan_type: String,
    expires_at: Option<DateTime<Utc>>,
    max_devices: i32,
    issued_at: DateTime<Utc>,
    user_id: i64,
}

impl TryFrom<AdminLicenseRow> for AdminLicenseSummary {
    type Error = RepositoryError;

    fn try_from(row: AdminLicenseRow) -> Result<Self, Self::Error> {
        Ok(AdminLicenseSummary {
            id: row.id,
            license_key: row.license_key,
            status: LicenseRecordStatus::from_str(&row.status)
                .map_err(RepositoryError::InvalidData)?,
            plan_type: PlanType::from_str(&row.plan_type).map_err(RepositoryError::InvalidData)?,
            expires_at: row.expires_at,
            max_devices: row.max_devices,
            issued_at: row.issued_at,
            user_id: row.user_id,
        })
    }
}

#[derive(sqlx::FromRow)]
struct AdminDeviceRow {
    id: i64,
    license_id: i64,
    user_id: i64,
    device_id: Uuid,
    device_label: Option<String>,
    last_seen_at: DateTime<Utc>,
    deactivated_at: Option<DateTime<Utc>>,
}

impl From<AdminDeviceRow> for AdminDeviceSummary {
    fn from(row: AdminDeviceRow) -> Self {
        AdminDeviceSummary {
            id: row.id,
            license_id: row.license_id,
            user_id: row.user_id,
            device_id: row.device_id,
            device_label: row.device_label,
            last_seen_at: row.last_seen_at,
            is_active: row.deactivated_at.is_none(),
        }
    }
}

#[derive(sqlx::FromRow)]
struct LoginHistoryRow {
    id: i64,
    user_id: i64,
    device_id: Option<Uuid>,
    success: bool,
    created_at: DateTime<Utc>,
}

impl From<LoginHistoryRow> for LoginHistoryEntry {
    fn from(row: LoginHistoryRow) -> Self {
        LoginHistoryEntry {
            id: row.id,
            user_id: row.user_id,
            device_id: row.device_id,
            success: row.success,
            created_at: row.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct LicenseValidationRow {
    id: i64,
    license_id: i64,
    device_id: Uuid,
    result: String,
    created_at: DateTime<Utc>,
}

impl TryFrom<LicenseValidationRow> for LicenseValidationEntry {
    type Error = RepositoryError;

    fn try_from(row: LicenseValidationRow) -> Result<Self, Self::Error> {
        Ok(LicenseValidationEntry {
            id: row.id,
            license_id: row.license_id,
            device_id: row.device_id,
            result: ValidationLogResult::from_str(&row.result)
                .map_err(RepositoryError::InvalidData)?,
            created_at: row.created_at,
        })
    }
}

#[async_trait]
impl AdminRepository for PgAdminRepository {
    async fn list_users(
        &self,
        filter: &UserListFilter,
    ) -> Result<Page<AdminUserSummary>, RepositoryError> {
        let mut count_qb: QueryBuilder<Postgres> =
            QueryBuilder::new("SELECT COUNT(*) FROM users u WHERE 1=1");
        push_user_filters(&mut count_qb, filter);
        let total: i64 = count_qb.build_query_scalar().fetch_one(&self.pool).await?;

        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            "SELECT u.id, u.email, u.company_name, u.role, \
                    (SELECT s.status FROM subscriptions s \
                     WHERE s.user_id = u.id ORDER BY s.created_at DESC LIMIT 1) AS subscription_status, \
                    u.created_at \
             FROM users u WHERE 1=1",
        );
        push_user_filters(&mut qb, filter);
        qb.push(match filter.sort_order {
            SortOrder::Ascending => " ORDER BY u.created_at ASC",
            SortOrder::Descending => " ORDER BY u.created_at DESC",
        });
        qb.push(" LIMIT ")
            .push_bind(filter.pagination.limit())
            .push(" OFFSET ")
            .push_bind(filter.pagination.offset());

        let rows = qb
            .build_query_as::<AdminUserRow>()
            .fetch_all(&self.pool)
            .await?;
        let items = rows
            .into_iter()
            .map(AdminUserSummary::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Page {
            items,
            page: filter.pagination.page,
            page_size: filter.pagination.page_size,
            total,
        })
    }

    async fn list_licenses(
        &self,
        filter: &LicenseListFilter,
    ) -> Result<Page<AdminLicenseSummary>, RepositoryError> {
        const FROM: &str =
            " FROM licenses l JOIN subscriptions s ON s.id = l.subscription_id WHERE 1=1";

        let mut count_qb: QueryBuilder<Postgres> =
            QueryBuilder::new(format!("SELECT COUNT(*){FROM}"));
        push_license_filters(&mut count_qb, filter);
        let total: i64 = count_qb.build_query_scalar().fetch_one(&self.pool).await?;

        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
            "SELECT l.id, l.license_key, l.status, s.plan_type, l.expires_at, \
                    l.max_devices, l.issued_at, s.user_id{FROM}"
        ));
        push_license_filters(&mut qb, filter);
        qb.push(" ORDER BY l.issued_at DESC LIMIT ")
            .push_bind(filter.pagination.limit())
            .push(" OFFSET ")
            .push_bind(filter.pagination.offset());

        let rows = qb
            .build_query_as::<AdminLicenseRow>()
            .fetch_all(&self.pool)
            .await?;
        let items = rows
            .into_iter()
            .map(AdminLicenseSummary::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Page {
            items,
            page: filter.pagination.page,
            page_size: filter.pagination.page_size,
            total,
        })
    }

    async fn list_devices(
        &self,
        filter: &DeviceListFilter,
    ) -> Result<Page<AdminDeviceSummary>, RepositoryError> {
        const FROM: &str = " FROM devices d \
             JOIN licenses l ON l.id = d.license_id \
             JOIN subscriptions s ON s.id = l.subscription_id \
             WHERE 1=1";

        let mut count_qb: QueryBuilder<Postgres> =
            QueryBuilder::new(format!("SELECT COUNT(*){FROM}"));
        push_device_filters(&mut count_qb, filter);
        let total: i64 = count_qb.build_query_scalar().fetch_one(&self.pool).await?;

        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(format!(
            "SELECT d.id, d.license_id, s.user_id, d.device_id, d.device_label, \
                    d.last_seen_at, d.deactivated_at{FROM}"
        ));
        push_device_filters(&mut qb, filter);
        qb.push(" ORDER BY d.last_seen_at DESC LIMIT ")
            .push_bind(filter.pagination.limit())
            .push(" OFFSET ")
            .push_bind(filter.pagination.offset());

        let rows = qb
            .build_query_as::<AdminDeviceRow>()
            .fetch_all(&self.pool)
            .await?;
        let items = rows.into_iter().map(AdminDeviceSummary::from).collect();

        Ok(Page {
            items,
            page: filter.pagination.page,
            page_size: filter.pagination.page_size,
            total,
        })
    }

    async fn find_device_by_id(
        &self,
        id: i64,
    ) -> Result<Option<AdminDeviceSummary>, RepositoryError> {
        let row = sqlx::query_as::<_, AdminDeviceRow>(
            "SELECT d.id, d.license_id, s.user_id, d.device_id, d.device_label, \
                    d.last_seen_at, d.deactivated_at \
             FROM devices d \
             JOIN licenses l ON l.id = d.license_id \
             JOIN subscriptions s ON s.id = l.subscription_id \
             WHERE d.id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(AdminDeviceSummary::from))
    }

    async fn reactivate_device(&self, id: i64) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE devices SET deactivated_at = NULL WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_login_history(
        &self,
        filter: &LoginHistoryFilter,
    ) -> Result<Page<LoginHistoryEntry>, RepositoryError> {
        let mut count_qb: QueryBuilder<Postgres> =
            QueryBuilder::new("SELECT COUNT(*) FROM login_history lh WHERE 1=1");
        push_login_history_filters(&mut count_qb, filter);
        let total: i64 = count_qb.build_query_scalar().fetch_one(&self.pool).await?;

        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            "SELECT lh.id, lh.user_id, lh.device_id, lh.success, lh.created_at \
             FROM login_history lh WHERE 1=1",
        );
        push_login_history_filters(&mut qb, filter);
        qb.push(" ORDER BY lh.created_at DESC LIMIT ")
            .push_bind(filter.pagination.limit())
            .push(" OFFSET ")
            .push_bind(filter.pagination.offset());

        let rows = qb
            .build_query_as::<LoginHistoryRow>()
            .fetch_all(&self.pool)
            .await?;
        let items = rows.into_iter().map(LoginHistoryEntry::from).collect();

        Ok(Page {
            items,
            page: filter.pagination.page,
            page_size: filter.pagination.page_size,
            total,
        })
    }

    async fn list_license_validations(
        &self,
        filter: &LicenseValidationFilter,
    ) -> Result<Page<LicenseValidationEntry>, RepositoryError> {
        let mut count_qb: QueryBuilder<Postgres> =
            QueryBuilder::new("SELECT COUNT(*) FROM license_validation_logs lvl WHERE 1=1");
        push_license_validation_filters(&mut count_qb, filter);
        let total: i64 = count_qb.build_query_scalar().fetch_one(&self.pool).await?;

        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
            "SELECT lvl.id, lvl.license_id, lvl.device_id, lvl.result, lvl.created_at \
             FROM license_validation_logs lvl WHERE 1=1",
        );
        push_license_validation_filters(&mut qb, filter);
        qb.push(" ORDER BY lvl.created_at DESC LIMIT ")
            .push_bind(filter.pagination.limit())
            .push(" OFFSET ")
            .push_bind(filter.pagination.offset());

        let rows = qb
            .build_query_as::<LicenseValidationRow>()
            .fetch_all(&self.pool)
            .await?;
        let items = rows
            .into_iter()
            .map(LicenseValidationEntry::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Page {
            items,
            page: filter.pagination.page,
            page_size: filter.pagination.page_size,
            total,
        })
    }

    async fn revoke_license(&self, id: i64, reason: Option<&str>) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE licenses SET status = 'revoked', revoked_at = now(), revoked_reason = $2 \
             WHERE id = $1",
        )
        .bind(id)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn restore_license(&self, id: i64) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE licenses SET status = 'active', revoked_at = NULL, revoked_reason = NULL \
             WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn push_user_filters(qb: &mut QueryBuilder<Postgres>, filter: &UserListFilter) {
    if let Some(search) = &filter.search {
        let pattern = format!("%{search}%");
        qb.push(" AND (u.email ILIKE ")
            .push_bind(pattern.clone())
            .push(" OR u.company_name ILIKE ")
            .push_bind(pattern)
            .push(")");
    }
}

fn push_license_filters(qb: &mut QueryBuilder<Postgres>, filter: &LicenseListFilter) {
    if let Some(status) = filter.status {
        qb.push(" AND l.status = ").push_bind(status.as_str());
    }
    if let Some(plan_type) = filter.plan_type {
        qb.push(" AND s.plan_type = ").push_bind(plan_type.as_str());
    }
    if let Some(before) = filter.expires_before {
        qb.push(" AND l.expires_at < ").push_bind(before);
    }
    if let Some(after) = filter.expires_after {
        qb.push(" AND l.expires_at > ").push_bind(after);
    }
}

fn push_device_filters(qb: &mut QueryBuilder<Postgres>, filter: &DeviceListFilter) {
    if let Some(user_id) = filter.user_id {
        qb.push(" AND s.user_id = ").push_bind(user_id);
    }
    if let Some(license_id) = filter.license_id {
        qb.push(" AND d.license_id = ").push_bind(license_id);
    }
}

fn push_login_history_filters(qb: &mut QueryBuilder<Postgres>, filter: &LoginHistoryFilter) {
    if let Some(user_id) = filter.user_id {
        qb.push(" AND lh.user_id = ").push_bind(user_id);
    }
    if let Some(success) = filter.success {
        qb.push(" AND lh.success = ").push_bind(success);
    }
    if let Some(from) = filter.from {
        qb.push(" AND lh.created_at >= ").push_bind(from);
    }
    if let Some(to) = filter.to {
        qb.push(" AND lh.created_at <= ").push_bind(to);
    }
}

fn push_license_validation_filters(
    qb: &mut QueryBuilder<Postgres>,
    filter: &LicenseValidationFilter,
) {
    if let Some(license_id) = filter.license_id {
        qb.push(" AND lvl.license_id = ").push_bind(license_id);
    }
    if let Some(result) = filter.result {
        qb.push(" AND lvl.result = ").push_bind(result.as_str());
    }
    if let Some(from) = filter.from {
        qb.push(" AND lvl.created_at >= ").push_bind(from);
    }
    if let Some(to) = filter.to {
        qb.push(" AND lvl.created_at <= ").push_bind(to);
    }
}
