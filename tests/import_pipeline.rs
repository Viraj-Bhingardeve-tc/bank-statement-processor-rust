//! Integration tests for the real import pipelines (PDF, Excel, CSV, OCR)
//! against real bank-statement fixtures copied from the old Electron app's
//! own `assets/` folder (`tests/fixtures/bank_statements/`) — the same files
//! that app's own test/demo data used, across 11 different banks.
//!
//! CSV bank-statement import (`parser::csv_parser`) has no real-bank fixture
//! file available (none exists anywhere in either repo), so its coverage
//! here is synthetic — mechanics-level tests already live in
//! `parser::csv_parser`'s own `#[cfg(test)]` module; the tests below only
//! confirm the public `parser::csv_parser::parse_csv_file` entry point
//! integrates end-to-end the same way `excel_parser`/`pdf_parser` already
//! do in this file.
//!
//! Real OCR-image fixtures are deliberately **not** exercised here — see the
//! module-level doc comment on `ocr_pipeline_has_no_real_image_fixture_available`
//! below for exactly why, rather than silently omitting them.

use std::path::{Path, PathBuf};

use bank_statement_processor::parser::{self, pdf_parser, text_extractor};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bank_statements")
        .join(name)
}

/// 5 of the 11 real PDF fixtures actually parse successfully through the
/// real app pipeline today. The other 6 are deliberately excluded here and
/// covered by their own `#[ignore]`d tests below, documenting three distinct,
/// independently-confirmed extraction bugs this suite discovered (not test
/// artifacts) — see `PDF_FIXTURES_WITH_IDENTITY_H_BUG` (4 files),
/// `cosmos_pdf_exposes_a_missing_ocr_fallback_for_near_empty_text` (1 file),
/// and `icici_wealth_management_pdf_extracts_zero_pages` (1 file). All 11
/// were probed via `cargo run --example pdf_batch_probe` before finalizing
/// this split — this is not a guess.
const PDF_FIXTURES: &[&str] = &[
    "Bank of Maharashtra.pdf",
    "IDBI Bank.pdf",
    "Kotak Bank.pdf",
    "Mahanager Co-operative bank.pdf",
    "SBI.pdf",
];

/// 4 of the 11 fixtures whose embedded text is (partly or entirely)
/// Identity-H/CID-encoded in a way `lopdf`'s text extractor cannot decode —
/// see `pdf_fixtures_with_identity_h_encoding_produce_zero_transactions`'s
/// doc comment for the full explanation. Note "Kotak Bank.pdf" also
/// contains some Identity-H-encoded text but is *not* in this list: its
/// transaction table happens to use a normal font, so multi-line
/// preprocessing still recovers 538 real transactions from it — proving the
/// failure mode is about *which* text is affected, not just presence of the
/// string "Identity-H" anywhere in the document.
const PDF_FIXTURES_WITH_IDENTITY_H_BUG: &[&str] = &[
    "BOB.pdf",
    "ICICI Bank.pdf",
    "IDFCFIRSTBankstatement.pdf",
    "Union Bank.pdf",
];

/// Runs the *real* two-stage pipeline `main.rs`'s "Load File" handler uses —
/// Stage 1 (`extract_pages` -> `parse_pdf_rows`, structured-column parsing)
/// first, falling back to Stage 2 (`extract_full_text` -> `parse_ocr_text`,
/// applied to the PDF's own embedded text layer, no Tesseract needed) if
/// Stage 1 doesn't recognize the format — exactly mirroring
/// `run_pdf_ocr_pipeline`'s non-Tesseract branch. Both stages independently
/// run bank-detection, so the returned `ParseResult.bank_name` is checked
/// regardless of which stage produced it.
fn parse_pdf_via_real_pipeline(path: &Path, name: &str) -> parser::ParseResult {
    let rows = text_extractor::extract_pages(path)
        .unwrap_or_else(|e| panic!("{name}: extract_pages failed: {e:#}"));
    assert!(
        !rows.is_empty(),
        "{name}: extract_pages returned no pages/rows"
    );

    if let Some(r) = pdf_parser::parse_pdf_rows(rows, name) {
        return r;
    }

    let full_text = text_extractor::extract_full_text(path);
    assert!(
        !full_text.trim().is_empty(),
        "{name}: Stage 1 failed AND embedded text is empty — would need real Tesseract OCR, unavailable in this environment"
    );
    let ocr = parser::ocr_parser::parse_ocr_text(&full_text, name);
    if ocr.transactions.iter().any(|t| !t.is_opening_balance) {
        return ocr;
    }
    let preprocessed = parser::ocr_parser::preprocess_multiline(&full_text);
    let ml = parser::ocr_parser::parse_ocr_text(&preprocessed, name);
    assert!(
        ml.transactions.iter().any(|t| !t.is_opening_balance),
        "{name}: neither Stage 1, Stage 2, nor multi-line-preprocessed Stage 2 found any transactions"
    );
    ml
}

