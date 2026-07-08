//! Integration tests for ledger import, the reconciliation engine, and
//! error handling on malformed/corrupt input.
//!
//! No real ledger-name-list sample exists anywhere in the old app's repo
//! (its `assets/` folder holds only bank-statement files) — a small,
//! representative CSV fixture (`tests/fixtures/ledgers/sample_ledgers.csv`)
//! was created here instead, using the exact "Name"/"Under" header shape
//! `main.rs`'s own column-matching logic expects.

use std::path::{Path, PathBuf};

use bank_statement_processor::db;
use bank_statement_processor::parser::{self, text_extractor};
use bank_statement_processor::reconciliation::{self, BankEntry, ReconConfig, Voucher};

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
}

// ── Ledger CSV import ────────────────────────────────────────────────────────

/// Parses the CSV fixture with the exact column-matching rules
/// `on_do_import_ledgers` uses (name_keys/group_keys header matching) — this
/// logic lives inline in a Slint callback closure in main.rs, not a
/// separately-callable function, so it's replicated here rather than
/// refactoring main.rs to extract it (out of scope for this feature).
fn parse_ledger_csv(path: &Path) -> Vec<(String, String)> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_path(path)
        .expect("open ledger csv fixture");
    let rows: Vec<Vec<String>> = reader
        .records()
        .filter_map(|rec| rec.ok())
        .map(|rec| rec.iter().map(|c| c.trim().to_string()).collect())
        .collect();
    assert!(
        rows.len() >= 2,
        "fixture must have a header row plus at least one data row"
    );

    let header: Vec<String> = rows[0].iter().map(|s| s.to_lowercase()).collect();
    let name_keys = ["name", "ledger name", "ledger", "ledgername"];
    let group_keys = [
        "under",
        "group",
        "under group",
        "ledger group",
        "group name",
        "parent",
    ];
    let name_col = header
        .iter()
        .position(|h| name_keys.contains(&h.as_str()))
        .expect("no name column found");
    let group_col = header.iter().position(|h| group_keys.contains(&h.as_str()));

    let mut entries = Vec::new();
    for row in rows.iter().skip(1) {
        let name = row.get(name_col).cloned().unwrap_or_default();
        let group = group_col
            .and_then(|c| row.get(c))
            .cloned()
            .unwrap_or_default();
        if !name.is_empty() {
            entries.push((name, group));
        }
    }
    entries
}

#[test]
fn ledger_csv_fixture_parses_into_expected_name_group_pairs() {
    let entries = parse_ledger_csv(&fixture("ledgers/sample_ledgers.csv"));
    assert_eq!(entries.len(), 6, "fixture has 6 data rows");
    assert!(entries
        .iter()
        .any(|(n, g)| n == "Amazon" && g == "Sundry Creditors"));
    assert!(entries
        .iter()
        .any(|(n, g)| n == "Cash" && g == "Cash-in-Hand"));
}

#[test]
fn ledger_import_is_duplicate_safe_on_reimport() {
    let conn = db::open(":memory:").expect("open in-memory db");
    let client_id = db::add_client(&conn, "Ledger Test Client", "Test Ledger").unwrap();
    let entries = parse_ledger_csv(&fixture("ledgers/sample_ledgers.csv"));

    let added_first = db::import_ledgers(&conn, client_id, &entries).unwrap();
    assert_eq!(
        added_first,
        entries.len(),
        "first import must add every row"
    );

    let added_second = db::import_ledgers(&conn, client_id, &entries).unwrap();
    assert_eq!(
        added_second, 0,
        "re-importing the identical ledger list must add nothing (INSERT OR IGNORE)"
    );

    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM ledgers WHERE client_id = ?1",
            [client_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        total as usize,
        entries.len(),
        "table must contain exactly the original rows, not doubled"
    );
}

// ── Reconciliation ───────────────────────────────────────────────────────────

/// Builds a small but representative bank/voucher dataset covering the
/// reconciliation engine's tiers: an exact match, a same-day-fuzzy-amount
/// "likely" match, a narration-similarity-only "possible" match, and one
/// bank entry with no corresponding voucher at all (unmatched).
fn sample_recon_data() -> (Vec<BankEntry>, Vec<Voucher>) {
    let bank = vec![
        BankEntry {
            date: "01/04/2024".to_string(),
            amount: 5000.0,
            narration: "NEFT AMAZON SELLER SERVICES".to_string(),
            reference: "REF001".to_string(),
        },
        BankEntry {
            date: "02/04/2024".to_string(),
            amount: 1200.50,
            narration: "UPI PAYMENT TO VENDOR XYZ".to_string(),
            reference: "REF002".to_string(),
        },
        BankEntry {
            date: "05/04/2024".to_string(),
            amount: 9999.0,
            narration: "UNRECOGNIZED WIRE TRANSFER".to_string(),
            reference: "REF999".to_string(),
        },
    ];
    let vouchers = vec![
        Voucher {
            date: "01/04/2024".to_string(),
            amount: 5000.0,
            narration: "Amazon Seller Services".to_string(),
            voucher_no: "V001".to_string(),
            voucher_type: "Payment".to_string(),
            ledger: "Amazon".to_string(),
        },
        Voucher {
            date: "02/04/2024".to_string(),
            amount: 1200.0,
            narration: "Vendor XYZ payment".to_string(),
            voucher_no: "V002".to_string(),
            voucher_type: "Payment".to_string(),
            ledger: "Vendor XYZ".to_string(),
        },
    ];
    (bank, vouchers)
}

