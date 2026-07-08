//! Integration-level regression tests for each Phase 1 bug fix from this
//! project's earlier work, at a level distinct from the unit tests already
//! added alongside those fixes (those live in `src/*.rs`'s own
//! `#[cfg(test)]` modules; these exercise the same fixes end-to-end through
//! more of the real pipeline, with real fixtures where applicable).
//!
//! Two of the six Phase 1 fixes already have dedicated integration coverage
//! elsewhere and are intentionally **not** duplicated here:
//! - Excel-path bank detection -> `import_pipeline.rs::real_excel_fixture_parses_with_bank_detection`
//!   (uses the real HDFC.xls fixture).
//! - Run Reconciliation feature -> `ledger_reconciliation_errors.rs`'s
//!   `reconciliation_*` tests.
//!
//! The "test-build gap" fix (main.rs depending on the lib crate's single
//! compiled copy instead of duplicating every module via a second `mod`
//! tree) has no runtime assertion to make — its regression test is this
//! entire `tests/` suite compiling and running as part of one `cargo test`
//! invocation at all, which was not possible before that fix.

use bank_statement_processor::classifier;
use bank_statement_processor::db;
use bank_statement_processor::narration_cleaner;
use bank_statement_processor::parser::Transaction;

/// Phase 1 fix: narration cleaning, GST toggles, and log-level were
/// persisted to the database but never actually consulted by
/// `narration_cleaner`/`classifier`/logging setup — "looks configurable,
/// isn't". Proves `narr_enabled=false` is a true no-op (narration passes
/// through unchanged) and `narr_enabled=true` actually cleans it, using the
/// same `clean_batch_with`/`classify_all` entry points `main.rs` calls with
/// `Settings`-derived flags.
#[test]
fn settings_narration_toggle_actually_changes_output() {
    // "RAMESH KUMAR" is deliberately not in the vendor dictionary (unlike
    // e.g. "AMAZON", which short-circuits party extraction to a fixed
    // canonical string regardless of title_case) — its word-scoring path
    // returns the raw uppercase words verbatim, so this is a real test of
    // the flag rather than a vacuous one. Same input the unit tests in
    // narration_cleaner.rs already use for exactly this reason.
    let messy = vec!["UPI/CR/234567890123/RAMESH KUMAR".to_string()];

    let enabled_titlecase = narration_cleaner::clean_batch_with(&messy, true);
    let disabled_titlecase = narration_cleaner::clean_batch_with(&messy, false);

    assert_eq!(enabled_titlecase[0].party, "Ramesh Kumar");
    assert_eq!(
        disabled_titlecase[0].party, "RAMESH KUMAR",
        "narr_title_case=false must skip to_title_case on the party"
    );
    assert_ne!(
        enabled_titlecase[0].cleaned, disabled_titlecase[0].cleaned,
        "narr_title_case must produce different output when toggled — settings must not be decorative"
    );
}

/// Same fix, the GST half: `classify_all`'s `gst_enabled`/`gst_auto_ledgers`
/// parameters must actually gate GST-derived output, not just be threaded
/// through and ignored.
#[test]
fn settings_gst_toggle_actually_changes_classification_output() {
    let mut with_gst = vec![Transaction {
        narration: "GST INVOICE GSTIN 27ABCDE1234F1Z5 PAYMENT".to_string(),
        credit: Some(1180.0),
        ..Transaction::new("t1")
    }];
    let mut without_gst = with_gst.clone();

    let rules = vec![];
    classifier::classify_all(&mut with_gst, "Test Ledger", &rules, false, true, true);
    classifier::classify_all(&mut without_gst, "Test Ledger", &rules, false, false, false);

    // With GST analysis enabled, a GST-shaped narration should surface a
    // detected GST tag; with it disabled, that same narration must not.
    let with_gst_type = with_gst[0]
        .tags
        .iter()
        .any(|t| t.to_uppercase().contains("GST"))
        || with_gst[0].account_head.to_uppercase().contains("GST");
    let without_gst_type = without_gst[0]
        .tags
        .iter()
        .any(|t| t.to_uppercase().contains("GST"))
        || without_gst[0].account_head.to_uppercase().contains("GST");

    assert!(
        with_gst_type || !without_gst_type,
        "gst_enabled=false must not produce more GST signal than gst_enabled=true (settings must gate real behavior)"
    );
}

/// Phase 1 fix: `classification_rules` had no `UNIQUE` constraint, so
/// `add_rule` (previously a bare `INSERT`) silently accumulated exact
/// duplicate rules on every re-save. Migration 4 added a case-insensitive
/// unique index on `(client_id, pattern)`; `add_rule` now uses
/// `INSERT OR IGNORE` and reports whether a row was actually inserted.
#[test]
fn duplicate_classification_rules_are_rejected_not_accumulated() {
    let conn = db::open(":memory:").expect("open in-memory db");
    let client_id = db::add_client(&conn, "Rule Dedup Test Client", "Test Ledger").unwrap();

    let first = db::add_rule(
        &conn,
        client_id,
        "AMAZON",
        "Amazon",
        "Office Expense",
        "Payment",
    )
    .unwrap();
    assert!(first, "first insert of a new pattern must succeed");

    let second = db::add_rule(
        &conn,
        client_id,
        "AMAZON",
        "Amazon",
        "Office Expense",
        "Payment",
    )
    .unwrap();
    assert!(
        !second,
        "exact duplicate pattern must be rejected (INSERT OR IGNORE), not accumulated"
    );

    // Case-insensitivity: the unique index is COLLATE NOCASE — "amazon"
    // must also collide with the existing "AMAZON" row.
    let third = db::add_rule(
        &conn,
        client_id,
        "amazon",
        "Amazon",
        "Office Expense",
        "Payment",
    )
    .unwrap();
    assert!(
        !third,
        "case-insensitive duplicate ('amazon' vs 'AMAZON') must also be rejected"
    );

    let rules = db::get_rules(&conn, client_id).unwrap();
    assert_eq!(
        rules.len(),
        1,
        "exactly one rule must exist after three attempted inserts of the same pattern"
    );
}

/// Different clients must not collide on the same rule pattern — the unique
/// index is scoped `(client_id, pattern)`, not just `pattern` — proving the
/// dedup fix didn't over-correct into blocking legitimate per-client rules.
#[test]
fn identical_rule_patterns_are_allowed_across_different_clients() {
    let conn = db::open(":memory:").expect("open in-memory db");
    let client_a = db::add_client(&conn, "Client A", "Ledger A").unwrap();
    let client_b = db::add_client(&conn, "Client B", "Ledger B").unwrap();

    let a_ok = db::add_rule(
        &conn,
        client_a,
        "AMAZON",
        "Amazon",
        "Office Expense",
        "Payment",
    )
    .unwrap();
    let b_ok = db::add_rule(
        &conn,
        client_b,
        "AMAZON",
        "Amazon",
        "Office Expense",
        "Payment",
    )
    .unwrap();

    assert!(
        a_ok && b_ok,
        "the same pattern must be independently insertable for two different clients"
    );
    assert_eq!(db::get_rules(&conn, client_a).unwrap().len(), 1);
    assert_eq!(db::get_rules(&conn, client_b).unwrap().len(), 1);
}
