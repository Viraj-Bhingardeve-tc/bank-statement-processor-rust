#!/usr/bin/env bash
# server/deploy/lib/backup-common.sh — shared helpers for backup.sh /
# restore.sh / verify-backup.sh (Phase 4L.2.1 — Backup & Disaster Recovery
# Hardening).
#
# Meant to be `source`d, never executed directly — every function here is
# pure file/tool manipulation (sha256sum, openssl, gzip), no Docker/Postgres
# access, so it's usable standalone (verify-backup.sh) as well as from the
# scripts that do talk to Docker (backup.sh, restore.sh). Kept dependency-
# free beyond what every target VPS already has: coreutils (sha256sum,
# stat), gzip/gunzip, and openssl (already required for TLS elsewhere on
# any such host) — no jq, no rclone, no new package installs.
#
# Encryption is symmetric AES-256-CBC via `openssl enc`, keyed by the
# `BACKUP_ENCRYPTION_KEY` environment variable (a passphrase, never passed
# on argv — `-pass env:BACKUP_ENCRYPTION_KEY` reads it directly from the
# named env var so it never appears in `ps`/process listings or shell
# history). Entirely optional: every function here degrades to today's
# unencrypted behavior when it's unset, so existing deployments and
# existing backup files on disk keep working unchanged.

# bc_sha256 <file>
# Prints the SHA-256 hex digest of <file>.
bc_sha256() {
    sha256sum "$1" | cut -d' ' -f1
}

# bc_file_size <file>
# Prints the size of <file> in bytes.
bc_file_size() {
    if stat -c%s "$1" >/dev/null 2>&1; then
        stat -c%s "$1"
    else
        wc -c <"$1" | tr -d ' '
    fi
}

# bc_write_metadata <meta_file> <database> <sha256> <size_bytes> <encrypted:true|false> <pg_version> <created_at>
# Writes a small, flat, hand-parseable JSON metadata sidecar. Deliberately
# not pretty-printed by a JSON library (no jq dependency) — one field per
# line, in a fixed format `bc_meta_get` below is written to match exactly.
bc_write_metadata() {
    local meta_file="$1" database="$2" sha256="$3" size_bytes="$4" \
        encrypted="$5" pg_version="$6" created_at="$7"
    cat >"$meta_file" <<EOF
{
  "backup_version": 1,
  "created_at": "$created_at",
  "database": "$database",
  "postgres_version": "$pg_version",
  "sha256": "$sha256",
  "size_bytes": $size_bytes,
  "encrypted": $encrypted,
  "generator": "server/deploy/backup.sh"
}
EOF
}

# bc_meta_get <meta_file> <key>
# Prints the value of <key> from a metadata file written by
# bc_write_metadata, with surrounding quotes/trailing comma stripped.
# Only needs to understand the exact flat format this repo writes — not a
# general JSON parser.
bc_meta_get() {
    local meta_file="$1" key="$2"
    grep -m1 "\"$key\"[[:space:]]*:" "$meta_file" \
        | sed -E 's/^[[:space:]]*"[^"]+"[[:space:]]*:[[:space:]]*//; s/,[[:space:]]*$//; s/^"(.*)"$/\1/'
}

# bc_verify_checksum <file> <sha256_sidecar_file>
# Returns 0 if <file>'s current SHA-256 matches the digest recorded in
# <sha256_sidecar_file>, 1 on mismatch. Caller is responsible for checking
# the sidecar exists first (its absence is not itself a failure — it just
# means checksum verification can't be performed, e.g. for a backup
# created before this hardening landed).
bc_verify_checksum() {
    local file="$1" sha_file="$2"
    local expected actual
    expected="$(awk '{print $1}' "$sha_file")"
    actual="$(bc_sha256 "$file")"
    [[ "$expected" == "$actual" ]]
}

# bc_encrypt <in_file> <out_file>
# AES-256-CBC-encrypts <in_file> to <out_file> using BACKUP_ENCRYPTION_KEY
# (required — fails clearly if unset rather than silently writing
# plaintext under an .enc name).
bc_encrypt() {
    local in="$1" out="$2"
    [[ -n "${BACKUP_ENCRYPTION_KEY:-}" ]] || {
        echo "bc_encrypt: BACKUP_ENCRYPTION_KEY is not set" >&2
        return 1
    }
    openssl enc -aes-256-cbc -pbkdf2 -salt -pass env:BACKUP_ENCRYPTION_KEY -in "$in" -out "$out"
}

