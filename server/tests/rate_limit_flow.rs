//! Integration tests for Phase 4J.6's rate limiting — exercises the
//! assembled router exactly as `main.rs` builds it, via
//! `tower::ServiceExt::oneshot`, same pattern every other integration test
//! in this crate uses.
//!
//! No real Postgres is needed: the rate limiter runs as middleware
//! *before* the handler, so a request that gets rejected with `429` never
//! reaches the database at all. Requests that stay within budget do reach
//! the (deliberately unreachable, per `common::test_config`) database and
//! come back as `500`s — this file only ever asserts "not 429" for those,
//! never a specific success status, so it needs no real database and
//! isn't `#[ignore]`d.
//!
//! `governor::Quota::per_minute(N)` allows a full burst of `N` requests
//! immediately (see `rate_limit.rs`'s own doc comment) and only starts
//! blocking on request `N + 1`. Crucially, that budget is tracked against
//! *real elapsed wall-clock time*, not "requests made so far" — and every
//! request that passes the limiter still reaches the handler, which tries
//! (and, per the deliberately-unreachable test database, fails) a real DB
//! acquire with a multi-second timeout. Sending the burst *sequentially*
//! and awaiting each response before firing the next would let several
//! seconds of real time elapse between requests, long enough for governor
//! to naturally replenish tokens — masking a real bug behind a
//! false-negative "never blocks" result. So the burst below is fired as a
//! batch of concurrently-spawned tasks (via `tokio::spawn`), with
//! `tokio::task::yield_now` used to let every task's synchronous
//! `check_key` call run *before* any of them progress into their slow,
//! `.await`ing DB path — this pins down exactly how many tokens the burst
//! consumes regardless of how long the resulting HTTP responses take to
//! resolve.

mod common;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use license_protocol::{HeartbeatRequest, LoginRequest, ValidateLicenseRequest};
use license_server::config::{AppConfig, ServerConfig};
use license_server::rate_limit::{DEVICE_REQUESTS_PER_MINUTE, LOGIN_REQUESTS_PER_MINUTE};
use license_server::state::AppState;
use license_server::{build_router, db};
use std::net::SocketAddr;
use tokio::task::JoinHandle;
use tower::ServiceExt;
use uuid::Uuid;

fn app() -> axum::Router {
    let config = common::test_config();
    let pool = db::build_pool(
        config.database.url.expose_secret(),
        config.database.max_connections,
    )
    .unwrap();
    build_router(AppState::new(config, pool))
}

/// Production Hardening, Finding H3: like `app()`, but with
/// `TRUSTED_PROXY_CIDRS` set to `cidrs` — used by the proxy-aware
/// `/login` tests below.
fn app_with_trusted_proxies(cidrs: &[&str]) -> axum::Router {
    let config = AppConfig {
        server: ServerConfig {
            trusted_proxies: cidrs.iter().map(|c| c.parse().unwrap()).collect(),
            ..common::test_config().server
        },
        ..common::test_config()
    };
    let pool = db::build_pool(
        config.database.url.expose_secret(),
        config.database.max_connections,
    )
    .unwrap();
    build_router(AppState::new(config, pool))
}

async fn post_json_from<Req: serde::Serialize>(
    app: &axum::Router,
    uri: &str,
    body: &Req,
    peer: SocketAddr,
) -> StatusCode {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));

    let response = app.clone().oneshot(request).await.unwrap();
    response.status()
}

async fn post_json<Req: serde::Serialize>(app: &axum::Router, uri: &str, body: &Req) -> StatusCode {
    // No client IP needed for device-keyed endpoints — `device_rate_limit`
    // doesn't read `ConnectInfo` at all.
    post_json_from(app, uri, body, "203.0.113.99:0".parse().unwrap()).await
}

