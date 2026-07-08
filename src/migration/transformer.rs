//! transformer.rs — Legacy (old-app, camelCase JS) record shapes and their
//! conversion into this app's native Rust types.
//!
//! Field names/shapes here are verified directly against the old app's
//! source (`db.js`, `app.js`, `src/config/config.js`), not guessed. Every
//! `#[serde(rename)]` below corresponds to an actual property name written
//! by the old app's own `JSON.stringify()` calls.

use std::collections::HashMap;

use serde::Deserialize;

use crate::parser::date_parser::normalize_transaction_date;
use crate::parser::{Transaction, TransactionStatus, VoucherType};
use crate::settings::Settings;

/// Old-app clientId → this app's new integer client id.
///
/// The old app's `"global"` client id (used by rules/ledgers for
/// cross-client entries) has **no working equivalent** on this side: while
/// `db::get_rules`'s read query does accept `client_id = 0` as a "global"
/// marker, nothing can ever actually create such a row —
/// `classification_rules.client_id` has a `NOT NULL REFERENCES clients(id)`
/// foreign key, and no client with id `0` is ever created (`clients.id` is
/// `AUTOINCREMENT` starting at 1), so an insert with `client_id = 0` always
/// fails the FK constraint. `ledgers` doesn't even have a read-side "global"
/// concept at all. `resolve_client("global")` therefore intentionally
/// returns `None` — the importer treats this as an unsupported (not
/// orphaned/corrupt) reference and reports it as a distinct, explained
/// warning rather than a generic failure. See `PROJECT_AUDIT`/Phase 2 notes
/// for the follow-up options (skip vs. duplicate-per-client) this leaves open.
#[derive(Debug, Default)]
pub struct IdMap {
    pub clients: HashMap<String, i64>,
}

impl IdMap {
    pub fn resolve_client(&self, legacy_client_id: &str) -> Option<i64> {
        if legacy_client_id == "global" {
            return None;
        }
        self.clients.get(legacy_client_id).copied()
    }

    /// `true` when a client reference failed to resolve specifically because
    /// it was the old app's unsupported "global" scope (see the struct docs)
    /// rather than a genuinely unknown/corrupt client id.
    pub fn is_unsupported_global(legacy_client_id: &str) -> bool {
        legacy_client_id == "global"
    }
}

