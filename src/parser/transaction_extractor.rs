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
fn extract_ref_from_narration(narr: &str) -> Option<String> {
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
