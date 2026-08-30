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

/// 8 of the 11 real PDF fixtures parse successfully through the real app
/// pipeline today. The other 3 are deliberately excluded here and covered by
/// their own `#[ignore]`d tests below — see
/// `pdf_fixtures_with_a_squashed_single_line_table_extract_far_fewer_
/// transactions_than_the_real_statement_contains` (2 files: "ICICI Bank.pdf",
/// "IDBI Bank.pdf" — see that test's doc comment; IDBI recovers a handful of
/// real transactions so isn't a hard zero, but is still excluded from the
/// per-fixture assertions below since most of its data is unrecovered), and
/// `icici_wealth_management_pdf_extracts_zero_pages` (1 file). All 11 were
/// probed via `cargo run --example pdf_batch_probe` before finalizing this
/// split — this is not a guess.
///
/// "BOB.pdf", "IDFCFIRSTBankstatement.pdf", and "Union Bank.pdf" moved into
/// this list after two fixes: (1) bumping `lopdf` to a pinned `=0.35.0`
/// resolved the Identity-H/CID-font decoding failure that used to make
/// `extract_full_text` return the literal placeholder string
/// `"?Identity-H Unimplemented?"` for these 4 files (this bug and its
/// symptom are why `PDF_FIXTURES_WITH_IDENTITY_H_BUG` used to exist — see
/// git history for that now-removed const; "Kotak Bank.pdf" was always the
/// counter-example proving Identity-H text elsewhere in a document doesn't
/// block its transaction table, which uses a normal font); (2)
/// `ocr_parser.rs` needed three further fixes before the now-readable text
/// actually produced *correct* transactions rather than a single reversed
/// or garbled amount, all found and locked in by
/// `bob_and_union_bank_pdfs_reconcile_almost_perfectly_after_the_ordering_
/// and_amount_extraction_fixes` below: reverse-chronological (newest-first)
/// statements broke the balance-movement debit/credit heuristic (BOB lists
/// newest-first; see `AmtInfo`'s doc comment in `ocr_parser.rs`), a
/// merchant/UTR id glued directly onto a narration word with no separating
/// space could be misread as a transaction amount by `extract_amounts`'s
/// bare-digit branch (Union Bank: "cfmer.33421130"), and a two-amount line
/// picked the wrong one by blind position instead of by which one actually
/// explains the observed balance movement (Union Bank: an SMS short-code in
/// page-footer marketing boilerplate outweighing the real amount). "ICICI
/// Bank.pdf" did NOT move: its Identity-H text is now readable too, but it
/// turned out to have the same squashed-single-line-table problem as "IDBI
/// Bank.pdf" — see that test below.
///
/// "IDFCFIRSTBankstatement.pdf" stayed in this list (it always parsed into
/// *some* transactions) but had its own separate, later-discovered bug
/// (2026-08-29): the same squashed-single-line-table root cause as ICICI/
/// IDBI (every embedded-text item at `x=0.0`) meant Stage 1 never actually
/// populated a real column here either, so this fixture was quietly riding
/// on Stage 2's flat-text Debit/Credit *guessing* the whole time — which
/// guessed wrong for the statement's very first transaction and a handful
/// of others throughout, not just a cosmetic edge case. `parse_pdf_via_real_
/// pipeline` (used by the loop below) still exercises that same flat-text
/// path and is intentionally left asserting only "some transactions, no
/// mixing" (loose enough that the pre-fix guesses satisfied it too); the
/// real fix lives in a dedicated `extract_idfc_first_transactions`
/// (`transaction_extractor.rs`) reached only via the Tier 0 OCR path — see
/// `idfc_first_bank_pdf_debit_credit_is_never_mixed_via_ocr` below for the
/// end-to-end lock-in against the real fixture via that path.
///
/// "Union Bank.pdf" had the exact same fate, discovered even later
/// (2026-08-30): the same `x=0.0` root cause, the same silent flat-text
/// Debit/Credit guessing riding underneath this loop's loose assertions
/// the whole time, and its own dedicated Tier-0-OCR extractor,
/// `extract_union_bank_transactions` — see that function's doc comment for
/// why this one needed a genuinely different anchoring strategy (no header
/// row survives anywhere in this fixture to read Debit/Credit/Balance
/// column positions from, unlike every other extractor in this module) and
/// `union_bank_pdf_debit_credit_is_never_mixed_via_ocr` below for the
/// end-to-end lock-in.
///
/// "Cosmos Co-operative.pdf" moved into this list after fixing the *actual*
/// root cause of the bug `cosmos_pdf_exposes_a_missing_ocr_fallback_for_
/// near_empty_text` used to document (that test name and its old rationale
/// are now WRONG — see
/// `cosmos_co_operative_bank_pdf_reconciles_exactly_after_the_quote_
/// operator_extraction_fix` below for what was actually happening and how
/// it was fixed): `lopdf::Document::extract_text` only understands the
/// `Tj`/`TJ` text-showing operators, not the equally spec-legal `'`
/// (quote) / `"` (double-quote) operators — and Cosmos's PDF generator
/// draws essentially every line of the transaction table with `'`. This
/// was never a missing-OCR-fallback problem: real, complete embedded text
/// was present in the PDF the whole time (confirmed directly against the
/// decoded content stream), `extract_text` was just silently dropping
/// nearly all of it. Fixed in `text_extractor::extract_page_text`, which
/// walks decoded content-stream operations itself instead of delegating to
/// `lopdf::Document::extract_text` — see that function's doc comment.
const PDF_FIXTURES: &[&str] = &[
    "Bank of Maharashtra.pdf",
    "BOB.pdf",
    "Cosmos Co-operative.pdf",
    "IDFCFIRSTBankstatement.pdf",
    "Kotak Bank.pdf",
    "Mahanager Co-operative bank.pdf",
    "SBI.pdf",
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

/// Every real PDF fixture in `PDF_FIXTURES` must parse into at
/// least one usable transaction, with a bank name detected and every real
/// row passing the app's own `is_usable()` gate (valid date, at least one
/// amount) — via the real two-stage pipeline, not Stage 1 in isolation
/// (some of these real statements, e.g. "Bank of Maharashtra.pdf", only
/// match Stage 2 in the real app too — that's expected, not a regression).
///
/// Bank-name detection is checked for every fixture, including
/// "Mahanager Co-operative bank.pdf" — previously excluded from this
/// assertion because "Mahanagar Co-operative Bank" fell outside the
/// ~45-bank pattern list ported from the old app (the already-audited
/// "bank coverage capped, no mechanism to add new banks without a code
/// change" limitation, `PROJECT_AUDIT_2026-07-06.md` §8). Fixed
/// 2026-08-30 by registering the bank in `bank_detection.rs`'s
/// IFSC/phrase/abbreviation maps — see
/// `mahanagar_co_operative_bank_pdf_is_identified_correctly` below for the
/// dedicated end-to-end lock-in (exact bank name, real transaction count,
/// and the counterparty-narration-code trap this fixture's own data
/// happens to contain).
#[test]
fn every_real_pdf_fixture_parses_into_usable_transactions_with_a_detected_bank() {
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
        assert!(
            !result.bank_name.is_empty(),
            "{name}: bank_name is empty — bank-detection did not run or matched nothing"
        );

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

/// "Mahanager Co-operative bank.pdf" bank-detection fix (2026-08-30), tested
/// end-to-end against the real fixture via the same real two-stage pipeline
/// `parse_pdf_via_real_pipeline` above exercises — not a synthetic
/// reproduction. This fixture's own no-header-row body specifically parses
/// via Stage 2 (`extract_full_text` -> `parse_ocr_text`, over the PDF's own
/// embedded text layer — no Tesseract needed, text is already present);
/// Stage 1's column-boundary detection has no header row anywhere in the
/// file to anchor on and returns `None`, same as `Bank of Maharashtra.pdf`
/// (see `every_real_pdf_fixture_parses_into_usable_transactions_with_a_
/// detected_bank`'s doc comment) — expected, not a regression.
///
/// Root cause of the actual bank-detection bug: this statement's own
/// identity (bank name, header, footer, letterhead, IFSC, branch, PDF
/// metadata) is entirely absent from the file — confirmed by rendering all
/// 5 pages to images and reading them directly, and by `mutool info`
/// showing no Title/Author/Subject, just a generic `Producer: iText`.
/// Every page is pure transaction-table body. "Mahanagar Co-operative
/// Bank" also simply wasn't a registered bank anywhere in
/// `bank_detection.rs`'s IFSC/phrase/abbreviation tables at all — not a
/// false-positive misdetection, `bank_name` came back completely empty.
/// Fixed by registering it (IFSC prefix "MCBL", verified against
/// real-world IFSC listings; phrase entries for both the correct spelling
/// and the "Mahanager" typo the actual filename carries; a matching
/// OCR-abbreviation entry) so the filename tier (P6) — the only evidence
/// source this file has — can resolve it. Transaction extraction itself
/// was untouched (already correct via the existing Stage 2 path; out of
/// scope per the fix's own requirements).
///
/// This fixture's own data happens to also exercise the exact "don't
/// misdetect from a counterparty's bank code in narration" trap this
/// engine has hit before (`detect_union_bank_not_saraswat_via_narration_
/// counterparty_code` in `bank_detection.rs`): the account holder makes
/// frequent IMPS transfers to their own linked Union Bank of India
/// account, so "SAVINGS 410702010405405 UBIN" (Union Bank's own IFSC
/// prefix) repeats throughout the real narration text — asserted below to
/// confirm it doesn't win over the correct "Mahanagar Co-operative Bank"
/// filename-based detection.
#[test]
fn mahanagar_co_operative_bank_pdf_is_identified_correctly() {
    let path = fixture("Mahanager Co-operative bank.pdf");
    let result = parse_pdf_via_real_pipeline(&path, "Mahanager Co-operative bank.pdf");

    assert_eq!(
        result.bank_name, "Mahanagar Co-operative Bank",
        "must be detected via the filename tier, not left empty and not misdetected as \
         \"Union Bank of India\" from the self-transfer narration's UBIN counterparty code"
    );

    let real: Vec<&parser::Transaction> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .collect();
    assert!(
        !real.is_empty(),
        "expected at least one real transaction, got zero"
    );
    assert!(
        real.iter().any(|t| t.narration.contains("UBIN")),
        "sanity check: this fixture's own real narration must still contain the Union Bank \
         counterparty reference this test exists to prove doesn't win — if this assertion \
         fails, the fixture's content changed and this test's premise needs re-checking"
    );

    for t in &real {
        assert!(
            !(t.debit.is_some() && t.credit.is_some()),
            "transaction has BOTH debit and credit set: {t:?}"
        );
        assert!(
            t.debit.is_some() || t.credit.is_some(),
            "transaction has NEITHER debit nor credit set: {t:?}"
        );
        assert!(!t.date.is_empty(), "transaction with empty date: {t:?}");
    }
}

/// Real Debit/Credit-mixing bug fix (2026-08-25), tested end-to-end against
/// the actual "Kotak Bank.pdf" fixture — not a synthetic reproduction (see
/// `transaction_extractor`'s own unit tests for that).
///
/// This file renders each transaction as 8 separate physical text lines
/// (Sl.No/Date/Time/ValueDate/Narration/Ref/signed Amount/Balance) with no
/// shared X position between fields at all — neither `extract_fw_transactions`
/// (needs a whole transaction on one physical line) nor the header/column-
/// boundary detection `parse_pdf_rows` normally relies on can recognize
/// this, so it used to fall all the way through to the unreliable OCR-text
/// fallback path (`ocr_parser`'s flat full-text extraction, which has no
/// column identity). Confirmed via `examples/kotak_debug_probe.rs` that path
/// silently read the running **Balance** into the Debit/Credit field and a
/// **Sl. No.** row-counter into the Balance field for the majority of real
/// transactions in this file — corrupting almost every amount, exactly the
/// live bug report this test locks in the fix for.
///
/// See `transaction_extractor::extract_kotak_narrow_transactions`'s own doc
/// comment for the full parsing design.
#[test]
fn kotak_narrow_layout_debit_credit_and_balance_reconcile_exactly() {
    let path = fixture("Kotak Bank.pdf");
    let rows = text_extractor::extract_pages(&path).expect("Kotak Bank.pdf: extract_pages failed");
    let result = pdf_parser::parse_pdf_rows(rows, "Kotak Bank.pdf").expect(
        "Kotak Bank.pdf must now parse via Stage 1 (the narrow-layout extractor), \
         not fall through to the unreliable OCR-text fallback",
    );

    assert_eq!(result.bank_name, "Kotak Mahindra Bank");

    let real: Vec<&parser::Transaction> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .collect();
    assert!(
        real.len() > 500,
        "expected several hundred real transactions, got {}",
        real.len()
    );

    for t in &real {
        assert!(
            !(t.debit.is_some() && t.credit.is_some()),
            "transaction has BOTH debit and credit set: {t:?}"
        );
        assert!(
            t.debit.is_some() || t.credit.is_some(),
            "transaction has NEITHER debit nor credit set: {t:?}"
        );
    }

    // Full balance-continuity reconciliation across the entire real
    // statement: every transaction's own balance must equal the previous
    // balance plus its credit minus its debit. This is exactly the
    // invariant the pre-fix bug violated on almost every row (it read the
    // *next* row's real balance as this row's amount, and a bogus row
    // counter as this row's balance).
    let mut prev_balance = result.opening_balance;
    let mut mismatches = 0usize;
    for t in &real {
        if let (Some(pb), Some(bal)) = (prev_balance, t.balance) {
            let expected = pb + t.credit.unwrap_or(0.0) - t.debit.unwrap_or(0.0);
            if (expected - bal).abs() > 0.01 {
                mismatches += 1;
            }
        }
        prev_balance = t.balance.or(prev_balance);
    }
    assert_eq!(
        mismatches, 0,
        "{mismatches} of {} transactions have a balance that doesn't reconcile with \
         (previous balance + credit - debit)",
        real.len()
    );

    // Spot-check a known Credit and a known Debit by narration — catches a
    // regression that swaps the two globally without necessarily breaking
    // reconciliation (e.g. if some other post-processing step masked it).
    let neft_credit = real
        .iter()
        .find(|t| t.narration.contains("CONNEXIONS"))
        .expect("expected at least one NEFT CONNEXIONS inward transfer in the fixture");
    assert!(
        neft_credit.credit.is_some() && neft_credit.debit.is_none(),
        "NEFT CONNEXIONS inward transfer must be a Credit: {neft_credit:?}"
    );
    let lic_debit = real
        .iter()
        .find(|t| t.narration.contains("LIC OF INDIA"))
        .expect("expected at least one LIC NACH debit in the fixture");
    assert!(
        lic_debit.debit.is_some() && lic_debit.credit.is_none(),
        "LIC NACH debit must be a Debit: {lic_debit:?}"
    );
}

/// **"ICICI Bank.pdf" fixed (2026-08-29) — via a different mechanism than
/// this test exercises, so its assertion below is intentionally unchanged.**
/// This test's ICICI half only ever checked the Stage 2 *flat-text* path
/// (`extract_full_text` → `parse_ocr_text`), which is genuinely
/// unrecoverable for this file for exactly the reason described below — that
/// remains true, hence `icici_real == 0` here still holds and always will.
/// The actual fix lives one level up the real pipeline: `run_pdf_ocr_pipeline`
/// (`main.rs`)'s Tier 0 renders the page to an image and reads it back with
/// Tesseract (`ocr_extractor::extract_pages_via_ocr`), recovering genuine
/// per-word X positions the flat-text layer had already destroyed, and
/// `extract_icici_normal_transactions` (`transaction_extractor.rs`) turns
/// those word-boxes into transactions. See
/// `icici_bank_normal_pdf_imports_successfully_via_ocr` below for the
/// end-to-end lock-in against the real fixture via that path.
///
/// **"IDBI Bank.pdf" fixed too (2026-08-29) — same mechanism, same reason
/// this test's IDBI assertion below is also intentionally unchanged.** Its
/// squashed page 1 is the exact same root cause (bare `Td`/`Tm` positioning,
/// no line-break operators) and the exact same fix: Tier 0 renders both
/// pages and reads real per-word positions back with Tesseract, and a
/// dedicated `extract_idbi_transactions` (`transaction_extractor.rs`) turns
/// those word-boxes into transactions — recovering page 1's ~20 rows *and*
/// page 2's remaining 4 uniformly, since OCR doesn't care that page 2's own
/// text layer happened to be less broken to begin with. This test's IDBI
/// half only ever checked `parse_pdf_via_real_pipeline` (Stage 1
/// embedded-text → Stage 2 flat-text → multiline-preprocessed Stage 2),
/// never the Tier 0 OCR path, so `idbi_real < 10` below remains true and
/// intentionally unchanged. See `idbi_bank_pdf_imports_successfully_via_ocr`
/// below for the end-to-end lock-in against the real fixture via that path.
///
/// "ICICI Bank.pdf" and "IDBI Bank.pdf" both extract fine at the text layer
/// (no Identity-H garbage — that separate bug, which used to affect these
/// two plus "BOB.pdf"/"IDFCFIRSTBankstatement.pdf"/"Union Bank.pdf", was
/// fixed by pinning `lopdf` to `=0.35.0`; see `PDF_FIXTURES`'s doc comment)
/// but both PDFs render their entire transaction table as one (ICICI) or a
/// few (IDBI) massively long single text lines with every row's fields —
/// serial no., two dates, a timestamp, free-text narration, and TWO
/// currency amounts — concatenated with **no consistent delimiter**:
/// sometimes a real space, sometimes nothing at all (verified verbatim in
/// the ICICI fixture: `"...409524494660//BALAJI C/KARB/balajiod9@kb"` next
/// to `"...58598.29378368.15"` — two 2-decimal amounts glued directly
/// together with zero separator, "58598.29" + "378368.15"). Splitting a
/// glued `"58598.29378368.15"` back into its two real numbers is
/// fundamentally ambiguous without a second signal (there's no delimiter to
/// anchor on), and getting it wrong produces exactly the "random amount
/// extraction" / "balance treated as transaction amount" failure modes this
/// suite's other fixes (see `PDF_FIXTURES`'s doc comment) were written to
/// eliminate — a rushed regex splitter here would trade one data-loss bug
/// for a data-corruption bug, worse for a user who can no longer tell the
/// output is wrong. This needs a dedicated positional/column-aware
/// extractor (the same class of fix `extract_kotak_narrow_transactions`
/// already provides for Kotak Bank.pdf's own distinct narrow-layout
/// problem), not a best-effort regex — genuinely out of scope for this
/// session's time-box.
///
/// "IDBI Bank.pdf" is a partial case, not a hard zero, at the Stage-1/2
/// levels this test exercises: its statement happens to *also* render its
/// most recent 4 transactions in a normal one-field-per-line layout (a
/// distinct "recent transactions" section) which parses correctly and
/// reconciles — but the other ~20 (of a bank-reported 24 total: "Dr Count 7"
/// + "Cr Count 17") live only in the unparseable squashed line and are
/// silently missing from this path's result. All 24 are recovered via the
/// Tier 0 OCR path instead — see the doc comment above and
/// `idbi_bank_pdf_imports_successfully_via_ocr` below.
#[test]
#[ignore = "documents the Stage-2 flat-text limitation this test's ICICI and IDBI halves were originally about (both still true, unchanged — both fixed via a different path, see icici_bank_normal_pdf_imports_successfully_via_ocr and idbi_bank_pdf_imports_successfully_via_ocr); see doc comment"]
fn pdf_fixtures_with_a_squashed_single_line_table_extract_far_fewer_transactions_than_the_real_statement_contains(
) {
    // ICICI Bank.pdf: the whole table is one line → Stage 2 (raw OCR-text
    // parsing, no preprocessing) can't find a single date-anchored row in
    // it, so it produces literally zero real transactions.
    let icici_path = fixture("ICICI Bank.pdf");
    let icici_text = text_extractor::extract_full_text(&icici_path);
    let icici_result = parser::ocr_parser::parse_ocr_text(&icici_text, "ICICI Bank.pdf");
    let icici_real = icici_result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .count();
    assert_eq!(
        icici_real, 0,
        "ICICI Bank.pdf: if this now fails (real transactions found), the squashed-line \
         extraction has improved — remove the #[ignore] and fold this fixture back into PDF_FIXTURES"
    );

    // IDBI Bank.pdf: recovers only its 4 "recent transactions" (a separately
    // and normally laid-out section), missing the ~20 that live only in the
    // file's squashed dense table.
    let idbi_path = fixture("IDBI Bank.pdf");
    let idbi_result = parse_pdf_via_real_pipeline(&idbi_path, "IDBI Bank.pdf");
    let idbi_real = idbi_result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .count();
    assert!(
        idbi_real < 10,
        "IDBI Bank.pdf: if this now fails (10+ real transactions found, closer to the \
         bank-reported 24), the squashed-line extraction has improved — remove the #[ignore] \
         and fold this fixture back into PDF_FIXTURES; got {idbi_real}"
    );
}

/// Locks in the BOB.pdf / Union Bank.pdf fixes described in `PDF_FIXTURES`'s
/// doc comment end-to-end against the real fixtures, the same way
/// `kotak_narrow_layout_debit_credit_and_balance_reconcile_exactly` does for
/// Kotak Bank.pdf — not a synthetic reproduction (`ocr_parser`'s own unit
/// tests already cover the mechanics in isolation: reverse-chronological
/// ordering, the glued-narration-digit-run amount guard, and the
/// magnitude-aware two-amount direction pick).
///
/// A handful of real mismatches are tolerated (not asserted to zero) because
/// a couple of true edge cases remain unexplained in each file (2/181 for
/// BOB, 0/1447 for Union Bank at the time of writing) rather than pinning
/// the test to an exact count that would break on unrelated future changes.
#[test]
fn bob_and_union_bank_pdfs_reconcile_almost_perfectly_after_the_ordering_and_amount_extraction_fixes(
) {
    for (name, min_real_txns, max_mismatch_pct) in
        [("BOB.pdf", 150, 0.05), ("Union Bank.pdf", 1000, 0.05)]
    {
        let path = fixture(name);
        let result = parse_pdf_via_real_pipeline(&path, name);
        let real: Vec<&parser::Transaction> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert!(
            real.len() > min_real_txns,
            "{name}: expected > {min_real_txns} real transactions, got {}",
            real.len()
        );

        for t in &real {
            assert!(
                !(t.debit.is_some() && t.credit.is_some()),
                "{name}: transaction has BOTH debit and credit set: {t:?}"
            );
            assert!(
                t.debit.is_some() || t.credit.is_some(),
                "{name}: transaction has NEITHER debit nor credit set: {t:?}"
            );
        }

        let mut checked = 0usize;
        let mut mismatches = 0usize;
        for t in &real {
            if let (Some(pb), Some(bal)) = (t.prev_balance, t.balance) {
                checked += 1;
                let expected = pb + t.credit.unwrap_or(0.0) - t.debit.unwrap_or(0.0);
                if (expected - bal).abs() > 0.01 {
                    mismatches += 1;
                }
            }
        }
        assert!(
            checked > 0,
            "{name}: no transaction had both prev_balance and balance set — reconciliation \
             wasn't actually exercised"
        );
        let pct = mismatches as f64 / checked as f64;
        assert!(
            pct <= max_mismatch_pct,
            "{name}: {mismatches}/{checked} ({:.1}%) transactions don't reconcile with \
             (previous balance + credit - debit), exceeding the {:.0}% tolerance",
            pct * 100.0,
            max_mismatch_pct * 100.0
        );
    }
}

/// **Real bug found and fixed (2026-08-27): Cosmos Co-operative Bank PDF
/// import.** Locks in the fix end-to-end against the real fixture, the same
/// way `bob_and_union_bank_pdfs_reconcile_almost_perfectly_after_the_
/// ordering_and_amount_extraction_fixes` does for BOB/Union Bank.
///
/// This test used to be `cosmos_pdf_exposes_a_missing_ocr_fallback_for_
/// near_empty_text`, `#[ignore]`d, asserting `extract_full_text` returned
/// under 500 chars of unusable letterhead text and blaming a missing OCR
/// fallback. That diagnosis was wrong. The actual root cause, found by
/// dumping this fixture's *decoded page content stream* directly (not just
/// re-reading `extract_full_text`'s output): Cosmos's PDF generator draws
/// essentially every line of the transaction table using the `'` (quote)
/// content-stream operator — "move to the next line and show a text
/// string", PDF 1.7 §9.4.3 Table 209, exactly as spec-legal as `Tj`/`TJ` —
/// but `lopdf::Document::extract_text` (`lopdf::parser_aux::
/// extract_text_chunks_from_page`) only matches `"Tj" | "TJ"` in its
/// operator loop. It silently drops every `'`-drawn line, which for this
/// file is nearly the whole page: the real, complete transaction-table text
/// was sitting right there in the PDF the entire time, extractable, just
/// never looked at. `main.rs`'s Tesseract fallback check
/// (`full_text.trim().is_empty()`) correctly did NOT fire, because the text
/// genuinely wasn't empty (a few real `Tj`-drawn fragments — the letterhead
/// — survived) — this was never a missing-OCR problem.
///
/// Fixed in `text_extractor::extract_page_text`, which walks the page's
/// decoded content-stream operations directly (same `Tf`/font-encoding
/// tracking, same `Document::decode_text` calls lopdf's own version uses —
/// verified to produce byte-identical output to `doc.extract_text()` for
/// every other fixture in `PDF_FIXTURES`, all of which use only `Tj`/`TJ`)
/// and additionally handles `'`, `"`, and `T*`. See that function's doc
/// comment for the full explanation.
///
/// Cross-checked independently against `pdftotext -layout` (poppler, not
/// lopdf) on the same fixture: 77 date-prefixed transaction rows, matching
/// this test's own count exactly.
#[test]
fn cosmos_co_operative_bank_pdf_reconciles_exactly_after_the_quote_operator_extraction_fix() {
    let path = fixture("Cosmos Co-operative.pdf");
    let result = parse_pdf_via_real_pipeline(&path, "Cosmos Co-operative.pdf");

    assert_eq!(result.bank_name, "Cosmos Co-operative Bank");

    let real: Vec<&parser::Transaction> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .collect();
    assert_eq!(
        real.len(),
        77,
        "expected exactly 77 real transactions (independently cross-checked via `pdftotext \
         -layout`), got {}",
        real.len()
    );

    for t in &real {
        assert!(
            !(t.debit.is_some() && t.credit.is_some()),
            "transaction has BOTH debit and credit set: {t:?}"
        );
        assert!(
            t.debit.is_some() || t.credit.is_some(),
            "transaction has NEITHER debit nor credit set: {t:?}"
        );
        // Reference must never leak into narration for the rows Cosmos's
        // own statement gives an explicit Chq.No. for.
        assert!(
            !t.narration.contains("7311") && !t.narration.contains("7312"),
            "cheque number leaked into narration instead of reference: {t:?}"
        );
    }

    // At least the great majority of real rows are UPI/IMPS/PRCR (all
    // slash-delimited with a real UTR) — confirm Reference is now actually
    // populated for them instead of uniformly empty.
    let with_reference = real.iter().filter(|t| !t.reference.is_empty()).count();
    assert!(
        with_reference >= 60,
        "expected at least 60/{} real transactions to have a non-empty Reference \
         (UPI/IMPS/PRCR UTR or Chq.No.), got {}",
        real.len(),
        with_reference
    );

    // Full balance-continuity reconciliation across the entire real
    // statement: every transaction's own balance must equal the previous
    // balance plus its credit minus its debit, exactly (this file's own
    // "Withdrawals"/"Deposits"/"Balance" columns are internally exact — no
    // tolerance needed, unlike the OCR-text-path BOB/Union Bank fixtures).
    let mut prev_balance = result.opening_balance;
    let mut mismatches = 0usize;
    let mut checked = 0usize;
    for t in &real {
        if let (Some(pb), Some(bal)) = (prev_balance, t.balance) {
            checked += 1;
            let expected = pb + t.credit.unwrap_or(0.0) - t.debit.unwrap_or(0.0);
            if (expected - bal).abs() > 0.01 {
                mismatches += 1;
            }
        }
        prev_balance = t.balance;
    }
    assert!(checked > 0, "no transaction had a usable prior balance to reconcile against");
    assert_eq!(
        mismatches, 0,
        "{mismatches}/{checked} transactions don't reconcile with (previous balance + credit - debit)"
    );

    // Cheque-number reference extraction (`extract_cosmos_ref`): Cosmos's
    // own "Chq.No." column is this format's real reference field — confirm
    // at least the known cheque-numbered rows in this fixture got it.
    let refs: Vec<&str> = real.iter().map(|t| t.reference.as_str()).collect();
    for expected_ref in ["7311", "7312", "7313", "7314"] {
        assert!(
            refs.contains(&expected_ref),
            "expected a transaction with reference {expected_ref:?}, refs found: {refs:?}"
        );
    }
}

/// **Fourth real bug found by this suite (2026-08-27): the fixture's first
/// transaction — a payment/debit of 316.00 originally narrated
/// "PRCR/303213675227/S R TRADER 13:16" — was imported as a *credit*.
///
/// (Narration/Reference split note, 2026-08-28: this doc comment and the
/// assertions below predate the `extract_cosmos_ref` fix that pulls the
/// UTR segment out into `reference` — narration is now "PRCR/S R TRADER
/// 13:16" with reference "303213675227"; the debit/credit direction bug
/// this test targets is unrelated and unaffected by that fix.)
///
/// This fixture has no "Opening Bal"/"Op Bal" line anywhere in its extracted
/// text (its transaction table starts mid-statement, on page 2 per the
/// statement's own "Page :- 2" header field), so `extract_cosmos_
/// transactions` had no known previous balance to diff the first row's
/// balance movement against, and fell back to guessing direction from a
/// narration-keyword list. That list hard-codes `nl.contains("prcr/")` as a
/// credit indicator, but this statement's real "Withdrawals" column (verified
/// directly against the fixture's extracted text: `316.00` sits at the same
/// character offset as the "Withdrawals" heading, not "Deposits" — a
/// fixed-width table where column position is unambiguous) shows PRCR/ rows
/// can just as well be debits. Root cause fixed by classifying the seed row's
/// direction from **which header column its amount's text actually starts
/// under** (computed once from the header row, since both "Withdrawals" and
/// "Deposits" are required substrings for Cosmos-header detection to even
/// fire) instead of guessing from narration keywords — keywords are now only
/// a fallback for the (here unreachable) case where those column offsets
/// can't be found at all.
///
/// Note this bug survived `cosmos_co_operative_bank_pdf_reconciles_exactly_
/// after_the_quote_operator_extraction_fix` above untouched: that test's
/// reconciliation check compares each transaction's balance against
/// `result.opening_balance`, but `prepend_opening_balance_row` *derives*
/// `opening_balance` from the first real transaction's own (buggy)
/// credit/debit assignment (`ob = balance - (credit - debit)`) whenever no
/// opening-balance line was found in the text — exactly this fixture's case.
/// That derivation is circular: it reconciles by construction no matter which
/// direction the first row was assigned, so it can never catch a first-row
/// direction bug. This test checks the first row's direction and amount
/// directly against the source PDF instead.
#[test]
fn cosmos_first_transaction_is_a_debit_not_a_credit() {
    let path = fixture("Cosmos Co-operative.pdf");
    let result = parse_pdf_via_real_pipeline(&path, "Cosmos Co-operative.pdf");

    let real: Vec<&parser::Transaction> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .collect();
    assert!(!real.is_empty(), "expected at least one real transaction");

    let first = real[0];
    assert_eq!(
        first.narration, "PRCR/S R TRADER 13:16",
        "unexpected first transaction, fixture may have changed: {first:?}"
    );
    assert_eq!(
        first.reference, "303213675227",
        "first transaction's UTR must land in Reference, not stay glued into narration: {first:?}"
    );
    assert_eq!(
        first.debit,
        Some(316.0),
        "first transaction (a payment) must be a Debit of 316.00, got {first:?}"
    );
    assert_eq!(
        first.credit, None,
        "first transaction's Credit must be empty, got {first:?}"
    );

    // The very next few transactions are all real UPI-DR debits — confirm the
    // fix didn't overcorrect and flip everything to debit regardless of
    // actual direction.
    assert_eq!(real[4].narration, "UPI-DR/gpay-112140716");
    assert_eq!(real[4].reference, "303433640023");
    assert_eq!(real[4].debit, Some(260.0));
    assert_eq!(real[4].credit, None);

    // The first UPI-CR (receipt) in the statement must still land as a
    // Credit — confirms receipts weren't swapped to Debit as collateral
    // damage of this fix.
    let first_credit = real
        .iter()
        .find(|t| t.narration.starts_with("UPI-CR/"))
        .expect("expected at least one UPI-CR transaction");
    assert_eq!(first_credit.narration, "UPI-CR/laad.shashikan");
    assert_eq!(first_credit.reference, "303536153436");
    assert_eq!(first_credit.credit, Some(200.0));
    assert_eq!(first_credit.debit, None);
}

/// **Real bug found and fixed (2026-08-28): ICICI Bank Wealth Management
/// PDF import.** This used to be `icici_wealth_management_pdf_extracts_
/// zero_pages`, asserting `extract_pages` returns zero rows and blaming
/// "PDF structural complexity" as an unconfirmed guess. That diagnosis was
/// incomplete: the *actual* root cause, confirmed by decoding this file's
/// page content streams directly, is that every one of its 36 pages has
/// **zero** `Tj`/`TJ`/`'`/`"` text-showing operators anywhere — every
/// character is drawn as vector line-art (`m`/`l`/`c`/`h`/`f` path
/// operators) and `doc.get_page_fonts()` finds zero fonts on every page.
/// There is no embedded text to extract by any means; `extract_pages`
/// returning empty is *correct* behavior given that input, not a bug in
/// `lopdf` usage.
///
/// Fixed by adding a real render+OCR fallback
/// (`ocr_extractor::extract_pages_via_ocr`, rasterizing each page via the
/// `mutool` CLI and reading Tesseract's positional TSV word-boxes back into
/// the same `Vec<Vec<PdfItem>>` shape embedded-text extraction produces)
/// feeding a dedicated column-aware extractor
/// (`transaction_extractor::extract_icici_wealth_transactions`) for this
/// statement's block-structured Date/Mode/Particulars/Deposits/
/// Withdrawals/Balance layout — see both functions' doc comments for the
/// full detail. Wired into `main.rs`'s `run_pdf_ocr_pipeline` ahead of the
/// existing flat-text OCR tiers.
///
/// Requires `mutool` (MuPDF) and `tesseract` on PATH — real external OCR
/// tools, not bundled — and takes ~2 minutes (36 pages, render + OCR each).
/// `#[ignore]`d for normal `cargo test` runs for exactly that reason; run
/// explicitly with `cargo test --ignored icici_wealth_management` on a
/// machine that has both installed (e.g. `winget install
/// ArtifexSoftware.mutool` / `winget install UB-Mannheim.TesseractOCR` on
/// Windows). Skips (not fails) if either tool isn't found, so it can't
/// spuriously break CI.
#[test]
#[ignore = "requires mutool + tesseract on PATH and takes ~2 minutes (36-page render+OCR) — run explicitly: cargo test --ignored icici_wealth_management"]
fn icici_wealth_management_pdf_imports_successfully_via_ocr() {
    let tools_available = std::process::Command::new("mutool")
        .arg("-v")
        .output()
        .is_ok()
        && std::process::Command::new("tesseract")
            .arg("--version")
            .output()
            .is_ok();
    if !tools_available {
        eprintln!(
            "SKIPPED: mutool and/or tesseract not found on PATH — install both to run this test \
             (see doc comment)"
        );
        return;
    }

    let path = fixture("ICICI Bank Wealth management.pdf");
    let rows = parser::ocr_extractor::extract_pages_via_ocr(&path);
    assert!(
        !rows.is_empty(),
        "extract_pages_via_ocr returned zero rows — mutool/tesseract ran but produced nothing"
    );

    let result = pdf_parser::parse_pdf_rows(rows, "ICICI Bank Wealth management.pdf")
        .expect("parse_pdf_rows returned None for OCR'd ICICI Wealth Management rows");

    assert_eq!(result.bank_name, "ICICI Bank Wealth Management");
    assert_eq!(
        result.account_no, "059501505351",
        "account number must be extracted from \"Savings A/c 059501505351\", not left empty"
    );

    let real: Vec<&parser::Transaction> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .collect();
    // Cross-checked directly against the rendered PDF pages: 36 pages of a
    // real UPI/NEFT/RTGS/IMPS-heavy statement land in the 700–900 range: an
    // exact count is too brittle against OCR run-to-run word-recognition
    // variance (whether a stray character misreads as a spurious extra
    // digit run, etc.) to assert precisely.
    assert!(
        real.len() > 700,
        "expected at least 700 real transactions from a 36-page statement, got {}",
        real.len()
    );

    for t in &real {
        assert!(
            !(t.debit.is_some() && t.credit.is_some()),
            "transaction has BOTH debit and credit set (Debit/Credit must never mix): {t:?}"
        );
        assert!(
            t.debit.is_some() || t.credit.is_some(),
            "transaction has NEITHER debit nor credit set: {t:?}"
        );
    }

    // Full balance-continuity reconciliation. Not required to be perfect —
    // this account's "Rev Sweep" auto fund-transfer rows (linked-FD
    // overdraft coverage) have a structure this extractor doesn't fully
    // model — but the overwhelming majority of rows must reconcile exactly
    // against the statement's own printed running balance, or the
    // Debit/Credit/Balance extraction has regressed.
    let mut prev_balance = result.opening_balance;
    let mut mismatches = 0usize;
    let mut checked = 0usize;
    for t in &real {
        if let (Some(pb), Some(bal)) = (prev_balance, t.balance) {
            checked += 1;
            let expected = pb + t.credit.unwrap_or(0.0) - t.debit.unwrap_or(0.0);
            if (expected - bal).abs() > 0.01 {
                mismatches += 1;
            }
        }
        prev_balance = t.balance;
    }
    assert!(checked > 0, "no transaction had a usable prior balance to reconcile against");
    let mismatch_rate = mismatches as f64 / checked as f64;
    assert!(
        mismatch_rate < 0.05,
        "{mismatches}/{checked} ({:.1}%) transactions don't reconcile with (previous balance + \
         credit - debit) — expected under 5%",
        mismatch_rate * 100.0
    );

    // Reference (UPI RRN / UTR) extraction: most rows are UPI/IMPS, whose
    // slash-delimited reference segment this format's extractor is built
    // to pull out — confirm it's actually populated for the majority,
    // not uniformly empty.
    let with_reference = real.iter().filter(|t| !t.reference.is_empty()).count();
    assert!(
        with_reference as f64 / real.len() as f64 > 0.6,
        "expected over 60% of real transactions to have a non-empty Reference, got {}/{}",
        with_reference,
        real.len()
    );
}

/// **Real bug found and fixed (2026-08-29): normal ICICI Bank PDF import.**
/// This is a completely different root cause from the Wealth Management
/// fixture above, despite both being "ICICI Bank" statements: this file's
/// embedded text is perfectly fine and complete, but `text_extractor::
/// extract_page_text` only breaks a line on the `'`/`"`/`T*`/`ET`
/// content-stream operators (see that module's doc comment) — this PDF's
/// generator moves the text cursor between visual lines with bare
/// `Td`/`Tm` operators instead, which are silently ignored, so the entire
/// transaction table collapses into one continuous run of text with fields
/// occasionally glued together with zero delimiter (confirmed verbatim in
/// this fixture: two adjacent 2-decimal amounts concatenated as
/// `"58598.29378368.15"`). This is unrecoverable at the flat-text layer —
/// see `pdf_fixtures_with_a_squashed_single_line_table_extract_far_fewer_
/// transactions_than_the_real_statement_contains` above, whose ICICI
/// assertion (Stage 2 flat-text finds zero transactions) remains true and
/// intentionally unchanged.
///
/// Fixed the same way the Wealth Management fixture was: `main.rs`'s
/// `run_pdf_ocr_pipeline` Tier 0 renders each page to an image and reads it
/// back with Tesseract (`ocr_extractor::extract_pages_via_ocr`), recovering
/// genuine per-word X positions regardless of how the embedded text layer
/// glued things together, feeding a dedicated extractor
/// (`transaction_extractor::extract_icici_normal_transactions`) for this
/// statement's `Sl No | Tran Id | Value/Transaction/Posted Date | Cheque no
/// / Ref No | Transaction Remarks | Withdrawal (Dr) | Deposit (Cr) |
/// Balance` layout, whose header itself wraps across 2-3 physical OCR rows.
/// See that function's doc comment for the two further OCR artifacts it
/// specifically survives (a border-line glyph glued onto an amount, and an
/// amount split across two physical rows).
///
/// This fixture is small (8 real transactions, cross-checked directly
/// against the rendered PDF's own printed "Page Total" summary), so unlike
/// the Wealth Management test above this asserts an *exact* transaction
/// count and *exact* reconciliation (zero tolerance) rather than a
/// statistical threshold.
#[test]
#[ignore = "requires mutool + tesseract on PATH and takes ~15 seconds (2-page render+OCR) — run explicitly: cargo test --ignored icici_bank_normal"]
fn icici_bank_normal_pdf_imports_successfully_via_ocr() {
    let tools_available = std::process::Command::new("mutool").arg("-v").output().is_ok()
        && std::process::Command::new("tesseract").arg("--version").output().is_ok();
    if !tools_available {
        eprintln!(
            "SKIPPED: mutool and/or tesseract not found on PATH — install both to run this test \
             (see doc comment)"
        );
        return;
    }

    let path = fixture("ICICI Bank.pdf");
    let rows = parser::ocr_extractor::extract_pages_via_ocr(&path);
    assert!(
        !rows.is_empty(),
        "extract_pages_via_ocr returned zero rows — mutool/tesseract ran but produced nothing"
    );

    let result = pdf_parser::parse_pdf_rows(rows, "ICICI Bank.pdf")
        .expect("parse_pdf_rows returned None for OCR'd ICICI Bank rows");

    assert_eq!(result.bank_name, "ICICI Bank");

    let real: Vec<&parser::Transaction> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .collect();
    assert_eq!(
        real.len(),
        8,
        "expected exactly 8 real transactions (cross-checked against the rendered PDF's own \
         table), got {}",
        real.len()
    );

    for t in &real {
        assert!(
            !(t.debit.is_some() && t.credit.is_some()),
            "transaction has BOTH debit and credit set (Debit/Credit must never mix): {t:?}"
        );
        assert!(
            t.debit.is_some() || t.credit.is_some(),
            "transaction has NEITHER debit nor credit set: {t:?}"
        );
        assert!(
            !t.date.is_empty(),
            "transaction with empty date: {t:?}"
        );
    }

    // Exact reconciliation — this fixture's own printed "Page Total"
    // ("Opening Bal: 23,856.92", "Withdrawls: 1,59,800.65", "Deposits:
    // 1,62,000.00", "Closing Bal: 26,056.27") gives ground truth to check
    // against exactly, not just statistically.
    assert_eq!(result.opening_balance, Some(23_856.92));
    let total_withdrawals: f64 = real.iter().filter_map(|t| t.debit).sum();
    let total_deposits: f64 = real.iter().filter_map(|t| t.credit).sum();
    assert!(
        (total_withdrawals - 159_800.65).abs() < 0.01,
        "total withdrawals {total_withdrawals:.2} != printed 1,59,800.65"
    );
    assert!(
        (total_deposits - 162_000.00).abs() < 0.01,
        "total deposits {total_deposits:.2} != printed 1,62,000.00"
    );
    let closing_balance = real.last().and_then(|t| t.balance);
    assert_eq!(
        closing_balance,
        Some(26_056.27),
        "last transaction's balance must equal the printed Closing Bal"
    );

    let mut prev_balance = result.opening_balance;
    for t in &real {
        if let (Some(pb), Some(bal)) = (prev_balance, t.balance) {
            let expected = pb + t.credit.unwrap_or(0.0) - t.debit.unwrap_or(0.0);
            assert!(
                (expected - bal).abs() < 0.01,
                "balance doesn't reconcile: prev={pb:.2} + credit={:?} - debit={:?} = {expected:.2}, \
                 but stated balance={bal:.2} for {t:?}",
                t.credit,
                t.debit
            );
        }
        prev_balance = t.balance;
    }

    // Redacted account-holder header (see `extract_icici_normal_
    // transactions`'s doc comment): account_no is deliberately left empty
    // rather than dug out from behind the black-box redaction, which masks
    // to bare "XXXX" in the UI — the documented "unavailable" fallback.
    assert_eq!(
        result.account_no, "",
        "account number must stay empty (redacted in the source PDF), not be reconstructed from \
         text hidden behind the redaction"
    );
}

/// Locks in the "IDBI Bank.pdf" page-1 fix (2026-08-29) end-to-end against
/// the real fixture, the same way `icici_bank_normal_pdf_imports_
/// successfully_via_ocr` does for ICICI Bank.pdf immediately above — same
/// root cause (bare `Td`/`Tm` positioning collapses page 1's ~20-row table
/// into one flat text line with no row/field delimiters), same fix (Tier 0
/// render+OCR via `ocr_extractor::extract_pages_via_ocr`, fed into a
/// dedicated `transaction_extractor::extract_idbi_transactions`).
///
/// Page 2 of this statement happens to render one-field-per-line already, so
/// its 4 "recent transactions" were always recoverable at the flat-text
/// layer (see the previous test's doc comment) — this is exactly why only 4
/// transactions were ever visible before this fix, not a pagination,
/// header-detection, or opening-balance bug as originally suspected. OCR
/// recovers page 1 and page 2 uniformly through the same code path, so this
/// test's real value is confirming BOTH pages land in one continuous,
/// non-duplicated 24-transaction list.
///
/// Ground truth is the fixture's own printed Statement Summary ("Dr Count
/// 7", "Cr Count 17", "Dr Amount 905024.70", "Cr Amount 736050.90"), cross-
/// checked row-for-row against both rendered pages directly (not just via
/// this crate's own OCR round-trip — see `extract_idbi_transactions`'s doc
/// comment for the three real bugs that first round of "it reconciles"
/// checking missed entirely: two OCR-dropped leading balance digits masked
/// by a coincidentally-still-plausible chain, one narration-continuation
/// digit run wrongly captured as a Cheque No reference, and a mislabeled
/// Opening/Closing Balance from returning the array in the statement's own
/// newest-first order instead of chronological). Every one of the 24 real
/// transactions' Date/Narration/Reference/Debit/Credit/Balance was verified
/// this way; this test locks in both the aggregate counts and (for the
/// first and last transactions specifically) the exact field values.
///
/// Debit and credit *counts* are asserted exactly (7/17). The *totals* are
/// asserted within a small tolerance (a few paise) rather than exactly: the
/// bank's own printed summary total itself differs from the sum of its own
/// individually-printed transaction amounts by 2-4 paise (confirmed by hand
/// on the rendered PDF — a rounding artifact in IDBI's own statement
/// generator, not an extraction bug on this side).
#[test]
#[ignore = "requires mutool + tesseract on PATH and takes ~15 seconds (2-page render+OCR) — run explicitly: cargo test --ignored idbi_bank_pdf"]
fn idbi_bank_pdf_imports_successfully_via_ocr() {
    let tools_available = std::process::Command::new("mutool").arg("-v").output().is_ok()
        && std::process::Command::new("tesseract").arg("--version").output().is_ok();
    if !tools_available {
        eprintln!(
            "SKIPPED: mutool and/or tesseract not found on PATH — install both to run this test \
             (see doc comment)"
        );
        return;
    }

    let path = fixture("IDBI Bank.pdf");
    let rows = parser::ocr_extractor::extract_pages_via_ocr(&path);
    assert!(
        !rows.is_empty(),
        "extract_pages_via_ocr returned zero rows — mutool/tesseract ran but produced nothing"
    );

    let result = pdf_parser::parse_pdf_rows(rows, "IDBI Bank.pdf")
        .expect("parse_pdf_rows returned None for OCR'd IDBI Bank rows");

    assert_eq!(result.bank_name, "IDBI Bank");

    let real: Vec<&parser::Transaction> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .collect();
    assert_eq!(
        real.len(),
        24,
        "expected exactly 24 real transactions (bank-reported 'Dr Count 7' + 'Cr Count 17'), \
         got {} — if this is 4, page 1 is being dropped again",
        real.len()
    );

    for t in &real {
        assert!(
            !(t.debit.is_some() && t.credit.is_some()),
            "transaction has BOTH debit and credit set (Debit/Credit must never mix): {t:?}"
        );
        assert!(
            t.debit.is_some() || t.credit.is_some(),
            "transaction has NEITHER debit nor credit set: {t:?}"
        );
        assert!(!t.date.is_empty(), "transaction with empty date: {t:?}");
    }

    let debit_count = real.iter().filter(|t| t.debit.is_some()).count();
    let credit_count = real.iter().filter(|t| t.credit.is_some()).count();
    assert_eq!(debit_count, 7, "expected 7 debits per the printed Statement Summary 'Dr Count'");
    assert_eq!(credit_count, 17, "expected 17 credits per the printed Statement Summary 'Cr Count'");

    let total_debit: f64 = real.iter().filter_map(|t| t.debit).sum();
    let total_credit: f64 = real.iter().filter_map(|t| t.credit).sum();
    assert!(
        (total_debit - 905_024.70).abs() < 0.10,
        "total debit {total_debit:.2} != printed Dr Amount 905024.70"
    );
    assert!(
        (total_credit - 736_050.90).abs() < 0.10,
        "total credit {total_credit:.2} != printed Cr Amount 736050.90"
    );

    // The array is chronological (oldest first), NOT the statement's own
    // newest-first print order — required so `opening_balance` below and
    // the Summary Panel's "Closing Balance" (derived downstream from
    // `real.last()`) land on the right ends of the date range. Verified
    // directly against the rendered PDF: the true opening balance is the
    // balance just before Sr24 (01/04/2025), i.e. 201093.74 - 18751.73.
    assert_eq!(
        result.opening_balance,
        Some(182_342.01),
        "opening_balance must be the balance before the OLDEST transaction, not the newest — \
         if this is 368368.15 (or 378368.15), the array is back in the statement's own \
         newest-first order"
    );

    // First transaction after chronological ordering (Sr24, oldest, from
    // page 1's squashed table): date, credit amount, and balance.
    let first = real.first().expect("at least one real transaction");
    assert_eq!(first.date, "01/04/2025");
    assert_eq!(first.credit, Some(18_751.73));
    assert_eq!(first.debit, None);
    assert_eq!(first.balance, Some(201_093.74));
    assert_eq!(first.narration, "NEFT- HDFCN52025040151267045-SMC GLOBAL SECURITIES");

    // Last transaction (Sr1, most recent, from page 2's own normally-laid-
    // out section): date, debit amount, Cheque No used as reference, and
    // balance — this specific balance is the one that locks in the
    // balance-chain repair (Tesseract itself read "{3368.15", a misread "1"
    // stripped as punctuation, leaving an OCR'd value 10000 short of the
    // true 13368.15; the chain recomputes it from Sr2's balance - this
    // debit instead of trusting the single OCR'd number).
    let last = real.last().expect("at least one real transaction");
    assert_eq!(last.date, "20/01/2026");
    assert_eq!(last.debit, Some(365_000.00));
    assert_eq!(last.credit, None);
    assert_eq!(last.reference, "409615");
    assert_eq!(
        last.balance,
        Some(13_368.15),
        "if this is 3368.15, the balance-chain repair (extract_idbi_transactions) regressed"
    );
    assert_eq!(last.narration, "PRAJAKTA RAMKRISHNA");

    // Locks in the Cheque-No-column-width fix: a bare narration-
    // continuation digit run ("202610006", the wrapped tail of "PMSBY
    // Renewal FY2025-202610006") must NOT be captured as if it came from
    // the Cheque No column (which is genuinely blank for this row) — it's
    // real narration text that also happens to look like a UTR reference,
    // same as several other rows below.
    let pmsby = real
        .iter()
        .find(|t| t.narration.contains("PMSBY"))
        .expect("expected the PMSBY Renewal row in the fixture");
    assert_eq!(pmsby.narration, "PMSBY Renewal FY2025- 202610006");
    assert_eq!(pmsby.debit, Some(20.00));

    // Redacted account-holder header (see `extract_idbi_transactions`'s doc
    // comment): account_no is deliberately left empty rather than dug out
    // from behind the black-box redaction, same as ICICI Bank.pdf above.
    assert_eq!(
        result.account_no, "",
        "account number must stay empty (redacted in the source PDF), not be reconstructed from \
         text hidden behind the redaction"
    );
}

/// Locks in the "IDFCFIRSTBankstatement.pdf" Debit/Credit-mixing fix
/// (2026-08-29) end-to-end against the real fixture. Same root cause as
/// ICICI Bank.pdf/IDBI Bank.pdf: every embedded-text item lands at `x=0.0`
/// (confirmed by dumping `text_extractor::extract_pages`'s raw rows), so
/// Stage 1's column-based `parse_pdf_rows` can detect a header here but
/// never populate an actual column from it, and the app fell back to Stage
/// 2's flat-*text* heuristic parser — which has to guess Debit vs Credit
/// from narration/ordering with no real column positions to check against.
/// That guess was wrong for the *first* transaction (a salary NEFT debit of
/// 10,022.00 came out as a Credit) and for a scattered handful of others
/// throughout the statement, exactly the live bug report this test locks in
/// the fix for — not a first-row-only issue, and not fixed by a global
/// Debit/Credit swap (this statement already has 17 genuine Credits mixed
/// among 57 genuine Debits; a blind swap would simply invert which side is
/// wrong).
///
/// Fixed the same way as ICICI Bank.pdf/IDBI Bank.pdf: Tier 0 renders every
/// page and reads real per-word X positions back with Tesseract, and a
/// dedicated `extract_idfc_first_transactions` (`transaction_extractor.rs`)
/// assigns Debit/Credit/Balance by which column an amount's X position
/// actually falls under — never by guessing. See that function's doc
/// comment for two IDFC-specific traps it survives: the per-page repeated
/// "Opening Balance / Total Debit / Total Credit / Closing Balance" summary
/// box, whose OWN "Debit"/"Credit"/"Balance" header words sit at different
/// X positions than the real column header's and would silently poison the
/// column anchors if not explicitly excluded from the anchor scan; and a
/// narration's own reference number getting OCR-split onto the same row as
/// the date/amounts (not just onto a continuation row), which — before the
/// second bug fix here — got swept in as a phantom amount and silently
/// dropped a whole transaction.
///
/// This fixture's own first page is genuinely missing from the 7-page PDF
/// on disk (its footer prints "Page 2 of 8", "Page 7 of 8", etc. — the real
/// statement was 8 pages; this saved fixture starts one page in), so the
/// printed header's Opening Balance (2,63,436.28) does NOT reconcile with
/// this file's own first visible transaction — that gap is a property of
/// the fixture, not a bug here. Because of that, the Total Debit assertion
/// below is a lower-bound / non-exact check, while Total Credit (unaffected
/// — the missing page's own transactions all appear to be debits, matching
/// the pattern visible on every other page) and the Closing Balance both
/// reconcile exactly against the statement's own printed footer, which is
/// possible specifically because `opening_balance` here means "the balance
/// immediately before this fixture's own first transaction", not the whole
/// statement's true opening balance — see `extract_idfc_first_transactions`
/// for why that's the correct contract for a chronological statement.
#[test]
#[ignore = "requires mutool + tesseract on PATH and takes ~15 seconds (7-page render+OCR) — run explicitly: cargo test --ignored idfc_first"]
fn idfc_first_bank_pdf_debit_credit_is_never_mixed_via_ocr() {
    let tools_available = std::process::Command::new("mutool").arg("-v").output().is_ok()
        && std::process::Command::new("tesseract").arg("--version").output().is_ok();
    if !tools_available {
        eprintln!(
            "SKIPPED: mutool and/or tesseract not found on PATH — install both to run this test \
             (see doc comment)"
        );
        return;
    }

    let path = fixture("IDFCFIRSTBankstatement.pdf");
    let rows = parser::ocr_extractor::extract_pages_via_ocr(&path);
    assert!(
        !rows.is_empty(),
        "extract_pages_via_ocr returned zero rows — mutool/tesseract ran but produced nothing"
    );

    let result = pdf_parser::parse_pdf_rows(rows, "IDFCFIRSTBankstatement.pdf")
        .expect("parse_pdf_rows returned None for OCR'd IDFC First Bank rows");

    assert_eq!(result.bank_name, "IDFC First Bank");
    assert_eq!(
        result.account_no, "10158467482",
        "unlike ICICI Normal/IDBI, this statement's account number is printed in the clear, \
         not redacted — it should be extracted, not left blank"
    );

    let real: Vec<&parser::Transaction> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .collect();
    assert!(
        real.len() >= 74,
        "expected at least 74 real transactions (this fixture's own first page is missing, \
         see doc comment, so a handful more may legitimately exist on it), got {}",
        real.len()
    );

    for t in &real {
        assert!(
            !(t.debit.is_some() && t.credit.is_some()),
            "transaction has BOTH debit and credit set (Debit/Credit must never mix): {t:?}"
        );
        assert!(
            t.debit.is_some() || t.credit.is_some(),
            "transaction has NEITHER debit nor credit set: {t:?}"
        );
        assert!(!t.date.is_empty(), "transaction with empty date: {t:?}");
    }

    // The FIRST transaction is the exact live bug report: a salary NEFT
    // debit that was coming out as a Credit.
    let first = real.first().expect("at least one real transaction");
    assert_eq!(first.date, "02/04/2024");
    assert_eq!(first.debit, Some(10_022.00));
    assert_eq!(first.credit, None);
    assert_eq!(first.balance, Some(207_508.28));

    // Last transaction: date, credit amount, and the statement's own
    // printed Closing Balance — reconciles exactly (see doc comment for
    // why this is possible despite the fixture's missing first page).
    let last = real.last().expect("at least one real transaction");
    assert_eq!(last.date, "30/04/2024");
    assert_eq!(last.credit, Some(300_000.00));
    assert_eq!(last.debit, None);
    assert_eq!(
        last.balance,
        Some(307_237.08),
        "must reconcile exactly with the statement's own printed Closing Balance"
    );

    let debit_count = real.iter().filter(|t| t.debit.is_some()).count();
    let credit_count = real.iter().filter(|t| t.credit.is_some()).count();
    let total_debit: f64 = real.iter().filter_map(|t| t.debit).sum();
    let total_credit: f64 = real.iter().filter_map(|t| t.credit).sum();
    assert_eq!(credit_count, 17);
    assert!(
        (total_credit - 1_030_823.80).abs() < 0.10,
        "total credit {total_credit:.2} != printed Total Credit 10,30,823.80 — unlike Total \
         Debit, this isn't affected by the fixture's missing first page (see doc comment)"
    );
    assert!(
        debit_count >= 57,
        "expected at least 57 debits (this fixture's missing first page may hold a few more), \
         got {debit_count}"
    );
    assert!(
        total_debit > 900_000.0,
        "total debit {total_debit:.2} implausibly low — a real Debit column value is being \
         dropped or misclassified somewhere"
    );
}

/// SBI.pdf (2026-08-29) shipped with two independent bugs, both fixed here.
///
/// **Bank detection**: `detect_by_phrase` used a plain, unbounded substring
/// search, so it matched the phrase "hdfc bank" inside a *counterparty's*
/// name glued with no separator by `norm()` — this statement's narration
/// text contains "...HDFC00161000007598HDFCBANKLTD..." (an NEFT/RTGS
/// sender's own bank+account, "HDFC BANK LTD", nothing to do with whose
/// statement this is), which `norm()` collapses into one unbroken run
/// containing "hdfcbankltd". That substring match won HDFC Bank at 0.80
/// confidence via P5 (phrase-in-full-text), which is high enough to block
/// the correct filename-based SBI detection (P6 is capped at 0.65 and only
/// runs when confidence is still < 0.70). Fixed in `bank_detection.rs` by
/// requiring non-alphanumeric boundaries around a phrase match
/// (`find_word_bounded`) — see `detect_counterparty_named_hdfc_bank_ltd_
/// does_not_steal_sbi` in that module's own tests for the regression lock-in
/// and confirmation that real HDFC detection (by IFSC, domain, and header
/// phrase) is unaffected.
///
/// **Debit/Credit extraction**: the same squashed-single-line-table root
/// cause as ICICI/IDBI/IDFC First (every embedded-text item at `x=0.0`)
/// meant this needed its own dedicated Tier-0-OCR extractor,
/// `extract_sbi_transactions`. Getting it right against the real fixture
/// took five separate fixes, each found by comparing extracted output
/// against the actual rendered PDF pages rather than trusting internal
/// self-consistency: (1) a stray narration-continuation fragment landing in
/// the Ref No. column's X range needed a shape filter, not just an X-range
/// boundary; (2) a 2-digit-day date's 4-digit year sometimes wraps onto the
/// *next* physical OCR row entirely, requiring a one-row lookahead
/// reconstructing day+month and year independently (bridging both with one
/// regex across an unpredictable amount of glued narration text — like the
/// real word "WITHDRAWAL" landing with zero separator right after the
/// month — turned out not to be reliably boundable, so this was rebuilt to
/// extract day+month from the current row and the year from the next row's
/// leading 4 digits, each on its own); (3) the Description column's own
/// left edge needed a small margin, not an exact cutoff, because a
/// genuine narration word occasionally rendered a fraction of a point to
/// its left; (4) a per-page footer disclaimer wraps across two physical OCR
/// rows and only the first was being skipped, so the second glued onto
/// whatever transaction happened to precede it (worst case: the statement's
/// very last transaction, corrupting its narration); (5) most seriously, a
/// junk glyph (a border-line misread `|`) glued onto the *second* half of
/// an OCR-split Ref No. fragment ("|4897691", continuing "162095" from the
/// row above) failed the Ref column's digit-only shape check on its raw
/// text, fell through into ordinary row content, and there *did* pass the
/// amount shape check (which already strips junk) — concatenating a stray
/// digit run onto a real Debit amount, corrupting it into an unparseable
/// string and silently dropping the whole transaction. That fifth bug is
/// the one that produced this task's headline symptom set: a dropped
/// 07/05/2024 debit of ₹1,00,000 whose knock-on effect was far worse than
/// one missing row — the balance-chain repair pass (same "trust the running
/// total over a mismatched OCR'd balance" design as IDBI/IDFC First) then
/// treated every subsequent transaction's correctly-OCR'd balance as wrong
/// and overwrote it with a chain computed from the gap, corrupting every
/// balance for the rest of the 11-month statement even though the raw OCR
/// text for most of them was already correct.
///
/// Verified transaction-by-transaction against the actual rendered PDF
/// pages (not just internal self-consistency): the first transaction (the
/// literal bug report — a debit that was coming out as a credit before the
/// column-based extractor replaced flat-text guessing), all 24 transactions
/// on page 6 (Jul 2024), and all 24 transactions on page 11 (Nov 2024 –
/// Feb 2025, including the two ambiguous same-day-same-amount ATM
/// withdrawals whose dates cross a page boundary) match the PDF exactly —
/// date, Debit/Credit side, and Balance, every one. No printed summary
/// totals exist anywhere in this 12-page statement (confirmed: its last
/// page ends directly with the disclaimer, no Total Debit/Credit box), so
/// there is nothing to reconcile against beyond the balance chain itself,
/// which closes with zero unexplained mismatches end to end.
#[test]
#[ignore = "requires mutool + tesseract on PATH and takes ~45 seconds (12-page render+OCR) — run explicitly: cargo test --ignored sbi_bank"]
fn sbi_bank_pdf_is_identified_correctly_and_debit_credit_is_never_mixed_via_ocr() {
    let tools_available = std::process::Command::new("mutool").arg("-v").output().is_ok()
        && std::process::Command::new("tesseract").arg("--version").output().is_ok();
    if !tools_available {
        eprintln!(
            "SKIPPED: mutool and/or tesseract not found on PATH — install both to run this test \
             (see doc comment)"
        );
        return;
    }

    let path = fixture("SBI.pdf");
    let rows = parser::ocr_extractor::extract_pages_via_ocr(&path);
    assert!(
        !rows.is_empty(),
        "extract_pages_via_ocr returned zero rows — mutool/tesseract ran but produced nothing"
    );

    let result = pdf_parser::parse_pdf_rows(rows, "SBI.pdf")
        .expect("parse_pdf_rows returned None for OCR'd SBI rows");

    assert_eq!(
        result.bank_name, "State Bank of India",
        "must never be HDFC Bank — see doc comment for the counterparty-name false-positive this \
         locks in"
    );

    let real: Vec<&parser::Transaction> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .collect();
    assert_eq!(real.len(), 265, "expected exactly 265 real transactions");

    for t in &real {
        assert!(
            !(t.debit.is_some() && t.credit.is_some()),
            "transaction has BOTH debit and credit set (Debit/Credit must never mix): {t:?}"
        );
        assert!(
            t.debit.is_some() || t.credit.is_some(),
            "transaction has NEITHER debit nor credit set: {t:?}"
        );
        assert!(!t.date.is_empty(), "transaction with empty date: {t:?}");
    }

    // The FIRST transaction is the exact live bug report: a UPI debit that
    // was coming out as a Credit before the column-based extractor replaced
    // flat-text guessing.
    let first = real.first().expect("at least one real transaction");
    assert_eq!(first.date, "04/05/2024");
    assert_eq!(first.debit, Some(30.0));
    assert_eq!(first.credit, None);
    assert_eq!(first.balance, Some(103_051.56));

    // Last transaction: date, debit amount, and Balance — reconciles
    // exactly with the statement's own last printed row (this statement has
    // no separate printed Closing Balance summary box).
    let last = real.last().expect("at least one real transaction");
    assert_eq!(last.date, "28/03/2025");
    assert_eq!(last.debit, Some(100_000.00));
    assert_eq!(last.credit, None);
    assert_eq!(last.balance, Some(15_827.25));

    let debit_count = real.iter().filter(|t| t.debit.is_some()).count();
    let credit_count = real.iter().filter(|t| t.credit.is_some()).count();
    let total_debit: f64 = real.iter().filter_map(|t| t.debit).sum();
    let total_credit: f64 = real.iter().filter_map(|t| t.credit).sum();
    assert_eq!(debit_count, 230);
    assert_eq!(credit_count, 35);
    assert!(
        (total_debit - 1_643_490.97).abs() < 0.10,
        "total debit {total_debit:.2} != expected 16,43,490.97"
    );
    assert!(
        (total_credit - 1_556_236.66).abs() < 0.10,
        "total credit {total_credit:.2} != expected 15,56,236.66"
    );
}

/// Union Bank.pdf (2026-08-30) shipped with two independent bugs, both
/// fixed here, plus a third defect in the shared OCR pipeline itself that
/// this fixture is what exposed.
///
/// **Bank detection**: the actual statement was detected as "Saraswat
/// Co-op Bank". Root cause: `detect_by_phrase`'s P5 tier (phrase match
/// anywhere in the full document text, capped 0.80) found "scbl" — a
/// counterparty's bank code embedded in an ordinary UPI narration
/// ("UPIAB/.../CR/MRRAJES/SCBL/9773690640-2@y", the *other party's* bank
/// in a peer-to-peer transfer, nothing to do with whose statement this is)
/// — and that confidence was high enough to suppress the correct
/// filename-based "Union Bank of India" detection (P6, capped 0.65). This
/// wasn't a word-boundary problem like the SBI "hdfcbankltd" bug fixed
/// earlier — "SCBL" already sits inside clean `/.../` delimiters — it's
/// that a counterparty's bank code embedded in *any* correctly-delimited
/// UPI/NEFT/IMPS/RTGS/ECS/NACH/ACH/POS reference is still not evidence
/// about the statement's own bank at all. Fixed in `bank_detection.rs` by
/// stripping every such transaction-reference-shaped span out of the text
/// before P5 ever scans it (`strip_transaction_references`) — a genuine
/// header/branding phrase is never itself preceded by one of those
/// payment-rail prefixes, so this can only ever remove narration noise,
/// never a real match. See `detect_union_bank_not_saraswat_via_narration_
/// counterparty_code` in `bank_detection.rs`'s own tests for the
/// regression lock-in, and confirmation that Saraswat Co-op Bank's own
/// real header phrase still detects correctly.
///
/// **Debit/Credit extraction**: the same squashed-single-line-table root
/// cause as ICICI/IDBI/IDFC First/SBI, needing its own dedicated
/// `extract_union_bank_transactions`. This fixture is uniquely hard among
/// all of them: no header row survives anywhere in its 79 pages (the one
/// page that would have carried "Debit"/"Credit"/"Balance" header text is
/// missing, and — unlike IDFC First — continuation pages never repeat it),
/// so column anchors have to be *derived from the printed amounts
/// themselves* via frequency clustering rather than read from labels; see
/// that function's doc comment for the two refinements that took (a
/// narration-position floor and wide bins to survive right-alignment
/// digit-width spread) and for the reference-column-prefix, bare-digit,
/// and trailing-dot narration traps found and fixed along the way.
///
/// **Shared OCR pipeline**: building this extractor also exposed a defect
/// in `ocr_extractor` itself, unrelated to any one bank — Tesseract's
/// automatic page-layout analysis silently loses an entire wide text
/// region on a page whose real content is a small block followed by a
/// disproportionately large blank area (this fixture's own true final
/// page, six transaction rows above a mostly-empty page), regardless of
/// `--psm`/`--oem`/DPI. Fixed generally in `ocr_extractor::crop_trailing_
/// blank_space`, applied to every rendered page for every bank — see that
/// function's doc comment; it can only ever remove pixels already
/// confirmed blank, so it's a no-op for any normally-populated page.
///
/// Verified transaction-by-transaction against the real rendered pages —
/// not just internal self-consistency — across a wide sample spanning the
/// full statement (pages 1, 6, 10, 11, 15, 20, 30, 55, 65, 71-79): the
/// first transaction (a Debit that was coming out as a Credit before this
/// fix), dozens of transactions from the early, middle, and final pages,
/// every Debit and every Credit example checked, and one specific fully-
/// OCR-missed transaction (a genuine "600.00" Credit with literally no
/// trace of the amount anywhere in Tesseract's output, recovered via the
/// balance-movement-recovery fallback in `extract_union_bank_transactions`
/// — see that function's own doc comment for why this is not the
/// balance-movement *column-choice* inference this whole extractor is
/// otherwise built to avoid). All match exactly.
///
/// **Single-digit OCR misread in an amount, not just a Balance** (found and
/// fixed 2026-08-30, after the three defects above): a real "2,560.00"
/// Debit (12/03/2025, "Amazon I") was read by Tesseract as "2,060.00" — a
/// genuine single-glyph misrecognition ("5" -> "0"), not a parsing bug —
/// confirmed directly against the rendered page. Left as printed, this
/// silently offset every following Balance, and the running total_debit,
/// by exactly 500.00 for the rest of the statement, because the balance-
/// chain repair below only knew how to overwrite a mismatching *Balance*
/// from the chain, which just propagates a broken *amount* forward instead
/// of fixing it at the source. Fixed generically via
/// `recover_single_digit_amount_misread`: when a row's amount and Balance
/// disagree with the chain, and the amount implied by the Balance movement
/// differs from the OCR'd amount in exactly one low-order digit (under
/// ₹1,000 — see that function's doc comment for why a same-shape match in a
/// *high*-order digit turned out to need rejecting, found via a real false
/// positive elsewhere in this same fixture: a large transfer whose Balance,
/// not its amount, was what OCR had actually misread), the amount is
/// corrected instead of the Balance. No amount, digit, or transaction is
/// referenced by name anywhere in that logic. Debit/Credit total and every
/// Balance now reconcile exactly against the statement's true printed
/// values, including this row.
#[test]
#[ignore = "requires mutool + tesseract on PATH and takes ~5-6 minutes (79-page render+OCR) — run explicitly: cargo test --ignored union_bank"]
fn union_bank_pdf_debit_credit_is_never_mixed_via_ocr() {
    let tools_available = std::process::Command::new("mutool")
        .arg("-v")
        .output()
        .is_ok()
        && std::process::Command::new("tesseract")
            .arg("--version")
            .output()
            .is_ok();
    if !tools_available {
        eprintln!(
            "SKIPPED: mutool and/or tesseract not found on PATH — install both to run this test \
             (see doc comment)"
        );
        return;
    }

    let path = fixture("Union Bank.pdf");
    let rows = parser::ocr_extractor::extract_pages_via_ocr(&path);
    assert!(
        !rows.is_empty(),
        "extract_pages_via_ocr returned zero rows — mutool/tesseract ran but produced nothing"
    );

    let result = pdf_parser::parse_pdf_rows(rows, "Union Bank.pdf")
        .expect("parse_pdf_rows returned None for OCR'd Union Bank rows");

    assert_eq!(
        result.bank_name, "Union Bank of India",
        "must never be Saraswat Co-op Bank — see doc comment for the counterparty-narration-code \
         false positive this locks in"
    );

    let real: Vec<&parser::Transaction> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .collect();
    assert_eq!(real.len(), 1447, "expected exactly 1447 real transactions");

    for t in &real {
        assert!(
            !(t.debit.is_some() && t.credit.is_some()),
            "transaction has BOTH debit and credit set (Debit/Credit must never mix): {t:?}"
        );
        assert!(
            t.debit.is_some() || t.credit.is_some(),
            "transaction has NEITHER debit nor credit set: {t:?}"
        );
        assert!(!t.date.is_empty(), "transaction with empty date: {t:?}");
    }

    // The FIRST transaction is the exact live bug report: a UPI debit that
    // was coming out as a Credit before the column-based extractor replaced
    // flat-text guessing.
    let first = real.first().expect("at least one real transaction");
    assert_eq!(first.date, "04/04/2024");
    assert_eq!(first.debit, Some(30.0));
    assert_eq!(first.credit, None);
    assert_eq!(first.balance, Some(86_355.35));

    // Last transaction: date, Debit amount, and Balance all reconcile
    // exactly with the statement's own last printed row — including the
    // single-digit-misread correction earlier in the statement flowing
    // all the way through to the closing balance with zero residual gap.
    let last = real.last().expect("at least one real transaction");
    assert_eq!(last.date, "01/04/2025");
    assert_eq!(last.debit, Some(1_027.00));
    assert_eq!(last.credit, None);
    assert_eq!(last.balance, Some(76_957.29));

    let debit_count = real.iter().filter(|t| t.debit.is_some()).count();
    let credit_count = real.iter().filter(|t| t.credit.is_some()).count();
    let total_debit: f64 = real.iter().filter_map(|t| t.debit).sum();
    let total_credit: f64 = real.iter().filter_map(|t| t.credit).sum();
    assert_eq!(debit_count, 1182);
    assert_eq!(credit_count, 265);
    assert!(
        (total_debit - 6_326_950.50).abs() < 0.10,
        "total debit {total_debit:.2} != expected 63,26,950.50 (exact reconciliation with the \
         statement's true printed total, including the recovered 2,560.00 Debit)"
    );
    assert!(
        (total_credit - 6_317_522.44).abs() < 0.10,
        "total credit {total_credit:.2} != expected 63,17,522.44"
    );

    // Opening balance (prepended as the synthetic is_opening_balance row)
    // must reconcile exactly with the closing balance via the debit/credit
    // totals above: opening + total_credit - total_debit == closing.
    let opening = result
        .transactions
        .iter()
        .find(|t| t.is_opening_balance)
        .and_then(|t| t.balance)
        .expect("opening balance row with a balance");
    let closing = last.balance.expect("last transaction has a balance");
    assert!(
        (opening + total_credit - total_debit - closing).abs() < 0.10,
        "opening ({opening:.2}) + credit ({total_credit:.2}) - debit ({total_debit:.2}) != \
         closing ({closing:.2})"
    );

    // No duplicate transactions: same date + narration + debit + credit +
    // balance appearing more than once would mean a row got double-counted
    // somewhere in extraction.
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    for t in &real {
        let key = (
            t.date.as_str(),
            t.narration.as_str(),
            t.debit.map(|v| (v * 100.0).round() as i64),
            t.credit.map(|v| (v * 100.0).round() as i64),
            t.balance.map(|v| (v * 100.0).round() as i64),
        );
        assert!(seen.insert(key), "duplicate transaction detected: {t:?}");
    }
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