// ── Legacy record shapes ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyClient {
    pub id: String,
    pub name: String,
    #[serde(default, rename = "tallyLedger")]
    pub tally_ledger: String,
    #[serde(default, rename = "createdAt")]
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyRule {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "clientId")]
    pub client_id: String,
    pub pattern: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default, rename = "accountHead")]
    pub account_head: String,
    #[serde(default, rename = "type")]
    pub txn_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyLedger {
    #[serde(rename = "clientId")]
    pub client_id: String,
    pub name: String,
    #[serde(default)]
    pub group: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyDedupe {
    #[serde(rename = "clientId")]
    pub client_id: String,
    pub hash: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LegacyImportMeta {
    pub id: String,
    #[serde(rename = "clientId")]
    pub client_id: String,
    #[serde(rename = "fileName")]
    pub file_name: String,
    #[serde(default, rename = "txnCount")]
    pub txn_count: i64,
    #[serde(default, rename = "importedAt")]
    pub imported_at: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LegacyTransaction {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub date: String,
    #[serde(default, rename = "dateTs")]
    pub date_ts: Option<i64>,
    #[serde(default)]
    pub narration: String,
    #[serde(default)]
    pub reference: String,
    #[serde(default)]
    pub debit: Option<f64>,
    #[serde(default)]
    pub credit: Option<f64>,
    #[serde(default)]
    pub balance: Option<f64>,
    #[serde(default)]
    pub vendor: String,
    #[serde(default, rename = "accountHead")]
    pub account_head: String,
    #[serde(default, rename = "type")]
    pub txn_type: String,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub status: String,
    #[serde(default, rename = "classifiedBy")]
    pub classified_by: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, rename = "bankName")]
    pub bank_name: String,
    #[serde(default, rename = "accountNo")]
    pub account_no: String,
    #[serde(default, rename = "isOpeningBalance")]
    pub is_opening_balance: bool,
    #[serde(default, rename = "systemGenerated")]
    pub system_generated: bool,
    #[serde(default, rename = "dupFlag")]
    pub dup_flag: bool,
    #[serde(default, rename = "gstRate")]
    pub gst_rate: Option<f64>,
    #[serde(default, rename = "gstAmount")]
    pub gst_amount: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LegacyConfig {
    #[serde(default)]
    pub ai: LegacyAiConfig,
    #[serde(default, rename = "narrationCleaner")]
    pub narration_cleaner: LegacyNarrationConfig,
    #[serde(default)]
    pub gst: LegacyGstConfig,
    #[serde(default)]
    pub reconciliation: LegacyReconConfig,
    #[serde(default)]
    pub logging: LegacyLoggingConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LegacyAiConfig {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default, rename = "openaiKey")]
    pub openai_key: Option<String>,
    #[serde(default, rename = "claudeKey")]
    pub claude_key: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LegacyNarrationConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default, rename = "titleCase")]
    pub title_case: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LegacyGstConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default, rename = "autoSuggestLedgers")]
    pub auto_suggest_ledgers: Option<bool>,
    #[serde(default, rename = "defaultState")]
    pub default_state: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LegacyReconConfig {
    #[serde(default, rename = "dateFuzzyDays")]
    pub date_fuzzy_days: Option<i32>,
    #[serde(default, rename = "amountFuzzyPct")]
    pub amount_fuzzy_pct: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LegacyLoggingConfig {
    #[serde(default)]
    pub level: Option<String>,
}

// ── Conversions ───────────────────────────────────────────────────────────────

/// The 9-entry state list used identically by the Settings screen's "Default
/// State" combo and the Export Wizard's state selector (`STATE_OPTS` in
/// `main.rs`) — kept in sync manually since it's UI-owned, not DB-owned.
const STATE_CODES: [&str; 9] = ["MH", "GJ", "DL", "KA", "TN", "TG", "RJ", "UP", "WB"];

fn state_code_to_idx(code: &str) -> i32 {
    STATE_CODES
        .iter()
        .position(|c| c.eq_ignore_ascii_case(code))
        .map(|i| i as i32)
        .unwrap_or(0)
}

fn map_status(s: &str) -> TransactionStatus {
    match s {
        "unreviewed" => TransactionStatus::Unreviewed,
        "classified" => TransactionStatus::Classified,
        "manual" => TransactionStatus::Manual,
        "suspense" => TransactionStatus::Suspense,
        // "system" (the old app's synthetic opening-balance row marker) has no
        // direct Rust equivalent — `is_opening_balance` carries that meaning
        // instead, so the status itself falls back to Unreviewed.
        _ => TransactionStatus::Unreviewed,
    }
}

fn map_voucher_type(s: &str) -> VoucherType {
    match s {
        "Payment" => VoucherType::Payment,
        "Receipt" => VoucherType::Receipt,
        "Contra" => VoucherType::Contra,
        "Journal" => VoucherType::Journal,
        "Sales" => VoucherType::Sales,
        "Purchase" => VoucherType::Purchase,
        _ => VoucherType::Unknown,
    }
}

/// Convert one legacy transaction into this app's native `Transaction`.
/// `bank_name_fallback`/`account_no_fallback` cover legacy rows that predate
/// per-transaction bank/account tagging (older exports may have these blank
/// on the transaction itself, carried instead at the import-batch level).
pub fn transaction_from_legacy(
    t: &LegacyTransaction,
    bank_name_fallback: &str,
    account_no_fallback: &str,
) -> Transaction {
    // Preserve the original id where one exists (task requirement: preserve
    // IDs where possible) — only synthesize one for the rare malformed record
    // that arrives with no id at all.
    let id = t.id.clone().unwrap_or_else(|| {
        format!(
            "legacy_{}",
            fast_fallback_id(&t.date, &t.narration, t.debit, t.credit)
        )
    });

    let parsed_date = normalize_transaction_date(&t.date);
    // Preserve the original timestamp when the export carried one; otherwise
    // derive it the same way the rest of the app already does for any date
    // string that didn't come with a pre-computed timestamp.
    let date_ts = t.date_ts.unwrap_or(parsed_date.ts);
    let display_date = if parsed_date.valid {
        parsed_date.display
    } else {
        t.date.clone()
    };

    let is_opening_balance = t.is_opening_balance || t.system_generated;

    let mut txn = Transaction::new(id);
    txn.date = display_date;
    txn.date_ts = date_ts;
    txn.narration = t.narration.clone();
    txn.reference = t.reference.clone();
    txn.debit = t.debit;
    txn.credit = t.credit;
    txn.balance = t.balance;
    txn.vendor = t.vendor.clone();
    txn.account_head = t.account_head.clone();
    txn.txn_type = map_voucher_type(&t.txn_type);
    txn.confidence = t.confidence;
    txn.status = map_status(&t.status);
    txn.classification_source = match t.classified_by.as_str() {
        "ai" => "ai".to_string(),
        "user" => "user".to_string(),
        "" => String::new(),
        other => other.to_string(),
    };
    txn.tags = t.tags.clone();
    txn.bank_name = if t.bank_name.is_empty() {
        bank_name_fallback.to_string()
    } else {
        t.bank_name.clone()
    };
    txn.account_no = if t.account_no.is_empty() {
        account_no_fallback.to_string()
    } else {
        t.account_no.clone()
    };
    txn.is_opening_balance = is_opening_balance;
    txn.dup_flag = t.dup_flag;
    txn.gst_rate = t.gst_rate;
    txn.gst_amount = t.gst_amount;
    txn
}

/// Deterministic fallback id for the rare legacy record with no `id` field at
/// all — stable across repeated migration runs of the same source file (so
/// re-running migration doesn't create fresh duplicate rows for these), but
/// intentionally not collision-proof across wildly different inputs; this is
/// a last-resort path, not the common case.
fn fast_fallback_id(
    date: &str,
    narration: &str,
    debit: Option<f64>,
    credit: Option<f64>,
) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    date.hash(&mut h);
    narration.hash(&mut h);
    debit.map(|v| v.to_bits()).hash(&mut h);
    credit.map(|v| v.to_bits()).hash(&mut h);
    format!("{:x}", h.finish())
}

