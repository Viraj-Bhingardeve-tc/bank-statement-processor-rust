//! reconciliation.rs — Port of the Electron `ReconciliationEngine`
//! (`src/engines/reconciliation.js`): tiered-confidence bank-vs-Tally-voucher
//! matching via greedy bipartite assignment.
//!
//! Scoring (per `_scoreMatch`):
//!   - Hard filter: amount must be within `2 * amount_fuzzy_pct` or the pair
//!     scores 0 outright.
//!   - Amount closeness: +0.40 exact, +0.30 within tolerance, +0.10 loose.
//!   - Date closeness: +0.40 same day, +0.30 ≤1 day, +0.20 ≤date_fuzzy_days,
//!     +0.05 ≤2×date_fuzzy_days, else reject (score 0).
//!   - Narration similarity (Jaccard over 4+ char tokens): +0.20 if ≥0.80,
//!     else +narr*0.15 if ≥ narr_similarity_min.
//!   - Reference/voucher-number exact match: +0.20 bonus.
//!
//! Status thresholds (per `_statusFromScore`): ≥0.90 Matched, ≥0.40 Likely,
//! >0 Possible, else Unmatched.
//!
//! Matching (per `_reconcile`): every (bank, voucher) pair with score > 0 is a
//! candidate; candidates are sorted best-score-first and greedily assigned,
//! so a voucher or bank transaction can only be claimed once — by its single
//! best available counterpart, not just the first one encountered.

use std::collections::HashSet;

use crate::parser::date_parser::normalize_transaction_date;

// ── Data types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchStatus {
    Matched,
    Likely,
    Possible,
    Unmatched,
}

impl MatchStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            MatchStatus::Matched   => "Matched",
            MatchStatus::Likely    => "Likely",
            MatchStatus::Possible  => "Possible",
            MatchStatus::Unmatched => "Unmatched",
        }
    }
}

/// One row from a Tally daybook export (or any external ledger export).
#[derive(Debug, Clone, Default)]
pub struct Voucher {
    pub date:         String,
    pub amount:       f64,
    pub narration:    String,
    pub voucher_no:   String,
    pub voucher_type: String,
    pub ledger:       String,
}

/// One bank transaction, reduced to just what reconciliation needs.
#[derive(Debug, Clone, Default)]
pub struct BankEntry {
    pub date:      String,
    pub amount:    f64,
    pub narration: String,
    pub reference: String,
}

/// Tunable matching tolerances. `date_fuzzy_days`/`amount_fuzzy_pct` come from
/// the Settings screen (`Settings.recon_days`/`recon_pct`); the other three
/// mirror the old app's own hardcoded `_cfg()` fallback defaults — neither app
/// exposes them in its Settings UI.
#[derive(Debug, Clone)]
pub struct ReconConfig {
    pub date_fuzzy_days:       i64,
    pub amount_fuzzy_pct:      f64,
    pub narr_similarity_min:   f64,
    pub auto_accept_above:     f64,
    pub flag_below_confidence: f64,
}

impl ReconConfig {
    pub fn new(date_fuzzy_days: i64, amount_fuzzy_pct: f64) -> Self {
        ReconConfig {
            date_fuzzy_days,
            amount_fuzzy_pct,
            narr_similarity_min:   0.55,
            auto_accept_above:     0.90,
            flag_below_confidence: 0.40,
        }
    }
}

impl Default for ReconConfig {
    fn default() -> Self { ReconConfig::new(3, 0.5) }
}

#[derive(Debug, Clone)]
pub struct MatchPair {
    pub bank_idx:    usize,
    pub voucher_idx: usize,
    pub score:       f64,
    pub status:      MatchStatus,
}

#[derive(Debug, Clone, Default)]
pub struct ReconReport {
    pub matches:            Vec<MatchPair>,
    pub unmatched_bank:     Vec<usize>,
    pub unmatched_vouchers: Vec<usize>,
}

impl ReconReport {
    pub fn matched_count(&self)  -> usize { self.matches.iter().filter(|m| m.status == MatchStatus::Matched).count() }
    pub fn likely_count(&self)   -> usize { self.matches.iter().filter(|m| m.status == MatchStatus::Likely).count() }
    pub fn possible_count(&self) -> usize { self.matches.iter().filter(|m| m.status == MatchStatus::Possible).count() }
}

