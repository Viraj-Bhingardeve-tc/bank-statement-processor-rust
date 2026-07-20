//! `POST /login`, `POST /logout` — server-account authentication
//! (`API_SPECIFICATION.md`), plus `require_session`, the protected-route
//! middleware every future account-scoped endpoint (e.g. a later
//! `GET /subscription`) will reuse.
//!
//! Handlers are thin: parse/validate, call one `AuthService` method, map
//! the `Result` onto a response — all real logic lives in
//! `service::auth_service` (`PHASE4_DESIGN.md` §1.2), not here, same
//! pattern `routes::license` already established.

use crate::domain::Session;
use crate::rate_limit::login_rate_limit;
use crate::routes::error::ApiError;
use crate::state::AppState;
use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::HeaderMap;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use license_protocol::{LicenseSummary, LoginRequest, LoginResponse, SubscriptionSummary};
use serde::Serialize;

/// Injected into request extensions by `require_session` once a bearer
/// token has been resolved to a still-valid `Session` — handlers behind
/// the middleware extract this instead of re-validating anything
/// themselves.
#[derive(Clone)]
pub struct AuthenticatedSession(pub Session);

/// Takes `state` directly (unlike every other `routes::*::router()`, which
/// stay state-agnostic until `lib.rs::build_router`'s final
/// `.with_state()`) because `/logout` needs `require_session` — and,
/// since Phase 4J.6, `/login` needs `rate_limit::login_rate_limit` —
/// wired up with a *concrete* `AppState` at construction time —
/// `axum::middleware::from_fn` alone always resolves its state parameter
/// to `()`, which can't satisfy either middleware's own `State<AppState>`
/// extractor. `.with_state(state)` here fully resolves each sub-router to
/// `Router<()>`, which axum can then merge into the still-generic rest of
/// the app (`Router<()>` merges into any `Router<S>`).
pub fn router(state: AppState) -> Router<AppState> {
    let protected = Router::new()
        .route("/logout", post(logout))
        .route("/subscription", get(subscription))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ))
        .with_state(state.clone());

    // `/login` alone gets the per-IP rate limiter — `/logout` needs an
    // already-valid session to reach at all, so it isn't a brute-force
    // target the same way.
    let login_route = Router::new()
        .route("/login", post(login))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            login_rate_limit,
        ))
        .with_state(state);

    Router::new().merge(login_route).merge(protected)
}

/// `pub(crate)` so `routes::admin::require_admin` can reuse the exact same
/// bearer-token parsing instead of duplicating it.
pub(crate) fn extract_bearer_token(headers: &HeaderMap) -> Result<&str, ApiError> {
    let value = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or(ApiError::Unauthorized)?;
    value.strip_prefix("Bearer ").ok_or(ApiError::Unauthorized)
}

/// The protected-route middleware: resolves the `Authorization: Bearer
/// <token>` header to a `Session` via `AuthService::validate_session`,
/// rejecting with `401 UNAUTHORIZED` (`ApiError::Unauthorized`) if it's
/// missing, malformed, unknown, expired, or revoked — a route wrapped in
/// this never runs its handler for any of those cases.
pub async fn require_session(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = extract_bearer_token(req.headers())?;
    let session = state.auth_service.validate_session(token).await?;
    req.extensions_mut().insert(AuthenticatedSession(session));
    Ok(next.run(req).await)
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, ApiError> {
    let outcome = state.auth_service.login(&req.email, &req.password).await?;

    Ok(Json(LoginResponse {
        session_token: outcome.session_token,
        user_id: outcome.user_id.to_string(),
        expires_at: outcome.expires_at.to_rfc3339(),
    }))
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct LogoutResponse {}

async fn logout(
    State(state): State<AppState>,
    axum::Extension(AuthenticatedSession(session)): axum::Extension<AuthenticatedSession>,
) -> Result<Json<LogoutResponse>, ApiError> {
    state.auth_service.logout(session.id).await?;
    Ok(Json(LogoutResponse {}))
}

/// `GET /subscription`. Account-scoped (keyed by the bearer session's
/// `user_id`), unlike `/validate-license`/`/heartbeat`/`/refresh-license`
/// which are device-scoped (keyed by `license_id`+`device_id`, no session
/// at all) — this is why it lives behind `require_session` here rather
/// than alongside those in `routes::license`.
async fn subscription(
    State(state): State<AppState>,
    axum::Extension(AuthenticatedSession(session)): axum::Extension<AuthenticatedSession>,
) -> Result<Json<SubscriptionSummary>, ApiError> {
    let outcome = state
        .license_service
        .subscription_summary(session.user_id)
        .await?;

    Ok(Json(SubscriptionSummary {
        subscription_id: outcome.subscription.id.to_string(),
        plan_type: outcome.subscription.plan_type.as_str().to_string(),
        status: outcome.subscription.status.as_str().to_string(),
        current_period_end: outcome
            .subscription
            .current_period_end
            .map(|d| d.to_rfc3339()),
        auto_renew: outcome.subscription.auto_renew,
        licenses: outcome
            .licenses
            .into_iter()
            .map(|l| LicenseSummary {
                license_id: l.license_id.to_string(),
                status: l.status.as_str().to_string(),
                devices_active: l.devices_active,
                max_devices: i64::from(l.max_devices),
            })
            .collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logout_response_serializes_to_an_empty_object() {
        let json = serde_json::to_value(LogoutResponse {}).unwrap();
        assert_eq!(json, serde_json::json!({}));
    }
}
