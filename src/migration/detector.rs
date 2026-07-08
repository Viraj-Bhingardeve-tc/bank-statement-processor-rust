//! detector.rs — Recognizes and sniffs a legacy-app data export before any
//! transformation/import is attempted.
//!
//! The old Electron app (`bank-statement-processing`) stores everything in
//! the browser `localStorage` API (verified directly against its `db.js` /
//! `src/config/config.js` source — there is no SQLite file on the old side
//! to open directly). The supported export format is therefore a plain JSON
//! dump of that `localStorage` — produced by running, in the old app's
//! DevTools console (F12 → Console) while it's open:
//!
//! ```js
//! copy(JSON.stringify(Object.fromEntries(Object.keys(localStorage)
//!   .filter(k => k.startsWith('bsp_'))
//!   .map(k => [k, localStorage.getItem(k)]))))
//! ```
//!
//! ...then pasting the clipboard contents into a `.json` file. This requires
//! no code changes to the old app and touches only data the old app already
//! has full, safe, native access to — it deliberately avoids reverse
//! engineering Chromium's on-disk LevelDB encoding for `localStorage`
//! (undocumented, version-dependent, and a real corruption/mis-read risk for
//! a migration tool where silent data loss is the worst possible failure
//! mode). Every value in the dump is itself a JSON-encoded string (matching
//! how `localStorage.getItem` actually returns it), but an already-parsed
//! value is also accepted, which keeps hand-built test fixtures simple.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

/// Well-known top-level `bsp_*` keys recognized from the old app's `db.js`
/// (`DB.K`) and `src/config/config.js`. `bsp_imp_<id>` keys (one per import
/// batch) are handled separately via [`LegacyExport::import_ids`] since the
/// `<id>` suffix is dynamic.
pub const KEY_CLIENTS: &str = "bsp_clients";
pub const KEY_RULES: &str = "bsp_rules";
pub const KEY_LEDGERS: &str = "bsp_ledgers";
pub const KEY_DEDUPE: &str = "bsp_dedupe";
pub const KEY_HISTORY: &str = "bsp_history";
pub const KEY_CONFIG: &str = "bsp_config";
pub const IMPORT_KEY_PREFIX: &str = "bsp_imp_";

/// A parsed `localStorage` dump: every recognized key already decoded from
/// its JSON-string-of-JSON encoding into a real [`serde_json::Value`].
#[derive(Debug, Clone, Default)]
pub struct LegacyExport {
    pub raw: BTreeMap<String, Value>,
}

impl LegacyExport {
    pub fn get_array(&self, key: &str) -> Vec<Value> {
        self.raw
            .get(key)
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
    }

    pub fn get_value(&self, key: &str) -> Option<&Value> {
        self.raw.get(key)
    }

    /// Every `bsp_imp_<id>` key's `<id>` suffix, in the order they appear in
    /// the map (which is key-sorted, i.e. deterministic, since `raw` is a
    /// `BTreeMap`).
    pub fn import_ids(&self) -> Vec<String> {
        self.raw
            .keys()
            .filter_map(|k| k.strip_prefix(IMPORT_KEY_PREFIX))
            .map(|s| s.to_string())
            .collect()
    }

    pub fn get_import_transactions(&self, import_id: &str) -> Vec<Value> {
        self.get_array(&format!("{IMPORT_KEY_PREFIX}{import_id}"))
    }
}

/// Result of sniffing a [`LegacyExport`] — what's present and how much of it,
/// without fully validating or transforming any record yet (see
/// `validator.rs` / `transformer.rs` for that).
#[derive(Debug, Clone, Default)]
pub struct DetectedSource {
    pub has_clients: bool,
    pub has_rules: bool,
    pub has_ledgers: bool,
    pub has_dedupe: bool,
    pub has_history: bool,
    pub has_config: bool,
    /// Entity name -> record count, in a stable display order.
    pub entity_counts: Vec<(String, usize)>,
}

impl DetectedSource {
    pub fn count_of(&self, entity: &str) -> usize {
        self.entity_counts
            .iter()
            .find(|(n, _)| n == entity)
            .map(|(_, c)| *c)
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.entity_counts.iter().all(|(_, c)| *c == 0)
    }
}

/// Parse a migration export file from disk.
pub fn parse_export_file(path: &Path) -> Result<LegacyExport> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read migration export file: {}", path.display()))?;
    parse_export_str(&text)
}