// ── Narration similarity ─────────────────────────────────────────────────────

/// Tokenize for narration-similarity scoring — port of `_tokenize`: uppercase,
/// replace everything but letters/digits with spaces, split on whitespace,
/// keep only tokens of 4+ characters. (JS's extra `!/^\d{1,3}$/` check on the
/// same tokens is a no-op given the length>=4 guard already excludes every
/// string that a 1-3-digit regex could ever match, so it's intentionally not
/// carried over as a separate condition here — same net behavior.)
fn tokenize(s: &str) -> HashSet<String> {
    s.to_uppercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|t| t.chars().count() >= 4)
        .map(|t| t.to_string())
        .collect()
}

/// Jaccard similarity of two narrations' token sets — port of `_jaccard`.
pub fn narr_similarity(a: &str, b: &str) -> f64 {
    let sa = tokenize(a);
    let sb = tokenize(b);
    if sa.is_empty() || sb.is_empty() { return 0.0; }
    let intersection = sa.intersection(&sb).count();
    intersection as f64 / (sa.len() + sb.len() - intersection) as f64
}

// ── Amount / date comparison ─────────────────────────────────────────────────

/// Amount comparison within `fuzz_pct` of the larger of the two — port of
/// `_amountMatch`.
fn amount_match(a: f64, b: f64, fuzz_pct: f64) -> bool {
    let a = a.abs();
    let b = b.abs();
    if a == 0.0 && b == 0.0 { return true; }
    if a == 0.0 || b == 0.0 { return false; }
    let tolerance = a.max(b) * (fuzz_pct / 100.0);
    (a - b).abs() <= tolerance
}

/// Days between two date strings — `None` if either fails to parse. Port of
/// `_daysDiff`, reusing the shared date parser (rather than hand-rolled
/// arithmetic) so month/year boundaries are handled correctly.
fn days_diff(a: &str, b: &str) -> Option<f64> {
    let pa = normalize_transaction_date(a);
    let pb = normalize_transaction_date(b);
    if !pa.valid || !pb.valid { return None; }
    Some(((pa.ts - pb.ts).abs() as f64) / 86_400_000.0)
}

// ── Scoring ───────────────────────────────────────────────────────────────

/// Score a single (bank, voucher) pair — port of `_scoreMatch`. Returns 0.0
/// when they can't be considered a match at all (amount or date too far
/// apart), matching the JS engine's hard-reject behavior.
pub fn score_match(bank: &BankEntry, voucher: &Voucher, cfg: &ReconConfig) -> f64 {
    if !amount_match(bank.amount, voucher.amount, cfg.amount_fuzzy_pct * 2.0) {
        return 0.0;
    }

    let mut score = 0.0f64;

    // Amount closeness
    if amount_match(bank.amount, voucher.amount, 0.0) {
        score += 0.40;
    } else if amount_match(bank.amount, voucher.amount, cfg.amount_fuzzy_pct) {
        score += 0.30;
    } else {
        score += 0.10;
    }

    // Date closeness — unparseable or too-distant dates reject the pair.
    let days = match days_diff(&bank.date, &voucher.date) {
        Some(d) => d,
        None    => return 0.0,
    };
    if days == 0.0 {
        score += 0.40;
    } else if days <= 1.0 {
        score += 0.30;
    } else if days <= cfg.date_fuzzy_days as f64 {
        score += 0.20;
    } else if days <= (cfg.date_fuzzy_days * 2) as f64 {
        score += 0.05;
    } else {
        return 0.0;
    }

    // Narration similarity
    let narr = narr_similarity(&bank.narration, &voucher.narration);
    if narr >= 0.80 {
        score += 0.20;
    } else if narr >= cfg.narr_similarity_min {
        score += narr * 0.15;
    }

    // Reference / voucher-number exact match bonus
    let bref = bank.reference.trim();
    let vref = voucher.voucher_no.trim();
    if !bref.is_empty() && !vref.is_empty() && bref == vref {
        score += 0.20;
    }

    (score.min(1.0) * 100.0).round() / 100.0
}

