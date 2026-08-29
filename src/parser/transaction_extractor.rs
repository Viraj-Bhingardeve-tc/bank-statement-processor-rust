//! transaction_extractor.rs — Fixed-width PDF parsers.
//!
//! Ports two JS functions that handle non-column-based PDF layouts:
//!   `_parseFWRows`    → `extract_fw_transactions`   (generic fixed-width)
//!   `_parseCosmosFW`  → `extract_cosmos_transactions` (Cosmos Co-operative Bank)
//!
//! Both functions receive `Vec<Vec<PdfItem>>` where each row has exactly ONE item
//! (the full text line at a fixed X position).  All column detection is done by
//! character-position within that line, not by PDF X coordinates.

use crate::parser::{
    amount_parser::parse_amount_str,
    column_detector::PdfItem,
    date_parser::normalize_transaction_date,
    excel_parser::{compute_prev_balances, prepend_opening_balance_row},
    noise_filter::is_noise_row,
    ParseResult, Transaction,
};
use crate::text_safety::{floor_char_boundary, safe_prefix};

// ── Shared regex constants ────────────────────────────────────────────────────

// ── Constants used only in legacy/dead paths — kept for documentation ─────────
// These describe the fixed-width patterns; the active parsers use inline regexes.

/// Check if a string starts with a DD-MM-YYYY / DD/MM/YYYY date. Returns
/// the display string ("DD-MM-YYYY", always exactly 10 ASCII bytes) *and*
/// the actual byte length of that date pattern in `s` itself.
///
/// The two can differ for the *second* separator (between MM and YYYY):
/// `date_str` is always rebuilt with a 1-byte ASCII `-`, but `s` may use
/// a typographic dash there (minus sign U+2212 or en-dash U+2013 — both
/// 3 bytes, and both `.replace()`d to `-` in `normalized` below, so they
/// still produce a matching `Some` here). The *first* separator can't
/// diverge this way — `bytes[3]`/`bytes[4]` below must already be ASCII
/// digits for `m1` to pass, which is only possible if `bytes[2]` (the
/// first separator) is exactly 1 byte. A caller that reused
/// `date_str.len()` to locate where the date ends *in `s`* (Phase
/// 4L.2.2's crash-safety pass did exactly that) would undershoot by up
/// to 2 bytes when the second separator is typographic —
/// `floor_char_boundary` made that panic-safe, but it could still land a
/// couple of bytes short of the true end, leaking a trailing date digit
/// into whatever text is read after it. Returning the real length here
/// (Phase 4L.2.2 follow-up) closes that gap at the source instead.
fn starts_with_date(s: &str) -> Option<(String, usize)> {
    let s = s.trim();
    if s.len() < 10 {
        return None;
    }
    // First 10 chars: DD[-/]MM[-/]YYYY
    let bytes = s.as_bytes();
    let d1 = bytes[0].is_ascii_digit() && bytes[1].is_ascii_digit();
    let sep1 = bytes[2] == b'-' || bytes[2] == b'/' || bytes[2] == 0xE2; // em-dash is multi-byte
    let m1 = bytes[3].is_ascii_digit() && bytes[4].is_ascii_digit();
    if !d1 || !sep1 || !m1 {
        return None;
    }
    // Find separator positions allowing em-dash (3 bytes)
    let normalized = s.replace(['\u{2212}', '\u{2013}'], "-");
    // `\u{2014}` (em-dash) isn't replaced above, so it — or any other
    // stray multi-byte character in this heuristically-detected date
    // region — can still be present here; `floor_char_boundary` keeps
    // this byte-10 cut from panicking on it (Phase 4L.2.2). Note this
    // means an em-dash-separated date already fails to match below (the
    // `.split` predicate only recognizes ASCII `-`/`/`, so an
    // un-replaced em-dash leaves `parts.len() < 3`) — safely rejected,
    // not silently mis-parsed.
    let cut = floor_char_boundary(&normalized, 10.min(normalized.len()));
    let parts: Vec<&str> = normalized[..cut].split(['-', '/']).collect();
    if parts.len() < 3 {
        return None;
    }
    if parts[0].len() == 2 && parts[1].len() == 2 && parts[2].len() == 4 {
        let date_str = format!("{}-{}-{}", parts[0], parts[1], parts[2]);
        // Walk `s`'s own bytes for the real separator widths, rather
        // than assuming `date_str.len()` (always 10) matches them.
        let sep1_len = separator_byte_len(s, 2)?;
        let sep2_len = separator_byte_len(s, 2 + sep1_len + 2)?;
        let orig_len = (2 + sep1_len + 2 + sep2_len + 4).min(s.len());
        return Some((date_str, orig_len));
    }
    None
}

/// Byte width of the date-separator character at `s`'s byte offset
/// `at`: 1 for ASCII `-`/`/`, 3 for a typographic dash (minus sign,
/// en-dash, or em-dash — the only multi-byte separators this parser
/// recognizes, matching `starts_with_date`'s own `bytes[2] == 0xE2`
/// check). `None` if `at` doesn't hold a recognized separator at all —
/// should not happen once `starts_with_date` has already matched the
/// date pattern via `parts`, but never guessed at (Phase 4L.2.2).
fn separator_byte_len(s: &str, at: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if at >= bytes.len() {
        return None;
    }
    match bytes[at] {
        b'-' | b'/' => Some(1),
        0xE2 if at + 3 <= bytes.len() && s.is_char_boundary(at + 3) => Some(3),
        _ => None,
    }
}

/// Extract all decimal amounts from a string.
/// Returns `(value, start_index, raw_string)` tuples.
fn extract_amounts(s: &str) -> Vec<(f64, usize, String)> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < s.len() {
        // Find a digit
        let digit_pos = match s[start..].find(|c: char| c.is_ascii_digit()) {
            Some(p) => start + p,
            None => break,
        };
        // Collect digits, commas
        let mut end = digit_pos;
        while end < s.len() {
            let c = s.as_bytes()[end];
            if c.is_ascii_digit() || c == b',' {
                end += 1;
            } else {
                break;
            }
        }
        // Require decimal point + digits
        if end < s.len() && s.as_bytes()[end] == b'.' {
            end += 1;
            let dec_start = end;
            while end < s.len() && s.as_bytes()[end].is_ascii_digit() {
                end += 1;
            }
            if end - dec_start >= 1 && end - dec_start <= 2 {
                let raw = &s[digit_pos..end];
                let val = raw.replace(',', "").parse::<f64>().unwrap_or(0.0);
                if val > 0.0 {
                    out.push((val, digit_pos, raw.to_string()));
                }
            }
        }
        start = end.max(digit_pos + 1);
    }
    out
}

/// Number of integer digits in a decimal string (commas stripped, before the dot).
fn int_digit_count(s: &str) -> usize {
    let s = s.replace(',', "");
    let s = if let Some(p) = s.find('.') {
        &s[..p]
    } else {
        &s
    };
    s.chars().filter(|c| c.is_ascii_digit()).count()
}

/// Extract the balance from the end of a line: "NNN.NNCr" or "NNN.NNDr".
fn extract_balance_suffix(line: &str) -> Option<(f64, usize, &str)> {
    let lower = line.to_lowercase();
    let suffix = if lower.trim_end().ends_with("cr") || lower.trim_end().ends_with("dr") {
        2
    } else {
        return None;
    };
    let base = line.trim_end();
    let base = &base[..base.len() - suffix].trim_end();
    let amts = extract_amounts(base);
    if let Some((val, idx, raw)) = amts.last() {
        let tail_start = base.rfind(raw.as_str()).unwrap_or(*idx);
        return Some((*val, tail_start, &line[tail_start..]));
    }
    None
}

// ── Narration keywords for direction inference ────────────────────────────────

fn is_debit_narr(nl: &str) -> bool {
    // Matches JS: /upi[-\s]?dr[/\s]|[/-]dr[/-]|\bdr\b.*[/-]|neft.*dr\b|rtgs.*dr\b|by\s+debit|cash\s*wd|atm\s*wd|chg\s*dr/
    nl.contains("upi-dr")
        || nl.contains("upi dr")
        || (nl.contains("/dr/") || nl.contains("-dr-"))
        || nl.contains("neft") && nl.contains("dr")
        || nl.contains("rtgs") && nl.contains("dr")
        || nl.contains("cash wd")
        || nl.contains("atm wd")
        || nl.contains("by debit")
        || nl.contains("chg dr")
}

fn is_credit_narr(nl: &str) -> bool {
    // Matches JS: /upi[-\s]?cr[/\s]|[/-]cr[/-]|\bcr\b.*[/-]|neft.*cr\b|rtgs.*cr\b|by\s+cr\b|interest\b|refund|reversal|salary/
    nl.contains("upi-cr")
        || nl.contains("upi cr")
        || (nl.contains("/cr/") || nl.contains("-cr-"))
        || nl.contains("neft") && nl.contains("cr")
        || nl.contains("rtgs") && nl.contains("cr")
        || nl.contains("interest")
        || nl.contains("refund")
        || nl.contains("reversal")
        || nl.contains("salary")
        || nl.contains("by cr")
}

// ── extract_fw_transactions ───────────────────────────────────────────────────

/// Port of `Parser._parseFWRows(rows, fileName)`.
///
/// Handles fixed-width PDF layouts where each row is a single text item containing
/// the full transaction line.  Detects two formats:
///
/// **Format A** — Two separate Withdrawal / Deposit columns.
/// Header contains "withdrawal" or "debit" AND "deposit" or "credit".
/// Column midpoint (`col_mid`) splits debit from credit by character position.
///
/// **Format B** — Single amount column (Cosmos Co-op style before the dedicated parser).
/// Header has "amount" or "amt" but NOT "withdrawal" or "deposit".
/// Direction inferred from narration keywords; balance movement corrects.
///
/// Returns `None` when no valid header is found or no transactions are extracted.
pub fn extract_fw_transactions(
    rows: &[Vec<PdfItem>],
    file_name: &str,
) -> Option<(Vec<Transaction>, Option<f64>, Option<f64>)> {
    // ── Header detection ──────────────────────────────────────────────────────
    let mut hdr_idx = usize::MAX;
    let mut wd_pos: i32 = -1; // char position of "withdrawal/debit" keyword
    let mut dep_pos: i32 = -1; // char position of "deposit/credit" keyword
    let mut single_amt = false;

    for (i, row) in rows.iter().enumerate().take(30) {
        let line = row.first().map_or("", |it| it.text.as_str());
        let ll = line.to_lowercase();
        if !ll.contains("date") || !ll.contains("balance") {
            continue;
        }

        if (ll.contains("withdrawal") || ll.contains("debit"))
            && (ll.contains("deposit") || ll.contains("credit"))
        {
            hdr_idx = i;
            // Find character positions of withdrawal/deposit keywords
            let wp = ll
                .find("withdrawal")
                .or_else(|| ll.find("debit"))
                .map(|p| p as i32)
                .unwrap_or(-1);
            let dp = ll
                .find("deposit")
                .or_else(|| ll.find("credit"))
                .map(|p| p as i32)
                .unwrap_or(-1);
            if wp >= 0 {
                wd_pos = wp;
            }
            if dp >= 0 {
                dep_pos = dp;
            }
            break;
        }

        let single = ll.contains("amount")
            || ll.contains("amt")
            || ll.contains("txn amt")
            || ll.contains("transaction amount");
        if single && !ll.contains("withdrawal") && !ll.contains("deposit") {
            hdr_idx = i;
            single_amt = true;
            break;
        }
    }
    if hdr_idx == usize::MAX {
        return None;
    }

    // Format A midpoint between withdrawal and deposit headers
    let col_mid: i32 = if !single_amt {
        if wd_pos >= 0 && dep_pos >= 0 {
            (wd_pos + dep_pos) / 2
        } else if dep_pos >= 0 {
            dep_pos
        } else {
            70
        }
    } else {
        0
    };

    // ── Transaction loop ──────────────────────────────────────────────────────
    let mut txns: Vec<Transaction> = Vec::new();
    let mut op_balance: Option<f64> = None;
    let mut closing_balance: Option<f64> = None;
    let mut txn_counter = 0usize;

    for (i, row) in rows.iter().enumerate().skip(hdr_idx + 1) {
        let line = row.first().map_or("", |it| it.text.as_str());
        let line = line.trim();
        if line.is_empty() || line.chars().all(|c| c == '-' || c == '=' || c == ' ') {
            continue;
        }

        // Require a date at the start of the line
        let (date_str, date_orig_len) = match starts_with_date(line) {
            Some(d) => d,
            None => continue,
        };
        let nd = normalize_transaction_date(&date_str);
        if !nd.valid {
            continue;
        }

        // Require balance suffix: "NNN.NNCr" or "NNN.NNDr"
        let (balance, bal_start, _bal_raw) = match extract_balance_suffix(line) {
            Some(b) => b,
            None => continue,
        };

        // The portion between date and balance. `date_orig_len` is the
        // date pattern's real byte length *in `line`* (from
        // `starts_with_date`, Phase 4L.2.2 follow-up) — using
        // `date_str.len()` here instead would undershoot on a
        // typographic-dash date, leaking trailing date bytes into
        // `narration`/`middle` below (not just an unsafe slice — a wrong
        // one). `floor_char_boundary` still guards the `+1` for the
        // trailing space, which `date_orig_len` doesn't account for.
        // `bal_start` is always boundary-safe already (found via `rfind`
        // on a same-offset-aligned substring).
        let date_part_len = floor_char_boundary(line, date_orig_len + 1);
        let after_date = if date_part_len < line.len() {
            &line[date_part_len..]
        } else {
            ""
        };
        let middle = if bal_start > date_part_len {
            &line[date_part_len..bal_start]
        } else {
            after_date
        };

        let ml = middle.to_lowercase();

        // Opening/closing balance markers
        if ml.contains("opening bal") || ml.contains("op bal") {
            op_balance = Some(balance);
            continue;
        }
        if ml.contains("closing bal") || ml.contains("cl bal") {
            closing_balance = Some(balance);
            continue;
        }

        let mut debit: Option<f64> = None;
        let mut credit: Option<f64> = None;
        let narration: String;
        let mut reference = String::new();

        if single_amt {
            // ── Format B (single amount column) ──────────────────────────────
            // All decimal amounts in `middle`; reject >10 int-digit UTR values.
            let real_amts: Vec<(f64, usize, String)> = extract_amounts(middle)
                .into_iter()
                .filter(|(_, _, raw)| int_digit_count(raw) <= 10)
                .collect();
            if real_amts.is_empty() {
                continue;
            }

            let txn_amt = real_amts.last().unwrap();
            narration = middle[..txn_amt.1].trim().to_string();

            // Extract plain integer reference (9+ digit sequence)
            if let Some(cap) = extract_ref_from_narration(&narration) {
                reference = cap;
            }

            let nl = narration.to_lowercase();
            if is_debit_narr(&nl) && !is_credit_narr(&nl) {
                debit = Some(txn_amt.0);
            } else if is_credit_narr(&nl) && !is_debit_narr(&nl) {
                credit = Some(txn_amt.0);
            } else {
                debit = Some(txn_amt.0); // ambiguous → balance pass corrects
            }
        } else {
            // ── Format A (two-column: Withdrawal | Deposit) ───────────────────
            let all_amts: Vec<(f64, usize, String)> = extract_amounts(middle).into_iter().collect();

            // Narration = everything before the first decimal token
            if let Some(&(_, idx, _)) = all_amts.first() {
                narration = middle[..idx].trim().to_string();
            } else {
                narration = middle.trim().to_string();
            }

            // Classify each amount as reference or transaction amount
            let mut txn_amts: Vec<(f64, i32)> = Vec::new(); // (value, abs_start in original line)
            for (val, mid_idx, raw) in &all_amts {
                let abs_start = (date_part_len + mid_idx) as i32;
                let is_ref_by_digits = int_digit_count(raw) > 10;
                let is_ref_by_pos = wd_pos >= 0 && abs_start < (wd_pos - 5);
                if is_ref_by_digits || is_ref_by_pos {
                    if reference.is_empty() {
                        reference = if is_ref_by_digits {
                            val.round().to_string()
                        } else {
                            raw.replace(".00", "").replace(',', "")
                        };
                    }
                } else {
                    txn_amts.push((*val, abs_start));
                }
            }

            // Assign by position vs midpoint
            for &(val, abs_start) in &txn_amts {
                if abs_start < col_mid {
                    debit = Some(val);
                } else {
                    credit = Some(val);
                }
            }
        }

        if debit.is_none() && credit.is_none() {
            continue;
        }

        txn_counter += 1;
        let mut t = Transaction::new(format!("t_fw_{}_{}", i, txn_counter));
        t.date = nd.display;
        t.date_ts = nd.ts;
        t.narration = narration;
        t.reference = reference;
        t.debit = debit;
        t.credit = credit;
        t.balance = Some(balance);
        t.bank_name = file_name.to_string();
        txns.push(t);
    }

    if txns.is_empty() {
        return None;
    }

    // ── Balance-direction post-pass ───────────────────────────────────────────
    // Corrects Format B misclassifications (and catches Format A edge cases).
    {
        let mut prev_bal = op_balance;
        if prev_bal.is_none() {
            if let Some(seed) = txns
                .iter()
                .find(|t| t.balance.is_some() && (t.debit.is_some() || t.credit.is_some()))
            {
                prev_bal = Some(
                    ((seed.balance.unwrap() - seed.credit.unwrap_or(0.0)
                        + seed.debit.unwrap_or(0.0))
                        * 100.0)
                        .round()
                        / 100.0,
                );
            }
        }
        for t in &mut txns {
            let bal = match t.balance {
                Some(b) => b,
                None => continue,
            };
            let prev = match prev_bal {
                Some(p) => p,
                None => {
                    prev_bal = Some(bal);
                    continue;
                }
            };
            let tol = |amt: f64| (amt * 0.02_f64).max(1.0);

            if t.debit.is_some() && t.credit.is_none() {
                let diff = bal - prev;
                let amt = t.debit.unwrap();
                if (diff - amt).abs() < tol(amt) {
                    t.credit = Some(amt);
                    t.debit = None; // balance went UP → credit
                }
            } else if t.credit.is_some() && t.debit.is_none() {
                let diff = bal - prev;
                let amt = t.credit.unwrap();
                if (diff + amt).abs() < tol(amt) {
                    t.debit = Some(amt);
                    t.credit = None; // balance went DOWN → debit
                }
            }
            prev_bal = Some(bal);
        }
    }

    Some((txns, op_balance, closing_balance))
}

