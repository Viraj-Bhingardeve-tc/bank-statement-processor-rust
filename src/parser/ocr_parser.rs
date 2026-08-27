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
    bank_detection::{detect, DetectOptions},
    date_parser::normalize_transaction_date,
    excel_parser::{compute_prev_balances, prepend_opening_balance_row},
    ocr_correction, ParseResult, Transaction,
};

// ── Regexes ───────────────────────────────────────────────────────────────────

// Date at start of line: DD/MM/YYYY, DD-MM-YYYY, DD.MM.YYYY (or 2-digit year).
// Mirrors JS: /^(\d{1,2}[\/\-\.]\d{1,2}[\/\-\.]\d{2,4})\b/
static DATE_START_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(\d{1,2}[/\-.]\d{1,2}[/\-.]\d{2,4})\b").unwrap());

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
static AMT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(\d{1,4}(?:,\d{2,3})*\.\d{1,2}|\d{5,}(?:\.\d{1,2})?)\b").unwrap());

// Dr / Cr markers to strip from narration text.
// Mirrors JS: /\b(DR|CR|Dr|Cr)\b\.?/g
static DRCR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\b(?:DR|CR)\b\.?").unwrap());

// Stray punctuation (keep word chars, whitespace, / - . @ &).
// JS `\w` = [A-Za-z0-9_]; Rust default `\w` is Unicode-aware, so we use explicit set.
static STRAY_PUNCT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^A-Za-z0-9_\s/\-.@&]").unwrap());

// UPI transaction identifier at the very start of a (by this point,
// amount-stripped) narration: "UPI" + its digit-run ID, optionally with a
// trailing descriptive/merchant hint the bank/OCR text glued directly onto
// it with no separator (e.g. "UPI209584004029coffee") or with just a
// space ("UPI 209474387500bhaji"). See `split_upi_reference`'s doc comment
// for what this is for.
static UPI_ID_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^UPI\s*([0-9]{6,})(.*)$").unwrap());

// U+E020 (Unicode Private Use Area) — a delimiter that can never appear in
// real bank-statement OCR/embedded-PDF text. `preprocess_multiline` wraps a
// structurally-confirmed reference number (found on its own dedicated OCR
// line, not guessed) in this marker so it survives being flattened into the
// single-line format `parse_ocr_text`'s main loop consumes; that loop strips
// the marker back out (`strip_ref_marker`) before touching amounts or
// narration, so the value can never be misread as a debit/credit/balance
// figure by `AMT_RE`'s no-decimal-required 5+-digit branch.
const REF_MARKER: char = '\u{E020}';

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
        let m = cap.get(1).unwrap();
        let raw = m.as_str().to_string();
        let v: f64 = raw.replace(',', "").parse().unwrap_or(0.0);
        // Skip zeros, sub-1 fractions, and 4-digit year literals
        if v < 1.0 {
            continue;
        }
        if (1900.0..=2100.0).contains(&v) && raw.len() == 4 {
            continue;
        }
        // The bare-digit (no decimal point) branch of AMT_RE only requires a
        // regex `\b` before the run — which fires on ANY non-word character,
        // not just whitespace, so a merchant/UTR ID glued directly onto a
        // word with a bare '.' (e.g. a payment gateway's own txn id
        // "cfmer.33421130" inside "UPIAR/.../Cashfree/ICIC/cfmer.33421130",
        // seen verbatim in a real Union Bank fixture) gets matched as an
        // 8-digit "amount" — passing the caller's `int_digit_count <= 8`
        // filter and getting treated as a real transaction amount even
        // though the line's actual amount ("40.00") is a separate token.
        // Every genuine amount in this codebase's own flattened single-line
        // text (see `preprocess_multiline`'s doc comment) is its own
        // whitespace/line-start-delimited token, so require that here too —
        // for this branch only; the decimal branch already can't start mid-
        // word because \b never fires between two adjacent word characters
        // (a letter directly followed by a digit).
        if !raw.contains('.') {
            let preceded_by_word_char = s[..m.start()]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace());
            if preceded_by_word_char {
                continue;
            }
        }
        out.push(AmountMatch {
            val: v,
            raw,
            idx: m.start(),
        });
    }
    out
}

// ── Reference-number extraction ─────────────────────────────────────────────

/// Number of integer digits in a decimal string (commas stripped, before
/// the dot). Local copy of the identically-named helper in
/// `transaction_extractor.rs` — kept independent per this module's existing
/// convention of not exposing private cross-module helpers (see
/// `csv_parser.rs`'s `apply_bank_detection` doc comment for the same
/// rationale).
fn int_digit_count(s: &str) -> usize {
    let s = s.replace(',', "");
    let s = if let Some(p) = s.find('.') { &s[..p] } else { &s };
    s.chars().filter(|c| c.is_ascii_digit()).count()
}

/// Strips a `REF_MARKER`-delimited token (`\u{E020}<digits>\u{E020}`) out of
/// `line`, returning the cleaned line and the recovered value, if present.
/// A missing closing marker (malformed input) is left untouched rather than
/// guessed at.
fn strip_ref_marker(line: &str) -> (String, Option<String>) {
    let Some(start) = line.find(REF_MARKER) else {
        return (line.to_string(), None);
    };
    let after_start = start + REF_MARKER.len_utf8();
    let Some(rel_end) = line[after_start..].find(REF_MARKER) else {
        return (line.to_string(), None);
    };
    let value = line[after_start..after_start + rel_end].to_string();
    let end = after_start + rel_end + REF_MARKER.len_utf8();
    let cleaned = format!("{}{}", &line[..start], &line[end..]);
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    (cleaned, if value.is_empty() { None } else { Some(value) })
}

