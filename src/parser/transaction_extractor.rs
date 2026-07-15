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
    ParseResult, Transaction,
    column_detector::PdfItem,
    date_parser::normalize_transaction_date,
    excel_parser::{compute_prev_balances, prepend_opening_balance_row},
    noise_filter::is_noise_row,
};
use crate::text_safety::floor_char_boundary;

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
    if s.len() < 10 { return None; }
    // First 10 chars: DD[-/]MM[-/]YYYY
    let bytes = s.as_bytes();
    let d1 = bytes[0].is_ascii_digit() && bytes[1].is_ascii_digit();
    let sep1 = bytes[2] == b'-' || bytes[2] == b'/' || bytes[2] == 0xE2; // em-dash is multi-byte
    let m1 = bytes[3].is_ascii_digit() && bytes[4].is_ascii_digit();
    if !d1 || !sep1 || !m1 { return None; }
    // Find separator positions allowing em-dash (3 bytes)
    let normalized = s.replace('\u{2212}', "-").replace('\u{2013}', "-");
    // `\u{2014}` (em-dash) isn't replaced above, so it — or any other
    // stray multi-byte character in this heuristically-detected date
    // region — can still be present here; `floor_char_boundary` keeps
    // this byte-10 cut from panicking on it (Phase 4L.2.2). Note this
    // means an em-dash-separated date already fails to match below (the
    // `.split` predicate only recognizes ASCII `-`/`/`, so an
    // un-replaced em-dash leaves `parts.len() < 3`) — safely rejected,
    // not silently mis-parsed.
    let cut = floor_char_boundary(&normalized, 10.min(normalized.len()));
    let parts: Vec<&str> = normalized[..cut].split(|c| c == '-' || c == '/').collect();
    if parts.len() < 3 { return None; }
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
    if at >= bytes.len() { return None; }
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
            if c.is_ascii_digit() || c == b',' { end += 1; } else { break; }
        }
        // Require decimal point + digits
        if end < s.len() && s.as_bytes()[end] == b'.' {
            end += 1;
            let dec_start = end;
            while end < s.len() && s.as_bytes()[end].is_ascii_digit() { end += 1; }
            if end - dec_start >= 1 && end - dec_start <= 2 {
                let raw = &s[digit_pos..end];
                let val = raw.replace(',', "").parse::<f64>().unwrap_or(0.0);
                if val > 0.0 { out.push((val, digit_pos, raw.to_string())); }
            }
        }
        start = end.max(digit_pos + 1);
    }
    out
}

/// Number of integer digits in a decimal string (commas stripped, before the dot).
fn int_digit_count(s: &str) -> usize {
    let s = s.replace(',', "");
    let s = if let Some(p) = s.find('.') { &s[..p] } else { &s };
    s.chars().filter(|c| c.is_ascii_digit()).count()
}

/// Extract the balance from the end of a line: "NNN.NNCr" or "NNN.NNDr".
fn extract_balance_suffix(line: &str) -> Option<(f64, usize, &str)> {
    let lower = line.to_lowercase();
    let suffix = if lower.trim_end().ends_with("cr") || lower.trim_end().ends_with("dr") {
        2
    } else { return None; };
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
    nl.contains("upi-dr") || nl.contains("upi dr")
    || (nl.contains("/dr/") || nl.contains("-dr-"))
    || nl.contains("neft") && nl.contains("dr")
    || nl.contains("rtgs") && nl.contains("dr")
    || nl.contains("cash wd") || nl.contains("atm wd")
    || nl.contains("by debit")
    || nl.contains("chg dr")
}

