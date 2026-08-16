//! CSV bank-statement parser.
//!
//! Unlike every other file in `parser/`, this one has no `parser.js`
//! ancestor to port — CSV bank-statement transaction import never existed
//! in the old Electron app either (see the now-removed
//! `csv_bank_statement_import_is_not_implemented_in_either_app` test in
//! `tests/import_pipeline.rs`, which documented that gap until this file
//! closed it).
//!
//! Rather than reimplementing header detection, debit/credit direction
//! handling (including the Kotak signed-column and Dr/Cr-suffix cases),
//! noise filtering, continuation-row merging, dedup, balance validation,
//! and opening-balance-row synthesis, this module reads a CSV file into
//! exactly the same `Vec<Vec<GridCell>>` grid shape
//! [`excel_parser::grid_from_range`] builds from a spreadsheet, then hands
//! it to the same [`excel_parser::extract_sheet_from_grid`] every Excel
//! sheet already goes through. A CSV file, once read into rows of text, is
//! not meaningfully different from a single-sheet Excel workbook as far as
//! that pipeline is concerned — so nothing about transaction extraction or
//! normalization is duplicated here, only CSV-specific file reading and
//! bank detection.

use std::path::Path;

use anyhow::{Context, Result};

use super::bank_detection::{detect, DetectOptions};
use super::excel_parser::{extract_sheet_from_grid, GridCell};
use super::ParseResult;

// ── CSV reading ───────────────────────────────────────────────────────────────

/// Empty/whitespace-only fields become `GridCell::Empty`, matching how a
/// blank Excel cell already arrives via `Data::Empty` — this keeps
/// `GridCell::is_empty_cell()` (used by `detect_cols_from_content`'s
/// blank-row skip) meaningful for CSV input too, rather than every blank
/// field showing up as a non-empty `Text("")`.
fn cell_from_field(raw: &str) -> GridCell {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        GridCell::Empty
    } else {
        GridCell::Text(trimmed.to_owned())
    }
}

/// Reads `path` into a grid of `GridCell`s.
///
/// `has_headers(false)` — header detection happens at the grid level
/// (`extract_sheet_from_grid` scans for it itself, exactly like it does
/// for Excel), matching real bank CSV exports which commonly prefix the
/// transaction table with several metadata rows the `csv` crate's own
/// header handling has no way to skip. `flexible(true)` tolerates a
/// short/ragged row (a trailing blank line, a row missing its final
/// column) without aborting the whole read — same `ReaderBuilder`
/// configuration already used for ledger CSV import
/// (`main.rs`'s `on_do_import_ledgers`).
///
/// Unlike ledger import, a genuinely malformed row (broken quote escaping,
/// not just a short row) is **not** silently filtered out here — it's
/// propagated as an `Err` so a corrupt file is reported, not silently
/// under-imported.
fn grid_from_csv(path: &Path) -> Result<Vec<Vec<GridCell>>> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_path(path)
        .with_context(|| format!("Cannot open CSV file: {}", path.display()))?;

    let mut grid = Vec::new();
    for (i, rec) in reader.records().enumerate() {
        let rec =
            rec.with_context(|| format!("Malformed CSV at row {} in {}", i + 1, path.display()))?;
        grid.push(rec.iter().map(cell_from_field).collect());
    }
    Ok(grid)
}

// ── Bank detection ────────────────────────────────────────────────────────────

