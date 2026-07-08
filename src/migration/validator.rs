//! validator.rs — Pre-migration corruption/sanity checks on the parsed
//! export, and post-migration checks that what was reported as imported
//! actually landed in the database.

use rusqlite::Connection;
use serde_json::Value;

use super::detector::LegacyExport;
use super::report::MigrationReport;
use super::transformer::{LegacyClient, LegacyDedupe, LegacyImportMeta, LegacyLedger, LegacyRule};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    /// Non-blocking — recorded in the report, migration continues.
    Warning,
    /// This specific entity's data could not be safely used at all (e.g. it
    /// doesn't parse as the expected shape) — that entity is skipped, but
    /// other entities still proceed.
    Fatal,
}

#[derive(Debug, Clone)]
pub struct ValidationIssue {
    pub severity: Severity,
    pub entity: String,
    pub message: String,
}

impl ValidationIssue {
    fn warn(entity: &str, message: impl Into<String>) -> Self {
        ValidationIssue {
            severity: Severity::Warning,
            entity: entity.to_string(),
            message: message.into(),
        }
    }
    fn fatal(entity: &str, message: impl Into<String>) -> Self {
        ValidationIssue {
            severity: Severity::Fatal,
            entity: entity.to_string(),
            message: message.into(),
        }
    }
}

/// Attempt to deserialize `raw` as `Vec<T>`, producing a single `Fatal` issue
/// (not one per element — a shape mismatch is almost always all-or-nothing
/// for an entire array) if it doesn't parse.
fn try_parse_array<T: serde::de::DeserializeOwned>(
    raw: &[Value],
    entity: &str,
    issues: &mut Vec<ValidationIssue>,
) -> Vec<T> {
    let mut out = Vec::with_capacity(raw.len());
    let mut bad = 0usize;
    for v in raw {
        match serde_json::from_value::<T>(v.clone()) {
            Ok(item) => out.push(item),
            Err(_) => bad += 1,
        }
    }
    if bad > 0 {
        issues.push(ValidationIssue::warn(
            entity,
            format!(
                "{bad} of {} record(s) did not match the expected shape and were skipped",
                raw.len()
            ),
        ));
    }
    out
}

