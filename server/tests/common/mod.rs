//! Shared test helpers for this crate's integration tests. Lives under
//! `tests/common/` (not `tests/common.rs`) specifically so `cargo test`
//! doesn't treat it as its own top-level test binary.

use license_server::config::{
    AppConfig, DatabaseConfig, PaymentConfig, ReconciliationConfig, Secret, ServerConfig,
};

/// A config pointing `DATABASE_URL` at a loopback address nothing listens
/// on — `db::build_pool` (lazy) accepts it without any network attempt, and
/// any test that actually queries the pool gets a fast, deterministic
/// connection-refused failure rather than hanging or needing a real
/// Postgres. See `routes::ready`'s doc comment for which tests need a real
/// database instead. Razorpay settings are left `None` (unconfigured) —
/// tests that need them override via
/// `AppConfig { payment: PaymentConfig { razorpay_webhook_secret: ..,
/// ..common::test_config().payment }, ..common::test_config() }`.
pub fn test_config() -> AppConfig {
    AppConfig {
        server: ServerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            log_filter: "info".to_string(),
            // Production Hardening, Finding H3: empty by default, matching
            // every deployment that hasn't set TRUSTED_PROXY_CIDRS — tests
            // needing a trusted proxy override via
            // `ServerConfig { trusted_proxies: vec![...], ..common::test_config().server }`.
            trusted_proxies: Vec::new(),
        },
        database: DatabaseConfig {
            url: Secret::new("postgres://user:pass@127.0.0.1:1/nonexistent_db".to_string()),
            max_connections: 5,
        },
        payment: PaymentConfig {
            razorpay_key_id: None,
            razorpay_key_secret: None,
            razorpay_webhook_secret: None,
            razorpay_monthly_plan_id: None,
            razorpay_yearly_plan_id: None,
        },
        reconciliation: ReconciliationConfig {
            interval_secs: 15 * 60,
            batch_size: 100,
            max_age_hours: 2,
        },
    }
}
