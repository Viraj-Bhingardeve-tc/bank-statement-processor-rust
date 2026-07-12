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

use crate::state::AppState;
use std::time::Duration;

/// 15 minutes (`PHASE4_DESIGN.md` §14 item 8, confirmed, fixed value) —
/// "frequent enough that a lost webhook is caught well within any
/// reasonable customer-support SLA, infrequent enough to stay well clear
/// of Razorpay's API rate limits."
const INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Spawns the reconciliation loop and returns its `JoinHandle`. The first
/// tick fires immediately (`tokio::time::interval`'s default behavior) —
/// deliberately not delayed a full 15 minutes, since the job is meant to
/// catch up on anything that accumulated while the server was down
/// (`PHASE4_DESIGN.md` §12's own framing: "...or arrives while the server
/// is down"), and there's no reason to make a fresh deploy wait to do
/// that.
///
/// A failed run (e.g. Razorpay unreachable) is logged and the loop
/// continues to the next tick — reconciliation failing must never affect
/// the server's ability to keep serving normal traffic.
///
/// Phase 4I.2 adds `reconciliation_runs_total`/
/// `reconciliation_payments_checked_total`/
/// `reconciliation_payments_healed_total` metrics here, alongside the
/// logging Phase 4G already established — same information, machine-
/// readable for an alerting rule (e.g. "no successful run in the last N
/// hours") rather than only grep-able in logs.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(INTERVAL);
        loop {
            interval.tick().await;
            match state.payment_service.reconcile_once().await {
                Ok(summary) => {
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
                Err(e) => {
                    metrics::counter!(
                        crate::observability::RECONCILIATION_RUNS_TOTAL,
                        "result" => "failure",
                    )
                    .increment(1);
                    tracing::error!(error = %e, "reconciliation run failed; will retry next tick");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_matches_the_confirmed_fifteen_minute_design_value() {
        assert_eq!(INTERVAL, Duration::from_secs(900));
    }
}
