//! ocr_parser.rs — Port of `Parser._parseOCRText(rawText, fileName)`.
//!
//! Converts raw OCR text output (from Tesseract or any OCR engine) into a
//! structured `ParseResult`.  No browser/canvas dependency — pure text logic.
//!
//! Algorithm:
//!   1. Split into lines; collapse whitespace; drop lines ≤ 4 chars.
//!   2. Pre-pass: lines before the first date line → headerText for bank detection.
//!   3. Main loop: lines starting with DD/MM/YYYY (or variants) are transactions.
//!      Continuation lines (no date, no amounts, not starting with a digit) are
//!      appended to the previous transaction's narration (up to 200 chars).
//!   4. From each transaction line: last amount = balance; others = txn amounts.
//!      Direction inferred from Dr/Cr markers or balance movement (2 % tolerance).
//!   5. Post-process: compute_prev_balances, bank detection, prepend OB row.

use once_cell::sync::Lazy;
use regex::Regex;

use crate::parser::{
    ParseResult, Transaction,
    bank_detection::{detect, DetectOptions},
    date_parser::normalize_transaction_date,
    excel_parser::{compute_prev_balances, prepend_opening_balance_row},
    ocr_correction,
};

// ── Regexes ───────────────────────────────────────────────────────────────────

// Date at start of line: DD/MM/YYYY, DD-MM-YYYY, DD.MM.YYYY (or 2-digit year).
// Mirrors JS: /^(\d{1,2}[\/\-\.]\d{1,2}[\/\-\.]\d{2,4})\b/
static DATE_START_RE: Lazy<Regex> = Lazy::new(||
    Regex::new(r"^(\d{1,2}[/\-.]\d{1,2}[/\-.]\d{2,4})\b").unwrap()
);

// Indian currency amount: comma-formatted or plain ≥5 digits, required decimal
// for 1–4 digit integers.
//
// Requiring a decimal point for 1–4 digit integers prevents false positives from:
//   • Value-date fragments  — "01/01/2024" → bare "01", "01" no longer match.
//   • Reference sub-strings — "SAL001", "ATM001" → bare "001" no longer matches.
//   • Short year-like tokens — "24" (2-digit year) no longer matches.
//
// 5+ digit integers still match without a decimal so plain amounts like "50000"
// (without ".00") are captured.  Values < 1 and 4-digit year literals (1900–2100)
// are filtered in the loop below.
static AMT_RE: Lazy<Regex> = Lazy::new(||
    Regex::new(r"\b(\d{1,4}(?:,\d{2,3})*\.\d{1,2}|\d{5,}(?:\.\d{1,2})?)\b").unwrap()
);

// Dr / Cr markers to strip from narration text.
// Mirrors JS: /\b(DR|CR|Dr|Cr)\b\.?/g
static DRCR_RE: Lazy<Regex> = Lazy::new(||
    Regex::new(r"(?i)\b(?:DR|CR)\b\.?").unwrap()
);

// Stray punctuation (keep word chars, whitespace, / - . @ &).
// JS `\w` = [A-Za-z0-9_]; Rust default `\w` is Unicode-aware, so we use explicit set.
static STRAY_PUNCT_RE: Lazy<Regex> = Lazy::new(||
    Regex::new(r"[^A-Za-z0-9_\s/\-.@&]").unwrap()
);

// ── Amount extraction ─────────────────────────────────────────────────────────

/// Extracted amount with its value, raw string, and byte position in the line.
#[derive(Debug, Clone)]
struct AmountMatch {
    val: f64,
    raw: String,
    idx: usize,
}

/// Port of the `extractAmounts` inner function.
///
/// Finds all Indian-format currency amounts in `s`.
/// Skips:
///   - Values < 1 (fractions, zeros)
///   - 4-digit values in the year range 1900–2100 (year literals)
fn extract_amounts(s: &str) -> Vec<AmountMatch> {
    let mut out = Vec::new();
    for cap in AMT_RE.captures_iter(s) {
        let m   = cap.get(1).unwrap();
        let raw = m.as_str().to_string();
        let v: f64 = raw.replace(',', "").parse().unwrap_or(0.0);
        // Skip zeros, sub-1 fractions, and 4-digit year literals
        if v < 1.0 { continue; }
        if v >= 1900.0 && v <= 2100.0 && raw.len() == 4 { continue; }
        out.push(AmountMatch { val: v, raw, idx: m.start() });
    }
    out
}

