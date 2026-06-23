// db/encryption.rs — SQLCipher data-at-rest encryption: key management,
// one-time plaintext→encrypted migration, and startup self-healing.
//
// RUNTIME DEPENDENCY (read before packaging a release build):
//   This build links SQLCipher's crypto calls dynamically against
//   `libcrypto-3-x64.dll` (OpenSSL 3.x). It does NOT statically vendor
//   OpenSSL (that path needs a full Perl/CPAN environment that isn't
//   available on this project's dev machine — see PRODUCTION_READINESS_AUDIT
//   discussion). Concretely:
//     - Exact DLL required: libcrypto-3-x64.dll (NOT libssl — SQLCipher only
//       calls into libcrypto's EVP/cipher primitives).
//     - Build-time: located via OPENSSL_DIR, defaulted in .cargo/config.toml
//       to PostgreSQL 18's bundled OpenSSL dev kit
//       ("C:\Program Files\PostgreSQL\18"). Any OpenSSL 3.x dev kit works;
//       override OPENSSL_DIR if that path doesn't exist on a given machine.
//     - Runtime: the OS resolves libcrypto-3-x64.dll via the standard search
//       order (the .exe's own directory, then system dirs, then PATH). On
//       this dev machine it happens to resolve via Git for Windows' and/or
//       PostgreSQL's own copies already being on PATH.
//     - PACKAGING IMPLICATION: a clean end-user machine will almost
//       certainly NOT have a compatible libcrypto-3-x64.dll on PATH. The
//       release installer/distribution MUST bundle this one DLL alongside
//       bank-statement-processor.exe (copy it from the OPENSSL_DIR used at
//       build time, e.g. "C:\Program Files\PostgreSQL\18\bin\libcrypto-3-x64.dll").
//
// WHAT HAPPENS IF THE DLL IS MISSING — empirically verified, not assumed:
//   `libcrypto-3-x64.dll` is a hard (implicit) import, confirmed via
//   `dumpbin /dependents` (it appears under the plain import list, not a
//   delay-load section). I tested this directly: running the built .exe
//   with a PATH stripped down to exclude every directory providing that
//   DLL, the process does not start at all — Windows' loader returns
//   STATUS_DLL_NOT_FOUND (0xC0000135, exit code -1073741515) before a
//   single line of this application's code executes, including `main()`.
//   On a real desktop this surfaces as Windows' own system dialog:
//   "The code execution cannot proceed because libcrypto-3-x64.dll was
//   not found. Reinstalling the program may fix this problem."
//
//   This means NO in-application diagnostic (toast, log line, startup
//   check) is reachable for a genuinely-missing DLL — there is no Rust
//   code path that runs before the OS loader's resolution step. Making
//   that interceptable would require converting the import to a delay-
//   loaded one (MSVC `/DELAYLOAD` + a custom `unsafe` failure hook) — given
//   this codebase has zero `unsafe` blocks today (a real, audited quality
//   signal), that tradeoff was rejected in favor of:
//     1. Documenting the exact error here so it's immediately recognizable
//        rather than mysterious.
//     2. Making the *installer/packaging step* responsible for verifying
//        the DLL is present (the only point where this is actually
//        preventable) — see PACKAGING IMPLICATION above.
//   What IS reachable and implemented below (see `diagnostics()` and its
//   call site in `main.rs`): once the process has launched — meaning the
//   hard DLL dependency was already satisfied — confirm SQLCipher is
//   genuinely active via `PRAGMA cipher_version`/`cipher_provider`, and
//   surface any `db::open()` failure to the user via the UI instead of
//   only a log line, since "the database silently isn't available" was
//   the previous (also audit-flagged) behavior.

use anyhow::{Context, Result};
use rand::RngExt;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

const KEYRING_SERVICE: &str = "bank-statement-processor";
// Tests use a distinct keyring entry so `cargo test` never reads/writes/
// deletes the real encryption key for a database a user is actually using.
#[cfg(not(test))]
const KEYRING_USERNAME: &str = "db_encryption_key";
#[cfg(test)]
const KEYRING_USERNAME: &str = "db_encryption_key_test";