/// Run every pre-migration sanity/corruption check against the parsed
/// export. Returns an empty list if everything looks internally consistent.
pub fn validate_source(export: &LegacyExport) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();

    let clients: Vec<LegacyClient> = try_parse_array(
        &export.get_array(super::detector::KEY_CLIENTS),
        "clients",
        &mut issues,
    );

    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_names = std::collections::HashSet::new();
    for c in &clients {
        if !seen_ids.insert(c.id.clone()) {
            issues.push(ValidationIssue::warn(
                "clients",
                format!("duplicate client id '{}' in export", c.id),
            ));
        }
        if !seen_names.insert(c.name.to_lowercase()) {
            issues.push(ValidationIssue::warn(
                "clients",
                format!("duplicate client name '{}' in export", c.name),
            ));
        }
    }

    let known_client_ids: std::collections::HashSet<&str> =
        clients.iter().map(|c| c.id.as_str()).collect();
    let client_ref_ok = |id: &str| id == "global" || known_client_ids.contains(id);

    let rules: Vec<LegacyRule> = try_parse_array(
        &export.get_array(super::detector::KEY_RULES),
        "classification_rules",
        &mut issues,
    );
    let orphaned_rules = rules
        .iter()
        .filter(|r| !client_ref_ok(&r.client_id))
        .count();
    if orphaned_rules > 0 {
        issues.push(ValidationIssue::warn(
            "classification_rules",
            format!("{orphaned_rules} rule(s) reference a client id not present in this export and will be skipped"),
        ));
    }

    let ledgers: Vec<LegacyLedger> = try_parse_array(
        &export.get_array(super::detector::KEY_LEDGERS),
        "ledgers",
        &mut issues,
    );
    let orphaned_ledgers = ledgers
        .iter()
        .filter(|l| !client_ref_ok(&l.client_id))
        .count();
    if orphaned_ledgers > 0 {
        issues.push(ValidationIssue::warn(
            "ledgers",
            format!("{orphaned_ledgers} ledger(s) reference a client id not present in this export and will be skipped"),
        ));
    }

    let dedupe: Vec<LegacyDedupe> = try_parse_array(
        &export.get_array(super::detector::KEY_DEDUPE),
        "dedupe_hashes",
        &mut issues,
    );
    let orphaned_dedupe = dedupe
        .iter()
        .filter(|d| !client_ref_ok(&d.client_id))
        .count();
    if orphaned_dedupe > 0 {
        issues.push(ValidationIssue::warn(
            "dedupe_hashes",
            format!("{orphaned_dedupe} dedupe hash(es) reference a client id not present in this export and will be skipped"),
        ));
    }

    let history: Vec<LegacyImportMeta> = try_parse_array(
        &export.get_array(super::detector::KEY_HISTORY),
        "import_history",
        &mut issues,
    );
    let orphaned_history = history
        .iter()
        .filter(|h| !client_ref_ok(&h.client_id))
        .count();
    if orphaned_history > 0 {
        issues.push(ValidationIssue::warn(
            "import_history",
            format!("{orphaned_history} import record(s) reference a client id not present in this export and will be skipped"),
        ));
    }

    // Cross-check declared txnCount against the actual bsp_imp_<id> array
    // length, and scan transactions for the two most consequential defects:
    // a missing date (breaks sorting/filtering) and neither debit nor credit
    // present on a non-opening-balance row (not a real transaction).
    for h in &history {
        let txns = export.get_import_transactions(&h.id);
        if h.txn_count >= 0 && txns.len() != h.txn_count as usize {
            issues.push(ValidationIssue::warn(
                "transactions",
                format!(
                    "import '{}' ({}) declares {} transaction(s) but {} were found — using the actual count",
                    h.id, h.file_name, h.txn_count, txns.len()
                ),
            ));
        }
        let no_date = txns
            .iter()
            .filter(|t| {
                t.get("date")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .is_empty()
            })
            .count();
        if no_date > 0 {
            issues.push(ValidationIssue::warn(
                "transactions",
                format!("import '{}': {no_date} transaction(s) have no date", h.id),
            ));
        }
        let no_amount = txns
            .iter()
            .filter(|t| {
                let is_ob = t
                    .get("isOpeningBalance")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || t.get("systemGenerated")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                !is_ob
                    && t.get("debit").map(Value::is_null).unwrap_or(true)
                    && t.get("credit").map(Value::is_null).unwrap_or(true)
            })
            .count();
        if no_amount > 0 {
            issues.push(ValidationIssue::warn(
                "transactions",
                format!("import '{}': {no_amount} transaction(s) have neither a debit nor a credit amount", h.id),
            ));
        }
    }

    if let Some(cfg) = export.get_value(super::detector::KEY_CONFIG) {
        if !cfg.is_object() {
            issues.push(ValidationIssue::fatal(
                "settings",
                "bsp_config is present but is not a JSON object — skipping settings import",
            ));
        }
    }

    issues
}