async fn post_json_from_and_parse<Req: serde::Serialize>(
    app: &axum::Router,
    uri: &str,
    body: &Req,
    peer: SocketAddr,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

async fn post_json_and_parse<Req: serde::Serialize>(
    app: &axum::Router,
    uri: &str,
    body: &Req,
) -> (StatusCode, serde_json::Value) {
    post_json_from_and_parse(app, uri, body, "203.0.113.99:0".parse().unwrap()).await
}

/// Fires a single request on its own spawned task, returning a handle
/// rather than the resolved status — used so a whole batch can be
/// dispatched without any one of them blocking the others on its (possibly
/// several-seconds-long) round trip through the deliberately-unreachable
/// test database.
fn spawn_json_request(
    app: axum::Router,
    uri: String,
    body: Vec<u8>,
    peer: SocketAddr,
) -> JoinHandle<StatusCode> {
    tokio::spawn(async move {
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));

        app.oneshot(request).await.unwrap().status()
    })
}

/// Dispatches `count` concurrent requests and returns their still-pending
/// join handles, after yielding enough times to guarantee every one of
/// them has already run its synchronous `check_key` call (the very first
/// thing `login_rate_limit`/`device_rate_limit` do, before any `.await`) —
/// see this file's module doc comment for why that matters. Callers can
/// then immediately issue a follow-up request and know exactly how much of
/// the budget the batch has already consumed, regardless of how long the
/// batch's own HTTP responses take to resolve.
async fn fire_concurrent_burst<Req: serde::Serialize>(
    app: &axum::Router,
    uri: &str,
    body: &Req,
    peer: SocketAddr,
    count: u32,
) -> Vec<JoinHandle<StatusCode>> {
    let bytes = serde_json::to_vec(body).unwrap();
    let handles: Vec<_> = (0..count)
        .map(|_| spawn_json_request(app.clone(), uri.to_string(), bytes.clone(), peer))
        .collect();

    for _ in 0..(count as usize + 10) {
        tokio::task::yield_now().await;
    }

    handles
}

// ── Production Hardening, Finding H3 ─────────────────────────────────────

async fn post_json_from_with_forwarded_for<Req: serde::Serialize>(
    app: &axum::Router,
    uri: &str,
    body: &Req,
    peer: SocketAddr,
    forwarded_for: &str,
) -> StatusCode {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-forwarded-for", forwarded_for)
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));

    app.clone().oneshot(request).await.unwrap().status()
}

fn spawn_json_request_with_forwarded_for(
    app: axum::Router,
    uri: String,
    body: Vec<u8>,
    peer: SocketAddr,
    forwarded_for: String,
) -> JoinHandle<StatusCode> {
    tokio::spawn(async move {
        let mut request = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("x-forwarded-for", forwarded_for)
            .body(Body::from(body))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));

        app.oneshot(request).await.unwrap().status()
    })
}

/// Same concurrency-safety reasoning as `fire_concurrent_burst`, plus a
/// fixed `X-Forwarded-For` value on every request in the batch.
async fn fire_concurrent_burst_with_forwarded_for<Req: serde::Serialize>(
    app: &axum::Router,
    uri: &str,
    body: &Req,
    peer: SocketAddr,
    forwarded_for: &str,
    count: u32,
) -> Vec<JoinHandle<StatusCode>> {
    let bytes = serde_json::to_vec(body).unwrap();
    let handles: Vec<_> = (0..count)
        .map(|_| {
            spawn_json_request_with_forwarded_for(
                app.clone(),
                uri.to_string(),
                bytes.clone(),
                peer,
                forwarded_for.to_string(),
            )
        })
        .collect();

    for _ in 0..(count as usize + 10) {
        tokio::task::yield_now().await;
    }

    handles
}

fn login_request() -> LoginRequest {
    LoginRequest {
        email: "someone@example.com".to_string(),
        password: "whatever".to_string(),
    }
}

fn validate_request(device_id: &str) -> ValidateLicenseRequest {
    ValidateLicenseRequest {
        license_id: "1".to_string(),
        device_id: device_id.to_string(),
        machine_fingerprint: "fp".to_string(),
        client_clock: chrono::Utc::now().to_rfc3339(),
    }
}

fn heartbeat_request(device_id: &str) -> HeartbeatRequest {
    HeartbeatRequest {
        license_id: "1".to_string(),
        device_id: device_id.to_string(),
    }
}