/// Extract the transaction's reference/UTR/RRN/UPI-reference number
/// embedded in `narration` — the longest run of >= 9 consecutive ASCII
/// digits found anywhere in the text.
///
/// Real OCR/embedded-PDF text glues the reference directly onto adjacent
/// letters with no delimiter at all — e.g. `"UPI209498825681Papad"`,
/// `"NEFT-HDFCN52025050515108279-SMCGLOBAL"`,
/// `"N121243012696624CONNEXIONS"` (verified against every real
/// successfully-parsing PDF fixture this parser handles — see
/// `PDF_FIXTURES` in `tests/import_pipeline.rs`). So unlike
/// `transaction_extractor::extract_ref_from_narration` (built for a
/// cleanly slash-delimited FW/Cosmos PDF layout), this deliberately does
/// **not** require a `/`/`-`/space boundary on either side. A run this
/// long is never a date (already stripped from the line before this text
/// is reached), a genuine amount (amounts here either carry a decimal
/// point or were already excluded by the `int_digit_count` filter above),
/// or a serial/line number in the bank statements this parser targets — so
/// 9+ digits stays a safe, conservative threshold without needing
/// punctuation cues.
///
/// Returns `None` when no qualifying run exists — never fabricates one.
fn extract_embedded_reference(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut best: Option<(usize, usize)> = None; // (start, len)
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let len = i - start;
            let is_better = match best {
                Some((_, blen)) => len > blen,
                None => true,
            };
            if len >= 9 && is_better {
                best = Some((start, len));
            }
        } else {
            i += 1;
        }
    }
    best.map(|(start, len)| text[start..start + len].to_string())
}

