//! PostgreSQL connection pool and migration runner.
//!
//! `PHASE4_DESIGN.md` §1.3 chose `sqlx` (async, compile-time-checkable
//! queries, built-in migration runner) against `LICENSE_DATABASE_SCHEMA.md`
//! §1's Postgres-flavored schema. Phase 4C.1 is scaffolding only — no
//! license/payment tables yet (`migrations/` is intentionally empty; the
//! first real migration lands in whichever later phase actually needs
//! `sessions`/`payment_webhook_events`/etc., per `PHASE4_DESIGN.md` §7).

use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

/// Embeds `migrations/` into the binary at compile time (not read from disk
/// at runtime) so a deployed container image can never drift from the
/// migrations it was built with — matches `PHASE4_DESIGN.md` §7's migration
/// tooling note. Empty in this phase; `run_migrations` still runs (and is
/// tested) against zero migrations, proving the wiring works before any
/// real schema lands on top of it.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Builds a connection pool **without** eagerly connecting.
/// `connect_lazy` defers the first real network attempt until the pool is
/// actually used (e.g. `/readyz`'s own query) — so the server process can
/// start and answer `/healthz` even if the database is temporarily
/// unreachable at boot, consistent with `PHASE4_DESIGN.md` §8.3's
/// liveness/readiness split. Only a malformed `database_url` fails here;
/// a merely-unreachable database does not.
///
/// Must be called from within a Tokio runtime context (e.g. inside
/// `#[tokio::main]`, as `main.rs` does) — the pool spawns its own idle-
/// connection-reaper background task even before any query runs, which
/// needs a runtime to spawn onto, independent of any actual network I/O.
pub fn build_pool(database_url: &str, max_connections: u32) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect_lazy(database_url)
}

/// Runs every pending migration. Called once at startup, before the server
/// starts accepting traffic — a failed migration should stop the process,
/// not leave it serving against a schema it doesn't actually have.
pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}

/// Whether `e` is a Postgres "insufficient privilege" error (SQLSTATE
/// `42501`) — e.g. `ALTER TABLE` failing under a role that isn't the
/// table's owner (Phase 4L.3, production validation, CRITICAL). Used only
/// to decide whether to print an actionable hint alongside the raw error;
/// never changes whether `run_migrations` itself fails.
pub fn is_insufficient_privilege_error(e: &sqlx::migrate::MigrateError) -> bool {
    match e {
        sqlx::migrate::MigrateError::Execute(inner) => {
            is_insufficient_privilege_error_from_sqlx(inner)
        }
        _ => false,
    }
}

/// The actual SQLSTATE check `is_insufficient_privilege_error` delegates
/// to — a separate `pub` function (rather than folded inline) so it's
/// directly testable against a real Postgres error from a plain failed
/// query (`server/tests/least_privilege_role.rs`), without needing to
/// construct a synthetic `sqlx::migrate::Migrator` just to get a
/// `MigrateError`.
pub fn is_insufficient_privilege_error_from_sqlx(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Database(db_err) => db_err.code().as_deref() == Some("42501"),
        _ => false,
    }
}

#[cfg(test)]
mod privilege_error_tests {
    use super::is_insufficient_privilege_error_from_sqlx;

    #[test]
    fn non_database_errors_are_never_treated_as_insufficient_privilege() {
        // `sqlx::Error::PoolTimedOut` needs no live connection to construct
        // — proves the non-Database branch stays `false` rather than
        // panicking or guessing.
        assert!(!is_insufficient_privilege_error_from_sqlx(
            &sqlx::Error::PoolTimedOut
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_pool_with_a_malformed_url_fails_immediately_without_a_network_attempt() {
        let result = build_pool("not-a-valid-postgres-url", 5);
        assert!(
            result.is_err(),
            "a malformed connection string must be caught at construction"
        );
    }

    #[tokio::test]
    async fn build_pool_with_a_well_formed_but_unreachable_url_succeeds_lazily() {
        // No network attempt happens here at all — `connect_lazy` only
        // parses the URL (construction does need a Tokio context, per
        // `build_pool`'s doc comment, hence `#[tokio::test]` here rather
        // than a plain `#[test]`). Reachability is exercised in
        // `routes::ready`'s own tests (the failure path is testable without
        // a real Postgres; the success path needs one, see that module's
        // doc comment).
        let result = build_pool("postgres://user:pass@127.0.0.1:1/nonexistent_db", 5);
        assert!(
            result.is_ok(),
            "connect_lazy must not attempt a connection at construction time"
        );
    }
}
