//! Production Hardening, Finding H7: a dedicated, explicit checkpoint that
//! every migration in `server/migrations/` applies cleanly against a real
//! Postgres — isolated into its own file (rather than relying solely on
//! the implicit `db::run_migrations` call every other ignored test's own
//! `connected_pool()`/`admin_pool_with_role_ready()` helper already makes
//! as part of its setup) so CI can run it as its own separately-logged
//! step ahead of the rest of the ignored, Postgres-backed suite, giving a
//! fast, unambiguous failure point if a migration itself is broken rather
//! than that surfacing inside an unrelated test's setup step.
//!
//! Same `#[ignore]`/`DATABASE_URL` convention as every other real-Postgres
//! test in this crate (see `server/README.md`'s "Tests" section) — never
//! run by plain `cargo test`, only explicitly:
//! `DATABASE_URL=postgres://... cargo test -p license-server --test db_migrations -- --ignored`

use license_server::db;

#[tokio::test]
#[ignore = "requires a real, reachable Postgres — see PHASE4_DESIGN.md §9"]
async fn migrations_apply_cleanly_against_a_real_database() {
    let database_url = std::env::var("DATABASE_URL")
        .expect("set DATABASE_URL to a reachable Postgres to run this ignored test");
    let pool = db::build_pool(&database_url, 5).expect("DATABASE_URL must be well-formed");
    db::run_migrations(&pool)
        .await
        .expect("every migration in server/migrations/ must apply cleanly");
}
