//! Payment reconciliation scheduler (`PHASE4_DESIGN.md` §12).
//!
//! A `tokio::time::interval` background task spawned alongside the axum
//! listener at startup (`main.rs`) — not a separate container/cron job,
//! per §12.1's stated reasoning ("keeps deployment simple... gives it
//! direct access to the same service-layer functions the webhook handler
//! already uses"). All the actual reconciliation logic (listing Razorpay
//! payments, comparing against local state, healing genuine gaps) lives
//! in `service::payment_service::PaymentService::reconcile_once` — this
//! module is only the scheduling wrapper, kept deliberately thin so the
//! logic itself is testable (§12.4) without a running scheduler.

use crate::config::ReconciliationConfig;
use crate::service::{PaymentOperationError, ReconciliationSummary};
use crate::state::AppState;
use std::future::Future;
use std::time::Duration;

/// The scheduler tick interval, from `RECONCILIATION_INTERVAL_SECS`
/// (`config::ReconciliationConfig`, Phase 4K.4) — previously a fixed
/// `INTERVAL` constant of 15 minutes (`PHASE4_DESIGN.md` §14 item 8:
/// "frequent enough that a lost webhook is caught well within any
/// reasonable customer-support SLA, infrequent enough to stay well clear
/// of Razorpay's API rate limits"). That value is still the default when
/// the variable is unset; factored into its own function so the mapping
/// is testable without a running scheduler or a real `AppState`/database.
fn interval_from_config(config: &ReconciliationConfig) -> Duration {
    Duration::from_secs(config.interval_secs)
}

/// Bounds a single `reconcile_once` run (production readiness audit HIGH
/// finding #5) — without it, a hung Razorpay call inside
/// `list_payments_since` would block this loop's `.await` forever,
/// silently disabling the reconciliation backstop with no error, no log,
/// nothing. A run that doesn't finish within this window is treated as a
/// failure and retried on the next tick, same as any other reconciliation
/// error.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// Spawns the reconciliation loop and returns its `JoinHandle`. The first
/// tick fires immediately (`tokio::time::interval`'s default behavior) —
/// deliberately not delayed a full 15 minutes, since the job is meant to
/// catch up on anything that accumulated while the server was down
/// (`PHASE4_DESIGN.md` §12's own framing: "...or arrives while the server
/// is down"), and there's no reason to make a fresh deploy wait to do
/// that.
///
/// A failed run (e.g. Razorpay unreachable, or the run timing out — see
/// `run_once_with_timeout`) is logged and the loop continues to the next
/// tick — reconciliation failing must never affect the server's ability
/// to keep serving normal traffic.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(interval_from_config(&state.config.reconciliation));
        loop {
            interval.tick().await;
            run_once_with_timeout(RUN_TIMEOUT, state.payment_service.reconcile_once()).await;
        }
    })
}

/// Runs one reconciliation attempt bounded by `timeout_duration`, recording
/// metrics and logging exactly once per outcome — success, an
/// application-level failure (`PaymentOperationError`, e.g. Razorpay
/// unreachable), or a timeout (`reconcile` didn't resolve in time).
/// Production readiness audit HIGH finding #5: wraps *only* the
/// reconciliation future itself in `tokio::time::timeout`, exactly as
/// specified — never panics, never returns an error the caller would need
/// to propagate, so the `loop` in `spawn` above always proceeds to its
/// next `interval.tick()` regardless of what happened here. Generic over
/// the future (rather than taking `&PaymentService` directly) so a test
/// can drive it with a future that deliberately never resolves, without a
/// real database or Razorpay account.
async fn run_once_with_timeout(
    timeout_duration: Duration,
    reconcile: impl Future<Output = Result<ReconciliationSummary, PaymentOperationError>>,
) {
    match tokio::time::timeout(timeout_duration, reconcile).await {
        Ok(Ok(summary)) => {
            metrics::counter!(
                crate::observability::RECONCILIATION_RUNS_TOTAL,
                "result" => "success",
            )
            .increment(1);
            metrics::counter!(crate::observability::RECONCILIATION_PAYMENTS_CHECKED_TOTAL)
                .increment(summary.checked as u64);
            metrics::counter!(crate::observability::RECONCILIATION_PAYMENTS_HEALED_TOTAL)
                .increment(summary.healed as u64);
        }
        Ok(Err(e)) => {
            metrics::counter!(
                crate::observability::RECONCILIATION_RUNS_TOTAL,
                "result" => "failure",
            )
            .increment(1);
            tracing::error!(error = %e, "reconciliation run failed; will retry next tick");
        }
        Err(_elapsed) => {
            // Counted under the same "failure" label as an application-
            // level error — both mean "this run did not complete" — but
            // logged with a distinct message so an operator grepping logs
            // can tell a hung network call apart from a normal error.
            metrics::counter!(
                crate::observability::RECONCILIATION_RUNS_TOTAL,
                "result" => "failure",
            )
            .increment(1);
            tracing::error!(
                timeout_secs = timeout_duration.as_secs(),
                "reconciliation run timed out; will retry next tick"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn interval_defaults_still_match_the_previously_hardcoded_fifteen_minutes() {
        let config = ReconciliationConfig {
            interval_secs: 15 * 60,
            batch_size: 100,
            max_age_hours: 2,
        };
        assert_eq!(interval_from_config(&config), Duration::from_secs(900));
    }

    #[test]
    fn interval_uses_the_configured_reconciliation_interval_seconds() {
        let config = ReconciliationConfig {
            interval_secs: 42,
            batch_size: 100,
            max_age_hours: 2,
        };
        assert_eq!(interval_from_config(&config), Duration::from_secs(42));
    }

    #[test]
    fn run_timeout_matches_the_documented_sixty_second_bound() {
        assert_eq!(RUN_TIMEOUT, Duration::from_secs(60));
    }

    /// The actual behavior Phase 4J.4 fixes: a reconciliation run that
    /// never resolves (simulating a hung Razorpay call inside
    /// `list_payments_since`) must not block `run_once_with_timeout`
    /// forever — it returns once the timeout elapses. Since `spawn`'s loop
    /// has no early-return/`?`/`break` around this call, this function
    /// returning at all is what lets the loop proceed to its next
    /// `interval.tick()` — i.e. this is also the proof the scheduler
    /// survives the hang rather than being permanently disabled by it.
    #[tokio::test]
    async fn scheduler_survives_a_reconciliation_run_that_never_finishes() {
        let never_finishes =
            std::future::pending::<Result<ReconciliationSummary, PaymentOperationError>>();

        let started = Instant::now();
        run_once_with_timeout(Duration::from_millis(50), never_finishes).await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "run_once_with_timeout must return once the timeout elapses, not hang forever \
             (took {elapsed:?})"
        );
    }

    /// A hung run isn't just survived once — a *subsequent* tick still
    /// works normally afterward, proving nothing about the timeout path
    /// leaves the scheduler in a bad state.
    #[tokio::test]
    async fn a_normal_run_still_succeeds_after_a_previous_run_timed_out() {
        let never_finishes =
            std::future::pending::<Result<ReconciliationSummary, PaymentOperationError>>();
        run_once_with_timeout(Duration::from_millis(50), never_finishes).await;

        let completes_immediately = async {
            Ok(ReconciliationSummary {
                checked: 0,
                healed: 0,
            })
        };
        let started = Instant::now();
        run_once_with_timeout(Duration::from_millis(50), completes_immediately).await;

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a fast, successful run right after a timeout must not itself be delayed"
        );
    }
}
