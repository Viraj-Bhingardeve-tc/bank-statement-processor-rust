#!/usr/bin/env bash
# server/deploy/backup.sh — nightly Postgres backup
# (PHASE4_DESIGN.md §8.2: "a nightly `pg_dump` (via a small `cron` entry on
# the host, or a `postgres`-image sidecar container running `pg_dump` on a
# schedule) writing compressed dumps to a path outside the Docker volume
# (ideally off-VPS ...), retained on a rolling window of 14 daily backups +
# 8 weekly backups").
#
# Phase 4J.2 (production readiness audit, CRITICAL finding #2) hardened two
# things this script used to get wrong:
#   1. The dump is restore-safe (`--clean --if-exists`) — `restore.sh`
#      can replace an already-populated database's contents instead of
#      erroring on "already exists" or, worse, silently appending
#      duplicate rows via `COPY` into tables that were never dropped.
#   2. The dump is written to a temporary file, gzip-integrity-checked,
#      and only renamed into `daily/`/`weekly/` once verified complete —
#      a `pg_dump`/`gzip` failure or truncated archive can no longer land
#      a partial/corrupt file at a path `prune()` or `restore.sh` would
#      treat as a real backup.
#
# Phase 4L.2.1 (Backup & Disaster Recovery Hardening — the one remaining
# Critical finding from FINAL_PRODUCTION_VALIDATION_REPORT.md) adds:
#   3. A `.sha256` + `.meta.json` sidecar next to every dump, unconditionally
#      — independent, persisted integrity proof beyond `gzip -t`'s own
#      stream check, plus auditable metadata (database, Postgres version,
#      size, encryption flag, backup-format version). Purely additive: the
#      primary `.sql.gz`/`.sql.gz.enc` artifact and its content are
#      unchanged.
#   4. Optional at-rest encryption (AES-256-CBC via `openssl enc`), enabled
#      only when `BACKUP_ENCRYPTION_KEY` is set. Unset (the default) means
#      byte-for-byte the same unencrypted output as before — this is a
#      capability addition, not a behavior change for anyone who hasn't
#      opted in.
#   5. Optional off-site sync via the operator-supplied `OFFSITE_SYNC_CMD`
#      hook (replaces the old commented-out TODO with something that
#      actually runs, without hardcoding a specific tool — rclone, aws s3,
#      rsync, restic, whatever the operator already uses). Unset means the
#      same "local only" behavior as before, now with an explicit reminder
#      instead of a silent gap.
#   6. `DAILY_RETENTION`/`WEEKLY_RETENTION` are now overridable (still
#      default to 14/8, unchanged).
#
# Intended to run via host cron, e.g.:
#   0 2 * * * REPO_DIR=/opt/license-server /opt/license-server/server/deploy/backup.sh
#
# See server/README.md's "Backup location" section for the full operator
# checklist (required/optional environment variables, verification, and
# restore procedure).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib/backup-common.sh"

REPO_DIR="${REPO_DIR:-$(pwd)}"
# Outside every Docker volume by design (PHASE4_DESIGN.md §8.2) — a
# `docker compose down -v` or `pgdata` volume loss can't take backups with
# it.
BACKUP_DIR="${BACKUP_DIR:-/var/backups/license-server}"
DAILY_DIR="$BACKUP_DIR/daily"
WEEKLY_DIR="$BACKUP_DIR/weekly"
DAILY_RETENTION="${DAILY_RETENTION:-14}"
WEEKLY_RETENTION="${WEEKLY_RETENTION:-8}"

cd "$REPO_DIR"
mkdir -p "$DAILY_DIR" "$WEEKLY_DIR"

# Clean up any temp file a previous run left behind (e.g. the host was
# killed mid-backup, before the `trap` below could fire — `kill -9` can't
# be trapped) — none of these are ever counted by `prune()` (its glob only
# matches finished `*.sql.gz`/`*.sql.gz.enc` files) and would otherwise
# accumulate on disk forever.
find "$DAILY_DIR" -maxdepth 1 \( -name '*.partial' -o -name '*.partial.sha256' -o -name '*.partial.meta.json' \) -type f -delete

if [[ -f server/.env ]]; then
    # shellcheck disable=SC1091
    source server/.env
fi
# The admin/superuser account (Phase 4J.8 — never the restricted
# `license_server_app` role `DATABASE_URL` connects as; `pg_dump` needs
# full read access across the whole database, which that role deliberately
# doesn't have).
POSTGRES_USER="${POSTGRES_USER:-postgres}"
POSTGRES_DB="${POSTGRES_DB:-license_server}"

TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
BASE_NAME="license-server-$TIMESTAMP.sql.gz"
PLAIN_TMP="$DAILY_DIR/$BASE_NAME.partial"

# Always remove every temp file this run created on exit — a no-op on
# success (each is already renamed away by the `mv`s below), the cleanup
# on any failure. `:-` guards against an empty array under `set -u`.
CLEANUP_FILES=("$PLAIN_TMP")
trap 'rm -f "${CLEANUP_FILES[@]:-}"' EXIT

echo "==> Dumping '$POSTGRES_DB' from the running postgres container"
# --clean --if-exists: emit `DROP ... IF EXISTS` before each `CREATE`, so
# this dump is safe to restore straight into an already-populated database
# (`restore.sh`'s actual use case) instead of erroring on "already exists"
# — or, worse, silently appending duplicate rows via `COPY` into tables
# that were never dropped. `--if-exists` also keeps a restore into a
# genuinely empty database working (no "does not exist" errors from the
# `DROP` statements themselves).
docker compose exec -T postgres pg_dump --clean --if-exists -U "$POSTGRES_USER" "$POSTGRES_DB" \
    | gzip > "$PLAIN_TMP"

