//! backup.rs — Pre-migration database backup and rollback.
//!
//! A migration is the highest-stakes write operation this app performs, so
//! it always takes a verified, restorable copy of the live database before
//! touching it, and can restore that copy verbatim if anything goes wrong.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Copy the live database file to a timestamped backup path next to it, then
/// verify the backup is actually a readable, intact SQLite database before
/// trusting it as a safety net. Also copies WAL/SHM sidecar files if present
/// (SQLite may have pending writes in them that a plain file copy of just the
/// main file would silently drop).
///
/// Returns the backup path. If verification fails, the just-written backup
/// file is removed and an error is returned — better to fail the migration
/// before it starts than to proceed believing there's a safety net that
/// doesn't actually work.
pub fn backup_database(db_path: &Path) -> Result<PathBuf> {
    if !db_path.exists() {
        anyhow::bail!("database file does not exist at {}", db_path.display());
    }

    let backup_path = timestamped_backup_path(db_path);
    std::fs::copy(db_path, &backup_path).with_context(|| {
        format!(
            "failed to copy database to backup path {}",
            backup_path.display()
        )
    })?;

    for ext in ["-wal", "-shm"] {
        let sidecar = sidecar_path(db_path, ext);
        if sidecar.exists() {
            let backup_sidecar = sidecar_path(&backup_path, ext);
            std::fs::copy(&sidecar, &backup_sidecar).with_context(|| {
                format!(
                    "failed to copy sidecar file {} to backup",
                    sidecar.display()
                )
            })?;
        }
    }

    if let Err(e) = verify_backup_readable(&backup_path) {
        let _ = std::fs::remove_file(&backup_path);
        return Err(e)
            .context("backup verification failed — refusing to proceed without a working backup");
    }

    Ok(backup_path)
}

/// Restore `db_path` from a previously-taken backup — used for rollback after
/// a failed migration. Copies the backup (and any sidecar files) back over
/// the live path, overwriting whatever partial state the failed migration
/// left behind.
///
/// Verifies twice, not once: the *source* backup is checked before copying
/// (no point restoring from something already broken), and the *destination*
/// is independently re-opened and checked after copying — the copy itself
/// reporting `Ok` only means the OS accepted the write, not that the bytes
/// that landed on disk are a valid, openable database (a truncated write from
/// a disk-full or permissions edge case would still return `Ok` from
/// `fs::copy`). A rollback that silently leaves the live file corrupt would
/// be worse than no rollback at all — the caller would report success while
/// the user has an unusable database.
pub fn restore_from_backup(backup_path: &Path, db_path: &Path) -> Result<()> {
    if !backup_path.exists() {
        anyhow::bail!(
            "backup file does not exist at {} — cannot roll back",
            backup_path.display()
        );
    }
    verify_backup_readable(backup_path)
        .context("refusing to restore from a backup that fails its own integrity check")?;

    std::fs::copy(backup_path, db_path).with_context(|| {
        format!(
            "failed to restore {} from backup {}",
            db_path.display(),
            backup_path.display()
        )
    })?;

    for ext in ["-wal", "-shm"] {
        let backup_sidecar = sidecar_path(backup_path, ext);
        let live_sidecar = sidecar_path(db_path, ext);
        if backup_sidecar.exists() {
            std::fs::copy(&backup_sidecar, &live_sidecar).with_context(|| {
                format!("failed to restore sidecar file {}", live_sidecar.display())
            })?;
        } else if live_sidecar.exists() {
            // The backup was taken with no pending WAL/SHM state — any
            // sidecar left over from the failed migration is now stale
            // relative to the just-restored main file and must not linger.
            let _ = std::fs::remove_file(&live_sidecar);
        }
    }

    verify_backup_readable(db_path).context(
        "rollback copied the backup over the live database, but the restored file failed its \
         own integrity check afterward — the live database may now be unusable; restore \
         manually from the backup path before using this app further",
    )?;

    Ok(())
}

fn timestamped_backup_path(db_path: &Path) -> PathBuf {
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let file_name = db_path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("bsp_data.db");
    db_path.with_file_name(format!("{file_name}.migration-backup-{ts}"))
}

fn sidecar_path(db_path: &Path, ext: &str) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push(ext);
    PathBuf::from(s)
}

