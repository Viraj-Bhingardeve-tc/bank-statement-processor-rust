//! Domain types for the Admin API (Module 3): pagination primitives, the
//! filter parameters each `GET /admin/*` list endpoint accepts, and the
//! read-model summaries `service::admin_service`/`repository::admin`
//! produce. Every summary here is a flattened, admin-facing *view* over
//! rows already owned by `domain::{user,license,device,audit}` — it never
//! replaces those types, since customer-facing code
//! (`service::auth_service`, `service::license_service`) still owns them
//! and is untouched by this module.

use crate::domain::{
    LicenseRecordStatus, PlanType, SubscriptionStatus, UserRole, ValidationLogResult,
};
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// A 1-indexed page number/size. Every Admin API list endpoint takes one
/// of these, and every list query is `LIMIT`/`OFFSET` against it — "never
/// load unlimited rows" (Module 3's own requirement) has no exception
/// anywhere in `repository::admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pagination {
    pub page: i64,
    pub page_size: i64,
}

impl Pagination {
    pub const DEFAULT_PAGE_SIZE: i64 = 20;
    pub const MAX_PAGE_SIZE: i64 = 100;

    /// Clamps whatever a caller's query string contained — `page` to at
    /// least 1, `page_size` to `1..=MAX_PAGE_SIZE` (falling back to
    /// `DEFAULT_PAGE_SIZE` if absent or non-positive) — into something
    /// every list query can bind safely without ever being asked for an
    /// unbounded result set.
    pub fn new(page: Option<i64>, page_size: Option<i64>) -> Self {
        let page = page.filter(|p| *p >= 1).unwrap_or(1);
        let page_size = page_size
            .filter(|s| *s >= 1)
            .map(|s| s.min(Self::MAX_PAGE_SIZE))
            .unwrap_or(Self::DEFAULT_PAGE_SIZE);
        Pagination { page, page_size }
    }

    pub fn limit(&self) -> i64 {
        self.page_size
    }

    pub fn offset(&self) -> i64 {
        (self.page - 1) * self.page_size
    }
}

impl Default for Pagination {
    fn default() -> Self {
        Pagination::new(None, None)
    }
}

/// One page of `T`, plus `total` so a caller can compute the page count
/// itself — every `repository::admin::AdminRepository::list_*` method
/// returns one of these instead of a bare `Vec<T>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub page: i64,
    pub page_size: i64,
    pub total: i64,
}

/// Ascending or descending — the only sort direction `GET /admin/users`
/// exposes (over `created_at`, its only sortable column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Ascending,
    Descending,
}

// ── GET /admin/users ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct UserListFilter {
    /// Matched against `email` and `company_name` (case-insensitive,
    /// substring) — "Search by email/company".
    pub search: Option<String>,
    pub sort_order: SortOrder,
    pub pagination: Pagination,
}

/// `subscription_status` is the account's most recent subscription's
/// status, not a column on `users` itself — `users` has no status column
/// of its own (Module 2 added `role`, nothing else) and adding one is out
/// of this module's scope. `None` for an account with no subscription yet.
#[derive(Debug, Clone, PartialEq)]
pub struct AdminUserSummary {
    pub id: i64,
    pub email: String,
    pub company_name: Option<String>,
    pub role: UserRole,
    pub subscription_status: Option<SubscriptionStatus>,
    pub created_at: DateTime<Utc>,
}

// ── GET /admin/licenses ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct LicenseListFilter {
    pub status: Option<LicenseRecordStatus>,
    pub plan_type: Option<PlanType>,
    pub expires_before: Option<DateTime<Utc>>,
    pub expires_after: Option<DateTime<Utc>>,
    pub pagination: Pagination,
}

/// A `domain::License` flattened with its subscription's
/// `plan_type`/`user_id` — fields `License` itself doesn't carry (they
/// live on `Subscription`), joined here purely for this read-only admin
/// view.
#[derive(Debug, Clone, PartialEq)]
pub struct AdminLicenseSummary {
    pub id: i64,
    pub license_key: String,
    pub status: LicenseRecordStatus,
    pub plan_type: PlanType,
    pub expires_at: Option<DateTime<Utc>>,
    pub max_devices: i32,
    pub issued_at: DateTime<Utc>,
    pub user_id: i64,
}

// ── GET /admin/devices ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceListFilter {
    pub user_id: Option<i64>,
    pub license_id: Option<i64>,
    pub pagination: Pagination,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AdminDeviceSummary {
    pub id: i64,
    pub license_id: i64,
    pub user_id: i64,
    pub device_id: Uuid,
    pub device_label: Option<String>,
    pub last_seen_at: DateTime<Utc>,
    /// `deactivated_at.is_none()` — surfaced as a bool since nothing here
    /// needs the timestamp itself, only whether the device is currently
    /// active ("Device status").
    pub is_active: bool,
}

// ── GET /admin/audit/login-history ───────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct LoginHistoryFilter {
    pub user_id: Option<i64>,
    pub success: Option<bool>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub pagination: Pagination,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoginHistoryEntry {
    pub id: i64,
    pub user_id: i64,
    pub device_id: Option<Uuid>,
    pub success: bool,
    pub created_at: DateTime<Utc>,
}

// ── GET /admin/audit/license-validations ─────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct LicenseValidationFilter {
    pub license_id: Option<i64>,
    pub result: Option<ValidationLogResult>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub pagination: Pagination,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LicenseValidationEntry {
    pub id: i64,
    pub license_id: i64,
    pub device_id: Uuid,
    pub result: ValidationLogResult,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_defaults_to_page_one_and_the_default_page_size() {
        let p = Pagination::new(None, None);
        assert_eq!(p.page, 1);
        assert_eq!(p.page_size, Pagination::DEFAULT_PAGE_SIZE);
        assert_eq!(p.offset(), 0);
    }

    #[test]
    fn pagination_computes_offset_from_page_and_page_size() {
        let p = Pagination::new(Some(3), Some(10));
        assert_eq!(p.offset(), 20);
        assert_eq!(p.limit(), 10);
    }

    #[test]
    fn pagination_rejects_a_non_positive_page_by_falling_back_to_one() {
        let p = Pagination::new(Some(0), None);
        assert_eq!(p.page, 1);
        let p = Pagination::new(Some(-5), None);
        assert_eq!(p.page, 1);
    }

    #[test]
    fn pagination_clamps_an_oversized_page_size_to_the_maximum() {
        let p = Pagination::new(None, Some(10_000));
        assert_eq!(p.page_size, Pagination::MAX_PAGE_SIZE);
    }

    #[test]
    fn pagination_rejects_a_non_positive_page_size_by_falling_back_to_the_default() {
        let p = Pagination::new(None, Some(0));
        assert_eq!(p.page_size, Pagination::DEFAULT_PAGE_SIZE);
    }
}
