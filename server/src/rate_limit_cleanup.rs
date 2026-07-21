//! Rate limiter memory-bound scheduler (Production Hardening, Finding H4 —
//! "the in-memory rate limiters grow forever").
//!
//! A `tokio::time::interval` background task spawned alongside the axum
//! listener at startup (`main.rs`), mirroring `reconciliation`'s own
//! scheduling pattern — kept deliberately thin, so the actual cleanup
//! mechanism (`rate_limit::RateLimiters::cleanup`, which is really just
//! `governor`'s own built-in `retain_recent`/`shrink_to_fit` housekeeping)
//! stays testable without a running scheduler.
//!
//! **Why periodic rather than per-request or fully lazy:** the
//! alternative this finding explicitly rules out is doing any O(n) scan on
//! every request — that would make every `/login` or `/validate-license`
//! call pay for cleaning up entries that have nothing to do with it. A
//! periodic sweep instead amortizes that O(n) cost (n = currently-tracked
//! distinct IPs/device ids) across the whole configured interval, adds
//! zero cost to the request path (`login_rate_limit`/`device_rate_limit`
//! never call `cleanup`), and touches the exact same lock/shard a request
//! already briefly holds during `check_key` — so it introduces no new
//! contention, just a periodic, brief, already-familiar one.

use crate::config::RateLimitConfig;
use crate::state::AppState;
use std::time::Duration;

/// The scheduler tick interval, from `RATE_LIMIT_ENTRY_TTL_SECONDS`
/// (`config::RateLimitConfig`, Production Hardening Finding H4) — see that
/// type's own doc comment for exactly what this does and does not control.
/// Factored into its own function so the mapping is testable without a
/// running scheduler or a real `AppState`.
fn interval_from_config(config: &RateLimitConfig) -> Duration {
    Duration::from_secs(config.entry_ttl_secs)
}

/// Spawns the cleanup loop and returns its `JoinHandle`. The first tick
/// fires immediately (`tokio::time::interval`'s default behavior) —
/// harmless on a fresh process (nothing has had time to go stale yet), and
/// consistent with `reconciliation::spawn`'s own reasoning for not
/// delaying its first run either.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(interval_from_config(&state.config.rate_limit));
        loop {
            interval.tick().await;
            let before_login = state.rate_limiters.login_len();
            let before_device = state.rate_limiters.device_len();
            state.rate_limiters.cleanup();
            tracing::debug!(
                login_entries_before = before_login,
                login_entries_after = state.rate_limiters.login_len(),
                device_entries_before = before_device,
                device_entries_after = state.rate_limiters.device_len(),
                "rate limiter cleanup sweep completed"
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_matches_the_default_rate_limit_entry_ttl_of_900_seconds() {
        let config = RateLimitConfig {
            entry_ttl_secs: 900,
        };
        assert_eq!(interval_from_config(&config), Duration::from_secs(900));
    }

    #[test]
    fn interval_uses_the_configured_rate_limit_entry_ttl_seconds() {
        let config = RateLimitConfig { entry_ttl_secs: 42 };
        assert_eq!(interval_from_config(&config), Duration::from_secs(42));
    }

    // A full spawn()-with-a-real-AppState wiring test (proving the
    // scheduler actually calls `RateLimiters::cleanup` on a tick) lives in
    // `rate_limit`'s own test module instead of here: `RateLimiters`'
    // `login`/`device` fields are crate-private to that module, and this
    // module has no legitimate production reason to reach into them either
    // — only `rate_limit`'s tests can insert a rate-limiter entry to
    // observe the scheduler evict or preserve it.
}
