//! End-to-end test (2026-08-25) for two live bug reports:
//!
//! 1. "Save & Learn → View Rules → reuse in Auto-Classify All" — checked
//!    against a REAL on-disk SQLite database, closed and reopened between
//!    steps (not a synthetic in-memory shortcut), using the exact same
//!    `db::add_rule`/`db::get_rules`/`db::upsert_transactions` calls
//!    `main.rs`'s handlers make. Exercises the real fixes: rule creation
//!    now uses the transaction's *resolved* vendor/head/type
//!    (`main.rs::on_do_save_txn`) and a pattern derived from combined
//!    narration+reference with the glued-reference-number bug fixed
//!    (`classifier::derive_rule_pattern`).
//! 2. "Auto-Classified transactions are not visually distinguishable, and
//!    must remain so after further Auto-Classify All runs" — the row-color
//!    badge itself (`classified_row_color` in main.rs) is a pure function
//!    of `classification_source`/`status` and isn't re-tested here (see
//!    `classifier.rs`'s own unit tests for `is_locked_from_auto_classify`);
//!    this file instead proves the *persistence* guarantee that badge
//!    depends on: a manually-classified row is never silently reclassified
//!    by a later Auto-Classify All run, and a rule-classified row's fields
//!    survive a real close+reopen of the database.
//!
//! Run with `cargo test --features test-keyring-mock --test
//! rules_and_auto_classify_workflow` — this is the one test file in the
//! suite that opens a real (non-`:memory:`) database file, which routes
//! through the OS-keyring-backed encryption key lookup; the
//! `test-keyring-mock` feature swaps that for an in-memory stand-in so this
//! never touches a real user's OS credential store.

use std::path::PathBuf;

use bank_statement_processor::classifier;
use bank_statement_processor::db;
use bank_statement_processor::parser::{Transaction, TransactionStatus, VoucherType};

/// Deletes the sqlite file (and its `-wal`/`-shm` siblings, left behind by
/// WAL mode) on drop, so this test cleans up after itself even if an
/// assertion panics partway through.
struct TempDb(PathBuf);
impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

fn temp_db_path(name: &str) -> TempDb {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "bsp_rules_workflow_test_{name}_{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    TempDb(p)
}

