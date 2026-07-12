//! Prometheus-compatible metrics (Phase 4I.2 — `PHASE4_DESIGN.md` §8.3
//! "Operational properties": "a log-shipping sidecar or external monitoring
//! agent is a reasonable later addition, not required for Phase 4 itself" —
//! this is that addition, a pull-based `/metrics` endpoint rather than a
//! push agent, so any standard Prometheus server can scrape this process
//! directly with no extra infrastructure).
//!
//! Named `observability`, not `metrics`, specifically so this module never
//! shadows the `metrics` crate's own name inside this crate root (`lib.rs`
//! declares `pub mod observability;` — if it were instead `pub mod
//! metrics;`, every bare `metrics::counter!`/`gauge!` call site *inside
//! lib.rs itself* would silently resolve to this module instead of the
//! external crate).
//!
//! One global recorder for the whole process (`handle()`, backed by a
//! `OnceLock` so every `AppState::new` call — including the many
//! independent ones this crate's own test suite builds in the same process
//! — installs it at most once, instead of panicking on a second global
//! install). Instrumentation call sites elsewhere in this crate
//! (`track_http_metrics` below, `routes::payment`'s webhook handler,
//! `reconciliation.rs`) record through the `metrics` crate's own
//! `counter!`/`gauge!`/`histogram!` macros, which always go through
//! whichever recorder `handle()` installed — they never need to hold the
//! handle themselves. Only `routes::metrics` (the `GET /metrics` handler
//! itself) needs the `PrometheusHandle` directly, to call `.render()`.

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use std::sync::OnceLock;
use std::time::Instant;

// ── HTTP request metrics (`track_http_metrics`, below) ──────────────────────
pub const HTTP_REQUESTS_TOTAL: &str = "http_requests_total";
pub const HTTP_REQUEST_DURATION_SECONDS: &str = "http_request_duration_seconds";
pub const HTTP_REQUESTS_IN_FLIGHT: &str = "http_requests_in_flight";

// ── Webhook metrics (`routes::payment`) ──────────────────────────────────────
pub const WEBHOOK_REQUESTS_TOTAL: &str = "webhook_requests_total";
pub const WEBHOOK_EVENTS_TOTAL: &str = "webhook_events_total";

// ── Reconciliation job metrics (`reconciliation.rs`) ─────────────────────────
pub const RECONCILIATION_RUNS_TOTAL: &str = "reconciliation_runs_total";
pub const RECONCILIATION_PAYMENTS_CHECKED_TOTAL: &str = "reconciliation_payments_checked_total";
pub const RECONCILIATION_PAYMENTS_HEALED_TOTAL: &str = "reconciliation_payments_healed_total";

// ── Database pool metrics (`routes::metrics`, computed at scrape time) ──────
pub const DB_POOL_CONNECTIONS: &str = "db_pool_connections";
pub const DB_POOL_IDLE_CONNECTIONS: &str = "db_pool_idle_connections";

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Installs the global Prometheus recorder the first time this is called in
/// the process, and returns the (cheaply `Clone`-able) handle used to
/// render `/metrics` output. Every later call — including from a fresh
/// `AppState::new` built by a different test in the same process — returns
/// the already-installed handle rather than attempting (and panicking on)
/// a second global install.
pub fn handle() -> PrometheusHandle {
    HANDLE
        .get_or_init(|| {
            let handle = PrometheusBuilder::new()
                .install_recorder()
                .expect("failed to install the Prometheus metrics recorder");
            describe_metrics();
            handle
        })
        .clone()
}

/// Registers a human-readable description for every metric this crate
/// emits, so `/metrics`'s Prometheus text output carries `# HELP`/`# TYPE`
/// lines rather than bare series — self-documenting for whoever is reading
/// a scrape or writing an alerting rule, not just this file's own comments.
/// Note: a `describe_*` call alone does not make a metric appear in
/// `render()`'s output — only a metric that has actually been recorded at
/// least once (`counter!`/`gauge!`/`histogram!` called somewhere) shows up;
/// this only attaches the description to whichever of those series
/// eventually appear.
fn describe_metrics() {
    metrics::describe_counter!(
        HTTP_REQUESTS_TOTAL,
        "Total HTTP requests handled, labeled by method, path, and status."
    );
    metrics::describe_histogram!(
        HTTP_REQUEST_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "HTTP request duration in seconds, labeled by method, path, and status."
    );
    metrics::describe_gauge!(
        HTTP_REQUESTS_IN_FLIGHT,
        "Number of HTTP requests currently being handled."
    );
    metrics::describe_counter!(
        WEBHOOK_REQUESTS_TOTAL,
        "Total inbound Razorpay webhook HTTP calls, labeled by outcome."
    );
    metrics::describe_counter!(
        WEBHOOK_EVENTS_TOTAL,
        "Total successfully processed Razorpay webhook events, labeled by event_type."
    );
    metrics::describe_counter!(
        RECONCILIATION_RUNS_TOTAL,
        "Total payment reconciliation job runs, labeled by result."
    );
    metrics::describe_counter!(
        RECONCILIATION_PAYMENTS_CHECKED_TOTAL,
        "Total Razorpay payments inspected across all reconciliation runs."
    );
    metrics::describe_counter!(
        RECONCILIATION_PAYMENTS_HEALED_TOTAL,
        "Total payments healed (webhook never arrived) across all reconciliation runs."
    );
    metrics::describe_gauge!(
        DB_POOL_CONNECTIONS,
        "Current total connections (idle + in-use) held by the database pool."
    );
    metrics::describe_gauge!(
        DB_POOL_IDLE_CONNECTIONS,
        "Current idle connections held by the database pool."
    );
}

/// RAII guard for `http_requests_in_flight` — increments on construction,
/// decrements on drop, so a panicking handler still leaves the gauge
/// correct (a plain decrement call placed after `next.run(...).await`
/// would be skipped if that future's poll unwinds).
struct InFlightGuard;

impl InFlightGuard {
    fn start() -> Self {
        metrics::gauge!(HTTP_REQUESTS_IN_FLIGHT).increment(1.0);
        InFlightGuard
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        metrics::gauge!(HTTP_REQUESTS_IN_FLIGHT).decrement(1.0);
    }
}

/// Axum middleware recording `http_requests_total`/
/// `http_request_duration_seconds`/`http_requests_in_flight` for every
/// request. Added via `Router::layer` in `lib.rs::build_router`, same as
/// the existing `TraceLayer` — `MatchedPath` is available there (axum
/// inserts it into request extensions before outer `.layer()` middleware
/// runs; see that router's own tracing span, which reads request state the
/// same way).
///
/// Labels by the *matched route pattern* (e.g. `/activate-license`), not
/// the raw URI, so an unmatched/404 path never creates an unbounded set of
/// label values — falls back to the raw request path only for requests
/// that never matched a route at all.
pub async fn track_http_metrics(req: Request, next: Next) -> Response {
    let method = req.method().to_string();
    let path = req
        .extensions()
        .get::<MatchedPath>()
        .map(|matched| matched.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());

    let _in_flight = InFlightGuard::start();
    let started_at = Instant::now();

    let response = next.run(req).await;

    let elapsed = started_at.elapsed().as_secs_f64();
    let status = response.status().as_u16().to_string();

    metrics::counter!(
        HTTP_REQUESTS_TOTAL,
        "method" => method.clone(),
        "path" => path.clone(),
        "status" => status.clone(),
    )
    .increment(1);
    metrics::histogram!(
        HTTP_REQUEST_DURATION_SECONDS,
        "method" => method,
        "path" => path,
        "status" => status,
    )
    .record(elapsed);

    response
}
