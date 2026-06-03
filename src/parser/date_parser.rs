//! Date parsing — port of `normalizeTransactionDate`, `_parseDate`,
//! `_dateStringToTimestamp`, and `_repairOCRChars` from parser.js.
//!
//! All output dates are in **"DD/MM/YYYY"** display format.
//! Timestamps are Unix **milliseconds** (i64), matching the JS `dateTs` field.
//! Only years in [2000, 2099] are accepted (mirrors JS `YEAR_OK`).
//!
//! ## Supported input formats (string path)
//!
//! | Pattern                     | Example           |
//! |-----------------------------|-------------------|
//! | DD/MM/YYYY, DD-MM-YYYY, DD.MM.YYYY | `"15/01/2024"` |
//! | DD/MM/YY (2-digit year)     | `"15/01/24"`      |
//! | DD MMM YYYY, DD-MMM-YYYY    | `"15 Jan 2024"`   |
//! | DDMmmYYYY (no separator)    | `"01Apr2026"`     |
//! | YYYY-MM-DD, YYYY/MM/DD      | `"2024-01-15"`    |
//! | YYYYMMDD (compact 8-digit)  | `"20240115"`      |
//! | Timestamp suffix stripped   | `"15/01/2024 18:36:14"` |
//! | OCR character repair        | `"O5/O1/2O24"` → `"05/01/2024"` |
//!
//! ## Excel serial path
//!
//! Call [`normalize_excel_date`] with the raw `f64` serial number.

use chrono::{Datelike, NaiveDate};
use once_cell::sync::Lazy;
use regex::Regex;

// ── Valid year range ──────────────────────────────────────────────────────────

const YEAR_MIN: i32 = 2000;
const YEAR_MAX: i32 = 2099;

// Excel serial: 36526 ≈ 2000-01-01, 54789 ≈ 2050-01-01
const EXCEL_MIN: f64 = 36_526.0;
const EXCEL_MAX: f64 = 54_789.0;

// ── Compiled regex patterns (lazily initialised) ──────────────────────────────

/// Strip trailing `"HH:MM"` or `"HH:MM:SS"` time component and everything after.
/// Handles: `"20/01/2026 18:36:14"` → `"20/01/2026"`.
static RE_TIME_SUFFIX: Lazy<Regex> = Lazy::new(||
    Regex::new(r"\s+\d{1,2}:\d{2}(?::\d{2})?.*$").unwrap()
);

/// DD/MM/YYYY, DD-MM-YYYY, DD.MM.YYYY (numeric month, any common separator).
static RE_DDMMYYYY: Lazy<Regex> = Lazy::new(||
    Regex::new(r"^(\d{1,2})[/\-.](\d{1,2})[/\-.](\d{2,4})$").unwrap()
);

/// DD MMM YYYY / DD-MMM-YYYY / DD/MMM/YYYY — space, dash or slash separator.
static RE_DD_MON_YYYY: Lazy<Regex> = Lazy::new(||
    Regex::new(r"^(\d{1,2})[\s\-/.]([A-Za-z]{3,})[\s\-/.](\d{2,4})$").unwrap()
);

/// DDMmmYYYY — NO separator between day, month-name, and year.
/// Example: `"01Apr2026"`.
static RE_DDMONYYYY_NOSEP: Lazy<Regex> = Lazy::new(||
    Regex::new(r"^(\d{1,2})([A-Za-z]{3,})(\d{2,4})$").unwrap()
);

/// YYYY-MM-DD or YYYY/MM/DD (ISO-style, 4-digit year first).
static RE_ISO: Lazy<Regex> = Lazy::new(||
    Regex::new(r"^(\d{4})[/\-](\d{1,2})[/\-](\d{1,2})$").unwrap()
);

/// Exactly 8 consecutive digits — YYYYMMDD compact.
static RE_COMPACT8: Lazy<Regex> = Lazy::new(||
    Regex::new(r"^\d{8}$").unwrap()
);

/// Separator characters used by `repair_ocr_chars`: runs of `/`, `-`, `.`, whitespace.
static RE_OCR_SEP: Lazy<Regex> = Lazy::new(||
    Regex::new(r"[/\-.\s]+").unwrap()
);

