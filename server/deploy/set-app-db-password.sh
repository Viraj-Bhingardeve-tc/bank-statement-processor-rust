#!/usr/bin/env bash
# server/deploy/set-app-db-password.sh — set or rotate the password for the
# restricted `license_server_app` Postgres role (Phase 4J.8 — production
# readiness audit's least-privilege DB role finding; see
# server/migrations/0003_least_privilege_app_role.sql and
# server/README.md's "Database roles and least privilege" section).
#
# The role itself is created by that migration (idempotent, embedded in the
# server binary via `sqlx::migrate!()`, applied automatically at startup —
# see server/src/db.rs) — but a migration file is committed to version
# control, so it deliberately never sets a password. This script is the
# out-of-band step that does, connecting as the Postgres *admin* account
# (`POSTGRES_USER`, never the restricted role itself — `ALTER ROLE ...
# PASSWORD` on your own role isn't enough to bootstrap it the first time,
# and this script's job is exactly that bootstrap plus every later
# rotation).
#
# Usage (run on the VPS, from the repository root, after `docker compose up
# -d postgres` — or the full stack — is running):
#   APP_DB_USER=license_server_app APP_DB_PASSWORD='...' server/deploy/set-app-db-password.sh
# or export APP_DB_USER / APP_DB_PASSWORD (or put them in server/.env, which
# this script also sources) before running it with no arguments.
#
# Safe to re-run: `ALTER ROLE ... WITH LOGIN PASSWORD ...` simply overwrites
# the previous password each time, which is exactly what a rotation is —
# there is no cumulative or destructive state here.
#
# After running this the first time, update server/.env's DATABASE_URL to
# use APP_DB_USER/APP_DB_PASSWORD (not POSTGRES_USER) and restart
# license-server — see server/README.md's "Database roles and least
# privilege" section for the full sequencing. license_server_app also holds
# CREATE on schema public (sqlx migration compatibility — see that
# migration's own doc comment), so this switch works cleanly even for the
# automatic migration step at every future startup.

set -euo pipefail

REPO_DIR="${REPO_DIR:-$(pwd)}"
cd "$REPO_DIR"

if [[ -f server/.env ]]; then
    # shellcheck disable=SC1091
    source server/.env
fi

POSTGRES_USER="${POSTGRES_USER:-postgres}"
POSTGRES_DB="${POSTGRES_DB:-license_server}"
APP_DB_USER="${APP_DB_USER:-license_server_app}"
APP_DB_PASSWORD="${APP_DB_PASSWORD:?Set APP_DB_PASSWORD (shell env or server/.env) before running this script}"

echo "==> Setting password for role '$APP_DB_USER' in database '$POSTGRES_DB'"
# `--set`/`-v` bind psql variables (`:"approle"` as a quoted identifier,
# `:'apppassword'` as a quoted literal) rather than string-interpolating
# either value directly into the SQL text — avoids any quoting/injection
# hazard if the role name or password contains a double/single quote. The
# heredoc uses a quoted delimiter ('SQL') so bash performs no expansion of
# its own on the `:"..."` syntax, leaving it for psql to substitute.
docker compose exec -T postgres psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -v ON_ERROR_STOP=1 \
    --set=approle="$APP_DB_USER" --set=apppassword="$APP_DB_PASSWORD" <<'SQL'
ALTER ROLE :"approle" WITH LOGIN PASSWORD :'apppassword';
SQL

echo "Done. If this is the first time, update server/.env's DATABASE_URL to:"
echo "  postgres://${APP_DB_USER}:<password>@postgres:5432/${POSTGRES_DB}"
echo "then restart license-server (docker compose restart license-server)."
