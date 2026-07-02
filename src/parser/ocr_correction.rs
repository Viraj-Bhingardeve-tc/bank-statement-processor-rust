//! ocr_correction.rs — Port of `src/engines/ocr-correction.js`'s text-level
//! correction pipeline (the OLD app's `OCRCorrection.correctText()`).
//!
//! The old engine runs 4 passes per line: (1) char-substitution repair, (2)
//! fuzzy banking-term correction, (3) suspicious-amount flagging, (4)
//! suspicious-date flagging — but its only real caller, `Parser._parseOCRText`
//! (parser.js:2016), only ever reads `.text` back and discards the
//! issues/flags/quality-score fields. This port therefore reproduces the text
//! transformation exactly (passes 1+2, run twice — `correctText`'s default
//! `passes: 2`) and does not surface flags, matching what the old app's own
//! pipeline actually does with the result.
//!
//! Note: pass 1 here is a *separate* port of JS `_repairString`/`CHAR_MAP`
//! from `date_parser::repair_ocr_chars` — that function is tuned specifically
//! for short date-component tokens (max 4 chars, 9 noise chars, `[/\-.\s]+`
//! separators only) and is used elsewhere for date-string repair. JS's real
//! `_repairString` (used by `OCRCorrection.correctText`) allows tokens up to
//! 20 chars, a 13-char noise set, and more separators (`,`, `|`, `:` in
//! addition to `/`, `-`, `.`, whitespace) — reusing the narrower function here
//! would silently under-repair OCR text, so it's ported fresh below.

use once_cell::sync::Lazy;
use regex::Regex;

/// Known Indian banking terminology dictionary (ocr-correction.js:26-37).
const BANK_TERMS: &[&str] = &[
    "NEFT", "RTGS", "IMPS", "UPI", "NACH", "ACH", "ECS", "BBPS",
    "CREDIT", "DEBIT", "TRANSFER", "BALANCE", "INTEREST", "CHARGES",
    "DEPOSIT", "WITHDRAWAL", "CHEQUE", "CLEARING", "NARRATION",
    "SALARY", "PAYMENT", "RECEIPT", "VOUCHER",
    "AIRTEL", "BSNL", "RELIANCE", "VODAFONE",
    "MSEDCL", "BESCOM", "ELECTRICITY",
    "AMAZON", "FLIPKART", "SWIGGY", "ZOMATO",
    "HDFC", "ICICI", "AXIS", "KOTAK", "SBI",
    "REFUND", "REVERSAL", "BOUNCE", "RETURN",
    "SUSPENSE", "CONTRA", "JOURNAL",
];

static ALPHA_WORD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z]{4,}$").unwrap());
// Separator runs — approximates JS's `/(\s+|[\/\-\.\,\|:])/` (whitespace runs,
// or one punctuation char at a time) by treating any run of these characters
// as one separator. Differs only when multiple distinct punctuation marks
// appear back-to-back with no digits between them — vanishingly rare in real
// bank-statement OCR text.
static SEP_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\s/\-.,|:]+").unwrap());
// Test-class for "is this token plausibly numeric" — port of JS's
// `/^[0-9OoIlSZBGqQAg]+$/`. Note this charset does NOT include lowercase 'z',
// even though CHAR_MAP maps 'z'->'2' — a token containing 'z' fails this test
// and is left untouched, exactly reproducing that (undocumented) JS quirk.
fn is_plausibly_numeric(tok: &str) -> bool {
    !tok.is_empty()
        && tok.len() <= 20
        && tok.chars().all(|c| c.is_ascii_digit() || "OoIlSZBGqQAg".contains(c))
}

/// Port of `_repairNumericToken`'s `CHAR_MAP` substitution.
fn repair_numeric_token(tok: &str) -> String {
    tok.chars().map(|c| match c {
        'O' | 'o' => '0',
        'I' | 'l' => '1',
        'S'       => '5',
        'Z' | 'z' => '2',
        'B'       => '8',
        'G'       => '6',
        'q' | 'Q' => '9',
        'A'       => '4',
        'g'       => '9',
        other     => other,
    }).collect()
}

/// Port of `_repairString(s)` (ocr-correction.js:64-73): split on separator
/// runs, repair only the tokens that look plausibly numeric, pass separators
/// through unchanged.
fn repair_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut last = 0usize;
    for m in SEP_RE.find_iter(s) {
        let token = &s[last..m.start()];
        result.push_str(&repair_token(token));
        result.push_str(m.as_str());
        last = m.end();
    }
    result.push_str(&repair_token(&s[last..]));
    result
}

fn repair_token(tok: &str) -> String {
    if is_plausibly_numeric(tok) { repair_numeric_token(tok) } else { tok.to_string() }
}