/// Apply legacy config values onto an existing `Settings` instance, only
/// where the legacy field maps unambiguously onto this app's own semantics —
/// see the `narr_preserve` note below for the one deliberate exception.
pub fn apply_legacy_config(cfg: &mut Settings, legacy: &LegacyConfig) {
    if let Some(v) = legacy.ai.enabled {
        cfg.ai_enabled = v;
    }
    if let Some(provider) = &legacy.ai.provider {
        match provider.as_str() {
            "openai" | "claude" => cfg.ai_provider = provider.clone(),
            // The old app's third option is "local" (no cloud provider at
            // all), which this app doesn't model as a distinct provider —
            // leave whatever provider is already configured rather than
            // guessing, since `ai_enabled` above already reflects intent.
            _ => {}
        }
    }
    // Keys are provider-specific in the old app but this app stores a single
    // active key (in the OS keyring, via `Settings::save`) for whichever
    // provider is selected — migrate whichever one matches the resolved
    // provider so the key that's actually usable is the one carried over.
    match cfg.ai_provider.as_str() {
        "openai" => {
            if let Some(k) = &legacy.ai.openai_key {
                if !k.is_empty() {
                    cfg.ai_api_key = k.clone();
                }
            }
        }
        "claude" => {
            if let Some(k) = &legacy.ai.claude_key {
                if !k.is_empty() {
                    cfg.ai_api_key = k.clone();
                }
            }
        }
        _ => {}
    }

    if let Some(v) = legacy.narration_cleaner.enabled {
        cfg.narr_enabled = v;
    }
    if let Some(v) = legacy.narration_cleaner.title_case {
        cfg.narr_title_case = v;
    }
    // Deliberately NOT migrating `narrationCleaner.preserveOriginal` into
    // `narr_preserve`: the two flags answer different questions despite the
    // similar name. The old app's `preserveOriginal` controls whether a
    // hidden `rawNarration` backup is kept before permanently overwriting
    // `t.narration` with the cleaned text (which the old app always does when
    // confident). This app never overwrites `narration` at all — its
    // `narr_preserve` instead controls whether the *displayed* table column
    // shows the cleaned suggestion or the raw text. Carrying over the old
    // flag's value would silently change this app's default display
    // behavior in a way that doesn't correspond to what the old app actually
    // showed its user, so the existing (or default) `narr_preserve` is left
    // untouched by migration.

    if let Some(v) = legacy.gst.enabled {
        cfg.gst_enabled = v;
    }
    if let Some(v) = legacy.gst.auto_suggest_ledgers {
        cfg.gst_auto_ledgers = v;
    }
    if let Some(state) = &legacy.gst.default_state {
        cfg.default_state_idx = state_code_to_idx(state);
    }

    if let Some(days) = legacy.reconciliation.date_fuzzy_days {
        cfg.recon_days = days;
    }
    if let Some(pct) = legacy.reconciliation.amount_fuzzy_pct {
        cfg.recon_pct = pct;
    }

    if let Some(level) = &legacy.logging.level {
        let upper = level.to_uppercase();
        if matches!(upper.as_str(), "DEBUG" | "INFO" | "WARN" | "ERROR") {
            cfg.log_level = upper;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_map_has_no_working_resolution_for_global_scope() {
        // See the struct-level doc comment: "global" has no working
        // equivalent in this app's schema (FK constraint), so this must
        // resolve to None, not a fabricated client_id=0.
        let map = IdMap::default();
        assert_eq!(map.resolve_client("global"), None);
        assert!(IdMap::is_unsupported_global("global"));
        assert!(!IdMap::is_unsupported_global("c_1"));
    }

    #[test]
    fn id_map_resolves_known_client_and_rejects_unknown() {
        let mut map = IdMap::default();
        map.clients.insert("c_1".to_string(), 42);
        assert_eq!(map.resolve_client("c_1"), Some(42));
        assert_eq!(map.resolve_client("c_unknown"), None);
    }

    #[test]
    fn deserializes_legacy_client_camelcase_fields() {
        let json = r#"{"id":"c_1","name":"Acme","tallyLedger":"Acme Bank","createdAt":"2026-01-01T00:00:00.000Z"}"#;
        let c: LegacyClient = serde_json::from_str(json).unwrap();
        assert_eq!(c.id, "c_1");
        assert_eq!(c.tally_ledger, "Acme Bank");
        assert_eq!(c.created_at, "2026-01-01T00:00:00.000Z");
    }

    #[test]
    fn deserializes_legacy_rule_with_global_client_id() {
        let json = r#"{"id":"r_1","clientId":"global","pattern":"AMAZON","vendor":"Amazon","accountHead":"Office Expense","type":"Payment"}"#;
        let r: LegacyRule = serde_json::from_str(json).unwrap();
        assert_eq!(r.client_id, "global");
        assert_eq!(r.account_head, "Office Expense");
    }

    #[test]
    fn transaction_from_legacy_preserves_id_and_maps_status_and_type() {
        let t = LegacyTransaction {
            id: Some("t_1".to_string()),
            date: "15/03/2026".to_string(),
            narration: "SALARY CREDIT".to_string(),
            credit: Some(50000.0),
            status: "classified".to_string(),
            txn_type: "Receipt".to_string(),
            vendor: "Employer".to_string(),
            ..Default::default()
        };
        let txn = transaction_from_legacy(&t, "HDFC Bank", "1234");
        assert_eq!(txn.id, "t_1");
        assert_eq!(txn.date, "15/03/2026");
        assert_eq!(txn.credit, Some(50000.0));
        assert_eq!(txn.status, TransactionStatus::Classified);
        assert_eq!(txn.txn_type, VoucherType::Receipt);
        assert_eq!(
            txn.bank_name, "HDFC Bank",
            "must fall back to the batch-level bank name"
        );
    }

    #[test]
    fn transaction_from_legacy_preserves_explicit_date_ts_when_present() {
        let t = LegacyTransaction {
            id: Some("t_1".to_string()),
            date: "15/03/2026".to_string(),
            date_ts: Some(1234567890000),
            ..Default::default()
        };
        let txn = transaction_from_legacy(&t, "", "");
        assert_eq!(
            txn.date_ts, 1234567890000,
            "an explicit legacy timestamp must be preserved verbatim"
        );
    }

    #[test]
    fn transaction_from_legacy_derives_date_ts_when_absent() {
        let t = LegacyTransaction {
            id: Some("t_1".to_string()),
            date: "15/03/2026".to_string(),
            date_ts: None,
            ..Default::default()
        };
        let txn = transaction_from_legacy(&t, "", "");
        assert_ne!(
            txn.date_ts, 0,
            "must derive a timestamp from the date string when none was supplied"
        );
    }

    #[test]
    fn transaction_from_legacy_maps_opening_balance_row() {
        let t = LegacyTransaction {
            id: Some("sys-ob-1".to_string()),
            narration: "Opening Balance".to_string(),
            balance: Some(85000.0),
            system_generated: true,
            status: "system".to_string(),
            ..Default::default()
        };
        let txn = transaction_from_legacy(&t, "", "");
        assert!(txn.is_opening_balance);
        assert_eq!(
            txn.status,
            TransactionStatus::Unreviewed,
            "unmapped 'system' status falls back safely"
        );
    }

    #[test]
    fn transaction_from_legacy_synthesizes_a_stable_id_when_missing() {
        let t = LegacyTransaction {
            id: None,
            date: "01/01/2026".to_string(),
            narration: "TEST".to_string(),
            debit: Some(10.0),
            ..Default::default()
        };
        let a = transaction_from_legacy(&t, "", "");
        let b = transaction_from_legacy(&t, "", "");
        assert_eq!(
            a.id, b.id,
            "fallback id generation must be deterministic across runs"
        );
        assert!(a.id.starts_with("legacy_"));
    }

    #[test]
    fn apply_legacy_config_maps_gst_and_recon_and_logging() {
        let mut cfg = Settings::default();
        let legacy: LegacyConfig = serde_json::from_str(
            r#"{
                "gst": {"enabled": false, "autoSuggestLedgers": false, "defaultState": "KA"},
                "reconciliation": {"dateFuzzyDays": 7, "amountFuzzyPct": 2.5},
                "logging": {"level": "debug"}
            }"#,
        )
        .unwrap();
        apply_legacy_config(&mut cfg, &legacy);
        assert!(!cfg.gst_enabled);
        assert!(!cfg.gst_auto_ledgers);
        assert_eq!(cfg.default_state_idx, 3, "KA is index 3 in STATE_CODES");
        assert_eq!(cfg.recon_days, 7);
        assert_eq!(cfg.recon_pct, 2.5);
        assert_eq!(cfg.log_level, "DEBUG");
    }

    #[test]
    fn apply_legacy_config_does_not_touch_narr_preserve() {
        let mut cfg = Settings::default();
        let original = cfg.narr_preserve;
        let legacy: LegacyConfig = serde_json::from_str(
            r#"{"narrationCleaner": {"enabled": true, "titleCase": true, "preserveOriginal": true}}"#,
        ).unwrap();
        apply_legacy_config(&mut cfg, &legacy);
        assert_eq!(
            cfg.narr_preserve, original,
            "narr_preserve must be left alone by migration"
        );
    }

    #[test]
    fn apply_legacy_config_leaves_fields_untouched_when_legacy_omits_them() {
        let mut cfg = Settings::default();
        cfg.recon_days = 99;
        let legacy = LegacyConfig::default();
        apply_legacy_config(&mut cfg, &legacy);
        assert_eq!(
            cfg.recon_days, 99,
            "absent legacy fields must not clobber existing settings"
        );
    }

    #[test]
    fn apply_legacy_config_maps_ai_key_to_the_resolved_provider() {
        let mut cfg = Settings::default();
        let legacy: LegacyConfig = serde_json::from_str(
            r#"{"ai": {"provider": "claude", "claudeKey": "sk-claude-xyz", "openaiKey": "sk-openai-abc", "enabled": true}}"#,
        ).unwrap();
        apply_legacy_config(&mut cfg, &legacy);
        assert_eq!(cfg.ai_provider, "claude");
        assert_eq!(
            cfg.ai_api_key, "sk-claude-xyz",
            "must pick the key matching the resolved provider, not the other one"
        );
        assert!(cfg.ai_enabled);
    }

    #[test]
    fn apply_legacy_config_ignores_local_only_provider() {
        let mut cfg = Settings::default();
        let original_provider = cfg.ai_provider.clone();
        let legacy: LegacyConfig =
            serde_json::from_str(r#"{"ai": {"provider": "local"}}"#).unwrap();
        apply_legacy_config(&mut cfg, &legacy);
        assert_eq!(
            cfg.ai_provider, original_provider,
            "'local' has no Rust-side equivalent provider, must not overwrite"
        );
    }
}
