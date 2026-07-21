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
//!   new limiter with its own, separate budget. **Not affected by Finding
//!   H3 below** — its key is a body field, never derived from the TCP
//!   peer address, so there's no proxy-IP problem for it to have.
//!
//! **Production Hardening, Finding H3:** [`login_rate_limit`] used to key
//! on the raw TCP peer IP unconditionally — behind a reverse proxy (this
//! crate's own documented Caddy topology, `PHASE4_DESIGN.md` §8), that's
//! always the proxy's own address for every real client, collapsing
//! everyone into one shared budget (or letting one attacker exhaust
//! everyone else's). [`resolve_client_ip`] is the fix: it only reads
//! `X-Forwarded-For` when the peer is inside the operator-configured
//! `TRUSTED_PROXY_CIDRS` (`config::ServerConfig::trusted_proxies`) —
//! see that function's own doc comment for the full "which header entry
//! to trust" reasoning. Unset (the default), this is a no-op: every
//! deployment that hasn't opted in behaves exactly as before.
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
use ipnet::IpNet;
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
/// **Production Hardening, Finding H3 (previously a known, documented
/// caveat):** behind Caddy (this crate's only supported deployment
/// topology, `PHASE4_DESIGN.md` §8), the peer address this process sees is
/// Caddy's own container IP, not the original client's — every real
/// client collapsed into one shared budget, or one attacker could exhaust
/// everyone else's. `resolve_client_ip` is the fix: `X-Forwarded-For` is
/// now read, but *only* when the direct TCP peer is inside the operator-
/// configured `TRUSTED_PROXY_CIDRS` (`config::ServerConfig::
/// trusted_proxies`) — blindly trusting a client-supplied header with no
/// trusted-proxy story would make the limiter trivially bypassable, which
/// is exactly why this was deferred rather than done naively before.
pub async fn login_rate_limit(
    State(state): State<AppState>,
    peer: Option<ConnectInfo<SocketAddr>>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(ConnectInfo(addr)) = peer {
        let forwarded_for = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok());
        let client_ip = resolve_client_ip(
            addr.ip(),
            &state.config.server.trusted_proxies,
            forwarded_for,
        );

        if state.rate_limiters.login.check_key(&client_ip).is_err() {
            tracing::warn!(
                client_ip = %client_ip,
                peer_ip = %addr.ip(),
                "rate limit exceeded for /login"
            );
            return Err(ApiError::RateLimited);
        }
    }
    Ok(next.run(req).await)
}