/// Re-count rows in the live database after a migration and confirm at least
/// as many exist as the importer reported writing — a defense-in-depth check
/// against a write silently not persisting despite being reported as
/// successful (the import itself runs inside one atomic transaction, so this
/// should never actually fire under normal operation, but it's cheap
/// insurance against exactly the kind of "looks done but isn't" bug this
/// codebase has hit before in other areas).
pub fn validate_migrated(conn: &Connection, report: &MigrationReport) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();

    let table_for = |entity: &str| -> Option<&'static str> {
        match entity {
            "clients" => Some("clients"),
            "classification_rules" => Some("classification_rules"),
            "ledgers" => Some("ledgers"),
            "dedupe_hashes" => Some("dedupe_hashes"),
            "import_history" => Some("import_history"),
            "transactions" => Some("transactions"),
            _ => None,
        }
    };

    for e in &report.entities {
        let Some(table) = table_for(&e.name) else {
            continue;
        };
        if e.imported == 0 {
            continue;
        }
        let count: i64 =
            match conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)) {
                Ok(c) => c,
                Err(err) => {
                    issues.push(ValidationIssue::fatal(
                        &e.name,
                        format!("could not verify row count in '{table}': {err}"),
                    ));
                    continue;
                }
            };
        if (count as usize) < e.imported {
            issues.push(ValidationIssue::fatal(
                &e.name,
                format!(
                    "reported {} row(s) imported into '{table}' but only {count} exist post-commit",
                    e.imported
                ),
            ));
        }
    }

    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::detector::parse_export_str;
    use crate::migration::report::MigrationReport;

    #[test]
    fn validate_source_flags_duplicate_client_ids_and_names() {
        let export = parse_export_str(
            &serde_json::json!({
                "bsp_clients": [
                    {"id": "c_1", "name": "Acme"},
                    {"id": "c_1", "name": "Acme Duplicate Id"},
                    {"id": "c_2", "name": "acme"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let issues = validate_source(&export);
        assert!(issues
            .iter()
            .any(|i| i.message.contains("duplicate client id")));
        assert!(issues
            .iter()
            .any(|i| i.message.contains("duplicate client name")));
    }

    #[test]
    fn validate_source_flags_orphaned_rule_client_reference() {
        let export = parse_export_str(
            &serde_json::json!({
                "bsp_clients": [{"id": "c_1", "name": "Acme"}],
                "bsp_rules": [
                    {"id": "r_1", "clientId": "c_1", "pattern": "X"},
                    {"id": "r_2", "clientId": "c_missing", "pattern": "Y"},
                    {"id": "r_3", "clientId": "global", "pattern": "Z"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let issues = validate_source(&export);
        let msg = issues
            .iter()
            .find(|i| i.entity == "classification_rules")
            .expect("expected an orphaned-rule warning");
        assert!(msg.message.contains("1 rule"));
    }

    #[test]
    fn validate_source_flags_transactions_missing_date_or_amount() {
        let export = parse_export_str(
            &serde_json::json!({
                "bsp_history": [{"id": "h_1", "clientId": "c_1", "fileName": "f.xlsx", "txnCount": 2}],
                "bsp_imp_h_1": [
                    {"id": "t_1", "date": "", "narration": "no date", "debit": 10.0},
                    {"id": "t_2", "date": "01/01/2026", "narration": "no amount"}
                ]
            }).to_string(),
        ).unwrap();
        let issues = validate_source(&export);
        assert!(issues.iter().any(|i| i.message.contains("no date")));
        assert!(issues
            .iter()
            .any(|i| i.message.contains("neither a debit nor a credit")));
    }

    #[test]
    fn validate_source_does_not_flag_opening_balance_row_for_missing_amount() {
        let export = parse_export_str(
            &serde_json::json!({
                "bsp_history": [{"id": "h_1", "clientId": "c_1", "fileName": "f.xlsx", "txnCount": 1}],
                "bsp_imp_h_1": [
                    {"id": "sys-ob-1", "date": "01/01/2026", "narration": "Opening Balance", "balance": 1000.0, "isOpeningBalance": true}
                ]
            }).to_string(),
        ).unwrap();
        let issues = validate_source(&export);
        assert!(!issues
            .iter()
            .any(|i| i.message.contains("neither a debit nor a credit")));
    }

    #[test]
    fn validate_source_flags_txn_count_mismatch() {
        let export = parse_export_str(
            &serde_json::json!({
                "bsp_history": [{"id": "h_1", "clientId": "c_1", "fileName": "f.xlsx", "txnCount": 5}],
                "bsp_imp_h_1": [{"id": "t_1", "date": "01/01/2026", "narration": "x", "debit": 1.0}]
            }).to_string(),
        ).unwrap();
        let issues = validate_source(&export);
        assert!(issues
            .iter()
            .any(|i| i.message.contains("declares 5 transaction")));
    }

    #[test]
    fn validate_source_returns_no_issues_for_clean_data() {
        let export = parse_export_str(
            &serde_json::json!({
                "bsp_clients": [{"id": "c_1", "name": "Acme"}],
                "bsp_rules": [{"id": "r_1", "clientId": "c_1", "pattern": "X"}]
            })
            .to_string(),
        )
        .unwrap();
        assert!(validate_source(&export).is_empty());
    }

    #[test]
    fn validate_source_marks_non_object_config_as_fatal() {
        let export =
            parse_export_str(&serde_json::json!({"bsp_config": [1, 2, 3]}).to_string()).unwrap();
        let issues = validate_source(&export);
        let issue = issues
            .iter()
            .find(|i| i.entity == "settings")
            .expect("expected a settings issue");
        assert_eq!(issue.severity, Severity::Fatal);
    }

    #[test]
    fn validate_migrated_passes_when_counts_match_reported_imports() {
        let conn = crate::db::open(":memory:").unwrap();
        crate::db::add_client(&conn, "Acme", "Acme Bank").unwrap();
        let mut report = MigrationReport::new("test.json");
        report.entity_mut("clients").imported = 1;
        assert!(validate_migrated(&conn, &report).is_empty());
    }

    #[test]
    fn validate_migrated_flags_a_shortfall() {
        let conn = crate::db::open(":memory:").unwrap();
        let mut report = MigrationReport::new("test.json");
        report.entity_mut("clients").imported = 5; // nothing was actually inserted
        let issues = validate_migrated(&conn, &report);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].severity, Severity::Fatal);
    }
}