/// Extract a 9+ digit reference number embedded in a narration
/// (sequences that appear between slashes or at segment start/end).
pub fn extract_ref_from_narration(narr: &str) -> Option<String> {
    // Look for 9+ consecutive digit sequences bounded by /, -, space, or string start/end
    let mut i = 0;
    let bytes = narr.as_bytes();
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let len = i - start;
            if len >= 9 {
                // Check boundaries: must be preceded/followed by /, -, space, or string boundary
                let pre_ok = start == 0 || matches!(bytes[start - 1], b'/' | b'-' | b' ');
                let post_ok = i >= bytes.len() || matches!(bytes[i], b'/' | b'-' | b' ');
                if pre_ok && post_ok {
                    return Some(narr[start..i].to_string());
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

// ── extract_icici_wealth_transactions ─────────────────────────────────────────

/// Parser for ICICI Bank Wealth Management PDF statements — a genuinely
/// different layout from a normal ICICI Bank statement, not just a
/// re-skin, requiring its own extractor rather than being forced through
/// `extract_fw_transactions`/the main column-loop in `pdf_parser.rs`.
///
/// Real bug fixed (2026-08-28): this statement's page content has *zero*
/// embedded text — every character on every page is drawn as vector
/// line-art (see `ocr_extractor::extract_pages_via_ocr`'s doc comment) — so
/// the `Vec<Vec<PdfItem>>` this function receives always comes from OCR
/// word-boxes, never from real PDF text. That has one structural
/// consequence this extractor is built around: each transaction's
/// Particulars column wraps 2–4 physical lines, and — because the table
/// cell is vertically centered — the Date/Deposits/Withdrawals/Balance
/// values can land on *any* of those physical lines, not reliably the
/// first or last. So unlike every other extractor in this module (which
/// treat "narration accumulates on rows *before* the date+amount row"),
/// this one groups by **block**: every row from one valid-date row up to
/// (not including) the next valid-date row belongs to the same
/// transaction, and the Deposits/Withdrawals/Balance amount is taken from
/// *wherever in the block* a column produces one — not from the date row
/// specifically.
///
/// Layout (header repeats on every page):
///   Date | Mode** | Particulars | Deposits | Withdrawals | Balance
///
/// Gated on `"wealth management"` appearing in the page text so it can
/// never fire for (and thus can never regress) a normal ICICI Bank
/// statement, which does not contain that phrase.
pub fn extract_icici_wealth_transactions(rows: &[Vec<PdfItem>], file_name: &str) -> Option<ParseResult> {
    use crate::parser::column_detector::{assign_cells, calc_col_boundaries, ColField, PdfColX};

    let early_text: String = rows
        .iter()
        .take(40)
        .map(|r| {
            r.iter()
                .map(|it| it.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let el = early_text.to_lowercase();
    if !el.contains("wealth management")
        && !(el.contains("mode**") && el.contains("deposits") && el.contains("withdrawals"))
    {
        return None;
    }

    // Header located directly by keyword rather than via `find_pdf_header`'s
    // generic scorer: that scorer requires the Narration/Particulars column
    // to score too, and OCR sometimes garbles "PARTICULARS" into something
    // unrecognizable (observed: a lone "a") even when Date/Deposits/
    // Withdrawals/Balance all read cleanly on the very same row — in that
    // case the scorer rejects the row outright and `pdf_parser.rs`'s own
    // `infer_header_from_data` fallback guesses badly-wrong column x's for
    // this specific layout (observed: Debit/Credit/Balance all inferred
    // within 20pts of each other). Requiring only Date+Deposits+
    // Withdrawals+Balance to be individually recognizable — never the
    // Narration cell's own text — is far more robust to that kind of
    // partial OCR noise.
    let mut hdr_idx = None;
    let mut col_x = PdfColX::default();
    let mut hdr_row: Vec<PdfItem> = Vec::new();
    'search: for (i, row) in rows.iter().enumerate().take(60) {
        let mut date_x = None;
        let mut deposits_x = None;
        let mut withdrawals_x = None;
        let mut balance_x = None;
        for it in row {
            let l = it.text.to_lowercase();
            if l == "date" {
                date_x = Some(it.x);
            } else if l.starts_with("deposit") {
                deposits_x = Some(it.x);
            } else if l.starts_with("withdrawal") {
                withdrawals_x = Some(it.x);
            } else if l.starts_with("balance") {
                balance_x = Some(it.x);
            }
        }
        if let (Some(dt), Some(dep), Some(wd), Some(bal)) =
            (date_x, deposits_x, withdrawals_x, balance_x)
        {
            // Narration/Particulars x: the rightmost header item that isn't
            // Date/Deposits/Withdrawals/Balance itself and sits left of the
            // amount columns — picks up "PARTICULARS" (or whatever OCR
            // mangled it into) while skipping past "MODE**", regardless of
            // what that item's text actually says.
            let money_left = dep.min(wd);
            let narr_x = row
                .iter()
                .filter(|it| {
                    let l = it.text.to_lowercase();
                    it.x > dt && it.x < money_left && l != "date"
                })
                .map(|it| it.x)
                .fold(None, |acc: Option<f64>, x| Some(acc.map_or(x, |a| a.max(x))));

            hdr_idx = Some(i);
            col_x = PdfColX {
                date: Some(dt),
                narration: narr_x,
                credit: Some(dep),  // Deposits = money in = Credit
                debit: Some(wd),    // Withdrawals = money out = Debit
                balance: Some(bal),
                ..Default::default()
            };
            hdr_row = row.clone();
            break 'search;
        }
    }
    let hdr_idx = hdr_idx?;
    let boundaries = calc_col_boundaries(&col_x, &hdr_row);

    fn cell(cells: &std::collections::HashMap<ColField, String>, f: ColField) -> String {
        cells.get(&f).cloned().unwrap_or_default()
    }

    // Account number — "Savings A/c 059501505351" / "...Account Number:
    // 059501505351..." both appear once, above the transaction table.
    let account_no = {
        static ACC_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
            regex::Regex::new(r"(?i)a/c(?:count)?\s*(?:number)?\s*[:\-]?\s*(\d{6,20})").unwrap()
        });
        rows.iter()
            .take(hdr_idx.max(1))
            .find_map(|r| {
                let joined = r.iter().map(|it| it.text.as_str()).collect::<Vec<_>>().join(" ");
                ACC_RE
                    .captures(&joined)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().to_string())
            })
            .unwrap_or_default()
    };

    struct Block {
        date_display: String,
        date_ts: i64,
        narration_parts: Vec<String>,
        deposit: Option<f64>,
        withdrawal: Option<f64>,
        balance: Option<f64>,
    }

    let mut txns: Vec<Transaction> = Vec::new();
    let mut op_balance: Option<f64> = None;
    let mut txn_counter = 0usize;
    let mut cur: Option<Block> = None;

    macro_rules! flush {
        () => {
            if let Some(b) = cur.take() {
                let narration_joined = b.narration_parts.join(" ");
                let narration_joined: String =
                    narration_joined.split_whitespace().collect::<Vec<_>>().join(" ");
                if b.deposit.is_some() || b.withdrawal.is_some() {
                    txn_counter += 1;
                    let (narration, reference) = extract_icici_wealth_ref(&narration_joined);
                    let mut t = Transaction::new(format!("t_iciciwm_{}", txn_counter));
                    t.date = b.date_display;
                    t.date_ts = b.date_ts;
                    t.narration = if narration.is_empty() { narration_joined } else { narration };
                    t.reference = reference;
                    t.debit = b.withdrawal;
                    t.credit = b.deposit;
                    t.balance = b.balance;
                    t.bank_name = "ICICI Bank Wealth Management".to_string();
                    t.account_no = account_no.clone();
                    txns.push(t);
                } else if op_balance.is_none() {
                    // "B/F" (brought forward) row with only a Balance value.
                    op_balance = b.balance;
                }
            }
        };
    }

    // First pass: resolve every post-header row to (valid date?, narration,
    // deposit, withdrawal, balance) — or `None` for a row that's noise
    // (repeated header/footer/summary) or the FD/TDS section boundary,
    // which also truncates the list outright.
    struct RowInfo {
        date: Option<crate::parser::date_parser::ParsedDate>,
        narr: String,
        deposit: Option<f64>,
        withdrawal: Option<f64>,
        balance: Option<f64>,
    }
    let mut infos: Vec<RowInfo> = Vec::new();
    for row in rows.iter().skip(hdr_idx + 1) {
        let row_joined: String = row.iter().map(|it| it.text.as_str()).collect::<Vec<_>>().join(" ");
        let rl = row_joined.to_lowercase();

        // Stop before the Fixed Deposit / TDS summary sections that follow
        // the transaction table on some ICICI WM statements (same stop
        // condition `pdf_parser.rs`'s main loop already uses for this
        // format).
        if rl.contains("statement of fixed deposit")
            || rl.contains("fixed deposit a/c")
            || rl.contains("summary of tds")
            || (rl.contains("additions") && rl.contains("deductions"))
        {
            break;
        }

        // Repeated per-page header row / page footer / account-summary
        // block — never part of any transaction's narration.
        if rl.contains("particulars")
            || (rl.contains("deposits") && rl.contains("withdrawals"))
            || (rl.starts_with("page ") && rl.contains(" of "))
            || rl.contains("account details")
            || rl.contains("statement of transactions")
            || rl.contains("nomination")
        {
            continue;
        }

        let cells = assign_cells(row, &boundaries);
        let raw_date = cell(&cells, ColField::Date);
        let raw_narr = cell(&cells, ColField::Narration);
        let deposit = parse_amount_str(&cell(&cells, ColField::Credit));
        let withdrawal = parse_amount_str(&cell(&cells, ColField::Debit));
        let balance = parse_amount_str(&cell(&cells, ColField::Balance));
        let nd = normalize_transaction_date(&raw_date);

        infos.push(RowInfo {
            date: if nd.valid { Some(nd) } else { None },
            narr: raw_narr.trim().to_string(),
            deposit,
            withdrawal,
            balance,
        });
    }

    // Second pass: block-group with one-row lookahead. A pure-narration
    // continuation row (no date, no amount of its own) that is immediately
    // followed by a new date row is the START of that upcoming
    // transaction's Particulars, not the tail of the one currently open —
    // this table's cell is vertically centered, so a transaction's first
    // narration line routinely lands one OCR row *above* its own date row
    // (confirmed against the real fixture: "UPI/zee5.../YES" — the true
    // first line of the 05-04-2025 transaction — otherwise gets glued onto
    // the unrelated B/F row immediately before it). Buffer such a row and
    // hand it to the block that starts next instead.
    let mut lookahead_narr: Vec<String> = Vec::new();
    let n = infos.len();
    for i in 0..n {
        let is_pure_continuation =
            infos[i].date.is_none() && infos[i].deposit.is_none() && infos[i].withdrawal.is_none();
        let next_starts_block = infos.get(i + 1).is_some_and(|nx| nx.date.is_some());

        if is_pure_continuation && next_starts_block && !infos[i].narr.is_empty() {
            lookahead_narr.push(infos[i].narr.clone());
            continue;
        }

        if let Some(nd) = infos[i].date.take() {
            flush!();
            let mut narration_parts = std::mem::take(&mut lookahead_narr);
            if !infos[i].narr.is_empty() {
                narration_parts.push(infos[i].narr.clone());
            }
            cur = Some(Block {
                date_display: nd.display,
                date_ts: nd.ts,
                narration_parts,
                deposit: infos[i].deposit,
                withdrawal: infos[i].withdrawal,
                balance: infos[i].balance,
            });
        } else if let Some(b) = cur.as_mut() {
            for part in std::mem::take(&mut lookahead_narr) {
                b.narration_parts.push(part);
            }
            if !infos[i].narr.is_empty() {
                b.narration_parts.push(infos[i].narr.clone());
            }
            if b.deposit.is_none() {
                b.deposit = infos[i].deposit;
            }
            if b.withdrawal.is_none() {
                b.withdrawal = infos[i].withdrawal;
            }
            if b.balance.is_none() {
                b.balance = infos[i].balance;
            }
        }
        // A non-date row before the first block (or after one already
        // flushed with nothing pending) carries no transaction to attach
        // to — dropped, matching every other extractor's "pre-date buffer
        // is only kept once a real transaction can claim it" behavior.
    }
    flush!();

    if txns.len() < 2 {
        log::debug!(
            "[BSP ICICI WM] only {} transactions extracted from \"{}\" — treating as a non-match",
            txns.len(),
            file_name
        );
        return None;
    }

    let op_balance = compute_prev_balances(&mut txns, op_balance);

    // Debit/Credit must never mix (this table has separate Deposits and
    // Withdrawals columns — a real row only ever posts to one). A block
    // occasionally picks up a value on *both* sides: almost always a
    // column mis-assignment or OCR digit-bleed artifact (observed: a
    // spurious `credit: 1.00` alongside a real debit; or a wildly
    // implausible credit like `100846667.00` where the true value was a
    // few hundred/thousand rupees). Use the balance chain — ground truth,
    // since it comes from the statement's own printed running balance, not
    // from noisy OCR digits — to keep whichever side actually explains the
    // observed balance movement and drop the other.
    for t in txns.iter_mut() {
        if let (Some(dr), Some(cr)) = (t.debit, t.credit) {
            let keep_credit = match (t.prev_balance, t.balance) {
                (Some(pb), Some(bal)) => {
                    let diff = ((bal - pb) * 100.0).round() / 100.0;
                    let tol = |amt: f64| f64::max(1.0, amt * 0.02);
                    let cr_fits = (diff - cr).abs() < tol(cr);
                    let dr_fits = (diff + dr).abs() < tol(dr);
                    if cr_fits && !dr_fits {
                        true
                    } else if dr_fits && !cr_fits {
                        false
                    } else {
                        // Neither (or both) fit cleanly — the smaller side
                        // is, in every case observed against this fixture,
                        // the spurious one; keep the larger.
                        cr >= dr
                    }
                }
                // No balance context to judge by — keep the larger amount.
                _ => cr >= dr,
            };
            if keep_credit {
                t.debit = None;
            } else {
                t.credit = None;
            }
            log::debug!(
                "[BSP ICICI WM] both debit={dr:.2} and credit={cr:.2} set for {} \"{}\" — kept {}",
                t.date,
                safe_prefix(&t.narration, 40),
                if keep_credit { "credit" } else { "debit" }
            );
        }
    }

    prepend_opening_balance_row(&mut txns, op_balance, "ICICI Bank Wealth Management", &account_no);

    Some(ParseResult {
        transactions: txns,
        opening_balance: op_balance,
        closing_balance: None,
        bank_name: "ICICI Bank Wealth Management".to_string(),
        account_no,
        source_name: file_name.to_string(),
        col_map: Default::default(),
        header_row_idx: hdr_idx,
        noise_row_count: 0,
        rejected_row_count: 0,
    })
}

/// Extract narration/reference from an ICICI Wealth Management block's
/// joined Particulars text. UPI/IMPS narrations are slash-delimited with
/// the UTR/RRN as its own 9–12 digit segment (same shape as Cosmos's —
/// see `extract_cosmos_ref`), so that rule is reused first; NEFT/RTGS
/// narrations instead glue the UTR into a hyphen-delimited segment
/// (`NEFT-KKBKN62025040723714936-KARAN...`), so the generic
/// `extract_ref_from_narration` (9+ digit run bounded by `/`, `-`, space,
/// or string edge) is tried as a fallback. Neither always finds a clean
/// reference for every narration shape this statement uses — when neither
/// matches, narration is left as-is and reference stays empty rather than
/// guessing.
fn extract_icici_wealth_ref(narration: &str) -> (String, String) {
    if narration.contains('/') {
        let segments: Vec<&str> = narration.split('/').collect();
        if segments.len() > 1 {
            if let Some(idx) = segments
                .iter()
                .position(|s| (6..=16).contains(&s.len()) && s.chars().all(|c| c.is_ascii_digit()))
            {
                let reference = segments[idx].to_string();
                let rest: Vec<&str> = segments
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != idx)
                    .map(|(_, s)| *s)
                    .collect();
                let narr = rest.join("/").trim().to_string();
                let narr = if narr.is_empty() { narration.to_string() } else { narr };
                return (narr, reference);
            }
        }
    }
    if let Some(reference) = extract_ref_from_narration(narration) {
        return (narration.to_string(), reference);
    }
    (narration.to_string(), String::new())
}

// ── extract_icici_normal_transactions ─────────────────────────────────────────

/// Parser for the *normal* ICICI Bank "Detailed Statement" PDF layout — a
/// completely different real-bug class from the Wealth Management statement
/// (`extract_icici_wealth_transactions`): this file has perfectly good
/// embedded text, but `text_extractor::extract_page_text` only inserts a
/// line break on the `'`/`"`/`T*`/`ET` content-stream operators (see that
/// module's doc comment) — this PDF's generator instead moves the text
/// cursor with bare `Td`/`Tm` operators between every visual line, which are
/// silently ignored, so the ENTIRE transaction table (all rows of all
/// columns) is emitted as one continuous run of un-separated text per
/// physical text object, occasionally gluing two adjacent fields together
/// with zero delimiter (confirmed against this fixture:
/// `"...409524494660//BALAJI C/KARB/balajiod9@kb"` glued directly onto the
/// next Tj's `"...58598.29378368.15"`, two 2-decimal amounts with no
/// separator). This is NOT a "which regex splits the glued numbers" problem
/// — it's unrecoverable at the flat-text layer, full stop, because the
/// column identity is gone. So this extractor is designed to run **only**
/// against OCR word-boxes (`ocr_extractor::extract_pages_via_ocr`'s Tier 0
/// fallback), the same class of fix `extract_icici_wealth_transactions`
/// uses for a different root cause — rendering the page to an image and
/// reading it back with Tesseract recovers genuine per-word X positions
/// regardless of how confused the PDF's own text layer is. Gated so it can
/// never fire against Stage 1's flat embedded-text rows (see below).
///
/// Layout (header repeats on every page, itself wrapped across 2-3 OCR
/// lines): `Sl No | Tran Id | Value Date | Transaction Date | Transaction
/// Posted Date | Cheque no / Ref No | Transaction Remarks | Withdrawal (Dr)
/// | Deposit (Cr) | Balance`. Each transaction spans 1-4 physical OCR rows
/// (Sl No starts a new block; the Value Date/Tran Id/amount digits
/// frequently split themselves across two of those rows too — see the
/// merge logic below).
///
/// **Redacted account-holder header**: this fixture's customer-detail box
/// (name/address/account number/IFSC/branch) is covered by a solid black
/// rectangle drawn *over* the text on the actual rendered page — the text
/// is still technically present in the PDF's content stream underneath
/// (confirmed directly), but that's a deliberate redaction by whoever
/// prepared this fixture, not "hidden" metadata this app is meant to
/// recover. This extractor deliberately does **not** dig it out from behind
/// the redaction — `account_no` is left empty (masks to bare `XXXX` in the
/// UI, the documented fallback for "unavailable") rather than resurrecting
/// PII its owner blacked out.
// Shared OCR-table block-classification helpers — originally built for
// `extract_icici_normal_transactions`, reused as-is by
// `extract_idbi_transactions` below (same OCR-word-box shape, same class of
// glued-junk/split-amount artifacts). `col0`/`col1` mean debit/withdrawal
// and credit/deposit respectively, whichever caller is using them.
//
// A narrow OCR quirk rules out the generic `calc_col_boundaries`/
// `assign_cells` midpoint-bucketing machinery every other extractor in this
// module uses: these tables' amount columns sit close enough together that
// a multi-word narration continuation line routinely has its later word
// start well past where a fixed midpoint boundary would put the
// Narration/amount-columns fence — a plain x-bucket read would silently
// swallow real narration words into the amount columns. Classifying by
// *shape* first (does this item's text look like part of a number at all?)
// and falling back to x only for genuine ambiguity between the money
// columns sidesteps that entirely.
//
// Known OCR junk glyphs that land glued onto an otherwise-clean amount (a
// mis-read cell border, observed on every amount that happens to sit at the
// right edge of its column: "3,000.00}", "1,50,000.|").
fn strip_ocr_junk(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '}' | '|' | '{' | '[' | ']' | '$')).collect()
}
// True for an item that — once known junk glyphs are stripped — is *purely*
// digits/commas/dots and therefore could only be all or part of an amount
// (a Tran Id/Cheque No like "S8592" or a narration fragment with a "/"
// always fails this).
fn is_amount_shaped(text: &str) -> bool {
    let cleaned = strip_ocr_junk(text);
    !cleaned.is_empty() && cleaned.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '.')
}
// A fragment "dangles" when it ends in a bare decimal point with no digits
// after it — the unambiguous signature of an amount OCR split across two
// physical rows (e.g. "1,58,266." continuing as "65" on the next row).
fn dangling(frags: &[String]) -> bool {
    frags.last().is_some_and(|f| strip_ocr_junk(f).ends_with('.'))
}