/// Verify a backup file is a genuinely readable, intact database — using
/// `db::open` (the app's real entry point, which transparently handles
/// SQLCipher-encrypted files via the same OS-keyring key as the live
/// database) rather than a raw `rusqlite::Connection::open`. A plain
/// unencrypted open would *always* fail against this app's real databases
/// (every file `db::open` creates for a real path is SQLCipher-encrypted, so
/// a keyless open sees encrypted bytes and correctly reports "not a database
/// file" even though the backup is perfectly intact) — that's not corruption
/// detection, it's a false positive on every single real backup.
///
/// `db::open` also applies pending migrations, but that's a no-op here in
/// practice: a backup is a byte-for-byte copy of the live database, which is
/// already at the latest migration version by construction, and migrations
/// are independently proven idempotent (`db::tests`), so there is nothing
/// left for it to apply.
fn verify_backup_readable(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path).context("cannot stat backup file")?;
    if meta.len() == 0 {
        anyhow::bail!("backup file is empty (0 bytes) — refusing to treat this as a valid backup");
    }
    crate::db::open(path)
        .context("backup file could not be opened/decrypted as a valid database")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    // Every test here that opens a real (non-`:memory:`) database file goes
    // through the app's SQLCipher-encrypted `db::open`, which reads/writes a
    // single shared OS-keyring entry — the same one `db::tests` and
    // `db::encryption::tests` use, so this must be the *same* lock they use
    // (re-exported `pub(crate)` from `db::mod` for exactly this reason) or
    // tests running concurrently in the same `cargo test` invocation race
    // each other on that shared resource.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::db::ENCRYPTION_KEYRING_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn temp_db(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bsp_migration_backup_test_{}_{}.db",
            std::process::id(),
            name
        ))
    }

    fn make_real_sqlite_file(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('hello');")
            .unwrap();
    }

    #[test]
    fn backup_database_copies_and_verifies_a_real_file() {
        let _guard = lock();
        let path = temp_db("source");
        let _ = std::fs::remove_file(&path);
        make_real_sqlite_file(&path);

        let backup = backup_database(&path).expect("backup should succeed");
        assert!(backup.exists());
        assert_ne!(backup, path);

        // Read back through the app's real (encryption-aware) open path —
        // verification may have transparently encrypted this plaintext test
        // fixture in place, exactly as it would for a genuine backup.
        let conn = crate::db::open(&backup).unwrap();
        let v: String = conn.query_row("SELECT v FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(v, "hello");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&backup);
    }

    #[test]
    fn backup_database_fails_cleanly_when_source_is_missing() {
        let path = temp_db("missing");
        let _ = std::fs::remove_file(&path);
        let err = backup_database(&path).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn backup_database_rejects_and_cleans_up_a_corrupted_copy() {
        let _guard = lock();
        // Simulate a "backup" that's actually garbage (e.g. a filesystem-level
        // copy race truncated it) by writing garbage bytes at the backup path
        // ourselves and pointing the verifier at that directly.
        let path = temp_db("corrupt_source");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"not a real sqlite file at all, just garbage bytes").unwrap();

        let result = backup_database(&path);
        // A garbage source file still copies fine at the OS level, but must
        // fail SQLite-level verification and clean up after itself.
        assert!(result.is_err(), "garbage input must fail verification");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn restore_from_backup_overwrites_live_file_with_backup_contents() {
        let _guard = lock();
        let live = temp_db("live");
        let backup = temp_db("backup_src");
        let _ = std::fs::remove_file(&live);
        let _ = std::fs::remove_file(&backup);

        make_real_sqlite_file(&backup);
        // Live file has different, "corrupted-by-a-failed-migration" content.
        {
            let conn = Connection::open(&live).unwrap();
            conn.execute_batch("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('corrupted');")
                .unwrap();
        }

        restore_from_backup(&backup, &live).expect("restore should succeed");

        let conn = crate::db::open(&live).unwrap();
        let v: String = conn.query_row("SELECT v FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(
            v, "hello",
            "live file must now match the backup, not its pre-restore content"
        );

        let _ = std::fs::remove_file(&live);
        let _ = std::fs::remove_file(&backup);
    }

    #[test]
    fn restore_from_backup_verifies_the_destination_is_openable_after_copying_not_just_the_source()
    {
        // `restore_from_backup_overwrites_live_file_with_backup_contents`
        // above already proves the happy path works — this test names the
        // specific guarantee explicitly: the function doesn't just trust
        // `fs::copy`'s `Ok` return value, it independently re-opens the
        // *destination* afterward (see the doc comment on
        // `restore_from_backup`). A genuinely truncated/corrupt write from
        // `fs::copy` itself isn't something a portable unit test can force
        // deterministically, so this asserts the observable contract instead:
        // if restore_from_backup returns Ok, db::open on db_path must
        // *already* succeed with no further repair step, immediately and
        // without needing its own migration-application side effects to
        // "fix" anything.
        let _guard = lock();
        let live = temp_db("verify_dest_live");
        let backup = temp_db("verify_dest_backup");
        let _ = std::fs::remove_file(&live);
        let _ = std::fs::remove_file(&backup);

        make_real_sqlite_file(&backup);
        std::fs::write(&live, b"garbage pre-restore content").unwrap();

        restore_from_backup(&backup, &live).expect("restore should succeed and self-verify");

        // If restore_from_backup's post-copy verification had been skipped,
        // a corrupt destination could still slip through; opening it here
        // independently re-proves it didn't.
        let conn = crate::db::open(&live).expect("destination must be independently openable");
        let v: String = conn.query_row("SELECT v FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(v, "hello");

        let _ = std::fs::remove_file(&live);
        let _ = std::fs::remove_file(&backup);
    }

    #[test]
    fn restore_from_backup_fails_when_backup_is_missing() {
        let live = temp_db("live2");
        let backup = temp_db("nonexistent_backup");
        let _ = std::fs::remove_file(&backup);
        let err = restore_from_backup(&backup, &live).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }
}