/// Backfills `result.bank_name`/`result.account_no` (and the same fields on
/// every transaction that doesn't already carry one) via
/// `bank_detection::detect` — the same call `excel_parser`'s own
/// (private) `apply_bank_detection` makes, reimplemented here rather than
/// exposed cross-module, since `excel_parser::apply_bank_detection` isn't
/// `pub`. Behaviorally identical: same `DetectOptions` fields, same
/// backfill-only-if-blank rule.
fn apply_bank_detection(result: &mut ParseResult, grid: &[Vec<GridCell>], filename: &str) {
    let header_text: String = if result.header_row_idx > 0 {
        grid[..result.header_row_idx]
            .iter()
            .map(|r| r.iter().map(|c| c.raw_str()).collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    };
    let full_text: String = grid
        .iter()
        .map(|r| r.iter().map(|c| c.raw_str()).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n");
    let narrations: Vec<&str> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .map(|t| t.narration.as_str())
        .collect();

    let detected = detect(DetectOptions {
        text: &full_text,
        header_text: &header_text,
        filename,
        narrations: &narrations,
    });

    result.bank_name = detected.bank_name.clone();
    result.account_no = detected.account_no.clone();
    for t in &mut result.transactions {
        if t.bank_name.is_empty() {
            t.bank_name = detected.bank_name.clone();
        }
        if t.account_no.is_empty() {
            t.account_no = detected.account_no.clone();
        }
    }
}

// ── Top-level file entry point ────────────────────────────────────────────────

/// Parse a CSV bank statement.
///
/// Mirrors `excel_parser::parse_excel_file`'s shape: read the file into a
/// grid, run it through the shared extraction pipeline, apply bank
/// detection, and fail with a descriptive `Err` — never a silently-empty
/// result — when the file can't be read, is empty, or doesn't contain a
/// recognizable Date/Narration/Debit/Credit table.
pub fn parse_csv_file(path: &Path) -> Result<ParseResult> {
    let grid = grid_from_csv(path)?;
    if grid.is_empty() {
        anyhow::bail!("CSV file is empty: {}", path.display());
    }

    let filename = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("")
        .to_owned();

    let mut result = extract_sheet_from_grid(&grid, &filename).with_context(|| {
        format!(
            "No transactions found in: {}\n\
             Make sure the file has Date, Narration/Description, Debit, and Credit columns.",
            path.display()
        )
    })?;

    // extract_sheet_from_grid can return Some(_) with only a synthetic
    // opening-balance row and zero real transactions (same contract
    // parse_excel_file checks via `real_count` before trusting a sheet) —
    // treat that the same way: not a usable result.
    let real_count = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .count();
    if real_count == 0 {
        anyhow::bail!(
            "No transactions found in: {}\n\
             Make sure the file has Date, Narration/Description, Debit, and Credit columns.",
            path.display()
        );
    }

    apply_bank_detection(&mut result, &grid, &filename);
    Ok(result)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `content` to a uniquely-named temp `.csv` file and returns its
    /// path — same `std::env::temp_dir()` + `std::process::id()` convention
    /// already used throughout this crate's own tests (e.g.
    /// `tests/ledger_reconciliation_errors.rs`'s
    /// `parsing_an_empty_excel_file_does_not_panic`).
    fn write_temp_csv(name: &str, content: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bsp_csv_parser_test_{}_{}.csv",
            name,
            std::process::id()
        ));
        std::fs::write(&path, content).expect("write temp csv");
        path
    }

    const HDFC_STYLE_CSV: &str = "\
A S Havaldar & Co\r\n\
Statement of account\r\n\
Account No: 50100123456789\r\n\
\r\n\
Date,Narration,Value Dt,Chq/Ref No.,Withdrawal Amt.,Deposit Amt.,Closing Balance\r\n\
02/01/2024,NEFT/RTG234567891/RATAN TATA/AXIS0001234,02/01/2024,RTG234567891,,50000.00,135000.00\r\n\
03/01/2024,ATM WDL/ATM123456/HDFC BANK ATM,03/01/2024,ATM123456,10000.00,,125000.00\r\n\
05/01/2024,SALARY CREDIT ACME PVT LTD JAN 2024,05/01/2024,SAL00001,,80000.00,205000.00\r\n\
";

    #[test]
    fn valid_csv_produces_transactions() {
        let path = write_temp_csv("valid", HDFC_STYLE_CSV);
        let result = parse_csv_file(&path).expect("valid CSV must parse");

        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert_eq!(real.len(), 3, "expected 3 real transactions, got: {real:?}");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn csv_debit_credit_columns_map_correctly() {
        let path = write_temp_csv("debit_credit", HDFC_STYLE_CSV);
        let result = parse_csv_file(&path).expect("valid CSV must parse");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();

        // Row 1: Withdrawal Amt. blank, Deposit Amt. = 50000 → credit, no debit.
        assert_eq!(real[0].debit, None);
        assert_eq!(real[0].credit, Some(50000.0));

        // Row 2: Withdrawal Amt. = 10000, Deposit Amt. blank → debit, no credit.
        assert_eq!(real[1].debit, Some(10000.0));
        assert_eq!(real[1].credit, None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn csv_date_and_narration_map_correctly() {
        let path = write_temp_csv("date_narration", HDFC_STYLE_CSV);
        let result = parse_csv_file(&path).expect("valid CSV must parse");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();

        assert_eq!(real[0].date, "02/01/2024");
        assert!(
            real[0].narration.contains("RATAN TATA"),
            "got narration: {}",
            real[0].narration
        );
        assert_eq!(real[2].date, "05/01/2024");
        assert!(real[2].narration.contains("SALARY CREDIT"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn csv_header_is_detected_despite_metadata_rows_above_it() {
        // HDFC_STYLE_CSV has 3 metadata text lines, then one genuinely
        // blank line, then the header. The `csv` crate skips a fully
        // blank line entirely rather than emitting an empty record for
        // it, so that line contributes no grid row — the header lands at
        // grid index 3 (3 metadata rows: indices 0-2), not row "5" by a
        // naive count of source lines. Either way, the point this test
        // proves is unchanged: the same scanning header detector Excel
        // uses (not "row 0 is always the header") is genuinely being
        // reused for CSV.
        let path = write_temp_csv("header_scan", HDFC_STYLE_CSV);
        let result = parse_csv_file(&path).expect("valid CSV must parse");
        assert_eq!(result.header_row_idx, 3);
        assert!(result.col_map.has_date());
        assert!(result.col_map.has_amount());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn kotak_style_signed_single_column_csv_maps_direction_by_sign() {
        // Proves the full excel_parser row-extraction pipeline is reused,
        // not just header detection — the Kotak signed-column handling
        // (negative = debit, positive = credit) lives entirely inside
        // extract_sheet_from_grid.
        let csv = "\
Date,Description,Reference,DEBIT/CREDIT,Balance\r\n\
01/04/2024,SALARY CREDIT,SAL001,50000,150000\r\n\
03/04/2024,UBER RIDE MUMBAI,UBR001,-650,149350\r\n\
";
        let path = write_temp_csv("kotak", csv);
        let result = parse_csv_file(&path).expect("valid CSV must parse");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();

        assert_eq!(real[0].credit, Some(50000.0));
        assert_eq!(real[0].debit, None);
        assert_eq!(real[1].debit, Some(650.0));
        assert_eq!(real[1].credit, None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_csv_file_returns_error() {
        let path = write_temp_csv("empty", "");
        let result = parse_csv_file(&path);
        assert!(result.is_err(), "an empty CSV file must return Err");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn csv_with_no_recognizable_bank_statement_columns_returns_error() {
        // Structurally valid CSV, but nothing resembling Date/Narration/
        // Debit/Credit — e.g. a ledger-name CSV, not a statement.
        let csv = "Name,Group\r\nAcme Traders,Sundry Creditors\r\nRatan Tata,Sundry Debtors\r\n";
        let path = write_temp_csv("unsupported", csv);
        let result = parse_csv_file(&path);
        assert!(
            result.is_err(),
            "a CSV with no date/amount columns must return Err, not an empty success"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_csv_quoting_returns_error_not_panic() {
        // An opening quote that never closes before EOF is a genuine CSV
        // syntax error the `csv` crate itself rejects, distinct from a
        // merely-ragged row (which `flexible(true)` already tolerates).
        let csv = "Date,Narration,Debit,Credit\r\n01/01/2024,\"UNTERMINATED,100,\r\n";
        let path = write_temp_csv("malformed", csv);
        let result = parse_csv_file(&path);
        assert!(
            result.is_err(),
            "malformed CSV quoting must return Err, not panic or silently drop the row"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn nonexistent_csv_file_returns_error_not_panic() {
        let path = std::env::temp_dir().join(format!(
            "bsp_csv_parser_test_does_not_exist_{}.csv",
            std::process::id()
        ));
        let result = parse_csv_file(&path);
        assert!(result.is_err(), "a missing file must return Err, not panic");
    }
}
