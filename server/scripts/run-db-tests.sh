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

# Every other ignored, Postgres-backed integration test in the crate,
# *except* least_privilege_role (run separately below). `least_privilege_role`
# is deliberately left out of this `--all-targets` sweep — see the next
# comment block for why — so this list has to be named explicitly rather
# than just excluding one target from `--all-targets` (cargo has no
# "all targets except this one" selector). If a new ignored, Postgres-backed
# test file is added under server/tests/, add it here too.
cargo test -p license-server --lib --bin license-server \
  --test admin_api_flow \
  --test auth_flow \
  --test db_migrations \
  --test health \
  --test license_flow \
  --test metrics \
  --test payment_flow \
  --test rate_limit_flow \
  --test ready \
  --test reconciliation_flow \
  -- --ignored

# `least_privilege_role` alone, single-threaded. Its 4 tests each call
# `ALTER ROLE license_server_app WITH LOGIN PASSWORD ...` as part of their
# own setup (see that file's `admin_pool_with_role_ready()`) — Postgres's
# system catalogs (pg_authid here) don't give concurrent `ALTER ROLE`
# statements on the same role the same MVCC-safe concurrent-update handling
# an ordinary user-table UPDATE gets, so running them at once under the
# test harness's default multi-threaded execution intermittently raises
# `XX000: tuple concurrently updated`. `--test-threads=1` forces this one
# binary's tests to run sequentially instead, which avoids the race without
# touching the test code, its assertions, or any production code.
cargo test -p license-server --test least_privilege_role -- --ignored --test-threads=1