/// Every real PDF fixture (except BOB.pdf, see above) must parse into at
/// least one usable transaction, with a bank name detected and every real
/// row passing the app's own `is_usable()` gate (valid date, at least one
/// amount) — via the real two-stage pipeline, not Stage 1 in isolation
/// (some of these real statements, e.g. "Bank of Maharashtra.pdf", only
/// match Stage 2 in the real app too — that's expected, not a regression).
///
/// Bank-name detection is checked too, except for
/// "Mahanager Co-operative bank.pdf": its 65 transactions parse correctly,
/// but its name doesn't match any pattern in the ~45-bank list ported from
/// the old app — a smaller regional co-operative bank falling outside that
/// fixed set. This is the already-audited "bank coverage capped, no
/// mechanism to add new banks without a code change" limitation
/// (`PROJECT_AUDIT_2026-07-06.md` §8), not a new finding, so it's excluded
/// from this specific assertion rather than reported as a fourth new bug.
#[test]
fn every_real_pdf_fixture_parses_into_usable_transactions_with_a_detected_bank() {
    const BANK_NAME_NOT_IN_PATTERN_LIST: &[&str] = &["Mahanager Co-operative bank.pdf"];

    for name in PDF_FIXTURES {
        let path = fixture(name);
        assert!(path.exists(), "fixture missing: {}", path.display());

        let result = parse_pdf_via_real_pipeline(&path, name);

        let real_txns = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .count();
        assert!(real_txns > 0, "{name}: parsed zero real transactions");
        if !BANK_NAME_NOT_IN_PATTERN_LIST.contains(name) {
            assert!(
                !result.bank_name.is_empty(),
                "{name}: bank_name is empty — bank-detection did not run or matched nothing"
            );
        }

        for t in result.transactions.iter().filter(|t| !t.is_opening_balance) {
            assert!(
                !t.date.is_empty(),
                "{name}: transaction with empty date: {t:?}"
            );
            assert!(
                t.debit.is_some() || t.credit.is_some(),
                "{name}: transaction with neither debit nor credit: {t:?}"
            );
        }
    }
}

/// **Real bug found by this suite, not fixed here** (out of scope for an
/// integration-test-only change — see the task rules: one feature at a
/// time, no unrelated fixes bundled in). Affects 4 of the 11 real fixtures
/// (`PDF_FIXTURES_WITH_IDENTITY_H_BUG`), not just one.
///
/// Each of these PDFs embeds its transaction-table text using an
/// Identity-H/CID-keyed font that the `lopdf`-based text extractor cannot
/// decode to Unicode glyphs — instead of real characters, `extract_full_text`
/// returns the literal placeholder string `"?Identity-H Unimplemented?"`
/// repeated hundreds to thousands of times (a condition `src/bin/pdf_diag.rs:25`
/// already had a dedicated check for, confirming this was previously
/// discovered — but never fixed, and not mentioned anywhere in
/// `PROJECT_AUDIT_2026-07-06.md`).
///
/// This is worse than a missing-Tesseract limitation: `run_pdf_ocr_pipeline`
/// in `main.rs` only falls back to Tesseract when `full_text.trim().is_empty()`
/// — but this garbage text is *not* empty, so the real app today would
/// silently hand this garbage to `parse_ocr_text`, get zero transactions, and
/// tell the user "No transactions found — PDF may use embedded fonts"
/// without ever attempting real OCR, even on a machine with Tesseract
/// properly installed. A real user with any of these 4 exact PDFs cannot
/// load them today, with no actionable error.
///
/// "Kotak Bank.pdf" is the counter-example proving this isn't a blanket
/// "any Identity-H text = broken" rule: it also contains Identity-H-encoded
/// text (probably a logo/header) but its transaction table uses a normal
/// font, so it still produces 538 real transactions — which is exactly why
/// it stays in `PDF_FIXTURES` rather than here.
///
/// Confirmed via `cargo run --example pdf_batch_probe` against all 11
/// copied fixtures. Flagged prominently as a production blocker in the
/// final report rather than fixed in this commit.
#[test]
#[ignore = "KNOWN BUG (not fixed here, out of scope for this feature): 4 fixtures use an Identity-H CID font that defeats lopdf's text extraction, producing placeholder garbage that main.rs's OCR fallback doesn't detect as \"needs Tesseract\" — see doc comment"]
fn pdf_fixtures_with_identity_h_encoding_produce_zero_transactions() {
    for name in PDF_FIXTURES_WITH_IDENTITY_H_BUG {
        let path = fixture(name);
        let full_text = text_extractor::extract_full_text(&path);
        assert!(
            full_text.contains("Identity-H Unimplemented"),
            "{name}: if this now fails, the Identity-H bug may have been fixed for this file — move it back into PDF_FIXTURES if so"
        );
    }
}

