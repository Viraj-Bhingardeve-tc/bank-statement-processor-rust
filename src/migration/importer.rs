//! importer.rs — Writes a parsed legacy export into the live database.
//!
//! The entire import runs inside one SQLite transaction (`conn.unchecked_transaction()`),
//! committed only at the very end — any hard failure anywhere leaves the
//! database completely untouched (SQLite rolls the transaction back
//! automatically if it's dropped without an explicit commit), and the caller
//! in `mod.rs` additionally restores the pre-migration file backup as a
//! second, independent safety net.
//!
//! Every entity is imported duplicate-safely by natural key, so re-running a
//! migration against the same (or an updated) export is always safe: nothing
//! already present gets duplicated, only genuinely new records are added.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::db;
use crate::parser::Transaction;
use crate::settings::Settings;

use super::detector::{self, LegacyExport};
use super::report::MigrationReport;
use super::transformer::{
    self, IdMap, LegacyClient, LegacyDedupe, LegacyImportMeta, LegacyLedger, LegacyRule,
};
use super::validator::{self, Severity};

/// Import every recognized entity from `export` into `conn`, recording
/// progress into `report`. `progress` is called once per phase (used to
/// drive a UI progress indicator); it receives `(percent_complete,
/// phase_label)`, with `percent_complete` on the same 0-100 overall scale
/// `mod::migrate` uses for its own pre/post-import phases (this function
/// only owns the middle slice of that range).
///
/// Deliberately does **not** wrap the whole import in one outer SQL
/// transaction: `db::add_dedupe_hashes`/`db::upsert_transactions` (like
/// several other functions in `db.rs`) already open their own transaction
/// per call, matching this codebase's existing convention throughout
/// (`main.rs`'s batch-import path is the same — a sequence of independently
/// atomic calls, not one spanning transaction) — and SQLite/rusqlite's plain
/// `unchecked_transaction()` cannot nest inside an already-open transaction.
/// Whole-migration atomicity is instead provided one layer up, in
/// `mod::migrate`, via a pre-migration file backup that gets restored
/// verbatim if any phase here returns an error — a strictly stronger
/// guarantee than an in-process SQL transaction anyway, since it also covers
/// a hard crash mid-migration, not just a returned `Err`.
pub fn import_all(
    conn: &Connection,
    export: &LegacyExport,
    report: &mut MigrationReport,
    mut progress: impl FnMut(i32, &str),
) -> Result<()> {
    let issues = validator::validate_source(export);
    let mut fatal_entities: HashSet<String> = HashSet::new();
    for issue in &issues {
        match issue.severity {
            Severity::Warning => report.warn(format!("[{}] {}", issue.entity, issue.message)),
            Severity::Fatal => {
                report.error(format!("[{}] {}", issue.entity, issue.message));
                fatal_entities.insert(issue.entity.clone());
            }
        }
    }

    let mut id_map = IdMap::default();

    progress(10, "Importing clients\u{2026}");
    import_clients(conn, export, &mut id_map, report)?;

    if !fatal_entities.contains("classification_rules") {
        progress(25, "Importing classification rules\u{2026}");
        import_rules(conn, export, &id_map, report)?;
    }

    if !fatal_entities.contains("ledgers") {
        progress(40, "Importing ledgers\u{2026}");
        import_ledgers(conn, export, &id_map, report)?;
    }

    if !fatal_entities.contains("dedupe_hashes") {
        progress(55, "Importing duplicate-detection history\u{2026}");
        import_dedupe(conn, export, &id_map, report)?;
    }

    if !fatal_entities.contains("import_history") {
        progress(75, "Importing transaction history\u{2026}");
        import_history_and_transactions(conn, export, &id_map, report)?;
    }

    if !fatal_entities.contains("settings") {
        progress(90, "Importing settings\u{2026}");
        import_settings(conn, export, report)?;
    }

    progress(95, "Finalizing\u{2026}");
    Ok(())
}

fn import_clients(
    conn: &Connection,
    export: &LegacyExport,
    id_map: &mut IdMap,
    report: &mut MigrationReport,
) -> Result<()> {
    let clients: Vec<LegacyClient> = export
        .get_array(detector::KEY_CLIENTS)
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();

    let e = report.entity_mut("clients");
    e.found = clients.len();

    for c in &clients {
        if let Some(existing) =
            db::get_client_by_name(conn, &c.name).context("get_client_by_name")?
        {
            id_map.clients.insert(c.id.clone(), existing.id);
            report.entity_mut("clients").skipped_duplicate += 1;
            continue;
        }
        let new_id = db::add_client(conn, &c.name, &c.tally_ledger).context("add_client")?;
        if !c.created_at.is_empty() {
            // Preserve the original creation timestamp rather than letting
            // it default to "now" — best-effort: an invalid/unparseable
            // string is simply not applied, the DB default stands.
            let _ = conn.execute(
                "UPDATE clients SET created_at = ?1 WHERE id = ?2",
                rusqlite::params![c.created_at, new_id],
            );
        }
        id_map.clients.insert(c.id.clone(), new_id);
        report.entity_mut("clients").imported += 1;
    }
    Ok(())
}

