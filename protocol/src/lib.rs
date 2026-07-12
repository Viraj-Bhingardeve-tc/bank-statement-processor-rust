//! `license-protocol` — shared wire types for the licensing system's
//! client/server boundary, matching `API_SPECIFICATION.md` exactly.
//!
//! Pure data — no I/O, no business logic. Both the desktop app
//! (`src/license/client.rs`) and the future licensing server (see
//! `PHASE4_DESIGN.md`) depend on this crate so the request/response shapes
//! can never drift from the spec independently on each side: a field
//! renamed or retyped on one side becomes a compile error on the other,
//! rather than a runtime mismatch discovered against a live server.
//!
//! Phase 4A extracted these types verbatim from `src/license/client.rs`
//! (no behavior change) — see `PHASE4_DESIGN.md` §13 phase 1.
//!
//! Every request/response DTO derives both `Serialize` and `Deserialize`
//! even though today's only caller (the desktop's `OfflineClient`) never
//! actually serializes anything: the desktop only ever needs to *send*
//! requests and *receive* responses, while a server needs the exact
//! opposite direction for the same types. Deriving both once, here, is
//! what lets either side use these types without a second, direction-
//! specific copy.

use serde::{Deserialize, Serialize};

/// Mirrors `API_SPECIFICATION.md`'s error code table exactly, plus two
/// client-local sentinel variants that have no wire representation at all:
/// `NoServerConfigured` (there is nothing to call — a desktop-only
/// configuration state, not a server response) and `NetworkError`
/// (a transport-level failure below the HTTP-response level, e.g. DNS or
/// connect failure). A server implementation only ever needs to construct
/// the other variants; this enum deliberately has no `Serialize`/
/// `Deserialize` derive, since a future `HttpLicenseClient` maps the wire's
/// error-code string onto this enum explicitly rather than expecting a
/// server to ever emit `"NO_SERVER_CONFIGURED"` or `"NETWORK_ERROR"` as a
/// real wire value.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivateLicenseRequest {
    pub license_key: String,
    pub device_id: String,
    pub machine_fingerprint: String,
    pub device_label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivateLicenseResponse {
    pub license_id: String,
    pub customer_id: String,
    pub subscription_type: String,
    pub status: String,
    pub expires_at: Option<String>,
    pub grace_period_days: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidateLicenseRequest {
    pub license_id: String,
    pub device_id: String,
    pub machine_fingerprint: String,
    pub client_clock: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidateLicenseResponse {
    pub status: String,
    pub expires_at: Option<String>,
    pub grace_period_days: i64,
    pub server_time: String,
    pub fingerprint_matched: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub license_id: String,
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoginResponse {
    pub session_token: String,
    pub user_id: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionSummary {
    pub subscription_id: String,
    pub plan_type: String,
    pub status: String,
    pub current_period_end: Option<String>,
    pub auto_renew: bool,
}

/// `POST /deactivate-license` — additive beyond the original 7 endpoints
/// this crate was first built against (`API_SPECIFICATION.md` mentions
/// device deactivation only as an admin-surface action, not a customer-
/// facing endpoint). Added in Phase 4D so a customer/support flow can free
/// up a device slot without an admin dashboard existing yet. Same
/// `license_id`/`device_id` shape as `ValidateLicenseRequest`, minus the
/// fields (`machine_fingerprint`, `client_clock`) deactivation doesn't need.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeactivateLicenseRequest {
    pub license_id: String,
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeactivateLicenseResponse {
    pub status: String,
    pub devices_active: i64,
}

/// `POST /create-checkout-session` — the one additive endpoint
/// `PHASE4_DESIGN.md` §3 anticipated from the start (payment was out of
/// scope for the original 7). Auth: Bearer session token — this is a
/// *server-account* action (`LICENSE_SYSTEM_DESIGN.md` §1), not something
/// `/activate-license`'s license-key flow needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateCheckoutSessionRequest {
    pub plan_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateCheckoutSessionResponse {
    pub checkout_url: String,
    pub provider_ref: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every DTO must survive a serialize→deserialize round trip unchanged
    /// — the whole point of this crate is that both sides agree on the
    /// wire shape, so a struct that can't round-trip itself would already
    /// be broken before a client and server ever talk to each other.
    fn round_trips<T>(value: T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(&value).expect("serialize");
        let back: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(value, back, "round trip through JSON: {json}");
    }

    #[test]
    fn activate_license_request_round_trips() {
        round_trips(ActivateLicenseRequest {
            license_key: "XXXX-XXXX-XXXX-XXXX".to_string(),
            device_id: "a1b2c3d4-uuid".to_string(),
            machine_fingerprint: "sha256-hex".to_string(),
            device_label: "DESKTOP-AB12CD".to_string(),
        });
    }

    #[test]
    fn activate_license_response_round_trips_with_and_without_expiry() {
        round_trips(ActivateLicenseResponse {
            license_id: "lic_456".to_string(),
            customer_id: "cus_789".to_string(),
            subscription_type: "yearly".to_string(),
            status: "active".to_string(),
            expires_at: Some("2027-07-09T00:00:00Z".to_string()),
            grace_period_days: 7,
        });
        round_trips(ActivateLicenseResponse {
            license_id: "lic_456".to_string(),
            customer_id: "cus_789".to_string(),
            subscription_type: "lifetime".to_string(),
            status: "active".to_string(),
            expires_at: None,
            grace_period_days: 7,
        });
    }

    #[test]
    fn validate_license_request_round_trips() {
        round_trips(ValidateLicenseRequest {
            license_id: "lic_456".to_string(),
            device_id: "a1b2c3d4-uuid".to_string(),
            machine_fingerprint: "sha256-hex".to_string(),
            client_clock: "2026-07-09T10:15:00Z".to_string(),
        });
    }

    #[test]
    fn validate_license_response_round_trips() {
        round_trips(ValidateLicenseResponse {
            status: "device_mismatch".to_string(),
            expires_at: Some("2027-07-09T00:00:00Z".to_string()),
            grace_period_days: 7,
            server_time: "2026-07-09T10:15:03Z".to_string(),
            fingerprint_matched: false,
        });
    }

    #[test]
    fn heartbeat_request_and_response_round_trip() {
        round_trips(HeartbeatRequest {
            license_id: "lic_456".to_string(),
            device_id: "a1b2c3d4-uuid".to_string(),
        });
        round_trips(HeartbeatResponse {
            status: "active".to_string(),
        });
    }

    #[test]
    fn login_request_and_response_round_trip() {
        round_trips(LoginRequest {
            email: "customer@example.com".to_string(),
            password: "hunter2".to_string(),
        });
        round_trips(LoginResponse {
            session_token: "opaque-bearer-token".to_string(),
            user_id: "usr_123".to_string(),
            expires_at: "2026-08-09T00:00:00Z".to_string(),
        });
    }

    #[test]
    fn subscription_summary_round_trips() {
        round_trips(SubscriptionSummary {
            subscription_id: "sub_321".to_string(),
            plan_type: "yearly".to_string(),
            status: "active".to_string(),
            current_period_end: Some("2027-07-09T00:00:00Z".to_string()),
            auto_renew: true,
        });
    }

    #[test]
    fn deactivate_license_request_and_response_round_trip() {
        round_trips(DeactivateLicenseRequest {
            license_id: "lic_456".to_string(),
            device_id: "a1b2c3d4-uuid".to_string(),
        });
        round_trips(DeactivateLicenseResponse {
            status: "deactivated".to_string(),
            devices_active: 0,
        });
    }

    #[test]
    fn create_checkout_session_request_and_response_round_trip() {
        round_trips(CreateCheckoutSessionRequest {
            plan_type: "yearly".to_string(),
        });
        round_trips(CreateCheckoutSessionResponse {
            checkout_url: "https://checkout.razorpay.com/xyz".to_string(),
            provider_ref: "order_xyz".to_string(),
        });
    }

    /// Field names are the wire contract (no `#[serde(rename)]` anywhere in
    /// this crate) — this test pins the exact JSON shape `API_SPECIFICATION.md`
    /// documents for one representative DTO, so an accidental field rename
    /// shows up as a failing assertion here, not as a 400 from a real server.
    #[test]
    fn activate_license_request_matches_the_documented_wire_shape() {
        let json = serde_json::to_value(ActivateLicenseRequest {
            license_key: "XXXX-XXXX-XXXX-XXXX".to_string(),
            device_id: "a1b2c3d4-...-uuid".to_string(),
            machine_fingerprint: "sha256-hex".to_string(),
            device_label: "DESKTOP-AB12CD".to_string(),
        })
        .unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "license_key": "XXXX-XXXX-XXXX-XXXX",
                "device_id": "a1b2c3d4-...-uuid",
                "machine_fingerprint": "sha256-hex",
                "device_label": "DESKTOP-AB12CD"
            })
        );
    }
}