/// Parse a migration export from its raw text (used directly by tests, and
/// by `parse_export_file` for real files).
pub fn parse_export_str(text: &str) -> Result<LegacyExport> {
    let top: Value = serde_json::from_str(text).context(
        "file is not valid JSON — make sure you pasted the full clipboard contents \
         from the DevTools export command with nothing added or removed",
    )?;
    let obj = top.as_object().context(
        "expected a JSON object at the top level (a localStorage key/value dump), \
         got something else — this file doesn't look like a migration export",
    )?;

    let mut raw = BTreeMap::new();
    for (k, v) in obj {
        if !k.starts_with("bsp_") {
            continue; // ignore any non-BSP localStorage keys present in a raw full dump
        }
        // localStorage values are strings containing JSON (`getItem` always
        // returns a string); accept either that real-world shape or an
        // already-parsed value, which keeps hand-built fixtures simple.
        let parsed = match v {
            Value::String(s) => {
                serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone()))
            }
            other => other.clone(),
        };
        raw.insert(k.clone(), parsed);
    }
    Ok(LegacyExport { raw })
}

/// Sniff a parsed export: what's present, how much of it, and whether it's
/// recognizable as a Bank Statement Processor export at all.
pub fn detect(export: &LegacyExport) -> Result<DetectedSource> {
    let bsp_key_count = export.raw.len();
    if bsp_key_count == 0 {
        anyhow::bail!(
            "no recognizable bsp_* keys found in this file — it doesn't look like a \
             Bank Statement Processor export. Re-run the DevTools export command in \
             the old app and make sure the whole clipboard contents were saved."
        );
    }

    let txn_total: usize = export
        .import_ids()
        .iter()
        .map(|id| export.get_import_transactions(id).len())
        .sum();

    Ok(DetectedSource {
        has_clients: export.raw.contains_key(KEY_CLIENTS),
        has_rules: export.raw.contains_key(KEY_RULES),
        has_ledgers: export.raw.contains_key(KEY_LEDGERS),
        has_dedupe: export.raw.contains_key(KEY_DEDUPE),
        has_history: export.raw.contains_key(KEY_HISTORY),
        has_config: export.raw.contains_key(KEY_CONFIG),
        entity_counts: vec![
            ("clients".to_string(), export.get_array(KEY_CLIENTS).len()),
            (
                "classification_rules".to_string(),
                export.get_array(KEY_RULES).len(),
            ),
            ("ledgers".to_string(), export.get_array(KEY_LEDGERS).len()),
            (
                "dedupe_hashes".to_string(),
                export.get_array(KEY_DEDUPE).len(),
            ),
            (
                "import_history".to_string(),
                export.get_array(KEY_HISTORY).len(),
            ),
            ("transactions".to_string(), txn_total),
        ],
    })
}

