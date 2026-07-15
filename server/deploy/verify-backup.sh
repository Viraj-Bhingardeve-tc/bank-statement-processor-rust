#!/usr/bin/env bash
# server/deploy/verify-backup.sh — non-destructive backup integrity check
# (Phase 4L.2.1 — Backup & Disaster Recovery Hardening).
#
# Runs the exact same checksum/decryption/gzip-integrity/content-sniff
# checks restore.sh runs before ever touching Docker or the live database
# — as a standalone tool an operator can run any time: nightly via cron
# right after backup.sh, ad hoc before trusting a copy that was synced
# off-site, or as part of a disaster-recovery drill. Needs no running
# Postgres/Docker stack at all — pure file-level checks.
#
# Usage:
#   server/deploy/verify-backup.sh /var/backups/license-server/daily/license-server-<ts>.sql.gz[.enc]
#
# Exit 0 and prints "OK: <file>" on success. Exits non-zero with a clear
# reason on any failed check (missing file, checksum mismatch, decryption
# failure, corrupt gzip stream). Never modifies, moves, or deletes the
# input file.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib/backup-common.sh"

DUMP_FILE="${1:?Usage: verify-backup.sh <path-to-dump.sql.gz[.enc]>}"

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

bc_verify_and_prepare "$DUMP_FILE" "$WORK_DIR" >/dev/null
echo "OK: $DUMP_FILE"