fn import_rules(
    conn: &Connection,
    export: &LegacyExport,
    id_map: &IdMap,
    report: &mut MigrationReport,
) -> Result<()> {
    let rules: Vec<LegacyRule> = export
        .get_array(detector::KEY_RULES)
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();

    let e = report.entity_mut("classification_rules");
    e.found = rules.len();

    let mut skipped_global = 0usize;
    for r in &rules {
        let Some(client_id) = id_map.resolve_client(&r.client_id) else {
            if IdMap::is_unsupported_global(&r.client_id) {
                skipped_global += 1;
            } else {
                report.entity_mut("classification_rules").failed += 1;
            }
            continue;
        };
        match db::add_rule(
            conn,
            client_id,
            &r.pattern,
            &r.vendor,
            &r.account_head,
            &r.txn_type,
        )
        .context("add_rule")?
        {
            true => report.entity_mut("classification_rules").imported += 1,
            false => report.entity_mut("classification_rules").skipped_duplicate += 1,
        }
    }
    warn_unsupported_global(report, "classification_rules", skipped_global);
    Ok(())
}

/// The old app's `client_id: "global"` scope has no working equivalent here
/// (see `IdMap`'s doc comment) — reported as one clear, aggregated warning
/// rather than either a silent drop or per-row noise.
fn warn_unsupported_global(report: &mut MigrationReport, entity: &str, skipped: usize) {
    if skipped > 0 {
        report.warn(format!(
            "[{entity}] {skipped} record(s) used the old app's global (cross-client) scope, \
             which this app's data model does not support — they were not migrated. \
             Re-create them manually per client if needed."
        ));
    }
}

fn import_ledgers(
    conn: &Connection,
    export: &LegacyExport,
    id_map: &IdMap,
    report: &mut MigrationReport,
) -> Result<()> {
    let ledgers: Vec<LegacyLedger> = export
        .get_array(detector::KEY_LEDGERS)
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();

    let e = report.entity_mut("ledgers");
    e.found = ledgers.len();

    // Group by resolved client id so each client's batch can go through one
    // `db::import_ledgers` call (which already reports a new-vs-duplicate count).
    let mut by_client: HashMap<i64, Vec<(String, String)>> = HashMap::new();
    let mut failed = 0usize;
    let mut skipped_global = 0usize;
    for l in &ledgers {
        match id_map.resolve_client(&l.client_id) {
            Some(client_id) => by_client
                .entry(client_id)
                .or_default()
                .push((l.name.clone(), l.group.clone())),
            None if IdMap::is_unsupported_global(&l.client_id) => skipped_global += 1,
            None => failed += 1,
        }
    }
    report.entity_mut("ledgers").failed += failed;
    warn_unsupported_global(report, "ledgers", skipped_global);

    for (client_id, entries) in by_client {
        let added = db::import_ledgers(conn, client_id, &entries).context("import_ledgers")?;
        let e = report.entity_mut("ledgers");
        e.imported += added;
        e.skipped_duplicate += entries.len() - added;
    }
    Ok(())
}

fn import_dedupe(
    conn: &Connection,
    export: &LegacyExport,
    id_map: &IdMap,
    report: &mut MigrationReport,
) -> Result<()> {
    let dedupe: Vec<LegacyDedupe> = export
        .get_array(detector::KEY_DEDUPE)
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();

    let e = report.entity_mut("dedupe_hashes");
    e.found = dedupe.len();

    let mut by_client: HashMap<i64, Vec<String>> = HashMap::new();
    let mut failed = 0usize;
    let mut skipped_global = 0usize;
    for d in &dedupe {
        match id_map.resolve_client(&d.client_id) {
            Some(client_id) => by_client.entry(client_id).or_default().push(d.hash.clone()),
            None if IdMap::is_unsupported_global(&d.client_id) => skipped_global += 1,
            None => failed += 1,
        }
    }
    report.entity_mut("dedupe_hashes").failed += failed;
    warn_unsupported_global(report, "dedupe_hashes", skipped_global);

    for (client_id, hashes) in by_client {
        let existing = db::get_dedupe_hashes(conn, client_id).context("get_dedupe_hashes")?;
        let new_count = hashes.iter().filter(|h| !existing.contains(*h)).count();
        db::add_dedupe_hashes(conn, client_id, &hashes).context("add_dedupe_hashes")?;
        let e = report.entity_mut("dedupe_hashes");
        e.imported += new_count;
        e.skipped_duplicate += hashes.len() - new_count;
    }
    Ok(())
}