fn is_credit_narr(nl: &str) -> bool {
    // Matches JS: /upi[-\s]?cr[/\s]|[/-]cr[/-]|\bcr\b.*[/-]|neft.*cr\b|rtgs.*cr\b|by\s+cr\b|interest\b|refund|reversal|salary/
    nl.contains("upi-cr") || nl.contains("upi cr")
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
    let mut hdr_idx      = usize::MAX;
    let mut wd_pos: i32  = -1;   // char position of "withdrawal/debit" keyword
    let mut dep_pos: i32 = -1;   // char position of "deposit/credit" keyword
    let mut col_mid      = 0i32; // midpoint between withdrawal and deposit
    let mut single_amt   = false;

    for (i, row) in rows.iter().enumerate().take(30) {
        let line = row.first().map_or("", |it| it.text.as_str());
        let ll   = line.to_lowercase();
        if !ll.contains("date") || !ll.contains("balance") { continue; }

        if (ll.contains("withdrawal") || ll.contains("debit")) &&
           (ll.contains("deposit")    || ll.contains("credit")) {
            hdr_idx = i;
            // Find character positions of withdrawal/deposit keywords
            let wp = ll.find("withdrawal").or_else(|| ll.find("debit")).map(|p| p as i32).unwrap_or(-1);
            let dp = ll.find("deposit").or_else(|| ll.find("credit")).map(|p| p as i32).unwrap_or(-1);
            if wp >= 0 { wd_pos  = wp; }
            if dp >= 0 { dep_pos = dp; }
            break;
        }

        let single = ll.contains("amount") || ll.contains("amt") || ll.contains("txn amt")
                  || ll.contains("transaction amount");
        if single && !ll.contains("withdrawal") && !ll.contains("deposit") {
            hdr_idx     = i;
            single_amt  = true;
            break;
        }
    }
    if hdr_idx == usize::MAX { return None; }

    // Format A midpoint between withdrawal and deposit headers
    col_mid = if !single_amt {
        if wd_pos >= 0 && dep_pos >= 0 { (wd_pos + dep_pos) / 2 }
        else if dep_pos >= 0 { dep_pos }
        else { 70 }
    } else { 0 };

    // ── Transaction loop ──────────────────────────────────────────────────────
    let mut txns: Vec<Transaction> = Vec::new();
    let mut op_balance:      Option<f64> = None;
    let mut closing_balance: Option<f64> = None;
    let mut txn_counter = 0usize;

    for (i, row) in rows.iter().enumerate().skip(hdr_idx + 1) {
        let line = row.first().map_or("", |it| it.text.as_str());
        let line = line.trim();
        if line.is_empty() || line.chars().all(|c| c == '-' || c == '=' || c == ' ') { continue; }

        // Require a date at the start of the line
        let (date_str, date_orig_len) = match starts_with_date(line) {
            Some(d) => d,
            None    => continue,
        };
        let nd = normalize_transaction_date(&date_str);
        if !nd.valid { continue; }

        // Require balance suffix: "NNN.NNCr" or "NNN.NNDr"
        let (balance, bal_start, _bal_raw) = match extract_balance_suffix(line) {
            Some(b) => b,
            None    => continue,
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
        let after_date = if date_part_len < line.len() { &line[date_part_len..] } else { "" };
        let middle = if bal_start > date_part_len {
            &line[date_part_len..bal_start]
        } else { after_date };

        let ml = middle.to_lowercase();

        // Opening/closing balance markers
        if ml.contains("opening bal") || ml.contains("op bal") {
            op_balance = Some(balance); continue;
        }
        if ml.contains("closing bal") || ml.contains("cl bal") {
            closing_balance = Some(balance); continue;
        }

        let mut debit:     Option<f64> = None;
        let mut credit:    Option<f64> = None;
        let mut narration  = String::new();
        let mut reference  = String::new();

        if single_amt {
            // ── Format B (single amount column) ──────────────────────────────
            // All decimal amounts in `middle`; reject >10 int-digit UTR values.
            let real_amts: Vec<(f64, usize, String)> = extract_amounts(middle)
                .into_iter()
                .filter(|(_, _, raw)| int_digit_count(raw) <= 10)
                .collect();
            if real_amts.is_empty() { continue; }

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
                let is_ref_by_pos    = wd_pos >= 0 && abs_start < (wd_pos - 5);
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
                if abs_start < col_mid { debit  = Some(val); }
                else                   { credit = Some(val); }
            }
        }

        if debit.is_none() && credit.is_none() { continue; }

        txn_counter += 1;
        let mut t = Transaction::new(format!("t_fw_{}_{}", i, txn_counter));
        t.date      = nd.display;
        t.date_ts   = nd.ts;
        t.narration = narration;
        t.reference = reference;
        t.debit     = debit;
        t.credit    = credit;
        t.balance   = Some(balance);
        t.bank_name = file_name.to_string();
        txns.push(t);
    }

    if txns.is_empty() { return None; }

    // ── Balance-direction post-pass ───────────────────────────────────────────
    // Corrects Format B misclassifications (and catches Format A edge cases).
    {
        let mut prev_bal = op_balance;
        if prev_bal.is_none() {
            if let Some(seed) = txns.iter().find(|t| t.balance.is_some() && (t.debit.is_some() || t.credit.is_some())) {
                prev_bal = Some(((seed.balance.unwrap() - seed.credit.unwrap_or(0.0) + seed.debit.unwrap_or(0.0)) * 100.0).round() / 100.0);
            }
        }
        for t in &mut txns {
            let bal = match t.balance { Some(b) => b, None => continue };
            let prev = match prev_bal { Some(p) => p, None => { prev_bal = Some(bal); continue; } };
            let tol  = |amt: f64| (amt * 0.02_f64).max(1.0);

            if t.debit.is_some() && t.credit.is_none() {
                let diff = bal - prev;
                let amt  = t.debit.unwrap();
                if (diff - amt).abs() < tol(amt) {
                    t.credit = Some(amt); t.debit = None; // balance went UP → credit
                }
            } else if t.credit.is_some() && t.debit.is_none() {
                let diff = bal - prev;
                let amt  = t.credit.unwrap();
                if (diff + amt).abs() < tol(amt) {
                    t.debit = Some(amt); t.credit = None; // balance went DOWN → debit
                }
            }
            prev_bal = Some(bal);
        }
    }

    Some((txns, op_balance, closing_balance))
}

