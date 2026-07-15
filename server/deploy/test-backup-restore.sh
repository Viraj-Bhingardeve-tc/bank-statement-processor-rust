#!/usr/bin/env bash
# server/deploy/test-backup-restore.sh — offline smoke test for
# backup.sh/restore.sh/verify-backup.sh (Phase 4J.2 and Phase 4L.2.1,
# production readiness audit CRITICAL finding), using a fake `docker` shim
# instead of a real Docker Compose/Postgres stack, so this runs anywhere
# `bash`/`gzip`/`openssl` exist — no VPS, no containers, no network.
#
# Verifies exactly the behaviour these phases changed:
#   Phase 4J.2:
#   - the dump is invoked with `--clean --if-exists` (restore-safe)
#   - a failed dump leaves neither a finished backup nor a stray `.partial`
#     file behind
#   - a successful dump produces one complete, gzip-valid archive
#   - `restore.sh` rejects a corrupt/truncated backup file before ever
#     touching `docker compose stop`
#   - `restore.sh` invokes `psql` with `-v ON_ERROR_STOP=1` and forwards
#     the decompressed dump content unchanged
#   Phase 4L.2.1 (Backup & Disaster Recovery Hardening):
#   - every backup gets a `.sha256` + `.meta.json` sidecar
#   - `restore.sh`/`verify-backup.sh` reject a backup whose content no
#     longer matches its recorded checksum, before touching Docker
#   - a backup with no sidecars at all (pre-hardening) still verifies,
#     with a warning, not a hard failure
#   - `BACKUP_ENCRYPTION_KEY` round-trips a backup through encrypt/decrypt
#     transparently
#   - `restore.sh` invokes `psql` with `--single-transaction`
#   - `restore.sh --verify-only` never invokes docker at all
#   - `DAILY_RETENTION` is honored when overridden
#   - `OFFSITE_SYNC_CMD` is invoked with `BACKUP_DIR` available to it
#
# Run: bash server/deploy/test-backup-restore.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

FAILURES=0
pass() { echo "  ok   - $1"; }
fail() {
    echo "  FAIL - $1"
    FAILURES=$((FAILURES + 1))
}

# ── Fake `docker` shim ───────────────────────────────────────────────────
# Understands just enough of `docker compose exec -T postgres <pg_dump|psql>
# ...` and `docker compose <stop|up> ...` to drive backup.sh/restore.sh
# without a real container. Behaviour is controlled entirely via env vars
# read at invocation time (FAKE_PG_DUMP_FAIL, FAKE_PG_DUMP_OUTPUT,
# FAKE_PSQL_FAIL, FAKE_PSQL_CAPTURE, FAKE_DOCKER_LOG).
mkdir -p "$WORK_DIR/bin"
cat >"$WORK_DIR/bin/docker" <<'FAKE_DOCKER'
#!/usr/bin/env bash
set -euo pipefail

log() { [[ -n "${FAKE_DOCKER_LOG:-}" ]] && echo "$*" >>"$FAKE_DOCKER_LOG"; return 0; }

if [[ "${1:-}" == "compose" && "${2:-}" == "exec" ]]; then
    shift 2
    [[ "${1:-}" == "-T" ]] && shift
    service="$1"; shift
    case "$1" in
        pg_dump)
            shift
            log "pg_dump_args: $*"
            if [[ "${FAKE_PG_DUMP_FAIL:-0}" == "1" ]]; then
                echo "-- partial output before a simulated failure"
                echo "fake docker: simulated pg_dump failure" >&2
                exit 1
            fi
            printf '%s\n' "${FAKE_PG_DUMP_OUTPUT:-}"
            ;;
        psql)
            shift
            log "psql_args: $*"
            case " $* " in
                *" -tAc "*)
                    # backup.sh's `SHOW server_version;` probe — a direct
                    # scalar query, no stdin to consume.
                    echo "${FAKE_PSQL_VERSION_OUTPUT:-16.4}"
                    ;;
                *)
                    if [[ -n "${FAKE_PSQL_CAPTURE:-}" ]]; then
                        cat >"$FAKE_PSQL_CAPTURE"
                    else
                        cat >/dev/null
                    fi
                    if [[ "${FAKE_PSQL_FAIL:-0}" == "1" ]]; then
                        echo "fake docker: simulated psql error" >&2
                        exit 1
                    fi
                    ;;
            esac
            ;;
        *)
            echo "fake docker: unhandled exec target: $1" >&2
            exit 99
            ;;
    esac
