//! Amount parsing — port of `_parseAmt()` and `fmtAmt()` from parser.js.
//!
//! Key rules (faithful to the JS implementation):
//!
//! **Numeric input (f64)**
//!   - Reject 0.0, non-finite (NaN / ±∞), |val| > 200 crore (2e9).
//!   - Round to 2 decimal places.
//!
//! **String input**
//!   - Reject bare 10+-digit strings (UPI/IMPS/NEFT reference IDs).
//!   - Strip trailing CR/DR marker (`"1,500.00 CR"` → `"1,500.00"`).
//!   - Strip ₹ symbol and whitespace.
//!   - Strip `Rs` / `Rs.` prefix.
//!   - Strip thousands commas.
//!   - `"(-)"` prefix → negative sign (ICICI Wealth Mgmt style).
//!   - `"(amount)"` → negative amount.
//!   - Reject `nil`, `n/a`, lone `-`.
//!   - Reject if raw digit count > 10 (OCR-concatenated reference).
//!   - Reject if > 2 decimal places (not a valid Indian currency amount).
//!   - Reject zero, reject |amount| > 2e9.

use once_cell::sync::Lazy;
use regex::Regex;

// ── Compiled regexes ──────────────────────────────────────────────────────────

/// Bare 10-or-more digit string → UPI/IMPS reference ID, not an amount.
static RE_REF_ID: Lazy<Regex> = Lazy::new(||
    Regex::new(r"^\d{10,}$").unwrap()
);

/// Trailing "CR" or "DR" (case-insensitive) and anything after it.
/// E.g. `"1,500.00 CR"` → strip ` CR`.
static RE_CR_DR_SUFFIX: Lazy<Regex> = Lazy::new(||
    Regex::new(r"(?i)\s*(CR|DR)\b.*$").unwrap()
);

/// `"Rs"` or `"Rs."` at the very start (case-insensitive).
static RE_RS_PREFIX: Lazy<Regex> = Lazy::new(||
    Regex::new(r"(?i)^Rs\.?").unwrap()
);

/// `(amount)` → negative.  E.g. `"(1500.00)"` → `"-1500.00"`.
static RE_NEGATIVE_PARENS: Lazy<Regex> = Lazy::new(||
    Regex::new(r"\(([^)]+)\)").unwrap()
);

// ── Constants ─────────────────────────────────────────────────────────────────

/// ₹200 crore — OCR garbage guard (matches JS `2e9`).
const MAX_AMOUNT: f64 = 2_000_000_000.0;

// ── Public types ──────────────────────────────────────────────────────────────

/// A raw cell value from an Excel / PDF row.
///
/// Mirrors the JS `val` parameter of `_parseAmt()` which accepts both numbers
/// and strings.
#[derive(Debug, Clone)]
pub enum CellValue {
    Empty,
    Number(f64),
    Text(String),
}

impl From<f64> for CellValue {
    fn from(f: f64) -> Self { CellValue::Number(f) }
}

impl From<i64> for CellValue {
    fn from(i: i64) -> Self { CellValue::Number(i as f64) }
}

impl From<String> for CellValue {
    fn from(s: String) -> Self {
        if s.trim().is_empty() { CellValue::Empty } else { CellValue::Text(s) }
    }
}