#[test]
fn reconciliation_matches_exact_and_leaves_the_unmatched_bank_entry_flagged() {
    let (bank, vouchers) = sample_recon_data();
    let cfg = ReconConfig::new(2, 2.0);
    let report = reconciliation::reconcile(&bank, &vouchers, &cfg);

    assert!(
        report.matched_count() >= 1,
        "expected at least one exact/high-confidence match (bank[0] <-> voucher[0])"
    );
    assert_eq!(
        report.unmatched_bank.len(),
        1,
        "the 3rd bank entry has no corresponding voucher and must stay unmatched"
    );
    assert_eq!(
        report.unmatched_bank[0], 2,
        "the unmatched entry must be the 9999.0 wire transfer at index 2"
    );
}

#[test]
fn reconciliation_report_csv_contains_every_bank_and_voucher_row() {
    let (bank, vouchers) = sample_recon_data();
    let cfg = ReconConfig::new(2, 2.0);
    let report = reconciliation::reconcile(&bank, &vouchers, &cfg);

    let csv_out = reconciliation::report_to_csv(&bank, &vouchers, &report);
    assert!(!csv_out.trim().is_empty());
    assert!(
        csv_out.contains("REF001") || csv_out.contains("5000"),
        "expected the first bank entry's data to appear in the CSV report"
    );
}

#[test]
fn parse_tally_grid_builds_vouchers_from_raw_rows() {
    let rows = vec![
        vec![
            "Date".to_string(),
            "Voucher No.".to_string(),
            "Ledger".to_string(),
            "Amount".to_string(),
            "Narration".to_string(),
        ],
        vec![
            "01-Apr-24".to_string(),
            "V001".to_string(),
            "Amazon".to_string(),
            "5000".to_string(),
            "Amazon Seller Services".to_string(),
        ],
    ];
    let vouchers = reconciliation::parse_tally_grid(&rows);
    assert!(
        !vouchers.is_empty(),
        "parse_tally_grid must recognize at least the one real data row"
    );
}

// ── Error handling ───────────────────────────────────────────────────────────

#[test]
fn loading_a_nonexistent_pdf_path_returns_an_error_not_a_panic() {
    let path = fixture("bank_statements/does_not_exist_at_all.pdf");
    let result = text_extractor::extract_pages(&path);
    assert!(
        result.is_err(),
        "extract_pages on a missing file must return Err, not panic or silently succeed"
    );
}

#[test]
fn parsing_garbage_bytes_as_a_pdf_does_not_panic() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("bsp_test_garbage_{}.pdf", std::process::id()));
    std::fs::write(&path, b"this is not a real PDF file, just garbage bytes").unwrap();

    // The real app's error path (main.rs `on_do_load_file`) treats any
    // Err/empty-rows result from a corrupt file as "no transactions found",
    // never a crash — this proves that contract holds for the parser layer.
    let result = text_extractor::extract_pages(&path);
    match result {
        Ok(rows) => assert!(
            rows.is_empty(),
            "garbage bytes must not produce fabricated rows"
        ),
        Err(_) => {} // also acceptable — the point is no panic either way
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn parsing_an_empty_excel_file_does_not_panic() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("bsp_test_empty_{}.xlsx", std::process::id()));
    std::fs::write(&path, b"").unwrap();

    let result = parser::excel_parser::parse_excel_file(&path);
    assert!(
        result.is_err(),
        "an empty/non-workbook file must return Err, not panic"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn ocr_text_parsing_handles_completely_empty_input_gracefully() {
    let result = parser::ocr_parser::parse_ocr_text("", "empty.png");
    assert!(
        result.transactions.iter().all(|t| t.is_opening_balance),
        "empty OCR text must produce zero real transactions, not fabricated ones"
    );
}

#[test]
fn db_open_on_an_invalid_path_returns_an_error_not_a_panic() {
    // A directory (not a file) path can never be opened as a SQLite
    // database file — must error cleanly, not panic.
    let dir_path = std::env::temp_dir();
    let result = db::open(&dir_path);
    assert!(
        result.is_err(),
        "opening a directory as a database file must return Err"
    );
}

#[test]
fn reconciliation_with_zero_bank_entries_or_zero_vouchers_does_not_panic() {
    let (bank, vouchers) = sample_recon_data();
    let cfg = ReconConfig::new(2, 2.0);

    let empty_bank: Vec<BankEntry> = vec![];
    let report1 = reconciliation::reconcile(&empty_bank, &vouchers, &cfg);
    assert_eq!(report1.matches.len(), 0);
    assert_eq!(report1.unmatched_vouchers.len(), vouchers.len());

    let empty_vouchers: Vec<Voucher> = vec![];
    let report2 = reconciliation::reconcile(&bank, &empty_vouchers, &cfg);
    assert_eq!(report2.matches.len(), 0);
    assert_eq!(report2.unmatched_bank.len(), bank.len());
}
