//! Shared application state, injected into handlers via axum's `State`
//! extractor.
//!
//! Phase 4B only holds the process config. Later phases add a database
//! connection pool and a Razorpay HTTP client here (`PHASE4_DESIGN.md`
//! §1.2's "Data access" and "External" layers) — each handler extracts only
//! the fields it actually needs via `State<AppState>`, so adding a field
//! here never forces every existing handler to change.

use crate::config::AppConfig;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
}

impl AppState {
    pub fn new(config: AppConfig) -> Self {
        AppState {
            config: Arc::new(config),
        }
    }
}
