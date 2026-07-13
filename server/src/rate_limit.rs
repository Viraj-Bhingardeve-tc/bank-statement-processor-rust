//! Rate limiting middleware (production readiness audit — "no rate
//! limiting exists anywhere; `/login` is fully exposed to brute force,
//! `/validate-license` to flooding"). Two independent, keyed rate
//! limiters built on `governor`'s GCRA algorithm:
//!
//! - [`login_rate_limit`] — keyed by client IP, applied to `POST /login`
//!   only (brute-force protection, `PHASE4_DESIGN.md` §5: "`/login`
//!   limited per-IP").
//! - [`device_rate_limit`] — keyed by the `device_id` field in the JSON
//!   request body, applied to `POST /validate-license` and (since Phase
//!   4J.7) `POST /heartbeat` (`PHASE4_DESIGN.md` §5: "`/validate-license`
//!   and `/heartbeat` limited per-`device_id`") — this middleware and the
//!   shared [`RateLimiters::device`] instance were built endpoint-agnostic
//!   from the start (keyed purely on a `device_id` field in the body,
//!   nothing `/validate-license`-specific), so wiring `/heartbeat` onto it
//!   in Phase 4J.7 was a one-line `.route(...)` addition to the same
//!   already-rate-limited sub-router (`routes::license::router`) — not a
//!   new limiter with its own, separate budget.
//!
//! **Why plain `governor` instead of `tower_governor`:** `tower_governor`
//! 0.8 depends on `axum = "0.8"`, which conflicts with this crate's pinned
//! `axum = "0.7"` — a real major-version mismatch (different, incompatible
//! `Request`/`Response`/`Router` types), not just an integration nicety.
//! `governor` itself has zero HTTP-framework dependency, so it composes
//! cleanly with any axum version via a small hand-written
//! `axum::middleware::from_fn_with_state` layer — the middleware here is
//! that thin composition glue, not a reimplementation of rate limiting
//! itself (the actual limiting algorithm/state is entirely `governor`'s).

use crate::routes::error::ApiError;
use crate::state::AppState;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use governor::{DefaultKeyedRateLimiter, Quota};
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroU32;
use std::sync::Arc;

/// `POST /login` — brute-force protection, keyed by client IP
/// (`PHASE4_DESIGN.md` §5). A burst of this many requests is allowed
/// immediately, replenishing at one request per `60/N` seconds thereafter
/// (`governor::Quota::per_minute`'s documented burst semantics) — plenty
/// for a genuine user mistyping a password a couple of times, tight enough
/// to make a credential-stuffing loop impractical.
pub const LOGIN_REQUESTS_PER_MINUTE: u32 = 5;

/// `POST /validate-license` and `POST /heartbeat` — keyed by `device_id`,
/// one shared budget across both endpoints (`PHASE4_DESIGN.md` §5). A
/// single already-activated device calling `/validate-license` once per
/// app launch, plus an occasional manual retry, sits far below this; a
/// device flooding either endpoint (or
/// splitting traffic across both to try to double its effective rate)
/// hits the same shared limit either way.
pub const DEVICE_REQUESTS_PER_MINUTE: u32 = 30;

fn quota_per_minute(requests_per_minute: u32) -> Quota {
    Quota::per_minute(
        NonZeroU32::new(requests_per_minute).expect("rate limit constants must be nonzero"),
    )
}

/// The two rate limiters, constructed once per [`AppState`] (i.e. once per
/// process in production; once per test in the test suite, so tests never
/// share — and never flake on — one another's rate-limit state). Each
/// field is independently `Arc`-wrapped and cheap to `Clone` alongside the
/// rest of `AppState`.
#[derive(Clone)]
pub struct RateLimiters {
    login: Arc<DefaultKeyedRateLimiter<IpAddr>>,
    device: Arc<DefaultKeyedRateLimiter<String>>,
}

impl RateLimiters {
    pub fn new() -> Self {
        RateLimiters {
            login: Arc::new(DefaultKeyedRateLimiter::keyed(quota_per_minute(
                LOGIN_REQUESTS_PER_MINUTE,
            ))),
            device: Arc::new(DefaultKeyedRateLimiter::keyed(quota_per_minute(
                DEVICE_REQUESTS_PER_MINUTE,
            ))),
        }
    }
}

impl Default for RateLimiters {
    fn default() -> Self {
        Self::new()
    }
}

/// `axum::middleware::from_fn_with_state` layer for `POST /login`, keyed
/// by the request's peer IP (`ConnectInfo`, populated by
/// `main.rs`'s `into_make_service_with_connect_info::<SocketAddr>()`).
///
/// Uses `Option<ConnectInfo<SocketAddr>>` rather than requiring it: if the
/// connection info extension is absent — the server wasn't served via
/// `into_make_service_with_connect_info` (a wiring bug), or a test built
/// the request directly without setting it — this fails *open* (the
/// request proceeds un-rate-limited) rather than turning a missing
/// extension into a hard error for every login attempt. Failing open here
/// only matters for defense-in-depth on top of Argon2's own cost and the
/// Phase 4J.5 timing fix; it never weakens `AuthService::login`'s actual
/// credential check.
///
/// **Known caveat, not fixed here (out of scope for this phase):** behind
/// Caddy (this crate's only supported deployment topology,
/// `PHASE4_DESIGN.md` §8), the peer address this process sees is Caddy's
/// own container IP, not the original client's, unless Caddy is
/// separately configured to forward the real address (its default
/// `reverse_proxy` behavior already does via `X-Forwarded-For`) *and* this
/// middleware is updated to read and trust that header instead of the raw
/// peer address. Not implemented here since blindly trusting a
/// client-supplied header without a trusted-proxy story would make the
/// limiter trivially bypassable — a real follow-up, not silently assumed
/// away.
pub async fn login_rate_limit(
    State(state): State<AppState>,
    peer: Option<ConnectInfo<SocketAddr>>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(ConnectInfo(addr)) = peer {
        if state.rate_limiters.login.check_key(&addr.ip()).is_err() {
            tracing::warn!(client_ip = %addr.ip(), "rate limit exceeded for /login");
            return Err(ApiError::RateLimited);
        }
    }
    Ok(next.run(req).await)
}