/// **Second real bug found by this suite, not fixed here** (same
/// out-of-scope rationale as the BOB.pdf case above).
///
/// `Cosmos Co-operative.pdf`'s embedded text layer contains almost nothing:
/// `extract_full_text` returns only 144 characters — page furniture like
/// "You can get account statement through e-mail" and "Date Stamp Manager"
/// — with the actual transaction table entirely absent from the text layer
/// (very likely rendered as an image or a graphics structure `lopdf`'s
/// content-stream text extraction doesn't capture).
///
/// This is the *same root cause class* as the BOB.pdf bug, with a different
/// symptom: `run_pdf_ocr_pipeline`'s decision to attempt real Tesseract OCR
/// is a bare `full_text.trim().is_empty()` check. 144 characters of
/// letterhead text is not empty, so Tesseract is never attempted here
/// either, even though the extracted text is transparently useless for
/// finding transactions. A real fix would need a stronger "is this text
/// actually usable" heuristic (e.g. a minimum line/token count, or checking
/// whether `parse_ocr_text`/`preprocess_multiline` found anything at all
/// before giving up) rather than a bare emptiness check — deliberately not
/// implemented here, flagged as a production blocker in the final report.
#[test]
#[ignore = "KNOWN BUG (not fixed here, out of scope for this feature): Cosmos Co-operative.pdf's embedded text layer is 144 characters of letterhead furniture with no transaction data, and main.rs's is_empty()-only OCR-fallback check doesn't detect this as \"needs Tesseract\" — see doc comment"]
fn cosmos_pdf_exposes_a_missing_ocr_fallback_for_near_empty_text() {
    let path = fixture("Cosmos Co-operative.pdf");
    let full_text = text_extractor::extract_full_text(&path);
    assert!(
        full_text.trim().len() < 500,
        "if this now fails (much more text than before), the extraction has improved — remove the #[ignore] and fold Cosmos Co-operative.pdf back into PDF_FIXTURES"
    );
}

/// **Third real bug found by this suite, not fixed here** (same
/// out-of-scope rationale as the two cases above; time-boxed per
/// instruction rather than root-caused further).
///
/// `ICICI Bank Wealth management.pdf` (the largest fixture, 6.2MB) fails at
/// the very first extraction step: `extract_pages` returns `Ok(vec![])` —
/// zero pages/rows, no error — and `extract_full_text` likewise returns
/// effectively nothing. Unlike the BOB.pdf and Cosmos cases, this fails
/// before even reaching the text layer, so this is a distinct symptom
/// (most likely explanation, not confirmed: this file's size/page count or
/// PDF structural complexity exceeds something `lopdf`'s positional
/// extraction handles for the other 10 fixtures — genuinely root-causing
/// this would mean stepping through `lopdf`'s object parsing on a 6.2MB
/// file, out of scope for an integration-test-only change).
#[test]
#[ignore = "KNOWN BUG (not fixed here, out of scope for this feature, not further root-caused per time-box): ICICI Bank Wealth management.pdf extracts zero pages/rows and zero text — fails before even reaching the text layer, unlike the other two known-bad fixtures"]
fn icici_wealth_management_pdf_extracts_zero_pages() {
    let path = fixture("ICICI Bank Wealth management.pdf");
    let rows = text_extractor::extract_pages(&path).unwrap_or_default();
    assert!(
        rows.is_empty(),
        "if this now fails (rows found), extraction has improved — remove the #[ignore] and fold this fixture back into PDF_FIXTURES"
    );
}

/// The Excel fixture (HDFC.xls) must parse through the exact same
/// `parse_excel_file` entry point the "Load File" button uses, and — since
/// this session's Phase 1 work specifically fixed Excel-path bank detection
/// — must come out with a non-empty bank name, not the pre-fix blank one.
#[test]
fn real_excel_fixture_parses_with_bank_detection() {
    let path = fixture("HDFC.xls");
    assert!(path.exists(), "fixture missing: {}", path.display());

    let result = parser::excel_parser::parse_excel_file(&path)
        .unwrap_or_else(|e| panic!("HDFC.xls: parse_excel_file failed: {e:#}"));

    let real_txns = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .count();
    assert!(real_txns > 0, "HDFC.xls: parsed zero real transactions");
    assert!(
        !result.bank_name.is_empty(),
        "HDFC.xls: bank_name is empty — Excel-path bank detection regressed"
    );
}