// Classify one row's items (already filtered to `x >= wall_x`) into the
// block's Narration text and col0/col1/Balance fragment lists.
//
// Amount columns are resolved by nearest anchor X *unless* one or more
// columns are already "dangling" (see above) — a split fragment's
// continuation routinely lands closer to the *next* column's anchor than
// its own (observed: a Deposit split's tail digits sit nearer to Balance's
// anchor by raw distance), so absolute X is unusable there. What *does*
// hold across every observed case: the dangling columns' relative
// left-to-right order always matches the continuation fragments' own
// left-to-right order on the row that completes them — pairing by sorted
// order rather than by nearest distance resolves both splits correctly.
fn classify_row(
    row: &[PdfItem],
    wall_x: f64,
    anchors: [f64; 3], // [col0 (debit), col1 (credit), balance]
    narration_parts: &mut Vec<String>,
    col0_frags: &mut Vec<String>,
    col1_frags: &mut Vec<String>,
    balance_frags: &mut Vec<String>,
) {
    let mut narration_words: Vec<&str> = Vec::new();
    let mut amount_items: Vec<&PdfItem> = Vec::new();
    for it in row.iter() {
        let t = it.text.trim();
        if it.x < wall_x || t.is_empty() {
            continue;
        }
        if is_amount_shaped(&it.text) {
            amount_items.push(it);
        } else if t.chars().any(|c| c.is_alphanumeric()) {
            // A token with zero alphanumeric characters at all (a bare
            // ":", ";", "." etc.) can never be part of a real narration —
            // it's always a border-ruling-line misread landing as its own
            // stray OCR word (observed gluing an errant ":" between two
            // real words a row apart, e.g. "...SMC" / ":" / "GLOBAL...").
            narration_words.push(it.text.as_str());
        }
    }
    if !narration_words.is_empty() {
        narration_parts.push(narration_words.join(" "));
    }
    if amount_items.is_empty() {
        return;
    }
    amount_items.sort_by(|a, b| a.x.partial_cmp(&b.x).unwrap());

    let is_dangling = [dangling(col0_frags), dangling(col1_frags), dangling(balance_frags)];
    let mut dangling_idxs: Vec<usize> = (0..3).filter(|&i| is_dangling[i]).collect();
    dangling_idxs.sort_by(|&a, &b| anchors[a].partial_cmp(&anchors[b]).unwrap());

    let nearest = |x: f64| -> usize {
        (0..3)
            .min_by(|&a, &b| (x - anchors[a]).abs().partial_cmp(&(x - anchors[b]).abs()).unwrap())
            .unwrap()
    };
    let mut push_to = |idx: usize, text: &str| match idx {
        0 => col0_frags.push(text.to_string()),
        1 => col1_frags.push(text.to_string()),
        _ => balance_frags.push(text.to_string()),
    };

    let paired = dangling_idxs.len().min(amount_items.len());
    for i in 0..paired {
        push_to(dangling_idxs[i], &amount_items[i].text);
    }
    for it in amount_items.iter().skip(paired) {
        push_to(nearest(it.x), &it.text);
    }
}

pub fn extract_icici_normal_transactions(rows: &[Vec<PdfItem>], file_name: &str) -> Option<ParseResult> {
    // Never fires for the Wealth Management layout (different extractor,
    // different header entirely) or for Stage 1's flat embedded-text rows —
    // every item there sits at X=0 (see `text_extractor::extract_pages`'s
    // doc comment), which would make every header keyword collapse onto the
    // same column and produce garbage. Requiring items spread across at
    // least a few distinct X positions in the scanned header window is a
    // simple, reliable "this came from real OCR word-boxes" signal.
    let header_window: Vec<&Vec<PdfItem>> = rows.iter().take(10).collect();
    let early_text: String = header_window
        .iter()
        .map(|r| r.iter().map(|it| it.text.as_str()).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n");
    let el = early_text.to_lowercase();
    if el.contains("wealth management") {
        return None;
    }
    let distinct_x = header_window
        .iter()
        .flat_map(|r| r.iter().map(|it| it.x.round() as i64))
        .collect::<std::collections::HashSet<_>>()
        .len();
    if distinct_x < 5 {
        return None;
    }

    // Header keywords, individually — this table's header text itself wraps
    // across 2-3 OCR rows ("Withdra" / "wal" / "(Dr)" on three different
    // rows), so no single row can be scored as a whole the way
    // `find_pdf_header` expects. `narration_x` anchors on the "Remarks" of
    // "Transaction Remarks" directly (an exact `COL_NARRATION` keyword,
    // unlike Wealth Management's OCR-mangled header text); `wall_x` anchors
    // on the "Cheque no / Ref No" header so its column — and everything left
    // of it (Sl No, Tran Id, all three date columns) — never bleeds into
    // Narration.
    let mut withdrawal_x = None;
    let mut deposit_x = None;
    let mut balance_x = None;
    let mut narration_x = None;
    let mut wall_x: f64 = 0.0;
    for row in &header_window {
        for it in row.iter() {
            let l: String = it.text.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
            if l.starts_with("withdra") && withdrawal_x.is_none() {
                withdrawal_x = Some(it.x);
            } else if l.starts_with("deposit") && deposit_x.is_none() {
                deposit_x = Some(it.x);
            } else if l.starts_with("balance") && balance_x.is_none() {
                balance_x = Some(it.x);
            } else if l.contains("remark") && narration_x.is_none() {
                narration_x = Some(it.x);
            } else if l == "ref" || l == "cheque" || l == "no" {
                wall_x = wall_x.max(it.x);
            }
        }
    }
    let (withdrawal_x, deposit_x, balance_x, narration_x) =
        match (withdrawal_x, deposit_x, balance_x, narration_x) {
            (Some(w), Some(d), Some(b), Some(n)) => (w, d, b, n),
            _ => return None,
        };
    let wall_x = if wall_x > 0.0 { wall_x + 10.0 } else { narration_x - 60.0 };

    // Account number: deliberately NOT extracted — see doc comment above.
    let account_no = String::new();

    // Shape-based amount/narration classification (`strip_ocr_junk`,
    // `is_amount_shaped`, `dangling`, `classify_row`) now lives at module
    // level above — shared with `extract_idbi_transactions`.

    struct Block {
        date_display: String,
        date_ts: i64,
        narration_parts: Vec<String>,
        debit_frags: Vec<String>,
        credit_frags: Vec<String>,
        balance_frags: Vec<String>,
    }

    // A transaction-starting row's very first (leftmost) item is the bare Sl
    // No — "1".."8", occasionally with a trailing OCR-noise "." ("3."). This
    // is a far more reliable "new transaction" signal than scanning for a
    // date shape: several *narrations* in this statement contain dot-
    // separated date-like substrings of their own (e.g. "09.05.2025" inside
    // a UTR fragment) that must never be mistaken for a row boundary.
    static SL_NO_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"^\d{1,3}\.?$").unwrap());
    // A *slash*-separated date (this table's Value/Transaction Date columns
    // use "03/Apr/2025"; Transaction Posted Date uses "03/04/2025") — never
    // matches the dot-separated dates that show up inside narration text.
    static DATE_ITEM_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"^\D*(\d{1,2}/(?:[A-Za-z]{3}|\d{1,2})/\d{4})").unwrap()
    });

    let mut txns: Vec<Transaction> = Vec::new();
    let mut txn_counter = 0usize;
    let mut cur: Option<Block> = None;

    macro_rules! flush {
        () => {
            if let Some(b) = cur.take() {
                let narration_joined: String = b
                    .narration_parts
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                // Cross-row split-amount fragments (e.g. "1,58,266." on one
                // OCR row, "65" on the next — see doc comment) are just
                // concatenated in row order, stripping everything but
                // digits/comma/dot first — OCR consistently glues a stray
                // border-line glyph ("}", "|") onto the column's last real
                // character (observed on every Deposit-column amount in
                // this fixture: "3,000.00}", "9,000.00}") which would
                // otherwise make the whole fragment fail to parse as a
                // float and silently drop a real transaction. Once cleaned,
                // "1,58,266." + "65" -> "1,58,266.65" parses correctly,
                // while two genuinely unrelated numbers concatenated this
                // way (the historic "glued amounts" bug) produces an
                // invalid float (two decimal points) that safely fails to
                // parse instead of silently returning a wrong number.
                fn clean_amount_frags(frags: &[String]) -> String {
                    frags
                        .concat()
                        .chars()
                        .filter(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
                        .collect()
                }
                let debit = parse_amount_str(&clean_amount_frags(&b.debit_frags));
                let credit = parse_amount_str(&clean_amount_frags(&b.credit_frags));
                let balance = parse_amount_str(&clean_amount_frags(&b.balance_frags));
                if debit.is_some() || credit.is_some() {
                    txn_counter += 1;
                    let reference = extract_ref_from_narration(&narration_joined).unwrap_or_default();
                    let narration = narration_joined;
                    let mut t = Transaction::new(format!("t_icici_{}", txn_counter));
                    t.date = b.date_display;
                    t.date_ts = b.date_ts;
                    t.narration = narration;
                    t.reference = reference;
                    t.debit = debit;
                    t.credit = credit;
                    t.balance = balance;
                    t.bank_name = "ICICI Bank".to_string();
                    t.account_no = account_no.clone();
                    txns.push(t);
                }
            }
        };
    }

    for row in rows.iter().skip(0) {
        if row.is_empty() {
            continue;
        }
        let row_joined: String = row.iter().map(|it| it.text.as_str()).collect::<Vec<_>>().join(" ");
        let rl = row_joined.to_lowercase();

        // End-of-table noise: page total / summary / legend glossary. Every
        // real transaction row precedes this in the source statement.
        if rl.contains("page total")
            || rl.contains("legends used")
            || rl.contains("opening bal")
            || (rl.contains("withdrawls") || rl.contains("withdrawals")) && rl.contains(':')
        {
            break;
        }
        // Repeated per-page header row.
        if rl.contains("transaction remarks") || (rl.contains("deposit") && rl.contains("balance")) {
            continue;
        }

        let first_x = row.first().map(|it| it.x).unwrap_or(f64::MAX);
        let starts_block = first_x < wall_x
            && row
                .first()
                .map(|it| SL_NO_RE.is_match(it.text.trim()))
                .unwrap_or(false);

        if starts_block {
            flush!();
            // Find this block's actual transaction date: the first
            // slash-dated item found anywhere in the row, left to right —
            // naturally skips the Tran Id/Value Date columns whenever OCR
            // has truncated them (e.g. "03/Apr/20" has only a 2-digit year
            // and fails `normalize_transaction_date`), landing on the first
            // *complete* date, which is this table's real "Transaction
            // Date" column.
            let mut date_display = String::new();
            let mut date_ts = 0i64;
            for it in row.iter() {
                let cleaned = it.text.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
                if let Some(caps) = DATE_ITEM_RE.captures(cleaned) {
                    let nd = normalize_transaction_date(&caps[1]);
                    if nd.valid {
                        date_display = nd.display;
                        date_ts = nd.ts;
                        break;
                    }
                }
            }
            if date_display.is_empty() {
                // No parseable date at all — not a real transaction row
                // (probably a stray Sl-No-shaped OCR artifact). Drop it;
                // `cur` is already `None` from the `flush!()` above.
                continue;
            }
            let mut b = Block {
                date_display,
                date_ts,
                narration_parts: Vec::new(),
                debit_frags: Vec::new(),
                credit_frags: Vec::new(),
                balance_frags: Vec::new(),
            };
            classify_row(
                row,
                wall_x,
                [withdrawal_x, deposit_x, balance_x],
                &mut b.narration_parts,
                &mut b.debit_frags,
                &mut b.credit_frags,
                &mut b.balance_frags,
            );
            cur = Some(b);
        } else if let Some(b) = cur.as_mut() {
            classify_row(
                row,
                wall_x,
                [withdrawal_x, deposit_x, balance_x],
                &mut b.narration_parts,
                &mut b.debit_frags,
                &mut b.credit_frags,
                &mut b.balance_frags,
            );
        }
    }
    flush!();

    // Anti-false-positive guard, same threshold every other dedicated
    // extractor in this module uses.
    if txns.len() < 2 {
        log::debug!(
            "[BSP ICICI Normal] only {} transactions extracted from \"{}\" — treating as a non-match",
            txns.len(),
            file_name
        );
        return None;
    }

    let op_balance = compute_prev_balances(&mut txns, None);

    // Debit/Credit must never mix — this table has separate Withdrawal/
    // Deposit columns, so a real row only ever posts to one. See
    // `extract_icici_wealth_transactions`'s identical guard for why the
    // balance chain (ground truth from the statement's own printed running
    // balance) is what decides which side is real.
    for t in txns.iter_mut() {
        if let (Some(dr), Some(cr)) = (t.debit, t.credit) {
            let keep_credit = match (t.prev_balance, t.balance) {
                (Some(pb), Some(bal)) => {
                    let diff = ((bal - pb) * 100.0).round() / 100.0;
                    let tol = |amt: f64| f64::max(1.0, amt * 0.02);
                    let cr_fits = (diff - cr).abs() < tol(cr);
                    let dr_fits = (diff + dr).abs() < tol(dr);
                    if cr_fits && !dr_fits {
                        true
                    } else if dr_fits && !cr_fits {
                        false
                    } else {
                        cr >= dr
                    }
                }
                _ => cr >= dr,
            };
            if keep_credit {
                t.debit = None;
            } else {
                t.credit = None;
            }
        }
    }

    prepend_opening_balance_row(&mut txns, op_balance, "ICICI Bank", &account_no);

    Some(ParseResult {
        transactions: txns,
        opening_balance: op_balance,
        closing_balance: None,
        bank_name: "ICICI Bank".to_string(),
        account_no,
        source_name: file_name.to_string(),
        col_map: Default::default(),
        header_row_idx: 0,
        noise_row_count: 0,
        rejected_row_count: 0,
    })
}

// ── extract_idbi_transactions ─────────────────────────────────────────────────

