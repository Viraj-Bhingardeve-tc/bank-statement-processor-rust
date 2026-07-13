-- Least-privilege application role (Phase 4J.8 — production readiness
-- audit: "the server's Postgres role should have INSERT/SELECT/UPDATE on
-- its own tables and nothing else" — PHASE4_DESIGN.md §5). Today the
-- server's own DATABASE_URL connects as `POSTGRES_USER`, which the
-- official `postgres` Docker image always creates as a full instance
-- superuser (CREATEDB, CREATEROLE, REPLICATION, BYPASSRLS, ALTER SYSTEM —
-- everything) — a compromised `license-server` process currently has the
-- same reach as a database administrator. This migration creates a
-- separate, narrowly-scoped role for the application to connect as
-- instead; `postgres`/`POSTGRES_USER` remains for administration only
-- (running this migration itself, `pg_dump`/`psql` in
-- `server/deploy/backup.sh`/`restore.sh`) — see `server/README.md`'s
-- "Database roles and least privilege" section for the full deployment
-- story.
--
-- `CREATE ROLE ... IF NOT EXISTS` doesn't exist in Postgres, hence the
-- `DO $$ ... $$` guard below — this makes the migration safe to embed via
-- `sqlx::migrate!()` and run unconditionally in any environment (fresh
-- local/CI database, or a production database that already has the role
-- from a previous deploy), matching every other migration in this
-- directory's "safe to run on both a fresh and an existing database"
-- convention (`CREATE TABLE IF NOT EXISTS` elsewhere serves the same
-- purpose; `CREATE ROLE` has no built-in `IF NOT EXISTS` clause, so this
-- is the equivalent).
--
-- No password is set here — a secret must never live in a file committed
-- to version control. Set (and later rotate) it out-of-band via
-- `server/deploy/set-app-db-password.sh`, sourced from the `APP_DB_USER`/
-- `APP_DB_PASSWORD` values in your environment (see `server/README.md`).
-- Until a password is set, this role exists but cannot log in.
DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = 'license_server_app') THEN
        -- LOGIN only — deliberately no SUPERUSER, CREATEDB, CREATEROLE,
        -- or REPLICATION (all default to off for a plain `CREATE ROLE`,
        -- listed here anyway so the absence is a deliberate, auditable
        -- statement rather than an accident of relying on defaults).
        -- BYPASSRLS also defaults off; this schema doesn't use row-level
        -- security today, but the role must never be able to bypass it if
        -- that changes later. `ALTER SYSTEM` requires SUPERUSER, so
        -- withholding SUPERUSER already withholds it too — there is no
        -- separate privilege to revoke for that specifically.
        CREATE ROLE license_server_app
            LOGIN
            NOSUPERUSER
            NOCREATEDB
            NOCREATEROLE
            NOREPLICATION
            NOBYPASSRLS;
    END IF;
END
$$;

-- `current_database()` rather than a hardcoded name — this migration runs
-- unmodified regardless of what `POSTGRES_DB` was set to for this
-- deployment (`docker-compose.yml` defaults it to `license_server`, but
-- that's operator-configurable).
DO $$
BEGIN
    EXECUTE format('GRANT CONNECT ON DATABASE %I TO license_server_app', current_database());
END
$$;

GRANT USAGE ON SCHEMA public TO license_server_app;

-- `CREATE` on the schema (not just `USAGE`) — the one deliberate exception
-- to this migration's otherwise DML-only grant list, required for sqlx
-- migration compatibility, not for anything this application's own
-- business logic does. `server/src/main.rs` calls
-- `db::run_migrations(&pool)` unconditionally on every process start,
-- using this same connection, and sqlx's migrator unconditionally issues
-- `CREATE TABLE IF NOT EXISTS _sqlx_migrations (...)` as its first step on
-- every single run — Postgres requires `CREATE` privilege on the schema to
-- even attempt that statement, regardless of whether the table already
-- exists or any new migration is actually pending. Without this grant,
-- `license_server_app` cannot be used as `DATABASE_URL` at all: the server
-- fails at startup with "permission denied for schema public" before ever
-- reaching a real query. `CREATE ON SCHEMA public` only ever lets this role
-- create objects inside this one schema, in this one database — it grants
-- nothing at the instance level, so `SUPERUSER`/`CREATEDB`/`CREATEROLE`/
-- `REPLICATION`/`BYPASSRLS` (and, transitively, `ALTER SYSTEM`, which
-- requires `SUPERUSER`) all remain withheld exactly as declared above. See
-- `server/README.md`'s "Database roles and least privilege" section for
-- the full rationale.
GRANT CREATE ON SCHEMA public TO license_server_app;

-- SELECT/INSERT/UPDATE/DELETE only, enumerated per table rather than
-- `ALL TABLES IN SCHEMA public` — explicit and auditable, and guarantees
-- this grant never silently widens to include some future non-application
-- table added to the same schema by an unrelated change. Every table this
-- server's repositories actually read/write, from both migrations
-- (0001_create_license_schema.sql, 0002_create_payment_schema.sql) —
-- `server/src/repository/*` is the source of truth this list was checked
-- against.
GRANT SELECT, INSERT, UPDATE, DELETE ON
    users,
    subscriptions,
    licenses,
    devices,
    sessions,
    payments,
    payment_webhook_events
TO license_server_app;

-- Every one of the tables above uses a `BIGSERIAL id` column, which is
-- Postgres sugar for a `BIGINT` default pulling from an implicitly-created
-- `<table>_id_seq` sequence — `INSERT` needs `USAGE` on that sequence
-- (for `nextval()`) to work at all; `SELECT` is additionally granted so
-- `currval()`/introspection isn't blocked either, though nothing in this
-- codebase currently calls it directly.
GRANT USAGE, SELECT ON
    users_id_seq,
    subscriptions_id_seq,
    licenses_id_seq,
    devices_id_seq,
    sessions_id_seq,
    payments_id_seq,
    payment_webhook_events_id_seq
TO license_server_app;

-- sqlx's own migration bookkeeping table (`_sqlx_migrations`, created by
-- `sqlx::migrate::Migrator` itself, not by any file in this directory) —
-- `db::run_migrations` reads it (`SELECT version, checksum ...`) on every
-- single startup, run as whatever role `DATABASE_URL` connects with, so
-- `license_server_app` needs at least SELECT here regardless of whether it
-- ever successfully applies a new migration itself. INSERT/UPDATE cover
-- the bookkeeping sqlx performs when a migration *is* applied successfully
-- (now possible under this role too, given the schema-CREATE grant above).
GRANT SELECT, INSERT, UPDATE ON _sqlx_migrations TO license_server_app;

-- Extends the same four privileges to any table this migration's own
-- runner (the admin role executing this file) creates in `public` in the
-- future, so a later, purely-additive migration doesn't also need a
-- follow-up GRANT statement of its own before `license_server_app` can use
-- the new table. Does not widen anything beyond the same SELECT/INSERT/
-- UPDATE/DELETE already granted above.
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO license_server_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT USAGE, SELECT ON SEQUENCES TO license_server_app;

-- ─────────────────────────────────────────────────────────────────────────
-- Once this migration has applied and `server/deploy/set-app-db-password.sh`
-- has set a password for `license_server_app`, `DATABASE_URL` can be
-- switched to that role for normal operation — including the automatic
-- migration step at every startup, now that the schema-CREATE grant above
-- covers sqlx's own bookkeeping requirement. See `server/README.md`'s
-- "Database roles and least privilege" section for the exact deployment
-- sequencing.
-- ─────────────────────────────────────────────────────────────────────────
