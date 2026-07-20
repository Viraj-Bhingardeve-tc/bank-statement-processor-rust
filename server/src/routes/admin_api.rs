//! The Admin API (Module 3): `GET /admin/users`, `GET /admin/licenses`,
//! `GET /admin/devices`, `GET /admin/audit/login-history`,
//! `GET /admin/audit/license-validations`, plus
//! `POST /admin/license/:license_id/{revoke,restore}` and
//! `POST /admin/device/:device_id/{deactivate,activate}`.
//!
//! Handlers are thin: parse/validate query or path params, call one
//! `AdminService` method, map the `Result` onto a response — all real
//! logic lives in `service::admin_service`, same pattern
//! `routes::license`/`routes::auth`/`routes::payment` already established.
//! Every route in `router()` below sits behind `routes::admin::require_admin`
//! (Module 2) — `401` for a missing/invalid/expired session, `403` for a
//! valid session that isn't an `Admin`'s, enforced once by that middleware,
//! not re-checked in any handler here.
//!
//! Query-string filters arrive as plain strings (`Option<String>` fields on
//! each `*Query` struct below) and are parsed by hand at the bottom of this
//! file, the same "parse once, at the HTTP boundary, into
//! `ApiError::InvalidRequest` on failure" convention `routes::license`'s
//! `parse_license_id`/`parse_device_id` already established — deliberately
//! not relying on serde/axum's own automatic numeric-query-param
//! deserialization, so a malformed filter value always comes back as this
//! crate's own JSON error envelope instead of axum's default rejection
//! body.

use crate::domain::{
    DeviceListFilter, LicenseListFilter, LicenseRecordStatus, LicenseValidationFilter,
    LoginHistoryFilter, Page, Pagination, PlanType, SortOrder, UserListFilter, ValidationLogResult,
};
use crate::routes::admin::require_admin;
use crate::routes::auth::AuthenticatedSession;
use crate::routes::error::ApiError;
use crate::routes::license::parse_license_id;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::middleware;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/admin/users", get(list_users))
        .route("/admin/licenses", get(list_licenses))
        .route("/admin/devices", get(list_devices))
        .route("/admin/audit/login-history", get(list_login_history))
        .route(
            "/admin/audit/license-validations",
            get(list_license_validations),
        )
        .route("/admin/license/:license_id/revoke", post(revoke_license))
        .route("/admin/license/:license_id/restore", post(restore_license))
        .route(
            "/admin/device/:device_id/deactivate",
            post(deactivate_device),
        )
        .route("/admin/device/:device_id/activate", post(activate_device))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin))
        .with_state(state)
}

// ── Shared response envelope ─────────────────────────────────────────────

#[derive(Debug, Serialize, PartialEq)]
struct PagedResponse<T: Serialize> {
    items: Vec<T>,
    page: i64,
    page_size: i64,
    total: i64,
}

impl<T: Serialize, U: Into<T>> From<Page<U>> for PagedResponse<T> {
    fn from(page: Page<U>) -> Self {
        PagedResponse {
            items: page.items.into_iter().map(Into::into).collect(),
            page: page.page,
            page_size: page.page_size,
            total: page.total,
        }
    }
}

// ── GET /admin/users ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct UserListQuery {
    page: Option<String>,
    page_size: Option<String>,
    search: Option<String>,
    /// `"asc"` or `"desc"` (default) — "Sort by created_at".
    order: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct AdminUserDto {
    id: String,
    email: String,
    company_name: Option<String>,
    role: String,
    status: Option<String>,
    created_at: String,
}

impl From<crate::domain::AdminUserSummary> for AdminUserDto {
    fn from(u: crate::domain::AdminUserSummary) -> Self {
        AdminUserDto {
            id: u.id.to_string(),
            email: u.email,
            company_name: u.company_name,
            role: u.role.as_str().to_string(),
            status: u.subscription_status.map(|s| s.as_str().to_string()),
            created_at: u.created_at.to_rfc3339(),
        }
    }
}