/// Parser for IDBI Bank's "Statement of Account" PDF — the exact same
/// architecture-level bug as `extract_icici_normal_transactions` (see that
/// function's doc comment): this PDF's generator moves the text cursor
/// between visual lines with bare `Td`/`Tm` operators, which
/// `text_extractor::extract_page_text` silently ignores, so page 1's entire
/// ~20-row transaction table collapses into one giant flat-text blob with
/// no row/field delimiters — unrecoverable at the flat-text layer. Page 2 of
/// this particular statement happens to also render its most recent 4
/// transactions in a normal one-field-per-line layout (a distinct "recent
/// transactions" section), so those 4 were already recoverable via the
/// existing flat-text fallback — which is the entire reason only 4
/// transactions were ever visible before this extractor existed, NOT a
/// pagination bug, NOT a header-detection-starts-on-page-2 bug, NOT a
/// validation-rejection bug, and NOT an opening-balance-misclassification
/// bug. Like ICICI Normal, this extractor runs only against OCR word-boxes
/// (Tier 0 — `ocr_extractor::extract_pages_via_ocr`), which recovers real
/// per-word X/Y positions for *both* pages uniformly, regardless of how
/// broken either page's own text layer is — so it recovers page 1's ~20
/// rows and page 2's remaining 4 through the same code path.
///
/// Layout (2-row split header, repeats per page): `S.No | Description |
/// Cheque No | Withdrawals (Dr) | Deposits (Cr) | Balance (INR)`, with a
/// second header row spelling out `Txn Date | Value Date` above the
/// Description column. Each transaction spans 1+ physical OCR rows (a new
/// block starts wherever a valid `dd/mm/yyyy` date is found; the Description
/// column continues across further rows for a long narration).
///
/// **Redacted account-holder header**: same as ICICI Normal — the
/// customer-detail box is covered by a solid black rectangle on the
/// rendered page, a deliberate redaction rather than hidden metadata to dig
/// out. `account_no` is left empty (masks to bare `XXXX` in the UI).
pub fn extract_idbi_transactions(rows: &[Vec<PdfItem>], file_name: &str) -> Option<ParseResult> {
    // Same "this came from real OCR word-boxes, not Stage 1's flat X=0 rows"
    // signal `extract_icici_normal_transactions` uses.
    let header_window: Vec<&Vec<PdfItem>> = rows.iter().take(10).collect();
    let early_text: String = header_window
        .iter()
        .map(|r| r.iter().map(|it| it.text.as_str()).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n");
    let el = early_text.to_lowercase();
    if !(el.contains("txn date")
        && el.contains("withdra")
        && el.contains("deposit")
        && el.contains("balance"))
    {
        return None;
    }
    let distinct_x = header_window
        .iter()
        .flat_map(|r| r.iter().map(|it| it.x.round() as i64))
        .collect::<std::collections::HashSet<_>>()
        .len();
    if distinct_x < 5 {
        return None;
    }

    // Header keywords, individually — this table's header wraps across 2
    // physical OCR rows ("S.No | Description | Cheque No | Withdrawals |
    // Deposits | Balance" on one, "Txn Date | Value Date ... (Dr) | (Cr) |
    // (INR)" on the next). `wall_x` anchors on the rightmost "Date" word
    // (both "Txn Date" and "Value Date" say "Date" — taking the max picks
    // whichever renders further right, Value Date, without needing to
    // disambiguate by which date column it belongs to) so the two date
    // columns and S.No never bleed into Description/amount classification.
    let mut withdrawal_x = None;
    let mut deposit_x = None;
    let mut balance_x = None;
    let mut narration_x = None;
    let mut cheque_no_x = None;
    let mut wall_x: f64 = 0.0;
    for row in &header_window {
        for it in row.iter() {
            let l: String =
                it.text.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
            if l.starts_with("withdra") && withdrawal_x.is_none() {
                withdrawal_x = Some(it.x);
            } else if l.starts_with("deposit") && deposit_x.is_none() {
                deposit_x = Some(it.x);
            } else if l.starts_with("balance") && balance_x.is_none() {
                balance_x = Some(it.x);
            } else if l == "description" && narration_x.is_none() {
                narration_x = Some(it.x);
            } else if l == "cheque" && cheque_no_x.is_none() {
                cheque_no_x = Some(it.x);
            } else if l == "date" {
                wall_x = wall_x.max(it.x);
            }
        }
    }
    let (withdrawal_x, deposit_x, balance_x, narration_x) =
        match (withdrawal_x, deposit_x, balance_x, narration_x) {
            (Some(w), Some(d), Some(b), Some(n)) => (w, d, b, n),
            _ => return None,
        };
    let wall_x = if wall_x > 0.0 { wall_x + 20.0 } else { return None };
    // Cheque No candidates must sit strictly in the Cheque No column, not
    // merely "somewhere before the amount columns" — that broader range
    // also covers the whole Description column, and a bare digit-run that's
    // really a narration continuation (e.g. a scheme code wrapped onto its
    // own line, "202610006") sits at Description's own x, not Cheque No's.
    // The midpoint between the two header words' x is a safe dividing line:
    // real cheque numbers render indented under "Cheque" (verified against
    // this fixture's one real cheque number, "409615" at x≈344 vs a
    // Description-column wrap at x≈201 — comfortably on either side of the
    // ~282 midpoint between "Description" at x≈235 and "Cheque" at x≈329).
    let cheque_no_min_x = match cheque_no_x {
        Some(cnx) => (narration_x + cnx) / 2.0,
        None => wall_x,
    };
    let cheque_no_max_x = withdrawal_x.min(deposit_x) - 10.0;

    // Account number: deliberately NOT extracted — see doc comment above.
    let account_no = String::new();

    // A transaction date in this statement is a plain `dd/mm/yyyy` (no
    // month abbreviations, unlike ICICI Normal's "03/Apr/2025" columns) —
    // never matches narration text, which this fixture never glues a
    // slash-date into.
    static DATE_ITEM_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"^\D*(\d{1,2}/\d{1,2}/\d{4})").unwrap());

    struct Block {
        date_display: String,
        date_ts: i64,
        narration_parts: Vec<String>,
        debit_frags: Vec<String>,
        credit_frags: Vec<String>,
        balance_frags: Vec<String>,
        chq_no: Option<String>,
    }

    let mut txns: Vec<Transaction> = Vec::new();
    let mut txn_counter = 0usize;
    let mut cur: Option<Block> = None;

    macro_rules! flush {
        () => {
            if let Some(b) = cur.take() {
                let narration_joined: String = b
                    .narration_parts
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                // Every real Description in this statement starts with a
                // letter (a bank narration code like "NEFT-"/"IPAY/"/"Int."
                // or a plain name); a leading run of anything else is the
                // Description column's own left border ruling line,
                // consistently misread by Tesseract as junk glued onto the
                // first word — variously "|", "_|", "=|", "‘|", "—_|"
                // depending on anti-aliasing noise (confirmed against every
                // row of this fixture's raw OCR output: never a real
                // character of the actual narration).
                // The right edge of the Description column has the same
                // border-ruling problem, glued onto the LAST word instead
                // (a lone trailing ";"/":"/"." — confirmed against this
                // fixture's raw OCR: never a real trailing character of an
                // actual narration, which always ends mid-word or on a
                // digit).
                let narration_joined = narration_joined
                    .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
                    .trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
                    .to_string();
                fn clean_amount_frags(frags: &[String]) -> String {
                    frags
                        .concat()
                        .chars()
                        .filter(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
                        .collect()
                }
                let debit = parse_amount_str(&clean_amount_frags(&b.debit_frags));
                let credit = parse_amount_str(&clean_amount_frags(&b.credit_frags));
                let balance = parse_amount_str(&clean_amount_frags(&b.balance_frags));
                if debit.is_some() || credit.is_some() {
                    txn_counter += 1;
                    let reference = b.chq_no.clone().unwrap_or_else(|| {
                        extract_ref_from_narration(&narration_joined).unwrap_or_default()
                    });
                    let mut t = Transaction::new(format!("t_idbi_{}", txn_counter));
                    t.date = b.date_display;
                    t.date_ts = b.date_ts;
                    t.narration = narration_joined;
                    t.reference = reference;
                    t.debit = debit;
                    t.credit = credit;
                    t.balance = balance;
                    t.bank_name = "IDBI Bank".to_string();
                    t.account_no = account_no.clone();
                    txns.push(t);
                }
            }
        };
    }

    for row in rows.iter() {
        if row.is_empty() {
            continue;
        }
        let row_joined: String =
            row.iter().map(|it| it.text.as_str()).collect::<Vec<_>>().join(" ");
        let rl = row_joined.to_lowercase();

        // End-of-table noise: statement summary / legend. Every real
        // transaction row precedes this in the source statement.
        if rl.contains("statement summary")
            || rl.contains("dr count")
            || rl.contains("legends")
            || rl.contains("this is a computer generated")
        {
            break;
        }
        // Repeated per-page header row.
        if rl.contains("txn date") || (rl.contains("withdra") && rl.contains("deposit")) {
            continue;
        }
        // Page-footer boilerplate (address/toll-free/page-number) sits
        // between page 1's last real row and page 2's first one — *before*
        // the statement summary noise caught above, so it can't `break`
        // (page 2's real transactions still follow it) but must still be
        // skipped rather than falling through to classification, where its
        // page-footer text — mid-x-range enough to overlap the Description
        // column — would otherwise get appended onto whatever transaction
        // block is still open (confirmed against this fixture: without
        // this, "Regd. Office: IDBI Tower, WTC Complex..." et al. bleed
        // into the last transaction before the page break).
        if rl.contains("idbi bank ltd")
            || rl.contains("regd. office")
            || rl.contains("toll-free numbers")
            || rl.contains("chargeable number")
            || (rl.starts_with("page ") && rl.contains(" of "))
        {
            continue;
        }

        // Cheque No: a pure-digit item sitting strictly in the Cheque No
        // column (`cheque_no_min_x..cheque_no_max_x`, NOT the wider
        // `wall_x..cheque_no_max_x` span — that would also catch a bare
        // digit-run narration continuation sitting at Description's own x;
        // see `cheque_no_min_x`'s doc comment). Identified by x-position +
        // shape only — NOT excluded from being amount-shaped, since a real
        // cheque number (e.g. "409615") always *is* digit-only. Filtered
        // out of the row by exact x-match before classification so it
        // can't concatenate onto a real amount and corrupt it.
        let chq_no_item = row.iter().find(|it| {
            it.x >= cheque_no_min_x
                && it.x < cheque_no_max_x
                && it.text.trim().chars().all(|c| c.is_ascii_digit())
                && it.text.trim().len() >= 4
        });
        let chq_no_x = chq_no_item.map(|it| it.x);
        let chq_no_found = chq_no_item.map(|it| it.text.trim().to_string());

        // First (leftmost) valid slash-date anywhere in the row is this
        // row's Txn Date — naturally the leftmost of the two date columns,
        // whichever OCR manages to read completely.
        let mut row_date_display = String::new();
        let mut row_date_ts = 0i64;
        for it in row.iter() {
            let cleaned = it.text.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
            if let Some(caps) = DATE_ITEM_RE.captures(cleaned) {
                let nd = normalize_transaction_date(&caps[1]);
                if nd.valid {
                    row_date_display = nd.display;
                    row_date_ts = nd.ts;
                    break;
                }
            }
        }

        if !row_date_display.is_empty() {
            // New transaction block.
            flush!();
            let mut b = Block {
                date_display: row_date_display,
                date_ts: row_date_ts,
                narration_parts: Vec::new(),
                debit_frags: Vec::new(),
                credit_frags: Vec::new(),
                balance_frags: Vec::new(),
                chq_no: chq_no_found,
            };
            let filtered_row: Vec<PdfItem> = match chq_no_x {
                Some(cx) => row.iter().filter(|it| it.x != cx).cloned().collect(),
                None => row.clone(),
            };
            classify_row(
                &filtered_row,
                wall_x,
                [withdrawal_x, deposit_x, balance_x],
                &mut b.narration_parts,
                &mut b.debit_frags,
                &mut b.credit_frags,
                &mut b.balance_frags,
            );
            cur = Some(b);
        } else if let Some(b) = cur.as_mut() {
            if b.chq_no.is_none() {
                b.chq_no = chq_no_found;
            }
            // A narration-continuation row's bare reference/scheme-code
            // number (e.g. "202610006", the tail of "FY2025-202610006"
            // split across OCR rows) is indistinguishable from a genuine
            // split-amount continuation by shape alone. Only treat a bare
            // digit-run as a real amount continuation when one of the
            // amount columns is actually "dangling" (a genuine in-progress
            // cross-row split) — otherwise route it into the narration
            // instead of letting `classify_row`'s shape-based amount
            // detection corrupt the real amount with it.
            let any_dangling =
                dangling(&b.debit_frags) || dangling(&b.credit_frags) || dangling(&b.balance_frags);
            let filtered_row: Vec<PdfItem> = if any_dangling {
                match chq_no_x {
                    Some(cx) => row.iter().filter(|it| it.x != cx).cloned().collect(),
                    None => row.clone(),
                }
            } else {
                let mut kept = Vec::new();
                let mut rescued_narration = Vec::new();
                for it in row.iter() {
                    if Some(it.x) == chq_no_x {
                        continue;
                    }
                    let t = it.text.trim();
                    let bare_whole_number = t.len() >= 4 && t.chars().all(|c| c.is_ascii_digit());
                    if bare_whole_number {
                        rescued_narration.push(t.to_string());
                    } else {
                        kept.push(it.clone());
                    }
                }
                if !rescued_narration.is_empty() {
                    b.narration_parts.push(rescued_narration.join(" "));
                }
                kept
            };
            classify_row(
                &filtered_row,
                wall_x,
                [withdrawal_x, deposit_x, balance_x],
                &mut b.narration_parts,
                &mut b.debit_frags,
                &mut b.credit_frags,
                &mut b.balance_frags,
            );
        }
    }
    flush!();

    // Anti-false-positive guard, same threshold every other dedicated
    // extractor in this module uses.
    if txns.len() < 2 {
        log::debug!(
            "[BSP IDBI] only {} transactions extracted from \"{}\" — treating as a non-match",
            txns.len(),
            file_name
        );
        return None;
    }

    // This statement lists transactions newest-first (S.No 1 is the most
    // recent date), so `txns` was built in that same order above. But
    // `compute_prev_balances` (and everything that reads `opening_balance`/
    // "closing balance" off the returned `ParseResult` downstream, e.g. the
    // Summary Panel) assumes the array is CHRONOLOGICAL — the balance
    // *before* array element 0 is the account's true opening balance, and
    // the last element is the most recent transaction. Reversing here (once
    // the anti-false-positive guard above has already run on natural
    // reading order) makes that hold, exactly like `extract_icici_normal_
    // transactions`'s own source statement already does natively (oldest
    // first) without needing this step.
    txns.reverse();

    let op_balance = compute_prev_balances(&mut txns, None);

    // Debit/Credit must never mix — this table has separate Withdrawal/
    // Deposit columns, so a real row only ever posts to one. Balance-chain
    // ground truth decides which side is real, same as ICICI Normal.
    for t in txns.iter_mut() {
        if let (Some(dr), Some(cr)) = (t.debit, t.credit) {
            let keep_credit = match (t.prev_balance, t.balance) {
                (Some(pb), Some(bal)) => {
                    let diff = ((bal - pb) * 100.0).round() / 100.0;
                    let tol = |amt: f64| f64::max(1.0, amt * 0.02);
                    let cr_fits = (diff - cr).abs() < tol(cr);
                    let dr_fits = (diff + dr).abs() < tol(dr);
                    if cr_fits && !dr_fits {
                        true
                    } else if dr_fits && !cr_fits {
                        false
                    } else {
                        cr >= dr
                    }
                }
                _ => cr >= dr,
            };
            if keep_credit {
                t.debit = None;
            } else {
                t.credit = None;
            }
        }
    }

    // Balance-chain repair: unlike Debit/Credit (each independently
    // OCR'd from its own clean column and verified correct against this
    // fixture row-for-row), the printed Balance is fully redundant —
    // always exactly `prev_balance + credit - debit` in a real statement —
    // and empirically the more OCR-fragile field here (Tesseract twice
    // dropped a leading digit outright, e.g. "13368.15" → "3368.15", and
    // once misread a leading "1" as "{", stripped by `strip_ocr_junk` as
    // punctuation before this point). Whenever a transaction's own balance
    // doesn't reconcile with the chain, recompute it from Debit/Credit +
    // the previous (already-repaired, since this walks oldest→newest)
    // balance instead of trusting the single OCR'd number.
    let mut running_balance = op_balance;
    for t in txns.iter_mut() {
        if let Some(pb) = running_balance {
            let expected =
                ((pb + t.credit.unwrap_or(0.0) - t.debit.unwrap_or(0.0)) * 100.0).round() / 100.0;
            if let Some(bal) = t.balance {
                if (expected - bal).abs() > 0.01 {
                    log::debug!(
                        "[BSP IDBI] balance chain mismatch for {} \"{}\": OCR'd {:.2}, chain says {:.2} — using chain value",
                        t.date,
                        safe_prefix(&t.narration, 40),
                        bal,
                        expected
                    );
                    t.balance = Some(expected);
                    t.prev_balance = Some(pb);
                }
            }
        }
        running_balance = t.balance;
    }

    prepend_opening_balance_row(&mut txns, op_balance, "IDBI Bank", &account_no);

    Some(ParseResult {
        transactions: txns,
        opening_balance: op_balance,
        closing_balance: None,
        bank_name: "IDBI Bank".to_string(),
        account_no,
        source_name: file_name.to_string(),
        col_map: Default::default(),
        header_row_idx: 0,
        noise_row_count: 0,
        rejected_row_count: 0,
    })
}

// ── extract_idfc_first_transactions ───────────────────────────────────────────