// Every test anywhere in the crate that calls `db::open()` on a real file
// path (not ":memory:") ends up touching the single shared OS keyring entry
// above. Rust runs tests in parallel by default, so without a lock shared
// across module boundaries, this module's own tests and
// `db::tests::real_file_database_opens_idempotently_across_repeated_opens`
// race on that one keychain entry and intermittently fail. Exposed
// crate-wide (not just to this module's own tests) for exactly that reason.
#[cfg(test)]
pub(crate) static ENCRYPTION_KEYRING_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Returns the SQLCipher raw-key literal (`x'<64 hex chars>'`) to pass to
/// `PRAGMA key`, generating and persisting a new random 256-bit key in the
/// OS credential store on first use.
fn key_literal() -> Result<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USERNAME)
        .context("open OS credential store entry for db encryption key")?;
    let hex = match entry.get_password() {
        Ok(h) => h,
        Err(keyring::Error::NoEntry) => {
            let hex = generate_key_hex();
            entry
                .set_password(&hex)
                .context("store newly generated db encryption key in OS credential store")?;
            hex
        }
        Err(e) => return Err(e).context("read db encryption key from OS credential store"),
    };
    Ok(format!("x'{hex}'"))
}

fn generate_key_hex() -> String {
    let mut bytes = [0u8; 32]; // 256-bit key
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".bak");
    PathBuf::from(s)
}

fn migrating_tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".migrating");
    PathBuf::from(s)
}

/// Opens `path` with `key` set and forces SQLCipher to actually attempt a
/// decrypt (PRAGMA key alone doesn't validate anything — SQLite only reads
/// the header lazily on first real access) so a wrong/missing key fails
/// here, not on some unrelated later query deep in the app.
fn try_open_with_key(path: &Path, key: &str) -> Result<Connection> {
    let conn = Connection::open(path).context("open db file")?;
    conn.pragma_update(None, "key", key)
        .context("set encryption key")?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))
        .context("validate encryption key by reading sqlite_master")?;
    Ok(conn)
}

/// True if `path` is openable as a normal, unencrypted SQLite file.
fn is_plaintext_sqlite(path: &Path) -> bool {
    let Ok(conn) = Connection::open(path) else { return false };
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))
        .is_ok()
}

/// Opens (or creates) the database at `path` as an encrypted SQLCipher
/// database, transparently migrating an existing plaintext database the
/// first time this runs against it, and self-healing from `<path>.bak` if a
/// previous migration attempt was interrupted (process killed, machine lost
/// power, etc. mid-migration).
///
/// Safety invariants this function upholds:
///   - The original plaintext file is never modified or removed until a
///     verified encrypted replacement exists.
///   - `<path>.bak` is created before any encrypted copy is attempted, and
///     is never deleted by this code path (only ever read from, to recover).
///   - The encrypted replacement only takes the place of `path` after
///     `PRAGMA integrity_check` returns "ok" AND a fresh re-open with the
///     real key succeeds AND every table's row count matches the original.
pub fn open_encrypted(path: &Path) -> Result<Connection> {
    let key = key_literal()?;
    let mut recovered_once = false;

    loop {
        if !path.exists() {
            let conn = Connection::open(path).context("create new db file")?;
            conn.pragma_update(None, "key", &key)
                .context("set encryption key on new db")?;
            return Ok(conn);
        }

        if let Ok(conn) = try_open_with_key(path, &key) {
            return Ok(conn); // already encrypted with our key — the normal case after first migration
        }

        if is_plaintext_sqlite(path) {
            migrate_plaintext_to_encrypted(path, &key)
                .context("migrate plaintext database to encrypted")?;
            return try_open_with_key(path, &key).context("open freshly migrated encrypted db");
        }

        // `path` is neither openable with our key nor plaintext: this is the
        // "process died mid-migration" recovery case. The backup is always
        // a plaintext copy (we only ever create it from a confirmed-
        // plaintext source), so restoring it and looping retries the
        // `is_plaintext_sqlite` branch above, which migrates it again.
        if recovered_once {
            anyhow::bail!(
                "{path:?} remained unreadable (not openable with the stored key, not plaintext) \
                 even after restoring from backup — manual recovery required"
            );
        }
        let bak = backup_path(path);
        if !bak.exists() {
            anyhow::bail!(
                "{path:?} is unreadable with the stored key, is not a plaintext SQLite file, \
                 and no backup exists at {bak:?} to recover from"
            );
        }
        log::warn!(
            "[db] {path:?} looks corrupted or partially migrated — restoring from backup {bak:?} and retrying"
        );
        std::fs::copy(&bak, path).context("restore from .bak after detecting a corrupted/partial db file")?;
        recovered_once = true;
    }
}

