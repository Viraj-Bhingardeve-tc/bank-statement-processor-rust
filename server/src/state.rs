//! Shared application state, injected into handlers via axum's `State`
//! extractor.
//!
//! Phase 4E adds `auth_service` alongside Phase 4D's `license_service`,
//! both constructed once here from the Postgres-backed repositories so
//! every handler shares one instance rather than building its own per
//! request. A Razorpay HTTP client lands here in a later phase
//! (`PHASE4_DESIGN.md` §1.2's "External" layer) — each handler extracts
//! only the fields it actually needs via `State<AppState>`, so adding a
//! field here never forces every existing handler to change.

use crate::config::AppConfig;
use crate::repository::device::PgDeviceRepository;
use crate::repository::license::PgLicenseRepository;
use crate::repository::session::PgSessionRepository;
use crate::repository::subscription::PgSubscriptionRepository;
use crate::repository::user::PgUserRepository;
use crate::service::{AuthService, LicenseService};
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

        AppState {
            config: Arc::new(config),
            db_pool,
            license_service,
            auth_service,
        }
    }
}