/// Parser for IDFC FIRST Bank's "Statement of Account" PDF — the same
/// architecture-level bug as `extract_icici_normal_transactions` and
/// `extract_idbi_transactions`: this PDF's generator moves the text cursor
/// with bare `Td`/`Tm` operators between visual lines, which `text_extractor
/// ::extract_page_text` silently ignores, so every embedded-text item across
/// the whole 8-page statement lands at `x = 0.0` — real column identity is
/// gone at the flat-text layer, full stop (confirmed directly: dumping
/// `text_extractor::extract_pages`'s raw rows shows literally every single
/// item, across all pages, at `x=0.0`). Before this extractor existed, Stage
/// 1's generic column-based `parse_pdf_rows` correctly detected the header
/// but produced zero transactions from that flat layer, so the app fell
/// back to Stage 2's flat-*text* heuristic parser (`ocr_parser::
/// parse_ocr_text`) — which has no real column positions to work from at
/// all and has to *guess* Debit vs Credit from narration/ordering
/// heuristics. That guess was wrong for many rows (confirmed against the
/// real fixture: the very first transaction, a salary NEFT debit of
/// 10,022.00, came out as a Credit), and the page-repeated "Opening
/// Balance / Total Debit / Total Credit / Closing Balance" summary box
/// (printed at the top of every page) was being swept up as if it were
/// transaction data, producing a handful of rows with the statement's own
/// grand-total Credit (10,30,823.80) and Closing Balance (3,07,237.08)
/// glued onto unrelated real transactions. Like ICICI Normal and IDBI, this
/// extractor runs only against OCR word-boxes (Tier 0 —
/// `ocr_extractor::extract_pages_via_ocr`), which recovers real per-word X
/// positions for every page uniformly, so Debit/Credit is decided by which
/// column an amount actually sits under, never by guesswork.
///
/// Layout (2-row split header, repeats on every page): `Transaction Date |
/// Value Date | Particulars | Cheque No | Debit | Credit | Balance`. Unlike
/// IDBI, this statement runs chronologically (oldest first) — no reordering
/// needed for `opening_balance`/closing balance to land correctly. Also
/// unlike IDBI, this statement's account number is printed in the clear
/// (`ACCOUNT NO : 10158467482`, no redaction), so it's extracted directly
/// rather than left blank.
///
/// Every page repeats: a "STATEMENT OF ACCOUNT / CUSTOMER ID / ACCOUNT NO /
/// ALWAYS YOU FIRST / STATEMENT PERIOD" masthead block, the Opening/Total
/// Debit/Total Credit/Closing summary box (2 rows: labels, then values —
/// the values row carries no keyword of its own, so it's skipped by
/// pairing it with the label row immediately before it), the 2-row column
/// header, and — at the bottom — a "REGISTERED OFFICE" address line plus a
/// "Page X of Y" footer. All of this sits inside the same X range as the
/// Particulars column and must be recognized and skipped explicitly, same
/// as the page-footer fix in `extract_idbi_transactions`, or it corrupts
/// whichever transaction block is still open when it's encountered. The
/// final page ends the transaction table with an "IMPORTANT MESSAGE"
/// disclaimer section (confirmed to appear exactly once, only after the
/// last real row) — an explicit, reliable stop marker.
pub fn extract_idfc_first_transactions(
    rows: &[Vec<PdfItem>],
    file_name: &str,
) -> Option<ParseResult> {
    let header_window: Vec<&Vec<PdfItem>> = rows.iter().take(10).collect();
    let early_text: String = header_window
        .iter()
        .map(|r| r.iter().map(|it| it.text.as_str()).collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n");
    let el = early_text.to_lowercase();
    if !(el.contains("transaction")
        && el.contains("particulars")
        && el.contains("debit")
        && el.contains("credit")
        && el.contains("balance"))
    {
        return None;
    }
    let distinct_x = header_window
        .iter()
        .flat_map(|r| r.iter().map(|it| it.x.round() as i64))
        .collect::<std::collections::HashSet<_>>()
        .len();
    if distinct_x < 5 {
        return None;
    }

    // Header keywords, individually — this table's header wraps across 2
    // physical OCR rows ("Transaction | Value Date | Particulars | Cheque |
    // Debit | Credit | Balance" on one, "Date | No" continuation on the
    // next). `wall_x` anchors on the rightmost "Date" word (both
    // "Transaction Date" and "Value Date" say "Date" — the max of the two
    // is used, same trick as IDBI) so neither date column bleeds into
    // Particulars/amount classification.
    //
    // Scanned rows are restricted to the real header pair (the row
    // containing "particulars", plus the one right after it) rather than
    // the whole `header_window` — this statement's per-page summary box
    // ("Opening Balance | Total Debit | Total Credit | Closing Balance")
    // sits just above the real header and *also* contains the literal
    // words "Debit"/"Credit"/"Balance". Scanning the whole window let that
    // row's "Total Debit"/"Total Credit" x-positions win the anchor (being
    // encountered first), which put every real amount's nearest-anchor
    // column one slot to the right of where it belonged — confirmed
    // directly: the real Debit amount and the Balance both landed in
    // `credit_frags`, leaving `debit_frags`/`balance_frags` empty for
    // every single transaction.
    let hdr_row1_idx = header_window.iter().position(|r| {
        r.iter().any(|it| it.text.to_lowercase().contains("particulars"))
    });
    let anchor_rows: Vec<&&Vec<PdfItem>> = match hdr_row1_idx {
        Some(i) => header_window.iter().skip(i).take(2).collect(),
        None => return None,
    };
    let mut debit_x = None;
    let mut credit_x = None;
    let mut balance_x = None;
    let mut narration_x = None;
    let mut cheque_no_x = None;
    let mut wall_x: f64 = 0.0;
    for row in anchor_rows {
        for it in row.iter() {
            let l: String =
                it.text.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
            if l == "debit" && debit_x.is_none() {
                debit_x = Some(it.x);
            } else if l == "credit" && credit_x.is_none() {
                credit_x = Some(it.x);
            } else if l.starts_with("balance") && balance_x.is_none() {
                balance_x = Some(it.x);
            } else if l == "particulars" && narration_x.is_none() {
                narration_x = Some(it.x);
            } else if l == "cheque" && cheque_no_x.is_none() {
                cheque_no_x = Some(it.x);
            } else if l == "date" {
                wall_x = wall_x.max(it.x);
            }
        }
    }
    let (debit_x, credit_x, balance_x, narration_x) =
        match (debit_x, credit_x, balance_x, narration_x) {
            (Some(d), Some(c), Some(b), Some(n)) => (d, c, b, n),
            _ => return None,
        };
    let wall_x = if wall_x > 0.0 { wall_x + 20.0 } else { return None };
    // Cheque No candidates must sit strictly in the Cheque No column — see
    // `extract_idbi_transactions::cheque_no_min_x`'s doc comment for why a
    // wider range risks catching a narration-continuation digit run
    // instead. This fixture never actually populates Cheque No, but the
    // guard costs nothing and matches the established pattern.
    let cheque_no_min_x = match cheque_no_x {
        Some(cnx) => (narration_x + cnx) / 2.0,
        None => wall_x,
    };
    let cheque_no_max_x = debit_x.min(credit_x) - 10.0;

    // Account number IS printed in the clear for this statement (no
    // redaction, unlike ICICI Normal/IDBI) — pull the digit run immediately
    // after "ACCOUNT"+"NO" in the early header text.
    let account_no = header_window
        .iter()
        .find_map(|row| {
            let texts: Vec<&str> = row.iter().map(|it| it.text.as_str()).collect();
            let pos = texts.iter().position(|t| t.eq_ignore_ascii_case("NO"))?;
            if texts.get(pos.checked_sub(1)?)?.eq_ignore_ascii_case("ACCOUNT") {
                let cand = texts.get(pos + 1)?;
                if cand.chars().all(|c| c.is_ascii_digit()) && cand.len() >= 6 {
                    return Some(cand.to_string());
                }
            }
            None
        })
        .unwrap_or_default();

    // A transaction date in this statement is `DD-Mon-YYYY` (e.g.
    // "02-Apr-2024") — never matches narration text, which this fixture
    // never glues a dated-looking token into.
    static DATE_ITEM_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"^\D*(\d{1,2}-[A-Za-z]{3}-\d{4})").unwrap()
    });

    struct Block {
        date_display: String,
        date_ts: i64,
        narration_parts: Vec<String>,
        debit_frags: Vec<String>,
        credit_frags: Vec<String>,
        balance_frags: Vec<String>,
        chq_no: Option<String>,
    }

    let mut txns: Vec<Transaction> = Vec::new();
    let mut txn_counter = 0usize;
    let mut cur: Option<Block> = None;
    // The masthead/summary-box/header noise blocks are each exactly 2 rows
    // (a label row carrying the recognizable keyword, then a values-only
    // row with no keyword of its own) and repeat on every page — this flag
    // pairs them so the second row never reaches classification.
    let mut skip_next_row = false;

    macro_rules! flush {
        () => {
            if let Some(b) = cur.take() {
                let narration_joined: String = b
                    .narration_parts
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                let narration_joined = narration_joined
                    .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
                    .trim_end_matches(|c: char| !c.is_ascii_alphanumeric())
                    .to_string();
                fn clean_amount_frags(frags: &[String]) -> String {
                    frags
                        .concat()
                        .chars()
                        .filter(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
                        .collect()
                }
                let debit = parse_amount_str(&clean_amount_frags(&b.debit_frags));
                let credit = parse_amount_str(&clean_amount_frags(&b.credit_frags));
                let balance = parse_amount_str(&clean_amount_frags(&b.balance_frags));
                if debit.is_some() || credit.is_some() {
                    txn_counter += 1;
                    let reference = b.chq_no.clone().unwrap_or_else(|| {
                        extract_ref_from_narration(&narration_joined).unwrap_or_default()
                    });
                    let mut t = Transaction::new(format!("t_idfcfirst_{}", txn_counter));
                    t.date = b.date_display;
                    t.date_ts = b.date_ts;
                    t.narration = narration_joined;
                    t.reference = reference;
                    t.debit = debit;
                    t.credit = credit;
                    t.balance = balance;
                    t.bank_name = "IDFC First Bank".to_string();
                    t.account_no = account_no.clone();
                    txns.push(t);
                }
            }
        };
    }

    for row in rows.iter() {
        if row.is_empty() {
            continue;
        }
        if skip_next_row {
            skip_next_row = false;
            continue;
        }
        let row_joined: String =
            row.iter().map(|it| it.text.as_str()).collect::<Vec<_>>().join(" ");
        let rl = row_joined.to_lowercase();

        // End of the transaction table for good — the final page's
        // disclaimer section, confirmed to appear exactly once, only after
        // the very last real transaction.
        if rl.contains("important message") {
            break;
        }
        // Per-page masthead (5 independently-recognizable rows — no
        // "skip_next_row" pairing needed, each carries its own keyword).
        if rl.contains("statement of account")
            || rl.contains("customer id")
            || rl.contains("account no")
            || rl.contains("always you first")
            || rl.contains("statement period")
        {
            continue;
        }
        // Per-page summary box ("Opening Balance | Total Debit | Total
        // Credit | Closing Balance" labels, then a values-only row with no
        // keyword of its own — paired via skip_next_row).
        if rl.contains("opening") && rl.contains("balance") && rl.contains("total") {
            skip_next_row = true;
            continue;
        }
        // Per-page repeated column header ("Transaction ... Particulars
        // ... Debit ... Credit ... Balance", then a "Date | No"
        // continuation row — paired via skip_next_row).
        if rl.contains("transaction") && rl.contains("particulars") {
            skip_next_row = true;
            continue;
        }
        // Per-page footer (address line, then "Page X of Y" — paired via
        // skip_next_row).
        if rl.contains("registered office") {
            skip_next_row = true;
            continue;
        }

        // Cheque No: a pure-digit item sitting strictly in the Cheque No
        // column — see the doc comment above `cheque_no_min_x`.
        let chq_no_item = row.iter().find(|it| {
            it.x >= cheque_no_min_x
                && it.x < cheque_no_max_x
                && it.text.trim().chars().all(|c| c.is_ascii_digit())
                && it.text.trim().len() >= 4
        });
        let chq_no_x = chq_no_item.map(|it| it.x);
        let chq_no_found = chq_no_item.map(|it| it.text.trim().to_string());

        // First (leftmost) valid dated item in the row is this row's
        // Transaction Date — naturally the leftmost of the two date
        // columns (Transaction Date always precedes Value Date here).
        let mut row_date_display = String::new();
        let mut row_date_ts = 0i64;
        for it in row.iter() {
            let cleaned = it.text.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
            if let Some(caps) = DATE_ITEM_RE.captures(cleaned) {
                let nd = normalize_transaction_date(&caps[1]);
                if nd.valid {
                    row_date_display = nd.display;
                    row_date_ts = nd.ts;
                    break;
                }
            }
        }

        // Bare no-decimal digit-run rescue: a narration's own reference
        // number frequently gets OCR-split across word-boxes on the SAME
        // row as the date/amounts too, not just on continuation rows (e.g.
        // "NEFT/IDFBH24115429071/jalindhar..." split into "NEFT/IDFBH241"
        // + "15429071" + "/jalindhar..."). Every real amount in this
        // statement carries a decimal point, so a bare digit-only token is
        // never one — without this, "15429071" got swept in by
        // `is_amount_shaped`'s shape-only check, concatenated onto the
        // real "10,000.00" into an unparseable string, and silently
        // dropped the entire transaction (confirmed: this was the single
        // real transaction missing from this fixture's total, the sole
        // remaining discrepancy after the continuation-row-only version of
        // this same fix). Unlike the continuation-row rescue below, no
        // "dangling" guard is needed here — this fires once, on a
        // freshly-started block whose frags are still all empty, so
        // nothing can be a genuine cross-row split in progress yet.
        let rescue_bare_digits = |row: &[PdfItem],
                                   chq_no_x: Option<f64>,
                                   narration_parts: &mut Vec<String>|
         -> Vec<PdfItem> {
            let mut kept = Vec::new();
            let mut rescued_narration = Vec::new();
            for it in row.iter() {
                if Some(it.x) == chq_no_x {
                    continue;
                }
                let t = it.text.trim();
                let bare_whole_number = !t.is_empty() && t.chars().all(|c| c.is_ascii_digit());
                if bare_whole_number {
                    rescued_narration.push(t.to_string());
                } else {
                    kept.push(it.clone());
                }
            }
            if !rescued_narration.is_empty() {
                narration_parts.push(rescued_narration.join(" "));
            }
            kept
        };

        if !row_date_display.is_empty() {
            // New transaction block.
            flush!();
            let mut b = Block {
                date_display: row_date_display,
                date_ts: row_date_ts,
                narration_parts: Vec::new(),
                debit_frags: Vec::new(),
                credit_frags: Vec::new(),
                balance_frags: Vec::new(),
                chq_no: chq_no_found,
            };
            let filtered_row = rescue_bare_digits(row, chq_no_x, &mut b.narration_parts);
            classify_row(
                &filtered_row,
                wall_x,
                [debit_x, credit_x, balance_x],
                &mut b.narration_parts,
                &mut b.debit_frags,
                &mut b.credit_frags,
                &mut b.balance_frags,
            );
            cur = Some(b);
        } else if let Some(b) = cur.as_mut() {
            if b.chq_no.is_none() {
                b.chq_no = chq_no_found;
            }
            // Same bare-digit-run rescue as IDBI: only treat a bare
            // digit-run as a genuine split-amount continuation when one of
            // the amount columns is actually "dangling"; otherwise route
            // it into narration instead of letting shape-based amount
            // detection corrupt the real amount with it.
            let any_dangling =
                dangling(&b.debit_frags) || dangling(&b.credit_frags) || dangling(&b.balance_frags);
            let filtered_row: Vec<PdfItem> = if any_dangling {
                match chq_no_x {
                    Some(cx) => row.iter().filter(|it| it.x != cx).cloned().collect(),
                    None => row.clone(),
                }
            } else {
                let mut kept = Vec::new();
                let mut rescued_narration = Vec::new();
                for it in row.iter() {
                    if Some(it.x) == chq_no_x {
                        continue;
                    }
                    let t = it.text.trim();
                    // No minimum length here (unlike IDBI's analogous
                    // rescue): every real Debit/Credit/Balance amount in
                    // this statement carries a decimal point ("677.00",
                    // "24.00" -- even sub-thousand ones), so a bare
                    // no-decimal digit run of ANY length on a continuation
                    // row is never a real amount. Confirmed against this
                    // fixture: without this, a narration's own free-text
                    // day-of-month ("salary march 24 jay", OCR-split
                    // across lines as "march"/"24"/"jay") got swept in as
                    // a phantom amount by is_amount_shaped's shape-only
                    // check, silently destroying every real Debit/Credit
                    // value on the statement.
                    let bare_whole_number = !t.is_empty() && t.chars().all(|c| c.is_ascii_digit());
                    if bare_whole_number {
                        rescued_narration.push(t.to_string());
                    } else {
                        kept.push(it.clone());
                    }
                }
                if !rescued_narration.is_empty() {
                    b.narration_parts.push(rescued_narration.join(" "));
                }
                kept
            };
            classify_row(
                &filtered_row,
                wall_x,
                [debit_x, credit_x, balance_x],
                &mut b.narration_parts,
                &mut b.debit_frags,
                &mut b.credit_frags,
                &mut b.balance_frags,
            );
        }
    }
    flush!();

    // Anti-false-positive guard, same threshold every other dedicated
    // extractor in this module uses.
    if txns.len() < 2 {
        log::debug!(
            "[BSP IDFC First] only {} transactions extracted from \"{}\" — treating as a non-match",
            txns.len(),
            file_name
        );
        return None;
    }

    // Already chronological (oldest first) — no reordering needed, unlike
    // IDBI's own reverse-chronological statement.
    let op_balance = compute_prev_balances(&mut txns, None);

    // Debit/Credit must never mix — this table has separate Debit/Credit
    // columns, so a real row only ever posts to one. Balance-chain ground
    // truth decides which side is real, same as ICICI Normal/IDBI.
    for t in txns.iter_mut() {
        if let (Some(dr), Some(cr)) = (t.debit, t.credit) {
            let keep_credit = match (t.prev_balance, t.balance) {
                (Some(pb), Some(bal)) => {
                    let diff = ((bal - pb) * 100.0).round() / 100.0;
                    let tol = |amt: f64| f64::max(1.0, amt * 0.02);
                    let cr_fits = (diff - cr).abs() < tol(cr);
                    let dr_fits = (diff + dr).abs() < tol(dr);
                    if cr_fits && !dr_fits {
                        true
                    } else if dr_fits && !cr_fits {
                        false
                    } else {
                        cr >= dr
                    }
                }
                _ => cr >= dr,
            };
            if keep_credit {
                t.debit = None;
            } else {
                t.credit = None;
            }
        }
    }

    // Balance-chain repair, same rationale as IDBI: Debit/Credit are
    // independently verified correct against this fixture, so a Balance
    // that doesn't reconcile is corrected from the chain rather than
    // trusted as printed.
    let mut running_balance = op_balance;
    for t in txns.iter_mut() {
        if let Some(pb) = running_balance {
            let expected =
                ((pb + t.credit.unwrap_or(0.0) - t.debit.unwrap_or(0.0)) * 100.0).round() / 100.0;
            if let Some(bal) = t.balance {
                if (expected - bal).abs() > 0.01 {
                    log::debug!(
                        "[BSP IDFC First] balance chain mismatch for {} \"{}\": OCR'd {:.2}, chain says {:.2} — using chain value",
                        t.date,
                        safe_prefix(&t.narration, 40),
                        bal,
                        expected
                    );
                    t.balance = Some(expected);
                    t.prev_balance = Some(pb);
                }
            }
        }
        running_balance = t.balance;
    }

    prepend_opening_balance_row(&mut txns, op_balance, "IDFC First Bank", &account_no);

    Some(ParseResult {
        transactions: txns,
        opening_balance: op_balance,
        closing_balance: None,
        bank_name: "IDFC First Bank".to_string(),
        account_no,
        source_name: file_name.to_string(),
        col_map: Default::default(),
        header_row_idx: 0,
        noise_row_count: 0,
        rejected_row_count: 0,
    })
}

// ── extract_cosmos_transactions ───────────────────────────────────────────────