/// Confirms encryption is genuinely active on an already-open connection by
/// querying SQLCipher's own `cipher_version`/`cipher_provider` pragmas, and
/// returns a one-line summary suitable for a startup log message.
///
/// This is NOT a check for whether the crypto DLL is *missing* — by the time
/// any of this code runs, the hard DLL dependency has already been resolved
/// by the OS loader, or the process wouldn't have started at all (see the
/// RUNTIME DEPENDENCY comment at the top of this file). What this DOES catch
/// is a more subtle problem: a `libcrypto-3-x64.dll` that loads (so the
/// process starts) but isn't actually a build SQLCipher can use — e.g. the
/// wrong major version, or a build missing expected symbols — which would
/// otherwise show up only as a confusing later failure on the first real
/// `PRAGMA key`/query.
pub fn diagnostics(conn: &Connection) -> String {
    let version: Option<String> = conn
        .query_row("PRAGMA cipher_version", [], |r| r.get(0))
        .ok();
    let provider: Option<String> = conn
        .query_row("PRAGMA cipher_provider", [], |r| r.get(0))
        .ok();
    match (version, provider) {
        (Some(v), Some(p)) => format!("SQLCipher {v} active (crypto provider: {p})"),
        (Some(v), None) => format!("SQLCipher {v} active (crypto provider unknown)"),
        (None, _) => "WARNING: SQLCipher version pragma returned nothing — encryption may not be active as expected".to_string(),
    }
}

/// Performs the one-time plaintext→encrypted migration for `path`:
/// backup → export to a temp encrypted copy → verify → atomically replace.
/// On any failure before the final replace, `path` (the original plaintext
/// file) is left completely untouched, so the caller's normal plaintext
/// open path remains available — the app still starts.
fn migrate_plaintext_to_encrypted(path: &Path, key: &str) -> Result<()> {
    let bak = backup_path(path);
    if !bak.exists() {
        std::fs::copy(path, &bak).context("create .bak backup before migrating to encrypted db")?;
        log::info!("[db] backed up plaintext db to {bak:?} before encrypting");
    } else {
        log::info!("[db] {bak:?} already exists from a previous migration attempt — reusing it as the safety backup");
    }

    let tmp = migrating_tmp_path(path);
    let _ = std::fs::remove_file(&tmp); // clear out any stale partial attempt

    {
        let conn = Connection::open(path).context("open plaintext db for export")?;
        conn.execute(
            "ATTACH DATABASE ?1 AS encrypted KEY ?2",
            rusqlite::params![tmp.to_string_lossy(), key],
        )
        .context("ATTACH new encrypted db")?;
        conn.execute_batch("SELECT sqlcipher_export('encrypted');")
            .context("sqlcipher_export into encrypted db")?;
        conn.execute_batch("DETACH DATABASE encrypted;")
            .context("DETACH encrypted db")?;
        // `conn` (and the attached encrypted db) close here, before we touch
        // any files below — required on Windows, which won't let an open
        // file be renamed.
    }

    if let Err(e) = verify_migrated_db(path, &tmp, key) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).context("verification failed — original plaintext db left untouched, backup retained");
    }

    std::fs::rename(&tmp, path).context("replace plaintext db with verified encrypted copy")?;
    log::info!("[db] migrated {path:?} to an encrypted database (plaintext backup retained at {bak:?})");
    Ok(())
}

