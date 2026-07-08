//! migration/ — Complete data-migration framework for moving an existing
//! user of the old Electron `bank-statement-processing` app onto this Rust
//! app without losing data.
//!
//! # Where the data comes from
//!
//! The old app stores everything in the browser `localStorage` API — it has
//! no SQLite database (verified directly against its `db.js` /
//! `src/config/config.js` source). The supported input is a JSON dump of
//! that `localStorage`, produced by a one-line DevTools console command with
//! no code changes to the old app. See `detector.rs`'s module doc for the
//! exact command and the reasoning for not attempting to parse Chromium's
//! on-disk LevelDB encoding directly (real corruption/mis-read risk for a
//! migration tool, where silent data loss is the worst possible outcome).
//!
//! # Flow
//!
//! ```text
//! export.json ──▶ detector::parse_export_file  (read + sniff the dump)
//!                       │
//!                       ▼
//!              detector::detect          (what's present, how much)
//!                       │
//!                       ▼
//!              backup::backup_database   (copy + verify the LIVE db first)
//!                       │
//!                       ▼
//!              importer::import_all      (clients → rules/ledgers/dedupe
//!                       │                  → history+transactions → settings;
//!                       │                  duplicate-safe by natural key)
//!              success? │  failure?
//!                       │      │
//!                       │      ▼
//!                       │  backup::restore_from_backup  (undo everything)
//!                       ▼
//!              validator::validate_migrated (row-count sanity check)
//!              fails?  │  passes?
//!                 │    │
//!                 ▼    ▼
//!         (rollback, as above)   report::MigrationReport (success)
//! ```
//!
//! Every path — success, import failure, or post-validation failure — ends
//! in a [`report::MigrationReport`], never a bare error the UI has nothing to
//! show for; see [`migrate`].

pub mod backup;
pub mod detector;
pub mod importer;
pub mod report;
pub mod transformer;
pub mod validator;

use std::path::Path;

pub use detector::DetectedSource;
pub use report::{EntityReport, MigrationReport};

/// Tunables for a migration run. `skip_backup` exists only for tests that
/// operate on an in-memory database (which has no file to back up) — real
/// invocations must always leave it `false`.
#[derive(Debug, Clone)]
pub struct MigrationOptions {
    pub skip_backup: bool,
}

impl Default for MigrationOptions {
    fn default() -> Self {
        MigrationOptions { skip_backup: false }
    }
}

/// Detect what a candidate export file contains without importing anything —
/// used to preview a migration before committing to it (e.g. in a
/// confirmation dialog: "found 3 clients, 1,204 transactions...").
pub fn preview(export_path: &Path) -> anyhow::Result<DetectedSource> {
    let export = detector::parse_export_file(export_path)?;
    detector::detect(&export)
}