# Verify the archive is actually complete and uncorrupted before it's
# treated as a real backup — catches a truncated/corrupt result that
# `pg_dump`/`gzip`'s own exit codes didn't already catch. Always run
# against the plaintext dump, before any encryption step below.
gzip -t "$PLAIN_TMP"

if [[ -n "${BACKUP_ENCRYPTION_KEY:-}" ]]; then
    echo "==> Encrypting (BACKUP_ENCRYPTION_KEY is set)"
    ENCRYPTED=true
    ARTIFACT_TMP="$DAILY_DIR/$BASE_NAME.enc.partial"
    CLEANUP_FILES+=("$ARTIFACT_TMP")
    bc_encrypt "$PLAIN_TMP" "$ARTIFACT_TMP"
    rm -f "$PLAIN_TMP"
    DUMP_FILE="$DAILY_DIR/$BASE_NAME.enc"
else
    ENCRYPTED=false
    ARTIFACT_TMP="$PLAIN_TMP"
    DUMP_FILE="$DAILY_DIR/$BASE_NAME"
fi

SHA256="$(bc_sha256 "$ARTIFACT_TMP")"
SIZE_BYTES="$(bc_file_size "$ARTIFACT_TMP")"
# Best-effort — recorded for disaster-recovery compatibility checks (e.g.
# confirming an old backup is safe to restore into a newer Postgres major
# version). Never worth failing the whole backup over; falls back to
# "unknown" if the probe itself has any problem.
PG_VERSION="$(docker compose exec -T postgres psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -tAc 'SHOW server_version;' 2>/dev/null | tr -d '[:space:]')"
PG_VERSION="${PG_VERSION:-unknown}"

SHA_TMP="$ARTIFACT_TMP.sha256"
META_TMP="$ARTIFACT_TMP.meta.json"
CLEANUP_FILES+=("$SHA_TMP" "$META_TMP")
printf '%s  %s\n' "$SHA256" "$(basename "$DUMP_FILE")" >"$SHA_TMP"
bc_write_metadata "$META_TMP" "$POSTGRES_DB" "$SHA256" "$SIZE_BYTES" "$ENCRYPTED" "$PG_VERSION" "$TIMESTAMP"

mv "$ARTIFACT_TMP" "$DUMP_FILE"
mv "$SHA_TMP" "$DUMP_FILE.sha256"
mv "$META_TMP" "$DUMP_FILE.meta.json"
echo "Wrote $DUMP_FILE (sha256=$SHA256, encrypted=$ENCRYPTED, size=${SIZE_BYTES}B)"

# Sunday's daily dump (plus its sidecars) is also kept in the weekly
# retention set.
if [[ "$(date -u +%u)" == "7" ]]; then
    cp "$DUMP_FILE" "$DUMP_FILE.sha256" "$DUMP_FILE.meta.json" "$WEEKLY_DIR/"
    echo "Also kept as a weekly backup: $WEEKLY_DIR/$(basename "$DUMP_FILE")"
fi

prune() {
    local dir="$1" keep="$2"
    # Primary dump files only (never sidecars) — matches both unencrypted
    # (*.sql.gz) and encrypted (*.sql.gz.enc) naming, since a deployment
    # may toggle BACKUP_ENCRYPTION_KEY on/off between runs and end up with
    # a mixed history.
    local old_dumps
    old_dumps="$(find "$dir" -maxdepth 1 \( -name '*.sql.gz' -o -name '*.sql.gz.enc' \) -type f -printf '%T@ %p\n' \
        | sort -rn \
        | tail -n "+$((keep + 1))" \
        | cut -d' ' -f2-)"
    [[ -z "$old_dumps" ]] && return 0
    while IFS= read -r dump; do
        rm -f "$dump" "$dump.sha256" "$dump.meta.json"
    done <<<"$old_dumps"
}

echo "==> Pruning daily backups beyond $DAILY_RETENTION"
prune "$DAILY_DIR" "$DAILY_RETENTION"

echo "==> Pruning weekly backups beyond $WEEKLY_RETENTION"
prune "$WEEKLY_DIR" "$WEEKLY_RETENTION"

if [[ -n "${OFFSITE_SYNC_CMD:-}" ]]; then
    echo "==> Running off-site sync: $OFFSITE_SYNC_CMD"
    # Deliberately run via `bash -c` (not `eval`) so an operator can supply
    # whatever off-site tool/flags they already use (rclone, aws s3 sync,
    # rsync, restic, ...) — this repo doesn't hardcode or depend on any one
    # of them. $BACKUP_DIR is exported so the command can reference it.
    if BACKUP_DIR="$BACKUP_DIR" bash -c "$OFFSITE_SYNC_CMD"; then
        echo "Off-site sync complete"
    else
        echo "WARNING: local backup succeeded but off-site sync failed (command: $OFFSITE_SYNC_CMD)" >&2
        exit 1
    fi
else
    echo "NOTE: OFFSITE_SYNC_CMD is not set — backups exist only on this host. See server/README.md's 'Backup location' section."
fi

echo "Backup complete: $DUMP_FILE"