/// Verifies a freshly-exported encrypted database before it's allowed to
/// replace the original: integrity_check must pass, and every real table's
/// row count must match the still-untouched original exactly.
fn verify_migrated_db(original_path: &Path, new_path: &Path, key: &str) -> Result<()> {
    let new_conn = try_open_with_key(new_path, key).context("open migrated db for verification")?;

    let integrity: String = new_conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .context("run integrity_check on migrated db")?;
    if integrity != "ok" {
        anyhow::bail!("integrity_check on migrated db returned {integrity:?}, expected \"ok\"");
    }

    // SQLCipher's own check, distinct from the generic one above: walks
    // every page and verifies its HMAC against the real key, which catches
    // encryption-specific corruption (e.g. a bad page written mid-crash)
    // that a plain integrity_check can't see since it just confirms the
    // (already-decrypted) B-tree structure is consistent. Returns zero rows
    // on success, one row per problem found otherwise.
    let cipher_problems: Vec<String> = {
        let mut stmt = new_conn
            .prepare("PRAGMA cipher_integrity_check")
            .context("prepare cipher_integrity_check")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))
            .context("run cipher_integrity_check on migrated db")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect cipher_integrity_check results")?
    };
    if !cipher_problems.is_empty() {
        anyhow::bail!("cipher_integrity_check on migrated db found problems: {}", cipher_problems.join("; "));
    }

    let orig_conn = Connection::open(original_path).context("reopen original plaintext db for row-count comparison")?;
    let tables: Vec<String> = {
        let mut stmt = orig_conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
            .context("list tables in original db")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("collect table names")?
    };

    for table in &tables {
        let orig_count: i64 = orig_conn
            .query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |r| r.get(0))
            .with_context(|| format!("count rows in original.{table}"))?;
        let new_count: i64 = new_conn
            .query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |r| r.get(0))
            .with_context(|| format!("count rows in migrated.{table}"))?;
        if orig_count != new_count {
            anyhow::bail!(
                "row count mismatch after migration in table {table}: original={orig_count} migrated={new_count}"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        ENCRYPTION_KEYRING_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clear_test_key() {
        if let Ok(e) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USERNAME) {
            let _ = e.delete_credential();
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("bsp_enc_test_{}_{}.db", std::process::id(), name))
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(backup_path(path));
        let _ = std::fs::remove_file(migrating_tmp_path(path));
    }

    fn make_plaintext_db_with_data(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE clients (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE transactions (id INTEGER PRIMARY KEY, narration TEXT, account_no TEXT);
             INSERT INTO clients (name) VALUES ('Acme Co'), ('Beta Ltd');
             INSERT INTO transactions (narration, account_no) VALUES
                ('UPI/RAMESH KUMAR/salary', '1234567890'),
                ('AIRTEL POSTPAID BILL', '1234567890'),
                ('NEFT/Vendor Payment', '9876543210');",
        )
        .unwrap();
    }

    #[test]
    fn key_literal_is_generated_once_and_stable_across_calls() {
        let _guard = lock();
        clear_test_key();
        let a = key_literal().unwrap();
        let b = key_literal().unwrap();
        assert_eq!(a, b, "the same stored key must be returned on every call");
        assert!(a.starts_with("x'") && a.ends_with('\''));
        assert_eq!(a.len(), 2 + 64 + 1, "x' + 64 hex chars + '");
        clear_test_key();
    }

    #[test]
    fn fresh_database_is_created_encrypted() {
        let _guard = lock();
        clear_test_key();
        let path = temp_path("fresh");
        cleanup(&path);

        {
            let conn = open_encrypted(&path).expect("create fresh encrypted db");
            conn.execute_batch("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('secret');").unwrap();
        }
        // A keyless open must NOT be able to read it — proves it's really encrypted.
        assert!(
            !is_plaintext_sqlite(&path),
            "freshly created db must not be readable without the key"
        );
        // Re-opening through the normal path must still work (key persisted in keyring).
        let conn = open_encrypted(&path).expect("re-open fresh encrypted db");
        let v: String = conn.query_row("SELECT v FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(v, "secret");

        cleanup(&path);
        clear_test_key();
    }

    #[test]
    fn diagnostics_confirms_sqlcipher_is_active_on_an_encrypted_connection() {
        let _guard = lock();
        clear_test_key();
        let path = temp_path("diag");
        cleanup(&path);

        let conn = open_encrypted(&path).expect("create fresh encrypted db");
        let summary = diagnostics(&conn);
        assert!(
            summary.starts_with("SQLCipher "),
            "expected a real version string, got: {summary}"
        );
        assert!(!summary.contains("WARNING"), "got: {summary}");

        cleanup(&path);
        clear_test_key();
    }

    #[test]
    fn migrates_existing_plaintext_db_and_preserves_all_data() {
        let _guard = lock();
        clear_test_key();
        let path = temp_path("migrate");
        cleanup(&path);
        make_plaintext_db_with_data(&path);
        assert!(is_plaintext_sqlite(&path), "precondition: starts plaintext");

        let conn = open_encrypted(&path).expect("migrate plaintext db to encrypted");

        // Data survived the migration intact.
        let client_count: i64 = conn.query_row("SELECT count(*) FROM clients", [], |r| r.get(0)).unwrap();
        assert_eq!(client_count, 2);
        let narr: String = conn
            .query_row("SELECT narration FROM transactions WHERE account_no = '9876543210'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(narr, "NEFT/Vendor Payment");

        // The main file is now genuinely encrypted, not plaintext.
        assert!(!is_plaintext_sqlite(&path), "migrated file must be encrypted, not plaintext");

        // The backup exists, was never deleted, and is itself still plaintext
        // and fully readable — the safety net the user asked for.
        let bak = backup_path(&path);
        assert!(bak.exists(), ".bak must exist after migration");
        assert!(is_plaintext_sqlite(&bak), ".bak must remain a readable plaintext copy");
        let bak_conn = Connection::open(&bak).unwrap();
        let bak_count: i64 = bak_conn.query_row("SELECT count(*) FROM clients", [], |r| r.get(0)).unwrap();
        assert_eq!(bak_count, 2, ".bak must contain the original data");

        // No leftover temp file.
        assert!(!migrating_tmp_path(&path).exists());

        cleanup(&path);
        clear_test_key();
    }

    #[test]
    fn reopening_an_already_migrated_db_does_not_remigrate() {
        let _guard = lock();
        clear_test_key();
        let path = temp_path("idempotent");
        cleanup(&path);
        make_plaintext_db_with_data(&path);

        open_encrypted(&path).unwrap(); // first open: migrates
        let bak_mtime_1 = std::fs::metadata(backup_path(&path)).unwrap().modified().unwrap();

        // Second open must NOT touch the backup again (no re-migration).
        let conn2 = open_encrypted(&path).expect("second open of already-encrypted db");
        let bak_mtime_2 = std::fs::metadata(backup_path(&path)).unwrap().modified().unwrap();
        assert_eq!(bak_mtime_1, bak_mtime_2, "re-opening must not touch the existing backup");

        let client_count: i64 = conn2.query_row("SELECT count(*) FROM clients", [], |r| r.get(0)).unwrap();
        assert_eq!(client_count, 2);

        cleanup(&path);
        clear_test_key();
    }

    #[test]
    fn startup_recovery_restores_from_backup_after_interrupted_migration() {
        let _guard = lock();
        clear_test_key();
        let path = temp_path("recovery");
        cleanup(&path);
        make_plaintext_db_with_data(&path);

        // Establish the key and a real backup the way a first real migration would.
        let key = key_literal().unwrap();
        std::fs::copy(&path, backup_path(&path)).unwrap();

        // Simulate a process death mid-migration: the main file is now garbage
        // (e.g. a truncated partial write), neither valid plaintext nor
        // openable with the real key.
        std::fs::write(&path, b"not a valid sqlite file at all").unwrap();
        assert!(!is_plaintext_sqlite(&path));
        assert!(try_open_with_key(&path, &key).is_err());

        // open_encrypted must detect this, restore from .bak, and complete
        // the migration successfully rather than failing outright.
        let conn = open_encrypted(&path).expect("must self-heal from backup and complete migration");
        let client_count: i64 = conn.query_row("SELECT count(*) FROM clients", [], |r| r.get(0)).unwrap();
        assert_eq!(client_count, 2, "data recovered via backup must be intact");
        assert!(!is_plaintext_sqlite(&path), "recovered db must end up encrypted, not left plaintext");

        cleanup(&path);
        clear_test_key();
    }

    #[test]
    fn fails_loudly_when_corrupted_with_no_backup_to_recover_from() {
        let _guard = lock();
        clear_test_key();
        let path = temp_path("nobak");
        cleanup(&path); // ensures no .bak exists either
        std::fs::write(&path, b"garbage, not a sqlite file, and no .bak anywhere").unwrap();

        let result = open_encrypted(&path);
        assert!(result.is_err(), "must not silently succeed or fabricate a working db with no recovery source");

        cleanup(&path);
        clear_test_key();
    }
}
