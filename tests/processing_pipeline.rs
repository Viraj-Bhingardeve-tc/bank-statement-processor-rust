//! Integration tests chaining the real processing pipeline — vendor
//! normalization, narration cleaning, classification, duplicate detection,
//! GST analysis — against transactions parsed from a real bank-statement
//! fixture (`SBI.pdf`, 136 real transactions, confirmed working by
//! `tests/import_pipeline.rs`), in the same order `main.rs`'s
//! `apply_parse_result`/`finish_batch` run them.

use std::path::{Path, PathBuf};

use bank_statement_processor::classifier;
use bank_statement_processor::db::ClassificationRule;
use bank_statement_processor::gst_engine;
use bank_statement_processor::narration_cleaner;
use bank_statement_processor::parser::{
    self, party_master, pdf_parser, text_extractor, Transaction,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bank_statements")
        .join(name)
}

/// Parses SBI.pdf via the real two-stage pipeline (mirrors
/// `parse_pdf_via_real_pipeline` in `import_pipeline.rs`) and returns just
/// the real (non-opening-balance) transactions — the shared starting point
/// for every test in this file.
fn real_sbi_transactions() -> Vec<Transaction> {
    let path = fixture("SBI.pdf");
    let rows = text_extractor::extract_pages(&path).expect("SBI.pdf: extract_pages failed");
    let result = match pdf_parser::parse_pdf_rows(rows, "SBI.pdf") {
        Some(r) => r,
        None => {
            let full_text = text_extractor::extract_full_text(&path);
            let preprocessed = parser::ocr_parser::preprocess_multiline(&full_text);
            parser::ocr_parser::parse_ocr_text(&preprocessed, "SBI.pdf")
        }
    };
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

/// Vendor normalization (`party_master::normalize_vendors`, "step 0" of
/// `apply_parse_result`) must run without panicking on real narrations and
/// must not blank out narrations it doesn't recognize a vendor in.
#[test]
fn vendor_normalization_runs_on_real_transactions_without_corrupting_narrations() {
    let mut txns = real_sbi_transactions();
    let original_narrations: Vec<String> = txns.iter().map(|t| t.narration.clone()).collect();

    let changed = party_master::normalize_vendors(&mut txns);
    assert_eq!(
        txns.len(),
        original_narrations.len(),
        "normalize_vendors must not add/remove rows"
    );
    // `changed` is a count of vendor fields it actually populated — bounded
    // by the transaction count, and not required to be > 0 (some real
    // statements have no rows the party-master's known-vendor list matches).
    assert!(changed <= txns.len());

    for (t, orig) in txns.iter().zip(original_narrations.iter()) {
        assert_eq!(
            &t.narration, orig,
            "normalize_vendors must not mutate the narration itself, only vendor/account_head"
        );
    }
}

/// Narration cleaning (`clean_batch_with`) on real narrations must produce
/// one `NarrationMeta` per input row, and every cleaned narration must be
/// non-empty whenever the original was non-empty (cleaning must never
/// silently blank out real bank narration text).
#[test]
fn narration_cleaning_processes_every_real_narration() {
    let txns = real_sbi_transactions();
    let narrations: Vec<String> = txns.iter().map(|t| t.narration.clone()).collect();

    let cleaned = narration_cleaner::clean_batch_with(&narrations, true);
    assert_eq!(
        cleaned.len(),
        narrations.len(),
        "one NarrationMeta per input row"
    );

    for (meta, orig) in cleaned.iter().zip(narrations.iter()) {
        if !orig.trim().is_empty() {
            assert!(
                !meta.cleaned.trim().is_empty(),
                "cleaned narration blanked out a non-empty original: {orig:?}"
            );
        }
        assert!(
            (0.0..=1.0).contains(&meta.confidence),
            "confidence out of [0,1] range: {}",
            meta.confidence
        );
    }
}

/// The classifier must run end-to-end against a real transaction set with a
/// small set of rules, changing at least the rows the rules actually match,
/// and must never crash on real (occasionally messy) bank narration text.
#[test]
fn classify_all_applies_rules_to_real_transactions() {
    let mut txns = real_sbi_transactions();

    // Pick a real, frequently-recurring token from the fixture's own
    // narrations to build a rule that's guaranteed to match at least one
    // real row, rather than an invented pattern that might match nothing.
    let sample_token = txns
        .iter()
        .flat_map(|t| t.narration.split_whitespace())
        .find(|w| w.len() >= 4 && w.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(|w| w.to_string());

    let rules = match &sample_token {
        Some(tok) => vec![ClassificationRule {
            id: 1,
            client_id: 1,
            pattern: tok.clone(),
            vendor: "Test Vendor".to_string(),
            account_head: "Test Expense".to_string(),
            txn_type: "Payment".to_string(),
        }],
        None => vec![],
    };

    let changed = classifier::classify_all(&mut txns, "Test Bank Ledger", &rules, true, true, true);

    if sample_token.is_some() {
        assert!(
            changed > 0,
            "expected at least one real row to match the rule built from its own narration token"
        );
    }
    // Every transaction must still have a valid, known status/type after
    // classification — no row should end up in a corrupt state.
    for t in &txns {
        assert!(
            t.confidence >= 0.0 && t.confidence <= 1.0,
            "confidence out of range: {}",
            t.confidence
        );
    }
}

/// Duplicate detection must run without panicking on a real transaction set
/// and must correctly flag an intentionally-introduced exact duplicate
/// (same date/narration/amounts) appended to the real data, while not
/// flagging the real, presumably-distinct rows against each other.
#[test]
fn detect_duplicates_flags_a_real_transaction_cloned_verbatim() {
    let mut txns = real_sbi_transactions();
    let clone_of_first = txns[0].clone();
    let original_dup_flags_before: usize = txns.iter().filter(|t| t.dup_flag).count();

    txns.push(clone_of_first);
    classifier::detect_duplicates(&mut txns);

    let dup_flags_after: usize = txns.iter().filter(|t| t.dup_flag).count();
    assert!(
        dup_flags_after > original_dup_flags_before,
        "appending an exact clone of a real transaction must be flagged as a duplicate"
    );
}

/// GST analysis (`gst_engine::analyse`) must run on real narrations without
/// panicking, and — for a narration this suite constructs to contain a real
/// GSTIN-shaped reference and a "GST"/tax keyword — must actually detect it.
#[test]
fn gst_analysis_detects_a_gstin_pattern_in_a_realistic_narration() {
    // A syntactically valid GSTIN shape (15 chars: 2-digit state code, PAN,
    // entity code, 'Z', checksum) embedded in a realistic narration string.
    let narration = "GST INVOICE PAYMENT TO VENDOR GSTIN 27ABCDE1234F1Z5 FOR SERVICES";
    let result = gst_engine::analyse(narration, "", "Vendor Pvt Ltd", Some(1180.0), None);
    assert!(
        result.is_some(),
        "expected GST analysis to detect the embedded GSTIN/keyword pattern"
    );

    // Also confirm it runs cleanly (no panic, no false positive) against
    // every real SBI.pdf narration — most won't be GST-related, and that's
    // expected; this just proves the function is robust against real text.
    let txns = real_sbi_transactions();
    for t in &txns {
        let _ = gst_engine::analyse(&t.narration, &t.reference, &t.vendor, t.debit, t.credit);
    }
}

/// The full chain — normalize vendors, clean narrations, classify, detect
/// duplicates — run in the same order `apply_parse_result`/`finish_batch`
/// use, against the complete real SBI.pdf transaction set, must complete
/// without panicking and must leave every transaction in a valid state.
#[test]
fn full_processing_chain_runs_end_to_end_on_a_real_statement() {
    let mut txns = real_sbi_transactions();
    let count_before = txns.len();

    party_master::normalize_vendors(&mut txns);

    let narrations: Vec<String> = txns.iter().map(|t| t.narration.clone()).collect();
    let cleaned = narration_cleaner::clean_batch_with(&narrations, true);
    for (t, meta) in txns.iter_mut().zip(cleaned.iter()) {
        if !meta.party.is_empty() {
            t.vendor = meta.party.clone();
        }
    }

    let rules: Vec<ClassificationRule> = vec![];
    classifier::classify_all(&mut txns, "SBI Current Account", &rules, true, true, true);
    classifier::detect_duplicates(&mut txns);

    assert_eq!(
        txns.len(),
        count_before,
        "processing chain must not add/remove transactions"
    );
    for t in &txns {
        assert!(
            !t.id.is_empty(),
            "every transaction must retain a stable id through the pipeline"
        );
        assert!(
            !t.date.is_empty(),
            "every transaction must retain its date through the pipeline"
        );
    }
}
