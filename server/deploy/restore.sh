#!/usr/bin/env bash
# server/deploy/restore.sh — restore Postgres from a backup produced by
# server/deploy/backup.sh (PHASE4_DESIGN.md §10 stage 5's rollback plan:
# "restore the *database* from the pre-bug backup if the corruption already
# happened, rather than trying to hand-patch rows live" — and "roll the
# *server* back first" before touching the database).
#
# Phase 4J.2 (production readiness audit, CRITICAL finding #2) hardened two
# things this script used to get wrong:
#   1. `backup.sh` dumps with `--clean --if-exists`, so this restore
#      actually replaces the target database's contents instead of
#      erroring on "already exists" (or, worse, silently appending
#      duplicate rows via `COPY` into tables nothing dropped first).
#   2. `psql` runs with `ON_ERROR_STOP=1`, so any SQL error during the
#      restore aborts immediately instead of `psql`'s default behaviour of
#      logging the error and continuing on to the next statement anyway.
#
# Phase 4L.2.1 (Backup & Disaster Recovery Hardening) adds:
#   3. Checksum verification against the `.sha256` sidecar `backup.sh` now
#      writes (fatal on mismatch), before anything else — extends the
#      existing `gzip -t` check with real tamper/bit-rot detection, and
#      runs before the destructive stop-server step, same as the existing
#      corruption check.
#   4. Transparent decryption for backups made with `BACKUP_ENCRYPTION_KEY`
#      set (auto-detected via the `.meta.json` sidecar, or a `.enc`
#      filename as a fallback).
#   5. `--single-transaction` on the restore `psql` invocation — combined
#      with `ON_ERROR_STOP=1`, a failure partway through the restore now
#      rolls back cleanly instead of leaving a half-dropped/half-restored
#      database (`pg_dump --clean --if-exists`'s DDL is fully transactional
#      in Postgres, so this is safe and changes nothing about a successful
#      restore).
#   6. A `--verify-only` mode that runs every non-destructive check
#      (checksum, decryption, gzip integrity, content sniff) and exits,
#      without prompting, stopping the server, or touching the database at
#      all — usable as a standalone disaster-recovery drill.
#
# Usage:
#   server/deploy/restore.sh [--verify-only] /var/backups/license-server/daily/license-server-<ts>.sql.gz[.enc]
#
# Template — this is a destructive operation (overwrites the live
# database); review the confirmation prompt and the stop-server-first
# ordering below before relying on it in production.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib/backup-common.sh"

REPO_DIR="${REPO_DIR:-$(pwd)}"
cd "$REPO_DIR"

VERIFY_ONLY=0
if [[ "${1:-}" == "--verify-only" ]]; then
    VERIFY_ONLY=1
    shift
fi
DUMP_FILE="${1:?Usage: restore.sh [--verify-only] <path-to-dump.sql.gz[.enc]>}"
[[ -f "$DUMP_FILE" ]] || { echo "No such file: $DUMP_FILE" >&2; exit 1; }

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

echo "==> Verifying backup file"
# Checksum + decrypt-if-needed + gzip integrity + content sniff — fails
# fast on a corrupt/truncated/tampered backup file, before the destructive
# stop-server step below, not partway through the restore itself.
PLAIN_FILE="$(bc_verify_and_prepare "$DUMP_FILE" "$WORK_DIR")"
echo "Verification passed."

if [[ "$VERIFY_ONLY" == "1" ]]; then
    echo "--verify-only: all checks passed, no restore performed."
    exit 0
fi

if [[ -f server/.env ]]; then
    # shellcheck disable=SC1091
    source server/.env
fi
# The admin/superuser account (Phase 4J.8 — a restore needs to drop/
# recreate tables via the `--clean --if-exists` dump, which the restricted
# `license_server_app` role can't do).
POSTGRES_USER="${POSTGRES_USER:-postgres}"
POSTGRES_DB="${POSTGRES_DB:-license_server}"

echo "This will REPLACE the contents of database '$POSTGRES_DB' with:"
echo "  $DUMP_FILE"
read -r -p "Type 'yes' to continue: " CONFIRM
[[ "$CONFIRM" == "yes" ]] || { echo "Aborted."; exit 1; }

# Stop the app first so nothing writes to Postgres mid-restore
# (PHASE4_DESIGN.md §10 stage 5's own ordering: server rollback, then
# database restore — not the other way around).
echo "==> Stopping license-server"
docker compose stop license-server

echo "==> Restoring into '$POSTGRES_DB'"
# ON_ERROR_STOP=1: without it, psql logs a failed statement and keeps
# running the rest of the script anyway, still exiting 0. --single-
# transaction wraps the whole restore in one BEGIN/COMMIT, so combined
# with ON_ERROR_STOP=1 the first SQL error now rolls the entire restore
# back instead of leaving a partially-restored database (some tables
# dropped, not yet recreated) — Phase 4L.2.1, closes the "partial restore
# risk" gap; pg_dump's --clean --if-exists DDL is fully transactional in
# Postgres, so this changes nothing about a successful restore.
gunzip -c "$PLAIN_FILE" \
    | docker compose exec -T postgres psql -v ON_ERROR_STOP=1 --single-transaction -U "$POSTGRES_USER" "$POSTGRES_DB"

echo "==> Starting license-server"
docker compose up -d license-server

cat <<'EOF'

Restore complete. Verify:
  - curl -f https://<your-domain>/readyz
  - Spot-check recent licenses/payments against what you expect post-restore.
  - PHASE4_DESIGN.md §10 stage 5: Razorpay itself is the source of truth for
    whether money actually moved — re-run the reconciliation job (or wait
    for its next scheduled pass) to re-sync `payments`/`licenses` against
    Razorpay after a restore.
EOF