// ── Public result type ────────────────────────────────────────────────────────

/// Result of a date parse attempt — mirrors JS `normalizeTransactionDate` return.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedDate {
    /// `"DD/MM/YYYY"` on success; raw input string on failure.
    pub display: String,
    /// Unix timestamp in **milliseconds** (0 when `valid` is false).
    pub ts: i64,
    /// `true` only when a valid, in-range date was successfully parsed.
    pub valid: bool,
}

impl ParsedDate {
    fn ok(display: String, ts: i64) -> Self {
        ParsedDate { display, ts, valid: true }
    }

    fn fail(display: String) -> Self {
        ParsedDate { display, ts: 0, valid: false }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Main entry point for string date values.
///
/// Equivalent to `Parser.normalizeTransactionDate(rawDate)` from parser.js.
///
/// Tries, in order:
/// 1. Direct structured parse (`_parseDate` path).
/// 2. Strip trailing time component and retry.
/// 3. OCR character substitution repair and retry.
/// 4. Return invalid `ParsedDate` with the raw string preserved.
pub fn normalize_transaction_date(raw: &str) -> ParsedDate {
    let original = raw.trim();

    // 1. Direct parse
    if let Some(pd) = parse_date(original) {
        return pd;
    }

    // 2. Strip time suffix ("DD/MM/YYYY HH:MM:SS …") then retry
    let stripped = RE_TIME_SUFFIX.replace(original, "");
    let stripped = stripped.trim();
    if stripped != original {
        if let Some(pd) = parse_date(stripped) {
            return pd;
        }
    }

    // 3. OCR repair and retry
    let repaired = repair_ocr_chars(original);
    if repaired != original {
        if let Some(pd) = parse_date(&repaired) {
            return pd;
        }
    }

    // 4. Unrecoverable
    ParsedDate::fail(original.to_owned())
}

/// Parse an **Excel serial date** (f64) to a `ParsedDate`.
///
/// Excel epoch is 1899-12-30 (Lotus 1-2-3 compatibility).
/// Returns `None` for serials outside the expected 2000-2050 window.
pub fn normalize_excel_date(serial: f64) -> Option<ParsedDate> {
    if serial < EXCEL_MIN || serial > EXCEL_MAX {
        return None;
    }
    let epoch = NaiveDate::from_ymd_opt(1899, 12, 30)?;
    let days  = serial.floor() as u64;
    let date  = epoch.checked_add_days(chrono::Days::new(days))?;
    if !year_ok(date.year()) { return None; }
    Some(ParsedDate::ok(fmt_display(date), date_to_ts(date)))
}

/// Return `true` if `s` parses as a valid date via the core `parse_date` logic.
///
/// Equivalent to `this._parseDate(t.trim()) !== null` used in `_assignCells`.
/// Does NOT apply OCR repair or time-suffix stripping.
pub fn is_valid_date_str(s: &str) -> bool {
    parse_date(s.trim()).is_some()
}

/// Convert a `"DD/MM/YYYY"` display string to a Unix millisecond timestamp.
///
/// Returns `0` if the string cannot be parsed.
pub fn display_to_ts(s: &str) -> i64 {
    let parts: Vec<&str> = s.splitn(3, '/').collect();
    if parts.len() != 3 { return 0; }
    let dd: u32 = match parts[0].parse() { Ok(v) => v, Err(_) => return 0 };
    let mm: u32 = match parts[1].parse() { Ok(v) => v, Err(_) => return 0 };
    let yy: i32 = match parts[2].parse() { Ok(v) => v, Err(_) => return 0 };
    match NaiveDate::from_ymd_opt(yy, mm, dd) {
        Some(d) => date_to_ts(d),
        None    => 0,
    }
}

/// Replace characters that OCR engines commonly confuse with digits.
///
/// Only tokens whose **every character** is a digit or a known OCR-noise
/// character, AND whose length is ≤ 4, are repaired.
/// Separator runs (`/`, `-`, `.`, whitespace) are preserved unchanged.
///
/// OCR substitution map:
/// ```text
/// O/o → 0   I/l → 1   S → 5   Z → 2   B → 8   G → 6   q → 9
/// ```
pub fn repair_ocr_chars(s: &str) -> String {
    // Split on separator runs, preserving them in output.
    let mut result = String::with_capacity(s.len());
    let mut last   = 0usize;

    for m in RE_OCR_SEP.find_iter(s) {
        let token = &s[last..m.start()];
        result.push_str(&repair_token(token));
        result.push_str(m.as_str()); // separator unchanged
        last = m.end();
    }
    // Remaining token after the last separator
    result.push_str(&repair_token(&s[last..]));
    result
}

// ── Tally date formatter ──────────────────────────────────────────────────────

/// Port of `Parser.tallyDate(dateStr)`.
///
/// Converts a display date string to the `YYYYMMDD` format expected by Tally XML.
///
/// Supported inputs:
///   - `"DD/MM/YYYY"` or `"DD-MM-YYYY"` → `"YYYYMMDD"` (p[2].length === 4)
///   - `"YYYY-MM-DD"` or `"YYYY/MM/DD"` → `"YYYYMMDD"` (p[0].length === 4)
///   - Anything else → returned unchanged (matches JS fallback `return dateStr`)
pub fn tally_date(date_str: &str) -> String {
    if date_str.is_empty() { return String::new(); }
    let s = date_str.to_string();
    let parts: Vec<&str> = s.split(|c| c == '/' || c == '-').collect();
    if parts.len() == 3 {
        if parts[2].len() == 4 {
            // DD/MM/YYYY → YYYYMMDD
            return format!("{}{:0>2}{:0>2}", parts[2], parts[1], parts[0]);
        }
        if parts[0].len() == 4 {
            // YYYY-MM-DD → YYYYMMDD
            return format!("{}{:0>2}{:0>2}", parts[0], parts[1], parts[2]);
        }
    }
    s
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Core structured parse — mirrors `_parseDate()` from parser.js.
fn parse_date(s: &str) -> Option<ParsedDate> {
    let s = s.trim();
    if s.is_empty() { return None; }

    // Normalise internal whitespace (multiple spaces → single space)
    let s_norm: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s_norm.as_str();

    // ── DD/MM/YYYY, DD-MM-YYYY, DD.MM.YYYY ───────────────────────────────────
    if let Some(caps) = RE_DDMMYYYY.captures(s) {
        if let Ok(mm) = caps[2].parse::<u32>() {
            if let Some(date) = make_date(&caps[1], mm, &caps[3]) {
                return Some(ParsedDate::ok(fmt_display(date), date_to_ts(date)));
            }
        }
    }

    // ── DD MMM YYYY / DD-MMM-YYYY / DD/MMM/YYYY ──────────────────────────────
    if let Some(caps) = RE_DD_MON_YYYY.captures(s) {
        if let Some(mm) = month_num(&caps[2]) {
            if let Some(date) = make_date(&caps[1], mm, &caps[3]) {
                return Some(ParsedDate::ok(fmt_display(date), date_to_ts(date)));
            }
        }
    }

    // ── DDMmmYYYY (no separator, e.g. "01Apr2026") ───────────────────────────
    if let Some(caps) = RE_DDMONYYYY_NOSEP.captures(s) {
        if let Some(mm) = month_num(&caps[2]) {
            if let Some(date) = make_date(&caps[1], mm, &caps[3]) {
                return Some(ParsedDate::ok(fmt_display(date), date_to_ts(date)));
            }
        }
    }

    // ── YYYY-MM-DD / YYYY/MM/DD ───────────────────────────────────────────────
    if let Some(caps) = RE_ISO.captures(s) {
        let y: i32 = caps[1].parse().ok()?;
        let m: u32 = caps[2].parse().ok()?;
        let d: u32 = caps[3].parse().ok()?;
        if year_ok(y) {
            if let Some(date) = NaiveDate::from_ymd_opt(y, m, d) {
                return Some(ParsedDate::ok(fmt_display(date), date_to_ts(date)));
            }
        }
    }

    // ── YYYYMMDD (compact 8-digit) ────────────────────────────────────────────
    if RE_COMPACT8.is_match(s) {
        let y: i32 = s[0..4].parse().ok()?;
        let m: u32 = s[4..6].parse().ok()?;
        let d: u32 = s[6..8].parse().ok()?;
        if year_ok(y) {
            if let Some(date) = NaiveDate::from_ymd_opt(y, m, d) {
                return Some(ParsedDate::ok(fmt_display(date), date_to_ts(date)));
            }
        }
    }

    None
}

/// Build a `NaiveDate` from string `dd`, numeric `mm`, and string `yy`.
/// Handles 2-digit years (treated as 20xx).
fn make_date(dd: &str, mm: u32, yy: &str) -> Option<NaiveDate> {
    let d: u32 = dd.parse().ok()?;
    let raw_y: i32 = yy.parse().ok()?;
    let y = if yy.len() <= 2 { 2000 + raw_y } else { raw_y };
    if !year_ok(y) { return None; }
    NaiveDate::from_ymd_opt(y, mm, d)
}

fn year_ok(y: i32) -> bool {
    y >= YEAR_MIN && y <= YEAR_MAX
}

/// Format a `NaiveDate` as `"DD/MM/YYYY"`.
fn fmt_display(date: NaiveDate) -> String {
    format!("{:02}/{:02}/{}", date.day(), date.month(), date.year())
}

/// Convert a `NaiveDate` to a Unix millisecond timestamp.
/// All dates in [2000, 2099] are well after the Unix epoch, so the result is
/// always positive.
fn date_to_ts(date: NaiveDate) -> i64 {
    date.and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc().timestamp_millis())
        .unwrap_or(0)
}

/// Month name → 1-based month number (case-insensitive, 3-letter or full name).
fn month_num(s: &str) -> Option<u32> {
    match s.to_lowercase().as_str() {
        "jan" | "january"   => Some(1),
        "feb" | "february"  => Some(2),
        "mar" | "march"     => Some(3),
        "apr" | "april"     => Some(4),
        "may"               => Some(5),
        "jun" | "june"      => Some(6),
        "jul" | "july"      => Some(7),
        "aug" | "august"    => Some(8),
        "sep" | "september" => Some(9),
        "oct" | "october"   => Some(10),
        "nov" | "november"  => Some(11),
        "dec" | "december"  => Some(12),
        _                   => None,
    }
}

/// Repair a single non-separator token: substitute OCR-noise chars for digits.
fn repair_token(tok: &str) -> String {
    const NOISE: &str = "OoIlSZBGq";
    // Only repair when every char is a digit or known OCR-noise char AND len ≤ 4
    if tok.len() <= 4 && tok.chars().all(|c| c.is_ascii_digit() || NOISE.contains(c)) {
        tok.chars().map(|c| match c {
            'O' | 'o' => '0',
            'I' | 'l' => '1',
            'S'        => '5',
            'Z'        => '2',
            'B'        => '8',
            'G'        => '6',
            'q'        => '9',
            other      => other,
        }).collect()
    } else {
        tok.to_owned()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: assert a valid parse with expected display string.
    fn assert_date(input: &str, expected_display: &str) {
        let r = normalize_transaction_date(input);
        assert!(
            r.valid && r.display == expected_display,
            "input={:?}: expected display={:?}, got display={:?} valid={}",
            input, expected_display, r.display, r.valid
        );
        assert!(r.ts > 0, "input={:?}: ts should be > 0", input);
    }

    // Helper: assert a failed parse.
    fn assert_invalid(input: &str) {
        let r = normalize_transaction_date(input);
        assert!(
            !r.valid,
            "input={:?}: expected invalid, got valid={} display={:?}",
            input, r.valid, r.display
        );
        assert_eq!(r.ts, 0, "input={:?}: ts should be 0 for invalid", input);
    }

    // ── From test-parser.js dateTests array ──────────────────────────────────

    #[test]
    fn dd_mm_yyyy_slash() {
        assert_date("15/01/2024", "15/01/2024");
    }

    #[test]
    fn dd_mm_yyyy_dash() {
        assert_date("15-01-2024", "15/01/2024");
    }

    #[test]
    fn dd_mm_yyyy_dot() {
        assert_date("15.01.2024", "15/01/2024");
    }

    #[test]
    fn yyyy_mm_dd_iso() {
        assert_date("2024-01-15", "15/01/2024");
    }

    #[test]
    fn dd_mm_yy_two_digit_year() {
        assert_date("15/01/24", "15/01/2024");
    }

    #[test]
    fn yyyymmdd_compact() {
        assert_date("20240115", "15/01/2024");
    }

    #[test]
    fn dd_mon_yyyy_jan() {
        assert_date("15 Jan 2024", "15/01/2024");
    }

    #[test]
    fn dd_mon_yyyy_feb() {
        assert_date("01 Feb 2024", "01/02/2024");
    }

    // ── Additional format coverage ────────────────────────────────────────────

    #[test]
    fn dd_mon_yyyy_dash_sep() {
        assert_date("15-Jan-2024", "15/01/2024");
    }

    #[test]
    fn dd_mon_yyyy_slash_sep() {
        assert_date("15/Jan/2024", "15/01/2024");
    }

    #[test]
    fn ddmonyyyy_no_sep() {
        assert_date("01Apr2026", "01/04/2026");
        assert_date("31Dec2024", "31/12/2024");
    }

    #[test]
    fn yyyy_slash_mm_slash_dd() {
        assert_date("2024/01/15", "15/01/2024");
    }

    #[test]
    fn dd_mon_full_name() {
        assert_date("15 January 2024",   "15/01/2024");
        assert_date("01 February 2024",  "01/02/2024");
        assert_date("31 December 2024",  "31/12/2024");
    }

    // ── Time suffix stripping ─────────────────────────────────────────────────

    #[test]
    fn strips_time_suffix_hhmm() {
        assert_date("20/01/2026 18:36", "20/01/2026");
    }

    #[test]
    fn strips_time_suffix_hhmmss() {
        assert_date("20/01/2026 18:36:14", "20/01/2026");
    }

    // ── Invalid / noise inputs ────────────────────────────────────────────────

    #[test]
    fn empty_string_invalid() {
        assert_invalid("");
    }

    #[test]
    fn column_header_invalid() {
        assert_invalid("Date");
        assert_invalid("Narration");
        assert_invalid("Balance");
    }

    #[test]
    fn balance_row_label_invalid() {
        assert_invalid("Opening Balance");
        assert_invalid("Closing Balance");
        assert_invalid("Grand Total");
    }

    #[test]
    fn plain_number_invalid() {
        assert_invalid("12345");
        assert_invalid("98765.43");
    }

    // ── Calendar validation ───────────────────────────────────────────────────

    #[test]
    fn feb_30_invalid() {
        assert_invalid("30/02/2024");
    }

    #[test]
    fn apr_31_invalid() {
        assert_invalid("31/04/2024");
    }

    #[test]
    fn feb_29_leap_year_valid() {
        assert_date("29/02/2024", "29/02/2024"); // 2024 is a leap year
    }

    #[test]
    fn feb_29_non_leap_year_invalid() {
        assert_invalid("29/02/2023");
    }

    // ── Year range guard ──────────────────────────────────────────────────────

    #[test]
    fn year_1999_rejected() {
        assert_invalid("01/01/1999");
    }

    #[test]
    fn year_2100_rejected() {
        assert_invalid("01/01/2100");
    }

    #[test]
    fn year_2000_accepted() {
        assert_date("01/01/2000", "01/01/2000");
    }

    #[test]
    fn year_2099_accepted() {
        assert_date("31/12/2099", "31/12/2099");
    }

    // ── Excel serial date ─────────────────────────────────────────────────────

    #[test]
    fn excel_serial_2024_01_15() {
        // 2024-01-15 in Excel = 45306
        let pd = normalize_excel_date(45306.0).expect("should parse");
        assert_eq!(pd.display, "15/01/2024");
        assert!(pd.valid);
        assert!(pd.ts > 0);
    }

    #[test]
    fn excel_serial_out_of_range_is_none() {
        assert!(normalize_excel_date(100.0).is_none());   // 1900-era
        assert!(normalize_excel_date(60000.0).is_none()); // ~2064 - still in range, adjust
    }

    // ── display_to_ts ─────────────────────────────────────────────────────────

    #[test]
    fn display_to_ts_valid() {
        let ts = display_to_ts("15/01/2024");
        assert!(ts > 0);
        // 2024-01-15 00:00:00 UTC = 1705276800000 ms
        assert_eq!(ts, 1_705_276_800_000);
    }

    #[test]
    fn display_to_ts_invalid_returns_zero() {
        assert_eq!(display_to_ts(""), 0);
        assert_eq!(display_to_ts("not-a-date"), 0);
        assert_eq!(display_to_ts("30/02/2024"), 0); // invalid calendar
    }

    // ── repair_ocr_chars ─────────────────────────────────────────────────────

    #[test]
    fn ocr_repair_simple() {
        assert_eq!(repair_ocr_chars("O5/O1/2O24"), "05/01/2024");
    }

    #[test]
    fn ocr_repair_all_subs() {
        // O→0, I→1, S→5, Z→2, B→8, G→6, q→9
        assert_eq!(repair_token("O"),  "0");
        assert_eq!(repair_token("I"),  "1");
        assert_eq!(repair_token("S"),  "5");
        assert_eq!(repair_token("Z"),  "2");
        assert_eq!(repair_token("B"),  "8");
        assert_eq!(repair_token("G"),  "6");
        assert_eq!(repair_token("q"),  "9");
    }

    #[test]
    fn ocr_repair_preserves_month_names() {
        // "Jan" contains 'a', 'n' — not all OCR chars → no repair
        assert_eq!(repair_token("Jan"), "Jan");
        // Token > 4 chars → no repair
        assert_eq!(repair_token("January"), "January");
    }

    #[test]
    fn ocr_repair_preserves_separators() {
        let s = "15/O1/2O24";
        let repaired = repair_ocr_chars(s);
        assert_eq!(repaired, "15/01/2024");
    }

    #[test]
    fn ocr_repair_5_char_token_not_repaired() {
        // Token "OOOOO" (5 chars) should NOT be repaired — exceeds length limit
        assert_eq!(repair_token("OOOOO"), "OOOOO");
    }

    #[test]
    fn ocr_repaired_date_roundtrip() {
        // End-to-end: OCR garbled date → normalize → valid
        assert_date("O5/O1/2O24", "05/01/2024");
    }

    // ── Timestamp monotonicity ────────────────────────────────────────────────

    #[test]
    fn earlier_date_has_smaller_ts() {
        let a = normalize_transaction_date("01/01/2024");
        let b = normalize_transaction_date("31/12/2024");
        assert!(a.valid && b.valid);
        assert!(a.ts < b.ts, "earlier date should have smaller ts");
    }

    // ── Cross-format equivalence ──────────────────────────────────────────────

    // ── tally_date ────────────────────────────────────────────────────────────

    #[test]
    fn tally_date_dd_mm_yyyy() {
        assert_eq!(tally_date("15/01/2024"), "20240115");
    }

    #[test]
    fn tally_date_dd_dash_mm_dash_yyyy() {
        assert_eq!(tally_date("15-01-2024"), "20240115");
    }

    #[test]
    fn tally_date_yyyy_mm_dd_iso() {
        assert_eq!(tally_date("2024-01-15"), "20240115");
    }

    #[test]
    fn tally_date_empty_returns_empty() {
        assert_eq!(tally_date(""), "");
    }

    #[test]
    fn tally_date_unrecognized_passthrough() {
        assert_eq!(tally_date("15 Jan 2024"), "15 Jan 2024");
    }

    #[test]
    fn same_date_different_formats_same_ts() {
        let formats = [
            "15/01/2024",
            "15-01-2024",
            "15.01.2024",
            "2024-01-15",
            "2024/01/15",
            "20240115",
            "15 Jan 2024",
            "15-Jan-2024",
            "15Jan2024",
        ];
        let ts0 = normalize_transaction_date(formats[0]).ts;
        for fmt in &formats {
            let r = normalize_transaction_date(fmt);
            assert!(
                r.valid && r.ts == ts0,
                "format {:?}: expected ts={}, got ts={} valid={}",
                fmt, ts0, r.ts, r.valid
            );
        }
    }
}
