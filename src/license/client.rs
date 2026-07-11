// client.rs — LicenseApiClient trait + request/response DTOs, matching
// API_SPECIFICATION.md exactly. See LICENSE_SYSTEM_DESIGN.md §9.
//
// `OfflineClient` is the only implementation that exists today: every
// method fails immediately with `ApiError::NoServerConfigured`, no network
// I/O attempted. This is deliberate — see LICENSE_SYSTEM_DESIGN.md §7 for
// why no real HTTP client is wired up in this phase. A future
// `HttpLicenseClient` (using the `reqwest` dependency already present in
// Cargo.toml, gated behind the same "ai" feature that already pulls it in)
// implements the same trait and is a drop-in replacement — no call site
// outside this module needs to change.

use serde::{Deserialize, Serialize};

/// Mirrors API_SPECIFICATION.md's error code table exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// No real backend exists yet in this phase — every `OfflineClient`
    /// call returns this immediately. Not a network failure; a
    /// configuration state ("there is nothing to call").
    NoServerConfigured,
    InvalidCredentials,
    Unauthorized,
    LicenseNotFound,
    DeviceNotActivated,
    DeviceLimitReached,
    LicenseExpired,
    LicenseRevoked,
    LicenseSuspended,
    RateLimited,
    /// Network-level failure (DNS, connect, timeout) — distinct from a
    /// well-formed error response, since callers treat this the same as
    /// "offline" for grace-period purposes, not as a definitive answer.
    NetworkError(String),
    ServerError(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivateLicenseRequest {
    pub license_key: String,
    pub device_id: String,
    pub machine_fingerprint: String,
    pub device_label: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActivateLicenseResponse {
    pub license_id: String,
    pub customer_id: String,
    pub subscription_type: String,
    pub status: String,
    pub expires_at: Option<String>,
    pub grace_period_days: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidateLicenseRequest {
    pub license_id: String,
    pub device_id: String,
    pub machine_fingerprint: String,
    pub client_clock: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ValidateLicenseResponse {
    pub status: String,
    pub expires_at: Option<String>,
    pub grace_period_days: i64,
    pub server_time: String,
    pub fingerprint_matched: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeartbeatRequest {
    pub license_id: String,
    pub device_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatResponse {
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginResponse {
    pub session_token: String,
    pub user_id: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubscriptionSummary {
    pub subscription_id: String,
    pub plan_type: String,
    pub status: String,
    pub current_period_end: Option<String>,
    pub auto_renew: bool,
}

/// The 7 endpoints from API_SPECIFICATION.md, as trait methods. Blocking
/// (not async) — matches this codebase's existing `reqwest` usage
/// convention (the `blocking` feature is already enabled in Cargo.toml for
/// the AI classifier's HTTP calls), so a future `HttpLicenseClient` fits the
/// same threading model already established elsewhere in this app (calls
/// made from a background `std::thread`, results marshaled back via
/// `slint::invoke_from_event_loop` — see `main.rs`'s OCR/AI-classify
/// handlers for the precedent this would follow).
pub trait LicenseApiClient {
    fn login(&self, req: &LoginRequest) -> Result<LoginResponse, ApiError>;
    fn activate_license(&self, req: &ActivateLicenseRequest) -> Result<ActivateLicenseResponse, ApiError>;
    fn validate_license(&self, req: &ValidateLicenseRequest) -> Result<ValidateLicenseResponse, ApiError>;
    fn refresh_license(&self, req: &ValidateLicenseRequest) -> Result<ValidateLicenseResponse, ApiError>;
    fn logout(&self) -> Result<(), ApiError>;
    fn get_subscription(&self) -> Result<SubscriptionSummary, ApiError>;
    fn heartbeat(&self, req: &HeartbeatRequest) -> Result<HeartbeatResponse, ApiError>;
}

/// The only `LicenseApiClient` implementation that exists in this phase —
/// see the module doc comment above.
pub struct OfflineClient;

impl LicenseApiClient for OfflineClient {
    fn login(&self, _req: &LoginRequest) -> Result<LoginResponse, ApiError> {
        Err(ApiError::NoServerConfigured)
    }
    fn activate_license(&self, _req: &ActivateLicenseRequest) -> Result<ActivateLicenseResponse, ApiError> {
        Err(ApiError::NoServerConfigured)
    }
    fn validate_license(&self, _req: &ValidateLicenseRequest) -> Result<ValidateLicenseResponse, ApiError> {
        Err(ApiError::NoServerConfigured)
    }
    fn refresh_license(&self, _req: &ValidateLicenseRequest) -> Result<ValidateLicenseResponse, ApiError> {
        Err(ApiError::NoServerConfigured)
    }
    fn logout(&self) -> Result<(), ApiError> {
        Err(ApiError::NoServerConfigured)
    }
    fn get_subscription(&self) -> Result<SubscriptionSummary, ApiError> {
        Err(ApiError::NoServerConfigured)
    }
    fn heartbeat(&self, _req: &HeartbeatRequest) -> Result<HeartbeatResponse, ApiError> {
        Err(ApiError::NoServerConfigured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_client_never_attempts_network_io_and_fails_every_call() {
        let client = OfflineClient;
        assert_eq!(
            client.login(&LoginRequest { email: "a@b.com".to_string(), password: "x".to_string() }).unwrap_err(),
            ApiError::NoServerConfigured
        );
        assert_eq!(
            client.validate_license(&ValidateLicenseRequest {
                license_id: "lic_1".to_string(),
                device_id: "dev_1".to_string(),
                machine_fingerprint: "fp".to_string(),
                client_clock: "2026-07-09T00:00:00Z".to_string(),
            }).unwrap_err(),
            ApiError::NoServerConfigured
        );
        assert_eq!(client.logout().unwrap_err(), ApiError::NoServerConfigured);
        assert_eq!(
            client.heartbeat(&HeartbeatRequest { license_id: "lic_1".to_string(), device_id: "dev_1".to_string() }).unwrap_err(),
            ApiError::NoServerConfigured
        );
    }
}
