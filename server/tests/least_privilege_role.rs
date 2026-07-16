//! Verifies Phase 4J.8's least-privilege database role actually behaves as
//! documented, against a *real* Postgres instance — no mocks. A mock
//! connection can't meaningfully prove anything about actual Postgres
//! `GRANT`/`REVOKE` enforcement (permission checks happen inside the
//! database server itself, not in this crate's code), so per this phase's
//! own instructions, this is real-database-or-nothing: every test below is
//! `#[ignore]`d, same convention as every other real-Postgres-only test in
//! this crate (`server/README.md`'s "Tests" section) — there is no
//! automated coverage for this that can run without a reachable Postgres,
//! and that is a documented, deliberate limitation, not an oversight.
//!
//! Verifies `license_server_app` can read/write its own tables, can create
//! a table in its own schema (the one deliberate `CREATE ON SCHEMA public`
//! exception — see `0003_least_privilege_app_role.sql`'s own doc comment),
//! and has none of `SUPERUSER`/`CREATEDB`/`CREATEROLE`/`REPLICATION`/
//! `BYPASSRLS` set.
//!
//! Run explicitly against a real, reachable Postgres, connected as an
//! **admin** account (able to run migrations and set another role's
//! password — see `server/README.md`'s "Database roles and least
//! privilege" section):
//! `DATABASE_URL=postgres://... cargo test -p license-server --test least_privilege_role -- --ignored`

use license_server::db;
use sqlx::PgPool;

/// A fixed, test-only password — never used outside this ignored test
/// suite, set fresh on every run via the same `ALTER ROLE ... PASSWORD`
/// mechanism `server/deploy/set-app-db-password.sh` uses in production
/// (the migration that creates `license_server_app` deliberately never
/// sets one itself — see that migration's own doc comment).
const TEST_APP_PASSWORD: &str = "phase-4j8-test-only-password";

/// Connects as the admin account (`DATABASE_URL`, same convention every
/// other ignored integration test in this crate uses), runs migrations —
/// including `0003_least_privilege_app_role.sql`, which creates the
/// restricted role — then sets a fixed test password for it. Returns the
/// admin pool (for cleanup/inspection queries) and the original
/// `DATABASE_URL` string (so the app-role connection string below can
/// reuse its host/port/dbname).
async fn admin_pool_with_role_ready() -> (PgPool, String) {
    let database_url = std::env::var("DATABASE_URL").expect(
        "set DATABASE_URL to a reachable Postgres (admin account) to run this ignored test",
    );
    let pool = db::build_pool(&database_url, 5).expect("DATABASE_URL must be well-formed");
    db::run_migrations(&pool)
        .await
        .expect("migrations, including the least-privilege role migration, must apply cleanly");

    sqlx::query(&format!(
        "ALTER ROLE license_server_app WITH LOGIN PASSWORD '{TEST_APP_PASSWORD}'"
    ))
    .execute(&pool)
    .await
    .expect("must be able to set the app role's password as the admin account");

    (pool, database_url)
}

/// Builds a `DATABASE_URL` for the restricted role by swapping only the
/// credentials on the admin connection string — reuses whatever real
/// host/port/dbname the ignored test suite is actually pointed at, rather
/// than hardcoding one.
fn app_role_database_url(admin_database_url: &str, password: &str) -> String {
    let after_at = admin_database_url
        .split_once('@')
        .map(|(_, rest)| rest)
        .expect("DATABASE_URL must be in postgres://user:pass@host:port/db form");
    format!("postgres://license_server_app:{password}@{after_at}")
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres, connected as an admin account — see PHASE4_DESIGN.md §9"]
async fn app_role_can_read_and_write_its_own_tables() {
    let (_admin_pool, admin_database_url) = admin_pool_with_role_ready().await;
    let app_url = app_role_database_url(&admin_database_url, TEST_APP_PASSWORD);
    let app_pool = db::build_pool(&app_url, 5).expect("app role DATABASE_URL must be well-formed");

    sqlx::query("SELECT COUNT(*) FROM users")
        .fetch_one(&app_pool)
        .await
        .expect("license_server_app must be able to SELECT from its own tables");

    // INSERT + UPDATE + DELETE, exercising the sequence grant too (`id` is
    // BIGSERIAL, so INSERT needs USAGE on `users_id_seq`).
    let email = format!("least-privilege-test-{}@example.com", uuid::Uuid::new_v4());
    let user_id: i64 =
        sqlx::query_scalar("INSERT INTO users (email, password_hash) VALUES ($1, $2) RETURNING id")
            .bind(&email)
            .bind("hash")
            .fetch_one(&app_pool)
            .await
            .expect("license_server_app must be able to INSERT (and use the id sequence)");

    sqlx::query("UPDATE users SET full_name = $1 WHERE id = $2")
        .bind("Test User")
        .bind(user_id)
        .execute(&app_pool)
        .await
        .expect("license_server_app must be able to UPDATE its own tables");

    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&app_pool)
        .await
        .expect("license_server_app must be able to DELETE from its own tables");
}

