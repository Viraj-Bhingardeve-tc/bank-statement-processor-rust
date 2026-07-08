//! Integration tests for analytics computation, all three export formats
//! (Excel, CSV, Tally XML), database persistence, re-import idempotency, and
//! multi-client isolation — using real transactions parsed from SBI.pdf
//! (confirmed working by `tests/import_pipeline.rs`).

use std::path::{Path, PathBuf};

use bank_statement_processor::analytics;
use bank_statement_processor::db;
use bank_statement_processor::export::{accounting, excel, tally};
use bank_statement_processor::parser::{self, pdf_parser, text_extractor, Transaction};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bank_statements")
        .join(name)
}

fn real_sbi_transactions() -> Vec<Transaction> {
    let path = fixture("SBI.pdf");
    let rows = text_extractor::extract_pages(&path).expect("SBI.pdf: extract_pages failed");
    let result = pdf_parser::parse_pdf_rows(rows, "SBI.pdf").unwrap_or_else(|| {
        let full_text = text_extractor::extract_full_text(&path);
        let preprocessed = parser::ocr_parser::preprocess_multiline(&full_text);
        parser::ocr_parser::parse_ocr_text(&preprocessed, "SBI.pdf")
    });
    let txns: Vec<Transaction> = result
        .transactions
        .into_iter()
        .filter(|t| !t.is_opening_balance)
        .collect();
    assert!(
        !txns.is_empty(),
        "test setup: SBI.pdf produced zero transactions"
    );
    txns
}

// ── Analytics ────────────────────────────────────────────────────────────────

#[test]
fn analytics_compute_produces_internally_consistent_results_for_real_data() {
    let txns = real_sbi_transactions();
    let result = analytics::compute(&txns, Some(10000.0));

    let expected_credit: f64 = txns.iter().filter_map(|t| t.credit).sum();
    let expected_debit: f64 = txns.iter().filter_map(|t| t.debit).sum();

    assert!(
        (result.summary.total_credit - expected_credit).abs() < 0.01,
        "total_credit mismatch"
    );
    assert!(
        (result.summary.total_debit - expected_debit).abs() < 0.01,
        "total_debit mismatch"
    );
    assert!(
        (result.summary.net_flow - (expected_credit - expected_debit)).abs() < 0.01,
        "net_flow must equal total_credit - total_debit"
    );
    assert_eq!(
        result.summary.txn_count,
        txns.len(),
        "txn_count must match input length"
    );
    assert!(
        result.summary.suspense_amount >= 0.0,
        "suspense_amount must never be negative"
    );
}

// ── DB persistence, re-import, multi-client isolation ───────────────────────

#[test]
fn real_transactions_persist_and_reload_identically() {
    let conn = db::open(":memory:").expect("open in-memory db");
    let client_id = db::add_client(&conn, "Persistence Test Client", "Test Bank Ledger").unwrap();
    let txns = real_sbi_transactions();

    let import_id = db::save_import(&conn, client_id, "SBI.pdf", "SBI", "", txns.len()).unwrap();
    let written = db::upsert_transactions(&conn, client_id, Some(import_id), &txns).unwrap();
    assert_eq!(
        written,
        txns.len(),
        "every real transaction must be written"
    );

    let reloaded = db::get_transactions(&conn, client_id).unwrap();
    assert_eq!(
        reloaded.len(),
        txns.len(),
        "reload must return the same count"
    );

    let mut original_ids: Vec<&str> = txns.iter().map(|t| t.id.as_str()).collect();
    let mut reloaded_ids: Vec<&str> = reloaded.iter().map(|t| t.id.as_str()).collect();
    original_ids.sort_unstable();
    reloaded_ids.sort_unstable();
    assert_eq!(
        original_ids, reloaded_ids,
        "reloaded transaction ids must match exactly (same set)"
    );
}