/// Classify a score into a status tier — port of `_statusFromScore`.
pub fn status_from_score(score: f64, cfg: &ReconConfig) -> MatchStatus {
    if score >= cfg.auto_accept_above       { MatchStatus::Matched }
    else if score >= cfg.flag_below_confidence { MatchStatus::Likely }
    else if score > 0.0                     { MatchStatus::Possible }
    else                                     { MatchStatus::Unmatched }
}

// ── Greedy bipartite matching ────────────────────────────────────────────────

/// Match every bank transaction against every voucher — port of `_reconcile`:
/// score every candidate pair, sort best-first, then greedily assign, only
/// skipping a pair once either side has already been claimed by a
/// higher-scoring one. This is what makes a duplicate/near-duplicate bank
/// transaction resolve correctly — the single best-scoring voucher match wins
/// it, and the runner-up transaction falls through to its own best remaining
/// candidate (or unmatched, if none is left).
pub fn reconcile(bank: &[BankEntry], vouchers: &[Voucher], cfg: &ReconConfig) -> ReconReport {
    let mut candidates: Vec<(usize, usize, f64)> = Vec::new();
    for (bi, b) in bank.iter().enumerate() {
        for (vi, v) in vouchers.iter().enumerate() {
            let score = score_match(b, v, cfg);
            if score > 0.0 {
                candidates.push((bi, vi, score));
            }
        }
    }
    // Stable sort descending by score — ties keep candidate-generation order
    // (bank index then voucher index).
    candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    let mut bank_used    = vec![false; bank.len()];
    let mut voucher_used = vec![false; vouchers.len()];
    let mut matches = Vec::new();

    for (bi, vi, score) in candidates {
        if bank_used[bi] || voucher_used[vi] { continue; }
        bank_used[bi]    = true;
        voucher_used[vi] = true;
        matches.push(MatchPair { bank_idx: bi, voucher_idx: vi, score, status: status_from_score(score, cfg) });
    }

    let unmatched_bank     = (0..bank.len()).filter(|&i| !bank_used[i]).collect();
    let unmatched_vouchers = (0..vouchers.len()).filter(|&i| !voucher_used[i]).collect();

    ReconReport { matches, unmatched_bank, unmatched_vouchers }
}

// ── Tally export parsing ─────────────────────────────────────────────────────

/// Parse a Tally daybook export's raw string grid into vouchers — port of
/// `_parseTallyRows`. `rows[0]` must be the header row; column names are
/// matched case-insensitively by substring, mirroring the JS engine
/// ("particular"/"narration"/"description" for narration text, "voucher" for
/// voucher number, "type" for voucher type, "ledger" for ledger name).
/// Deviates from the JS engine in one deliberate way: amount is read from a
/// unified "amount"/"debit" column if one exists, else falls back to a
/// separate "credit" column per row — real Tally daybook exports commonly
/// split Debit/Credit into two columns with no unified "Amount" column at
/// all, which the literal JS logic (amount-or-debit-only, no credit
/// fallback) would silently miss entirely for credit rows.
pub fn parse_tally_grid(rows: &[Vec<String>]) -> Vec<Voucher> {
    if rows.len() < 2 { return Vec::new(); }

    let header: Vec<String> = rows[0].iter().map(|c| c.to_lowercase()).collect();
    let col = |name: &str| header.iter().position(|h| h.contains(name));

    let date_col    = col("date");
    let narr_col    = col("particular").or_else(|| col("narration")).or_else(|| col("description"));
    let amt_col     = col("amount").or_else(|| col("debit"));
    let credit_col  = col("credit");
    let voucher_col = col("voucher");
    let type_col    = col("type");
    let ledger_col  = col("ledger");

    let (date_col, narr_col) = match (date_col, narr_col) {
        (Some(d), Some(n)) => (d, n),
        _ => return Vec::new(),
    };

    let get = |row: &[String], idx: Option<usize>| -> String {
        idx.and_then(|i| row.get(i)).cloned().unwrap_or_default()
    };
    let parse_amount = |raw: &str| -> f64 {
        let cleaned: String = raw.chars().filter(|c| !"₹, ".contains(*c)).collect();
        let cleaned = cleaned.to_uppercase().replace("CR", "").replace("DR", "");
        cleaned.trim().parse::<f64>().unwrap_or(0.0).abs()
    };

    rows.iter().skip(1).filter_map(|row| {
        let raw_date = get(row, Some(date_col));
        let raw_narr = get(row, Some(narr_col));
        if raw_date.trim().is_empty() { return None; }

        let raw_amt = get(row, amt_col);
        let amount = if !raw_amt.trim().is_empty() {
            parse_amount(&raw_amt)
        } else {
            parse_amount(&get(row, credit_col))
        };

        if amount <= 0.0 && raw_narr.trim().is_empty() { return None; }

        Some(Voucher {
            date:         raw_date.trim().to_string(),
            amount,
            narration:    raw_narr.trim().to_string(),
            voucher_no:   get(row, voucher_col).trim().to_string(),
            voucher_type: get(row, type_col).trim().to_string(),
            ledger:       get(row, ledger_col).trim().to_string(),
        })
    }).collect()
}