impl From<&str> for CellValue {
    fn from(s: &str) -> Self {
        if s.trim().is_empty() { CellValue::Empty } else { CellValue::Text(s.to_owned()) }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Parse a monetary amount from a cell value.
///
/// Returns `None` when the value is empty, zero, or not a recognisable amount.
/// This is the primary entry point — equivalent to `Parser._parseAmt(val)`.
pub fn parse_amount(val: &CellValue) -> Option<f64> {
    match val {
        CellValue::Empty      => None,
        CellValue::Number(f)  => parse_float(*f),
        CellValue::Text(s)    => parse_str(s),
    }
}

/// Convenience wrapper: parse directly from a `&str`.
pub fn parse_amount_str(s: &str) -> Option<f64> {
    if s.trim().is_empty() { return None; }
    parse_str(s)
}

/// Format an amount in Indian locale style (crore / lakh grouping).
///
/// Returns an empty `String` for `None` — matches `Parser.fmtAmt(val)` returning `""`.
///
/// Examples:
/// ```
/// # use bank_statement_processor::parser::amount_parser::fmt_amount;
/// assert_eq!(fmt_amount(Some(1_23_456.78)), "1,23,456.78");
/// assert_eq!(fmt_amount(Some(50_000.00)),   "50,000.00");
/// assert_eq!(fmt_amount(None),              "");
/// ```
pub fn fmt_amount(val: Option<f64>) -> String {
    match val {
        None    => String::new(),
        Some(v) => format_indian(v),
    }
}

// ── Internal: numeric path ────────────────────────────────────────────────────

fn parse_float(f: f64) -> Option<f64> {
    if !f.is_finite() || f == 0.0 { return None; }
    if f.abs() > MAX_AMOUNT        { return None; }
    Some(round2(f))
}

// ── Internal: string path ─────────────────────────────────────────────────────

fn parse_str(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();

    // Reject bare 10+-digit strings (UPI/IMPS/NEFT reference IDs)
    if RE_REF_ID.is_match(trimmed) {
        return None;
    }

    // 1. Strip trailing CR/DR suffix ("1,500.00 CR" → "1,500.00")
    let s = RE_CR_DR_SUFFIX.replace(trimmed, "");

    // 2. Strip ₹ (U+20B9) and all whitespace
    let s: String = s.chars()
        .filter(|&c| c != '₹' && !c.is_whitespace())
        .collect();

    // 3. Strip "Rs" / "Rs." prefix
    let s = RE_RS_PREFIX.replace(&s, "");

    // 4. Strip thousands commas
    let s = s.replace(',', "");

    // 5. "(-)" prefix → minus sign (ICICI Wealth Management style)
    //    Must happen before the parens regex so "(-)" isn't matched by it.
    let s: String = if s.starts_with("(-)") {
        format!("-{}", &s[3..])
    } else {
        s.to_string()
    };

    // 6. (amount) → negative (e.g. "(1500.00)" → "-1500.00")
    let s = RE_NEGATIVE_PARENS.replace(&s, "-$1").into_owned();

    let s = s.trim().to_string();

    if s.is_empty() || s == "-" { return None; }
    // Reject nil / n/a strings
    if s.eq_ignore_ascii_case("nil") || s.eq_ignore_ascii_case("n/a") {
        return None;
    }

    // 7. Reject if raw digit count > 10 (OCR-concatenated reference number)
    let digit_count = s.chars().filter(|c| c.is_ascii_digit()).count();
    if digit_count > 10 { return None; }

    // 8. Reject > 2 decimal places (not valid Indian currency)
    if let Some(dot) = s.find('.') {
        if s.len() - dot - 1 > 2 { return None; }
    }

    // 9. Parse and validate
    let n: f64 = s.parse().ok()?;
    if n == 0.0      { return None; }
    if n.abs() > MAX_AMOUNT { return None; }

    Some(round2(n))
}

// ── Internal: formatting helpers ──────────────────────────────────────────────

fn round2(f: f64) -> f64 {
    (f * 100.0).round() / 100.0
}

/// Format as Indian number system string.
/// Last group = 3 digits; all preceding groups = 2 digits.
fn format_indian(v: f64) -> String {
    let negative = v < 0.0;
    let paise    = (v.abs() * 100.0).round() as u64;
    let rupees   = paise / 100;
    let frac     = paise % 100;

    let r_str = format_indian_int(rupees);
    let sign   = if negative { "-" } else { "" };
    format!("{}{}.{:02}", sign, r_str, frac)
}

fn format_indian_int(n: u64) -> String {
    if n == 0 { return "0".to_string(); }
    let s   = n.to_string();
    let len = s.len();

    if len <= 3 { return s; }

    // Split off the last 3 digits, then group the rest in twos from the right.
    let mut groups: Vec<&str> = Vec::new();
    groups.push(&s[len - 3..]);          // last 3

    let mut rest = &s[..len - 3];
    while !rest.is_empty() {
        let take  = rest.len().min(2);
        let start = rest.len() - take;
        groups.push(&rest[start..]);
        rest = &rest[..start];
    }
    groups.reverse();
    groups.join(",")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Numeric (f64) inputs — mirrors _parseAmt(typeof val === 'number') ──────

    #[test]
    fn numeric_zero_is_none() {
        assert_eq!(parse_amount(&CellValue::Number(0.0)), None);
    }

    #[test]
    fn numeric_negative_zero_is_none() {
        assert_eq!(parse_amount(&CellValue::Number(-0.0)), None);
    }

    #[test]
    fn numeric_nan_is_none() {
        assert_eq!(parse_amount(&CellValue::Number(f64::NAN)), None);
    }

    #[test]
    fn numeric_inf_is_none() {
        assert_eq!(parse_amount(&CellValue::Number(f64::INFINITY)), None);
        assert_eq!(parse_amount(&CellValue::Number(f64::NEG_INFINITY)), None);
    }

    #[test]
    fn numeric_over_200_crore_is_none() {
        assert_eq!(parse_amount(&CellValue::Number(2_100_000_000.0)), None);
        assert_eq!(parse_amount(&CellValue::Number(-2_100_000_000.0)), None);
    }

    #[test]
    fn numeric_valid_rounded_to_2dp() {
        assert_eq!(parse_amount(&CellValue::Number(50_000.0)),   Some(50_000.0));
        assert_eq!(parse_amount(&CellValue::Number(15_000.0)),   Some(15_000.0));
        assert_eq!(parse_amount(&CellValue::Number(1_234.567)),  Some(1_234.57));
        assert_eq!(parse_amount(&CellValue::Number(-1_500.0)),   Some(-1_500.0));
    }

    // ── Empty inputs ──────────────────────────────────────────────────────────

    #[test]
    fn empty_cell_is_none() {
        assert_eq!(parse_amount(&CellValue::Empty), None);
    }

    #[test]
    fn empty_string_is_none() {
        assert_eq!(parse_amount_str(""),    None);
        assert_eq!(parse_amount_str("  "),  None);
    }

    // ── Nil / N/A / dash strings ──────────────────────────────────────────────

    #[test]
    fn nil_strings_are_none() {
        assert_eq!(parse_amount_str("nil"),  None);
        assert_eq!(parse_amount_str("NIL"),  None);
        assert_eq!(parse_amount_str("n/a"),  None);
        assert_eq!(parse_amount_str("N/A"),  None);
        assert_eq!(parse_amount_str("-"),    None);
    }

    // ── Reference ID rejection (≥10 digits) ──────────────────────────────────

    #[test]
    fn bare_10_digit_is_none() {
        assert_eq!(parse_amount_str("1234567890"),       None); // 10 digits
        assert_eq!(parse_amount_str("12345678901234"),   None); // 14 digits
        assert_eq!(parse_amount_str("000000012345678"),  None); // 15 digits (ATM ref)
    }

    #[test]
    fn nine_digit_is_parsed() {
        // 9 digits is OK (≥100 crore is blocked separately, but 9 digits can be < 2e9)
        assert_eq!(parse_amount_str("100000000"), Some(100_000_000.0));
    }

    // ── ₹ symbol and Rs prefix ────────────────────────────────────────────────

    #[test]
    fn rupee_symbol_stripped() {
        assert_eq!(parse_amount_str("₹1,234.56"),   Some(1_234.56));
        assert_eq!(parse_amount_str("₹ 50,000.00"), Some(50_000.0));
    }

    #[test]
    fn rs_prefix_stripped() {
        assert_eq!(parse_amount_str("Rs.1234.56"), Some(1_234.56));
        assert_eq!(parse_amount_str("Rs 5000"),    Some(5_000.0));
        assert_eq!(parse_amount_str("RS 5000"),    Some(5_000.0));
    }

    // ── CR/DR suffix ──────────────────────────────────────────────────────────

    #[test]
    fn cr_suffix_stripped() {
        assert_eq!(parse_amount_str("1,500.00 CR"), Some(1_500.0));
        assert_eq!(parse_amount_str("2,800.00 Cr"), Some(2_800.0));
        assert_eq!(parse_amount_str("5000.00CR"),   Some(5_000.0));
    }

    #[test]
    fn dr_suffix_stripped() {
        assert_eq!(parse_amount_str("2,800.00 Dr"), Some(2_800.0));
        assert_eq!(parse_amount_str("1000DR"),      Some(1_000.0));
    }

    // ── Comma-formatted thousands ─────────────────────────────────────────────

    #[test]
    fn indian_comma_format_parsed() {
        assert_eq!(parse_amount_str("1,23,456.78"), Some(1_23_456.78));
        assert_eq!(parse_amount_str("10,000.00"),   Some(10_000.0));
        assert_eq!(parse_amount_str("1,00,000"),    Some(1_00_000.0));
    }

    // ── Parentheses → negative ────────────────────────────────────────────────

    #[test]
    fn parentheses_make_negative() {
        assert_eq!(parse_amount_str("(1,500.00)"), Some(-1_500.0));
        assert_eq!(parse_amount_str("(500)"),      Some(-500.0));
    }

    /// ICICI Wealth Management: "(-)" prefix followed by the amount.
    #[test]
    fn icici_wm_minus_prefix() {
        assert_eq!(parse_amount_str("(-) 4,85,878.84"), Some(-4_85_878.84));
        assert_eq!(parse_amount_str("(-)1000.00"),      Some(-1_000.0));
    }

    // ── Decimal place validation ──────────────────────────────────────────────

    #[test]
    fn more_than_2dp_rejected() {
        assert_eq!(parse_amount_str("123.456"),  None);
        assert_eq!(parse_amount_str("1234.567"), None);
    }

    #[test]
    fn exactly_2dp_accepted() {
        assert_eq!(parse_amount_str("123.45"),  Some(123.45));
        assert_eq!(parse_amount_str("1234.56"), Some(1_234.56));
    }

    // ── Zero ─────────────────────────────────────────────────────────────────

    #[test]
    fn zero_string_is_none() {
        assert_eq!(parse_amount_str("0"),    None);
        assert_eq!(parse_amount_str("0.00"), None);
        assert_eq!(parse_amount_str("0.0"),  None);
    }

    // ── Standard amounts from test-parser.js sample data ─────────────────────

    #[test]
    fn hdfc_sample_amounts() {
        assert_eq!(parse_amount_str("50000"),    Some(50_000.0));  // NEFT credit
        assert_eq!(parse_amount_str("10000"),    Some(10_000.0));  // ATM WDL
        assert_eq!(parse_amount_str("80000"),    Some(80_000.0));  // Salary
        assert_eq!(parse_amount_str("850"),      Some(850.0));     // Swiggy
        assert_eq!(parse_amount_str("3500"),     Some(3_500.0));   // BPCL
        assert_eq!(parse_amount_str("2800"),     Some(2_800.0));   // MSEDCL
        assert_eq!(parse_amount_str("15000"),    Some(15_000.0));  // UPI credit
        assert_eq!(parse_amount_str("18000"),    Some(18_000.0));  // GST
        assert_eq!(parse_amount_str("4500"),     Some(4_500.0));   // Amazon
        assert_eq!(parse_amount_str("12000"),    Some(12_000.0));  // LIC
        assert_eq!(parse_amount_str("850"),      Some(850.0));     // Interest
        assert_eq!(parse_amount_str("35000"),    Some(35_000.0));  // Rent
        assert_eq!(parse_amount_str("25000"),    Some(25_000.0));  // Income Tax
        assert_eq!(parse_amount_str("5000"),     Some(5_000.0));   // SIP
    }

    // ── fmt_amount ────────────────────────────────────────────────────────────

    #[test]
    fn fmt_none_is_empty() {
        assert_eq!(fmt_amount(None), "");
    }

    #[test]
    fn fmt_small_amounts() {
        assert_eq!(fmt_amount(Some(850.0)),   "850.00");
        assert_eq!(fmt_amount(Some(1_000.0)), "1,000.00");
        assert_eq!(fmt_amount(Some(999.99)),  "999.99");
    }

    #[test]
    fn fmt_lakh_range() {
        assert_eq!(fmt_amount(Some(50_000.0)),    "50,000.00");
        assert_eq!(fmt_amount(Some(1_00_000.0)),  "1,00,000.00");
        assert_eq!(fmt_amount(Some(9_99_999.0)),  "9,99,999.00");
    }

    #[test]
    fn fmt_crore_range() {
        assert_eq!(fmt_amount(Some(1_00_00_000.0)), "1,00,00,000.00");
        assert_eq!(fmt_amount(Some(1_23_456.78)),   "1,23,456.78");
    }

    #[test]
    fn fmt_negative() {
        assert_eq!(fmt_amount(Some(-1_500.0)), "-1,500.00");
        assert_eq!(fmt_amount(Some(-50_000.0)), "-50,000.00");
    }

    // ── format_indian_int (internal grouping helper) ──────────────────────────

    #[test]
    fn indian_int_grouping() {
        assert_eq!(format_indian_int(0),          "0");
        assert_eq!(format_indian_int(123),         "123");
        assert_eq!(format_indian_int(1_234),       "1,234");
        assert_eq!(format_indian_int(12_345),      "12,345");
        assert_eq!(format_indian_int(1_23_456),    "1,23,456");
        assert_eq!(format_indian_int(12_34_567),   "12,34,567");
        assert_eq!(format_indian_int(1_00_00_000), "1,00,00,000");
    }
}