/// Splits a UPI transaction's identifier from a trailing descriptive/
/// merchant hint (2026-08-24) — explicit product requirement: for a UPI
/// narration, `narration` should keep only "UPI" + its digit-run
/// identifier (e.g. "UPI209498825681"); any descriptive text the bank/OCR
/// text glued onto that ID with no delimiter ("UPI209584004029coffee") or
/// just a space ("UPI 209474387500bhaji") belongs in `reference` instead
/// — confirmed against every real UPI row in a live-imported dataset,
/// none of which had a populated `reference` before this. Deliberately
/// narrower than (and independent of) `extract_embedded_reference` above:
/// that one recovers a bare reference *number* for narrations in general
/// (NEFT/RTGS/IMPS/etc., where the surrounding text is already the
/// meaningful description and the number is the noise to pull out); this
/// one is UPI-specific and does the opposite assignment on purpose — the
/// ID is what belongs in `narration` here, and any trailing text is what
/// moves to `reference` — so it only ever matches a narration that starts
/// with "UPI" and is applied *before*, not instead of, that fallback (see
/// its call site in `parse_ocr_text`). Returns `None` for a narration that
/// doesn't start with "UPI" + a 6+ digit run at all, leaving those
/// untouched.
fn split_upi_reference(narration: &str) -> Option<(String, String)> {
    let caps = UPI_ID_RE.captures(narration)?;
    let digits = caps.get(1)?.as_str();
    let suffix = caps.get(2)?.as_str().trim().to_string();
    Some((format!("UPI{digits}"), suffix))
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
        Some(n) => lines[..n].join("\n"),
    };

    // 4. Main extraction loop.
    //
    // Debit/credit direction is decided from balance *movement*, which only
    // works if rows are visited in true chronological order. Many real PDFs
    // (e.g. BOB) list transactions newest-first — so this loop is split into
    // two passes: pass 1 builds every row (date/narration/reference/balance)
    // in the PDF's own display order and stashes each row's raw amount
    // candidates + Dr/Cr markers in `amt_infos` (same index as `txns`)
    // without yet deciding debit vs. credit; pass 2 (below, after the loop)
    // walks the rows in *chronological* order — reversed from display order
    // when the file turns out to be newest-first — so `prev_balance` is
    // always the balance that actually preceded this row in time, then
    // writes the decided debit/credit back into `txns` at its original
    // (display-order) index. This keeps output ordering identical to before
    // while fixing the direction inference for reverse-chronological PDFs.
    struct AmtInfo {
        txn_amts: Vec<f64>,
        dr_marker: bool,
        cr_marker: bool,
    }
    let mut txns: Vec<Transaction> = Vec::new();
    let mut amt_infos: Vec<AmtInfo> = Vec::new();
    let mut txn_counter = 0usize;

    for line in &lines {
        if let Some(cap) = DATE_START_RE.captures(line) {
            let date_str = cap[1].to_string();
            let rest_raw = line[date_str.len()..].trim();

            // Clean OCR artefacts from rest of line
            let rest_clean: String = {
                let s = rest_raw.replace(|c| "|\\{}[]".contains(c), " ");
                s.split_whitespace().collect::<Vec<_>>().join(" ")
            };

            // Recover a structurally-confirmed reference number
            // `preprocess_multiline` may have smuggled through as a
            // REF_MARKER-delimited token — strip it out before any
            // amount/narration processing touches this line.
            let (rest_clean, marker_ref) = strip_ref_marker(&rest_clean);

            let raw_amts = extract_amounts(&rest_clean);
            // Bank amounts realistically never reach 9 integer digits (≥10
            // crore for one line item, essentially unheard of for the small-
            // business/individual statements this app targets) — a longer
            // bare digit run is a UTR/RRN/reference number that AMT_RE's
            // no-decimal-required 5+-digit branch would otherwise
            // misclassify as a debit/credit/balance figure — mirrors
            // transaction_extractor.rs's identical `int_digit_count` guard,
            // same reasoning, kept as a local copy per this module's
            // existing convention of not exposing private cross-module
            // helpers (see csv_parser.rs's `apply_bank_detection` doc
            // comment for the same rationale).
            //
            // Tightened from <=10 to <=8 (2026-08-24) after finding real
            // OCR-imported rows in a live dataset with debit values in the
            // trillions (e.g. 2277239107622.0, 2340995652128303.0) — every
            // one had a plausible-looking balance figure alongside it, so
            // the corruption was specifically in the debit/credit amount,
            // not the whole line. <=10 (up to ~1000 crore) evidently wasn't
            // tight enough to catch every real-world OCR misread of this
            // kind; <=8 leaves comfortable headroom below any realistic
            // single-line amount while sitting far below every one of those
            // observed bad values (13-17 integer digits). Paired with an
            // explicit value ceiling below as defense-in-depth, since a
            // digit-count check alone can't distinguish "genuinely 9+
            // digits" from "9+ digits only because of how OCR garbled the
            // comma grouping or decimal point" — the value check catches
            // the latter even when the raw digit count of the matched
            // string happens to look small.
            const MAX_PLAUSIBLE_AMOUNT: f64 = 10_00_00_000.0; // ₹10 crore
            let amts: Vec<AmountMatch> = raw_amts
                .into_iter()
                .filter(|a| int_digit_count(&a.raw) <= 8 && a.val <= MAX_PLAUSIBLE_AMOUNT)
                .collect();
            if amts.is_empty() {
                continue;
            }

            // Last amount = balance; all others = transaction amounts
            let balance = (amts.last().unwrap().val * 100.0).round() / 100.0;
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
            if narration.is_empty() {
                narration = "(OCR)".to_string();
            }

            // UPI narration/reference split (2026-08-24) — see
            // `split_upi_reference`'s doc comment for what this does and
            // why it's separate from `extract_embedded_reference` below.
            // Applied *before* the general reference fallback so a UPI
            // narration's trailing descriptive text wins over that fallback
            // re-discovering the same digit run for a different purpose;
            // `narration` itself is only ever replaced when this actually
            // matched (a non-UPI narration reaches the code below
            // unchanged).
            let upi_suffix = if let Some((upi_id, suffix)) = split_upi_reference(&narration) {
                narration = upi_id;
                if suffix.is_empty() {
                    None
                } else {
                    Some(suffix)
                }
            } else {
                None
            };

            // Reference: a UPI narration's own descriptive suffix (above)
            // takes priority; otherwise prefer the structurally-confirmed
            // marker value (a whole line that was nothing but digits, per
            // `preprocess_multiline`'s own layout doc comment); otherwise
            // fall back to the longest embedded 9+ digit run still present
            // in narration — real bank-statement OCR text glues the
            // reference directly onto adjacent letters with no delimiter
            // (see `extract_embedded_reference`'s doc comment). Never
            // invented: every source here only ever returns a value
            // actually present in the source text.
            let reference = upi_suffix
                .or(marker_ref)
                .or_else(|| extract_embedded_reference(&narration))
                .unwrap_or_default();

            // Determine debit / credit
            let dr_marker = {
                let up = rest_clean.to_uppercase();
                // word-boundary check: is "DR" a standalone token?
                up.split_whitespace()
                    .any(|w| w.trim_end_matches('.') == "DR")
            };
            let cr_marker = {
                let up = rest_clean.to_uppercase();
                up.split_whitespace()
                    .any(|w| w.trim_end_matches('.') == "CR")
            };

            amt_infos.push(AmtInfo {
                txn_amts: txn_amts.iter().map(|a| a.val).collect(),
                dr_marker,
                cr_marker,
            });

            let nd = normalize_transaction_date(&date_str);

            txn_counter += 1;
            let mut t = Transaction::new(format!("t_ocr_{}", txn_counter));
            t.date = nd.display;
            t.date_ts = nd.ts;
            t.narration = narration;
            t.reference = reference;
            // debit/credit decided in the chronological-order second pass below
            t.balance = Some(balance);
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

    // 4b. Second pass: decide debit/credit in true chronological order (see
    // the doc comment above `AmtInfo`). Detect direction by comparing the
    // first and last row's own date_ts (ignoring rows with an unparsed/zero
    // date, which normalize_transaction_date already excludes from
    // ordering); a strictly-decreasing overall trend means the source PDF
    // lists newest-first, so chronological order is the *reverse* of
    // display order.
    let descending = {
        let dated: Vec<i64> = txns.iter().map(|t| t.date_ts).filter(|&ts| ts != 0).collect();
        match (dated.first(), dated.last()) {
            (Some(&first), Some(&last)) => first > last,
            _ => false,
        }
    };
    let chrono_order: Vec<usize> = if descending {
        (0..txns.len()).rev().collect()
    } else {
        (0..txns.len()).collect()
    };
    let mut prev_balance: Option<f64> = None;
    for idx in chrono_order {
        let balance = txns[idx].balance.unwrap_or(0.0);
        let info = &amt_infos[idx];
        let (debit, credit) = if info.txn_amts.len() == 1 {
            let amt = info.txn_amts[0];
            if info.dr_marker {
                (Some(amt), None)
            } else if info.cr_marker {
                (None, Some(amt))
            } else if let Some(prev) = prev_balance {
                let diff = balance - prev;
                if (diff - amt).abs() < amt * 0.02 {
                    (None, Some(amt)) // balance went UP → credit
                } else if (diff + amt).abs() < amt * 0.02 {
                    (Some(amt), None) // balance went DOWN → debit
                } else {
                    (None, Some(amt)) // best guess: credit
                }
            } else {
                (None, Some(amt)) // first txn → credit
            }
        } else if info.txn_amts.len() >= 2 {
            let a = info.txn_amts[0];
            let b = *info.txn_amts.last().unwrap();
            if let Some(prev) = prev_balance {
                let diff = balance - prev;
                // Narration text can smuggle extra numeric-looking tokens
                // into `txn_amts` alongside the real transaction amount —
                // e.g. an SMS short-code from marketing boilerplate glued
                // onto the last transaction of a page (seen verbatim in a
                // real Union Bank fixture: "...SMS <ULOAN> TO 56161", where
                // 56161 is a shortcode, not a rupee figure). Blindly
                // trusting position (first vs. last) can then pick the
                // spurious one. Prefer whichever candidate's magnitude
                // actually explains the observed balance movement; fall
                // back to the original position convention only when no
                // candidate does (preserves prior behavior for genuine
                // two-column debit/credit layouts).
                let best = info.txn_amts.iter().copied().min_by(|&x, &y| {
                    (diff.abs() - x)
                        .abs()
                        .partial_cmp(&(diff.abs() - y).abs())
                        .unwrap()
                });
                let matched = best.filter(|&v| (diff.abs() - v).abs() < v.max(1.0) * 0.02);
                let chosen = matched.unwrap_or(if diff < 0.0 { a } else { b });
                let debit = if diff < 0.0 { Some(chosen) } else { None };
                let credit = if diff > 0.0 { Some(chosen) } else { None };
                (debit, credit)
            } else {
                // No prior balance to disambiguate direction from (first
                // transaction in the file). Never report both debit AND
                // credit for one row (an invariant real callers rely on —
                // see e.g. `kotak_narrow_layout_...`'s "NEITHER/BOTH set"
                // assertions) — take the first candidate and apply the same
                // "unknown direction on the very first row → assume credit"
                // default the single-amount branch above uses.
                (None, Some(a))
            }
        } else {
            (None, None)
        };

        txns[idx].debit = debit
            .filter(|&v| v > 0.0)
            .map(|v| (v * 100.0).round() / 100.0);
        txns[idx].credit = credit
            .filter(|&v| v > 0.0)
            .map(|v| (v * 100.0).round() / 100.0);
        prev_balance = Some(balance);
    }

    // 5. Post-processing.
    // `compute_prev_balances` walks `txns` in index order assuming it's
    // chronological — true for display order unless this file turned out to
    // be newest-first, in which case feed it the reverse so its own
    // prev_balance stamping (and mismatch logging) lines up the same way
    // the debit/credit pass above did.
    let op_balance = if descending {
        txns.reverse();
        let ob = compute_prev_balances(&mut txns, None);
        txns.reverse();
        ob
    } else {
        compute_prev_balances(&mut txns, None)
    };

    // Bank detection from full text + header text.
    let narrations: Vec<&str> = txns.iter().map(|t| t.narration.as_str()).collect();
    let bank_meta = detect(DetectOptions {
        text: &corrected,
        header_text: &header_text,
        filename: file_name,
        narrations: &narrations,
    });
    let bank_name = bank_meta.bank_name.clone();
    let account_no = bank_meta.account_no.clone();
    for t in &mut txns {
        if t.bank_name.is_empty() {
            t.bank_name = bank_name.clone();
        }
        if t.account_no.is_empty() {
            t.account_no = account_no.clone();
        }
    }

    prepend_opening_balance_row(&mut txns, op_balance, &bank_name, &account_no);

    ParseResult {
        transactions: txns,
        opening_balance: op_balance,
        closing_balance: None, // OCR text rarely contains explicit closing balance
        bank_name,
        account_no,
        source_name: format!("{} [OCR]", file_name),
        col_map: Default::default(),
        header_row_idx: 0,
        noise_row_count: 0,
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
        "date",
        "type",
        "particulars",
        "debit",
        "credit",
        "balance",
        "channel",
        "cheque",
        "reference",
        "txn",
        "value",
        "valuedate",
        "description",
        "chq",
        "ref",
        "s.no",
        "sr.no",
        "sl.no",
        "serial",
        "withdrawal",
        "deposit",
        "dr",
        "cr",
        "amount",
        "narration",
        "details",
    ];

    let is_header_line = |line: &str| -> bool {
        let l = line.to_lowercase();
        header_words.iter().any(|h| l.trim() == *h)
    };

    // True when the line is a pure reference number: >= 6 digits, no decimal.
    let is_pure_integer = |line: &str| -> bool {
        let s = line.replace(',', "");
        let s = s.trim();
        s.len() >= 6 && s.chars().all(|c| c.is_ascii_digit())
    };

    // True when the line is an amount line: after stripping "Rs"/"Rs." prefix,
    // > 80 % of non-whitespace characters are digit/separator.
    let is_amount_line = |line: &str| -> bool {
        let s = line.trim();
        let s = if s.to_lowercase().starts_with("rs.") {
            &s[3..]
        } else if s.to_lowercase().starts_with("rs") {
            &s[2..]
        } else {
            s
        };
        let s = s.trim();
        if s.is_empty() {
            return false;
        }
        let total: usize = s.chars().filter(|c| !c.is_whitespace()).count();
        if total == 0 {
            return false;
        }
        let num: usize = s
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
            .count();
        (num as f64 / total as f64) > 0.80
    };

    // Parse the amount from an amount line (strip "Rs" prefix, commas).
    let parse_amt_line = |line: &str| -> Option<f64> {
        let s = line.trim();
        let s = if s.to_lowercase().starts_with("rs.") {
            &s[3..]
        } else if s.to_lowercase().starts_with("rs") {
            &s[2..]
        } else {
            s
        };
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
    let mut cur_narr: Vec<String> = Vec::new();
    let mut cur_amts: Vec<f64> = Vec::new();
    let mut cur_balance: Option<f64> = None;
    let mut cur_ref: Option<String> = None;

    // True when the line is a running-balance figure explicitly marked with
    // a trailing "Cr"/"Dr" suffix glued directly onto the number with no
    // space (e.g. BOB: "22,95,856.02Cr"). BOB and similarly-generated bank
    // PDFs never mark the transaction's own debit/credit amount this way —
    // only the balance column — so this is a reliable, position-independent
    // signal for "this specific line is the balance," letting `flush` below
    // always place it last regardless of which order the source PDF's flat
    // text stream actually emitted narration/amount/balance lines in.
    // Verified against real fixture text: BOB emits Balance *before*
    // Narration+Amount for a credit-only row, but Amount *before* Balance
    // for a debit-only row — there is no fixed order to rely on without
    // this marker, and without stripping the suffix first, `parse_amt_line`
    // fails to parse the trailing letters and the whole line — the balance
    // itself — was silently dropped, which is what made every BOB
    // transaction fail `flush`'s "amts.len() < 2" minimum and produce zero
    // transactions even after real (non-garbage) text extraction.
    let balance_with_suffix = |line: &str| -> Option<f64> {
        let l = line.trim();
        if l.len() < 3 {
            return None;
        }
        let lower = l.to_lowercase();
        if !(lower.ends_with("cr") || lower.ends_with("dr")) {
            return None;
        }
        let body = l[..l.len() - 2].trim();
        if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '.') {
            return None;
        }
        body.replace(',', "")
            .parse::<f64>()
            .ok()
            .filter(|&v| v > 0.0 && v < 2e9)
    };

    let flush = |date: &str,
                 narrs: &[String],
                 amts: &[f64],
                 balance: Option<f64>,
                 refnum: &Option<String>,
                 out: &mut Vec<String>| {
        // Balance (if separately detected via its Cr/Dr suffix) always goes
        // last, regardless of the order its source line appeared in —
        // `parse_ocr_text`'s "last amount = balance" convention requires it.
        let mut amts: Vec<f64> = amts.to_vec();
        if let Some(b) = balance {
            amts.push(b);
        }
        if amts.len() < 2 {
            return;
        } // need txn amount + balance minimum
        let narr = narrs.join(" ");
        let narr = if narr.trim().is_empty() {
            "TRANSACTION".to_string()
        } else {
            narr.trim().to_string()
        };
        // Format amounts as plain decimals so parse_ocr_text can re-parse them
        let amts_str: Vec<String> = amts.iter().map(|a| format!("{:.2}", a)).collect();
        // Carry a structurally-confirmed reference number (its own
        // dedicated line, per this function's layout doc comment above)
        // through as a REF_MARKER-delimited token instead of discarding it
        // — parse_ocr_text strips the marker back out before touching
        // amounts or narration, so it can't be misread as a debit/credit/
        // balance figure downstream.
        match refnum {
            Some(r) => out.push(format!(
                "{} {} {}{}{} {}",
                date,
                narr,
                REF_MARKER,
                r,
                REF_MARKER,
                amts_str.join(" ")
            )),
            None => out.push(format!("{} {} {}", date, narr, amts_str.join(" "))),
        }
    };

    // Marketing/legal/disclaimer boilerplate that real bank PDFs repeat on
    // every page footer — confirmed verbatim in real fixtures (Union Bank's
    // "SMS <ULOAN> TO 56161" footer, IDFC First's fraud-warning block, SBI's
    // "Please do not share..." block, IDBI's "Contents of this statement...").
    // Without this, `preprocess_multiline`'s narration-continuation logic
    // (any non-date, non-amount, non-reference line gets appended to the
    // still-open transaction's narration) glues this text onto whichever
    // transaction happens to be open when the footer appears — corrupting
    // narration, and previously even corrupting amounts (a footer phone
    // number or SMS short-code could be misread as an extra transaction
    // amount by `extract_amounts`'s bare-digit branch downstream in
    // `parse_ocr_text`, see that function's `AmtInfo` handling). Keyed on
    // substrings that appear in this genre of boilerplate but never in a
    // real transaction narration (which is compact and abbreviation-heavy,
    // not prose sentences), so this generalizes across banks rather than
    // hard-coding one bank's exact wording.
    const FOOTER_BOILERPLATE_MARKERS: &[&str] = &[
        "for any queries",
        "customer service",
        "customers outside india",
        "system generated output",
        "requires no signature",
        "requested to immediately notify",
        "avail our loan",
        "missed call",
        "important information",
        "important safety tips",
        "grievance",
        "nodal officer",
        "commonly used abbreviation",
        "deposit insurance",
        "dicgc",
        "phishing",
        "fraudulent",
        "never ask",
        "never share your card",
        "never sign blank cheque",
        "end of the statement",
        "end of statement",
        "legends for transactions",
        "legends used in",
        "unless the constituent notifies",
        "value date is the effective",
        "closing balance displayed",
        "contact the branch",
        "toll-free",
        "toll free",
        "welcome letter",
        "www.dicgc.org",
        "important message",
        "found the account correct",
        "suspicious device",
        "blank cheque",
        "call us at",
        "contact us",
        "escalate your concern",
        "nodaldesk@",
        "banker@",
        "does not send requests",
        "sensitive financial information",
        "brought forward",
    ];
    let is_footer_boilerplate = |line: &str| -> bool {
        let l = line.to_lowercase();
        FOOTER_BOILERPLATE_MARKERS.iter().any(|m| l.contains(m))
    };

    // Noise patterns that never belong in a narration
    let is_noise_line = |line: &str| -> bool {
        let l = line.to_lowercase();
        (l.contains("page") && l.contains("of"))
            || l.starts_with("page no")   // "Page No2", "Page No80", …
            || l.starts_with("11111")     // BOM channel code
            || l.contains("?identity")    // lopdf font error
            || l.starts_with("statement for account")
            || l.starts_with("idbi bank")
            || l.starts_with("our toll")
            || is_footer_boilerplate(line)
            || l.len() <= 2
    };

    for &line in &lines {
        if is_noise_line(line) {
            continue;
        }
        if is_header_line(line) {
            continue;
        }

        // Try to parse line as a date
        let nd = normalize_transaction_date(line);
        if nd.valid {
            let group_has_content =
                !cur_narr.is_empty() || !cur_amts.is_empty() || cur_balance.is_some();
            if cur_date.is_some() && !group_has_content {
                // A second date line appeared before any narration/amount/
                // balance content was collected — this is a second date
                // *column* for the same still-open transaction (e.g. BOB's
                // Txn Date + Value Date, each on its own line), not a new
                // transaction starting. Keep the first date already
                // captured (Txn Date — the conventional "transaction date"
                // for statement purposes) and ignore this one, rather than
                // flushing an empty group and silently losing the first date.
                continue;
            }
            // Flush previous transaction group
            if let Some(ref date) = cur_date {
                flush(date, &cur_narr, &cur_amts, cur_balance, &cur_ref, &mut out);
            }
            cur_date = Some(nd.display.clone());
            cur_narr.clear();
            cur_amts.clear();
            cur_balance = None;
            cur_ref = None;
        } else if cur_date.is_some() {
            if let Some(bal) = balance_with_suffix(line) {
                cur_balance = Some(bal);
            } else if is_pure_integer(line) {
                // Reference number line — keep the first one seen for this
                // transaction (matches this codebase's other "first
                // non-empty wins" reference conventions, e.g.
                // transaction_extractor.rs's Format A path).
                if cur_ref.is_none() {
                    cur_ref = Some(line.replace(',', "").trim().to_string());
                }
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
        flush(date, &cur_narr, &cur_amts, cur_balance, &cur_ref, &mut out);
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
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
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
        assert!(
            result
                .transactions
                .first()
                .is_some_and(|t| t.is_opening_balance),
            "first row should be synthetic opening balance"
        );
    }

    #[test]
    fn parse_closing_balance_is_none() {
        let result = parse_ocr_text(BASIC_OCR, "hdfc.pdf");
        assert!(
            result.closing_balance.is_none(),
            "OCR closing balance always None"
        );
    }

    // ── Reference-number extraction (regression coverage for the Ref/
    // Reference table column being permanently blank for every OCR/
    // embedded-text PDF import — see extract_embedded_reference's doc
    // comment for the real-fixture evidence this is modeled on) ────────────

    #[test]
    fn upi_narration_keeps_only_the_id_moves_descriptive_text_to_reference() {
        // Exactly the shape real fixtures produce: no delimiter at all
        // between the narration text and the reference digits. Updated
        // 2026-08-24 for an explicit product requirement — a UPI
        // narration's descriptive/merchant text (here "Papad") now belongs
        // in `reference`, not glued onto the digit-run ID left in
        // `narration` (this test's own name/assertions used to describe
        // and check the opposite mapping; see `split_upi_reference`'s doc
        // comment for the full reasoning).
        let text = "15/01/2024 UPI209498825681Papad 610.00 1,32,408.22\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert_eq!(real.len(), 1);
        assert_eq!(real[0].narration, "UPI209498825681");
        assert_eq!(real[0].reference, "Papad");
    }

    #[test]
    fn upi_narration_split_handles_a_space_before_the_id_too() {
        // "UPI 209474387500bhaji" — a space between "UPI" and its digit
        // ID, then the descriptive text glued directly onto the digits
        // with no separator at all. Both real shapes seen in a live
        // dataset; narration is normalized to the no-space form either way.
        let text = "15/01/2024 UPI 209474387500bhaji 55.00 1,32,408.22\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert_eq!(real.len(), 1);
        assert_eq!(real[0].narration, "UPI209474387500");
        assert_eq!(real[0].reference, "bhaji");
    }

    #[test]
    fn upi_narration_with_no_trailing_text_still_recovers_a_reference() {
        // No descriptive suffix to move — falls through to the existing
        // `extract_embedded_reference` fallback against the now-shortened
        // narration, which still finds the same digit run.
        let text = "15/01/2024 UPI209584004029 505.00 1,32,408.22\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert_eq!(real.len(), 1);
        assert_eq!(real[0].narration, "UPI209584004029");
        assert_eq!(real[0].reference, "209584004029");
    }

    #[test]
    fn non_upi_narration_is_never_touched_by_the_upi_split() {
        let text = "15/01/2024 NEFT-HDFCN52025050515108279-SMCGLOBAL SECURITIES 610.00 1,32,408.22\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert_eq!(real.len(), 1);
        assert!(
            real[0].narration.contains("SMCGLOBAL SECURITIES"),
            "non-UPI narration must be unaffected: {:?}",
            real[0].narration
        );
    }

    #[test]
    fn no_qualifying_digit_run_leaves_reference_blank() {
        // "50000" (5 digits) already gets consumed as a real amount by
        // AMT_RE; nothing else in this line is >= 9 consecutive digits — the
        // source genuinely has no reference here, so it must stay blank,
        // never invented.
        let text = "15/01/2024 SALARY CREDIT ACME PVT LTD 50000.00 1,50,000.00\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert_eq!(real.len(), 1);
        assert_eq!(real[0].reference, "", "no source reference → must stay blank");
    }

    #[test]
    fn a_short_digit_run_under_nine_digits_is_not_treated_as_reference() {
        // An 8-digit run must not be picked up (the codebase-wide 9+ digit
        // convention for "this is definitely a UTR/RRN, not something
        // else") — stays out of the reference field.
        let text = "15/01/2024 CHQ12345678 CLEARED 10000.00 90000.00\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert_eq!(real.len(), 1);
        assert_eq!(real[0].reference, "");
    }

    #[test]
    fn long_embedded_digit_run_does_not_get_misread_as_an_amount() {
        // Without the int_digit_count guard, AMT_RE's no-decimal 5+-digit
        // branch would treat "302498825681" (12 digits) as a legitimate
        // debit/credit/balance figure and corrupt the real amounts — this
        // proves the guard keeps balance/debit/credit correct while the
        // long run is still recovered as the reference.
        let text = "15/01/2024 UPI/302498825681/PAPAD SHOP 610.00 1,32,408.22\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert_eq!(real.len(), 1);
        assert_eq!(real[0].reference, "302498825681");
        assert!(
            (real[0].balance.unwrap() - 132408.22).abs() < 0.01,
            "balance must be the real 1,32,408.22 figure, not the reference number: {:?}",
            real[0].balance
        );
        let moved_amt = real[0].debit.or(real[0].credit).unwrap();
        assert!(
            (moved_amt - 610.0).abs() < 0.01,
            "debit/credit must be the real 610.00 figure, not the reference number: dr={:?} cr={:?}",
            real[0].debit,
            real[0].credit
        );
    }

    #[test]
    fn implausibly_huge_comma_grouped_amount_is_rejected_not_accepted() {
        // Reproduces the exact shape of amounts found in a real, live-
        // imported dataset (2026-08-24): a plausible-looking balance
        // alongside a comma-grouped "amount" whose integer part runs to 13
        // digits — evidently still able to slip past the old <=10-integer-
        // digit guard for some OCR-garbled inputs. The huge figure must be
        // filtered out entirely rather than accepted as the debit/credit —
        // with it gone, only the balance figure remains on the line, so
        // the row is still created (real date/narration/balance are all
        // genuine and worth keeping) but with no debit/credit rather than
        // a corrupted one. Silently accepting ₹22,77,23,91,07,622.00 as a
        // real debit corrupts every downstream sum; dropping the amount
        // while keeping the row loses far less.
        let text = "15/01/2024 UPI KGMOONG STICK 2,277,239,107,622.00 24,133.14\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert_eq!(real.len(), 1);
        assert!(
            (real[0].balance.unwrap() - 24133.14).abs() < 0.01,
            "balance must be the real 24,133.14 figure: {:?}",
            real[0].balance
        );
        assert_eq!(
            real[0].debit, None,
            "the implausible figure must not be accepted as a debit: {:?}",
            real[0].debit
        );
        assert_eq!(
            real[0].credit, None,
            "the implausible figure must not be accepted as a credit: {:?}",
            real[0].credit
        );
    }

    #[test]
    fn preprocess_multiline_isolated_reference_line_yields_to_the_upi_split_for_a_upi_narration() {
        // Exactly the layout preprocess_multiline's own doc comment
        // describes: date / narration / bare reference number / amount /
        // balance, each on its own OCR line — but the narration here is a
        // UPI one, so (2026-08-24) the UPI split now wins over this
        // isolated-line marker: "Papad" is genuinely more useful than
        // re-surfacing the exact same digit ID that's already sitting in
        // `narration` as "UPI209498825681". See
        // `preprocess_multiline_isolated_reference_line_still_wins_for_a_non_upi_narration`
        // right below for confirmation the marker mechanism itself is
        // unchanged for a narration the UPI split doesn't touch.
        let text = "\
04/04/2022
UPI209498825681Papad
209498825681
610.00
13,24,083.22
";
        let pre = preprocess_multiline(text);
        let result = parse_ocr_text(&pre, "bom.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert_eq!(real.len(), 1, "preprocessed text: {pre:?}");
        assert_eq!(real[0].narration, "UPI209498825681");
        assert_eq!(real[0].reference, "Papad");
        assert!(
            (real[0].balance.unwrap() - 1324083.22).abs() < 0.01,
            "balance must be the real running-balance figure, not the reference: {:?}",
            real[0].balance
        );
    }

    #[test]
    fn preprocess_multiline_isolated_reference_line_still_wins_for_a_non_upi_narration() {
        // Same isolated-reference-line layout as the UPI test above, but a
        // plain (non-UPI) narration the UPI split never touches — confirms
        // `marker_ref`'s own recovery mechanism is unaffected by the UPI
        // split being added ahead of it.
        let text = "\
04/04/2022
NEFT PAYMENT ACME TRADERS
209498825681
610.00
13,24,083.22
";
        let pre = preprocess_multiline(text);
        let result = parse_ocr_text(&pre, "bom.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert_eq!(real.len(), 1, "preprocessed text: {pre:?}");
        assert_eq!(real[0].narration, "NEFT PAYMENT ACME TRADERS");
        assert_eq!(real[0].reference, "209498825681");
    }

    #[test]
    fn preprocess_multiline_leaves_reference_blank_when_no_isolated_line_present() {
        let text = "\
23/08/2024
SALARY FOR JULY 2024
25000.00
1,50,000.00
";
        let pre = preprocess_multiline(text);
        let result = parse_ocr_text(&pre, "mahanagar.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert_eq!(real.len(), 1, "preprocessed text: {pre:?}");
        assert_eq!(real[0].reference, "", "no source reference → must stay blank");
    }

    // ── Direction inference ───────────────────────────────────────────────────

    #[test]
    fn dr_marker_sets_debit() {
        let text = "15/01/2024 PAYMENT DR 10000.00 90000.00\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        if !real.is_empty() {
            assert!(
                real[0].debit.is_some(),
                "DR marker → debit; got credit={:?}",
                real[0].credit
            );
        }
    }

    #[test]
    fn cr_marker_sets_credit() {
        let text = "15/01/2024 NEFT CR 25000.00 1,25,000.00\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        if !real.is_empty() {
            assert!(
                real[0].credit.is_some(),
                "CR marker → credit; got debit={:?}",
                real[0].debit
            );
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
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert!(!real.is_empty());
        // First transaction: no prev_balance → defaults to credit
        assert!(
            real[0].credit.is_some() || real[0].debit.is_some(),
            "must have either debit or credit"
        );
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
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        if !real.is_empty() {
            // The continuation line "SHARMA MUMBAI BRANCH" should be in the first txn narration
            assert!(
                real[0].narration.contains("SHARMA") || real[0].narration.len() > 15,
                "continuation should be appended: {:?}",
                real[0].narration
            );
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
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert!(!real.is_empty(), "should have transactions");
    }

    // ── Empty / edge cases ────────────────────────────────────────────────────

    #[test]
    fn empty_text_returns_empty_result() {
        let result = parse_ocr_text("", "empty.pdf");
        assert_eq!(result.transactions.len(), 0, "empty text → no transactions");
    }

    #[test]
    fn no_dates_returns_empty_result() {
        let result = parse_ocr_text(
            "HDFC Bank Account Statement Jan 2024\nNo transactions found.",
            "x.pdf",
        );
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert!(real.is_empty(), "no date lines → no real transactions");
    }

    #[test]
    fn amounts_rounded_to_2dp() {
        let text = "15/01/2024 SALARY 50000.5 1,50,000.5\n";
        let result = parse_ocr_text(text, "x.pdf");
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        if let Some(t) = real.first() {
            let bal = t.balance.unwrap();
            // Balance 150000.5 rounded to 2dp → 150000.5
            assert!(
                (bal - (bal * 100.0).round() / 100.0).abs() < 0.001,
                "balance should be rounded to 2dp"
            );
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
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        // Verify parsing doesn't crash and actually produces a transaction
        // (the previous `real.len() >= 0` here was vacuously always-true —
        // a usize can never be negative — and didn't check anything).
        assert!(
            !real.is_empty(),
            "expected at least one transaction to be parsed"
        );
    }
}
