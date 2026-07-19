//! Noise row filter — port of `Parser._isNoiseRow(narration)` from parser.js.
//!
//! A "noise row" is any row in a bank statement that is NOT a real transaction:
//! page totals, sub-totals, opening/closing balance rows, column header repeats,
//! page-number artifacts, bank metadata rows (masked account numbers), etc.
//!
//! The function returns `true` when the narration matches any TOTALS or HDRS
//! pattern, exactly mirroring the JS implementation.
//!
//! ## Pattern sets (exact port)
//!
//! **TOTALS** — summary/balance/totals rows:
//! opening/closing balance, brought forward, carried forward, subtotals,
//! grand total, page total, pagination markers, balance-as-of, etc.
//!
//! **HDRS** — re-printed column header rows:
//! date, narration, particulars, debit, credit, balance, cheque, reference,
//! serial number variants, IDBI PDF artefacts, standalone tick/cross symbols.
//!
//! Input is lowercased before matching, matching JS `nl = narration.toLowerCase().trim()`.

use once_cell::sync::Lazy;
use regex::RegexSet;

// ── TOTALS pattern set ────────────────────────────────────────────────────────
// Patterns are applied to the lowercased, trimmed narration.
// Each string is a Rust regex; character class escapes follow the `regex` crate
// (which uses RE2 syntax — no lookaheads, no backreferences).

static TOTALS: Lazy<RegexSet> = Lazy::new(|| {
    RegexSet::new([
        // grand total / total / sub-total
        r"^(?:grand\s*)?total\b",
        r"^sub[\s\-]?total\b",
        // Balance rows
        r"^closing\s*(?:balance|bal)\b",
        r"^opening\s*(?:balance|bal)\b",
        // Brought/carried forward
        r"^brought\s+forward\b",
        r"^balance\s+brought\s+forward\b",
        // "B/F", "B\F", "B/D", "B\D"
        r"^b\s*[/\\]\s*[fd]\b",
        // balance c/f, b/f, carried forward, bf, cf
        r"^balance\s*(?:c/f|b/f|carried\s*forward|bf|cf)\b",
        // Carried forward / c/f / b/f standalone
        r"^carried\s+forward\b",
        r"^c/f\b",
        r"^b/f\b",
        // Balance as on / at / of date
        r"^balance\s+as\s+(?:on|at|of)\b",
        // Available / ledger / total balance (standalone label rows)
        r"^available\s*balance\b",
        r"^ledger\s*balance\b",
        r"^total\s*balance\b",
        // Statement summary header
        r"^statement\s*(?:of|summary)\b",
        // IDFC First / IDBI — additional opening balance variants
        r"^op\.?\s*(?:balance|bal)\b",
        r"^prev(?:ious)?\s*(?:balance|bal|closing)\b",
        r"^balance\s*(?:forward|fwd)\b",
        r"^net\s*(?:balance|bal)\b",
        // Mid-row page total (not anchored — matches anywhere in the string)
        r"\bpage\s*total\b",
        // Pagination markers: "Page 1", "Page 1 of 5"
        r"^page\s+\d+(?:\s+of\s+\d+)?$",
        // "1 of 5" style
        r"^\d+\s+of\s+\d+$",
        // Bank account metadata rows: "IDBI Bank 0460XXXXXX3948" or unmasked long number
        r"^[a-z\s]+bank\s+\d+x+\d+$",
        r"^[a-z\s]+bank\s+\d{10,}$",
    ])
    .expect("TOTALS regex set failed to compile")
});

// ── HDRS pattern set ──────────────────────────────────────────────────────────

static HDRS: Lazy<RegexSet> = Lazy::new(|| {
    RegexSet::new([
        r"^date\b",
        r"^narration\b",
        r"^particulars\b",
        r"^description\b",
        r"^transaction\s*(?:date|details|id|no)\b",
        r"^tran\s*(?:date|details|id|no)\b",
        r"^debit\b",
        r"^credit\b",
        r"^balance\b",
        r"^amount\b",
        r"^withdrawal\b",
        r"^deposit\b",
        r"^chq\b",
        r"^cheque\b",
        r"^reference\b",
        r"^remarks\b",
        // Serial number column headers
        r"^s\.?\s*no\b",
        r"^sr\.?\s*no\b",
        r"^sl\.?\s*no\b",
        r"^serial\s*(?:no|number)\b",
        // IDBI PDF artefacts: standalone "Receipt" or "Receipt?"
        r"^receipt\b[?]?$",
        // Standalone tick / cross symbols (✓✗✔✘)
        r"^[\u{2713}\u{2717}\u{2714}\u{2718}]+$",
    ])
    .expect("HDRS regex set failed to compile")
});