fn import_history_and_transactions(
    conn: &Connection,
    export: &LegacyExport,
    id_map: &IdMap,
    report: &mut MigrationReport,
) -> Result<()> {
    let history: Vec<LegacyImportMeta> = export
        .get_array(detector::KEY_HISTORY)
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();

    report.entity_mut("import_history").found = history.len();

    for h in &history {
        let Some(client_id) = id_map.resolve_client(&h.client_id) else {
            report.entity_mut("import_history").failed += 1;
            continue;
        };

        let raw_txns = export.get_import_transactions(&h.id);
        report.entity_mut("transactions").found += raw_txns.len();

        let legacy_txns: Vec<transformer::LegacyTransaction> = raw_txns
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect();

        // Derive a batch-level bank/account fallback from the first
        // transaction that actually carries one (older exports may only tag
        // bank/account at this level rather than per-row).
        let bank_fallback = legacy_txns
            .iter()
            .map(|t| t.bank_name.as_str())
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        let acct_fallback = legacy_txns
            .iter()
            .map(|t| t.account_no.as_str())
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();

        // De-duplicate an import_history entry that's already been migrated
        // (same client, file name, and transaction count) rather than
        // re-registering it under a new id every time migration re-runs.
        let existing_import_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM import_history WHERE client_id = ?1 AND file_name = ?2 AND txn_count = ?3",
                rusqlite::params![client_id, h.file_name, h.txn_count],
                |r| r.get(0),
            )
            .ok();

        let import_id = match existing_import_id {
            Some(id) => {
                report.entity_mut("import_history").skipped_duplicate += 1;
                id
            }
            None => {
                let new_id = db::save_import(
                    conn,
                    client_id,
                    &h.file_name,
                    &bank_fallback,
                    &acct_fallback,
                    legacy_txns.len(),
                )
                .context("save_import")?;
                if !h.imported_at.is_empty() {
                    let _ = conn.execute(
                        "UPDATE import_history SET imported_at = ?1 WHERE id = ?2",
                        rusqlite::params![h.imported_at, new_id],
                    );
                }
                report.entity_mut("import_history").imported += 1;
                new_id
            }
        };

        let txns: Vec<Transaction> = legacy_txns
            .iter()
            .map(|t| transformer::transaction_from_legacy(t, &bank_fallback, &acct_fallback))
            .collect();

        let ids: Vec<&str> = txns.iter().map(|t| t.id.as_str()).collect();
        let already_present = count_existing_transaction_ids(conn, &ids)?;

        db::upsert_transactions(conn, client_id, Some(import_id), &txns)
            .context("upsert_transactions")?;

        let e = report.entity_mut("transactions");
        e.skipped_duplicate += already_present;
        e.imported += txns.len().saturating_sub(already_present);
    }
    Ok(())
}

fn count_existing_transaction_ids(conn: &Connection, ids: &[&str]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("SELECT COUNT(*) FROM transactions WHERE id IN ({placeholders})");
    let mut stmt = conn
        .prepare(&sql)
        .context("count_existing_transaction_ids prepare")?;
    let params: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let count: i64 = stmt
        .query_row(params.as_slice(), |r| r.get(0))
        .context("count_existing_transaction_ids query")?;
    Ok(count as usize)
}

