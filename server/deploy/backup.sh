#!/usr/bin/env bash
# server/deploy/backup.sh — nightly Postgres backup
# (PHASE4_DESIGN.md §8.2: "a nightly `pg_dump` (via a small `cron` entry on
# the host, or a `postgres`-image sidecar container running `pg_dump` on a
# schedule) writing compressed dumps to a path outside the Docker volume
# (ideally off-VPS ...), retained on a rolling window of 14 daily backups +
# 8 weekly backups").
#
# Intended to run via host cron, e.g.:
#   0 2 * * * REPO_DIR=/opt/license-server /opt/license-server/server/deploy/backup.sh
#
# Template — the retention scheme and backup path match PHASE4_DESIGN.md
# exactly; the off-VPS sync step is left as a marked hook below since the
# design doc deliberately doesn't fix a specific destination (§11: "an
# accepted trade-off for this stage, revisit if uptime/data-durability
# requirements grow").

set -euo pipefail

REPO_DIR="${REPO_DIR:-$(pwd)}"
# Outside every Docker volume by design (PHASE4_DESIGN.md §8.2) — a
# `docker compose down -v` or `pgdata` volume loss can't take backups with
# it.
BACKUP_DIR="${BACKUP_DIR:-/var/backups/license-server}"
DAILY_DIR="$BACKUP_DIR/daily"
WEEKLY_DIR="$BACKUP_DIR/weekly"
DAILY_RETENTION=14
WEEKLY_RETENTION=8

cd "$REPO_DIR"
mkdir -p "$DAILY_DIR" "$WEEKLY_DIR"

if [[ -f server/.env ]]; then
    # shellcheck disable=SC1091
    source server/.env
fi
POSTGRES_USER="${POSTGRES_USER:-license_server}"
POSTGRES_DB="${POSTGRES_DB:-license_server}"

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
DUMP_FILE="$DAILY_DIR/license-server-$TIMESTAMP.sql.gz"

echo "==> Dumping '$POSTGRES_DB' from the running postgres container"
docker compose exec -T postgres pg_dump -U "$POSTGRES_USER" "$POSTGRES_DB" | gzip > "$DUMP_FILE"
echo "Wrote $DUMP_FILE"

# Sunday's daily dump is also kept in the weekly retention set.
if [[ "$(date -u +%u)" == "7" ]]; then
    cp "$DUMP_FILE" "$WEEKLY_DIR/"
    echo "Also kept as a weekly backup: $WEEKLY_DIR/$(basename "$DUMP_FILE")"
fi

prune() {
    local dir="$1" keep="$2"
    find "$dir" -maxdepth 1 -name '*.sql.gz' -type f -printf '%T@ %p\n' \
        | sort -rn \
        | tail -n "+$((keep + 1))" \
        | cut -d' ' -f2- \
        | xargs -r rm -f
}

echo "==> Pruning daily backups beyond $DAILY_RETENTION"
prune "$DAILY_DIR" "$DAILY_RETENTION"

echo "==> Pruning weekly backups beyond $WEEKLY_RETENTION"
prune "$WEEKLY_DIR" "$WEEKLY_RETENTION"

# TODO (not fixed by PHASE4_DESIGN.md — pick a destination before relying
# on this in production): sync $BACKUP_DIR off-VPS, e.g.
#   rclone sync "$BACKUP_DIR" remote:license-server-backups
echo "Backup complete: $DUMP_FILE"