// ── Public API ────────────────────────────────────────────────────────────────

/// Return `true` when `narration` is a noise row (not a real transaction).
///
/// Exactly mirrors `Parser._isNoiseRow(narration)` from parser.js:
/// ```js
/// const nl = (narration || '').toLowerCase().trim();
/// if (!nl) return false;
/// return TOTALS.some(p => p.test(nl)) || HDRS.some(p => p.test(nl));
/// ```
pub fn is_noise_row(narration: &str) -> bool {
    let nl = narration.to_lowercase();
    let nl = nl.trim();
    if nl.is_empty() {
        return false;
    }
    TOTALS.is_match(nl) || HDRS.is_match(nl)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: assert noise
    fn noise(s: &str) {
        assert!(
            is_noise_row(s),
            "expected is_noise_row({:?}) = true, got false",
            s
        );
    }

    // Helper: assert NOT noise
    fn real(s: &str) {
        assert!(
            !is_noise_row(s),
            "expected is_noise_row({:?}) = false, got true",
            s
        );
    }

    // ── Empty / blank ─────────────────────────────────────────────────────────

    #[test]
    fn empty_string_not_noise() {
        real("");
        real("   ");
    }

    // ── TOTALS patterns ───────────────────────────────────────────────────────

    #[test]
    fn grand_total_noise() {
        noise("grand total");
        noise("Grand Total"); // case-insensitive via lowercase
        noise("GRAND TOTAL");
    }

    #[test]
    fn total_noise() {
        noise("total");
        noise("Total Transactions"); // starts with "total "
    }

    #[test]
    fn sub_total_noise() {
        noise("sub total");
        noise("sub-total");
        noise("subtotal");
    }

    #[test]
    fn closing_balance_noise() {
        noise("closing balance");
        noise("Closing Balance");
        noise("Closing Bal");
        noise("closing bal");
    }

    #[test]
    fn opening_balance_noise() {
        noise("opening balance");
        noise("Opening Balance");
        noise("opening bal");
    }

    #[test]
    fn brought_forward_noise() {
        noise("brought forward");
        noise("Brought Forward");
        noise("balance brought forward");
    }

    #[test]
    fn bf_cf_noise() {
        noise("b/f");
        noise("c/f");
        noise("B/F");
        noise("C/F");
        noise("b\\f"); // back-slash variant
    }

    #[test]
    fn balance_cf_variants_noise() {
        noise("balance c/f");
        noise("balance b/f");
        noise("balance carried forward");
        noise("balance bf");
        noise("balance cf");
        noise("balance forward");
        noise("balance fwd");
    }

    #[test]
    fn carried_forward_noise() {
        noise("carried forward");
    }

    #[test]
    fn balance_as_on_noise() {
        noise("balance as on 01/01/2024");
        noise("balance as at 31/03/2024");
        noise("balance as of 31/12/2023");
    }

    #[test]
    fn available_ledger_total_balance_noise() {
        noise("available balance");
        noise("ledger balance");
        noise("total balance");
    }

    #[test]
    fn statement_summary_noise() {
        noise("statement of account");
        noise("statement summary");
    }

    #[test]
    fn op_balance_variants_noise() {
        noise("op. balance");
        noise("op balance");
        noise("op. bal");
        noise("op bal");
    }

    #[test]
    fn previous_balance_noise() {
        noise("previous balance");
        noise("previous bal");
        noise("previous closing");
        noise("prev balance");
        noise("prev bal");
    }

    #[test]
    fn net_balance_noise() {
        noise("net balance");
        noise("Net Balance");
        noise("net bal");
    }

    #[test]
    fn page_total_noise() {
        noise("page total");
        noise("Page Total");
        noise("Monthly page total"); // mid-string match (no ^ anchor)
    }

    #[test]
    fn pagination_markers_noise() {
        noise("page 1");
        noise("page 1 of 5");
        noise("page 10 of 25");
        noise("1 of 5");
        noise("3 of 12");
    }

    #[test]
    fn bank_metadata_rows_noise() {
        noise("idbi bank 0460xxx3948"); // masked account
        noise("IDBI BANK 0460XXX3948"); // uppercase (via lowercase)
        noise("sbi bank 30120456789012"); // unmasked long account (≥10 digits)
    }

    // ── HDRS patterns ─────────────────────────────────────────────────────────

    #[test]
    fn column_headers_noise() {
        noise("date");
        noise("narration");
        noise("particulars");
        noise("description");
        noise("debit");
        noise("credit");
        noise("balance");
        noise("amount");
        noise("withdrawal");
        noise("deposit");
        noise("chq");
        noise("cheque");
        noise("reference");
        noise("remarks");
    }

    #[test]
    fn transaction_header_variants_noise() {
        noise("transaction date");
        noise("transaction details");
        noise("transaction id");
        noise("transaction no");
        noise("tran date");
        noise("tran details");
    }

    #[test]
    fn serial_number_headers_noise() {
        noise("s no");
        noise("s. no");
        noise("sr no");
        noise("sr. no");
        noise("sl no");
        noise("sl. no");
        noise("serial no");
        noise("serial number");
    }

    #[test]
    fn receipt_idbi_noise() {
        noise("receipt");
        noise("receipt?");
        noise("Receipt");
    }

    #[test]
    fn tick_cross_symbols_noise() {
        noise("✓");
        noise("✗");
        noise("✔");
        noise("✘");
        noise("✓✗");
    }

    // ── Real transactions (must NOT be noise) ─────────────────────────────────

    #[test]
    fn neft_payment_not_noise() {
        real("NEFT/RTG234567891/RATAN TATA/AXIS0001234");
        real("NEFT PAYMENT TO VENDOR");
    }

    #[test]
    fn upi_credit_not_noise() {
        real("UPI/CR/234567890123/MAHESH KUMAR/mahesh@okaxis");
    }

    #[test]
    fn atm_withdrawal_not_noise() {
        real("ATM WDL/ATM123456/HDFC BANK ATM");
    }

    #[test]
    fn salary_credit_not_noise() {
        real("SALARY CREDIT ACME PVT LTD JAN 2024");
        real("SAL/MARCH/2024/XYZ COMPANY PVT LTD");
    }

    #[test]
    fn swiggy_zomato_not_noise() {
        real("SWIGGY TECHNOLOGIES PVT LTD");
        real("ZOMATO MEDIA PVT LTD");
    }

    #[test]
    fn gst_payment_not_noise() {
        real("GST PMT CGST SGST CHALLAN 09-24");
    }

    #[test]
    fn interest_credited_not_noise() {
        real("INTEREST CREDITED FOR JAN 2024");
    }

    #[test]
    fn receipt_in_narration_not_noise() {
        // "receipt" only matches when it's the ENTIRE (trimmed) string
        // "receipt of payment" → ^receipt\b[?]?$ doesn't match ($ fails after "receipt ")
        real("receipt of payment from vendor");
        real("received payment from ABC");
    }

    #[test]
    fn totals_in_description_not_noise() {
        // "grand total purchases" starts with "grand total" → ^...total\b → NOISE
        // Let's verify the word-boundary rule:
        // "totals for the month" starts with "total" then "s" — \b is between l and s?
        // In "totals", \b is at start and end of the word, NOT between 'l' and 's'.
        // So /^total\b/ does NOT match "totals" since the 's' breaks the boundary.
        real("totals for the month"); // "totals" → ^(grand\s*)?total\b → "totals" has no \b after "total"
    }

    #[test]
    fn balance_word_in_narration_not_noise() {
        // "account balance enquiry" does NOT start with "balance" — wait, it does!
        // "balance enquiry" starts with "balance" → NOISE
        // But "low account balance alert" doesn't start with "balance"
        real("low account balance alert");
        real("minimum balance charge deducted"); // starts with "minimum" → not noise
    }

    #[test]
    fn partial_balance_header_not_noise() {
        // "balance transfer" starts with "balance" → IS noise by HDRS /^balance\b/
        // So this IS noise — confirming our regex correctly matches
        noise("balance transfer");
    }

    #[test]
    fn page_number_in_narration_not_noise() {
        // "transferred on page 3" contains "page" but doesn't start with "page \d+"
        // BUT page_total: \bpage\s*total\b is a non-anchored pattern → matches if "page total" anywhere
        real("transferred on page 3 from branch"); // no "total" adjacent → not noise
    }

    // ── Case-insensitivity (via lowercase) ───────────────────────────────────

    #[test]
    fn uppercase_headers_noise() {
        noise("DATE");
        noise("NARRATION");
        noise("DEBIT");
        noise("CREDIT");
        noise("BALANCE");
        noise("GRAND TOTAL");
        noise("CLOSING BALANCE");
    }

    #[test]
    fn mixed_case_noise() {
        noise("Grand Total");
        noise("Opening Balance");
        noise("Brought Forward");
    }
}