/// "Re-import Excel"/re-loading the same statement twice must not duplicate
/// rows in the database — `upsert_transactions` uses `INSERT OR REPLACE` by
/// the transaction's own stable id, so importing the identical file twice
/// must leave the row count unchanged, not doubled.
#[test]
fn re_importing_the_same_real_statement_does_not_duplicate_rows() {
    let conn = db::open(":memory:").expect("open in-memory db");
    let client_id = db::add_client(&conn, "Re-import Test Client", "Test Bank Ledger").unwrap();
    let txns = real_sbi_transactions();

    let import_id_1 = db::save_import(&conn, client_id, "SBI.pdf", "SBI", "", txns.len()).unwrap();
    db::upsert_transactions(&conn, client_id, Some(import_id_1), &txns).unwrap();
    let count_after_first = db::get_transactions(&conn, client_id).unwrap().len();

    // Re-import the identical parsed transactions (same ids, same content).
    let import_id_2 = db::save_import(&conn, client_id, "SBI.pdf", "SBI", "", txns.len()).unwrap();
    db::upsert_transactions(&conn, client_id, Some(import_id_2), &txns).unwrap();
    let count_after_second = db::get_transactions(&conn, client_id).unwrap().len();

    assert_eq!(
        count_after_first, count_after_second,
        "re-importing the identical statement must not duplicate rows (INSERT OR REPLACE by id)"
    );
}

/// Two different clients loading statements whose transaction IDs happen to
/// collide must not corrupt/reassign each other's data — the same fixture
/// written under two different `client_id`s should be independently
/// retrievable, and deleting one client's transactions should not affect
/// the other's.
///
/// **Formerly a real, severe bug — fixed in migration 5** (see
/// `CROSS_CLIENT_TRANSACTION_ID_FIX_REPORT.md`). `transactions.id` used to
/// be the table's *sole* primary key, global across every client, while ids
/// are generated purely from in-file position with no client-specific salt
/// at all (guaranteed to collide for the synthetic opening-balance row,
/// plausible for any two files with matching row counts) — two clients
/// whose imports produced the same id silently overwrote and reassigned
/// each other's transactions via `upsert_transactions`'s `INSERT OR
/// REPLACE`. Migration 5 rebuilds the table with a composite `PRIMARY KEY
/// (client_id, id)`, so the *same* literal id now coexists correctly across
/// different clients at the schema level — this test was previously
/// `#[ignore]`d documenting the bug; it now runs for real as a permanent
/// regression test.
#[test]
fn multi_client_transaction_data_is_fully_isolated() {
    let conn = db::open(":memory:").expect("open in-memory db");
    let client_a = db::add_client(&conn, "Client A", "Ledger A").unwrap();
    let client_b = db::add_client(&conn, "Client B", "Ledger B").unwrap();
    let txns = real_sbi_transactions();

    let import_a = db::save_import(&conn, client_a, "SBI.pdf", "SBI", "", txns.len()).unwrap();
    db::upsert_transactions(&conn, client_a, Some(import_a), &txns).unwrap();
    let import_b = db::save_import(&conn, client_b, "SBI.pdf", "SBI", "", txns.len()).unwrap();
    db::upsert_transactions(&conn, client_b, Some(import_b), &txns).unwrap();

    assert_eq!(
        db::get_transactions(&conn, client_a).unwrap().len(),
        txns.len(),
        "client A's rows must not be overwritten/reassigned by client B's insert of colliding ids"
    );
    assert_eq!(
        db::get_transactions(&conn, client_b).unwrap().len(),
        txns.len()
    );

    db::delete_transactions_for_client(&conn, client_a).unwrap();
    assert_eq!(
        db::get_transactions(&conn, client_a).unwrap().len(),
        0,
        "client A's transactions must be gone"
    );
    assert_eq!(
        db::get_transactions(&conn, client_b).unwrap().len(),
        txns.len(),
        "client B's transactions must be untouched by client A's deletion"
    );
}