async fn list_users(
    State(state): State<AppState>,
    Extension(AuthenticatedSession(session)): Extension<AuthenticatedSession>,
    Query(q): Query<UserListQuery>,
) -> Result<Json<PagedResponse<AdminUserDto>>, ApiError> {
    let sort_order = match q.order.as_deref() {
        None | Some("desc") => SortOrder::Descending,
        Some("asc") => SortOrder::Ascending,
        Some(_) => {
            return Err(ApiError::InvalidRequest(
                "order must be 'asc' or 'desc'".to_string(),
            ))
        }
    };
    let filter = UserListFilter {
        search: q.search,
        sort_order,
        pagination: Pagination::new(
            parse_optional_i64(q.page, "page")?,
            parse_optional_i64(q.page_size, "page_size")?,
        ),
    };

    let page = state
        .admin_service
        .list_users(session.user_id, filter)
        .await?;
    Ok(Json(page.into()))
}

// ── GET /admin/licenses ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct LicenseListQuery {
    page: Option<String>,
    page_size: Option<String>,
    status: Option<String>,
    plan: Option<String>,
    expires_before: Option<String>,
    expires_after: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct AdminLicenseDto {
    license_id: String,
    license_key: String,
    status: String,
    plan_type: String,
    expires_at: Option<String>,
    max_devices: i32,
    issued_at: String,
    user_id: String,
}

impl From<crate::domain::AdminLicenseSummary> for AdminLicenseDto {
    fn from(l: crate::domain::AdminLicenseSummary) -> Self {
        AdminLicenseDto {
            license_id: l.id.to_string(),
            license_key: l.license_key,
            status: l.status.as_str().to_string(),
            plan_type: l.plan_type.as_str().to_string(),
            expires_at: l.expires_at.map(|d| d.to_rfc3339()),
            max_devices: l.max_devices,
            issued_at: l.issued_at.to_rfc3339(),
            user_id: l.user_id.to_string(),
        }
    }
}

async fn list_licenses(
    State(state): State<AppState>,
    Extension(AuthenticatedSession(session)): Extension<AuthenticatedSession>,
    Query(q): Query<LicenseListQuery>,
) -> Result<Json<PagedResponse<AdminLicenseDto>>, ApiError> {
    let filter = LicenseListFilter {
        status: parse_optional::<LicenseRecordStatus>(q.status, "status")?,
        plan_type: parse_optional::<PlanType>(q.plan, "plan")?,
        expires_before: parse_optional_datetime(q.expires_before, "expires_before")?,
        expires_after: parse_optional_datetime(q.expires_after, "expires_after")?,
        pagination: Pagination::new(
            parse_optional_i64(q.page, "page")?,
            parse_optional_i64(q.page_size, "page_size")?,
        ),
    };

    let page = state
        .admin_service
        .list_licenses(session.user_id, filter)
        .await?;
    Ok(Json(page.into()))
}

// ── GET /admin/devices ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DeviceListQuery {
    page: Option<String>,
    page_size: Option<String>,
    user_id: Option<String>,
    license_id: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct AdminDeviceDto {
    id: String,
    license_id: String,
    user_id: String,
    device_id: String,
    device_label: Option<String>,
    last_seen_at: String,
    is_active: bool,
}

impl From<crate::domain::AdminDeviceSummary> for AdminDeviceDto {
    fn from(d: crate::domain::AdminDeviceSummary) -> Self {
        AdminDeviceDto {
            id: d.id.to_string(),
            license_id: d.license_id.to_string(),
            user_id: d.user_id.to_string(),
            device_id: d.device_id.to_string(),
            device_label: d.device_label,
            last_seen_at: d.last_seen_at.to_rfc3339(),
            is_active: d.is_active,
        }
    }
}

async fn list_devices(
    State(state): State<AppState>,
    Extension(AuthenticatedSession(session)): Extension<AuthenticatedSession>,
    Query(q): Query<DeviceListQuery>,
) -> Result<Json<PagedResponse<AdminDeviceDto>>, ApiError> {
    let filter = DeviceListFilter {
        user_id: parse_optional_i64(q.user_id, "user_id")?,
        license_id: parse_optional_i64(q.license_id, "license_id")?,
        pagination: Pagination::new(
            parse_optional_i64(q.page, "page")?,
            parse_optional_i64(q.page_size, "page_size")?,
        ),
    };

    let page = state
        .admin_service
        .list_devices(session.user_id, filter)
        .await?;
    Ok(Json(page.into()))
}

