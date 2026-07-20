//! Admin-only route guard (Module 2: Admin Authentication).
//!
//! No admin-only endpoint exists yet — `require_admin` is the reusable
//! `route_layer` middleware a later module's admin routes will wrap
//! themselves in, the same pattern `routes::auth::require_session`
//! established for account-scoped ones. Building the guard ahead of any
//! route that needs it mirrors how Module 1 (`service::audit_service`)
//! landed its full write path before anything read it back.

use crate::routes::auth::{extract_bearer_token, AuthenticatedSession};
use crate::routes::error::ApiError;
use crate::state::AppState;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

/// Resolves the `Authorization: Bearer <token>` header the same way
/// `routes::auth::require_session` does, but via
/// `AuthService::require_admin` instead of `validate_session` — rejecting
/// with `403 FORBIDDEN` (`ApiError::Forbidden`) for a valid session whose
/// account isn't an `Admin`, on top of `require_session`'s existing
/// `401 UNAUTHORIZED` cases (missing/malformed/unknown/expired/revoked
/// token). Wrap an admin-only route in this instead of `require_session`,
/// never both — `require_admin` already re-does everything
/// `validate_session` does.
pub async fn require_admin(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = extract_bearer_token(req.headers())?;
    let session = state.auth_service.require_admin(token).await?;
    req.extensions_mut().insert(AuthenticatedSession(session));
    Ok(next.run(req).await)
}
