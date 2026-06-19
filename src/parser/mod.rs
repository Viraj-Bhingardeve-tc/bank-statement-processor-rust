//! Parser module — port of parser.js (Excel + PDF extraction pipeline).
//!
//! Port plan (feature-by-feature):
//!   Phase 3a  amount_parser  — `_parseAmt()` + `fmtAmt()`
//!   Phase 3b  date_parser    — `normalizeTransactionDate()` + `_parseDate()`
//!   Phase 3c  col_detector   — `_detectExcelCols()` / `_findPDFHeader()`
//!   Phase 3d  noise_filter   — `_isNoiseRow()`
//!   Phase 3e  excel_parser   — `_extractSheet()` / full Excel pipeline
//!   Phase 3f  pdf_parser     — `_parsePDFRows()` / PDF pipeline
//!
//! Sub-modules active in this phase:
pub mod amount_parser;
pub mod bank_detection;
pub mod column_detector;
pub mod date_parser;
pub mod excel_parser;
pub mod narration_cleaner;
pub mod noise_filter;
pub mod ocr_extractor;
pub mod ocr_parser;
pub mod party_master;
pub mod pdf_parser;
pub mod row_builder;
pub mod text_extractor;
pub mod transaction_extractor;

use serde::{Deserialize, Serialize};

// ── Transaction status ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TransactionStatus {
    #[default]
    Unreviewed,
    Classified,
    Manual,
    Suspense,
    NeedsReview,
}

impl std::fmt::Display for TransactionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransactionStatus::Unreviewed  => write!(f, "unreviewed"),
            TransactionStatus::Classified  => write!(f, "classified"),
            TransactionStatus::Manual      => write!(f, "manual"),
            TransactionStatus::Suspense    => write!(f, "suspense"),
            TransactionStatus::NeedsReview => write!(f, "needs_review"),
        }
    }
}

// ── Voucher type ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VoucherType {
    #[default]
    Unknown,
    Payment,
    Receipt,
    Contra,
    Journal,
    Sales,
    Purchase,
}

impl std::fmt::Display for VoucherType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VoucherType::Unknown  => write!(f, ""),
            VoucherType::Payment  => write!(f, "Payment"),
            VoucherType::Receipt  => write!(f, "Receipt"),
            VoucherType::Contra   => write!(f, "Contra"),
            VoucherType::Journal  => write!(f, "Journal"),
            VoucherType::Sales    => write!(f, "Sales"),
            VoucherType::Purchase => write!(f, "Purchase"),
        }
    }
}

// ── Column map ────────────────────────────────────────────────────────────────

/// Column indices found in an Excel header row or PDF layout scan.
/// `-1` means the column was not detected.
#[derive(Debug, Clone)]
pub struct ColumnMap {
    pub date:         i32,
    pub narration:    i32,
    pub reference:    i32,
    pub debit:        i32,
    pub credit:       i32,
    pub balance:      i32,
    /// Compound "DEBIT/CREDIT" signed column (Kotak-style). -1 if absent.
    pub debit_credit: i32,
}

impl Default for ColumnMap {
    fn default() -> Self {
        ColumnMap {
            date: -1, narration: -1, reference: -1,
            debit: -1, credit: -1, balance: -1, debit_credit: -1,
        }
    }
}

impl ColumnMap {
    pub fn has_date(&self)   -> bool { self.date >= 0 }
    pub fn has_amount(&self) -> bool {
        self.debit >= 0 || self.credit >= 0 || self.debit_credit >= 0
    }
    pub fn is_usable(&self)  -> bool { self.has_date() && self.has_amount() }
}

// ── Transaction ───────────────────────────────────────────────────────────────

/// A single bank transaction — mirrors the JS `Transaction` object exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub id: String,
    pub import_id: Option<i64>,

    /// Display date in "DD/MM/YYYY" format.
    pub date: String,
    /// Unix timestamp in **milliseconds** (0 when date is invalid).
    pub date_ts: i64,

    pub narration: String,
    pub reference: String,

    pub debit:   Option<f64>,
    pub credit:  Option<f64>,
    pub balance: Option<f64>,

    pub vendor:       String,
    pub account_head: String,
    pub txn_type:     VoucherType,
    pub confidence:   f64,
    pub status:       TransactionStatus,
    /// How this transaction was classified: "rule" | "keyword" | "ai" | "user" | "" (unclassified).
    #[serde(default)]
    pub classification_source: String,

    pub tags: Vec<String>,

    pub bank_name:  String,
    pub account_no: String,

    /// True for the synthetic opening-balance marker row.
    pub is_opening_balance: bool,
    /// True when detected as an exact or near-duplicate.
    pub dup_flag: bool,
    /// Account balance BEFORE this transaction (stamped by compute_prev_balances).
    pub prev_balance: Option<f64>,
    /// true = balance reconciles; false = mismatch (stamped by validate_balances).
    pub balance_ok: Option<bool>,
}