// ── CSV report ────────────────────────────────────────────────────────────

fn csv_field(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Format a reconciliation report as CSV — port of `reportToCSV`.
pub fn report_to_csv(bank: &[BankEntry], vouchers: &[Voucher], report: &ReconReport) -> String {
    let mut lines = vec![
        "Match Status,Bank Date,Bank Amount,Bank Narration,Voucher Date,Voucher Amount,Voucher Narration,Voucher No,Confidence".to_string(),
    ];

    for m in &report.matches {
        let b = &bank[m.bank_idx];
        let v = &vouchers[m.voucher_idx];
        lines.push([
            csv_field(m.status.as_str()),
            csv_field(&b.date), csv_field(&format!("{:.2}", b.amount)), csv_field(&b.narration),
            csv_field(&v.date), csv_field(&format!("{:.2}", v.amount)), csv_field(&v.narration), csv_field(&v.voucher_no),
            csv_field(&format!("{:.0}%", m.score * 100.0)),
        ].join(","));
    }
    for &bi in &report.unmatched_bank {
        let b = &bank[bi];
        lines.push([
            csv_field("UNMATCHED-BANK"),
            csv_field(&b.date), csv_field(&format!("{:.2}", b.amount)), csv_field(&b.narration),
            csv_field(""), csv_field(""), csv_field(""), csv_field(""),
            csv_field("0%"),
        ].join(","));
    }
    for &vi in &report.unmatched_vouchers {
        let v = &vouchers[vi];
        lines.push([
            csv_field("MISSING-IN-BANK"),
            csv_field(""), csv_field(""), csv_field(""),
            csv_field(&v.date), csv_field(&format!("{:.2}", v.amount)), csv_field(&v.narration), csv_field(&v.voucher_no),
            csv_field("0%"),
        ].join(","));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bank(date: &str, amount: f64, narration: &str, reference: &str) -> BankEntry {
        BankEntry { date: date.to_string(), amount, narration: narration.to_string(), reference: reference.to_string() }
    }
    fn voucher(date: &str, amount: f64, narration: &str, voucher_no: &str) -> Voucher {
        Voucher { date: date.to_string(), amount, narration: narration.to_string(), voucher_no: voucher_no.to_string(), voucher_type: String::new(), ledger: String::new() }
    }

    // ── Exact matches ──────────────────────────────────────────────────────

    #[test]
    fn exact_date_and_amount_scores_at_or_above_auto_accept() {
        let cfg = ReconConfig::default();
        let b = bank("01/04/2026", 1000.0, "SALARY CREDIT ACME PVT LTD", "");
        let v = voucher("01/04/2026", 1000.0, "SALARY CREDIT ACME PVT LTD", "");
        let score = score_match(&b, &v, &cfg);
        assert!(score >= cfg.auto_accept_above, "exact match should score >= {}, got {}", cfg.auto_accept_above, score);
        assert_eq!(status_from_score(score, &cfg), MatchStatus::Matched);
    }

    #[test]
    fn reconcile_pairs_exact_match_and_leaves_no_unmatched() {
        let cfg = ReconConfig::default();
        let bank_v = vec![bank("01/04/2026", 1000.0, "SALARY CREDIT ACME PVT LTD", "")];
        let vouchers = vec![voucher("01/04/2026", 1000.0, "SALARY CREDIT ACME PVT LTD", "")];
        let report = reconcile(&bank_v, &vouchers, &cfg);
        assert_eq!(report.matches.len(), 1);
        assert_eq!(report.matches[0].status, MatchStatus::Matched);
        assert!(report.unmatched_bank.is_empty());
        assert!(report.unmatched_vouchers.is_empty());
    }

    // ── Amount tolerance ────────────────────────────────────────────────────

    #[test]
    fn amount_within_tolerance_still_matches_with_slightly_lower_score() {
        let cfg = ReconConfig::new(3, 1.0); // 1% amount tolerance
        let b = bank("01/04/2026", 1000.0, "RENT PAYMENT", "");
        let v = voucher("01/04/2026", 1005.0, "RENT PAYMENT", ""); // 0.5% off — within 1% tolerance
        let score = score_match(&b, &v, &cfg);
        assert!(score > 0.0, "amount within tolerance must still score > 0");
        let exact = score_match(&b, &voucher("01/04/2026", 1000.0, "RENT PAYMENT", ""), &cfg);
        assert!(score < exact, "a tolerance-only amount match must score lower than an exact one");
    }

    #[test]
    fn amount_outside_double_tolerance_is_rejected_entirely() {
        let cfg = ReconConfig::new(3, 0.5); // hard filter at 1% (2x)
        let b = bank("01/04/2026", 1000.0, "RENT PAYMENT", "");
        let v = voucher("01/04/2026", 1100.0, "RENT PAYMENT", ""); // 10% off
        assert_eq!(score_match(&b, &v, &cfg), 0.0, "amount far outside tolerance must hard-reject regardless of date/narration");
    }

    // ── Date tolerance ──────────────────────────────────────────────────────

    #[test]
    fn date_within_fuzzy_window_scores_lower_than_same_day() {
        let cfg = ReconConfig::new(3, 0.5);
        let b = bank("01/04/2026", 1000.0, "RENT PAYMENT", "");
        let same_day = score_match(&b, &voucher("01/04/2026", 1000.0, "RENT PAYMENT", ""), &cfg);
        let two_days = score_match(&b, &voucher("03/04/2026", 1000.0, "RENT PAYMENT", ""), &cfg);
        assert!(two_days > 0.0, "date within the fuzzy window must still score > 0");
        assert!(two_days < same_day, "a date within the fuzzy window must score lower than an exact same-day match");
    }

    #[test]
    fn date_beyond_double_fuzzy_window_is_rejected() {
        let cfg = ReconConfig::new(3, 0.5); // date rejects beyond 2*3 = 6 days
        let b = bank("01/04/2026", 1000.0, "RENT PAYMENT", "");
        let v = voucher("10/04/2026", 1000.0, "RENT PAYMENT", ""); // 9 days apart
        assert_eq!(score_match(&b, &v, &cfg), 0.0, "date far beyond the fuzzy window must hard-reject");
    }

    // ── Partial / narration-driven matches ──────────────────────────────────

    #[test]
    fn narration_similarity_and_reference_bonus_can_lift_a_weak_pair_into_possible() {
        let cfg = ReconConfig::new(3, 0.5);
        // Amount only loosely within double-tolerance (not exact, not within
        // single tolerance), date at the outer edge of the fuzzy window —
        // on their own these two components alone would land below the
        // "likely" threshold; matching reference numbers should still let it
        // register as at least a "possible" match instead of a hard reject.
        let b = bank("01/04/2026", 1000.0, "PAYMENT TO XYZ TRADERS FOR SUPPLIES", "REF12345");
        let v = voucher("04/04/2026", 1009.0, "PAYMENT TO XYZ TRADERS FOR SUPPLIES", "REF12345");
        let score = score_match(&b, &v, &cfg);
        assert!(score > 0.0, "must not hard-reject when reference numbers match");
        assert_ne!(status_from_score(score, &cfg), MatchStatus::Unmatched);
    }

    #[test]
    fn reference_number_mismatch_does_not_add_a_bonus() {
        let cfg = ReconConfig::default();
        let b = bank("01/04/2026", 1000.0, "PAYMENT", "REF-A");
        let with_match    = score_match(&b, &voucher("01/04/2026", 1000.0, "PAYMENT", "REF-A"), &cfg);
        let with_mismatch = score_match(&b, &voucher("01/04/2026", 1000.0, "PAYMENT", "REF-B"), &cfg);
        assert!(with_match >= with_mismatch);
    }

    // ── Unmatched transactions ──────────────────────────────────────────────

    #[test]
    fn bank_transaction_with_no_plausible_voucher_is_unmatched() {
        let cfg = ReconConfig::default();
        let bank_v = vec![bank("01/04/2026", 1000.0, "RENT", "")];
        let vouchers = vec![voucher("01/04/2026", 50000.0, "SOMETHING ELSE ENTIRELY", "")];
        let report = reconcile(&bank_v, &vouchers, &cfg);
        assert!(report.matches.is_empty());
        assert_eq!(report.unmatched_bank, vec![0]);
        assert_eq!(report.unmatched_vouchers, vec![0]);
    }

    #[test]
    fn empty_vouchers_leaves_every_bank_transaction_unmatched() {
        let cfg = ReconConfig::default();
        let bank_v = vec![bank("01/04/2026", 1000.0, "RENT", ""), bank("02/04/2026", 2000.0, "SALARY", "")];
        let report = reconcile(&bank_v, &[], &cfg);
        assert_eq!(report.matches.len(), 0);
        assert_eq!(report.unmatched_bank.len(), 2);
    }

    // ── Duplicate transactions ───────────────────────────────────────────────

    #[test]
    fn duplicate_bank_transactions_do_not_both_claim_the_same_voucher() {
        // Two identical bank transactions (e.g. a real same-day duplicate
        // charge) against a single matching voucher: exactly one must be
        // matched, the other must fall through to unmatched — never both
        // claiming the same voucher, and never the voucher being "used twice".
        let cfg = ReconConfig::default();
        let bank_v = vec![
            bank("01/04/2026", 1000.0, "SUBSCRIPTION FEE MONTHLY", ""),
            bank("01/04/2026", 1000.0, "SUBSCRIPTION FEE MONTHLY", ""),
        ];
        let vouchers = vec![voucher("01/04/2026", 1000.0, "SUBSCRIPTION FEE MONTHLY", "")];
        let report = reconcile(&bank_v, &vouchers, &cfg);
        assert_eq!(report.matches.len(), 1, "only one of the two duplicates can claim the single voucher");
        assert_eq!(report.unmatched_bank.len(), 1, "the other duplicate must be left unmatched");
        assert!(report.unmatched_vouchers.is_empty());
    }

    #[test]
    fn greedy_matching_prefers_the_globally_best_score_not_first_encountered() {
        // Bank txn A is an OK match for voucher 1 but a PERFECT match for
        // voucher 2. Bank txn B only matches voucher 1. A naive
        // first-available-wins matcher (iterating bank order) would grab
        // voucher 1 for A first, leaving B unmatched even though B has no
        // other option and A could have taken voucher 2 instead. The greedy
        // best-score-first algorithm must resolve this correctly: A takes
        // voucher 2 (its best match), freeing voucher 1 for B.
        let cfg = ReconConfig::new(3, 0.5);
        let bank_v = vec![
            bank("01/04/2026", 1000.0, "PAYMENT ALPHA CORP SERVICES", ""),   // A
            bank("01/04/2026", 1000.0, "PAYMENT BETA CORP", ""),              // B
        ];
        let vouchers = vec![
            voucher("01/04/2026", 1000.0, "PAYMENT BETA CORP", ""),          // voucher 1 — only matches B well, and A weakly (no narration overlap)
            voucher("01/04/2026", 1000.0, "PAYMENT ALPHA CORP SERVICES", ""),// voucher 2 — perfect match for A
        ];
        let report = reconcile(&bank_v, &vouchers, &cfg);
        assert_eq!(report.matches.len(), 2, "both bank transactions should end up matched");
        for m in &report.matches {
            if m.bank_idx == 0 { assert_eq!(m.voucher_idx, 1, "A must claim its perfect-match voucher 2"); }
            if m.bank_idx == 1 { assert_eq!(m.voucher_idx, 0, "B must get voucher 1, its only option"); }
        }
    }

    // ── Multiple statements / larger batches ─────────────────────────────────

    #[test]
    fn reconcile_handles_a_mixed_batch_of_matched_likely_and_unmatched() {
        let cfg = ReconConfig::new(3, 0.5);
        let bank_v = vec![
            bank("01/04/2026", 1000.0, "SALARY CREDIT ACME PVT LTD", ""),   // exact
            bank("05/04/2026", 2000.0, "RENT PAYMENT TO OWNER", ""),        // 2 days off — likely
            bank("10/04/2026", 999999.0, "UNRELATED HUGE TRANSFER", ""),    // no plausible voucher
        ];
        let vouchers = vec![
            voucher("01/04/2026", 1000.0, "SALARY CREDIT ACME PVT LTD", ""),
            voucher("07/04/2026", 2000.0, "RENT PAYMENT TO OWNER", ""),
        ];
        let report = reconcile(&bank_v, &vouchers, &cfg);
        assert_eq!(report.matched_count(), 1);
        assert!(report.likely_count() + report.possible_count() >= 1);
        assert_eq!(report.unmatched_bank, vec![2]);
        assert!(report.unmatched_vouchers.is_empty());
    }

    // ── Tally grid parsing ───────────────────────────────────────────────────

    #[test]
    fn parse_tally_grid_extracts_all_voucher_fields() {
        let rows = vec![
            vec!["Date".into(), "Particulars".into(), "Voucher No".into(), "Type".into(), "Debit".into(), "Credit".into(), "Ledger".into()],
            vec!["01/04/2026".into(), "Salary Credit Acme".into(), "V-001".into(), "Payment".into(), "1000".into(), "".into(), "Salaries".into()],
            vec!["02/04/2026".into(), "Rent Received".into(), "V-002".into(), "Receipt".into(), "".into(), "5000".into(), "Rent Income".into()],
        ];
        let vouchers = parse_tally_grid(&rows);
        assert_eq!(vouchers.len(), 2);
        assert_eq!(vouchers[0].amount, 1000.0);
        assert_eq!(vouchers[0].voucher_no, "V-001");
        assert_eq!(vouchers[0].voucher_type, "Payment");
        assert_eq!(vouchers[0].ledger, "Salaries");
        assert_eq!(vouchers[1].amount, 5000.0, "must fall back to the Credit column when Debit is blank");
    }

    #[test]
    fn parse_tally_grid_returns_empty_when_no_date_or_narration_column_found() {
        let rows = vec![
            vec!["Foo".into(), "Bar".into()],
            vec!["x".into(), "y".into()],
        ];
        assert!(parse_tally_grid(&rows).is_empty());
    }

    #[test]
    fn parse_tally_grid_skips_blank_date_rows() {
        let rows = vec![
            vec!["Date".into(), "Particulars".into(), "Debit".into()],
            vec!["".into(), "".into(), "".into()],
            vec!["01/04/2026".into(), "Salary".into(), "1000".into()],
        ];
        assert_eq!(parse_tally_grid(&rows).len(), 1);
    }

    // ── Narration similarity + CSV ───────────────────────────────────────────

    #[test]
    fn narr_similarity_identical_is_one_and_unrelated_is_zero() {
        assert_eq!(narr_similarity("SALARY CREDIT ACME PVT LTD", "SALARY CREDIT ACME PVT LTD"), 1.0);
        assert_eq!(narr_similarity("SALARY CREDIT ACME PVT LTD", "COMPLETELY DIFFERENT TEXT HERE"), 0.0);
    }

    #[test]
    fn report_to_csv_includes_matched_and_unmatched_rows() {
        let cfg = ReconConfig::default();
        let bank_v = vec![bank("01/04/2026", 1000.0, "SALARY", ""), bank("02/04/2026", 999.0, "UNRELATED", "")];
        let vouchers = vec![voucher("01/04/2026", 1000.0, "SALARY", "")];
        let report = reconcile(&bank_v, &vouchers, &cfg);
        let csv = report_to_csv(&bank_v, &vouchers, &report);
        assert!(csv.contains("Match Status,Bank Date"));
        assert!(csv.contains("Matched"));
        assert!(csv.contains("UNMATCHED-BANK"));
    }
}