#[tokio::test]
async fn login_returns_429_rate_limited_after_the_burst_is_exhausted() {
    let app = app();
    let peer: SocketAddr = "198.51.100.1:12345".parse().unwrap();
    let req = login_request();

    let burst = fire_concurrent_burst(&app, "/login", &req, peer, LOGIN_REQUESTS_PER_MINUTE).await;

    let (status, body) = post_json_from_and_parse(&app, "/login", &req, peer).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"]["code"], "RATE_LIMITED");
    assert_eq!(body["ok"], false);

    for (attempt, handle) in burst.into_iter().enumerate() {
        let status = handle.await.unwrap();
        assert_ne!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "burst request {attempt} within budget must not be rate-limited"
        );
    }
}

#[tokio::test]
async fn login_from_a_different_client_ip_is_unaffected_by_another_ips_exhausted_budget() {
    let app = app();
    let exhausted_peer: SocketAddr = "198.51.100.2:1".parse().unwrap();
    let fresh_peer: SocketAddr = "198.51.100.3:1".parse().unwrap();
    let req = login_request();

    let burst = fire_concurrent_burst(
        &app,
        "/login",
        &req,
        exhausted_peer,
        LOGIN_REQUESTS_PER_MINUTE,
    )
    .await;

    let status = post_json_from(&app, "/login", &req, exhausted_peer).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "sanity check: this IP's budget must actually be exhausted"
    );

    let status = post_json_from(&app, "/login", &req, fresh_peer).await;
    assert_ne!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a different client IP must have its own, untouched budget"
    );

    for handle in burst {
        let _ = handle.await.unwrap();
    }
}

#[tokio::test]
async fn validate_license_returns_429_rate_limited_after_the_burst_is_exhausted() {
    let app = app();
    let device_id = Uuid::new_v4().to_string();
    let req = validate_request(&device_id);
    let peer: SocketAddr = "203.0.113.99:0".parse().unwrap();

    let burst = fire_concurrent_burst(
        &app,
        "/validate-license",
        &req,
        peer,
        DEVICE_REQUESTS_PER_MINUTE,
    )
    .await;

    let (status, body) = post_json_and_parse(&app, "/validate-license", &req).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"]["code"], "RATE_LIMITED");

    for (attempt, handle) in burst.into_iter().enumerate() {
        let status = handle.await.unwrap();
        assert_ne!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "burst request {attempt} within budget must not be rate-limited"
        );
    }
}

#[tokio::test]
async fn validate_license_for_a_different_device_id_is_unaffected_by_another_devices_exhausted_budget(
) {
    let app = app();
    let exhausted_device = Uuid::new_v4().to_string();
    let fresh_device = Uuid::new_v4().to_string();
    let peer: SocketAddr = "203.0.113.99:0".parse().unwrap();

    let burst = fire_concurrent_burst(
        &app,
        "/validate-license",
        &validate_request(&exhausted_device),
        peer,
        DEVICE_REQUESTS_PER_MINUTE,
    )
    .await;

    let status = post_json(
        &app,
        "/validate-license",
        &validate_request(&exhausted_device),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "sanity check: this device's budget must actually be exhausted"
    );

    let status = post_json(&app, "/validate-license", &validate_request(&fresh_device)).await;
    assert_ne!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a different device_id must have its own, untouched budget"
    );

    for handle in burst {
        let _ = handle.await.unwrap();
    }
}

/// Phase 4J.7: `/heartbeat` was wired onto the exact same
/// `device_rate_limit` middleware instance `/validate-license` already
/// used (`routes::license::router`'s `device_rate_limited_routes`
/// sub-router), not a second, independent limiter — this proves that
/// sharing holds over real HTTP requests, not just at the `governor` call
/// level (see `rate_limit.rs`'s own unit test for that lower-level proof).
#[tokio::test]
async fn heartbeat_shares_its_rate_limit_budget_with_validate_license() {
    let app = app();
    let device_id = Uuid::new_v4().to_string();
    let peer: SocketAddr = "203.0.113.99:0".parse().unwrap();

    // Exhaust the shared budget entirely through /validate-license...
    let burst = fire_concurrent_burst(
        &app,
        "/validate-license",
        &validate_request(&device_id),
        peer,
        DEVICE_REQUESTS_PER_MINUTE,
    )
    .await;

    // ...and confirm /heartbeat, for the exact same device_id, is now
    // rate-limited too.
    let (status, body) =
        post_json_and_parse(&app, "/heartbeat", &heartbeat_request(&device_id)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"]["code"], "RATE_LIMITED");

    for handle in burst {
        let _ = handle.await.unwrap();
    }
}

#[tokio::test]
async fn heartbeat_for_a_fresh_device_id_is_unaffected_by_validate_licenses_exhausted_budget() {
    let app = app();
    let exhausted_device = Uuid::new_v4().to_string();
    let fresh_device = Uuid::new_v4().to_string();
    let peer: SocketAddr = "203.0.113.99:0".parse().unwrap();

    let burst = fire_concurrent_burst(
        &app,
        "/validate-license",
        &validate_request(&exhausted_device),
        peer,
        DEVICE_REQUESTS_PER_MINUTE,
    )
    .await;

    let status = post_json(&app, "/heartbeat", &heartbeat_request(&fresh_device)).await;
    assert_ne!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a fresh device_id's heartbeat must be unaffected by another device's exhausted validate-license budget"
    );

    for handle in burst {
        let _ = handle.await.unwrap();
    }
}