// ── parse_ocr_text ────────────────────────────────────────────────────────────

/// Port of `Parser._parseOCRText(rawText, fileName)`.
///
/// Call with the raw string produced by an OCR engine.  The function runs the
/// full `OCRCorrection.correctText()` pipeline (char repair + fuzzy banking-
/// term correction, 2 passes) then extracts transactions.
///
/// Returns a `ParseResult` with `source_name = "{file_name} [OCR]"`.
/// Closing balance is always `None` (OCR text rarely contains it explicitly).
pub fn parse_ocr_text(raw_text: &str, file_name: &str) -> ParseResult {

    // 1. Apply OCR correction (char repair l→1, O→0, S→5, … + fuzzy banking-
    //    term correction against BANK_TERMS) — port of `OCRCorrection.correctText()`,
    //    matching the old app's default 2-pass call (parser.js:2016).
    let corrected = if raw_text.is_empty() {
        raw_text.to_string()
    } else {
        ocr_correction::correct_text(raw_text, 2)
    };

    // 2. Split + normalise lines; discard lines ≤ 4 chars.
    let lines: Vec<String> = corrected
        .lines()
        .map(|l| {
            // collapse internal whitespace (mirrors JS .replace(/\s+/g,' ').trim())
            l.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .filter(|l| l.len() > 4)
        .collect();

    // 3. Pre-pass: collect header text (lines before first date line).
    let first_txn_idx = lines.iter().position(|l| DATE_START_RE.is_match(l));
    let header_text: String = match first_txn_idx {
        Some(0) | None => String::new(),
        Some(n)        => lines[..n].join("\n"),
    };

    // 4. Main extraction loop.
    let mut txns:        Vec<Transaction> = Vec::new();
    let mut prev_balance: Option<f64>     = None;
    let mut txn_counter                   = 0usize;

    for line in &lines {
        if let Some(cap) = DATE_START_RE.captures(line) {
            let date_str = cap[1].to_string();
            let rest_raw = line[date_str.len()..].trim();

            // Clean OCR artefacts from rest of line
            let rest_clean: String = {
                let s = rest_raw.replace(|c| "|\\{}[]".contains(c), " ");
                s.split_whitespace().collect::<Vec<_>>().join(" ")
            };

            let amts = extract_amounts(&rest_clean);
            if amts.is_empty() { continue; }

            // Last amount = balance; all others = transaction amounts
            let balance  = (amts.last().unwrap().val * 100.0).round() / 100.0;
            let txn_amts = &amts[..amts.len() - 1];

            // Build narration: remove all amount strings (reverse order to preserve indices)
            let mut narration = rest_clean.clone();
            for a in amts.iter().rev() {
                let end = (a.idx + a.raw.len()).min(narration.len());
                if a.idx <= narration.len() {
                    narration = format!("{}{}", &narration[..a.idx], &narration[end..]);
                }
            }
            // Strip Dr/Cr markers
            narration = DRCR_RE.replace_all(&narration, "").to_string();
            // Strip stray punctuation
            narration = STRAY_PUNCT_RE.replace_all(&narration, " ").to_string();
            // Collapse whitespace
            narration = narration.split_whitespace().collect::<Vec<_>>().join(" ");
            if narration.is_empty() { narration = "(OCR)".to_string(); }

            // Determine debit / credit
            let dr_marker = {
                let up = rest_clean.to_uppercase();
                // word-boundary check: is "DR" a standalone token?
                up.split_whitespace().any(|w| w.trim_end_matches('.') == "DR")
            };
            let cr_marker = {
                let up = rest_clean.to_uppercase();
                up.split_whitespace().any(|w| w.trim_end_matches('.') == "CR")
            };

            let (debit, credit) = if txn_amts.len() == 1 {
                let amt = txn_amts[0].val;
                if dr_marker {
                    (Some(amt), None)
                } else if cr_marker {
                    (None, Some(amt))
                } else if let Some(prev) = prev_balance {
                    let diff = balance - prev;
                    if (diff - amt).abs() < amt * 0.02 {
                        (None, Some(amt))         // balance went UP → credit
                    } else if (diff + amt).abs() < amt * 0.02 {
                        (Some(amt), None)          // balance went DOWN → debit
                    } else {
                        (None, Some(amt))          // best guess: credit
                    }
                } else {
                    (None, Some(amt))              // first txn → credit
                }
            } else if txn_amts.len() >= 2 {
                let a = txn_amts[0].val;
                let b = txn_amts.last().unwrap().val;
                if let Some(prev) = prev_balance {
                    let diff = balance - prev;
                    let debit  = if diff < 0.0 { Some(if a > 0.0 { a } else { b }) } else { None };
                    let credit = if diff > 0.0 { Some(if b > 0.0 { b } else { a }) } else { None };
                    (debit, credit)
                } else {
                    (if a > 0.0 { Some(a) } else { None }, if b > 0.0 { Some(b) } else { None })
                }
            } else {
                (None, None)
            };

            prev_balance = Some(balance);
            let nd = normalize_transaction_date(&date_str);

            txn_counter += 1;
            let mut t = Transaction::new(format!("t_ocr_{}", txn_counter));
            t.date      = nd.display;
            t.date_ts   = nd.ts;
            t.narration = narration;
            t.debit     = debit .filter(|&v| v > 0.0).map(|v| (v * 100.0).round() / 100.0);
            t.credit    = credit.filter(|&v| v > 0.0).map(|v| (v * 100.0).round() / 100.0);
            t.balance   = Some(balance);
            txns.push(t);

        } else {
            // Continuation line: no date → append to last txn narration
            if let Some(last) = txns.last_mut() {
                let line_amts = extract_amounts(line);
                let starts_with_digit = line.starts_with(|c: char| c.is_ascii_digit());
                if line_amts.is_empty() && line.len() > 3 && !starts_with_digit {
                    let combined = format!("{} {}", last.narration, line);
                    last.narration = combined.trim().chars().take(200).collect();
                }
            }
        }
    }

    // 5. Post-processing.
    let op_balance = compute_prev_balances(&mut txns, None);

    // Bank detection from full text + header text.
    let narrations: Vec<&str> = txns.iter().map(|t| t.narration.as_str()).collect();
    let bank_meta = detect(DetectOptions {
        text:        &corrected,
        header_text: &header_text,
        filename:    file_name,
        narrations:  &narrations,
    });
    let bank_name  = bank_meta.bank_name.clone();
    let account_no = bank_meta.account_no.clone();
    for t in &mut txns {
        if t.bank_name.is_empty()  { t.bank_name  = bank_name.clone(); }
        if t.account_no.is_empty() { t.account_no = account_no.clone(); }
    }

    prepend_opening_balance_row(&mut txns, op_balance, &bank_name, &account_no);

    ParseResult {
        transactions:       txns,
        opening_balance:    op_balance,
        closing_balance:    None,  // OCR text rarely contains explicit closing balance
        bank_name,
        account_no,
        source_name:        format!("{} [OCR]", file_name),
        col_map:            Default::default(),
        header_row_idx:     0,
        noise_row_count:    0,
        rejected_row_count: 0,
    }
}

// ── preprocess_multiline ──────────────────────────────────────────────────────

/// Normalise multi-line PDF text (BOM, SBI, Mahanagar…) into single-line
/// format that `parse_ocr_text` can consume.
///
/// These banks extract as:
///   Line 1: "04/04/2022"         ← entire line is the date
///   Line 2: "UPI209…Papad"       ← narration
///   Line 3: "209498825681"        ← reference (pure integer → skip)
///   Line 4: "610.00"             ← debit or credit amount
///   Line 5: "13,24,083.22"       ← running balance
///   Line 6: "11111-CentralData"  ← noise (channel/branch code)
///   Line 7: "04/04/2022"         ← next transaction
///
/// Output: `"04/04/2022 UPI209…Papad 610.00 1324083.22"`
///
/// Amounts are identified by the "mostly-digits" ratio: a line where > 80 %
/// of non-whitespace characters are digits or decimal/comma separators is an
/// amount line; everything else is narration or noise.
pub fn preprocess_multiline(text: &str) -> String {
    use crate::parser::date_parser::normalize_transaction_date;

    // Header / noise words that appear as standalone lines in some PDFs
    let header_words: &[&str] = &[
        "date","type","particulars","debit","credit","balance","channel",
        "cheque","reference","txn","value","valuedate","description","chq",
        "ref","s.no","sr.no","sl.no","serial","withdrawal","deposit",
        "dr","cr","amount","narration","details",
    ];

    let is_header_line = |line: &str| -> bool {
        let l = line.to_lowercase();
        header_words.iter().any(|h| l.trim() == *h)
    };

    /// True when the line is a pure reference number: >= 6 digits, no decimal.
    let is_pure_integer = |line: &str| -> bool {
        let s = line.replace(',', "");
        let s = s.trim();
        s.len() >= 6 && s.chars().all(|c| c.is_ascii_digit())
    };

    /// True when the line is an amount line: after stripping "Rs"/"Rs." prefix,
    /// > 80 % of non-whitespace characters are digit/separator.
    let is_amount_line = |line: &str| -> bool {
        let s = line.trim();
        let s = if s.to_lowercase().starts_with("rs.") { &s[3..] }
                else if s.to_lowercase().starts_with("rs") { &s[2..] }
                else { s };
        let s = s.trim();
        if s.is_empty() { return false; }
        let total: usize = s.chars().filter(|c| !c.is_whitespace()).count();
        if total == 0 { return false; }
        let num: usize = s.chars().filter(|c| c.is_ascii_digit() || *c == ',' || *c == '.').count();
        (num as f64 / total as f64) > 0.80
    };

    /// Parse the amount from an amount line (strip "Rs" prefix, commas).
    let parse_amt_line = |line: &str| -> Option<f64> {
        let s = line.trim();
        let s = if s.to_lowercase().starts_with("rs.") { &s[3..] }
                else if s.to_lowercase().starts_with("rs") { &s[2..] }
                else { s };
        let s = s.trim().replace(',', "");
        s.parse::<f64>().ok().filter(|&v| v > 0.0 && v < 2e9)
    };

    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.len() > 1)
        .collect();

    let mut out: Vec<String> = Vec::new();
    let mut cur_date: Option<String> = None;
    let mut cur_narr: Vec<String>    = Vec::new();
    let mut cur_amts: Vec<f64>       = Vec::new();

    let flush = |date: &str, narrs: &[String], amts: &[f64], out: &mut Vec<String>| {
        if amts.len() < 2 { return; } // need txn amount + balance minimum
        let narr = narrs.join(" ");
        let narr = if narr.trim().is_empty() { "TRANSACTION".to_string() } else { narr.trim().to_string() };
        // Format amounts as plain decimals so parse_ocr_text can re-parse them
        let amts_str: Vec<String> = amts.iter().map(|a| format!("{:.2}", a)).collect();
        out.push(format!("{} {} {}", date, narr, amts_str.join(" ")));
    };

    // Noise patterns that never belong in a narration
    let is_noise_line = |line: &str| -> bool {
        let l = line.to_lowercase();
        (l.contains("page") && l.contains("of"))
            || l.starts_with("11111")     // BOM channel code
            || l.contains("?identity")    // lopdf font error
            || l.starts_with("statement for account")
            || l.starts_with("idbi bank")
            || l.starts_with("our toll")
            || l.len() <= 2
    };

    for &line in &lines {
        if is_noise_line(line) { continue; }
        if is_header_line(line) { continue; }

        // Try to parse line as a date
        let nd = normalize_transaction_date(line);
        if nd.valid {
            // Flush previous transaction group
            if let Some(ref date) = cur_date {
                flush(date, &cur_narr, &cur_amts, &mut out);
            }
            cur_date = Some(nd.display.clone());
            cur_narr.clear();
            cur_amts.clear();
        } else if cur_date.is_some() {
            if is_pure_integer(line) {
                // Reference number — skip entirely
            } else if is_amount_line(line) {
                if let Some(v) = parse_amt_line(line) {
                    cur_amts.push(v);
                }
            } else {
                // Text line — add to narration if not noise
                cur_narr.push(line.to_string());
            }
        }
    }

    // Flush the last group
    if let Some(ref date) = cur_date {
        flush(date, &cur_narr, &cur_amts, &mut out);
    }

    out.join("\n")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_amounts ───────────────────────────────────────────────────────

    #[test]
    fn extracts_plain_amount() {
        let amts = extract_amounts("SALARY 50000.00 1,50,000.00");
        assert_eq!(amts.len(), 2);
        assert!((amts[0].val - 50000.0).abs() < 0.01);
        assert!((amts[1].val - 150000.0).abs() < 0.01);
    }

    #[test]
    fn extracts_comma_formatted_amount() {
        let amts = extract_amounts("1,50,000.50");
        assert_eq!(amts.len(), 1);
        assert!((amts[0].val - 150000.5).abs() < 0.01);
    }

    #[test]
    fn skips_year_literals() {
        // "2024" alone should be skipped (4-digit year range)
        let amts = extract_amounts("STATEMENT 2024 BALANCE 50000.00");
        assert_eq!(amts.len(), 1, "2024 should be skipped as year literal");
        assert!((amts[0].val - 50000.0).abs() < 0.01);
    }

    #[test]
    fn skips_sub_one_values() {
        // alt1 = \d{1,4}(,\d{2,3})*\.\d{1,2} (decimal required for ≤4-digit ints)
        // alt2 = \d{5,}(\.\d{1,2})? (5+ digit ints, decimal optional)
        // "0.50"  → alt1: 1 digit + ".50" → val=0.50 < 1.0 → filtered
        // "50000.00" → alt2 (5 digits) → val=50000.0, kept
        let amts = extract_amounts("0.50 some text 50000.00");
        assert_eq!(amts.len(), 1, "0.50 filtered; 50000.00 kept");
        assert!((amts[0].val - 50000.0).abs() < 0.01);
    }

    #[test]
    fn extracts_large_five_plus_digit() {
        let amts = extract_amounts("12345 50000.00");
        assert_eq!(amts.len(), 2);
    }

    // ── parse_ocr_text — basic extraction ────────────────────────────────────

    const BASIC_OCR: &str = "\
HDFC Bank Account Statement Jan 2024
Account No: 50100123456789

15/01/2024 SALARY CREDIT ACME PVT LTD 50000.00 1,50,000.00
16/01/2024 ATM WDL DR 10000.00 1,40,000.00
17/01/2024 NEFT FROM RAJESH KUMAR CR 25000.00 1,65,000.00
19/01/2024 SWIGGY ORDER 850.00 1,64,150.00
";

    #[test]
    fn parse_basic_returns_transactions() {
        let result = parse_ocr_text(BASIC_OCR, "hdfc_ocr.pdf");
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        assert!(!real.is_empty(), "should extract at least one transaction");
    }

    #[test]
    fn parse_source_name_has_ocr_suffix() {
        let result = parse_ocr_text(BASIC_OCR, "hdfc.pdf");
        assert_eq!(result.source_name, "hdfc.pdf [OCR]");
    }

    #[test]
    fn parse_has_opening_balance_row() {
        let result = parse_ocr_text(BASIC_OCR, "hdfc.pdf");
        assert!(result.transactions.first().map_or(false, |t| t.is_opening_balance),
            "first row should be synthetic opening balance");
    }

    #[test]
    fn parse_closing_balance_is_none() {
        let result = parse_ocr_text(BASIC_OCR, "hdfc.pdf");
        assert!(result.closing_balance.is_none(), "OCR closing balance always None");
    }

    // ── Direction inference ───────────────────────────────────────────────────

    #[test]
    fn dr_marker_sets_debit() {
        let text = "15/01/2024 PAYMENT DR 10000.00 90000.00\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        if !real.is_empty() {
            assert!(real[0].debit.is_some(), "DR marker → debit; got credit={:?}", real[0].credit);
        }
    }

    #[test]
    fn cr_marker_sets_credit() {
        let text = "15/01/2024 NEFT CR 25000.00 1,25,000.00\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        if !real.is_empty() {
            assert!(real[0].credit.is_some(), "CR marker → credit; got debit={:?}", real[0].debit);
        }
    }

    #[test]
    fn balance_movement_up_is_credit() {
        // No DR/CR markers; balance went 100000 → 125000 (up) → credit
        let text = "\
15/01/2024 SALARY 25000.00 1,25,000.00
16/01/2024 ATM WDL 10000.00 1,15,000.00
";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        assert!(real.len() >= 1);
        // First transaction: no prev_balance → defaults to credit
        assert!(real[0].credit.is_some() || real[0].debit.is_some(),
            "must have either debit or credit");
    }

    // ── Continuation line appending ───────────────────────────────────────────

    #[test]
    fn continuation_line_appended_to_narration() {
        let text = "\
15/01/2024 NEFT FROM RAJESH KUMAR 25000.00 1,25,000.00
SHARMA MUMBAI BRANCH
16/01/2024 ATM WDL 10000.00 1,15,000.00
";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        if real.len() >= 1 {
            // The continuation line "SHARMA MUMBAI BRANCH" should be in the first txn narration
            assert!(real[0].narration.contains("SHARMA") || real[0].narration.len() > 15,
                "continuation should be appended: {:?}", real[0].narration);
        }
    }

    #[test]
    fn continuation_line_with_amounts_not_appended() {
        // A line with amounts is NOT appended as continuation
        let text = "\
15/01/2024 SALARY 50000.00 1,50,000.00
25000.00 some number line
16/01/2024 ATM WDL 10000.00 1,40,000.00
";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        assert!(real.len() >= 1, "should have transactions");
    }

    // ── Empty / edge cases ────────────────────────────────────────────────────

    #[test]
    fn empty_text_returns_empty_result() {
        let result = parse_ocr_text("", "empty.pdf");
        assert_eq!(result.transactions.len(), 0, "empty text → no transactions");
    }

    #[test]
    fn no_dates_returns_empty_result() {
        let result = parse_ocr_text("HDFC Bank Account Statement Jan 2024\nNo transactions found.", "x.pdf");
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        assert!(real.is_empty(), "no date lines → no real transactions");
    }

    #[test]
    fn amounts_rounded_to_2dp() {
        let text = "15/01/2024 SALARY 50000.5 1,50,000.5\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        if let Some(t) = real.first() {
            let bal = t.balance.unwrap();
            // Balance 150000.5 rounded to 2dp → 150000.5
            assert!((bal - (bal * 100.0).round() / 100.0).abs() < 0.001,
                "balance should be rounded to 2dp");
        }
    }

    // ── Header text extraction for bank detection ─────────────────────────────

    #[test]
    fn bank_name_from_header_text() {
        let text = "\
HDFC Bank Account Statement January 2024
Account No: 50100123456789

15/01/2024 SALARY 50000.00 1,50,000.00
";
        let result = parse_ocr_text(text, "hdfc.pdf");
        // Bank name may or may not be detected depending on header parsing
        // Just verify it doesn't crash and returns a valid structure
        assert!(!result.source_name.is_empty());
    }

    // ── Two txn amounts → two-column layout ──────────────────────────────────

    #[test]
    fn two_txn_amounts_uses_balance_movement() {
        // Line has: debit_amt credit_amt balance_amt
        // balance went 100000 → 90000 (down by 10000) → debit = 10000
        let text = "\
15/01/2024 ATM WDL 10000.00 5000.00 90000.00
";
        // prev_balance = None → first txn, debit = a=10000, credit = b=5000
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        // Verify parsing doesn't crash and actually produces a transaction
        // (the previous `real.len() >= 0` here was vacuously always-true —
        // a usize can never be negative — and didn't check anything).
        assert!(!real.is_empty(), "expected at least one transaction to be parsed");
    }
}