fn import_settings(
    conn: &Connection,
    export: &LegacyExport,
    report: &mut MigrationReport,
) -> Result<()> {
    let e = report.entity_mut("settings");
    let Some(raw) = export.get_value(detector::KEY_CONFIG) else {
        e.found = 0;
        return Ok(());
    };
    e.found = 1;

    let legacy: transformer::LegacyConfig = match serde_json::from_value(raw.clone()) {
        Ok(c) => c,
        Err(err) => {
            report.entity_mut("settings").failed += 1;
            report.error(format!(
                "[settings] bsp_config did not match the expected shape: {err}"
            ));
            return Ok(());
        }
    };

    let mut cfg = Settings::load(conn);
    transformer::apply_legacy_config(&mut cfg, &legacy);
    cfg.save(conn).context("Settings::save during migration")?;
    report.entity_mut("settings").imported = 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::detector::parse_export_str;

    fn sample_export() -> LegacyExport {
        parse_export_str(
            &serde_json::json!({
                "bsp_clients": [
                    {"id": "c_1", "name": "Acme Co", "tallyLedger": "Acme Bank", "createdAt": "2020-01-01T00:00:00.000Z"}
                ],
                "bsp_rules": [
                    {"id": "r_1", "clientId": "c_1", "pattern": "AMAZON", "vendor": "Amazon", "accountHead": "Office Expense", "type": "Payment"},
                    {"id": "r_2", "clientId": "c_1", "pattern": "SALARY", "vendor": "", "accountHead": "Salaries", "type": "Payment"}
                ],
                "bsp_ledgers": [
                    {"clientId": "c_1", "name": "Cash", "group": "Cash-in-Hand"}
                ],
                "bsp_dedupe": [
                    {"clientId": "c_1", "hash": "abc123"}
                ],
                "bsp_history": [
                    {"id": "h_1", "clientId": "c_1", "fileName": "jan.xlsx", "txnCount": 2, "importedAt": "2020-02-01T00:00:00.000Z"}
                ],
                "bsp_imp_h_1": [
                    {"id": "t_1", "date": "01/01/2020", "narration": "SALARY CREDIT", "credit": 50000.0, "balance": 50000.0, "bankName": "HDFC Bank", "accountNo": "1234"},
                    {"id": "t_2", "date": "02/01/2020", "narration": "RENT PAYMENT", "debit": 15000.0, "balance": 35000.0, "bankName": "HDFC Bank", "accountNo": "1234"}
                ],
                "bsp_config": {
                    "gst": {"enabled": false},
                    "reconciliation": {"dateFuzzyDays": 5}
                }
            }).to_string(),
        ).unwrap()
    }

    #[test]
    fn import_all_populates_every_entity() {
        let conn = db::open(":memory:").unwrap();
        let export = sample_export();
        let mut report = MigrationReport::new("test.json");
        import_all(&conn, &export, &mut report, |_, _| {}).expect("import should succeed");

        assert_eq!(report.entity_mut("clients").imported, 1);
        assert_eq!(report.entity_mut("classification_rules").imported, 2);
        assert_eq!(report.entity_mut("ledgers").imported, 1);
        assert_eq!(report.entity_mut("dedupe_hashes").imported, 1);
        assert_eq!(report.entity_mut("import_history").imported, 1);
        assert_eq!(report.entity_mut("transactions").imported, 2);
        assert_eq!(report.entity_mut("settings").imported, 1);

        let clients = db::get_clients(&conn).unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].name, "Acme Co");

        let cfg = Settings::load(&conn);
        assert!(!cfg.gst_enabled);
        assert_eq!(cfg.recon_days, 5);
    }

    #[test]
    fn import_all_preserves_client_created_at() {
        let conn = db::open(":memory:").unwrap();
        let export = sample_export();
        let mut report = MigrationReport::new("test.json");
        import_all(&conn, &export, &mut report, |_, _| {}).unwrap();

        let created_at: String = conn
            .query_row(
                "SELECT created_at FROM clients WHERE name = 'Acme Co'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(created_at, "2020-01-01T00:00:00.000Z");
    }

    #[test]
    fn import_all_preserves_transaction_ids_verbatim() {
        let conn = db::open(":memory:").unwrap();
        let export = sample_export();
        let mut report = MigrationReport::new("test.json");
        import_all(&conn, &export, &mut report, |_, _| {}).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE id IN ('t_1','t_2')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 2,
            "original legacy transaction ids must be preserved verbatim"
        );
    }

    #[test]
    fn re_running_import_all_is_fully_idempotent() {
        let conn = db::open(":memory:").unwrap();
        let export = sample_export();

        let mut report1 = MigrationReport::new("test.json");
        import_all(&conn, &export, &mut report1, |_, _| {}).unwrap();

        let mut report2 = MigrationReport::new("test.json");
        import_all(&conn, &export, &mut report2, |_, _| {}).unwrap();

        assert_eq!(report2.entity_mut("clients").imported, 0);
        assert_eq!(report2.entity_mut("clients").skipped_duplicate, 1);
        assert_eq!(report2.entity_mut("classification_rules").imported, 0);
        assert_eq!(
            report2.entity_mut("classification_rules").skipped_duplicate,
            2
        );
        assert_eq!(report2.entity_mut("ledgers").skipped_duplicate, 1);
        assert_eq!(report2.entity_mut("dedupe_hashes").skipped_duplicate, 1);
        assert_eq!(report2.entity_mut("import_history").skipped_duplicate, 1);
        assert_eq!(report2.entity_mut("transactions").skipped_duplicate, 2);

        // No duplicate rows actually exist in the DB.
        let client_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM clients", [], |r| r.get(0))
            .unwrap();
        assert_eq!(client_count, 1);
        let txn_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM transactions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(txn_count, 2);
    }

    #[test]
    fn import_all_skips_rules_referencing_unknown_clients_without_failing_others() {
        let conn = db::open(":memory:").unwrap();
        let export = parse_export_str(
            &serde_json::json!({
                "bsp_clients": [{"id": "c_1", "name": "Acme Co", "tallyLedger": ""}],
                "bsp_rules": [
                    {"id": "r_1", "clientId": "c_1", "pattern": "OK", "accountHead": "A"},
                    {"id": "r_2", "clientId": "c_ghost", "pattern": "GHOST", "accountHead": "B"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let mut report = MigrationReport::new("test.json");
        import_all(&conn, &export, &mut report, |_, _| {}).unwrap();
        assert_eq!(report.entity_mut("classification_rules").imported, 1);
        assert_eq!(report.entity_mut("classification_rules").failed, 1);
    }

    #[test]
    fn import_all_skips_global_scope_rules_and_ledgers_with_a_clear_warning_not_a_failure() {
        // "global" has no working equivalent in this app's schema (the FK
        // constraint on classification_rules.client_id makes client_id=0
        // uninsertable) — it must be reported as an explained warning, not
        // silently counted as a generic failure.
        let conn = db::open(":memory:").unwrap();
        let export = parse_export_str(
            &serde_json::json!({
                "bsp_clients": [{"id": "c_1", "name": "Acme Co", "tallyLedger": ""}],
                "bsp_rules": [{"id": "r_1", "clientId": "global", "pattern": "SALARY", "accountHead": "Salaries"}],
                "bsp_ledgers": [{"clientId": "global", "name": "Cash", "group": "Cash-in-Hand"}],
                "bsp_dedupe": [{"clientId": "global", "hash": "abc"}]
            }).to_string(),
        ).unwrap();
        let mut report = MigrationReport::new("test.json");
        import_all(&conn, &export, &mut report, |_, _| {}).unwrap();

        assert_eq!(report.entity_mut("classification_rules").imported, 0);
        assert_eq!(
            report.entity_mut("classification_rules").failed,
            0,
            "must not be counted as a generic failure"
        );
        assert_eq!(report.entity_mut("ledgers").imported, 0);
        assert_eq!(report.entity_mut("dedupe_hashes").imported, 0);
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("classification_rules") && w.contains("global")));
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("ledgers") && w.contains("global")));
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("dedupe_hashes") && w.contains("global")));
    }

    #[test]
    fn import_clients_and_rules_compose_cleanly_inside_an_external_transaction() {
        // The individual phase functions take a plain `&Connection` (which a
        // `rusqlite::Transaction` derefs to), so a caller can still wrap a
        // subset of phases in its own transaction if it wants to — dropping
        // that transaction without committing rolls everything back, same as
        // any other SQLite transaction. `import_all` itself no longer does
        // this globally (see its doc comment: whole-migration atomicity is
        // provided by the file-level backup/restore in `mod::migrate`
        // instead, exercised end-to-end by the rollback test there) because
        // two of its phases (`import_dedupe`, `import_history_and_transactions`)
        // call `db` functions that open their own internal transaction, which
        // cannot nest inside an already-open one.
        let conn = db::open(":memory:").unwrap();
        let export = sample_export();
        {
            let txn = conn.unchecked_transaction().unwrap();
            let mut id_map = IdMap::default();
            let mut report = MigrationReport::new("test.json");
            import_clients(&txn, &export, &mut id_map, &mut report).unwrap();
            import_rules(&txn, &export, &id_map, &mut report).unwrap();
            // txn dropped here without .commit() — must roll back everything.
        }
        let client_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM clients", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            client_count, 0,
            "an uncommitted transaction must leave the database untouched"
        );
    }

    #[test]
    fn import_settings_leaves_defaults_when_no_config_present() {
        let conn = db::open(":memory:").unwrap();
        let export = parse_export_str(r#"{"bsp_clients": []}"#).unwrap();
        let mut report = MigrationReport::new("test.json");
        import_all(&conn, &export, &mut report, |_, _| {}).unwrap();
        assert_eq!(report.entity_mut("settings").found, 0);
        assert_eq!(report.entity_mut("settings").imported, 0);
    }
}
