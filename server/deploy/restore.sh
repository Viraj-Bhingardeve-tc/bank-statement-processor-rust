#!/usr/bin/env bash
# server/deploy/restore.sh — restore Postgres from a backup produced by
# server/deploy/backup.sh (PHASE4_DESIGN.md §10 stage 5's rollback plan:
# "restore the *database* from the pre-bug backup if the corruption already
# happened, rather than trying to hand-patch rows live" — and "roll the
# *server* back first" before touching the database).
#
# Usage:
#   server/deploy/restore.sh /var/backups/license-server/daily/license-server-<ts>.sql.gz
#
# Template — this is a destructive operation (overwrites the live
# database); review the confirmation prompt and the stop-server-first
# ordering below before relying on it in production.

set -euo pipefail

REPO_DIR="${REPO_DIR:-$(pwd)}"
cd "$REPO_DIR"

DUMP_FILE="${1:?Usage: restore.sh <path-to-dump.sql.gz>}"
[[ -f "$DUMP_FILE" ]] || { echo "No such file: $DUMP_FILE" >&2; exit 1; }

if [[ -f server/.env ]]; then
    # shellcheck disable=SC1091
    source server/.env
fi
POSTGRES_USER="${POSTGRES_USER:-license_server}"
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
gunzip -c "$DUMP_FILE" | docker compose exec -T postgres psql -U "$POSTGRES_USER" "$POSTGRES_DB"

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