#[tokio::test]
async fn a_malformed_body_on_validate_license_is_not_rate_limited_and_reaches_the_handler() {
    // `device_rate_limit` must pass a body it can't parse a device_id out
    // of straight through, unmodified, rather than erroring itself — the
    // handler's own `Json<...>` extractor is what should reject this, with
    // its usual error, not `RATE_LIMITED`.
    let app = app();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/validate-license")
                .header("content-type", "application/json")
                .body(Body::from("not valid json"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_ne!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

// ── Production Hardening, Finding H3 ─────────────────────────────────────

#[tokio::test]
async fn login_rate_limit_uses_the_forwarded_ip_when_the_peer_is_a_trusted_proxy() {
    let trusted_peer: SocketAddr = "198.51.100.50:1".parse().unwrap();
    let app = app_with_trusted_proxies(&["198.51.100.50/32"]);
    let req = login_request();

    // Exhaust the budget for forwarded client "203.0.113.1", all arriving
    // through the same trusted proxy peer...
    let burst = fire_concurrent_burst_with_forwarded_for(
        &app,
        "/login",
        &req,
        trusted_peer,
        "203.0.113.1",
        LOGIN_REQUESTS_PER_MINUTE,
    )
    .await;

    let status =
        post_json_from_with_forwarded_for(&app, "/login", &req, trusted_peer, "203.0.113.1").await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "sanity check: this forwarded client's budget must actually be exhausted"
    );

    // ...but a DIFFERENT forwarded client IP, through the exact same
    // trusted proxy peer, must have its own untouched budget — proving the
    // limiter's real key is now the forwarded IP, not the shared peer IP.
    let status =
        post_json_from_with_forwarded_for(&app, "/login", &req, trusted_peer, "203.0.113.2").await;
    assert_ne!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a different forwarded client IP must have its own, untouched budget"
    );

    for handle in burst {
        let _ = handle.await.unwrap();
    }
}

#[tokio::test]
async fn login_rate_limit_ignores_the_forwarded_header_when_the_peer_is_not_a_trusted_proxy() {
    // `trusted_proxies` is configured (non-empty), but this specific peer
    // isn't in it — proving "some proxy is trusted somewhere" doesn't
    // imply "every peer is trusted."
    let untrusted_peer: SocketAddr = "198.51.100.60:1".parse().unwrap();
    let app = app_with_trusted_proxies(&["127.0.0.1/32"]);
    let req = login_request();

    // Exhaust the budget while sending a different forged X-Forwarded-For
    // value on every request in the burst...
    let burst = fire_concurrent_burst_with_forwarded_for(
        &app,
        "/login",
        &req,
        untrusted_peer,
        "9.9.9.9",
        LOGIN_REQUESTS_PER_MINUTE,
    )
    .await;

    // ...then even a request claiming a brand-new forwarded IP must still
    // be rejected: the header must be ignored entirely for an untrusted
    // peer, so the real key was always the peer IP, which is now over
    // budget regardless of what it claims via the header.
    let status =
        post_json_from_with_forwarded_for(&app, "/login", &req, untrusted_peer, "1.2.3.4").await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "an untrusted peer's forged X-Forwarded-For must never grant a fresh budget"
    );

    for handle in burst {
        let _ = handle.await.unwrap();
    }
}