/// Production Hardening, Finding H3: resolves the "real" client IP a rate
/// limiter should key on, given the raw TCP peer address, the operator's
/// configured trusted-proxy CIDR list, and the request's own
/// `X-Forwarded-For` header value (if any).
///
/// **If `peer_ip` is not inside `trusted_proxies` — including the default,
/// empty list every deployment gets unless it explicitly sets
/// `TRUSTED_PROXY_CIDRS`** — this always returns `peer_ip` unchanged,
/// `X-Forwarded-For` is never even inspected. This is what "never trust
/// forwarded headers coming directly from the Internet" means in practice:
/// an untrusted caller gets zero influence over its own rate-limit key by
/// sending a header, no matter what it contains.
///
/// When the peer *is* trusted, `X-Forwarded-For`'s comma-separated entries
/// are walked **right to left** — the entries closest to this server are
/// the ones a trusted hop itself appended (or, for a single-hop topology
/// like this crate's own documented Caddy deployment, replaced the header
/// with entirely) and can vouch for; anything a client prepended to that
/// header *before* it ever reached the first trusted hop is exactly the
/// forgeable value the "never trust" rule warns about. Concretely: each
/// entry, from the right, is checked — if it falls inside
/// `trusted_proxies` itself (another hop in a trusted proxy chain), it's
/// skipped and the walk continues left; the first entry that ISN'T itself
/// a trusted proxy's address is treated as the real client. This reduces
/// to "just take the only entry" when the header has exactly one value
/// (the common case for a single direct-facing trusted proxy). An empty,
/// missing, entirely-unparseable, or entirely-trusted-proxies-only header
/// falls back to `peer_ip` rather than propagating a malformed or absent
/// value into the limiter.
fn resolve_client_ip(
    peer_ip: IpAddr,
    trusted_proxies: &[IpNet],
    forwarded_for: Option<&str>,
) -> IpAddr {
    let is_trusted_proxy = |ip: &IpAddr| trusted_proxies.iter().any(|net| net.contains(ip));

    if !is_trusted_proxy(&peer_ip) {
        return peer_ip;
    }

    let Some(header) = forwarded_for else {
        return peer_ip;
    };

    header
        .split(',')
        .rev()
        .filter_map(|entry| entry.trim().parse::<IpAddr>().ok())
        .find(|ip| !is_trusted_proxy(ip))
        .unwrap_or(peer_ip)
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

    // ── Production Hardening, Finding H3: resolve_client_ip ──────────────

    fn cidr(s: &str) -> IpNet {
        s.parse().unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn normal_client_with_no_trusted_proxies_configured_uses_the_peer_ip() {
        let peer = ip("198.51.100.7");
        let resolved = resolve_client_ip(peer, &[], None);
        assert_eq!(resolved, peer);
    }

    #[test]
    fn normal_client_with_a_forwarded_header_but_no_trusted_proxies_still_uses_the_peer_ip() {
        // The default, behavior-preserving case: even a well-formed header
        // is ignored entirely when nothing is configured as trusted.
        let peer = ip("198.51.100.7");
        let resolved = resolve_client_ip(peer, &[], Some("203.0.113.9"));
        assert_eq!(resolved, peer);
    }

    #[test]
    fn untrusted_proxy_sending_a_forged_forwarded_header_is_ignored() {
        // The peer is real (some IP on the internet), just not inside
        // `trusted_proxies` — a forged header from it must have zero
        // effect, exactly the "never trust forwarded headers coming
        // directly from the Internet" requirement.
        let trusted = vec![cidr("127.0.0.1/32")];
        let peer = ip("203.0.113.66"); // not in `trusted`
        let resolved = resolve_client_ip(peer, &trusted, Some("10.0.0.1"));
        assert_eq!(
            resolved, peer,
            "an untrusted peer's forwarded header must never override its own IP"
        );
    }

    #[test]
    fn trusted_proxy_with_one_forwarded_ip_uses_that_ip() {
        let trusted = vec![cidr("127.0.0.1/32")];
        let peer = ip("127.0.0.1");
        let resolved = resolve_client_ip(peer, &trusted, Some("203.0.113.7"));
        assert_eq!(resolved, ip("203.0.113.7"));
    }

    #[test]
    fn trusted_proxy_with_multiple_forwarded_ips_uses_the_rightmost_untrusted_one() {
        // Neither forwarded entry is itself a trusted-proxy address here,
        // so the rightmost (the one closest to this server, and the one a
        // single direct-facing trusted hop itself appended or set) wins —
        // NOT the leftmost, which a client could have forged as a prefix
        // before the request ever reached the trusted hop.
        let trusted = vec![cidr("127.0.0.1/32")];
        let peer = ip("127.0.0.1");
        let resolved = resolve_client_ip(peer, &trusted, Some("9.9.9.9, 203.0.113.7"));
        assert_eq!(resolved, ip("203.0.113.7"));
    }

    #[test]
    fn a_trusted_proxy_entry_within_the_forwarded_chain_is_skipped() {
        // A deeper, multi-hop-trusted-proxy-chain case: the rightmost
        // entry (172.16.0.5) is itself inside `trusted_proxies` (another
        // trusted hop, not the client), so it's skipped and the next
        // entry to the left (203.0.113.7, not trusted) is used instead.
        let trusted = vec![cidr("127.0.0.1/32"), cidr("172.16.0.0/12")];
        let peer = ip("127.0.0.1");
        let resolved = resolve_client_ip(peer, &trusted, Some("203.0.113.7, 172.16.0.5"));
        assert_eq!(resolved, ip("203.0.113.7"));
    }

    #[test]
    fn a_forwarded_header_made_entirely_of_trusted_proxy_addresses_falls_back_to_the_peer_ip() {
        let trusted = vec![cidr("127.0.0.1/32"), cidr("172.16.0.0/12")];
        let peer = ip("127.0.0.1");
        let resolved = resolve_client_ip(peer, &trusted, Some("172.16.0.5, 172.16.0.6"));
        assert_eq!(resolved, peer);
    }

    #[test]
    fn trusted_proxy_with_a_malformed_forwarded_header_falls_back_to_the_peer_ip() {
        let trusted = vec![cidr("127.0.0.1/32")];
        let peer = ip("127.0.0.1");
        let resolved = resolve_client_ip(peer, &trusted, Some("not-an-ip-address"));
        assert_eq!(resolved, peer);
    }

    #[test]
    fn trusted_proxy_with_a_mix_of_malformed_and_valid_entries_skips_the_malformed_one() {
        let trusted = vec![cidr("127.0.0.1/32")];
        let peer = ip("127.0.0.1");
        let resolved = resolve_client_ip(peer, &trusted, Some("203.0.113.7, garbage"));
        assert_eq!(
            resolved,
            ip("203.0.113.7"),
            "an unparseable rightmost entry must be skipped, not treated as fatal"
        );
    }

    #[test]
    fn trusted_proxy_with_a_missing_forwarded_header_falls_back_to_the_peer_ip() {
        let trusted = vec![cidr("127.0.0.1/32")];
        let peer = ip("127.0.0.1");
        let resolved = resolve_client_ip(peer, &trusted, None);
        assert_eq!(resolved, peer);
    }

    #[test]
    fn trusted_proxy_with_an_empty_forwarded_header_falls_back_to_the_peer_ip() {
        let trusted = vec![cidr("127.0.0.1/32")];
        let peer = ip("127.0.0.1");
        let resolved = resolve_client_ip(peer, &trusted, Some(""));
        assert_eq!(resolved, peer);
    }

    #[test]
    fn ipv4_trusted_proxy_and_ipv4_forwarded_client_resolve_correctly() {
        let trusted = vec![cidr("10.0.0.0/8")];
        let peer = ip("10.1.2.3");
        let resolved = resolve_client_ip(peer, &trusted, Some("198.51.100.42"));
        assert_eq!(resolved, ip("198.51.100.42"));
    }

    #[test]
    fn ipv6_trusted_proxy_and_ipv6_forwarded_client_resolve_correctly() {
        let trusted = vec![cidr("::1/128")];
        let peer = ip("::1");
        let resolved = resolve_client_ip(peer, &trusted, Some("2001:db8::1"));
        assert_eq!(resolved, ip("2001:db8::1"));
    }

    #[test]
    fn ipv6_peer_outside_the_trusted_range_uses_the_peer_ip() {
        let trusted = vec![cidr("::1/128")];
        let peer = ip("2001:db8::dead:beef");
        let resolved = resolve_client_ip(peer, &trusted, Some("2001:db8::1"));
        assert_eq!(resolved, peer);
    }

    #[test]
    fn a_mixed_ipv4_trusted_range_does_not_trust_an_ipv6_peer() {
        // Guards against a subtle class of bug: an IPv4 CIDR must never
        // accidentally be treated as matching an IPv6 address (or vice
        // versa) through some implicit mapping.
        let trusted = vec![cidr("127.0.0.1/32")];
        let peer = ip("::1");
        let resolved = resolve_client_ip(peer, &trusted, Some("203.0.113.7"));
        assert_eq!(
            resolved, peer,
            "an IPv4-only trusted range must not match an IPv6 peer"
        );
    }

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
