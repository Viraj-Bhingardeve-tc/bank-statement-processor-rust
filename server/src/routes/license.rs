//! `POST /activate-license`, `POST /validate-license`,
//! `POST /deactivate-license` — the licensing endpoints
//! (`API_SPECIFICATION.md`, plus the additive `/deactivate-license`; see
//! `license_protocol::DeactivateLicenseRequest`'s doc comment).
//!
//! Handlers are thin: parse/validate the request, call one
//! `LicenseService` method, map the `Result` onto a response — all real
//! logic lives in `service::license_service` (`PHASE4_DESIGN.md` §1.2),
//! not here.

use crate::routes::error::ApiError;
use crate::state::AppState;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use license_protocol::{
    ActivateLicenseRequest, ActivateLicenseResponse, DeactivateLicenseRequest,
    DeactivateLicenseResponse, ValidateLicenseRequest, ValidateLicenseResponse,
};
use uuid::Uuid;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/activate-license", post(activate))
        .route("/validate-license", post(validate))
        .route("/deactivate-license", post(deactivate))
}

fn parse_device_id(raw: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(raw)
        .map_err(|_| ApiError::InvalidRequest("device_id must be a valid UUID".to_string()))
}

/// `license_id` travels over the wire as a `String` (`license_protocol`'s
/// DTOs match `API_SPECIFICATION.md` exactly, which never assumes a
/// specific ID representation) but this server's internal identity is a
/// plain `BIGSERIAL` — parsed once, here, at the HTTP boundary.
fn parse_license_id(raw: &str) -> Result<i64, ApiError> {
    raw.parse::<i64>()
        .map_err(|_| ApiError::InvalidRequest("license_id must be a valid integer".to_string()))
}

async fn activate(
    State(state): State<AppState>,
    Json(req): Json<ActivateLicenseRequest>,
) -> Result<Json<ActivateLicenseResponse>, ApiError> {
    let device_id = parse_device_id(&req.device_id)?;

    let outcome = state
        .license_service
        .activate(
            &req.license_key,
            device_id,
            &req.machine_fingerprint,
            &req.device_label,
        )
        .await?;

    Ok(Json(ActivateLicenseResponse {
        license_id: outcome.license.id.to_string(),
        customer_id: outcome.customer_id.to_string(),
        subscription_type: outcome.plan_type.as_str().to_string(),
        status: outcome.license.status.as_str().to_string(),
        expires_at: outcome.license.expires_at.map(|d| d.to_rfc3339()),
        grace_period_days: i64::from(outcome.license.grace_period_days),
    }))
}

async fn validate(
    State(state): State<AppState>,
    Json(req): Json<ValidateLicenseRequest>,
) -> Result<Json<ValidateLicenseResponse>, ApiError> {
    let license_id = parse_license_id(&req.license_id)?;
    let device_id = parse_device_id(&req.device_id)?;

    let outcome = state
        .license_service
        .validate(license_id, device_id, &req.machine_fingerprint)
        .await?;

    Ok(Json(ValidateLicenseResponse {
        status: outcome.status.as_str().to_string(),
        expires_at: outcome.expires_at.map(|d| d.to_rfc3339()),
        grace_period_days: i64::from(outcome.grace_period_days),
        server_time: chrono::Utc::now().to_rfc3339(),
        fingerprint_matched: outcome.fingerprint_matched,
    }))
}

async fn deactivate(
    State(state): State<AppState>,
    Json(req): Json<DeactivateLicenseRequest>,
) -> Result<Json<DeactivateLicenseResponse>, ApiError> {
    let license_id = parse_license_id(&req.license_id)?;
    let device_id = parse_device_id(&req.device_id)?;

    let outcome = state
        .license_service
        .deactivate(license_id, device_id)
        .await?;

    Ok(Json(DeactivateLicenseResponse {
        status: "deactivated".to_string(),
        devices_active: outcome.devices_active,
    }))
}
