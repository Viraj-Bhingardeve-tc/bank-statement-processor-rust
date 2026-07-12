//! HTTP-facing error type for the licensing endpoints — maps
//! `LicenseOperationError` onto `API_SPECIFICATION.md`'s response envelope
//! and documented error-code table, plus `INVALID_REQUEST` for malformed
//! input (UUID/integer parsing) the original 7-endpoint spec didn't need a
//! code for, since it only ever anticipated a well-behaved
//! `HttpLicenseClient` sending values it generated itself.

use crate::domain::Device;
use crate::service::{AuthError, LicenseOperationError, PaymentOperationError};
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
    /// `POST /login` failure — matches `API_SPECIFICATION.md`'s
    /// `401 INVALID_CREDENTIALS`. Deliberately the same response whether
    /// the email is unknown or the password is wrong (see
    /// `AuthService::login`'s doc comment).
    InvalidCredentials,
    /// Missing, malformed, expired, or revoked bearer token — matches
    /// `API_SPECIFICATION.md`'s `401 UNAUTHORIZED`.
    Unauthorized,
    /// `POST /create-checkout-session` with an unrecognized `plan_type` —
    /// `PHASE4_DESIGN.md` §3's additive `400 INVALID_PLAN_TYPE` code.
    InvalidPlanType,
    /// The Razorpay API call itself failed (unconfigured, network error,
    /// non-2xx response) — `PHASE4_DESIGN.md` §3's additive
    /// `502 PROVIDER_ERROR` code, surfaced honestly rather than a fake
    /// success (same "no server configured" precedent from Phase 3).
    ProviderError(String),
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
            ApiError::InvalidCredentials => write!(f, "invalid credentials"),
            ApiError::Unauthorized => write!(f, "unauthorized"),
            ApiError::InvalidPlanType => write!(f, "invalid plan_type"),
            ApiError::ProviderError(msg) => write!(f, "payment provider error: {msg}"),
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
            LicenseOperationError::Repository(err) => ApiError::Server(err.to_string()),
        }
    }
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::InvalidCredentials => ApiError::InvalidCredentials,
            AuthError::Unauthorized => ApiError::Unauthorized,
            AuthError::Repository(err) => ApiError::Server(err.to_string()),
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
                ApiError::InvalidCredentials => {
                    (StatusCode::UNAUTHORIZED, "INVALID_CREDENTIALS", None)
                }
                ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "UNAUTHORIZED", None),
                ApiError::InvalidPlanType => (StatusCode::BAD_REQUEST, "INVALID_PLAN_TYPE", None),
                ApiError::ProviderError(_) => (StatusCode::BAD_GATEWAY, "PROVIDER_ERROR", None),
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