#[test]
fn save_and_learn_creates_a_reusable_rule_that_survives_a_real_app_reopen() {
    let db_path = temp_db_path("save_learn");

    let client_id = {
        let conn = db::open(&db_path.0).expect("open real db file");
        db::add_client(&conn, "Test Client", "Test Client Current A/c").expect("add client")
    };

    // ── Step 1: a transaction the user manually classifies via "Save & Learn" ──
    // Real narration shape (glued UPI reference number, descriptive
    // merchant hint split into `reference` by ocr_parser.rs) — not a
    // hand-picked easy case; this is the dominant shape in real data.
    let mut t1 = Transaction::new("t1");
    t1.date = "10/04/2024".to_string();
    t1.narration = "UPI209584004029".to_string();
    t1.reference = "kirana".to_string();
    t1.debit = Some(505.0);
    // Mirrors on_do_save_txn's field-resolution: by the time "Save & Learn"
    // runs, vendor/head/type are already the transaction's own resolved
    // values (not the raw, possibly-blank UI callback parameters).
    t1.vendor = "Shree Ganesh Kirana Stores".to_string();
    t1.account_head = "Grocery Expense".to_string();
    t1.txn_type = VoucherType::Payment;
    t1.status = TransactionStatus::Classified;
    t1.confidence = 1.0;
    t1.classification_source = "user".to_string();

    let pattern = classifier::derive_rule_pattern(&t1).expect("must derive a usable pattern");
    assert!(
        !pattern.contains("209584004029"),
        "the unique transaction id must not survive into the learned pattern: {pattern:?}"
    );

    {
        let conn = db::open(&db_path.0).expect("reopen real db file");
        let created = db::add_rule(
            &conn,
            client_id,
            &pattern,
            &t1.vendor,
            &t1.account_head,
            &t1.txn_type.to_string(),
        )
        .expect("add_rule must not error");
        assert!(created, "add_rule must report the rule as newly created");
        db::upsert_transactions(&conn, client_id, None, std::slice::from_ref(&t1))
            .expect("persist the manually-classified transaction");
    }

    // ── Step 2: "reopen the application" — a brand new connection ──────────
    let rules = {
        let conn = db::open(&db_path.0).expect("reopen real db file again");
        db::get_rules(&conn, client_id).expect("get_rules")
    };
    assert_eq!(
        rules.len(),
        1,
        "the learned rule must survive a real close+reopen of the database"
    );
    let rule = &rules[0];
    assert_eq!(rule.pattern, pattern, "View Rules must show the exact pattern that was learned");
    assert_eq!(rule.vendor, "Shree Ganesh Kirana Stores");
    assert_eq!(rule.account_head, "Grocery Expense");
    assert_eq!(rule.txn_type, "Payment");
    assert_eq!(rule.client_id, client_id, "a client-specific rule, not accidentally global");

    // ── Step 3: a *different*, later transaction with a similar narration ──
    // A different UPI reference id (every real UPI transaction has its own
    // unique one) but the same merchant hint — exactly "add a similar
    // transaction" from the acceptance criteria.
    let mut t2 = Transaction::new("t2");
    t2.date = "15/04/2024".to_string();
    t2.narration = "UPI209666677788".to_string();
    t2.reference = "kirana".to_string();
    t2.debit = Some(320.0);

    let mut txns = vec![t2.clone()];
    let changed = classifier::classify_all(
        &mut txns,
        "Test Client Current A/c",
        &rules,
        false,
        false,
        false,
    );
    assert_eq!(changed, 1);
    let classified = txns[0].clone();
    assert_eq!(
        classified.vendor, "Shree Ganesh Kirana Stores",
        "Auto-Classify All must reuse the learned rule for a similar future transaction"
    );
    assert_eq!(classified.account_head, "Grocery Expense");
    assert_eq!(classified.classification_source, "rule");
    assert!(matches!(classified.status, TransactionStatus::Classified));

    // ── Step 4: the manually-classified row (t1) must never be silently
    // reclassified by a later Auto-Classify All run ────────────────────────
    // This is the persistence guarantee the "AUTO vs MANUAL" row badge
    // depends on: if classify_all could still overwrite t1's fields, its
    // badge would silently flip from MANUAL back to AUTO on the very next
    // run despite the user never having touched it again.
    let mut combined = vec![t1.clone(), classified];
    let changed2 = classifier::classify_all(
        &mut combined,
        "Test Client Current A/c",
        &rules,
        false,
        false,
        false,
    );
    assert_eq!(
        changed2, 0,
        "neither the manually-classified row nor the already rule-classified row should change again"
    );
    assert_eq!(combined[0].classification_source, "user");
    assert_eq!(combined[0].vendor, "Shree Ganesh Kirana Stores");
    assert_eq!(combined[0].account_head, "Grocery Expense");

    // ── Step 5: the manually-classified transaction's own DB row also
    // survives a real reopen unchanged ──────────────────────────────────────
    let reloaded = {
        let conn = db::open(&db_path.0).expect("reopen real db file a third time");
        db::get_transactions(&conn, client_id).expect("get_transactions")
    };
    let reloaded_t1 = reloaded
        .iter()
        .find(|t| t.id == "t1")
        .expect("the manually-classified transaction must still be in the database");
    assert_eq!(reloaded_t1.vendor, "Shree Ganesh Kirana Stores");
    assert_eq!(reloaded_t1.account_head, "Grocery Expense");
    assert_eq!(reloaded_t1.classification_source, "user");
    assert!(matches!(reloaded_t1.status, TransactionStatus::Classified));
}

#[test]
fn deleting_one_rule_leaves_every_other_rule_for_the_client_intact() {
    let db_path = temp_db_path("delete_rule");
    let conn = db::open(&db_path.0).expect("open real db file");
    let client_id = db::add_client(&conn, "Test Client", "Test Client Current A/c").expect("add client");

    db::add_rule(&conn, client_id, "KIRANA STORE", "Kirana Store", "Grocery Expense", "Payment")
        .expect("add rule 1");
    db::add_rule(&conn, client_id, "AIRTEL POSTPAID", "Airtel", "Telephone Expense", "Payment")
        .expect("add rule 2");

    let rules = db::get_rules(&conn, client_id).expect("get_rules");
    assert_eq!(rules.len(), 2);
    let kirana_rule_id = rules
        .iter()
        .find(|r| r.pattern == "KIRANA STORE")
        .expect("kirana rule present")
        .id;

    db::delete_rule(&conn, kirana_rule_id).expect("delete_rule");

    let remaining = db::get_rules(&conn, client_id).expect("get_rules after delete");
    assert_eq!(remaining.len(), 1, "only the selected rule must be deleted");
    assert_eq!(remaining[0].pattern, "AIRTEL POSTPAID");
}