/// Extract a 9+ digit reference number embedded in a narration
/// (sequences that appear between slashes or at segment start/end).
fn extract_ref_from_narration(narr: &str) -> Option<String> {
    // Look for 9+ consecutive digit sequences bounded by /, -, space, or string start/end
    let mut i = 0;
    let bytes = narr.as_bytes();
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
            let len = i - start;
            if len >= 9 {
                // Check boundaries: must be preceded/followed by /, -, space, or string boundary
                let pre_ok  = start == 0 || matches!(bytes[start-1], b'/' | b'-' | b' ');
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
pub fn extract_cosmos_transactions(
    rows: &[Vec<PdfItem>],
    file_name: &str,
) -> Option<ParseResult> {

    // ── Step 1: locate Cosmos header ─────────────────────────────────────────
    let mut hdr_idx = usize::MAX;
    for (i, row) in rows.iter().enumerate().take(50) {
        let line = row.iter().map(|it| it.text.as_str()).collect::<Vec<_>>().join(" ");
        let ll   = line.to_lowercase();
        if ll.contains("date") && ll.contains("particulars") &&
           ll.contains("chq")  && ll.contains("withdrawal") &&
           ll.contains("deposit") && ll.contains("balance") {
            hdr_idx = i;
            break;
        }
    }
    if hdr_idx == usize::MAX {
        log::debug!("[BSP Cosmos] No Cosmos header in \"{}\" — skipping", file_name);
        return None;
    }

    // ── Step 2: parse transaction rows ────────────────────────────────────────
    // Structure holds txnVal temporarily until direction is resolved in Step 3.
    struct Pending { t: Transaction, txn_val: f64 }

    let mut pending:         Vec<Pending>  = Vec::new();
    let mut op_balance:      Option<f64>   = None;
    let mut closing_balance: Option<f64>   = None;
    let mut txn_counter = 0usize;

    for (i, row) in rows.iter().enumerate().skip(hdr_idx + 1) {
        let line = row.iter().map(|it| it.text.as_str()).collect::<Vec<_>>().join(" ").trim().to_string();
        if line.is_empty() || line.chars().all(|c| c == '-' || c == '=' || c == ' ') { continue; }

        // Date at start
        let (date_str, date_orig_len) = match starts_with_date(&line) {
            Some(d) => d,
            None    => continue,
        };
        let nd = normalize_transaction_date(&date_str);
        if !nd.valid { continue; }

        // Balance at end: "NNN,NNN.NNCr" or "Dr"
        let (balance, bal_raw_start, _) = match extract_balance_suffix(&line) {
            Some(b) => b,
            None    => continue,
        };

        // See the equivalent comment in extract_fw_transactions above —
        // `date_orig_len` is the date pattern's real byte length in
        // `line` (Phase 4L.2.2 follow-up); `date_str.len()` alone would
        // undershoot on a typographic-dash date and leak trailing date
        // bytes into `middle`, not just risk an unsafe slice.
        let date_part_len = floor_char_boundary(&line, date_orig_len + 1);
        let middle = if bal_raw_start > date_part_len {
            line[date_part_len..bal_raw_start].trim().to_string()
        } else {
            String::new()
        };
        let ml = middle.to_lowercase();

        if ml.contains("opening bal") || ml.contains("op bal") {
            op_balance = Some(balance); continue;
        }
        if ml.contains("closing bal") || ml.contains("cl bal") {
            closing_balance = Some(balance); continue;
        }
        if is_noise_row(&middle) { continue; }

        // ── Amount extraction: rightmost valid decimal (≤ 10 int digits) ──────
        let amt_candidates: Vec<(f64, usize, String)> = extract_amounts(&middle)
            .into_iter()
            .filter(|(_, _, raw)| int_digit_count(raw) <= 10)
            .collect();
        if amt_candidates.is_empty() { continue; }

        let (txn_val, txn_idx, _) = amt_candidates.last().unwrap().clone();
        let text_part = middle[..txn_idx].trim().to_string();

        // Chq.No. = trailing 4–7 digit integer in textPart
        let (narration, reference) = extract_cosmos_ref(&text_part);
        if narration.is_empty() { continue; }

        txn_counter += 1;
        let mut t = Transaction::new(format!("t_cosmos_{}_{}", i, txn_counter));
        t.date      = nd.display;
        t.date_ts   = nd.ts;
        t.narration = narration;
        t.reference = reference;
        t.balance   = Some(balance);
        t.bank_name = "Cosmos Co-operative Bank".to_string();
        // debit/credit resolved in Step 3
        pending.push(Pending { t, txn_val });
    }

    if pending.len() < 2 { return None; }

    // ── Step 3: determine debit / credit from balance movement ────────────────
    let is_cosmos_credit = |nl: &str| -> bool {
        nl.contains("upi-cr") || nl.contains("prcr/") || nl.contains("upi cr")
        || (nl.contains("neft") && nl.contains("cr"))
        || nl.contains("^by ") || nl.starts_with("by/") || nl.starts_with("by ")
        || nl.contains("imps/p2a") || nl.contains("upi-rd")
        || nl.contains("salary") || nl.contains("refund") || nl.contains("interest")
        || nl.contains("reversal") || nl.contains("deposit")
    };
    let is_cosmos_debit = |nl: &str| -> bool {
        nl.contains("upi-dr") || nl.contains("upi dr")
        || (nl.contains("neft") && nl.contains("dr"))
        || nl.contains("atm") || nl.contains("cwdr")
        || nl.contains("cash w/d") || nl.contains("cash w-d")
        || nl.contains("withdrawal") || nl.contains("payment to")
    };

    let mut prev_bal = op_balance;

    // Seed prevBal when opening balance unknown
    if prev_bal.is_none() {
        let seed = &pending[0];
        let nl = seed.t.narration.to_lowercase();
        if is_cosmos_credit(&nl) {
            prev_bal = Some((seed.t.balance.unwrap() - seed.txn_val) * 100.0 / 100.0);
            prev_bal = prev_bal.map(|v| (v * 100.0).round() / 100.0);
        } else if is_cosmos_debit(&nl) {
            prev_bal = Some(((seed.t.balance.unwrap() + seed.txn_val) * 100.0).round() / 100.0);
        }
    }

    let mut resolved: Vec<Transaction> = Vec::new();

    for mut p in pending {
        let bal = p.t.balance.unwrap();
        let tv  = p.txn_val;

        if prev_bal.is_none() {
            // Anchor: assign by narration keyword
            let nl = p.t.narration.to_lowercase();
            if is_cosmos_credit(&nl)      { p.t.credit = Some(tv); }
            else if is_cosmos_debit(&nl)  { p.t.debit  = Some(tv); }
            else                           { p.t.debit  = Some(tv); }
            prev_bal = Some(bal);
            resolved.push(p.t);
            continue;
        }

        let diff = ((bal - prev_bal.unwrap()) * 100.0).round() / 100.0;
        let tol  = (tv * 0.001_f64).max(0.02);

        if (diff - tv).abs() <= tol {
            p.t.credit = Some(tv);          // balance UP → Deposits / credit
        } else if (diff + tv).abs() <= tol {
            p.t.debit  = Some(tv);          // balance DOWN → Withdrawals / debit
        } else {
            // Reconciliation miss → narration keyword fallback
            let nl = p.t.narration.to_lowercase();
            if is_cosmos_credit(&nl)      { p.t.credit = Some(tv); }
            else if is_cosmos_debit(&nl)  { p.t.debit  = Some(tv); }
            else                           { p.t.debit  = Some(tv); }
        }

        prev_bal = Some(bal);
        resolved.push(p.t);
    }

    let valid: Vec<Transaction> = resolved.into_iter()
        .filter(|t| t.debit.is_some() || t.credit.is_some())
        .collect();
    if valid.len() < 2 { return None; }

    let mut txns = valid;
    let op_balance = compute_prev_balances(&mut txns, op_balance);
    prepend_opening_balance_row(&mut txns, op_balance, "Cosmos Co-operative Bank", "");

    Some(ParseResult {
        transactions:       txns,
        opening_balance:    op_balance,
        closing_balance,
        bank_name:          "Cosmos Co-operative Bank".to_string(),
        account_no:         String::new(),
        source_name:        file_name.to_string(),
        col_map:            Default::default(),
        header_row_idx:     hdr_idx,
        noise_row_count:    0,
        rejected_row_count: 0,
    })
}

/// Extract Cosmos narration and reference from the text portion before the txn amount.
/// Chq.No. = trailing 4–7 digit integer (longer runs stay as narration text).
fn extract_cosmos_ref(text_part: &str) -> (String, String) {
    // Find trailing 4-7 digit integer separated by whitespace
    let re_end: Option<(usize, &str)> = {
        let words: Vec<&str> = text_part.split_whitespace().collect();
        if let Some(last) = words.last() {
            if last.len() >= 4 && last.len() <= 7 && last.chars().all(|c| c.is_ascii_digit()) {
                let trailing_start = text_part.rfind(last).unwrap();
                Some((trailing_start, last))
            } else { None }
        } else { None }
    };

    if let Some((pos, chq)) = re_end {
        let narr = text_part[..pos].trim().to_string();
        let narr = if narr.is_empty() { text_part.to_string() } else { narr };
        (narr, chq.to_string())
    } else {
        (text_part.trim().to_string(), String::new())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pdf_item(text: &str) -> PdfItem {
        PdfItem { x: 10.0, text: text.to_owned(), w: 400.0 }
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
        let (val, _, _) = extract_balance_suffix("01/01/2024 SALARY 50000.00 1,50,000.00Cr").unwrap();
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
    fn cosmos_long_digits_stay_in_narration() {
        // 12+ digit UTR stays in narration — not extracted as reference
        let (narr, chq) = extract_cosmos_ref("UPI-DR/305561534108/AMAZON");
        assert!(chq.is_empty() || chq.len() <= 7, "long digit run should not be chq ref");
        let _ = narr; // consumed to suppress warning
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
        let salary = txns.iter().find(|t| t.narration.to_lowercase().contains("salary")
            || t.narration.to_lowercase().contains("cr")).expect("UPI-CR row");
        assert!(salary.credit.is_some(), "UPI-CR row should be credit");
    }

    #[test]
    fn fw_format_b_upi_dr_is_debit() {
        let rows = fw_format_b_rows();
        let (txns, _, _) = extract_fw_transactions(&rows, "test.pdf").unwrap();
        let atm = txns.iter().find(|t| t.narration.to_lowercase().contains("atm")
            || t.narration.to_lowercase().contains("upi-dr")).expect("UPI-DR row");
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
                txns.iter().any(|t| t.narration.to_lowercase().contains("atm")),
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
            !t.narration.chars().next().is_some_and(|c| c.is_ascii_digit()),
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
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
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
        let real: Vec<_> = result.transactions.iter().filter(|t| !t.is_opening_balance).collect();
        assert!(real.len() >= 2, "at least 2 real transactions");
        // First transaction: salary credit (balance went from 100000 to 150000 = UP → credit)
        let salary = real.iter().find(|t| t.narration.to_lowercase().contains("salary"));
        if let Some(t) = salary {
            assert!(t.credit.is_some(), "salary → credit (balance moved up)");
        }
    }

    #[test]
    fn cosmos_no_header_returns_none() {
        let rows = vec![
            row_of("01-01-2024 SALARY 50000.00 150000.00Cr"),
        ];
        assert!(extract_cosmos_transactions(&rows, "x.pdf").is_none());
    }
}
