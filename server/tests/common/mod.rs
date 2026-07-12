//! Shared test helpers for this crate's integration tests. Lives under
//! `tests/common/` (not `tests/common.rs`) specifically so `cargo test`
//! doesn't treat it as its own top-level test binary.

use license_server::config::AppConfig;

/// A config pointing `DATABASE_URL` at a loopback address nothing listens
/// on — `db::build_pool` (lazy) accepts it without any network attempt, and
/// any test that actually queries the pool gets a fast, deterministic
/// connection-refused failure rather than hanging or needing a real
/// Postgres. See `routes::ready`'s doc comment for which tests need a real
/// database instead. Razorpay settings are left `None` (unconfigured) —
/// tests that need them override via `AppConfig { razorpay_key_id: ..,
/// ..test_config() }`.
pub fn test_config() -> AppConfig {
    AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        log_filter: "info".to_string(),
        database_url: "postgres://user:pass@127.0.0.1:1/nonexistent_db".to_string(),
        database_max_connections: 5,
        razorpay_key_id: None,
        razorpay_key_secret: None,
        razorpay_webhook_secret: None,
        razorpay_monthly_plan_id: None,
        razorpay_yearly_plan_id: None,
    }
}