/// `axum::middleware::from_fn_with_state` layer for `POST /validate-license`
/// and `POST /heartbeat` — keyed by the `device_id` field in the JSON
/// request body. Buffers the body to read `device_id`, then reconstructs
/// an identical request for the real handler.
///
/// A body that isn't valid JSON, or has no string `device_id` field, is
/// passed through unchanged and *not* rate-limited here at all — the
/// handler's own `Json<...>` extractor still rejects a malformed body
/// exactly as it always has (`400 INVALID_REQUEST`/similar), so this
/// middleware never changes what a malformed request gets back; it only
/// ever adds a `429` for a well-formed request that's over budget.
pub async fn device_rate_limit(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => {
            // Body couldn't even be read — let the handler's own
            // extraction fail exactly the way it always would have on an
            // unreadable body.
            return Ok(next.run(Request::from_parts(parts, Body::empty())).await);
        }
    };

    if let Some(device_id) = extract_device_id(&bytes) {
        if state.rate_limiters.device.check_key(&device_id).is_err() {
            tracing::warn!(
                device_id = %device_id,
                "rate limit exceeded for a device-keyed endpoint"
            );
            return Err(ApiError::RateLimited);
        }
    }

    let req = Request::from_parts(parts, Body::from(bytes));
    Ok(next.run(req).await)
}

fn extract_device_id(bytes: &Bytes) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    value.get("device_id")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn login_limiter_allows_the_documented_burst_then_blocks() {
        let limiters = RateLimiters::new();
        let ip: IpAddr = "203.0.113.1".parse().unwrap();

        for attempt in 0..LOGIN_REQUESTS_PER_MINUTE {
            assert!(
                limiters.login.check_key(&ip).is_ok(),
                "request {attempt} within the burst budget must be allowed"
            );
        }
        assert!(
            limiters.login.check_key(&ip).is_err(),
            "the request past the burst budget must be rejected"
        );
    }

    #[test]
    fn device_limiter_allows_the_documented_burst_then_blocks() {
        let limiters = RateLimiters::new();
        let device_id = Uuid::new_v4().to_string();

        for attempt in 0..DEVICE_REQUESTS_PER_MINUTE {
            assert!(
                limiters.device.check_key(&device_id).is_ok(),
                "request {attempt} within the burst budget must be allowed"
            );
        }
        assert!(
            limiters.device.check_key(&device_id).is_err(),
            "the request past the burst budget must be rejected"
        );
    }

    #[test]
    fn different_device_ids_have_independent_budgets() {
        let limiters = RateLimiters::new();
        let device_a = Uuid::new_v4().to_string();
        let device_b = Uuid::new_v4().to_string();

        for _ in 0..DEVICE_REQUESTS_PER_MINUTE {
            assert!(limiters.device.check_key(&device_a).is_ok());
        }
        assert!(
            limiters.device.check_key(&device_a).is_err(),
            "device_a must now be over budget"
        );
        assert!(
            limiters.device.check_key(&device_b).is_ok(),
            "device_b must have its own, untouched budget"
        );
    }

    #[test]
    fn different_client_ips_have_independent_budgets() {
        let limiters = RateLimiters::new();
        let ip_a: IpAddr = "203.0.113.10".parse().unwrap();
        let ip_b: IpAddr = "203.0.113.20".parse().unwrap();

        for _ in 0..LOGIN_REQUESTS_PER_MINUTE {
            assert!(limiters.login.check_key(&ip_a).is_ok());
        }
        assert!(
            limiters.login.check_key(&ip_a).is_err(),
            "ip_a must now be over budget"
        );
        assert!(
            limiters.login.check_key(&ip_b).is_ok(),
            "ip_b must have its own, untouched budget"
        );
    }

    /// Proves the underlying mechanism directly at the `governor` level:
    /// two logical callers consuming from the *same* `RateLimiters::device`
    /// instance share one budget, regardless of which one exhausts it —
    /// this is exactly what makes `/validate-license` and `/heartbeat`
    /// share a budget over HTTP (see `tests/rate_limit_flow.rs`'s
    /// `heartbeat_shares_its_rate_limit_budget_with_validate_license` for
    /// the HTTP-level proof of the same thing, now that `/heartbeat` is a
    /// real route as of Phase 4J.7).
    #[test]
    fn a_shared_device_limiter_instance_pools_budget_across_two_logical_callers() {
        let limiters = RateLimiters::new();
        let device_id = Uuid::new_v4().to_string();

        // Simulates `/validate-license` consuming most of the budget...
        for _ in 0..DEVICE_REQUESTS_PER_MINUTE - 1 {
            assert!(limiters.device.check_key(&device_id).is_ok());
        }
        // ...and a *different* logical caller (standing in for
        // `/heartbeat`, once it exists) consuming from the same instance
        // finds only the one remaining slot, then is rejected — proving
        // the budget is genuinely shared, not per-caller.
        assert!(
            limiters.device.check_key(&device_id).is_ok(),
            "the one remaining slot must still be usable by the second caller"
        );
        assert!(
            limiters.device.check_key(&device_id).is_err(),
            "the shared budget must now be exhausted for both callers"
        );
    }
}
