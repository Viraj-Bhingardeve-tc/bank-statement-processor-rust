//! HTTP-facing error type for the licensing endpoints — maps
//! `LicenseOperationError` onto `API_SPECIFICATION.md`'s response envelope
//! and documented error-code table, plus `INVALID_REQUEST` for malformed
//! input (UUID/integer parsing) the original 7-endpoint spec didn't need a
//! code for, since it only ever anticipated a well-behaved
//! `HttpLicenseClient` sending values it generated itself.

use crate::domain::Device;
use crate::service::{
    AdminOperationError, AuthError, LicenseOperationError, PaymentOperationError,
};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use std::fmt;

#[derive(Debug, Serialize, PartialEq)]
pub struct DeviceSummary {
    pub device_id: String,
    pub device_label: Option<String>,
    pub first_seen_at: String,
    pub last_seen_at: String,
}

impl From<&Device> for DeviceSummary {
    fn from(d: &Device) -> Self {
        DeviceSummary {
            device_id: d.device_id.to_string(),
            device_label: d.device_label.clone(),
            first_seen_at: d.first_seen_at.to_rfc3339(),
            last_seen_at: d.last_seen_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    devices: Option<Vec<DeviceSummary>>,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    ok: bool,
    error: ErrorBody,
}

#[derive(Debug)]
pub enum ApiError {
    InvalidRequest(String),
    LicenseNotFound,
    LicenseRevoked,
    LicenseExpired,
    DeviceNotActivated,
    DeviceLimitReached(Vec<Device>),
    /// Production Hardening, Finding C1: `/validate-license`,
    /// `/refresh-license`, `/deactivate-license` reject outright on a
    /// `machine_fingerprint` mismatch instead of the old "report but
    /// succeed" behavior. Additive `403 DEVICE_MISMATCH` — no existing
    /// caller could previously receive this code.
    DeviceMismatch,
    /// `POST /login` failure — matches `API_SPECIFICATION.md`'s
    /// `401 INVALID_CREDENTIALS`. Deliberately the same response whether
    /// the email is unknown or the password is wrong (see
    /// `AuthService::login`'s doc comment).
    InvalidCredentials,
    /// Missing, malformed, expired, or revoked bearer token — matches
    /// `API_SPECIFICATION.md`'s `401 UNAUTHORIZED`.
    Unauthorized,
    /// Module 2: a genuinely valid session whose account isn't an `Admin`
    /// — `routes::admin::require_admin`'s rejection, distinct from
    /// `Unauthorized` (see `AuthService::require_admin`'s doc comment).
    /// Additive `403 FORBIDDEN`, no existing endpoint returns it.
    Forbidden,
    /// Too many requests from this caller (`rate_limit::login_rate_limit`
    /// keyed by client IP on `/login`, `rate_limit::device_rate_limit`
    /// keyed by `device_id` on `/validate-license`) — matches
    /// `API_SPECIFICATION.md`'s documented `429 RATE_LIMITED` code, which
    /// existed in the spec's error table from the start but was
    /// unreachable until this phase actually implemented rate limiting.
    RateLimited,
    /// `POST /create-checkout-session` with an unrecognized `plan_type` —
    /// `PHASE4_DESIGN.md` §3's additive `400 INVALID_PLAN_TYPE` code.
    InvalidPlanType,
    /// The Razorpay API call itself failed (unconfigured, network error,
    /// non-2xx response) — `PHASE4_DESIGN.md` §3's additive
    /// `502 PROVIDER_ERROR` code, surfaced honestly rather than a fake
    /// success (same "no server configured" precedent from Phase 3).
    ProviderError(String),
    /// Module 3: `POST /admin/device/:id/{deactivate,activate}` against a
    /// `device_id` no `devices` row exists for — `LicenseNotFound` above
    /// (reused as-is for the equivalent admin license lookup) has no
    /// device-shaped counterpart to reuse.
    DeviceNotFound,
    /// Module 3: `POST /admin/license/:id/restore` on a license whose
    /// current status isn't `revoked` — additive `409 LICENSE_NOT_REVOKED`.
    LicenseNotRevoked,
    Server(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::InvalidRequest(msg) => write!(f, "invalid request: {msg}"),
            ApiError::LicenseNotFound => write!(f, "license not found"),
            ApiError::LicenseRevoked => write!(f, "license has been revoked"),
            ApiError::LicenseExpired => write!(f, "license has expired"),
            ApiError::DeviceNotActivated => write!(f, "device not activated for this license"),
            ApiError::DeviceLimitReached(_) => write!(f, "device limit reached for this license"),
            ApiError::DeviceMismatch => {
                write!(f, "machine fingerprint does not match the activated device")
            }
            ApiError::InvalidCredentials => write!(f, "invalid credentials"),
            ApiError::Unauthorized => write!(f, "unauthorized"),
            ApiError::Forbidden => write!(f, "forbidden"),
            ApiError::RateLimited => write!(f, "too many requests"),
            ApiError::InvalidPlanType => write!(f, "invalid plan_type"),
            ApiError::ProviderError(msg) => write!(f, "payment provider error: {msg}"),
            ApiError::DeviceNotFound => write!(f, "device not found"),
            ApiError::LicenseNotRevoked => write!(f, "license is not revoked"),
            ApiError::Server(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl From<LicenseOperationError> for ApiError {
    fn from(e: LicenseOperationError) -> Self {
        match e {
            LicenseOperationError::LicenseNotFound => ApiError::LicenseNotFound,
            LicenseOperationError::LicenseRevoked => ApiError::LicenseRevoked,
            LicenseOperationError::LicenseExpired => ApiError::LicenseExpired,
            LicenseOperationError::DeviceNotActivated => ApiError::DeviceNotActivated,
            LicenseOperationError::DeviceLimitReached(devices) => {
                ApiError::DeviceLimitReached(devices)
            }
            LicenseOperationError::DeviceMismatch => ApiError::DeviceMismatch,
            LicenseOperationError::Repository(err) => ApiError::Server(err.to_string()),
        }
    }
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::InvalidCredentials => ApiError::InvalidCredentials,
            AuthError::Unauthorized => ApiError::Unauthorized,
            AuthError::Forbidden => ApiError::Forbidden,
            AuthError::Repository(err) => ApiError::Server(err.to_string()),
        }
    }
}

impl From<AdminOperationError> for ApiError {
    fn from(e: AdminOperationError) -> Self {
        match e {
            // Reused as-is — same 404 condition `LicenseOperationError::
            // LicenseNotFound` already represents, just reached from an
            // admin lookup instead of `/activate-license`.
            AdminOperationError::LicenseNotFound => ApiError::LicenseNotFound,
            AdminOperationError::LicenseNotRevoked => ApiError::LicenseNotRevoked,
            AdminOperationError::DeviceNotFound => ApiError::DeviceNotFound,
            AdminOperationError::Repository(err) => ApiError::Server(err.to_string()),
        }
    }
}

impl From<PaymentOperationError> for ApiError {
    fn from(e: PaymentOperationError) -> Self {
        match e {
            PaymentOperationError::InvalidPlanType => ApiError::InvalidPlanType,
            PaymentOperationError::ProviderError(msg) => ApiError::ProviderError(msg),
            PaymentOperationError::Repository(err) => ApiError::Server(err.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, devices): (StatusCode, &'static str, Option<Vec<DeviceSummary>>) =
            match &self {
                ApiError::InvalidRequest(_) => (StatusCode::BAD_REQUEST, "INVALID_REQUEST", None),
                ApiError::LicenseNotFound => (StatusCode::NOT_FOUND, "LICENSE_NOT_FOUND", None),
                ApiError::DeviceNotActivated => {
                    (StatusCode::NOT_FOUND, "DEVICE_NOT_ACTIVATED", None)
                }
                ApiError::DeviceLimitReached(existing) => (
                    StatusCode::CONFLICT,
                    "DEVICE_LIMIT_REACHED",
                    Some(existing.iter().map(DeviceSummary::from).collect()),
                ),
                ApiError::LicenseExpired => (StatusCode::GONE, "LICENSE_EXPIRED", None),
                ApiError::LicenseRevoked => (StatusCode::GONE, "LICENSE_REVOKED", None),
                ApiError::DeviceMismatch => (StatusCode::FORBIDDEN, "DEVICE_MISMATCH", None),
                ApiError::InvalidCredentials => {
                    (StatusCode::UNAUTHORIZED, "INVALID_CREDENTIALS", None)
                }
                ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "UNAUTHORIZED", None),
                ApiError::Forbidden => (StatusCode::FORBIDDEN, "FORBIDDEN", None),
                ApiError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED", None),
                ApiError::InvalidPlanType => (StatusCode::BAD_REQUEST, "INVALID_PLAN_TYPE", None),
                ApiError::ProviderError(_) => (StatusCode::BAD_GATEWAY, "PROVIDER_ERROR", None),
                ApiError::DeviceNotFound => (StatusCode::NOT_FOUND, "DEVICE_NOT_FOUND", None),
                ApiError::LicenseNotRevoked => (StatusCode::CONFLICT, "LICENSE_NOT_REVOKED", None),
                ApiError::Server(_) => (StatusCode::INTERNAL_SERVER_ERROR, "SERVER_ERROR", None),
            };
        let message = self.to_string();

        (
            status,
            Json(ErrorEnvelope {
                ok: false,
                error: ErrorBody {
                    code,
                    message,
                    devices,
                },
            }),
        )
            .into_response()
    }
}
