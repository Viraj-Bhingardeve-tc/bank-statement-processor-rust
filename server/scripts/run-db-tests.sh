#!/usr/bin/env bash
# server/scripts/run-db-tests.sh — runs the license-server integration
# tests that need a real, reachable Postgres and are normally #[ignore]d
# (server/README.md's "Tests" section; Production Hardening, Finding H7).
#
# This is the one place that command lives — CI's db-tests job
# (.github/workflows/ci.yml) and local reproduction both call this script
# instead of each carrying their own copy of the `cargo test` invocation,
# so the two can't silently drift apart.
#
# Requires DATABASE_URL (see server/.env.example) pointing at a reachable
# Postgres — an admin-level account, since `tests/least_privilege_role.rs`
# needs to create/alter the restricted `license_server_app` role. Every
# migration in server/migrations/ is applied automatically as each test's
# own setup runs (sqlx's migrator is idempotent — safe to re-run against an
# already-migrated database).
#
# Usage:
#   docker run -d --name license-server-test-db \
#     -e POSTGRES_USER=postgres -e POSTGRES_PASSWORD=postgres \
#     -e POSTGRES_DB=license_server_test -p 5432:5432 postgres:16
#   DATABASE_URL=postgres://postgres:postgres@localhost:5432/license_server_test \
#     server/scripts/run-db-tests.sh

set -euo pipefail
cd "$(dirname "$0")/../.."

: "${DATABASE_URL:?Set DATABASE_URL to a reachable Postgres before running this script}"

# Runs first, on its own, so a broken migration fails fast with an
# unambiguous "migrations" label rather than surfacing inside whichever
# other ignored test's setup step happened to run first.
cargo test -p license-server --test db_migrations -- --ignored

# Every other ignored, Postgres-backed integration test in the crate
# (auth_flow, license_flow, payment_flow, reconciliation_flow,
# least_privilege_role, ready, admin_api_flow, db_migrations again — a
# no-op re-run, harmless since migrations are idempotent). `--all-targets`
# picks up every test binary under server/tests/ without hardcoding a list
# that would go stale the next time a suite is added or renamed.
cargo test -p license-server --all-targets -- --ignored