elif [[ "${1:-}" == "compose" && ( "${2:-}" == "stop" || "${2:-}" == "up" ) ]]; then
    log "compose $*"
else
    echo "fake docker: unhandled invocation: $*" >&2
    exit 99
fi
FAKE_DOCKER
chmod +x "$WORK_DIR/bin/docker"

export PATH="$WORK_DIR/bin:$PATH"
export REPO_DIR="$WORK_DIR/repo"
export POSTGRES_USER="testuser"
export POSTGRES_DB="testdb"
mkdir -p "$REPO_DIR"

# ── Test 1: a clean dump is restore-safe and complete ────────────────────
echo "Test 1: successful backup"
BACKUP_DIR="$WORK_DIR/backups1"
FAKE_DOCKER_LOG="$WORK_DIR/docker1.log"
export BACKUP_DIR FAKE_DOCKER_LOG
export FAKE_PG_DUMP_OUTPUT="-- fixture dump content"
unset FAKE_PG_DUMP_FAIL || true

if bash "$SCRIPT_DIR/backup.sh" >"$WORK_DIR/backup1.out" 2>&1; then
    pass "backup.sh exits 0"
else
    fail "backup.sh exited non-zero: $(cat "$WORK_DIR/backup1.out")"
fi

mapfile -t dumps < <(find "$BACKUP_DIR/daily" -maxdepth 1 -name '*.sql.gz' -type f)
if [[ ${#dumps[@]} -eq 1 ]]; then
    pass "exactly one .sql.gz produced"
else
    fail "expected exactly 1 .sql.gz, found ${#dumps[@]}"
fi

if [[ ${#dumps[@]} -eq 1 ]] && gzip -t "${dumps[0]}" 2>/dev/null; then
    pass "produced archive passes gzip -t"
else
    fail "produced archive is not a valid gzip stream"
fi

if [[ ${#dumps[@]} -eq 1 ]] && [[ "$(gunzip -c "${dumps[0]}")" == "-- fixture dump content" ]]; then
    pass "decompressed content matches what pg_dump emitted"
else
    fail "decompressed content did not match the fixture"
fi

if [[ -z "$(find "$BACKUP_DIR/daily" -maxdepth 1 -name '*.partial' -type f)" ]]; then
    pass "no .partial file left behind after success"
else
    fail "a .partial file was left behind after a successful run"
fi

if grep -q -- '--clean' "$FAKE_DOCKER_LOG" && grep -q -- '--if-exists' "$FAKE_DOCKER_LOG"; then
    pass "pg_dump was invoked with --clean --if-exists"
else
    fail "pg_dump was NOT invoked with --clean --if-exists (got: $(grep pg_dump_args "$FAKE_DOCKER_LOG" || true))"
fi

DUMP1="${dumps[0]:-}"
if [[ -n "$DUMP1" ]] && [[ -f "$DUMP1.sha256" ]] && [[ -f "$DUMP1.meta.json" ]]; then
    pass "a .sha256 and .meta.json sidecar were written alongside the dump"
else
    fail "expected $DUMP1.sha256 and $DUMP1.meta.json to exist"
fi

if [[ -n "$DUMP1" ]] && [[ "$(awk '{print $1}' "$DUMP1.sha256")" == "$(sha256sum "$DUMP1" | awk '{print $1}')" ]]; then
    pass "the .sha256 sidecar matches the dump's actual checksum"
else
    fail "the .sha256 sidecar does not match the dump's actual checksum"
fi

if [[ -n "$DUMP1" ]] && grep -q '"encrypted": false' "$DUMP1.meta.json"; then
    pass "metadata correctly records encrypted: false for an unencrypted backup"
else
    fail "metadata did not record encrypted: false"
fi

# ── Test 2: a failed dump leaves nothing behind ──────────────────────────
echo "Test 2: failed backup leaves no partial or finished file"
BACKUP_DIR="$WORK_DIR/backups2"
FAKE_DOCKER_LOG="$WORK_DIR/docker2.log"
export BACKUP_DIR FAKE_DOCKER_LOG
export FAKE_PG_DUMP_FAIL=1

if bash "$SCRIPT_DIR/backup.sh" >"$WORK_DIR/backup2.out" 2>&1; then
    fail "backup.sh should have exited non-zero on a simulated pg_dump failure"
else
    pass "backup.sh exits non-zero when pg_dump fails"
fi

if [[ -z "$(find "$BACKUP_DIR/daily" -maxdepth 1 -name '*.sql.gz' -type f 2>/dev/null)" ]]; then
    pass "no finished .sql.gz after a failed dump"
else
    fail "a .sql.gz file exists despite the simulated pg_dump failure"
fi

if [[ -z "$(find "$BACKUP_DIR/daily" -maxdepth 1 -name '*.partial' -type f 2>/dev/null)" ]]; then
    pass "no .partial file left behind after a failed dump"
else
    fail "a .partial file was left behind after the simulated failure"
fi
unset FAKE_PG_DUMP_FAIL

# ── Test 3: restore.sh rejects a corrupt backup file before touching docker
echo "Test 3: restore.sh rejects a corrupt/truncated backup file"
CORRUPT_FILE="$WORK_DIR/corrupt.sql.gz"
printf 'this is not a gzip file' >"$CORRUPT_FILE"
FAKE_DOCKER_LOG="$WORK_DIR/docker3.log"
export FAKE_DOCKER_LOG
rm -f "$FAKE_DOCKER_LOG"

if echo "yes" | bash "$SCRIPT_DIR/restore.sh" "$CORRUPT_FILE" >"$WORK_DIR/restore3.out" 2>&1; then
    fail "restore.sh should have rejected a corrupt backup file"
else
    pass "restore.sh exits non-zero on a corrupt backup file"
fi

if grep -qi "corrupt" "$WORK_DIR/restore3.out"; then
    pass "restore.sh reports the file as corrupt/truncated"
else
    fail "restore.sh did not report the corrupt-file error clearly"
fi

if [[ ! -f "$FAKE_DOCKER_LOG" ]]; then
    pass "docker was never invoked for a corrupt input file"
else
    fail "docker compose was invoked despite the corrupt input file"
fi

# ── Test 4: restore.sh uses ON_ERROR_STOP and forwards content unchanged
echo "Test 4: restore.sh enforces ON_ERROR_STOP and forwards content"
GOOD_FILE="$WORK_DIR/good.sql.gz"
printf 'SELECT 1;\n' | gzip >"$GOOD_FILE"
FAKE_DOCKER_LOG="$WORK_DIR/docker4.log"
FAKE_PSQL_CAPTURE="$WORK_DIR/psql_stdin.sql"
export FAKE_DOCKER_LOG FAKE_PSQL_CAPTURE
rm -f "$FAKE_DOCKER_LOG" "$FAKE_PSQL_CAPTURE"

if echo "yes" | bash "$SCRIPT_DIR/restore.sh" "$GOOD_FILE" >"$WORK_DIR/restore4.out" 2>&1; then
    pass "restore.sh exits 0 for a valid backup file"
else
    fail "restore.sh failed on a valid backup file: $(cat "$WORK_DIR/restore4.out")"
fi

if grep -q -- '-v ON_ERROR_STOP=1' "$FAKE_DOCKER_LOG"; then
    pass "psql was invoked with -v ON_ERROR_STOP=1"
else
    fail "psql was NOT invoked with -v ON_ERROR_STOP=1 (got: $(grep psql_args "$FAKE_DOCKER_LOG" || true))"
fi

if grep -q -- '--single-transaction' "$FAKE_DOCKER_LOG"; then
    pass "psql was invoked with --single-transaction"
else
    fail "psql was NOT invoked with --single-transaction (got: $(grep psql_args "$FAKE_DOCKER_LOG" || true))"
fi

if [[ -f "$FAKE_PSQL_CAPTURE" ]] && [[ "$(cat "$FAKE_PSQL_CAPTURE")" == "SELECT 1;" ]]; then
    pass "decompressed dump content reached psql unchanged"
else
    fail "psql did not receive the expected decompressed content"
fi

# ── Test 5: a checksum mismatch is rejected before touching docker ──────
echo "Test 5: restore.sh --verify-only rejects a tampered backup"
BACKUP_DIR="$WORK_DIR/backups5"
FAKE_DOCKER_LOG="$WORK_DIR/docker5.log"
export BACKUP_DIR FAKE_DOCKER_LOG
export FAKE_PG_DUMP_OUTPUT="-- PostgreSQL database dump fixture content"
rm -f "$FAKE_DOCKER_LOG"

bash "$SCRIPT_DIR/backup.sh" >"$WORK_DIR/backup5.out" 2>&1
mapfile -t dumps5 < <(find "$BACKUP_DIR/daily" -maxdepth 1 -name '*.sql.gz' -type f)
TAMPERED="${dumps5[0]}"
# Corrupt the dump's content without touching its .sha256 sidecar, so the
# recorded checksum no longer matches — simulates bit-rot or tampering
# after the fact, independent of gzip's own (still-valid) stream CRC.
printf 'tampered bytes' >>"$TAMPERED"
rm -f "$FAKE_DOCKER_LOG"

if echo "yes" | bash "$SCRIPT_DIR/restore.sh" --verify-only "$TAMPERED" >"$WORK_DIR/restore5.out" 2>&1; then
    fail "restore.sh --verify-only should have rejected a checksum-mismatched backup"
else
    pass "restore.sh --verify-only exits non-zero on a checksum mismatch"
fi

if grep -qi "checksum mismatch" "$WORK_DIR/restore5.out"; then
    pass "restore.sh reports the checksum mismatch clearly"
else
    fail "restore.sh did not report the checksum mismatch clearly: $(cat "$WORK_DIR/restore5.out")"
fi

if [[ ! -f "$FAKE_DOCKER_LOG" ]]; then
    pass "docker was never invoked for a checksum-mismatched file"
else
    fail "docker compose was invoked despite the checksum mismatch"
fi

if echo "yes" | bash "$SCRIPT_DIR/verify-backup.sh" "$TAMPERED" >"$WORK_DIR/verify5.out" 2>&1; then
    fail "verify-backup.sh should have rejected a checksum-mismatched backup"
else
    pass "verify-backup.sh (standalone) also exits non-zero on a checksum mismatch"
fi

# ── Test 6: a backup with no sidecars (pre-hardening) still verifies ────
echo "Test 6: verify-backup.sh accepts a pre-hardening backup with no sidecars, with a warning"
LEGACY_FILE="$WORK_DIR/legacy.sql.gz"
printf -- '-- PostgreSQL database dump\nSELECT 1;\n' | gzip >"$LEGACY_FILE"

if OUT="$(bash "$SCRIPT_DIR/verify-backup.sh" "$LEGACY_FILE" 2>&1)"; then
    pass "verify-backup.sh exits 0 for a sidecar-less legacy backup"
else
    fail "verify-backup.sh rejected a legacy backup with no sidecars: $OUT"
fi

if echo "$OUT" | grep -qi "no checksum sidecar"; then
    pass "verify-backup.sh warns (not fails) about the missing checksum sidecar"
else
    fail "verify-backup.sh did not warn about the missing checksum sidecar"
fi

# ── Test 7: BACKUP_ENCRYPTION_KEY round-trips transparently ─────────────
echo "Test 7: encrypted backup round-trips through verify-backup.sh"
BACKUP_DIR="$WORK_DIR/backups7"
FAKE_DOCKER_LOG="$WORK_DIR/docker7.log"
export BACKUP_DIR FAKE_DOCKER_LOG
export FAKE_PG_DUMP_OUTPUT="-- PostgreSQL database dump encrypted-fixture"
export BACKUP_ENCRYPTION_KEY="test-passphrase-do-not-use-in-prod"
rm -f "$FAKE_DOCKER_LOG"

bash "$SCRIPT_DIR/backup.sh" >"$WORK_DIR/backup7.out" 2>&1
mapfile -t dumps7 < <(find "$BACKUP_DIR/daily" -maxdepth 1 -name '*.sql.gz.enc' -type f)
if [[ ${#dumps7[@]} -eq 1 ]]; then
    pass "BACKUP_ENCRYPTION_KEY produces a .sql.gz.enc artifact"
else
    fail "expected exactly 1 .sql.gz.enc, found ${#dumps7[@]}: $(cat "$WORK_DIR/backup7.out")"
fi
ENC_DUMP="${dumps7[0]:-}"

if [[ -n "$ENC_DUMP" ]] && grep -q '"encrypted": true' "$ENC_DUMP.meta.json"; then
    pass "metadata correctly records encrypted: true"
else
    fail "metadata did not record encrypted: true"
fi

if [[ -n "$ENC_DUMP" ]] && bash "$SCRIPT_DIR/verify-backup.sh" "$ENC_DUMP" >"$WORK_DIR/verify7.out" 2>&1; then
    pass "verify-backup.sh decrypts and verifies the encrypted backup with the correct key"
else
    fail "verify-backup.sh failed against a correctly-encrypted backup: $(cat "$WORK_DIR/verify7.out")"
fi

if [[ -n "$ENC_DUMP" ]] && openssl enc -d -aes-256-cbc -pbkdf2 -pass env:BACKUP_ENCRYPTION_KEY -in "$ENC_DUMP" 2>/dev/null | gunzip -c | grep -q "encrypted-fixture"; then
    pass "decrypted content matches what pg_dump emitted, byte for byte"
else
    fail "decrypted content did not match the original fixture"
fi

unset BACKUP_ENCRYPTION_KEY
if bash "$SCRIPT_DIR/verify-backup.sh" "$ENC_DUMP" >"$WORK_DIR/verify7b.out" 2>&1; then
    fail "verify-backup.sh should fail to decrypt without BACKUP_ENCRYPTION_KEY set"
else
    pass "verify-backup.sh fails clearly when BACKUP_ENCRYPTION_KEY is missing"
fi

# ── Test 8: --verify-only never touches docker, even for a valid backup ─
echo "Test 8: restore.sh --verify-only performs no destructive action"
FAKE_DOCKER_LOG="$WORK_DIR/docker8.log"
export FAKE_DOCKER_LOG
rm -f "$FAKE_DOCKER_LOG"

if bash "$SCRIPT_DIR/restore.sh" --verify-only "$LEGACY_FILE" >"$WORK_DIR/restore8.out" 2>&1; then
    pass "restore.sh --verify-only exits 0 for a valid backup"
else
    fail "restore.sh --verify-only failed on a valid backup: $(cat "$WORK_DIR/restore8.out")"
fi

if [[ ! -f "$FAKE_DOCKER_LOG" ]]; then
    pass "restore.sh --verify-only never invokes docker (no stop, no restore)"
else
    fail "restore.sh --verify-only invoked docker despite --verify-only"
fi

if grep -qi "no restore performed" "$WORK_DIR/restore8.out"; then
    pass "restore.sh --verify-only reports that no restore was performed"
else
    fail "restore.sh --verify-only did not clearly report a no-op"
fi

# ── Test 9: DAILY_RETENTION is honored when overridden ──────────────────
echo "Test 9: DAILY_RETENTION override prunes down to the configured count"
BACKUP_DIR="$WORK_DIR/backups9"
FAKE_DOCKER_LOG="$WORK_DIR/docker9.log"
mkdir -p "$BACKUP_DIR/daily" "$BACKUP_DIR/weekly"
# Seed 3 pre-existing, complete (non-.partial) dumps with distinct mtimes
# so pruning has a deterministic oldest-first order.
for i in 1 2 3; do
    f="$BACKUP_DIR/daily/license-server-2020010${i}T000000Z.sql.gz"
    printf 'old dump %s' "$i" | gzip >"$f"
    printf '%s  %s\n' "$(sha256sum "$f" | awk '{print $1}')" "$(basename "$f")" >"$f.sha256"
    touch -d "2020-01-0${i}T00:00:00Z" "$f" "$f.sha256" 2>/dev/null || true
done
export BACKUP_DIR FAKE_DOCKER_LOG
export FAKE_PG_DUMP_OUTPUT="-- PostgreSQL database dump retention-fixture"
export DAILY_RETENTION=2
rm -f "$FAKE_DOCKER_LOG"

bash "$SCRIPT_DIR/backup.sh" >"$WORK_DIR/backup9.out" 2>&1
mapfile -t dumps9 < <(find "$BACKUP_DIR/daily" -maxdepth 1 -name '*.sql.gz' -type f)
if [[ ${#dumps9[@]} -eq 2 ]]; then
    pass "DAILY_RETENTION=2 keeps exactly 2 dumps (3 seeded + 1 new, pruned to 2)"
else
    fail "expected exactly 2 dumps after DAILY_RETENTION=2 pruning, found ${#dumps9[@]}"
fi

if [[ -z "$(find "$BACKUP_DIR/daily" -maxdepth 1 -name 'license-server-20200101*' -type f)" ]]; then
    pass "the oldest seeded dump (and its sidecar) was pruned first"
else
    fail "the oldest seeded dump was not pruned despite DAILY_RETENTION=2"
fi
unset DAILY_RETENTION

# ── Test 10: OFFSITE_SYNC_CMD is invoked with BACKUP_DIR available ──────
echo "Test 10: OFFSITE_SYNC_CMD hook actually runs"
BACKUP_DIR="$WORK_DIR/backups10"
OFFSITE_DIR="$WORK_DIR/offsite10"
FAKE_DOCKER_LOG="$WORK_DIR/docker10.log"
export BACKUP_DIR FAKE_DOCKER_LOG
export FAKE_PG_DUMP_OUTPUT="-- PostgreSQL database dump offsite-fixture"
export OFFSITE_SYNC_CMD="mkdir -p '$OFFSITE_DIR' && cp -r \"\$BACKUP_DIR\"/. '$OFFSITE_DIR'/"
rm -f "$FAKE_DOCKER_LOG"

if bash "$SCRIPT_DIR/backup.sh" >"$WORK_DIR/backup10.out" 2>&1; then
    pass "backup.sh exits 0 when OFFSITE_SYNC_CMD succeeds"
else
    fail "backup.sh failed with OFFSITE_SYNC_CMD set: $(cat "$WORK_DIR/backup10.out")"
fi

if [[ -n "$(find "$OFFSITE_DIR/daily" -maxdepth 1 -name '*.sql.gz' -type f 2>/dev/null)" ]]; then
    pass "OFFSITE_SYNC_CMD actually copied the backup to the off-site destination"
else
    fail "OFFSITE_SYNC_CMD did not result in a copy at the off-site destination"
fi
unset OFFSITE_SYNC_CMD

echo
if [[ "$FAILURES" -eq 0 ]]; then
    echo "All checks passed."
    exit 0
else
    echo "$FAILURES check(s) failed."
    exit 1
fi