/// Port of `Parser._parseCosmosFW(rows, fileName)`.
///
/// Parser for Cosmos Co-operative Bank PDF statements.
///
/// Layout (one text item per row, or multiple items joined):
///   Date | Particulars | Chq.No. | Withdrawals | Deposits | Balance(Cr/Dr)
///
/// Direction is determined **solely by balance movement**:
///   balance increased → credit (Deposits column)
///   balance decreased → debit  (Withdrawals column)
///
/// Narration keywords are used only as a fallback when `prev_bal` is unknown.
pub fn extract_cosmos_transactions(rows: &[Vec<PdfItem>], file_name: &str) -> Option<ParseResult> {
    // ── Step 1: locate Cosmos header ─────────────────────────────────────────
    let mut hdr_idx = usize::MAX;
    // Character offsets of the "Withdrawals" / "Deposits" column headings within
    // the header line — this is a fixed-width text table (every row lines up on
    // the same character columns), so these offsets double as the true column
    // boundaries for every transaction row below. Used in Step 3 to resolve
    // debit/credit for a row **by which column its amount actually sits under**,
    // instead of guessing from narration keywords (see that step's comment for
    // why the keyword guess alone is unreliable).
    let mut wd_col: Option<usize> = None;
    let mut dep_col: Option<usize> = None;
    for (i, row) in rows.iter().enumerate().take(50) {
        let line = row
            .iter()
            .map(|it| it.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let ll = line.to_lowercase();
        if ll.contains("date")
            && ll.contains("particulars")
            && ll.contains("chq")
            && ll.contains("withdrawal")
            && ll.contains("deposit")
            && ll.contains("balance")
        {
            hdr_idx = i;
            // Both substrings are ASCII, so byte offsets in the lowercased copy
            // are identical to offsets in `line` itself.
            wd_col = ll.find("withdrawal");
            dep_col = ll.find("deposit");
            break;
        }
    }
    if hdr_idx == usize::MAX {
        log::debug!(
            "[BSP Cosmos] No Cosmos header in \"{}\" — skipping",
            file_name
        );
        return None;
    }

    // Classify a transaction amount by which header column its text actually
    // starts under (nearest of the two column headings wins). This is the
    // authoritative signal — it reads the same column position a human would
    // look at — used when there is no previous balance to diff against (the
    // opening-balance seed row, or a row with a completely unparseable prior
    // balance).
    let classify_by_column = |amt_col: usize| -> Option<bool> {
        match (wd_col, dep_col) {
            (Some(w), Some(d)) => {
                let dw = (amt_col as isize - w as isize).unsigned_abs();
                let dd = (amt_col as isize - d as isize).unsigned_abs();
                Some(dd < dw) // true = credit (Deposits column), false = debit (Withdrawals)
            }
            _ => None,
        }
    };

    // ── Step 2: parse transaction rows ────────────────────────────────────────
    // Structure holds txnVal temporarily until direction is resolved in Step 3.
    struct Pending {
        t: Transaction,
        txn_val: f64,
        /// Character offset of the amount's first byte within its row's `line`
        /// — compared against `wd_col`/`dep_col` in Step 3.
        amt_col: usize,
    }

    let mut pending: Vec<Pending> = Vec::new();
    let mut op_balance: Option<f64> = None;
    let mut closing_balance: Option<f64> = None;
    let mut txn_counter = 0usize;

    for (i, row) in rows.iter().enumerate().skip(hdr_idx + 1) {
        let line = row
            .iter()
            .map(|it| it.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        if line.is_empty() || line.chars().all(|c| c == '-' || c == '=' || c == ' ') {
            continue;
        }

        // Date at start
        let (date_str, date_orig_len) = match starts_with_date(&line) {
            Some(d) => d,
            None => continue,
        };
        let nd = normalize_transaction_date(&date_str);
        if !nd.valid {
            continue;
        }

        // Balance at end: "NNN,NNN.NNCr" or "Dr"
        let (balance, bal_raw_start, _) = match extract_balance_suffix(&line) {
            Some(b) => b,
            None => continue,
        };

        // See the equivalent comment in extract_fw_transactions above —
        // `date_orig_len` is the date pattern's real byte length in
        // `line` (Phase 4L.2.2 follow-up); `date_str.len()` alone would
        // undershoot on a typographic-dash date and leak trailing date
        // bytes into `middle`, not just risk an unsafe slice.
        let date_part_len = floor_char_boundary(&line, date_orig_len + 1);
        let (middle, middle_start) = if bal_raw_start > date_part_len {
            let raw = &line[date_part_len..bal_raw_start];
            let leading_ws = raw.len() - raw.trim_start().len();
            (raw.trim().to_string(), date_part_len + leading_ws)
        } else {
            (String::new(), date_part_len)
        };
        let ml = middle.to_lowercase();

        if ml.contains("opening bal") || ml.contains("op bal") {
            op_balance = Some(balance);
            continue;
        }
        if ml.contains("closing bal") || ml.contains("cl bal") {
            closing_balance = Some(balance);
            continue;
        }
        if is_noise_row(&middle) {
            continue;
        }

        // ── Amount extraction: rightmost valid decimal (≤ 10 int digits) ──────
        let amt_candidates: Vec<(f64, usize, String)> = extract_amounts(&middle)
            .into_iter()
            .filter(|(_, _, raw)| int_digit_count(raw) <= 10)
            .collect();
        if amt_candidates.is_empty() {
            continue;
        }

        let (txn_val, txn_idx, _) = amt_candidates.last().unwrap().clone();
        let text_part = middle[..txn_idx].trim().to_string();

        // Chq.No. = trailing 4–7 digit integer in textPart
        let (narration, reference) = extract_cosmos_ref(&text_part);
        if narration.is_empty() {
            continue;
        }

        txn_counter += 1;
        let mut t = Transaction::new(format!("t_cosmos_{}_{}", i, txn_counter));
        t.date = nd.display;
        t.date_ts = nd.ts;
        t.narration = narration;
        t.reference = reference;
        t.balance = Some(balance);
        t.bank_name = "Cosmos Co-operative Bank".to_string();
        // debit/credit resolved in Step 3
        pending.push(Pending {
            t,
            txn_val,
            amt_col: middle_start + txn_idx,
        });
    }

    if pending.len() < 2 {
        return None;
    }

    // ── Step 3: determine debit / credit from balance movement ────────────────
    let is_cosmos_credit = |nl: &str| -> bool {
        nl.contains("upi-cr")
            || nl.contains("prcr/")
            || nl.contains("upi cr")
            || (nl.contains("neft") && nl.contains("cr"))
            || nl.contains("^by ")
            || nl.starts_with("by/")
            || nl.starts_with("by ")
            || nl.contains("imps/p2a")
            || nl.contains("upi-rd")
            || nl.contains("salary")
            || nl.contains("refund")
            || nl.contains("interest")
            || nl.contains("reversal")
            || nl.contains("deposit")
    };
    let mut prev_bal = op_balance;

    // Seed prevBal when opening balance unknown. Column position (which of
    // Withdrawals/Deposits the amount actually sits under) is the authoritative
    // signal here — narration keywords are a guess and this codebase's own
    // "PRCR/" keyword is a confirmed false positive: real Cosmos statements
    // print ordinary Withdrawals-column transactions under a "PRCR/<UTR>/..."
    // narration (verified against "Cosmos Co-operative.pdf"'s first
    // transaction, a payment/debit that `is_cosmos_credit`'s "prcr/" branch
    // was misreading as a receipt/credit), so keyword matching alone cannot be
    // trusted to seed the very first row, which has no prior balance to
    // cross-check against. Fall back to the keyword guess only when the header
    // didn't yield usable column offsets.
    if prev_bal.is_none() {
        let seed = &pending[0];
        let is_credit = classify_by_column(seed.amt_col)
            .unwrap_or_else(|| is_cosmos_credit(&seed.t.narration.to_lowercase()));
        if is_credit {
            prev_bal = Some(((seed.t.balance.unwrap() - seed.txn_val) * 100.0).round() / 100.0);
        } else {
            prev_bal = Some(((seed.t.balance.unwrap() + seed.txn_val) * 100.0).round() / 100.0);
        }
    }

    let mut resolved: Vec<Transaction> = Vec::new();

    for mut p in pending {
        let bal = p.t.balance.unwrap();
        let tv = p.txn_val;

        if prev_bal.is_none() {
            let is_credit = classify_by_column(p.amt_col)
                .unwrap_or_else(|| is_cosmos_credit(&p.t.narration.to_lowercase()));
            if is_credit {
                p.t.credit = Some(tv);
            } else {
                p.t.debit = Some(tv);
            }
            prev_bal = Some(bal);
            resolved.push(p.t);
            continue;
        }

        let diff = ((bal - prev_bal.unwrap()) * 100.0).round() / 100.0;
        let tol = (tv * 0.001_f64).max(0.02);

        if (diff - tv).abs() <= tol {
            p.t.credit = Some(tv); // balance UP → Deposits / credit
        } else if (diff + tv).abs() <= tol {
            p.t.debit = Some(tv); // balance DOWN → Withdrawals / debit
        } else {
            // Reconciliation miss → narration keyword fallback
            let nl = p.t.narration.to_lowercase();
            if is_cosmos_credit(&nl) {
                p.t.credit = Some(tv);
            } else {
                p.t.debit = Some(tv);
            }
        }

        prev_bal = Some(bal);
        resolved.push(p.t);
    }

    let valid: Vec<Transaction> = resolved
        .into_iter()
        .filter(|t| t.debit.is_some() || t.credit.is_some())
        .collect();
    if valid.len() < 2 {
        return None;
    }

    let mut txns = valid;
    let op_balance = compute_prev_balances(&mut txns, op_balance);
    prepend_opening_balance_row(&mut txns, op_balance, "Cosmos Co-operative Bank", "");

    Some(ParseResult {
        transactions: txns,
        opening_balance: op_balance,
        closing_balance,
        bank_name: "Cosmos Co-operative Bank".to_string(),
        account_no: String::new(),
        source_name: file_name.to_string(),
        col_map: Default::default(),
        header_row_idx: hdr_idx,
        noise_row_count: 0,
        rejected_row_count: 0,
    })
}

/// Extract Cosmos narration and reference from the text portion before the txn amount.
///
/// Two reference shapes, tried in order:
///
/// 1. **UPI/NEFT/IMPS/PRCR/ATM-style UTR**: these narrations are
///    slash-delimited (`PRCR/303213675227/S R TRADER 13:16`,
///    `UPI-DR/303433640023/gpay-112140716`,
///    `0191/ATM/XXXXXX1775CWDR/3037184018`) with the actual UTR/transaction
///    ID sitting in its own segment as a bare 6–16 digit run. Previously
///    this whole string was kept as narration verbatim and Reference was
///    left empty — a real bug (2026-08-28): every UPI/IMPS/PRCR row in a
///    real "Cosmos Co-operative.pdf" statement showed no Reference at all.
///    Fixed by pulling that digit-only segment out into `reference` and
///    rejoining the remaining segments (transaction-type prefix + party/
///    description) with `/` as the narration — e.g. `PRCR/S R TRADER
///    13:16`, `UPI-DR/gpay-112140716`. Narration stays meaningful (keeps
///    the prefix identifying UPI-DR/UPI-CR/PRCR/etc. and the party/handle);
///    Reference gets the actual, non-redundant transaction ID.
/// 2. **Cheque/bank-collection reference**: no slash, but a standalone
///    4–7 digit token appears somewhere in the text — Cosmos's own
///    "Chq.No." column (`JAYANTILAL AND COMPANY   7311` → ref `7311`) and
///    unlabeled bank-collection rows (`BY   3256 HDFC AND` → ref `3256`)
///    both take this shape. The token is pulled out of narration wherever
///    it sits, not just at the end.
///
/// If neither shape matches (e.g. Cosmos's own NEFT rows, which this
/// statement's layout truncates to `NEFT:ASPEN FOODS PVT LTD:ICIC` with no
/// reference number surviving in the source text at all), narration is
/// left as-is and reference stays empty — there is genuinely nothing to
/// extract.
fn extract_cosmos_ref(text_part: &str) -> (String, String) {
    let is_ref_digits =
        |s: &str| (6..=16).contains(&s.len()) && s.chars().all(|c| c.is_ascii_digit());

    // 1) Slash-delimited UTR/transaction-ID segment.
    if text_part.contains('/') {
        let segments: Vec<&str> = text_part.split('/').collect();
        if segments.len() > 1 {
            if let Some(idx) = segments.iter().position(|s| is_ref_digits(s)) {
                let reference = segments[idx].to_string();
                let rest: Vec<&str> = segments
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != idx)
                    .map(|(_, s)| *s)
                    .collect();
                let narr = rest.join("/").trim().to_string();
                let narr = if narr.is_empty() {
                    text_part.to_string()
                } else {
                    narr
                };
                return (narr, reference);
            }
        }
    }

    // 2) Standalone 4–7 digit cheque/collection reference token, wherever
    //    it sits in the text (Cosmos's own "Chq.No." column, or an
    //    unlabeled bank-collection reference).
    let words: Vec<&str> = text_part.split_whitespace().collect();
    if let Some((wi, chq)) = words
        .iter()
        .enumerate()
        .find(|(_, w)| w.len() >= 4 && w.len() <= 7 && w.chars().all(|c| c.is_ascii_digit()))
    {
        let narr = words
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != wi)
            .map(|(_, s)| *s)
            .collect::<Vec<_>>()
            .join(" ");
        let narr = if narr.trim().is_empty() {
            text_part.to_string()
        } else {
            narr
        };
        return (narr, chq.to_string());
    }

    (text_part.trim().to_string(), String::new())
}

// ── Kotak "narrow" e-statement PDF layout ─────────────────────────────────────
//
// Some Kotak Mahindra Bank statement PDFs (a mobile-app/e-statement export
// template — confirmed 2026-08-25 against the real "Kotak Bank.pdf" fixture,
// which is the exact file a live Debit/Credit-mixing bug report traced back
// to this layout) render each transaction as 8 SEPARATE physical text lines
// rather than columns sharing a line or an X position:
//
//   Sl. No. | Date | Time | Value Date | Narration | Chq./Ref. No. |
//   signed Amount | Balance
//
// `text_extractor`'s row-clustering yields one `PdfItem` per physical line
// here, all at the same X (0.0) — every column boundary this module's other
// two extractors rely on is gone: `extract_fw_transactions` needs a whole
// transaction on *one* line, and `column_detector`'s header/boundary
// detection needs distinct X positions per column. Neither matches, so
// `parse_pdf_rows` used to fall all the way through to the OCR-text
// fallback path (`ocr_parser`'s flat full-text extraction), which has no
// column identity at all — confirmed (via `examples/kotak_debug_probe.rs`)
// to silently read the real running **Balance** into the Debit/Credit
// field and the **Sl. No.** row-counter into the Balance field for the
// large majority of transactions, corrupting every amount in the file.
//
// This extractor is a `parse_pdf_rows` **last-resort fallback**, tried only
// after normal header detection *and* `extract_fw_transactions` have both
// already failed (see its call site in `pdf_parser.rs`) — it can only ever
// add coverage for a file nothing else already handles; it is never in a
// position to change what any currently-working bank/layout produces,
// including the *other*, traditionally-tabular Kotak layout the main
// column-based loop already special-cases via its own "Kotak signed
// combined column" handling (`ColField::DebitCredit`).
//
// The signed Amount line is the one unambiguous anchor: it is the only one
// of the 8 fields carrying an explicit leading sign — `+56,238.00` for a
// Credit, `-562,389.00` for a Debit, confirmed against every real
// transaction in the fixture with zero exceptions. Direction always comes
// from that sign, never from balance movement (which the fallback path's
// bug shows can't be trusted to even land in the right field, let alone be
// used to infer direction).

/// `s` is a bare small non-negative integer — the "Sl. No." column's shape.
/// Capped at 6 digits: a genuine transaction serial number in any real
/// statement is far shorter, and this cap keeps an ordinary amount or
/// reference number from ever being mistaken for one.
fn is_kotak_sl_no(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && s.len() <= 6 && s.chars().all(|c| c.is_ascii_digit())
}

/// `s` is a bare "H:MM AM"/"HH:MM PM" time-of-day — the "Time" column's shape.
fn is_kotak_time_of_day(s: &str) -> bool {
    let s = s.trim();
    let rest = s
        .strip_suffix("AM")
        .or_else(|| s.strip_suffix("PM"))
        .or_else(|| s.strip_suffix("am"))
        .or_else(|| s.strip_suffix("pm"));
    let Some(rest) = rest else { return false };
    let parts: Vec<&str> = rest.trim().split(':').collect();
    parts.len() == 2
        && !parts[0].is_empty()
        && parts[0].len() <= 2
        && parts[0].chars().all(|c| c.is_ascii_digit())
        && parts[1].len() == 2
        && parts[1].chars().all(|c| c.is_ascii_digit())
}

/// `s` is a bare (unsigned) decimal amount, comma grouping optional, 0-2
/// decimal places — the "Balance" column's shape (never signed).
fn is_kotak_plain_amount(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let mut dot_seen = false;
    let mut digits_after_dot = 0usize;
    let mut any_digit = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            any_digit = true;
            if dot_seen {
                digits_after_dot += 1;
            }
        } else if c == ',' && !dot_seen {
            // thousands separator — only valid before the decimal point
        } else if c == '.' && !dot_seen {
            dot_seen = true;
        } else {
            return false;
        }
    }
    any_digit && (!dot_seen || (1..=2).contains(&digits_after_dot))
}

/// `s` is a signed decimal amount — `+`/`-` immediately followed by a
/// `is_kotak_plain_amount`-shaped number. This is the layout's Debit/Credit
/// direction anchor (see the module doc comment above): every real
/// transaction amount in this layout carries an explicit sign, so a line
/// that doesn't is never mistaken for one — it's either the unsigned
/// Balance that always immediately follows it, or narration/reference text.
fn is_kotak_signed_amount(s: &str) -> bool {
    let s = s.trim();
    let rest = s.strip_prefix('+').or_else(|| s.strip_prefix('-'));
    rest.is_some_and(is_kotak_plain_amount)
}

/// Minimum number of transaction blocks this layout must find before it's
/// trusted — mirrors `extract_cosmos_transactions`'s own `pending.len() < 2`
/// sanity floor (own doc comment above it) against a false-positive match on
/// some unrelated document that happens to contain a couple of small-
/// integer/date-shaped lines by coincidence. Set a little higher here since
/// this layout's block shape (4-line anchor, then a bounded scan for a
/// signed-amount line) is looser than Cosmos's single-line-per-transaction
/// match.
const MIN_KOTAK_NARROW_TXNS: usize = 3;

/// How many lines past the 4-line Sl.No/Date/Time/ValueDate anchor this will
/// scan looking for the signed-Amount line before giving up on that block —
/// generous enough for a multi-line-wrapped narration+reference (never seen
/// to exceed 2 lines in the real fixture) without letting one failed match
/// scan arbitrarily far into the rest of the document.
const MAX_KOTAK_NARRATION_SPAN: usize = 12;

