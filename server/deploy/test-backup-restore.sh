#!/usr/bin/env bash
# server/deploy/test-backup-restore.sh — offline smoke test for
# backup.sh/restore.sh (Phase 4J.2, production readiness audit CRITICAL
# finding #2), using a fake `docker` shim instead of a real Docker
# Compose/Postgres stack, so this runs anywhere `bash`/`gzip` exist —
# no VPS, no containers, no network.
#
# Verifies exactly the behaviour this phase changed:
#   - the dump is invoked with `--clean --if-exists` (restore-safe)
#   - a failed dump leaves neither a finished backup nor a stray `.partial`
#     file behind
#   - a successful dump produces one complete, gzip-valid archive
#   - `restore.sh` rejects a corrupt/truncated backup file before ever
#     touching `docker compose stop`
#   - `restore.sh` invokes `psql` with `-v ON_ERROR_STOP=1` and forwards
#     the decompressed dump content unchanged
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

if [[ -f "$FAKE_PSQL_CAPTURE" ]] && [[ "$(cat "$FAKE_PSQL_CAPTURE")" == "SELECT 1;" ]]; then
    pass "decompressed dump content reached psql unchanged"
else
    fail "psql did not receive the expected decompressed content"
fi

echo
if [[ "$FAILURES" -eq 0 ]]; then
    echo "All checks passed."
    exit 0
else
    echo "$FAILURES check(s) failed."
    exit 1
fi
