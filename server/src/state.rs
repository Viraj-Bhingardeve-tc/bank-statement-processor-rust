//! Shared application state, injected into handlers via axum's `State`
//! extractor.
//!
//! Phase 4F adds `payment_service` alongside Phase 4D/4E's
//! `license_service`/`auth_service`, all constructed once here from the
//! Postgres-backed repositories (and, for payments, the Razorpay HTTP
//! client) so every handler shares one instance rather than building its
//! own per request — each handler extracts only the fields it actually
//! needs via `State<AppState>`, so adding a field here never forces every
//! existing handler to change.

use crate::config::AppConfig;
use crate::rate_limit::RateLimiters;
use crate::razorpay::HttpRazorpayClient;
use crate::repository::device::PgDeviceRepository;
use crate::repository::license::PgLicenseRepository;
use crate::repository::payment::PgPaymentRepository;
use crate::repository::payment_webhook_event::PgPaymentWebhookEventRepository;
use crate::repository::session::PgSessionRepository;
use crate::repository::subscription::PgSubscriptionRepository;
use crate::repository::user::PgUserRepository;
use crate::service::{AuthService, LicenseService, PaymentService};
use metrics_exporter_prometheus::PrometheusHandle;
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    /// Already cheaply `Clone`-able internally (`sqlx::Pool` wraps its own
    /// `Arc`) — stored directly rather than behind another `Arc`.
    pub db_pool: PgPool,
    pub license_service: Arc<LicenseService>,
    pub auth_service: Arc<AuthService>,
    pub payment_service: Arc<PaymentService>,
    /// The process-wide Prometheus recorder handle (`observability::handle`)
    /// — only `routes::metrics`'s `GET /metrics` handler actually calls
    /// `.render()` on it; every other metric call site instruments through
    /// the `metrics` crate's own macros instead of touching this field.
    pub metrics_handle: PrometheusHandle,
    /// The `/login` (per-IP) and `/validate-license` (per-`device_id`)
    /// rate limiters (Phase 4J.6) — constructed fresh per `AppState`
    /// (once per process in production; once per test in the test suite,
    /// so tests never share rate-limit state with one another).
    pub rate_limiters: RateLimiters,
}

impl AppState {
    pub fn new(config: AppConfig, db_pool: PgPool) -> Self {
        let license_service = Arc::new(LicenseService::new(
            Arc::new(PgLicenseRepository::new(db_pool.clone())),
            Arc::new(PgDeviceRepository::new(db_pool.clone())),
            Arc::new(PgSubscriptionRepository::new(db_pool.clone())),
        ));
        let auth_service = Arc::new(AuthService::new(
            Arc::new(PgUserRepository::new(db_pool.clone())),
            Arc::new(PgSessionRepository::new(db_pool.clone())),
        ));
        let razorpay_client = Arc::new(HttpRazorpayClient::new(
            config.razorpay_key_id.clone(),
            config.razorpay_key_secret.clone(),
            config.razorpay_monthly_plan_id.clone(),
            config.razorpay_yearly_plan_id.clone(),
        ));
        let payment_service = Arc::new(PaymentService::new(
            Arc::new(PgPaymentRepository::new(db_pool.clone())),
            Arc::new(PgPaymentWebhookEventRepository::new(db_pool.clone())),
            Arc::new(PgSubscriptionRepository::new(db_pool.clone())),
            Arc::new(PgLicenseRepository::new(db_pool.clone())),
            razorpay_client,
        ));

        AppState {
            config: Arc::new(config),
            db_pool,
            license_service,
            auth_service,
            payment_service,
            metrics_handle: crate::observability::handle(),
            rate_limiters: RateLimiters::new(),
        }
    }
}
