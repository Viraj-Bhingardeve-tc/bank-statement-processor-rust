//! Shared application state, injected into handlers via axum's `State`
//! extractor.
//!
//! Phase 4C.1 adds the database pool alongside the process config. A
//! Razorpay HTTP client lands here in a later phase (`PHASE4_DESIGN.md`
//! §1.2's "External" layer) — each handler extracts only the fields it
//! actually needs via `State<AppState>`, so adding a field here never
//! forces every existing handler to change.

use crate::config::AppConfig;
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    /// Already cheaply `Clone`-able internally (`sqlx::Pool` wraps its own
    /// `Arc`) — stored directly rather than behind another `Arc`.
    pub db_pool: PgPool,
}

impl AppState {
    pub fn new(config: AppConfig, db_pool: PgPool) -> Self {
        AppState {
            config: Arc::new(config),
            db_pool,
        }
    }
}