// ── GET /admin/audit/login-history ───────────────────────────────────────

#[derive(Debug, Deserialize)]
struct LoginHistoryQuery {
    page: Option<String>,
    page_size: Option<String>,
    user_id: Option<String>,
    success: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct LoginHistoryDto {
    id: String,
    user_id: String,
    device_id: Option<String>,
    success: bool,
    created_at: String,
}

impl From<crate::domain::LoginHistoryEntry> for LoginHistoryDto {
    fn from(e: crate::domain::LoginHistoryEntry) -> Self {
        LoginHistoryDto {
            id: e.id.to_string(),
            user_id: e.user_id.to_string(),
            device_id: e.device_id.map(|d| d.to_string()),
            success: e.success,
            created_at: e.created_at.to_rfc3339(),
        }
    }
}

async fn list_login_history(
    State(state): State<AppState>,
    Extension(AuthenticatedSession(session)): Extension<AuthenticatedSession>,
    Query(q): Query<LoginHistoryQuery>,
) -> Result<Json<PagedResponse<LoginHistoryDto>>, ApiError> {
    let filter = LoginHistoryFilter {
        user_id: parse_optional_i64(q.user_id, "user_id")?,
        success: parse_optional_bool(q.success, "success")?,
        from: parse_optional_datetime(q.from, "from")?,
        to: parse_optional_datetime(q.to, "to")?,
        pagination: Pagination::new(
            parse_optional_i64(q.page, "page")?,
            parse_optional_i64(q.page_size, "page_size")?,
        ),
    };

    let page = state
        .admin_service
        .list_login_history(session.user_id, filter)
        .await?;
    Ok(Json(page.into()))
}

// ── GET /admin/audit/license-validations ─────────────────────────────────

#[derive(Debug, Deserialize)]
struct LicenseValidationQuery {
    page: Option<String>,
    page_size: Option<String>,
    license_id: Option<String>,
    result: Option<String>,
    from: Option<String>,
    to: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct LicenseValidationDto {
    id: String,
    license_id: String,
    device_id: String,
    result: String,
    created_at: String,
}

impl From<crate::domain::LicenseValidationEntry> for LicenseValidationDto {
    fn from(e: crate::domain::LicenseValidationEntry) -> Self {
        LicenseValidationDto {
            id: e.id.to_string(),
            license_id: e.license_id.to_string(),
            device_id: e.device_id.to_string(),
            result: e.result.as_str().to_string(),
            created_at: e.created_at.to_rfc3339(),
        }
    }
}

async fn list_license_validations(
    State(state): State<AppState>,
    Extension(AuthenticatedSession(session)): Extension<AuthenticatedSession>,
    Query(q): Query<LicenseValidationQuery>,
) -> Result<Json<PagedResponse<LicenseValidationDto>>, ApiError> {
    let filter = LicenseValidationFilter {
        license_id: parse_optional_i64(q.license_id, "license_id")?,
        result: parse_optional::<ValidationLogResult>(q.result, "result")?,
        from: parse_optional_datetime(q.from, "from")?,
        to: parse_optional_datetime(q.to, "to")?,
        pagination: Pagination::new(
            parse_optional_i64(q.page, "page")?,
            parse_optional_i64(q.page_size, "page_size")?,
        ),
    };

    let page = state
        .admin_service
        .list_license_validations(session.user_id, filter)
        .await?;
    Ok(Json(page.into()))
}

// ── POST /admin/license/:license_id/revoke, .../restore ──────────────────

#[derive(Debug, Deserialize)]
struct RevokeLicenseRequest {
    reason: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct LicenseActionResponse {
    license_id: String,
    status: String,
}

impl From<crate::domain::License> for LicenseActionResponse {
    fn from(l: crate::domain::License) -> Self {
        LicenseActionResponse {
            license_id: l.id.to_string(),
            status: l.status.as_str().to_string(),
        }
    }
}

async fn revoke_license(
    State(state): State<AppState>,
    Extension(AuthenticatedSession(session)): Extension<AuthenticatedSession>,
    Path(raw_license_id): Path<String>,
    body: Option<Json<RevokeLicenseRequest>>,
) -> Result<Json<LicenseActionResponse>, ApiError> {
    let license_id = parse_license_id(&raw_license_id)?;
    let reason = body.and_then(|Json(b)| b.reason);

    let updated = state
        .admin_service
        .revoke_license(session.user_id, license_id, reason)
        .await?;
    Ok(Json(updated.into()))
}

async fn restore_license(
    State(state): State<AppState>,
    Extension(AuthenticatedSession(session)): Extension<AuthenticatedSession>,
    Path(raw_license_id): Path<String>,
) -> Result<Json<LicenseActionResponse>, ApiError> {
    let license_id = parse_license_id(&raw_license_id)?;

    let updated = state
        .admin_service
        .restore_license(session.user_id, license_id)
        .await?;
    Ok(Json(updated.into()))
}

// ── POST /admin/device/:device_id/deactivate, .../activate ───────────────

#[derive(Debug, Serialize, PartialEq)]
struct DeviceActionResponse {
    device_id: String,
    status: &'static str,
}

/// `device_id` here is the `devices` table's own `BIGSERIAL` row id (what
/// `GET /admin/devices` returns as `id`) — not the client-generated
/// `device_id: Uuid` column `routes::license` parses, which is only unique
/// per-license, not globally. Parsed the same "string over the wire" way
/// as `parse_license_id`.
fn parse_admin_device_id(raw: &str) -> Result<i64, ApiError> {
    raw.parse::<i64>()
        .map_err(|_| ApiError::InvalidRequest("device_id must be a valid integer".to_string()))
}

async fn deactivate_device(
    State(state): State<AppState>,
    Extension(AuthenticatedSession(session)): Extension<AuthenticatedSession>,
    Path(raw_device_id): Path<String>,
) -> Result<Json<DeviceActionResponse>, ApiError> {
    let device_id = parse_admin_device_id(&raw_device_id)?;

    state
        .admin_service
        .deactivate_device(session.user_id, device_id)
        .await?;
    Ok(Json(DeviceActionResponse {
        device_id: device_id.to_string(),
        status: "deactivated",
    }))
}

async fn activate_device(
    State(state): State<AppState>,
    Extension(AuthenticatedSession(session)): Extension<AuthenticatedSession>,
    Path(raw_device_id): Path<String>,
) -> Result<Json<DeviceActionResponse>, ApiError> {
    let device_id = parse_admin_device_id(&raw_device_id)?;

    state
        .admin_service
        .activate_device(session.user_id, device_id)
        .await?;
    Ok(Json(DeviceActionResponse {
        device_id: device_id.to_string(),
        status: "activated",
    }))
}

// ── Query-string parsing helpers ─────────────────────────────────────────
//
// Every filter arrives as `Option<String>` (see this file's own doc
// comment for why) and is parsed here, once, into `ApiError::InvalidRequest`
// on failure.

fn parse_optional_i64(raw: Option<String>, field: &str) -> Result<Option<i64>, ApiError> {
    raw.map(|s| {
        s.parse::<i64>()
            .map_err(|_| ApiError::InvalidRequest(format!("{field} must be a valid integer")))
    })
    .transpose()
}

fn parse_optional_bool(raw: Option<String>, field: &str) -> Result<Option<bool>, ApiError> {
    raw.map(|s| match s.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ApiError::InvalidRequest(format!(
            "{field} must be 'true' or 'false'"
        ))),
    })
    .transpose()
}

fn parse_optional_datetime(
    raw: Option<String>,
    field: &str,
) -> Result<Option<DateTime<Utc>>, ApiError> {
    raw.map(|s| {
        DateTime::parse_from_rfc3339(&s)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|_| {
                ApiError::InvalidRequest(format!("{field} must be a valid RFC3339 timestamp"))
            })
    })
    .transpose()
}

fn parse_optional<T: FromStr>(raw: Option<String>, field: &str) -> Result<Option<T>, ApiError> {
    raw.map(|s| {
        T::from_str(&s)
            .map_err(|_| ApiError::InvalidRequest(format!("{field} is not a recognized value")))
    })
    .transpose()
}