impl Transaction {
    /// Port of `Parser.hash(txn)`.
    ///
    /// Produces a stable 8-character hex fingerprint from date + narration + debit + credit.
    /// Used to generate deterministic transaction IDs and to detect near-duplicates.
    ///
    /// Algorithm: 31-polynomial rolling hash with 32-bit wrapping arithmetic,
    /// matching JS `Math.imul` + `| 0` + `>>> 0` exactly.
    /// `charCodeAt` in JS returns UTF-16 code units; for ASCII (all bank statement
    /// fields) this equals the char code, so `char as i32` gives identical results.
    pub fn hash(&self) -> String {
        let s = format!(
            "{}|{}|{}|{}",
            self.date, self.narration,
            self.debit .map_or(String::new(), |v| format!("{}", v)),
            self.credit.map_or(String::new(), |v| format!("{}", v)),
        );
        let mut h: i32 = 0;
        for c in s.chars() {
            // Math.imul(31, h) + charCodeAt(i) | 0  →  wrapping 32-bit arithmetic
            h = 31i32.wrapping_mul(h).wrapping_add(c as i32);
        }
        format!("{:x}", h as u32)
    }

    pub fn new(id: impl Into<String>) -> Self {
        Transaction {
            id: id.into(),
            import_id: None,
            date: String::new(),
            date_ts: 0,
            narration: String::new(),
            reference: String::new(),
            debit: None,
            credit: None,
            balance: None,
            vendor: String::new(),
            account_head: String::new(),
            txn_type: VoucherType::Unknown,
            confidence: 0.0,
            status: TransactionStatus::Unreviewed,
            classification_source: String::new(),
            tags: Vec::new(),
            bank_name: String::new(),
            account_no: String::new(),
            is_opening_balance: false,
            dup_flag: false,
            prev_balance: None,
            balance_ok: None,
        }
    }
}

// ── Parse result ──────────────────────────────────────────────────────────────

/// Result of parsing one bank statement file (or one Excel sheet).
#[derive(Debug, Clone)]
pub struct ParseResult {
    pub transactions:    Vec<Transaction>,
    pub opening_balance: Option<f64>,
    pub closing_balance: Option<f64>,

    pub bank_name:  String,
    pub account_no: String,

    /// Excel sheet name or PDF file name.
    pub source_name: String,

    pub col_map:         ColumnMap,
    pub header_row_idx:  usize,
    pub noise_row_count: usize,
    pub rejected_row_count: usize,
}

impl ParseResult {
    pub fn empty(source_name: impl Into<String>) -> Self {
        ParseResult {
            transactions: Vec::new(),
            opening_balance: None,
            closing_balance: None,
            bank_name: String::new(),
            account_no: String::new(),
            source_name: source_name.into(),
            col_map: ColumnMap::default(),
            header_row_idx: 0,
            noise_row_count: 0,
            rejected_row_count: 0,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn txn(date: &str, narration: &str, debit: Option<f64>, credit: Option<f64>) -> Transaction {
        Transaction { date: date.into(), narration: narration.into(), debit, credit, ..Transaction::new("t") }
    }

    // ── Transaction::hash ─────────────────────────────────────────────────────

    #[test]
    fn hash_is_hex_string() {
        let t = txn("01/01/2024", "SALARY CREDIT", None, Some(50000.0));
        let h = t.hash();
        assert!(!h.is_empty(), "hash must not be empty");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "hash must be hex: {}", h);
    }

    #[test]
    fn hash_same_inputs_same_output() {
        let t1 = txn("01/01/2024", "SALARY CREDIT", None, Some(50000.0));
        let t2 = txn("01/01/2024", "SALARY CREDIT", None, Some(50000.0));
        assert_eq!(t1.hash(), t2.hash(), "identical inputs → identical hash");
    }

    #[test]
    fn hash_different_date_different_hash() {
        let t1 = txn("01/01/2024", "SALARY CREDIT", None, Some(50000.0));
        let t2 = txn("02/01/2024", "SALARY CREDIT", None, Some(50000.0));
        assert_ne!(t1.hash(), t2.hash(), "different dates → different hash");
    }

    #[test]
    fn hash_different_narration_different_hash() {
        let t1 = txn("01/01/2024", "ATM WDL", Some(10000.0), None);
        let t2 = txn("01/01/2024", "ATM WDL BANDRA", Some(10000.0), None);
        assert_ne!(t1.hash(), t2.hash());
    }

    #[test]
    fn hash_debit_vs_credit_different() {
        // Same amount in debit vs credit should produce different hashes
        let t1 = txn("01/01/2024", "PAYMENT", Some(5000.0), None);
        let t2 = txn("01/01/2024", "PAYMENT", None, Some(5000.0));
        assert_ne!(t1.hash(), t2.hash(), "debit vs credit → different hash");
    }

    #[test]
    fn hash_empty_fields_stable() {
        // date="", narration="", debit=None, credit=None →
        // key string = "|||" (three pipe separators, no other content).
        // Hash is deterministic and non-empty (not the zero string from truly empty input).
        let t = txn("", "", None, None);
        let h = t.hash();
        assert!(!h.is_empty(), "hash must never be empty");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "must be hex: {}", h);
        // Same input always gives same hash
        assert_eq!(t.hash(), h, "hash must be deterministic");
    }

    // Verify against JS reference: the hash algorithm must match Math.imul behavior.
    // JS: s = "01/01/2024|SALARY|50000|", h starts at 0, rolls over each char.
    // We test structural properties (determinism, hex output) not absolute values,
    // since we can't run JS in tests — the algorithm is verified by code inspection.
    #[test]
    fn hash_is_8_chars_or_fewer() {
        let t = txn("01/01/2024", "NEFT PAYMENT FROM RAJESH KUMAR", Some(25000.0), None);
        let h = t.hash();
        assert!(h.len() <= 8, "u32::MAX in hex = 8 chars, got: {}", h);
    }
}