/// OCR's text-parsing half (`parse_ocr_text`) is exercised with realistic
/// OCR-shaped text (the same style of input the existing `ocr_parser` unit
/// tests already use) chained through downstream processing, proving the
/// OCR entity itself produces usable `Transaction`s end-to-end.
///
/// This is **not** a full image -> Tesseract -> text -> transactions test:
/// no OCR image fixture exists anywhere in either repo (verified — a fresh
/// search of both trees for png/jpg/jpeg/tiff/tif/bmp outside icon/build
/// directories returned zero results), and Tesseract itself is not
/// installed on this machine (`tesseract` is absent from PATH), so the
/// image-to-text step cannot be exercised here even if a fixture existed.
/// Per instruction, this is reported rather than faked: the Tesseract
/// shell-out is untested by this suite; the text-to-transactions logic it
/// feeds into is.
#[test]
fn ocr_text_pipeline_produces_usable_transactions_from_realistic_ocr_text() {
    let realistic_ocr_text = "\
Date        Narration                          Withdrawal   Deposit   Balance
01/04/2024  UPI-AMAZON PAY-user@ybl-REF123456   1500.00                48500.00
02/04/2024  NEFT-SALARY CREDIT-ACME CORP                    50000.00  98500.00
03/04/2024  ATM WDL-SBI ATM MUMBAI              5000.00                93500.00
";
    let result = parser::ocr_parser::parse_ocr_text(realistic_ocr_text, "ocr_test.png");
    let real_txns: Vec<_> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .collect();
    assert!(
        !real_txns.is_empty(),
        "OCR text pipeline produced zero transactions from realistic input"
    );
    assert!(
        real_txns.iter().any(|t| t.debit.is_some()) && real_txns.iter().any(|t| t.credit.is_some()),
        "expected at least one debit and one credit row to be recognized"
    );
}

/// End-to-end through the public `parser::csv_parser::parse_csv_file` entry
/// point (as opposed to `parser::csv_parser`'s own unit tests, which call
/// its private helpers directly) — a real file on disk, read, header-
/// detected, and turned into transactions, the same integration shape as
/// this file's Excel/PDF tests. CSV *ledger name* import (a separate,
/// already-implemented feature) is unaffected — see
/// `ledger_reconciliation_errors.rs`.
#[test]
fn csv_bank_statement_import_produces_real_debit_and_credit_transactions() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "bsp_import_pipeline_csv_test_{}.csv",
        std::process::id()
    ));
    std::fs::write(
        &path,
        "Date,Narration,Ref No.,Withdrawal Amt.,Deposit Amt.,Balance\r\n\
         02/01/2024,NEFT/RTG234567891/RATAN TATA/AXIS0001234,RTG234567891,,50000.00,135000.00\r\n\
         03/01/2024,ATM WDL/ATM123456/HDFC BANK ATM,ATM123456,10000.00,,125000.00\r\n",
    )
    .unwrap();

    let result = parser::csv_parser::parse_csv_file(&path).expect("a well-formed CSV must parse");
    let real_txns: Vec<_> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .collect();

    assert_eq!(real_txns.len(), 2, "expected 2 real transactions");
    assert!(
        real_txns.iter().any(|t| t.debit.is_some()),
        "expected at least one debit row"
    );
    assert!(
        real_txns.iter().any(|t| t.credit.is_some()),
        "expected at least one credit row"
    );
    assert_eq!(real_txns[0].date, "02/01/2024");

    let _ = std::fs::remove_file(&path);
}

/// Mirrors this file's own `parsing_garbage_bytes_as_a_pdf_does_not_panic`/
/// `parsing_an_empty_excel_file_does_not_panic` pattern (see
/// `ledger_reconciliation_errors.rs`) for the CSV path: a file with no
/// recognizable Date/Narration/Debit/Credit table must fail loudly, not
/// silently import zero rows as if it had succeeded.
#[test]
fn csv_bank_statement_import_rejects_a_file_with_no_recognizable_columns() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "bsp_import_pipeline_csv_unsupported_test_{}.csv",
        std::process::id()
    ));
    std::fs::write(&path, "Name,Group\r\nAcme Traders,Sundry Creditors\r\n").unwrap();

    let result = parser::csv_parser::parse_csv_file(&path);
    assert!(
        result.is_err(),
        "a CSV with no statement-shaped columns must return Err"
    );

    let _ = std::fs::remove_file(&path);
}