# bc_decrypt <in_file> <out_file>
# Reverses bc_encrypt. Requires the same BACKUP_ENCRYPTION_KEY the backup
# was encrypted with.
bc_decrypt() {
    local in="$1" out="$2"
    [[ -n "${BACKUP_ENCRYPTION_KEY:-}" ]] || {
        echo "bc_decrypt: BACKUP_ENCRYPTION_KEY is not set (required to decrypt this backup)" >&2
        return 1
    }
    openssl enc -d -aes-256-cbc -pbkdf2 -pass env:BACKUP_ENCRYPTION_KEY -in "$in" -out "$out"
}

# bc_looks_like_pg_dump <sql_gz_file>
# Best-effort content sniff: does the decompressed stream start with the
# comment header `pg_dump` has emitted for a very long time? Not
# authoritative (a future pg_dump could change its header wording) — only
# ever used as a non-fatal warning, never a hard failure.
bc_looks_like_pg_dump() {
    gunzip -c "$1" 2>/dev/null | head -c 4096 | grep -q 'PostgreSQL database dump'
}

# bc_verify_and_prepare <dump_file> <work_dir>
# Runs every non-destructive check available against a backup file:
#   1. SHA-256 checksum against its .sha256 sidecar, if one exists
#      (mismatch is fatal; a missing sidecar is a warning, not a failure —
#      backward compatible with backups made before this hardening).
#   2. Decryption, if the file is encrypted (per its .meta.json sidecar,
#      or a `.enc` filename as a fallback for when the sidecar itself is
#      missing/lost).
#   3. gzip integrity (`gzip -t`) on the resulting plaintext.
#   4. A best-effort pg_dump content sniff (warning only).
# Never modifies, moves, or deletes <dump_file> itself. On success, prints
# ONLY the path to a plain, gzip-valid .sql.gz file ready for
# `gunzip -c | psql` to stdout (a decrypted copy under <work_dir> if
# decryption was needed, otherwise <dump_file> unchanged) — all
# human-readable progress/warnings go to stderr, so callers can do
# `plain="$(bc_verify_and_prepare "$dump" "$work_dir")"` and get back a
# clean path. Returns non-zero on any fatal check failure.
bc_verify_and_prepare() {
    local dump_file="$1" work_dir="$2"
    [[ -f "$dump_file" ]] || {
        echo "No such file: $dump_file" >&2
        return 1
    }

    local sha_file="$dump_file.sha256"
    if [[ -f "$sha_file" ]]; then
        if bc_verify_checksum "$dump_file" "$sha_file"; then
            echo "checksum OK ($sha_file)" >&2
        else
            echo "CHECKSUM MISMATCH: $dump_file does not match $sha_file (corrupted or tampered)" >&2
            return 1
        fi
    else
        echo "WARNING: no checksum sidecar ($sha_file) — skipping checksum verification (backup predates this hardening?)" >&2
    fi

    local meta_file="$dump_file.meta.json" encrypted="false"
    if [[ -f "$meta_file" ]]; then
        encrypted="$(bc_meta_get "$meta_file" encrypted)"
    elif [[ "$dump_file" == *.enc ]]; then
        encrypted="true"
    fi

    local plain_file="$dump_file"
    if [[ "$encrypted" == "true" ]]; then
        echo "backup is encrypted — decrypting" >&2
        plain_file="$work_dir/$(basename "${dump_file%.enc}")"
        bc_decrypt "$dump_file" "$plain_file" || {
            echo "decryption failed — wrong or missing BACKUP_ENCRYPTION_KEY?" >&2
            return 1
        }
    fi

    gzip -t "$plain_file" || {
        echo "Corrupt or truncated backup file (gzip -t failed): $plain_file" >&2
        return 1
    }
    echo "gzip integrity OK" >&2

    if bc_looks_like_pg_dump "$plain_file"; then
        echo "content sniff OK (looks like a real pg_dump)" >&2
    else
        echo "WARNING: decompressed content does not look like a pg_dump header — proceeding anyway (sniff is best-effort, not authoritative)" >&2
    fi

    echo "$plain_file"
}