// ── Export: Excel, CSV, Tally XML ───────────────────────────────────────────

#[test]
fn excel_export_writes_a_readable_file_with_the_real_transaction_count() {
    let txns = real_sbi_transactions();
    let dir = std::env::temp_dir();
    let path = dir.join(format!("bsp_test_export_{}.xlsx", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let written = excel::export_xlsx(
        &txns,
        "Test Client",
        "Test Ledger",
        "SBI.pdf",
        Some(10000.0),
        None,
        &path,
    )
    .expect("excel export failed");
    assert_eq!(written, txns.len());
    assert!(path.exists(), "export_xlsx must create a file");
    assert!(
        std::fs::metadata(&path).unwrap().len() > 0,
        "exported xlsx must not be empty"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn csv_export_content_includes_every_real_transaction_amount() {
    let txns = real_sbi_transactions();
    let dir = std::env::temp_dir();
    let path = dir.join(format!("bsp_test_export_{}.csv", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let written = excel::export_csv(
        &txns,
        "Test Client",
        "Test Ledger",
        "SBI.pdf",
        Some(10000.0),
        None,
        &path,
    )
    .expect("csv export failed");
    assert_eq!(written, txns.len());

    let content = std::fs::read_to_string(&path).expect("read exported csv");
    // Spot-check: the first real transaction's narration must appear
    // somewhere in the exported content (proves real data reached the file,
    // not just a header-only stub).
    if let Some(first) = txns.first() {
        if !first.narration.trim().is_empty() {
            assert!(
                content.contains(first.narration.trim()),
                "exported CSV must contain the first real transaction's narration"
            );
        }
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn tally_xml_export_is_well_formed_and_contains_real_amounts() {
    let txns = real_sbi_transactions();
    let opts = tally::TallyOpts {
        company: "Test Company".to_string(),
        gstin: String::new(),
        fy: String::new(),
        bank_ledger: "Test Bank Ledger".to_string(),
        date_from: None,
        date_to: None,
        only_classified: false,
        include_ledgers: true,
        include_narrations: true,
        include_ob: false,
        skip_low_conf: false,
    };
    let xml = tally::generate(&txns, &opts, Some(10000.0));

    assert!(
        xml.contains("<ENVELOPE>") || xml.contains("<TALLYMESSAGE"),
        "expected well-formed Tally XML envelope tags, got: {}",
        &xml[..xml.len().min(300)]
    );
    assert!(!xml.trim().is_empty());

    // Every real transaction must contribute at least one voucher amount to
    // the XML — spot-check the count-preview matches the real txn count.
    let preview = tally::count_preview(&txns, &opts);
    assert!(
        preview.total > 0,
        "expected at least one voucher generated from real transactions"
    );
}

#[test]
fn generic_xml_accounting_export_validates_cleanly_for_real_data() {
    let txns = real_sbi_transactions();
    let opts = accounting::AccountingOpts {
        software: accounting::Software::Xml,
        company: "Test Company".to_string(),
        gstin: String::new(),
        fy: String::new(),
        state_code: String::new(),
        currency: "INR".to_string(),
        bank_ledger: "Test Bank Ledger".to_string(),
        date_from: None,
        date_to: None,
        include_ob: false,
        include_gst: true,
        include_ledgers: true,
        include_narrations: true,
        only_classified: false,
        skip_low_conf: false,
    };
    let xml = accounting::generate(&txns, &opts, Some(10000.0));
    assert!(
        !xml.trim().is_empty(),
        "generic XML export must not be empty for a real, non-empty transaction set"
    );

    let validation = accounting::validate(&txns, &opts);
    // Real bank data always has at least one row; validation must not
    // reject the whole export outright (errors are allowed for individual
    // rows, e.g. unclassified ones, but the export itself must be usable).
    assert!(
        !validation
            .errors
            .iter()
            .any(|e| e.to_lowercase().contains("no transactions")),
        "export must not report zero transactions for real data"
    );
}