/// Port of the Kotak "narrow" e-statement layout described above.
/// Returns `(transactions, opening_balance, closing_balance)` in the same
/// shape `extract_fw_transactions` does, for the same caller-side
/// post-processing (`compute_prev_balances` derives the opening balance from
/// the first transaction when this layout has no explicit "Opening Balance"
/// line of its own, exactly as it already does for `extract_fw_transactions`
/// callers with the same gap).
pub fn extract_kotak_narrow_transactions(
    rows: &[Vec<PdfItem>],
    file_name: &str,
) -> Option<(Vec<Transaction>, Option<f64>, Option<f64>)> {
    let lines: Vec<String> = rows
        .iter()
        .filter(|r| !r.is_empty())
        .map(|r| {
            r.iter()
                .map(|it| it.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();

    let n = lines.len();
    let mut txns: Vec<Transaction> = Vec::new();
    let mut txn_counter = 0usize;
    let mut i = 0usize;

    while i < n {
        let is_block_start = i + 3 < n
            && is_kotak_sl_no(&lines[i])
            && normalize_transaction_date(&lines[i + 1]).valid
            && is_kotak_time_of_day(&lines[i + 2])
            && normalize_transaction_date(&lines[i + 3]).valid;
        if !is_block_start {
            i += 1;
            continue;
        }

        let nd = normalize_transaction_date(&lines[i + 1]);

        // Scan forward for the signed-amount anchor.
        let scan_end = n.min(i + 4 + MAX_KOTAK_NARRATION_SPAN);
        let amt_idx = (i + 4..scan_end).find(|&j| is_kotak_signed_amount(&lines[j]));
        let Some(amt_idx) = amt_idx else {
            // No amount found in range — this wasn't really a transaction
            // block (or the layout assumption broke down here); move past
            // just the anchor and keep scanning rather than getting stuck.
            i += 1;
            continue;
        };

        let signed = parse_amount_str(&lines[amt_idx]);
        let Some(signed) = signed else {
            i = amt_idx + 1;
            continue;
        };

        // Narration + reference: everything between the Value Date (i+3)
        // and the signed Amount. The line immediately before the amount is
        // the reference; anything earlier (normally exactly one line) is
        // narration.
        let between = &lines[i + 4..amt_idx];
        let (narration, reference) = match between.len() {
            0 => (String::new(), String::new()),
            1 => (between[0].clone(), String::new()),
            _ => (
                between[..between.len() - 1].join(" "),
                between[between.len() - 1].clone(),
            ),
        };

        if narration.is_empty() || is_noise_row(&narration) {
            i = amt_idx + 1;
            continue;
        }

        // Balance immediately follows the signed Amount.
        let bal_idx = amt_idx + 1;
        let (balance, next_i) = if bal_idx < n && is_kotak_plain_amount(&lines[bal_idx]) {
            (parse_amount_str(&lines[bal_idx]), bal_idx + 1)
        } else {
            (None, bal_idx)
        };

        // Sign determines direction — never balance movement (spec: "the
        // sign must determine the transaction direction"; see the module
        // doc comment for why balance movement is exactly what corrupted
        // this data in the OCR-text fallback path).
        let (debit, credit) = if signed < 0.0 {
            (Some(signed.abs()), None)
        } else {
            (None, Some(signed))
        };

        txn_counter += 1;
        let mut t = Transaction::new(format!("t_kotak_narrow_{}_{}", i, txn_counter));
        t.date = nd.display;
        t.date_ts = nd.ts;
        t.narration = narration;
        t.reference = reference;
        t.debit = debit;
        t.credit = credit;
        t.balance = balance;
        t.bank_name = "Kotak Mahindra Bank".to_string();
        txns.push(t);

        i = next_i;
    }

    if txns.len() < MIN_KOTAK_NARROW_TXNS {
        log::debug!(
            "[BSP Kotak narrow] \"{}\": only {} block(s) matched — not trusting this layout",
            file_name,
            txns.len()
        );
        return None;
    }

    log::debug!(
        "[BSP Kotak narrow] \"{}\": matched {} transactions",
        file_name,
        txns.len()
    );
    Some((txns, None, None))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf_item(text: &str) -> PdfItem {
        PdfItem {
            x: 10.0,
            text: text.to_owned(),
            w: 400.0,
        }
    }

    fn row_of(text: &str) -> Vec<PdfItem> {
        vec![pdf_item(text)]
    }

    // ── starts_with_date ──────────────────────────────────────────────────────

    #[test]
    fn date_detected_slash() {
        assert!(starts_with_date("01/01/2024 SALARY").is_some());
    }

    #[test]
    fn date_detected_dash() {
        assert!(starts_with_date("15-03-2024 ATM WDL").is_some());
    }

    #[test]
    fn non_date_line_none() {
        assert!(starts_with_date("SALARY PAYMENT").is_none());
    }

    /// Phase 4L.2.2: a typographic em-dash (U+2014, 3 bytes) as a date
    /// separator — a realistic PDF/copy-paste artifact this parser already
    /// special-cases (`bytes[2] == 0xE2` in `starts_with_date`) — used to
    /// panic when the resulting `normalized[..10]` byte-index cut landed
    /// mid-character. Must not panic, whatever it returns.
    #[test]
    fn date_with_em_dash_separator_does_not_panic() {
        let _ = starts_with_date("01—01—2024 SALARY"); // must not panic
    }

    #[test]
    fn date_with_mixed_ascii_and_em_dash_separators_does_not_panic() {
        let _ = starts_with_date("01-01—2024 SALARY"); // must not panic
        let _ = starts_with_date("01—01-2024 SALARY"); // must not panic
    }

    // ── extract_amounts ───────────────────────────────────────────────────────

    #[test]
    fn amounts_from_line() {
        let amts = extract_amounts("SALARY 50,000.00 1,50,000.00");
        assert_eq!(amts.len(), 2);
        assert!((amts[0].0 - 50000.0).abs() < 0.01);
        assert!((amts[1].0 - 150000.0).abs() < 0.01);
    }

    #[test]
    fn amount_rejects_utr_digits() {
        // 303213675227.00 has 12 int digits → int_digit_count=12 > 10
        let amts = extract_amounts("303213675227.00");
        assert_eq!(amts.len(), 1); // extract_amounts doesn't filter — caller filters
        assert_eq!(int_digit_count(&amts[0].2), 12);
    }

    // ── extract_balance_suffix ────────────────────────────────────────────────

    #[test]
    fn balance_suffix_cr() {
        let (val, _, _) =
            extract_balance_suffix("01/01/2024 SALARY 50000.00 1,50,000.00Cr").unwrap();
        assert!((val - 150000.0).abs() < 0.01);
    }

    #[test]
    fn balance_suffix_dr() {
        let (val, _, _) = extract_balance_suffix("ATM WDL 10000.00 1,40,000.00Dr").unwrap();
        assert!((val - 140000.0).abs() < 0.01);
    }

    #[test]
    fn no_balance_suffix_returns_none() {
        assert!(extract_balance_suffix("SALARY 50000.00").is_none());
    }

    // ── extract_cosmos_ref ────────────────────────────────────────────────────

    #[test]
    fn cosmos_ref_extracted() {
        let (narr, chq) = extract_cosmos_ref("UPI-DR-305561534108-SWIGGY 123456");
        assert_eq!(chq, "123456");
        assert!(narr.contains("SWIGGY") || narr.contains("UPI"));
    }

    #[test]
    fn cosmos_slash_delimited_utr_extracted_as_reference() {
        // Real bug (2026-08-28): this used to keep the 12-digit UTR glued
        // into narration and leave Reference empty for every UPI/NEFT/IMPS/
        // PRCR row in a real Cosmos statement. The UTR segment must now be
        // pulled into `reference`, and narration must keep the meaningful
        // prefix + party text without the raw digit run mixed in.
        let (narr, reference) = extract_cosmos_ref("UPI-DR/305561534108/AMAZON");
        assert_eq!(reference, "305561534108");
        assert_eq!(narr, "UPI-DR/AMAZON");
        assert!(!narr.contains("305561534108"));
    }

    // ── extract_fw_transactions (Format A — two-column) ───────────────────────

    fn fw_format_a_rows() -> Vec<Vec<PdfItem>> {
        // Simulate a simple fixed-width PDF with two amount columns (Withdrawal | Deposit).
        // Header positions: "Date" at 0, "Withdrawal" at ~50, "Deposit" at ~70, "Balance" at ~90
        vec![
            row_of("Date         Narration               Withdrawal Deposit Balance"),
            row_of("01-01-2024   SALARY CREDIT                      50000.00 1,50,000.00Cr"),
            row_of("02-01-2024   ATM WDL BANDRA          10000.00            1,40,000.00Cr"),
            row_of("03-01-2024   SWIGGY ORDER             850.00             1,39,150.00Cr"),
            row_of("04-01-2024   NEFT FROM RAJESH                   25000.00 1,64,150.00Cr"),
        ]
    }

    #[test]
    fn fw_format_a_detects_header() {
        let rows = fw_format_a_rows();
        let result = extract_fw_transactions(&rows, "test.pdf");
        assert!(result.is_some(), "Format A should be detected");
    }

    #[test]
    fn fw_format_a_extracts_transactions() {
        let rows = fw_format_a_rows();
        let (txns, _, _) = extract_fw_transactions(&rows, "test.pdf").unwrap();
        assert!(!txns.is_empty(), "transactions extracted");
    }

    // ── extract_fw_transactions (Format B — single amount) ────────────────────

    fn fw_format_b_rows() -> Vec<Vec<PdfItem>> {
        // Format B: single "Amount" column — direction inferred from keywords + balance movement
        vec![
            row_of("Date         Particulars                    Amount    Balance"),
            row_of("01-01-2024   UPI-CR/305561/SALARY           50000.00 1,50,000.00Cr"),
            row_of("02-01-2024   UPI-DR/105561/ATM WDL          10000.00 1,40,000.00Cr"),
            row_of("03-01-2024   NEFT CR RAJESH SHAH            25000.00 1,65,000.00Cr"),
        ]
    }

    #[test]
    fn fw_format_b_detected() {
        let rows = fw_format_b_rows();
        let result = extract_fw_transactions(&rows, "test.pdf");
        assert!(result.is_some(), "Format B should be detected");
    }

    #[test]
    fn fw_format_b_upi_cr_is_credit() {
        let rows = fw_format_b_rows();
        let (txns, _, _) = extract_fw_transactions(&rows, "test.pdf").unwrap();
        let salary = txns
            .iter()
            .find(|t| {
                t.narration.to_lowercase().contains("salary")
                    || t.narration.to_lowercase().contains("cr")
            })
            .expect("UPI-CR row");
        assert!(salary.credit.is_some(), "UPI-CR row should be credit");
    }

    #[test]
    fn fw_format_b_upi_dr_is_debit() {
        let rows = fw_format_b_rows();
        let (txns, _, _) = extract_fw_transactions(&rows, "test.pdf").unwrap();
        let atm = txns
            .iter()
            .find(|t| {
                t.narration.to_lowercase().contains("atm")
                    || t.narration.to_lowercase().contains("upi-dr")
            })
            .expect("UPI-DR row");
        assert!(atm.debit.is_some(), "UPI-DR row should be debit");
    }

    /// Phase 4L.2.2: `date_part_len` (derived from `starts_with_date`'s
    /// ASCII-normalized reconstruction) used to be applied directly as a
    /// byte offset into the *original*, un-normalized `line` — panicking
    /// whenever a row's actual date separator was a multi-byte character
    /// (em-dash, en-dash, minus sign). The whole fixed-width extraction
    /// pipeline must survive such a row without panicking, and must still
    /// process the other, normally-separated rows around it.
    #[test]
    fn fw_extraction_survives_a_row_with_em_dash_date_separators() {
        let rows = vec![
            row_of("Date         Particulars                    Amount    Balance"),
            row_of("01—01—2024   UPI-CR/305561/SALARY           50000.00 1,50,000.00Cr"),
            row_of("02-01-2024   UPI-DR/105561/ATM WDL          10000.00 1,40,000.00Cr"),
        ];
        let result = extract_fw_transactions(&rows, "test.pdf"); // must not panic
        if let Some((txns, _, _)) = result {
            assert!(
                txns.iter()
                    .any(|t| t.narration.to_lowercase().contains("atm")),
                "the normally-separated row should still be extracted"
            );
        }
    }

    /// Phase 4L.2.2 follow-up: a minus sign (U+2212) as the *second* date
    /// separator (between MM and YYYY) *is* recognized by
    /// `starts_with_date` — it's `.replace()`d to ASCII before the split,
    /// so `Some` is returned, unlike an em-dash. (The *first* separator
    /// can't be 3 bytes and still reach `Some` at all — see
    /// `starts_with_date`'s own doc comment — so this is the one
    /// realistically reachable shape.) This is the actual live
    /// corruption path the crash-safety pass's `floor_char_boundary`
    /// alone didn't close: it prevented a panic, but `date_part_len`
    /// still undershot the real end of the date by 2 bytes (a 3-byte
    /// separator vs. the reconstructed "DD-MM-YYYY"'s 1-byte one),
    /// leaking the last digit of "2024" into the extracted narration as
    /// a stray "4" prefix. Must now produce the *correct* narration, not
    /// just avoid panicking.
    #[test]
    fn fw_minus_sign_date_separator_does_not_leak_into_narration() {
        let rows = vec![
            row_of("Date         Particulars                    Amount    Balance"),
            row_of("01-01\u{2212}2024   SALARY CREDIT                 50000.00 1,50,000.00Cr"),
        ];
        let (txns, _, _) = extract_fw_transactions(&rows, "test.pdf")
            .expect("a minus-sign-separated date must still be extracted, not just survive without panicking");
        let t = txns.first().expect("one transaction");
        assert!(
            t.narration.to_uppercase().starts_with("SALARY"),
            "narration must start with the real text, not a leaked date fragment: {:?}",
            t.narration
        );
        assert!(
            !t.narration
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit()),
            "narration must not start with a stray digit leaked from the date: {:?}",
            t.narration
        );
    }

    #[test]
    fn fw_no_header_returns_none() {
        let rows = vec![
            row_of("01-01-2024 SALARY 50000.00 150000.00Cr"),
            row_of("02-01-2024 ATM WDL 10000.00 140000.00Cr"),
        ];
        // No header row containing "date"+"balance"+"withdrawal/deposit" or "amount"
        // → should return None (no header found)
        let result = extract_fw_transactions(&rows, "test.pdf");
        // Either None (good) or Some (if the simple rows accidentally satisfy the check)
        // In this case there's no valid header → None
        assert!(result.is_none(), "no header → None");
    }

    // ── extract_cosmos_transactions ───────────────────────────────────────────

    fn cosmos_rows() -> Vec<Vec<PdfItem>> {
        vec![
            row_of("COSMOS CO-OPERATIVE BANK"),
            row_of("Account Statement"),
            row_of("Date     Particulars          Chq.No. Withdrawals Deposits Balance"),
            row_of("01-01-2024 SALARY CREDIT UPI-CR/305561 123456  50000.00 1,50,000.00Cr"),
            row_of("02-01-2024 ATM WDL COSMOS ATM                   10000.00 1,40,000.00Cr"),
            row_of("03-01-2024 NEFT CR RAJESH SHAH 234567           25000.00 1,65,000.00Cr"),
        ]
    }

    #[test]
    fn cosmos_detects_header() {
        let rows = cosmos_rows();
        let result = extract_cosmos_transactions(&rows, "cosmos.pdf");
        assert!(result.is_some(), "Cosmos header detected");
    }

    #[test]
    fn cosmos_extracts_transactions() {
        let rows = cosmos_rows();
        let result = extract_cosmos_transactions(&rows, "cosmos.pdf").unwrap();
        // Transactions + synthetic OB row
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert!(!real.is_empty(), "real transactions extracted");
    }

    #[test]
    fn cosmos_direction_by_balance_movement() {
        // Three transactions with known balances: credit, debit, credit
        // Balance: 100000 → 150000 (credit 50000) → 140000 (debit 10000) → 165000 (credit 25000)
        let rows = vec![
            row_of("Date     Particulars          Chq.No. Withdrawals Deposits Balance"),
            row_of("01-01-2024 OPENING BAL                                       1,00,000.00Cr"),
            row_of("02-01-2024 SALARY CREDIT                            50000.00 1,50,000.00Cr"),
            row_of("03-01-2024 ATM WDL                        10000.00           1,40,000.00Cr"),
            row_of("04-01-2024 NEFT FROM RAJESH                         25000.00 1,65,000.00Cr"),
        ];
        let result = extract_cosmos_transactions(&rows, "cosmos.pdf").unwrap();
        let real: Vec<_> = result
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        assert!(real.len() >= 2, "at least 2 real transactions");
        // First transaction: salary credit (balance went from 100000 to 150000 = UP → credit)
        let salary = real
            .iter()
            .find(|t| t.narration.to_lowercase().contains("salary"));
        if let Some(t) = salary {
            assert!(t.credit.is_some(), "salary → credit (balance moved up)");
        }
    }

    #[test]
    fn cosmos_no_header_returns_none() {
        let rows = vec![row_of("01-01-2024 SALARY 50000.00 150000.00Cr")];
        assert!(extract_cosmos_transactions(&rows, "x.pdf").is_none());
    }

    // ── extract_icici_wealth_ref ───────────────────────────────────────────────

    #[test]
    fn icici_wealth_ref_slash_delimited_upi_utr() {
        let (narr, reference) =
            extract_icici_wealth_ref("UPI/rahulchauhan393/UPI/BANK OF BARODA/514917447583/ICIf858c7685fb3");
        assert_eq!(reference, "514917447583");
        assert!(!narr.contains("514917447583"));
        assert!(narr.contains("BANK OF BARODA"));
    }

    #[test]
    fn icici_wealth_ref_neft_hyphen_delimited_falls_back_to_generic_digit_run() {
        let (_, reference) =
            extract_icici_wealth_ref("NEFT-KKBKN62025040723714936-KARAN NANDKISHOAR-PAYMENT-1411495898");
        // No slash present, so the generic 9+ digit boundary-bounded fallback
        // applies — it finds the trailing standalone digit run.
        assert_eq!(reference, "1411495898");
    }

    #[test]
    fn icici_wealth_ref_no_recognizable_pattern_stays_empty() {
        let (narr, reference) = extract_icici_wealth_ref("CLG/BHARAT AGARWAL NAGARI");
        assert_eq!(reference, "");
        assert_eq!(narr, "CLG/BHARAT AGARWAL NAGARI");
    }

    // ── extract_icici_wealth_transactions ─────────────────────────────────────
    // Synthetic rows in the same coordinate shape `ocr_extractor::
    // extract_pages_via_ocr` produces: one `PdfItem` per OCR word, at real
    // (approximate) column x-positions — not single-item fused-text rows
    // like the Cosmos tests above, since this extractor's column detection
    // depends on per-word X position, and its block-merge logic depends on
    // amounts/dates being able to land on different physical rows within
    // one transaction.

    fn wm_item(text: &str, x: f64) -> PdfItem {
        PdfItem {
            x,
            text: text.to_owned(),
            w: (text.len() as f64) * 6.0,
        }
    }

    /// Column x's approximate the real fixture's (pixel-to-point-scaled)
    /// positions: Date=30, Mode**=77 (present in the header only, to
    /// reproduce the real fence gap — never a real ColField), Particulars
    /// starts ~160, Deposits~390, Withdrawals~445, Balance~547.
    fn wm_header_and_gate_rows() -> Vec<Vec<PdfItem>> {
        vec![
            row_of("ICICI Bank Wealth Management"),
            vec![
                wm_item("DATE", 30.0),
                wm_item("MODE**", 77.0),
                wm_item("PARTICULARS", 160.0),
                wm_item("DEPOSITS", 390.0),
                wm_item("WITHDRAWALS", 445.0),
                wm_item("BALANCE", 547.0),
            ],
        ]
    }

    #[test]
    fn icici_wealth_gated_on_signature_phrase() {
        let rows = vec![row_of("ICICI Bank"), row_of("01-04-2025 SALARY 50000.00 150000.00")];
        assert!(
            extract_icici_wealth_transactions(&rows, "x.pdf").is_none(),
            "a normal ICICI Bank statement (no 'wealth management'/'mode**' signal) must not be routed here"
        );
    }

    #[test]
    fn icici_wealth_extracts_bank_account_and_opening_balance() {
        let mut rows = wm_header_and_gate_rows();
        rows.insert(0, row_of("Savings A/c 059501505351"));
        rows.push(vec![wm_item("01-04-2025", 30.0), wm_item("B/F", 160.0), wm_item("1,030.97", 547.0)]);
        rows.push(vec![
            wm_item("05-04-2025", 30.0),
            wm_item("UPI/zee5/YES", 160.0),
            wm_item("199.00", 445.0),
            wm_item("831.97", 547.0),
        ]);
        rows.push(vec![
            wm_item("07-04-2025", 30.0),
            wm_item("BIL/INFT/EDB07/BUILD", 160.0),
            wm_item("50000.00", 390.0),
            wm_item("50831.97", 547.0),
        ]);

        let result = extract_icici_wealth_transactions(&rows, "icici_wm.pdf")
            .expect("must detect the ICICI Wealth Management layout");
        assert_eq!(result.bank_name, "ICICI Bank Wealth Management");
        assert_eq!(result.account_no, "059501505351");
        assert_eq!(result.opening_balance, Some(1030.97));

        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        assert_eq!(real.len(), 2);
        assert_eq!(real[0].date, "05/04/2025");
        assert_eq!(real[0].debit, Some(199.0));
        assert_eq!(real[0].credit, None);
        assert_eq!(real[0].balance, Some(831.97));
        assert_eq!(real[1].credit, Some(50000.0));
        assert_eq!(real[1].debit, None);
    }

    #[test]
    fn icici_wealth_merges_multi_line_particulars_block() {
        // The amount/balance land on a *different* physical row than the
        // date — the exact shape this extractor exists to handle.
        let mut rows = wm_header_and_gate_rows();
        rows.push(vec![wm_item("01-04-2025", 30.0), wm_item("B/F", 160.0), wm_item("1,030.97", 547.0)]);
        rows.push(vec![wm_item("05-04-2025", 30.0), wm_item("UPI/foo/BANK", 160.0)]);
        rows.push(vec![wm_item("OF/514917447583/hash", 160.0), wm_item("199.00", 445.0), wm_item("831.97", 547.0)]);
        // A second transaction — `extract_icici_wealth_transactions` requires
        // ≥ 2 real transactions to treat the layout as a confirmed match
        // (same anti-false-positive guard `extract_cosmos_transactions` uses).
        rows.push(vec![wm_item("07-04-2025", 30.0), wm_item("BIL/INFT/EDB07", 160.0), wm_item("50000.00", 390.0), wm_item("50831.97", 547.0)]);

        let result = extract_icici_wealth_transactions(&rows, "icici_wm.pdf").unwrap();
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        assert_eq!(real.len(), 2);
        assert_eq!(real[0].date, "05/04/2025");
        assert_eq!(real[0].debit, Some(199.0));
        assert_eq!(real[0].balance, Some(831.97));
        assert_eq!(real[0].reference, "514917447583");
        assert!(real[0].narration.contains("UPI/foo/BANK"));
        assert!(real[0].narration.contains("hash"));
    }

    #[test]
    fn icici_wealth_never_leaves_both_debit_and_credit_set() {
        // A block that (as OCR sometimes does) picks up a spurious value on
        // both sides — balance moved up by exactly the deposit amount, so
        // the withdrawal must be dropped as the artifact, never kept
        // alongside it.
        let mut rows = wm_header_and_gate_rows();
        rows.push(vec![wm_item("01-04-2025", 30.0), wm_item("B/F", 160.0), wm_item("1,000.00", 547.0)]);
        rows.push(vec![
            wm_item("05-04-2025", 30.0),
            wm_item("UPI/real/txn", 160.0),
            wm_item("5000.00", 390.0), // real deposit
            wm_item("1.00", 445.0),    // spurious withdrawal artifact
            wm_item("6000.00", 547.0), // balance: 1000 + 5000 = 6000, confirms deposit is real
        ]);
        // A second transaction — `extract_icici_wealth_transactions` requires
        // ≥ 2 real transactions to treat the layout as a confirmed match
        // (same anti-false-positive guard `extract_cosmos_transactions` uses).
        rows.push(vec![
            wm_item("07-04-2025", 30.0),
            wm_item("BIL/INFT/EDB07", 160.0),
            wm_item("500.00", 445.0),
            wm_item("5500.00", 547.0),
        ]);

        let result = extract_icici_wealth_transactions(&rows, "icici_wm.pdf").unwrap();
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        assert_eq!(real.len(), 2);
        assert_eq!(real[0].credit, Some(5000.0));
        assert_eq!(real[0].debit, None);
    }

    // ── extract_icici_normal_transactions ─────────────────────────────────────
    //
    // Synthetic reproductions of the real "ICICI Bank.pdf" fixture's OCR
    // word-box shape: a header that itself wraps across two physical rows
    // (`icici_normal_header_rows`), Sl-No-anchored transaction blocks, and
    // the two real OCR artifacts this extractor was built to survive — a
    // border-line glyph glued onto an amount ("3,000.00}") and an amount
    // split across two physical rows ("1,58,266." / "65").

    /// Column x's approximate the real fixture's OCR word-box positions:
    /// Sl No=91, Tran Id=110, Value Date=143/146, Transaction Date=175/189,
    /// Transaction Posted Date=224/234, Cheque no/Ref No=274-314,
    /// Transaction Remarks=335/341, Withdrawal (Dr)=397/414,
    /// Deposit (Cr)=436/444, Balance=473.
    fn icici_normal_header_rows() -> Vec<Vec<PdfItem>> {
        vec![
            vec![
                wm_item("SI", 95.0),
                wm_item("Tran", 113.0),
                wm_item("Value", 143.0),
                wm_item("Transaction", 175.0),
                wm_item("Transaction", 224.0),
                wm_item("Cheque", 274.0),
                wm_item("no", 304.0),
                wm_item("/", 314.0),
                wm_item("Transaction", 335.0),
                wm_item("Withdra", 397.0),
                wm_item("Deposit", 436.0),
                wm_item("Balance", 473.0),
            ],
            vec![
                wm_item("No", 93.0),
                wm_item("Id", 118.0),
                wm_item("Date", 146.0),
                wm_item("Date", 189.0),
                wm_item("Posted", 234.0),
                wm_item("Ref", 283.0),
                wm_item("No", 298.0),
                wm_item("Remarks", 341.0),
                wm_item("wal", 397.0),
                wm_item("(Dr)", 414.0),
                wm_item("(Cr)", 444.0),
            ],
        ]
    }

    #[test]
    fn icici_normal_gated_on_header_keywords_and_real_ocr_x_positions() {
        // Wrong bank entirely.
        let rows = vec![row_of("Kotak Mahindra Bank"), row_of("01/04/2025 SALARY 50000.00 150000.00")];
        assert!(extract_icici_normal_transactions(&rows, "x.pdf").is_none());

        // Right keywords, but Stage 1's flat embedded-text shape (every item
        // at X=0, see `text_extractor::extract_pages`) — must never fire
        // there; this layout is only recoverable via OCR word-boxes.
        let flat_rows = vec![
            row_of("SI No Tran Id Value Date Transaction Date Cheque no/Ref No Transaction Remarks Withdrawal(Dr) Deposit(Cr) Balance"),
            row_of("1 S8592 03/Apr/2025 03/04/2025 MMT/IMPS/5093117 3,000.00 26,856.92"),
        ];
        assert!(
            extract_icici_normal_transactions(&flat_rows, "x.pdf").is_none(),
            "flat X=0 rows (Stage 1) must never match — only real OCR word-boxes carry the distinct X positions this extractor requires"
        );
    }

    #[test]
    fn icici_normal_extracts_debit_and_credit_with_correct_bank_name() {
        let mut rows = icici_normal_header_rows();
        rows.push(vec![
            wm_item("1", 91.0),
            wm_item("S85926893", 110.0),
            wm_item("03/Apr/2025", 175.0),
            wm_item("03/04/2025", 224.0),
            wm_item("MMT/IMPS/5093117", 323.0),
            wm_item("3,000.00", 439.0),
            wm_item("26,856.92", 472.0),
        ]);
        rows.push(vec![
            wm_item("2", 91.0),
            wm_item("S6593608", 110.0),
            wm_item("23/Apr/2025", 175.0),
            wm_item("23/04/2025", 224.0),
            wm_item("NMQAB Chg", 323.0),
            wm_item("1,180.00", 402.0),
            wm_item("25,676.92", 472.0),
        ]);

        let result = extract_icici_normal_transactions(&rows, "icici.pdf").expect("must match");
        assert_eq!(result.bank_name, "ICICI Bank");
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        assert_eq!(real.len(), 2);
        assert_eq!(real[0].date, "03/04/2025");
        assert_eq!(real[0].credit, Some(3000.0));
        assert_eq!(real[0].debit, None);
        assert_eq!(real[0].balance, Some(26856.92));
        assert_eq!(real[1].debit, Some(1180.0));
        assert_eq!(real[1].credit, None);
    }

    #[test]
    fn icici_normal_strips_ocr_border_glyph_junk_from_amounts() {
        // Real bug: OCR consistently glues a mis-read cell-border glyph onto
        // the Deposit column's amount ("3,000.00}", "9,000.00}" in the real
        // fixture) — without stripping it, `parse_amount_str` fails on the
        // trailing junk and the whole transaction silently vanishes.
        let mut rows = icici_normal_header_rows();
        rows.push(vec![
            wm_item("1", 91.0),
            wm_item("03/04/2025", 224.0),
            wm_item("MMT/IMPS/5093117", 323.0),
            wm_item("3,000.00}", 439.0), // border glyph glued on
            wm_item("26,856.92", 472.0),
        ]);
        rows.push(vec![
            wm_item("2", 91.0),
            wm_item("23/04/2025", 224.0),
            wm_item("NMQAB Chg", 323.0),
            wm_item("1,180.00", 402.0),
            wm_item("25,676.92", 472.0),
        ]);

        let result = extract_icici_normal_transactions(&rows, "icici.pdf").expect("must match");
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        assert_eq!(real.len(), 2, "the junk-suffixed deposit row must not be silently dropped");
        assert_eq!(real[0].credit, Some(3000.0));
    }

    #[test]
    fn icici_normal_merges_cross_row_split_amount_by_dangling_order() {
        // Real bug: OCR occasionally splits ONE amount's trailing digits
        // onto the next physical row ("1,58,266." then "65"), and this
        // fixture has a block where the row *completing* the split has TWO
        // continuation fragments at once — one for Deposit, one for
        // Balance ("00" and "42") — landing at X positions that are each
        // individually *closer* to the wrong column by raw distance. Only
        // pairing dangling columns to continuation fragments by matching
        // left-to-right order (not nearest X) recovers both correctly.
        let mut rows = icici_normal_header_rows();
        rows.push(vec![
            wm_item("1", 91.0),
            wm_item("03/04/2025", 224.0),
            wm_item("Opening", 323.0),
            wm_item("1,000.00", 439.0),
            wm_item("26,000.00", 472.0),
        ]);
        rows.push(vec![
            wm_item("2", 91.0),
            wm_item("14/05/2025", 224.0),
            wm_item("CLG/BHASWATI", 323.0),
            wm_item("1,50,000.|", 438.0), // Deposit, split — dangling
            wm_item("1,75,352.", 475.0),  // Balance, split — also dangling
        ]);
        rows.push(vec![
            wm_item("00", 459.0), // completes Deposit — closer to Balance's anchor by raw X!
            wm_item("42", 497.0), // completes Balance
        ]);
        rows.push(vec![
            wm_item("3", 91.0),
            wm_item("15/05/2025", 224.0),
            wm_item("VIN/TLR", 323.0),
            wm_item("1,58,266.65", 402.0),
            wm_item("17,085.77", 472.0),
        ]);

        let result = extract_icici_normal_transactions(&rows, "icici.pdf").expect("must match");
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        let split = real.iter().find(|t| t.date == "14/05/2025").expect("split-amount transaction");
        assert_eq!(split.credit, Some(150000.0), "Deposit continuation must attach to Deposit, not Balance");
        assert_eq!(split.balance, Some(175352.42), "Balance continuation must attach to Balance, not Deposit");
    }

    #[test]
    fn icici_normal_never_leaves_both_debit_and_credit_set() {
        let mut rows = icici_normal_header_rows();
        rows.push(vec![
            wm_item("1", 91.0),
            wm_item("01/04/2025", 224.0),
            wm_item("B/F", 323.0),
            wm_item("1,000.00", 472.0),
        ]);
        rows.push(vec![
            wm_item("2", 91.0),
            wm_item("05/04/2025", 224.0),
            wm_item("UPI/real/txn", 323.0),
            wm_item("5000.00", 439.0), // real deposit
            wm_item("1.00", 402.0),    // spurious withdrawal artifact
            wm_item("6000.00", 472.0), // balance: 1000 + 5000 = 6000, confirms deposit is real
        ]);
        rows.push(vec![
            wm_item("3", 91.0),
            wm_item("07/04/2025", 224.0),
            wm_item("BIL/INFT", 323.0),
            wm_item("500.00", 439.0),
            wm_item("6500.00", 472.0),
        ]);

        let result = extract_icici_normal_transactions(&rows, "icici.pdf").expect("must match");
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        let t = real.iter().find(|t| t.date == "05/04/2025").expect("target transaction");
        assert_eq!(t.credit, Some(5000.0));
        assert_eq!(t.debit, None);
    }

    // ── extract_kotak_narrow_transactions ─────────────────────────────────────
    // Synthetic reproductions of the real "Kotak Bank.pdf" fixture's exact
    // 8-line-per-transaction shape (Sl.No/Date/Time/ValueDate/Narration/
    // Ref/signed Amount/Balance) — the real bug this was written for is
    // covered end-to-end against the actual fixture PDF in
    // tests/import_pipeline.rs; these test the block-scanning algorithm
    // itself in isolation, including the shapes it must specifically not
    // misparse.

    fn rows_from(lines: &[&str]) -> Vec<Vec<PdfItem>> {
        lines.iter().map(|l| row_of(l)).collect()
    }

    // Every call site below passes string literals, so a `&'static str`
    // signature avoids any lifetime juggling — this only ever needs to
    // build a fixed test fixture, never a runtime-computed string.
    fn kotak_block(
        sl_no: &'static str,
        date: &'static str,
        narr: &'static str,
        refno: &'static str,
        signed: &'static str,
        bal: &'static str,
    ) -> Vec<&'static str> {
        vec![sl_no, date, "07:58 PM", date, narr, refno, signed, bal]
    }

    #[test]
    fn kotak_narrow_negative_amount_is_a_debit_positive_is_a_credit() {
        let mut lines: Vec<&str> = Vec::new();
        lines.extend(kotak_block(
            "1", "10 Apr 2024", "UPI/SOME MERCHANT/410166340136/UPI", "UPI-410108414656",
            "-805.50", "16,02,264.84",
        ));
        lines.extend(kotak_block(
            "2", "11 Apr 2024", "NEFT SOMECOMPANY HDFC0000001", "NEFTINW-0841873091",
            "+50,000.00", "16,52,264.84",
        ));
        lines.extend(kotak_block(
            "3", "12 Apr 2024", "UPI/OTHER MERCHANT/410233400260/UPI", "UPI-410248001456",
            "-18.00", "16,52,246.84",
        ));
        let rows = rows_from(&lines);
        let (txns, op_bal, cl_bal) =
            extract_kotak_narrow_transactions(&rows, "test.pdf").expect("must match the narrow layout");
        assert_eq!(op_bal, None);
        assert_eq!(cl_bal, None);
        assert_eq!(txns.len(), 3);

        assert_eq!(txns[0].debit, Some(805.50));
        assert_eq!(txns[0].credit, None);
        assert_eq!(txns[0].balance, Some(1602264.84));

        assert_eq!(txns[1].debit, None);
        assert_eq!(txns[1].credit, Some(50000.0));
        assert_eq!(txns[1].balance, Some(1652264.84));

        assert_eq!(txns[2].debit, Some(18.0));
        assert_eq!(txns[2].credit, None);

        // Never both, never neither.
        for t in &txns {
            assert!(!(t.debit.is_some() && t.credit.is_some()));
            assert!(t.debit.is_some() || t.credit.is_some());
        }
    }

    #[test]
    fn kotak_narrow_amount_without_comma_or_decimal_parses_correctly() {
        // Spec's own example shape: "+56238" / "-562389" — no thousands
        // comma, no decimal point at all.
        let mut lines: Vec<&str> = Vec::new();
        lines.extend(kotak_block("1", "10 Apr 2024", "UPI/A/1/UPI", "UPI-1", "+56238", "156238"));
        lines.extend(kotak_block("2", "11 Apr 2024", "UPI/B/2/UPI", "UPI-2", "-562389", "0"));
        lines.extend(kotak_block("3", "12 Apr 2024", "UPI/C/3/UPI", "UPI-3", "-1.00", "0.00"));
        let rows = rows_from(&lines);
        let (txns, _, _) =
            extract_kotak_narrow_transactions(&rows, "test.pdf").expect("must match the narrow layout");
        assert_eq!(txns[0].credit, Some(56238.0));
        assert_eq!(txns[0].debit, None);
        assert_eq!(txns[1].debit, Some(562389.0));
        assert_eq!(txns[1].credit, None);
    }

    #[test]
    fn kotak_narrow_survives_a_page_break_interruption_between_blocks() {
        // Reproduces the real fixture's page-break furniture exactly: a
        // statement-generated timestamp, a "Page N of" marker, the account
        // holder's name repeated, the statement period repeated, and a run
        // of undecodable "Identity-H Unimplemented" header-cell placeholders
        // — none of which match the 4-line Sl.No/Date/Time/ValueDate anchor,
        // so the scanner must skip past them one line at a time and resume
        // matching real blocks right after.
        let mut lines: Vec<&str> = Vec::new();
        lines.extend(kotak_block("1", "10 Apr 2024", "UPI/A/1/UPI", "UPI-1", "-100.00", "900.00"));
        lines.extend([
            "Statement generated on 03 Sep 2025, 11:09 AM",
            "Page 2 of",
            "SOME ACCOUNT HOLDER",
            "Account Statement 01 Apr 2024 - 31 Mar 2025",
            "?Identity-H Unimplemented?",
            "?Identity-H Unimplemented?",
        ]);
        lines.extend(kotak_block("2", "11 Apr 2024", "UPI/B/2/UPI", "UPI-2", "+200.00", "1100.00"));
        lines.extend(kotak_block("3", "12 Apr 2024", "UPI/C/3/UPI", "UPI-3", "-50.00", "1050.00"));
        let rows = rows_from(&lines);
        let (txns, _, _) =
            extract_kotak_narrow_transactions(&rows, "test.pdf").expect("must match the narrow layout");
        assert_eq!(txns.len(), 3, "must recover all three blocks around the page-break furniture");
        assert_eq!(txns[0].debit, Some(100.0));
        assert_eq!(txns[1].credit, Some(200.0));
        assert_eq!(txns[2].debit, Some(50.0));
    }

    #[test]
    fn kotak_narrow_amount_is_never_confused_with_balance() {
        // The unsigned Balance line must never itself be mistaken for a
        // transaction amount — only a line with an explicit leading sign
        // is ever treated as the amount.
        assert!(!is_kotak_signed_amount("16,02,264.84"));
        assert!(is_kotak_signed_amount("-805.50"));
        assert!(is_kotak_signed_amount("+50,000.00"));
        assert!(!is_kotak_signed_amount("15")); // Sl. No., not an amount at all
    }

    #[test]
    fn kotak_narrow_does_not_misfire_on_an_unrelated_document() {
        // A handful of small integers and dates that happen to appear near
        // each other, but with no real signed-amount anchor anywhere, must
        // not be mistaken for this layout (MIN_KOTAK_NARROW_TXNS guard).
        let rows = rows_from(&[
            "1", "01 Jan 2024", "Some unrelated document",
            "2", "02 Jan 2024", "with no transactions in it at all",
        ]);
        assert!(extract_kotak_narrow_transactions(&rows, "unrelated.pdf").is_none());
    }
}