/// Best-effort, informational-only check for whether the old Electron app's
/// user-data directory still exists on this machine — used purely to power
/// "recovery instructions" copy (e.g. "we found the old app's data folder at
/// X, relaunch it from there to run the export command") when no export file
/// has been provided yet. Never reads or parses anything inside it — actual
/// data must come through [`parse_export_file`].
pub fn detect_old_app_userdata_dir() -> Option<std::path::PathBuf> {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(std::path::PathBuf::from)
    } else if cfg!(target_os = "macos") {
        dirs_home().map(|h| h.join("Library/Application Support"))
    } else {
        dirs_home().map(|h| h.join(".config"))
    }?;

    // Electron derives the userData folder name from the app's `name` field
    // in its package.json — the old app's product name is "Bank Statement
    // Processor" per its window title; Electron slugifies that when no
    // explicit `app.setName()` override is present. Check a few plausible
    // spellings rather than assuming one exact match.
    for candidate in [
        "bank-statement-processing",
        "Bank Statement Processor",
        "bank-statement-processor",
    ] {
        let p = base.join(candidate);
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dump() -> String {
        // Mirrors the real DevTools export shape: every value is itself a
        // JSON-encoded string, exactly like `localStorage.getItem` returns.
        serde_json::json!({
            "bsp_clients": serde_json::to_string(&serde_json::json!([
                {"id": "c_1", "name": "Acme Co", "tallyLedger": "Acme Bank", "createdAt": "2026-01-01T00:00:00.000Z"}
            ])).unwrap(),
            "bsp_rules": serde_json::to_string(&serde_json::json!([
                {"id": "r_1", "clientId": "c_1", "pattern": "AMAZON", "vendor": "Amazon", "accountHead": "Office Expense", "type": "Payment", "hitCount": 3}
            ])).unwrap(),
            "bsp_ledgers": "[]",
            "bsp_dedupe": "[]",
            "bsp_history": serde_json::to_string(&serde_json::json!([
                {"id": "h_1", "clientId": "c_1", "fileName": "jan.xlsx", "txnCount": 2, "importedAt": "2026-01-02T00:00:00.000Z"}
            ])).unwrap(),
            "bsp_imp_h_1": serde_json::to_string(&serde_json::json!([
                {"id": "t_1", "date": "01/01/2026", "narration": "SALARY", "debit": null, "credit": 1000.0, "balance": 1000.0},
                {"id": "t_2", "date": "02/01/2026", "narration": "RENT", "debit": 500.0, "credit": null, "balance": 500.0}
            ])).unwrap(),
            "bsp_config": serde_json::to_string(&serde_json::json!({"ai": {"provider": "openai", "enabled": true}})).unwrap(),
        }).to_string()
    }

    #[test]
    fn parses_string_encoded_localstorage_values() {
        let export = parse_export_str(&sample_dump()).expect("parse");
        assert_eq!(export.get_array("bsp_clients").len(), 1);
        assert_eq!(export.get_array("bsp_rules").len(), 1);
    }

    #[test]
    fn parses_already_parsed_values_too_for_fixture_convenience() {
        let text = serde_json::json!({
            "bsp_clients": [{"id": "c_1", "name": "Direct", "tallyLedger": ""}]
        })
        .to_string();
        let export = parse_export_str(&text).expect("parse");
        assert_eq!(export.get_array("bsp_clients").len(), 1);
    }

    #[test]
    fn ignores_non_bsp_keys_in_a_full_localstorage_dump() {
        let text = serde_json::json!({
            "bsp_clients": "[]",
            "some_other_app_key": "irrelevant",
        })
        .to_string();
        let export = parse_export_str(&text).expect("parse");
        assert!(export.raw.contains_key("bsp_clients"));
        assert!(!export.raw.contains_key("some_other_app_key"));
    }

    #[test]
    fn rejects_invalid_json() {
        let err = parse_export_str("{not json").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn rejects_non_object_top_level() {
        let err = parse_export_str("[1,2,3]").unwrap_err();
        assert!(err.to_string().contains("JSON object"));
    }

    #[test]
    fn detect_rejects_a_json_file_with_no_bsp_keys() {
        let export = parse_export_str(r#"{"unrelated": 1}"#).expect("parse");
        // "unrelated" is filtered out during parsing (not bsp_-prefixed), so
        // the resulting export is empty and detect() must reject it.
        let err = detect(&export).unwrap_err();
        assert!(err.to_string().contains("doesn't look like"));
    }

    #[test]
    fn detect_counts_every_entity_including_transactions_across_import_batches() {
        let export = parse_export_str(&sample_dump()).expect("parse");
        let d = detect(&export).expect("detect");
        assert!(d.has_clients);
        assert!(d.has_rules);
        assert!(d.has_history);
        assert!(d.has_config);
        assert_eq!(d.count_of("clients"), 1);
        assert_eq!(d.count_of("classification_rules"), 1);
        assert_eq!(d.count_of("import_history"), 1);
        assert_eq!(
            d.count_of("transactions"),
            2,
            "must sum across all bsp_imp_* keys"
        );
        assert!(!d.is_empty());
    }

    #[test]
    fn detect_handles_missing_optional_keys_gracefully() {
        let export = parse_export_str(r#"{"bsp_clients": "[]"}"#).expect("parse");
        let d = detect(&export).expect("detect");
        assert!(d.has_clients);
        assert!(!d.has_rules);
        assert!(!d.has_config);
        assert_eq!(d.count_of("classification_rules"), 0);
    }

    #[test]
    fn import_ids_extracts_every_batch_suffix() {
        let text = serde_json::json!({
            "bsp_imp_h_1": "[]",
            "bsp_imp_h_2": "[]",
        })
        .to_string();
        let export = parse_export_str(&text).expect("parse");
        let mut ids = export.import_ids();
        ids.sort();
        assert_eq!(ids, vec!["h_1".to_string(), "h_2".to_string()]);
    }
}