/// Run a complete migration: detect → backup → import → validate → report,
/// with automatic rollback to the pre-migration backup on any failure.
///
/// Deliberately opens and closes **its own** connection to `db_path` rather
/// than accepting one the caller already has open. This app runs its
/// database in WAL mode (`PRAGMA journal_mode = WAL`, `db::open`), and
/// replacing the on-disk `.db`/`.db-wal`/`.db-shm` files (what rollback does)
/// while another connection still has them open is not safe — that
/// connection's in-memory understanding of the WAL/shared-memory index can
/// go stale or mismatch the just-restored files in ways SQLite does not
/// define reliable behavior for. Closing the connection *before* any
/// restore, and never reopening it until this function has fully returned,
/// avoids that entirely.
///
/// **Callers must not use a connection to `db_path` opened before this call,
/// and must open a fresh one afterwards** — regardless of whether the
/// migration succeeded, failed, or rolled back, the file on disk may no
/// longer be what any pre-existing connection thinks it is.
///
/// This function does not return `Err` for ordinary migration failures
/// (a corrupt export, a write error, a failed post-validation check) — those
/// are always captured in the returned [`MigrationReport`] (`success:
/// false`, with `errors` and recovery instructions via
/// [`MigrationReport::to_markdown`]) so the UI always has something
/// meaningful to show. `Err` is reserved for failures so fundamental that no
/// report can be trusted (e.g. the database file itself can't be opened at
/// all).
///
/// `progress` is called at each named phase with `(percent_complete,
/// phase_label)`, `percent_complete` monotonically non-decreasing from 0 to
/// 100 across a single call — driving a real progress bar, not just a status
/// line. The exact phases and their assigned percentages are internal and
/// may shift between releases; callers should treat them only as "moved
/// forward", not rely on specific values or a fixed phase count (phases
/// covering entities absent from the export, or invalidated by a fatal
/// pre-check, are skipped — see `importer::import_all`).
pub fn migrate(
    export_path: &Path,
    db_path: &Path,
    opts: &MigrationOptions,
    mut progress: impl FnMut(i32, &str),
) -> anyhow::Result<MigrationReport> {
    let mut report = MigrationReport::new(&export_path.display().to_string());

    progress(2, "Reading export file\u{2026}");
    let export = match detector::parse_export_file(export_path) {
        Ok(e) => e,
        Err(err) => {
            report.error(format!("{err:#}"));
            report.finish(false, false);
            return Ok(report);
        }
    };

    if let Err(err) = detector::detect(&export) {
        report.error(format!("{err:#}"));
        report.finish(false, false);
        return Ok(report);
    }

    // A pre-migration backup is mandatory for any real run: without one,
    // there is nothing to roll back to if the import fails partway, which
    // would leave the live database in a genuinely unknown, unrecoverable
    // state. Refuse to proceed rather than import "optimistically".
    let backup_path = if opts.skip_backup {
        None
    } else {
        match backup::backup_database(db_path) {
            Ok(p) => {
                report.backup_path = Some(p.display().to_string());
                Some(p)
            }
            Err(err) => {
                report.error(format!(
                    "Could not create a pre-migration backup, so the migration was not started: {err:#}"
                ));
                report.finish(false, false);
                return Ok(report);
            }
        }
    };

    progress(5, "Importing data\u{2026}");
    // Opened and dropped entirely within this block: by the time rollback
    // (if needed) runs below, nothing still holds `db_path` open.
    let (import_result, post_check_issues) = {
        let conn = match crate::db::open(db_path) {
            Ok(c) => c,
            Err(err) => {
                report.error(format!("Could not open the database for import: {err:#}"));
                report.finish(false, false);
                return Ok(report);
            }
        };
        let import_result = importer::import_all(&conn, &export, &mut report, |pct, phase| {
            progress(pct, phase)
        });
        let post_check_issues = if import_result.is_ok() {
            validator::validate_migrated(&conn, &report)
        } else {
            Vec::new()
        };
        (import_result, post_check_issues)
        // `conn` dropped here.
    };

    let post_check_failed = post_check_issues
        .iter()
        .any(|i| i.severity == validator::Severity::Fatal);

    if let Err(err) = &import_result {
        report.error(format!("Import failed: {err:#}"));
    }
    for issue in &post_check_issues {
        report.error(format!("[{}] {}", issue.entity, issue.message));
    }

    if import_result.is_err() || post_check_failed {
        progress(97, "Rolling back\u{2026}");
        match &backup_path {
            Some(bp) => match backup::restore_from_backup(bp, db_path) {
                Ok(()) => {
                    report.finish(false, true);
                }
                Err(restore_err) => {
                    report.error(format!(
                        "CRITICAL: automatic rollback itself failed: {restore_err:#}. \
                         Your data may be in a partially-migrated state. Manually restore \
                         from the backup file listed above before using this app further."
                    ));
                    report.finish(false, false);
                }
            },
            None => {
                // opts.skip_backup was set (test-only path) — nothing to roll
                // back to; the caller accepted that risk explicitly.
                report.finish(false, false);
            }
        }
        return Ok(report);
    }

    progress(100, "Done");
    report.finish(true, false);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // See `backup::tests::lock` — every test here that opens a real database
    // file (i.e. everything except the `preview_*` tests, which only touch
    // the export JSON) shares the same OS-keyring-backed encryption key as
    // `db::tests`/`db::encryption::tests`/`backup::tests` and must serialize
    // on the same lock to avoid racing them.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::db::ENCRYPTION_KEYRING_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bsp_migration_e2e_{}_{}.db",
            std::process::id(),
            name
        ))
    }

    fn write_export(name: &str, json: &serde_json::Value) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bsp_migration_export_{}_{}.json",
            std::process::id(),
            name
        ));
        std::fs::write(&path, json.to_string()).unwrap();
        path
    }

    fn happy_export() -> serde_json::Value {
        serde_json::json!({
            "bsp_clients": [{"id": "c_1", "name": "Acme Co", "tallyLedger": "Acme Bank", "createdAt": "2020-01-01T00:00:00.000Z"}],
            "bsp_rules": [{"id": "r_1", "clientId": "c_1", "pattern": "AMAZON", "accountHead": "Office Expense", "type": "Payment"}],
            "bsp_history": [{"id": "h_1", "clientId": "c_1", "fileName": "jan.xlsx", "txnCount": 1, "importedAt": "2020-02-01T00:00:00.000Z"}],
            "bsp_imp_h_1": [{"id": "t_1", "date": "01/01/2020", "narration": "SALARY", "credit": 1000.0, "balance": 1000.0}]
        })
    }

    #[test]
    fn preview_reports_counts_without_writing_anything() {
        let export_path = write_export("preview", &happy_export());
        let detected = preview(&export_path).expect("preview should succeed");
        assert_eq!(detected.count_of("clients"), 1);
        assert_eq!(detected.count_of("transactions"), 1);
        let _ = std::fs::remove_file(&export_path);
    }

    #[test]
    fn preview_fails_cleanly_on_garbage_input() {
        let export_path = write_export("garbage", &serde_json::json!("not an object"));
        assert!(preview(&export_path).is_err());
        let _ = std::fs::remove_file(&export_path);
    }

    #[test]
    fn migrate_end_to_end_happy_path_creates_backup_and_succeeds() {
        let _guard = lock();
        let db_path = temp_path("happy");
        let _ = std::fs::remove_file(&db_path);
        // Create the fresh (empty-schema) database, then close it — migrate()
        // owns its own connection lifecycle (see its doc comment) and no
        // connection to db_path may be held open across the call.
        drop(crate::db::open(&db_path).expect("open fresh db"));

        let export_path = write_export("happy", &happy_export());
        let mut progress_calls: Vec<(i32, String)> = Vec::new();

        let report = migrate(
            &export_path,
            &db_path,
            &MigrationOptions::default(),
            |pct, p| {
                progress_calls.push((pct, p.to_string()));
            },
        )
        .expect("migrate should not hard-error");

        assert!(report.success, "expected success, got: {:?}", report.errors);
        assert!(!report.rolled_back);
        assert!(
            report.backup_path.is_some(),
            "a backup must have been taken"
        );
        assert!(std::path::Path::new(report.backup_path.as_ref().unwrap()).exists());
        assert_eq!(
            report.total_imported(),
            4,
            "1 client + 1 rule + 1 import_history record + 1 transaction"
        );
        assert!(
            !progress_calls.is_empty(),
            "progress callback must be invoked"
        );
        // The percentage must actually drive a real progress bar, not sit at
        // 0 the whole time: monotonically non-decreasing, reaching 100 at
        // the very last call on a successful run.
        assert!(
            progress_calls.windows(2).all(|w| w[1].0 >= w[0].0),
            "percent must never go backwards, got: {:?}",
            progress_calls
        );
        assert_eq!(
            progress_calls.last().unwrap().0,
            100,
            "a successful migration must finish progress at 100%, got: {:?}",
            progress_calls
        );
        assert!(
            progress_calls
                .iter()
                .any(|(_, msg)| msg.contains("clients")),
            "expected a client-import phase in: {:?}",
            progress_calls
        );

        let conn = crate::db::open(&db_path).expect("reopen after migrate");
        let clients = crate::db::get_clients(&conn).unwrap();
        assert_eq!(clients.len(), 1);

        drop(conn);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&export_path);
        if let Some(bp) = &report.backup_path {
            let _ = std::fs::remove_file(bp);
        }
    }

    #[test]
    fn migrate_rejects_a_garbage_export_file_without_touching_the_database() {
        let _guard = lock();
        let db_path = temp_path("garbage_source");
        let _ = std::fs::remove_file(&db_path);
        drop(crate::db::open(&db_path).expect("open fresh db"));

        let export_path = write_export("garbage_source", &serde_json::json!({"not_bsp": true}));
        let report = migrate(
            &export_path,
            &db_path,
            &MigrationOptions::default(),
            |_, _| {},
        )
        .unwrap();

        assert!(!report.success);
        assert!(!report.errors.is_empty());
        // No backup should have been attempted — detection failed before that step.
        assert!(report.backup_path.is_none());

        let conn = crate::db::open(&db_path).expect("reopen after migrate");
        let clients = crate::db::get_clients(&conn).unwrap();
        assert!(clients.is_empty());

        drop(conn);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&export_path);
    }

    #[test]
    fn migrate_rolls_back_to_pre_migration_state_when_a_later_phase_fails() {
        // Force a real, deterministic, cross-platform-reliable mid-migration
        // SQL failure (an OS-level trick like flipping the file's read-only
        // attribute does *not* reliably block writes through an
        // already-open handle on every platform, notably Windows — this
        // instead sabotages the live schema directly: `classification_rules`
        // is dropped before migrate() ever runs, so the export's rule write
        // genuinely fails partway through `import_all`, exactly like a real
        // corrupted-schema scenario would).
        let _guard = lock();
        let db_path = temp_path("rollback");
        let _ = std::fs::remove_file(&db_path);
        let pre_migration_count = {
            let conn = crate::db::open(&db_path).expect("open fresh db");
            crate::db::add_client(&conn, "Pre-Existing Client", "Some Ledger").unwrap();
            // Drop a *column*, not the whole table: `db::open`'s schema init
            // uses `CREATE TABLE IF NOT EXISTS`, which is a no-op once the
            // table already exists — dropping the whole table gets silently
            // healed the moment migrate() reopens the connection, but a
            // missing column on an existing table survives that and makes
            // `db::add_rule`'s hardcoded INSERT genuinely fail.
            conn.execute(
                "ALTER TABLE classification_rules DROP COLUMN account_head",
                [],
            )
            .expect("sabotage the schema");
            crate::db::get_clients(&conn).unwrap().len()
            // conn dropped here
        };
        assert_eq!(pre_migration_count, 1);

        let export = serde_json::json!({
            "bsp_clients": [{"id": "c_1", "name": "New Client", "tallyLedger": ""}],
            "bsp_rules": [{"id": "r_1", "clientId": "c_1", "pattern": "X", "accountHead": "Y"}],
        });
        let export_path = write_export("rollback", &export);

        // migrate()'s own internal backup (taken first thing inside the
        // call) captures this already-sabotaged state, which is exactly
        // right: rollback's job is to undo whatever *migration* did, not to
        // repair pre-existing damage that predates the migration attempt.
        // What this proves is the part that actually matters:
        // `import_clients` (the first phase) successfully writes "New
        // Client" to the live, un-transacted database *before*
        // `import_rules` (the next phase) hits the missing column and
        // returns Err — a real partial write that rollback must undo.
        let mut report = migrate(
            &export_path,
            &db_path,
            &MigrationOptions::default(),
            |_, _| {},
        )
        .unwrap();

        assert!(!report.success);
        assert!(
            report.rolled_back,
            "a mid-migration write failure must trigger rollback, got errors: {:?}",
            report.errors
        );
        assert!(
            report.errors.iter().any(|e| e.contains("Import failed")),
            "got: {:?}",
            report.errors
        );
        assert_eq!(
            report.entity_mut("clients").imported,
            1,
            "import_clients must have actually run and written the new client before the failure"
        );

        let conn2 = crate::db::open(&db_path).expect("reopen after rollback");
        let post_rollback_count = crate::db::get_clients(&conn2).unwrap().len();
        assert_eq!(
            post_rollback_count, pre_migration_count,
            "the partially-written 'New Client' from the failed migration must have been rolled back"
        );

        drop(conn2);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&export_path);
        if let Some(bp) = &report.backup_path {
            let _ = std::fs::remove_file(bp);
        }
    }

    #[test]
    fn migrate_is_idempotent_across_two_full_end_to_end_runs() {
        let _guard = lock();
        let db_path = temp_path("idempotent_e2e");
        let _ = std::fs::remove_file(&db_path);
        drop(crate::db::open(&db_path).expect("open fresh db"));
        let export_path = write_export("idempotent_e2e", &happy_export());

        let r1 = migrate(
            &export_path,
            &db_path,
            &MigrationOptions::default(),
            |_, _| {},
        )
        .unwrap();
        assert!(r1.success, "first run failed: {:?}", r1.errors);
        let r2 = migrate(
            &export_path,
            &db_path,
            &MigrationOptions::default(),
            |_, _| {},
        )
        .unwrap();
        assert!(r2.success, "second run failed: {:?}", r2.errors);

        let conn = crate::db::open(&db_path).expect("reopen after migrate");
        let clients = crate::db::get_clients(&conn).unwrap();
        assert_eq!(
            clients.len(),
            1,
            "re-running migration must not duplicate the client"
        );

        drop(conn);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&export_path);
        for r in [&r1, &r2] {
            if let Some(bp) = &r.backup_path {
                let _ = std::fs::remove_file(bp);
            }
        }
    }
}