/// `license_server_app` deliberately *does* hold `CREATE` on schema
/// `public` — the one documented exception to an otherwise DML-only role,
/// required so sqlx's own `CREATE TABLE IF NOT EXISTS _sqlx_migrations`
/// bookkeeping (run unconditionally on every startup, per
/// `db::run_migrations`) doesn't fail with "permission denied for schema
/// public" (see `0003_least_privilege_app_role.sql`'s own doc comment and
/// `server/README.md`'s "Why `CREATE` on the schema is granted" section).
/// This asserts that grant is actually in effect — a regression here would
/// break `license-server` startup in production the moment `DATABASE_URL`
/// is switched to this role, so it's worth its own canary in either
/// direction, not just the "nothing dangerous" check below.
#[tokio::test]
#[ignore = "requires a real, reachable Postgres, connected as an admin account — see PHASE4_DESIGN.md §9"]
async fn app_role_can_create_a_table_in_its_own_schema() {
    let (admin_pool, admin_database_url) = admin_pool_with_role_ready().await;
    let app_url = app_role_database_url(&admin_database_url, TEST_APP_PASSWORD);
    let app_pool = db::build_pool(&app_url, 5).expect("app role DATABASE_URL must be well-formed");

    let result = sqlx::query("CREATE TABLE least_privilege_probe (id INT)")
        .execute(&app_pool)
        .await;

    // Clean up regardless of outcome — never leave a stray table behind,
    // whether the assertion below passes or is about to fail the test.
    sqlx::query("DROP TABLE IF EXISTS least_privilege_probe")
        .execute(&admin_pool)
        .await
        .ok();

    assert!(
        result.is_ok(),
        "license_server_app must be able to create tables in schema public (CREATE ON SCHEMA \
         public is a deliberate grant for sqlx migration compatibility) — if this now fails, the \
         grant in 0003_least_privilege_app_role.sql has regressed: {result:?}"
    );
}

#[tokio::test]
#[ignore = "requires a real, reachable Postgres, connected as an admin account — see PHASE4_DESIGN.md §9"]
async fn app_role_has_no_dangerous_instance_level_privileges() {
    let (admin_pool, _admin_database_url) = admin_pool_with_role_ready().await;

    let (is_superuser, can_createdb, can_createrole, can_replicate, can_bypass_rls): (
        bool,
        bool,
        bool,
        bool,
        bool,
    ) = sqlx::query_as(
        "SELECT rolsuper, rolcreatedb, rolcreaterole, rolreplication, rolbypassrls \
         FROM pg_roles WHERE rolname = 'license_server_app'",
    )
    .fetch_one(&admin_pool)
    .await
    .expect("license_server_app role must exist after migrations");

    assert!(!is_superuser, "license_server_app must never be SUPERUSER");
    assert!(
        !can_createdb,
        "license_server_app must never be able to CREATEDB"
    );
    assert!(
        !can_createrole,
        "license_server_app must never be able to CREATEROLE"
    );
    assert!(
        !can_replicate,
        "license_server_app must never be able to REPLICATION"
    );
    assert!(
        !can_bypass_rls,
        "license_server_app must never be able to BYPASSRLS"
    );
}

/// Phase 4L.3 (production validation, CRITICAL): reproduces the exact
/// production failure — a schema-altering migration (an `ALTER TABLE`,
/// like `migrations/0004_add_payment_dispute_support.sql`'s) running
/// under `license_server_app` after `DATABASE_URL` has already been
/// switched to it, per the documented deploy sequence. Confirms both that
/// this genuinely fails under the restricted role (proving the
/// ownership-gap finding is real, not theoretical) and that
/// `db::is_insufficient_privilege_error` correctly recognizes the
/// resulting error, so the actionable hint in `main.rs` actually fires
/// for it.
#[tokio::test]
#[ignore = "requires a real, reachable Postgres, connected as an admin account — see PHASE4_DESIGN.md §9"]
async fn altering_a_table_under_the_app_role_fails_with_insufficient_privilege() {
    let (_admin_pool, admin_database_url) = admin_pool_with_role_ready().await;
    let app_url = app_role_database_url(&admin_database_url, TEST_APP_PASSWORD);
    let app_pool = db::build_pool(&app_url, 5).expect("app role DATABASE_URL must be well-formed");

    let result = sqlx::query("ALTER TABLE users ADD COLUMN least_privilege_probe TEXT")
        .execute(&app_pool)
        .await;

    let err = result.expect_err(
        "license_server_app must NOT be able to ALTER TABLE — if this now succeeds, the role has \
         somehow gained ownership/ALTER privilege, which defeats the least-privilege design",
    );
    assert!(
        db::is_insufficient_privilege_error_from_sqlx(&err),
        "expected a SQLSTATE 42501 insufficient-privilege error, got: {err:?}"
    );
}
