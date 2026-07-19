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
/// the AI classifier's HTTP calls), so `HttpLicenseClient` fits the same
/// threading model already established elsewhere in this app (calls made
/// from a background `std::thread`, results marshaled back via
/// `slint::invoke_from_event_loop` — see `main.rs`'s OCR/AI-classify
/// handlers for the precedent this follows).
///
/// `Send + Sync` (Phase 4K.3): both implementations already satisfy this
/// trivially (`OfflineClient` is a unit struct; `HttpLicenseClient` wraps a
/// `reqwest::blocking::Client`, itself `Send + Sync`) — added so `main.rs`
/// can hold one shared `Arc<dyn LicenseApiClient + Send + Sync>` across the
/// login handler and the periodic revalidation timer's background thread,
/// rather than constructing a fresh client (and connection pool) per call.
pub trait LicenseApiClient: Send + Sync {
    fn login(&self, req: &LoginRequest) -> Result<LoginResponse, ApiError>;
    fn activate_license(
        &self,
        req: &ActivateLicenseRequest,
    ) -> Result<ActivateLicenseResponse, ApiError>;
    fn validate_license(
        &self,
        req: &ValidateLicenseRequest,
    ) -> Result<ValidateLicenseResponse, ApiError>;
    fn refresh_license(
        &self,
        req: &ValidateLicenseRequest,
    ) -> Result<ValidateLicenseResponse, ApiError>;
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
    fn activate_license(
        &self,
        _req: &ActivateLicenseRequest,
    ) -> Result<ActivateLicenseResponse, ApiError> {
        Err(ApiError::NoServerConfigured)
    }
    fn validate_license(
        &self,
        _req: &ValidateLicenseRequest,
    ) -> Result<ValidateLicenseResponse, ApiError> {
        Err(ApiError::NoServerConfigured)
    }
    fn refresh_license(
        &self,
        _req: &ValidateLicenseRequest,
    ) -> Result<ValidateLicenseResponse, ApiError> {
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

/// The real `LicenseApiClient` implementation (Phase 4K.3 desktop
/// enforcement) — HTTP calls against the licensing server built in Phase
/// 4F–4K.2. `OfflineClient`'s module-doc-comment plan for this ("a future
/// `HttpLicenseClient`... is a drop-in replacement — no call site outside
/// this module needs to change") is exactly what this is: no other type in
/// this trait, no call site in `license::mod`, needed to change.
///
/// Gated behind the `ai` feature (a default feature) purely because that's
/// the flag `reqwest` is already behind in `Cargo.toml` — nothing about
/// this client is AI-related, it just reuses the dependency already pulled
/// in for the AI classifier's HTTP calls rather than adding a second one.
#[cfg(feature = "ai")]
pub struct HttpLicenseClient {
    http: reqwest::blocking::Client,
    base_url: String,
}

#[cfg(feature = "ai")]
impl HttpLicenseClient {
    /// Conservative, production-safe bounds so a single hung request can
    /// never block the caller — same values and same reasoning as the
    /// licensing server's own outbound Razorpay client
    /// (`server/src/razorpay/client.rs`'s `CONNECT_TIMEOUT`/`REQUEST_TIMEOUT`).
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    /// `base_url` — e.g. `"https://license.example.com"`, no trailing
    /// slash required (stripped if present). See `main.rs`'s
    /// `build_license_client` for where this comes from (the
    /// `LICENSE_SERVER_URL` environment variable) and why an unset value
    /// means `OfflineClient` is used instead of this type at all.
    pub fn new(base_url: &str) -> Self {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Self::CONNECT_TIMEOUT)
            .timeout(Self::REQUEST_TIMEOUT)
            .build()
            .expect("failed to build the license-server HTTP client");
        HttpLicenseClient {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    fn post<Req: Serialize, Resp: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        req: &Req,
    ) -> Result<Resp, ApiError> {
        let response = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .json(req)
            .send()
            .map_err(|e| ApiError::NetworkError(e.to_string()))?;
        Self::handle_response(response)
    }

    fn handle_response<Resp: for<'de> Deserialize<'de>>(
        response: reqwest::blocking::Response,
    ) -> Result<Resp, ApiError> {
        if response.status().is_success() {
            response
                .json::<Resp>()
                .map_err(|e| ApiError::NetworkError(format!("malformed server response: {e}")))
        } else {
            let status = response.status();
            // Best-effort: a well-formed error body gives an exact code to
            // map; any other shape (a proxy's own error page, an empty
            // body) still resolves to a status-code-derived fallback below
            // rather than failing to parse the *error itself*.
            let code = response.json::<ErrorEnvelope>().ok().map(|e| e.error.code);
            Err(map_error_code(status.as_u16(), code.as_deref()))
        }
    }
}

/// Mirrors just enough of `server/src/routes/error.rs`'s `ErrorEnvelope`
/// shape (`{"ok": false, "error": {"code": ..., "message": ..., ...}}`) to
/// read the one field this client maps on — the full envelope (including
/// `devices`, only meaningful for `DEVICE_LIMIT_REACHED`) isn't needed
/// here since `LicenseApiClient::ApiError::DeviceLimitReached` (unlike the
/// server's own error type) doesn't carry the device list.
#[cfg(feature = "ai")]
#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[cfg(feature = "ai")]
#[derive(Deserialize)]
struct ErrorBody {
    code: String,
}

/// Maps a non-2xx HTTP response onto `ApiError` — by the server's own
/// documented `code` string when the body parsed as one
/// (`server/src/routes/error.rs`'s exact code table), falling back to the
/// bare status code otherwise (a proxy timeout, a malformed body, or a
/// status this client build doesn't recognize yet all resolve to
/// `ServerError`, never silently to a "success-shaped" outcome).
#[cfg(feature = "ai")]
fn map_error_code(status: u16, code: Option<&str>) -> ApiError {
    match code {
        Some("LICENSE_NOT_FOUND") => ApiError::LicenseNotFound,
        Some("DEVICE_NOT_ACTIVATED") => ApiError::DeviceNotActivated,
        Some("DEVICE_LIMIT_REACHED") => ApiError::DeviceLimitReached,
        Some("LICENSE_EXPIRED") => ApiError::LicenseExpired,
        Some("LICENSE_REVOKED") => ApiError::LicenseRevoked,
        Some("INVALID_CREDENTIALS") => ApiError::InvalidCredentials,
        Some("UNAUTHORIZED") => ApiError::Unauthorized,
        Some("RATE_LIMITED") => ApiError::RateLimited,
        _ => ApiError::ServerError(format!("server returned HTTP {status}")),
    }
}

#[cfg(feature = "ai")]
impl LicenseApiClient for HttpLicenseClient {
    /// Account-login (server-account Bearer-session flow,
    /// `LICENSE_SYSTEM_DESIGN.md` §1) is a separate system from the
    /// license-key activation flow this phase enforces, and the desktop
    /// app has no UI for it yet (no session-token storage exists to hold
    /// the result) — honestly unimplemented rather than faked, same "no
    /// server configured" honesty precedent `OfflineClient` already
    /// established for every method before a real client existed at all.
    fn login(&self, _req: &LoginRequest) -> Result<LoginResponse, ApiError> {
        Err(ApiError::ServerError(
            "account login is not available from the desktop client".to_string(),
        ))
    }

    fn activate_license(
        &self,
        req: &ActivateLicenseRequest,
    ) -> Result<ActivateLicenseResponse, ApiError> {
        self.post("/activate-license", req)
    }

    fn validate_license(
        &self,
        req: &ValidateLicenseRequest,
    ) -> Result<ValidateLicenseResponse, ApiError> {
        self.post("/validate-license", req)
    }

    fn refresh_license(
        &self,
        req: &ValidateLicenseRequest,
    ) -> Result<ValidateLicenseResponse, ApiError> {
        self.post("/refresh-license", req)
    }

    fn logout(&self) -> Result<(), ApiError> {
        Err(ApiError::ServerError(
            "account login is not available from the desktop client".to_string(),
        ))
    }

    fn get_subscription(&self) -> Result<SubscriptionSummary, ApiError> {
        Err(ApiError::ServerError(
            "account login is not available from the desktop client".to_string(),
        ))
    }

    fn heartbeat(&self, req: &HeartbeatRequest) -> Result<HeartbeatResponse, ApiError> {
        self.post("/heartbeat", req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_client_never_attempts_network_io_and_fails_every_call() {
        let client = OfflineClient;
        assert_eq!(
            client
                .login(&LoginRequest {
                    email: "a@b.com".to_string(),
                    password: "x".to_string()
                })
                .unwrap_err(),
            ApiError::NoServerConfigured
        );
        assert_eq!(
            client
                .validate_license(&ValidateLicenseRequest {
                    license_id: "lic_1".to_string(),
                    device_id: "dev_1".to_string(),
                    machine_fingerprint: "fp".to_string(),
                    client_clock: "2026-07-09T00:00:00Z".to_string(),
                })
                .unwrap_err(),
            ApiError::NoServerConfigured
        );
        assert_eq!(client.logout().unwrap_err(), ApiError::NoServerConfigured);
        assert_eq!(
            client
                .heartbeat(&HeartbeatRequest {
                    license_id: "lic_1".to_string(),
                    device_id: "dev_1".to_string()
                })
                .unwrap_err(),
            ApiError::NoServerConfigured
        );
    }

    #[cfg(feature = "ai")]
    #[test]
    fn map_error_code_matches_the_servers_documented_code_table() {
        assert_eq!(
            map_error_code(404, Some("LICENSE_NOT_FOUND")),
            ApiError::LicenseNotFound
        );
        assert_eq!(
            map_error_code(404, Some("DEVICE_NOT_ACTIVATED")),
            ApiError::DeviceNotActivated
        );
        assert_eq!(
            map_error_code(409, Some("DEVICE_LIMIT_REACHED")),
            ApiError::DeviceLimitReached
        );
        assert_eq!(
            map_error_code(410, Some("LICENSE_EXPIRED")),
            ApiError::LicenseExpired
        );
        assert_eq!(
            map_error_code(410, Some("LICENSE_REVOKED")),
            ApiError::LicenseRevoked
        );
        assert_eq!(
            map_error_code(401, Some("INVALID_CREDENTIALS")),
            ApiError::InvalidCredentials
        );
        assert_eq!(
            map_error_code(401, Some("UNAUTHORIZED")),
            ApiError::Unauthorized
        );
        assert_eq!(
            map_error_code(429, Some("RATE_LIMITED")),
            ApiError::RateLimited
        );
    }

    #[cfg(feature = "ai")]
    #[test]
    fn map_error_code_falls_back_to_a_status_derived_server_error_for_an_unrecognized_code() {
        assert_eq!(
            map_error_code(500, Some("SOME_FUTURE_CODE")),
            ApiError::ServerError("server returned HTTP 500".to_string())
        );
        assert_eq!(
            map_error_code(502, None),
            ApiError::ServerError("server returned HTTP 502".to_string())
        );
    }

    #[cfg(feature = "ai")]
    #[test]
    fn http_license_client_strips_a_trailing_slash_from_the_base_url() {
        let client = HttpLicenseClient::new("https://license.example.com/");
        assert_eq!(client.base_url, "https://license.example.com");
    }

    /// Same fix, same test shape as the licensing server's own outbound
    /// Razorpay client (Phase 4J.4) — a request to a server that accepts
    /// the connection but never responds must still fail within roughly
    /// the configured timeout, not hang indefinitely (every
    /// `HttpLicenseClient` method routes through `post`/`handle_response`,
    /// built on this same client).
    #[cfg(feature = "ai")]
    #[test]
    fn a_request_past_the_configured_timeout_fails_instead_of_hanging_forever() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            // Accepts the connection and then never responds.
            let _ = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(30));
        });

        let http = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(200))
            .timeout(std::time::Duration::from_millis(200))
            .build()
            .unwrap();

        let started = std::time::Instant::now();
        let result = http.get(format!("http://{addr}/")).send();
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "a request to an unresponsive server must fail, not hang until it succeeds"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "the request must fail within roughly the configured timeout, not block indefinitely (took {elapsed:?})"
        );
    }
}