/// Levenshtein edit distance — port of `_lev(a, b)` (ocr-correction.js:76-87).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for (i, row) in dp.iter_mut().enumerate() { row[0] = i; }
    for j in 0..=n { dp[0][j] = j; }
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = if a[i - 1] == b[j - 1] {
                dp[i - 1][j - 1]
            } else {
                1 + dp[i - 1][j].min(dp[i][j - 1]).min(dp[i - 1][j - 1])
            };
        }
    }
    dp[m][n]
}

/// Fuzzy-correct a word against the banking-term dictionary — port of
/// `_fuzzyTerm(word)` (ocr-correction.js:90-105).
fn fuzzy_correct_term(word: &str) -> String {
    let up = word.to_uppercase();
    if BANK_TERMS.contains(&up.as_str()) { return up; }

    let len = word.chars().count();
    if !(4..=15).contains(&len) { return word.to_string(); }
    let max_dist: i64 = if len <= 5 { 1 } else if len <= 8 { 2 } else { 3 };

    let mut best = word.to_string();
    let mut best_dist = i64::MAX;
    for &term in BANK_TERMS {
        let term_len = term.chars().count() as i64;
        if (term_len - len as i64).abs() > max_dist { continue; }
        let d = levenshtein(&up, term) as i64;
        if d < best_dist && d <= max_dist {
            best_dist = d;
            best = term.to_string();
        }
    }
    best
}

/// Correct one line of OCR text — port of `_correctLine`'s pass 1 (char
/// repair) + pass 2 (fuzzy banking-term correction). Passes 3/4 (suspicious
/// amount/date flagging) are intentionally not ported: the old app computes
/// but discards them at every call site (see module doc).
fn correct_line(line: &str) -> String {
    if line.trim().is_empty() { return line.to_string(); }

    // Pass 1: repair digit-look-alike characters.
    let text = repair_string(line);

    // Pass 2: fuzzy-correct likely-mangled banking terms (whole alphabetic
    // words, length >= 4 only) — matches JS's `text.split(/\s+/)` + rejoin,
    // which collapses whitespace runs to single spaces.
    let corrected: Vec<String> = text.split_whitespace().map(|w| {
        if !ALPHA_WORD_RE.is_match(w) { return w.to_string(); }
        let fixed = fuzzy_correct_term(w);
        let upper = w.to_uppercase();
        // JS only substitutes when the fuzzy result differs from BOTH the
        // original word and its plain uppercase form — an exact dictionary
        // hit (fixed == upper) is treated as "no change needed" and the
        // original casing of `w` is preserved.
        if fixed != upper && fixed != w { fixed } else { w.to_string() }
    }).collect();

    corrected.join(" ")
}

/// Correct a full block of OCR text — port of `OCRCorrection.correctText(text,
/// { passes })`. Old app's only caller (`_parseOCRText`) calls this with no
/// options, i.e. the default `passes: 2`.
pub fn correct_text(text: &str, passes: usize) -> String {
    let mut result = text.to_string();
    for _ in 0..passes {
        result = result.lines().map(correct_line).collect::<Vec<_>>().join("\n");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_dictionary_term_keeps_original_casing() {
        // "neft" uppercases to "NEFT", which is already a BANK_TERMS exact
        // match — JS's `fixed !== w.toUpperCase()` check is false here, so
        // the original lowercase word passes through unchanged.
        assert_eq!(correct_line("neft transfer received"), "neft transfer received");
    }

    #[test]
    fn mangled_term_gets_corrected() {
        // "NEDT" (1 substitution away from "NEFT") should fuzzy-correct.
        let out = correct_line("NEDT PAYMENT RECEIVED");
        assert!(out.contains("NEFT"), "expected NEFT correction, got: {}", out);
    }

    #[test]
    fn short_words_are_left_alone() {
        // "ATM" (3 chars) doesn't meet the {4,} length requirement for fuzzy pass.
        assert_eq!(correct_line("ATM WDL 5000"), "ATM WDL 5000");
    }

    #[test]
    fn char_repair_pass_applies_to_longer_numeric_tokens() {
        // "O5OOO.OO" repairs to "05000.00" — note this exercises the fuller
        // (up to 20 chars) numeric-token repair, unlike date_parser's
        // 4-char-capped variant.
        let out = correct_line("Amount O5OOO.OO paid");
        assert!(out.contains("05000.00"), "expected char repair, got: {}", out);
    }

    #[test]
    fn multi_pass_matches_default_of_two() {
        let out = correct_text("NEDT TRANSFER", 2);
        assert!(out.contains("NEFT"));
    }

    #[test]
    fn empty_line_untouched() {
        assert_eq!(correct_line(""), "");
        assert_eq!(correct_line("   "), "   ");
    }

    #[test]
    fn levenshtein_matches_known_values() {
        assert_eq!(levenshtein("NEFT", "NEFT"), 0);
        assert_eq!(levenshtein("NEDT", "NEFT"), 1);
        assert_eq!(levenshtein("KITTEN", "SITTING"), 3);
    }
}
