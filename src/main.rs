// main.rs — Entry point for the Bank Statement Processor (Rust + Slint).
//
// Boot sequence:
//   1. Initialise logger
//   2. Open (or create) the SQLite database
//   3. Create the Slint AppWindow
//   4. Wire callbacks: do-login, do-load-file, all toolbar/footer actions
//   5. Run the Slint event loop

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth;
mod analytics;
mod classifier;
mod db;
mod export;
mod narration_cleaner;
mod tally_group_engine;
mod parser;
mod settings;
mod ui;
#[cfg(feature = "ai")]
mod ai_classifier;

use std::sync::{Arc, Mutex};

#[cfg(feature = "slint-ui")]
use slint::{SharedString, Model as _};

#[cfg(feature = "slint-ui")]
slint::include_modules!();

// ── Login attempt tracker ─────────────────────────────────────────────────────

struct LoginState {
    attempts: u32,
    max:      u32,
}

impl LoginState {
    fn new() -> Self { Self { attempts: 0, max: 3 } }
    fn record_failure(&mut self) { self.attempts += 1; }
    fn remaining(&self) -> u32 { self.max.saturating_sub(self.attempts) }
    fn exhausted(&self) -> bool { self.attempts >= self.max }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[cfg(feature = "slint-ui")]
fn fmt_cell(v: Option<f64>) -> String {
    match v {
        None    => String::new(),
        Some(n) => ui::fmt_inr(n),
    }
}

#[cfg(feature = "slint-ui")]
fn stub_callback(name: &str) {
    log::info!("[Stub] {} — not yet implemented", name);
}

#[cfg(feature = "slint-ui")]
fn audit_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let z = (secs / 86400) as i64 + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp  = (5 * doy + 2) / 153;
    let d   = doy - (153 * mp + 2) / 5 + 1;
    let m   = if mp < 10 { mp + 3 } else { mp - 9 };
    let y   = if m <= 2 { y + 1 } else { y };
    let hh  = (secs % 86400) / 3600;
    let mm2 = (secs % 3600) / 60;
    let ss  = secs % 60;
    format!("{:02}/{:02}/{} {:02}:{:02}:{:02}", d, m, y, hh, mm2, ss)
}

// ── Reconcile helpers ─────────────────────────────────────────────────────────

/// Parse a Tally daybook Excel export into (date_str DD/MM/YYYY, amount) pairs.
/// Looks for the first header row containing "Date" and any Debit/Credit column.
#[cfg(feature = "slint-ui")]
fn reconcile_parse_tally(mut wb: calamine::Sheets<std::io::BufReader<std::fs::File>>) -> Vec<(String, f64)> {
    use calamine::Reader;
    let sheet_name = match wb.sheet_names().first() {
        Some(n) => n.to_string(),
        None    => return vec![],
    };
    let range = match wb.worksheet_range(&sheet_name) {
        Ok(r)  => r,
        Err(_) => return vec![],
    };
    let rows: Vec<Vec<calamine::Data>> = range.rows()
        .map(|r| r.to_vec())
        .collect();

    // Find header row: must contain "date" (case-insensitive)
    let mut date_col:   Option<usize> = None;
    let mut debit_col:  Option<usize> = None;
    let mut credit_col: Option<usize> = None;
    let mut header_row = 0usize;

    'outer: for (ri, row) in rows.iter().enumerate() {
        for (ci, cell) in row.iter().enumerate() {
            let s = cell.to_string().to_lowercase();
            if s.contains("date") { date_col = Some(ci); }
            if s.contains("debit")  { debit_col  = Some(ci); }
            if s.contains("credit") { credit_col = Some(ci); }
        }
        if date_col.is_some() && (debit_col.is_some() || credit_col.is_some()) {
            header_row = ri;
            break 'outer;
        }
        // reset if row didn't satisfy
        date_col = None; debit_col = None; credit_col = None;
    }

    let dc = match date_col { Some(c) => c, None => return vec![] };

    let mut entries: Vec<(String, f64)> = vec![];
    for row in rows.iter().skip(header_row + 1) {
        if row.len() <= dc { continue; }
        let raw_date = row[dc].to_string();
        if raw_date.trim().is_empty() { continue; }

        // Normalise date: accept DD/MM/YYYY, DD-MM-YYYY, YYYY-MM-DD
        let date_str = normalise_tally_date(&raw_date);
        if date_str.is_empty() { continue; }

        // Amount: prefer debit column, fall back to credit, then sum both
        let amount = match (debit_col, credit_col) {
            (Some(di), Some(ci)) => {
                let empty = calamine::Data::Empty;
                let d = parse_cell_amount(row.get(di).unwrap_or(&empty));
                let c = parse_cell_amount(row.get(ci).unwrap_or(&empty));
                if d > 0.0 { d } else { c }
            }
            (Some(di), None) => { let empty = calamine::Data::Empty; parse_cell_amount(row.get(di).unwrap_or(&empty)) }
            (None, Some(ci)) => { let empty = calamine::Data::Empty; parse_cell_amount(row.get(ci).unwrap_or(&empty)) }
            (None, None)     => 0.0,
        };
        if amount <= 0.0 { continue; }
        entries.push((date_str, amount));
    }
    entries
}

fn normalise_tally_date(s: &str) -> String {
    let s = s.trim();
    // DD/MM/YYYY or DD-MM-YYYY
    if s.len() == 10 {
        let sep = if s.contains('/') { '/' } else { '-' };
        let parts: Vec<&str> = s.splitn(3, sep).collect();
        if parts.len() == 3 {
            // Detect YYYY-MM-DD
            if parts[0].len() == 4 {
                return format!("{}/{}/{}", parts[2], parts[1], parts[0]);
            }
            return format!("{}/{}/{}", parts[0], parts[1], parts[2]);
        }
    }
    String::new()
}

fn parse_cell_amount(cell: &calamine::Data) -> f64 {
    match cell {
        calamine::Data::Float(f) => f.abs(),
        calamine::Data::Int(i)   => (*i as f64).abs(),
        calamine::Data::String(s) => s.replace(',', "").trim().parse::<f64>().unwrap_or(0.0).abs(),
        _                         => 0.0,
    }
}

/// Match Tally entries against bank entries.
/// Returns (exact_matched, likely_matched, _) counts.
/// Exact = same date + same amount (±0.01).
/// Likely = amount matches but date differs by ≤7 days.
#[cfg(feature = "slint-ui")]
// Returns (exact, likely, unmatched_tally, tally_status["Matched"|"Likely"|"Unmatched"], bank_used)
fn reconcile_match(
    tally:      &[(String, f64)],
    bank:       &[(String, f64)],
    days_window: i64,
    amt_tol:     f64,
) -> (usize, usize, usize, Vec<&'static str>, Vec<bool>) {
    let mut bank_used   = vec![false; bank.len()];
    let mut tally_used  = vec![false; tally.len()];
    let mut tally_exact = vec![false; tally.len()];
    let mut exact  = 0usize;
    let mut likely = 0usize;
    let amt_tolerance = |a: f64, b: f64| -> bool {
        if a == 0.0 { return (b - a).abs() <= 0.01; }
        (a - b).abs() <= 0.01 || (a - b).abs() / a * 100.0 <= amt_tol
    };

    // Exact pass: same date + same amount (within tolerance)
    for (ti, (td, ta)) in tally.iter().enumerate() {
        for (bi, (bd, ba)) in bank.iter().enumerate() {
            if bank_used[bi] { continue; }
            if amt_tolerance(*ta, *ba) && td == bd {
                bank_used[bi]   = true;
                tally_used[ti]  = true;
                tally_exact[ti] = true;
                exact += 1;
                break;
            }
        }
    }
    // Likely pass: same amount (within tolerance), date within days_window
    for (ti, (td, ta)) in tally.iter().enumerate() {
        if tally_used[ti] { continue; }
        for (bi, (bd, ba)) in bank.iter().enumerate() {
            if bank_used[bi] { continue; }
            if amt_tolerance(*ta, *ba) {
                if let (Some(td_ymd), Some(bd_ymd)) = (parse_date_for_recon(td), parse_date_for_recon(bd)) {
                    if (td_ymd as i64 - bd_ymd as i64).abs() <= days_window {
                        bank_used[bi]  = true;
                        tally_used[ti] = true;
                        likely += 1;
                        break;
                    }
                }
            }
        }
    }
    let tally_status: Vec<&'static str> = (0..tally.len()).map(|i| {
        if tally_exact[i]      { "Matched" }
        else if tally_used[i]  { "Likely"  }
        else                   { "Unmatched" }
    }).collect();
    (exact, likely, tally.len().saturating_sub(exact + likely), tally_status, bank_used)
}

fn parse_date_for_recon(s: &str) -> Option<u32> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 3 { return None; }
    let dd = parts[0].parse::<u32>().ok()?;
    let mm = parts[1].parse::<u32>().ok()?;
    let yy = parts[2].parse::<u32>().ok()?;
    Some(yy * 10000 + mm * 100 + dd)
}

// ── Parse DD/MM/YYYY → (yyyy, mm, dd) for ordering comparisons ────────────────
#[cfg(feature = "slint-ui")]
fn parse_date_ymd(s: &str) -> Option<(i32, u32, u32)> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 3 { return None; }
    let dd   = parts[0].parse::<u32>().ok()?;
    let mm   = parts[1].parse::<u32>().ok()?;
    let yyyy = parts[2].parse::<i32>().ok()?;
    Some((yyyy, mm, dd))
}

// ── Apply status + date + bank filters to a transaction list ──────────────────
#[cfg(feature = "slint-ui")]
fn apply_txn_filters<'a>(
    txns: &'a [parser::Transaction],
    status:  &str,
    from:    &str,
    to:      &str,
    bank:    &str,
) -> Vec<&'a parser::Transaction> {
    txns.iter()
        .filter(|t| !t.is_opening_balance)
        .filter(|t| match status {
            "unreviewed"   => matches!(t.status, parser::TransactionStatus::Unreviewed),
            "suspense"     => matches!(t.status, parser::TransactionStatus::Suspense),
            "high"         => matches!(t.status, parser::TransactionStatus::Classified) && t.confidence >= 0.7,
            "duplicates"   => t.dup_flag,
            "gst"          => t.tags.iter().any(|g| { let u = g.to_uppercase(); u.contains("GST") || u.contains("TAX") }),
            "needs_review" => matches!(t.status, parser::TransactionStatus::NeedsReview),
            _              => true,
        })
        .filter(|t| {
            if from.is_empty() && to.is_empty() { return true; }
            let td = match parse_date_ymd(&t.date) { Some(d) => d, None => return true };
            if !from.is_empty() {
                if let Some(fd) = parse_date_ymd(from) { if td < fd { return false; } }
            }
            if !to.is_empty() {
                if let Some(td2) = parse_date_ymd(to) { if td > td2 { return false; } }
            }
            true
        })
        .filter(|t| bank.is_empty() || bank == "All Banks" || t.bank_name == bank)
        .collect()
}

// ── Build Slint TxnRow model from filtered transaction slice ──────────────────
#[cfg(feature = "slint-ui")]
fn build_txn_rows(txns: &[&parser::Transaction]) -> Vec<TxnRow> {
    txns.iter().map(|t| {
        let narr: String = t.narration.chars().take(80).collect();
        let has_gst = t.tags.iter().any(|g| g.to_uppercase().contains("GST"));
        let has_tax = t.tags.iter().any(|g| g.to_uppercase().contains("TAX"));
        let has_dup = t.dup_flag || t.tags.iter().any(|g| g.to_uppercase().contains("DUP"));
        let row_color: i32 = match t.status {
            parser::TransactionStatus::NeedsReview => 3,
            parser::TransactionStatus::Suspense    => 4,
            parser::TransactionStatus::Manual      => 6,
            _ if t.dup_flag                        => 5,
            parser::TransactionStatus::Classified  => if t.confidence >= 0.7 { 1 } else { 2 },
            _                                      => 0,
        };
        TxnRow {
            bank_name:    SharedString::from(t.bank_name.as_str()),
            account_no:   SharedString::from(t.account_no.as_str()),
            date:         SharedString::from(t.date.as_str()),
            narration:    SharedString::from(narr.as_str()),
            ref_no:       SharedString::from(t.reference.as_str()),
            debit:        SharedString::from(fmt_cell(t.debit).as_str()),
            credit:       SharedString::from(fmt_cell(t.credit).as_str()),
            balance:      SharedString::from(fmt_cell(t.balance).as_str()),
            vendor:       SharedString::from(t.vendor.as_str()),
            ledger:       SharedString::from(t.account_head.as_str()),
            expense_head: SharedString::from(""),
            status_text:  SharedString::from(t.status.to_string().as_str()),
            tags:         SharedString::from(t.tags.join(" ").as_str()),
            review:       SharedString::from(""),
            row_color,
            has_gst,
            has_tax,
            has_dup_tag:  has_dup,
        }
    }).collect()
}

// ── Compute filter-count badges from full unfiltered list ─────────────────────
#[cfg(feature = "slint-ui")]
fn compute_filter_counts(txns: &[parser::Transaction]) -> [usize; 7] {
    let real: Vec<&parser::Transaction> = txns.iter().filter(|t| !t.is_opening_balance).collect();
    let all        = real.len();
    let unreviewed = real.iter().filter(|t| matches!(t.status, parser::TransactionStatus::Unreviewed)).count();
    let suspense   = real.iter().filter(|t| matches!(t.status, parser::TransactionStatus::Suspense)).count();
    let high       = real.iter().filter(|t| matches!(t.status, parser::TransactionStatus::Classified) && t.confidence >= 0.7).count();
    let duplicates = real.iter().filter(|t| t.dup_flag).count();
    let gst        = real.iter().filter(|t| t.tags.iter().any(|g| { let u = g.to_uppercase(); u.contains("GST") || u.contains("TAX") })).count();
    let review     = real.iter().filter(|t| matches!(t.status, parser::TransactionStatus::NeedsReview)).count();
    [all, unreviewed, suspense, high, duplicates, gst, review]
}

// ── Rebuild visible TxnRow model + filter counts from current AppState filters ─
#[cfg(feature = "slint-ui")]
fn rebuild_rows(h: &AppWindow, st: &ui::AppState) {
    let filtered = apply_txn_filters(
        &st.transactions,
        &st.active_filter,
        &st.date_from,
        &st.date_to,
        &st.bank_filter,
    );
    let rows = build_txn_rows(&filtered);
    h.set_transaction_rows(slint::ModelRc::new(slint::VecModel::from(rows)));

    // Update filter badge counts (always from full unfiltered list)
    let [all, unreviewed, suspense, high, dups, gst, review] =
        compute_filter_counts(&st.transactions);
    h.set_fc_all(SharedString::from(all.to_string().as_str()));
    h.set_fc_unreviewed(SharedString::from(unreviewed.to_string().as_str()));
    h.set_fc_suspense(SharedString::from(suspense.to_string().as_str()));
    h.set_fc_high(SharedString::from(high.to_string().as_str()));
    h.set_fc_duplicates(SharedString::from(dups.to_string().as_str()));
    h.set_fc_gst(SharedString::from(gst.to_string().as_str()));
    h.set_fc_review(SharedString::from(review.to_string().as_str()));

    log::info!("[Filter] showing {} / {} txns  (status='{}' from='{}' to='{}' bank='{}')",
        filtered.len(), st.transactions.iter().filter(|t| !t.is_opening_balance).count(),
        st.active_filter, st.date_from, st.date_to, st.bank_filter);
}

// ── Compute FY date range (Indian FY: Apr 1 → Mar 31) ────────────────────────
#[cfg(feature = "slint-ui")]
fn fy_range(current: bool) -> (String, String) {
    // Use today from system time; approximate year via a fixed reference
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Seconds since epoch → rough date (leap years ignored, close enough for FY)
    let days = (now / 86400) as i64;
    let year = 1970 + days / 365;
    let day_of_year = days % 365;
    // April 1 = day 90 (approx)
    let (fy_start_year, fy_end_year) = if day_of_year >= 90 {
        (year, year + 1)   // Apr..Dec → FY start = this year
    } else {
        (year - 1, year)   // Jan..Mar → FY start = last year
    };
    if current {
        (format!("01/04/{}", fy_start_year), format!("31/03/{}", fy_end_year))
    } else {
        (format!("01/04/{}", fy_start_year - 1), format!("31/03/{}", fy_start_year))
    }
}

// ── Compute today / this-month / last-month date ranges ──────────────────────
#[cfg(feature = "slint-ui")]
fn preset_range(preset: &str) -> (String, String) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days_since_epoch = (now / 86400) as i64;
    // Gregorian date calculation (Zeller-ish, accurate enough for presets)
    let z = days_since_epoch + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    match preset {
        "today" => {
            let today = format!("{:02}/{:02}/{}", d, m, y);
            (today.clone(), today)
        }
        "this_month" => {
            let last_day = days_in_month(m as u32, y as i32);
            (format!("01/{:02}/{}", m, y), format!("{:02}/{:02}/{}", last_day, m, y))
        }
        "last_month" => {
            let (pm, py) = if m == 1 { (12i64, y - 1) } else { (m - 1, y) };
            let last_day = days_in_month(pm as u32, py as i32);
            (format!("01/{:02}/{}", pm, py), format!("{:02}/{:02}/{}", last_day, pm, py))
        }
        "current_fy" => fy_range(true),
        "prev_fy"    => fy_range(false),
        _            => (String::new(), String::new()),
    }
}

#[cfg(feature = "slint-ui")]
fn days_in_month(month: u32, year: i32) -> u32 {
    match month {
        1|3|5|7|8|10|12 => 31,
        4|6|9|11 => 30,
        2 => if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) { 29 } else { 28 },
        _ => 30,
    }
}

#[cfg(feature = "slint-ui")]
fn push_dashboard(h: &AppWindow, txns: &[parser::Transaction], opening_bal: Option<f64>) {
    use analytics::{compute, unique_banks, unique_vendors, unique_heads, fmt_amt};

    let data = compute(txns, opening_bal);
    let s = &data.summary;

    // Summary cards
    h.set_dash_credits(SharedString::from(fmt_amt(Some(s.total_credit)).as_str()));
    h.set_dash_debits(SharedString::from(fmt_amt(Some(s.total_debit)).as_str()));
    h.set_dash_net_flow(SharedString::from(fmt_amt(Some(s.net_flow)).as_str()));
    h.set_dash_vendors(SharedString::from(s.vendor_count.to_string().as_str()));
    h.set_dash_txn_count(SharedString::from(s.txn_count.to_string().as_str()));
    h.set_dash_top_expense(SharedString::from(s.top_expense_head.as_str()));
    h.set_dash_top_exp_amt(SharedString::from(
        if s.top_expense_amt > 0.0 { fmt_amt(Some(s.top_expense_amt)) } else { String::new() }
    .as_str()));
    h.set_dash_has_data(s.txn_count > 0);

    // Insights
    let ins = &data.insights;
    h.set_dash_ins_max_dr(SharedString::from(ins.max_dr_amt.as_str()));
    h.set_dash_ins_max_dr_narr(SharedString::from(ins.max_dr_narr.as_str()));
    h.set_dash_ins_max_cr(SharedString::from(ins.max_cr_amt.as_str()));
    h.set_dash_ins_max_cr_narr(SharedString::from(ins.max_cr_narr.as_str()));
    h.set_dash_ins_avg_dr(SharedString::from(ins.avg_dr.as_str()));
    h.set_dash_ins_avg_cr(SharedString::from(ins.avg_cr.as_str()));
    h.set_dash_ins_dr_count(SharedString::from(ins.dr_count.as_str()));
    h.set_dash_ins_cr_count(SharedString::from(ins.cr_count.as_str()));
    h.set_dash_ins_freq_vendor(SharedString::from(ins.freq_vendor.as_str()));

    // Monthly chart — normalise bars
    let max_monthly = data.monthly.credits.iter().chain(data.monthly.debits.iter())
        .cloned().fold(0.0f64, f64::max);
    let month_bars: Vec<DashMonthBar> = data.monthly.labels.iter()
        .enumerate()
        .map(|(i, lbl)| {
            let cr = data.monthly.credits.get(i).cloned().unwrap_or(0.0);
            let dr = data.monthly.debits.get(i).cloned().unwrap_or(0.0);
            let scale = if max_monthly > 0.0 { max_monthly } else { 1.0 };
            DashMonthBar {
                label:      SharedString::from(lbl.as_str()),
                credit_h:   (cr / scale) as f32,
                debit_h:    (dr / scale) as f32,
                credit_str: SharedString::from(fmt_amt(Some(cr)).as_str()),
                debit_str:  SharedString::from(fmt_amt(Some(dr)).as_str()),
            }
        })
        .collect();
    h.set_dash_chart_monthly(slint::ModelRc::new(slint::VecModel::from(month_bars)));

    // Expense breakdown — normalise widths
    let max_exp = data.expenses.iter().map(|e| e.amount).fold(0.0f64, f64::max);
    let exp_bars: Vec<DashExpBar> = data.expenses.iter().map(|e| DashExpBar {
        label:      SharedString::from(e.label.as_str()),
        w:          (if max_exp > 0.0 { e.amount / max_exp } else { 0.0 }) as f32,
        amount_str: SharedString::from(fmt_amt(Some(e.amount)).as_str()),
        color_idx:  e.color_idx,
        pct:        e.pct,
    }).collect();
    h.set_dash_chart_expenses(slint::ModelRc::new(slint::VecModel::from(exp_bars)));

    // Cash flow
    let cf: Vec<DashCashPt> = data.cashflow.iter().map(|p| DashCashPt { h: p.norm }).collect();
    h.set_dash_chart_cashflow(slint::ModelRc::new(slint::VecModel::from(cf)));

    // Vendors — normalise widths
    let max_vendor = data.vendors.iter().map(|v| v.debit.max(v.credit)).fold(0.0f64, f64::max);
    let vbars: Vec<DashVendorBar> = data.vendors.iter().map(|v| {
        let scale = if max_vendor > 0.0 { max_vendor } else { 1.0 };
        DashVendorBar {
            label:      SharedString::from(v.name.as_str()),
            debit_w:    (v.debit  / scale) as f32,
            credit_w:   (v.credit / scale) as f32,
            debit_str:  SharedString::from(
                if v.debit  > 0.0 { analytics::fmt_short_pub(v.debit)  } else { String::new() }.as_str()),
            credit_str: SharedString::from(
                if v.credit > 0.0 { analytics::fmt_short_pub(v.credit) } else { String::new() }.as_str()),
        }
    }).collect();
    h.set_dash_chart_vendors(slint::ModelRc::new(slint::VecModel::from(vbars)));

    // Filter dropdown options
    let mut banks_opts: Vec<SharedString> = std::iter::once(SharedString::from("All Banks"))
        .chain(unique_banks(txns).into_iter().map(|s| SharedString::from(s.as_str())))
        .collect();
    let mut vendor_opts: Vec<SharedString> = std::iter::once(SharedString::from("All Vendors"))
        .chain(unique_vendors(txns).into_iter().map(|s| SharedString::from(s.as_str())))
        .collect();
    let mut head_opts: Vec<SharedString> = std::iter::once(SharedString::from("All Expense Heads"))
        .chain(unique_heads(txns).into_iter().map(|s| SharedString::from(s.as_str())))
        .collect();
    let _ = (banks_opts.len(), vendor_opts.len(), head_opts.len());

    h.set_dash_filter_banks(slint::ModelRc::new(slint::VecModel::from(banks_opts)));
    h.set_dash_filter_vendors(slint::ModelRc::new(slint::VecModel::from(vendor_opts)));
    h.set_dash_filter_heads(slint::ModelRc::new(slint::VecModel::from(head_opts)));
}

// ── main ──────────────────────────────────────────────────────────────────────

#[cfg(feature = "slint-ui")]
fn apply_parse_result(
    h: &AppWindow,
    state_ref: &Arc<Mutex<ui::AppState>>,
    db_ref: &Arc<Mutex<Option<rusqlite::Connection>>>,
    result: parser::ParseResult,
    file_name: &str,
) {
    let real: Vec<&parser::Transaction> = result
        .transactions
        .iter()
        .filter(|t| !t.is_opening_balance)
        .collect();

    let total_dr: f64 = real.iter().filter_map(|t| t.debit).sum();
    let total_cr: f64 = real.iter().filter_map(|t| t.credit).sum();

    let dates: Vec<&str> = real.iter()
        .filter(|t| !t.date.is_empty())
        .map(|t| t.date.as_str())
        .collect();
    let period = if dates.len() >= 2 {
        format!("{} \u{2013} {}", dates[0], dates[dates.len() - 1])
    } else if dates.len() == 1 {
        dates[0].to_string()
    } else {
        "\u{2014}".to_string()
    };

    let unreviewed_cnt = real.iter()
        .filter(|t| matches!(t.status, parser::TransactionStatus::Unreviewed))
        .count();
    let credit_cnt = real.iter().filter(|t| t.credit.is_some()).count();
    let debit_cnt  = real.iter().filter(|t| t.debit.is_some()).count();

    let calc_closing = match result.opening_balance {
        Some(ob) => Some((ob + total_cr - total_dr).round() / 1.0),
        None     => None,
    };
    let has_mismatch = match (result.closing_balance, calc_closing) {
        (Some(stated), Some(calc)) => (stated - calc).abs() >= 0.5,
        _ => false,
    };
    let mismatch_str = if has_mismatch {
        match (result.closing_balance, calc_closing) {
            (Some(stated), Some(calc)) => format!("Diff: {}", ui::fmt_inr((stated - calc).abs())),
            _ => String::new(),
        }
    } else {
        String::new()
    };

    log::info!(
        "Summary: bank='{}' txns={} dr={:.2} cr={:.2} ob={:?} cb={:?}",
        result.bank_name, real.len(), total_dr, total_cr,
        result.opening_balance, result.closing_balance
    );

    // Run narration cleaner on all transactions.
    let narration_strs: Vec<String> = real.iter().map(|t| t.narration.clone()).collect();
    let cleaned_narrations = narration_cleaner::clean_batch(&narration_strs);

    // Compute Tally group for each transaction.
    let tally_inputs: Vec<(String, String, bool, f64)> = real.iter().enumerate().map(|(idx, t)| {
        let narr = cleaned_narrations[idx].cleaned.clone();
        let is_credit = t.credit.is_some();
        let amount = t.credit.unwrap_or(0.0) + t.debit.unwrap_or(0.0);
        (t.account_head.clone(), narr, is_credit, amount)
    }).collect();
    let tally_groups = tally_group_engine::classify_batch(&tally_inputs, None);

    let mut bank_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let row_models: Vec<TxnRow> = real
        .iter()
        .enumerate()
        .map(|(idx, t)| {
            bank_set.insert(t.bank_name.clone());
            let meta = &cleaned_narrations[idx];
            let narr: String = if meta.confidence >= 0.4 && !meta.cleaned.is_empty() {
                meta.cleaned.chars().take(80).collect()
            } else {
                t.narration.chars().take(80).collect()
            };
            let vendor_display = if t.vendor.is_empty() && !meta.party.is_empty() {
                meta.party.chars().take(40).collect::<String>()
            } else {
                t.vendor.clone()
            };
            let has_gst = t.tags.iter().any(|tag| tag.to_uppercase().contains("GST"));
            let has_tax = t.tags.iter().any(|tag| tag.to_uppercase().contains("TAX"));
            let has_dup = t.tags.iter().any(|tag| tag.to_uppercase().contains("DUP")) || t.dup_flag;
            let row_color: i32 = match t.status {
                parser::TransactionStatus::NeedsReview => 3,
                parser::TransactionStatus::Suspense    => 4,
                parser::TransactionStatus::Manual      => 6,
                _ if t.dup_flag                        => 5,
                parser::TransactionStatus::Classified  => {
                    if t.confidence >= 0.7 { 1 } else { 2 }
                }
                _ => 0,
            };
            let tally_group = tally_groups[idx].as_str();
            TxnRow {
                bank_name:    SharedString::from(t.bank_name.as_str()),
                account_no:   SharedString::from(t.account_no.as_str()),
                date:         SharedString::from(t.date.as_str()),
                narration:    SharedString::from(narr.as_str()),
                ref_no:       SharedString::from(t.reference.as_str()),
                debit:        SharedString::from(fmt_cell(t.debit).as_str()),
                credit:       SharedString::from(fmt_cell(t.credit).as_str()),
                balance:      SharedString::from(fmt_cell(t.balance).as_str()),
                vendor:       SharedString::from(vendor_display.as_str()),
                ledger:       SharedString::from(t.account_head.as_str()),
                expense_head: SharedString::from(tally_group),
                status_text:  SharedString::from(t.status.to_string().as_str()),
                tags:         SharedString::from(t.tags.join(" ").as_str()),
                review:       SharedString::from(""),
                row_color,
                has_gst,
                has_tax,
                has_dup_tag: has_dup,
            }
        })
        .collect();

    let bank_names: Vec<SharedString> = std::iter::once(SharedString::from("All Banks"))
        .chain(bank_set.iter().map(|b| SharedString::from(b.as_str())))
        .collect();

    let table_model = slint::ModelRc::new(slint::VecModel::from(row_models));
    let banks_model = slint::ModelRc::new(slint::VecModel::from(bank_names));

    h.set_transaction_rows(table_model);
    h.set_bank_names(banks_model);
    h.set_status_file(SharedString::from(file_name));
    h.set_status_bank(SharedString::from(result.bank_name.as_str()));

    h.set_dash_bank_name(SharedString::from(result.bank_name.as_str()));
    h.set_dash_opening(SharedString::from(ui::AppState::fmt_amount(result.opening_balance).as_str()));
    h.set_dash_closing(SharedString::from(ui::AppState::fmt_amount(result.closing_balance).as_str()));
    h.set_dash_credits(SharedString::from(ui::AppState::fmt_amount(Some(total_cr)).as_str()));
    h.set_dash_debits(SharedString::from(ui::AppState::fmt_amount(Some(total_dr)).as_str()));
    h.set_dash_txn_count(SharedString::from(real.len().to_string().as_str()));
    h.set_dash_vendors(SharedString::from("\u{2014}"));
    h.set_dash_account_no(SharedString::from(result.account_no.as_str()));
    h.set_dash_period(SharedString::from(period.as_str()));
    h.set_dash_credit_count(SharedString::from(credit_cnt.to_string().as_str()));
    h.set_dash_debit_count(SharedString::from(debit_cnt.to_string().as_str()));
    h.set_dash_unreviewed(SharedString::from(unreviewed_cnt.to_string().as_str()));
    h.set_dash_suspense(SharedString::from("0"));
    h.set_dash_needs_review(SharedString::from("0"));
    h.set_dash_duplicates(SharedString::from("0"));
    h.set_dash_gst_count(SharedString::from("0"));
    h.set_dash_calc_closing(SharedString::from(ui::AppState::fmt_amount(calc_closing).as_str()));
    h.set_dash_has_mismatch(has_mismatch);
    h.set_dash_mismatch(SharedString::from(mismatch_str.as_str()));

    let import_id_persisted: Option<i64> = {
        let client_id = state_ref.lock().unwrap().client_id;
        if let Some(cid) = client_id {
            let db = db_ref.lock().unwrap();
            if let Some(conn) = db.as_ref() {
                let imp_id = db::save_import(
                    conn, cid, file_name, &result.bank_name,
                    &result.account_no, real.len(),
                ).ok();
                if let Some(iid) = imp_id {
                    let _ = db::upsert_transactions(conn, cid, Some(iid), &result.transactions);
                    log::info!("[LoadFile] persisted {} txns import_id={}", real.len(), iid);
                }
                imp_id
            } else { None }
        } else { None }
    };

    {
        let mut st = state_ref.lock().unwrap();
        st.bank_name       = result.bank_name.clone();
        st.account_no      = result.account_no.clone();
        st.file_name       = file_name.to_owned();
        st.opening_balance = result.opening_balance;
        st.closing_balance = result.closing_balance;
        st.total_debits    = total_dr;
        st.total_credits   = total_cr;
        st.txn_count       = real.len();
        st.unreviewed      = unreviewed_cnt;
        st.transactions    = result.transactions.clone();
        st.active_filter   = "all".to_string();
        st.date_from       = String::new();
        st.date_to         = String::new();
        st.bank_filter     = String::new();
        st.pending_pdf_path = None;
        st.pending_pdf_name = String::new();
        if let Some(iid) = import_id_persisted {
            st.import_ids.push(iid);
        }
    }

    let [all_cnt, unreview_cnt2, susp_cnt, high_cnt, dup_cnt, gst_cnt, rev_cnt] =
        compute_filter_counts(&result.transactions);
    h.set_fc_all(SharedString::from(all_cnt.to_string().as_str()));
    h.set_fc_unreviewed(SharedString::from(unreview_cnt2.to_string().as_str()));
    h.set_fc_suspense(SharedString::from(susp_cnt.to_string().as_str()));
    h.set_fc_high(SharedString::from(high_cnt.to_string().as_str()));
    h.set_fc_duplicates(SharedString::from(dup_cnt.to_string().as_str()));
    h.set_fc_gst(SharedString::from(gst_cnt.to_string().as_str()));
    h.set_fc_review(SharedString::from(rev_cnt.to_string().as_str()));

    let txns_all: Vec<parser::Transaction> = result.transactions.clone();
    push_dashboard(h, &txns_all, result.opening_balance);

    log::info!("UI updated with {} transactions", real.len());
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .init();

    log::info!("Bank Statement Processor starting…");

    let db_path = {
        let mut p = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("."));
        p.pop();
        p.push("bsp_data.db");
        p
    };
    let db_conn: Arc<Mutex<Option<rusqlite::Connection>>> = Arc::new(Mutex::new(
        match db::open(&db_path) {
            Ok(c)    => { log::info!("Database ready at {:?}", db_path); Some(c) }
            Err(err) => { log::warn!("Database init failed (non-fatal): {}", err); None }
        }
    ));

    // ── Slint UI ──────────────────────────────────────────────────────────────
    #[cfg(feature = "slint-ui")]
    {
        let app = AppWindow::new()?;
        // Login screen shown by default (logged-in = false, set by Slint default)

        let app_state: Arc<Mutex<ui::AppState>> =
            Arc::new(Mutex::new(ui::AppState::default()));

        // ── Load clients from DB into dropdown on startup ─────────────────────
        {
            let db = db_conn.lock().unwrap();
            if let Some(conn) = db.as_ref() {
                if let Ok(clients) = db::get_clients(conn) {
                    let names: Vec<SharedString> =
                        std::iter::once(SharedString::from("-- Select Client --"))
                        .chain(clients.iter().map(|c| SharedString::from(c.name.as_str())))
                        .collect();
                    app.set_client_names(slint::ModelRc::new(slint::VecModel::from(names)));
                }
                // Restore all settings
                let cfg = settings::Settings::load(conn);
                app.set_ai_provider_idx(match cfg.ai_provider.as_str() {
                    "claude" => 1, "gemini" => 2, _ => 0,
                });
                app.set_ai_api_key(SharedString::from(cfg.ai_api_key.as_str()));
                // Populate Application Settings UI properties
                app.set_settings_narr_enabled(cfg.narr_enabled);
                app.set_settings_narr_title_case(cfg.narr_title_case);
                app.set_settings_narr_preserve(cfg.narr_preserve);
                app.set_settings_gst_enabled(cfg.gst_enabled);
                app.set_settings_gst_auto_ledgers(cfg.gst_auto_ledgers);
                app.set_settings_recon_days(SharedString::from(cfg.recon_days.to_string().as_str()));
                app.set_settings_recon_pct(SharedString::from(cfg.recon_pct.to_string().as_str()));
                app.set_settings_log_level(match cfg.log_level.as_str() {
                    "DEBUG" => 1, "WARN" => 2, "ERROR" => 3, _ => 0,
                });
                {
                    let mut st = app_state.lock().unwrap();
                    st.ai_provider = cfg.ai_provider;
                    st.ai_api_key  = cfg.ai_api_key;
                    st.ai_enabled  = cfg.ai_enabled;
                }
            }
        }

        // ── Login ─────────────────────────────────────────────────────────────
        {
            let handle      = app.as_weak();
            let login_state = Arc::new(Mutex::new(LoginState::new()));

            app.on_do_login(move |email: SharedString, password: SharedString| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut ls = login_state.lock().unwrap();

                if ls.exhausted() {
                    h.set_login_error(
                        "Too many failed attempts. Please restart the application.".into(),
                    );
                    return;
                }

                if auth::validate_credentials(&email, &password) {
                    log::info!("Login successful for {}", email);
                    h.set_logged_in(true);
                    h.set_login_error("".into());
                    h.set_login_loading(false);
                } else {
                    ls.record_failure();
                    let remaining = ls.remaining();
                    let msg: SharedString = if remaining == 0 {
                        "Too many failed attempts. Please restart the application.".into()
                    } else {
                        format!(
                            "Invalid credentials. {} attempt{} remaining.",
                            remaining,
                            if remaining == 1 { "" } else { "s" }
                        )
                        .into()
                    };
                    log::warn!("Login failed for {} — {} remaining", email, remaining);
                    h.set_login_error(msg);
                }
            });
        }

        // ── Load File ─────────────────────────────────────────────────────────
        {
            let handle     = app.as_weak();
            let state_ref  = app_state.clone();
            let db_ref     = db_conn.clone();

            app.on_do_load_file(move || {
                let path = match rfd::FileDialog::new()
                    .set_title("Open Bank Statement")
                    .add_filter("Bank Statements", &["pdf", "xlsx", "xls", "xlsm"])
                    .add_filter("PDF", &["pdf"])
                    .add_filter("Excel", &["xlsx", "xls", "xlsm"])
                    .pick_file()
                {
                    Some(p) => p,
                    None    => return,
                };

                let h = match handle.upgrade() { Some(h) => h, None => return };

                let file_name = path
                    .file_name()
                    .map_or_else(|| "unknown".to_string(), |n| n.to_string_lossy().into_owned());

                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();

                log::info!("Opening: {} (ext={})", file_name, ext);
                h.set_status_file(SharedString::from(file_name.as_str()));
                h.set_status_bank(SharedString::from("Parsing…"));

                let parse_result: Option<parser::ParseResult> =
                    if ["xlsx", "xls", "xlsm"].contains(&ext.as_str()) {
                        match parser::excel_parser::parse_excel_file(&path) {
                            Ok(r) => {
                                log::info!("Excel OK: {} rows", r.transactions.len());
                                Some(r)
                            }
                            Err(e) => {
                                log::error!("Excel parse error: {}", e);
                                h.set_status_bank(SharedString::from("Excel parse error — see log"));
                                return;
                            }
                        }
                    } else if ext == "pdf" {
                        // Stage 1: structured row parsing
                        let stage1 = match parser::text_extractor::extract_pages(&path) {
                            Ok(rows) => {
                                if rows.is_empty() { None }
                                else { parser::pdf_parser::parse_pdf_rows(rows, &file_name) }
                            }
                            Err(e) => {
                                let emsg = e.to_string();
                                if emsg.contains("password-protected") || emsg.to_lowercase().contains("encrypt") {
                                    {
                                        let mut st = state_ref.lock().unwrap();
                                        st.pending_pdf_path = Some(path.clone());
                                        st.pending_pdf_name = file_name.clone();
                                    }
                                    h.set_pdf_pwd_visible(true);
                                    h.set_pdf_pwd_prompt(SharedString::from(
                                        format!("'{}' is password-protected. Enter the PDF password:", file_name).as_str(),
                                    ));
                                    h.set_status_bank(SharedString::from("PDF password required\u{2026}"));
                                    return;
                                }
                                log::error!("PDF extract error: {}", emsg);
                                None
                            }
                        };

                        if stage1.is_some() {
                            stage1
                        } else {
                            // Stage 2a: OCR text parsing
                            let full_text = parser::text_extractor::extract_full_text(&path);
                            // When lopdf returns no text, try Tesseract CLI for scanned PDFs.
                            let effective_text = if full_text.trim().is_empty() {
                                h.set_status_bank(SharedString::from("Scanned PDF — trying OCR…"));
                                match parser::ocr_extractor::extract_via_tesseract(&path) {
                                    Some(t) if !t.trim().is_empty() => t,
                                    _ => {
                                        h.set_status_bank(SharedString::from(
                                            "Scanned PDF — install Tesseract for OCR support",
                                        ));
                                        return;
                                    }
                                }
                            } else {
                                full_text.clone()
                            };
                            let full_text = effective_text;

                            let ocr = parser::ocr_parser::parse_ocr_text(&full_text, &file_name);
                            let real_count = ocr.transactions.iter()
                                .filter(|t| !t.is_opening_balance)
                                .count();

                            if real_count > 0 {
                                Some(ocr)
                            } else {
                                // Stage 2b: multiline preprocessor
                                let preprocessed =
                                    parser::ocr_parser::preprocess_multiline(&full_text);
                                if !preprocessed.trim().is_empty() {
                                    let ml = parser::ocr_parser::parse_ocr_text(
                                        &preprocessed, &file_name,
                                    );
                                    let ml_count = ml.transactions.iter()
                                        .filter(|t| !t.is_opening_balance)
                                        .count();
                                    if ml_count > 0 { Some(ml) } else {
                                        h.set_status_bank(SharedString::from(
                                            "No transactions found — PDF may use embedded fonts",
                                        ));
                                        return;
                                    }
                                } else {
                                    h.set_status_bank(SharedString::from(
                                        "No transactions found — PDF may use embedded fonts",
                                    ));
                                    return;
                                }
                            }
                        }
                    } else {
                        log::warn!("Unsupported extension: {}", ext);
                        h.set_status_bank(SharedString::from("Unsupported file type"));
                        return;
                    };

                let result = match parse_result {
                    Some(r) => r,
                    None => {
                        h.set_status_bank(SharedString::from("No transactions found"));
                        return;
                    }
                };

                apply_parse_result(&h, &state_ref, &db_ref, result, &file_name);
            });
        }

        // ── Batch Folder Processing ───────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_batch_folder(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };

                let paths = match rfd::FileDialog::new()
                    .set_title("Select Bank Statement Files (multiple)")
                    .add_filter("Bank Statements", &["pdf","xlsx","xls","xlsm"])
                    .pick_files()
                {
                    Some(p) if !p.is_empty() => p,
                    _ => return,
                };

                let mut all_txns: Vec<parser::Transaction> = {
                    let st = state_ref.lock().unwrap();
                    st.transactions.clone()
                };
                let mut loaded = 0usize;
                let mut skipped = 0usize;
                let mut errors = 0usize;
                let mut first_bank = String::new();
                let mut first_ob: Option<f64> = None;
                let mut new_import_ids: Vec<i64> = vec![];

                for path in &paths {
                    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

                    let result = if ["xlsx","xls","xlsm"].contains(&ext.as_str()) {
                        parser::excel_parser::parse_excel_file(path).ok()
                    } else if ext == "pdf" {
                        let pages = parser::text_extractor::extract_pages(path).ok().unwrap_or_default();
                        if !pages.is_empty() {
                            parser::pdf_parser::parse_pdf_rows(pages, &file_name)
                        } else {
                            let text = parser::text_extractor::extract_full_text(path);
                            if !text.trim().is_empty() {
                                let r = parser::ocr_parser::parse_ocr_text(&text, &file_name);
                                if r.transactions.iter().any(|t| !t.is_opening_balance) { Some(r) } else { None }
                            } else { None }
                        }
                    } else { None };

                    match result {
                        Some(r) if !r.transactions.is_empty() => {
                            let r_bank    = r.bank_name.clone();
                            let r_account = r.account_no.clone();
                            let r_ob      = r.opening_balance;
                            let r_txns    = r.transactions.clone();
                            let r_cnt     = r_txns.iter().filter(|t| !t.is_opening_balance).count();
                            let before = all_txns.len();
                            let existing_hashes: std::collections::HashSet<String> =
                                all_txns.iter().map(|t| t.hash()).collect();
                            let new_txns: Vec<parser::Transaction> = r.transactions.into_iter()
                                .filter(|t| t.is_opening_balance || !existing_hashes.contains(&t.hash()))
                                .collect();
                            skipped += before.saturating_sub(all_txns.len());
                            all_txns.extend(new_txns.clone());
                            loaded += 1;
                            if first_bank.is_empty() { first_bank = r_bank.clone(); }
                            if first_ob.is_none() { first_ob = r_ob; }
                            let db = db_ref.lock().unwrap();
                            if let Some(conn) = db.as_ref() {
                                let client_id = { state_ref.lock().unwrap().client_id.unwrap_or(0) };
                                if client_id > 0 {
                                    if let Ok(iid) = db::save_import(conn, client_id, &file_name, &r_bank, &r_account, r_cnt) {
                                        let _ = db::upsert_transactions(conn, client_id, Some(iid), &new_txns);
                                        new_import_ids.push(iid);
                                    }
                                }
                            }
                        }
                        _ => { errors += 1; log::warn!("[Batch] failed to parse: {:?}", path); }
                    }
                }

                if all_txns.is_empty() { return; }

                // Classify and build model
                let (bank_ledger, client_id2) = {
                    let st = state_ref.lock().unwrap();
                    (st.tally_ledger.clone(), st.client_id.unwrap_or(0))
                };
                let rules = {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        db::get_rules(conn, client_id2).unwrap_or_default()
                    } else { vec![] }
                };
                classifier::classify_all(&mut all_txns, &bank_ledger, &rules);
                classifier::detect_duplicates(&mut all_txns);

                let real: Vec<&parser::Transaction> = all_txns.iter().filter(|t| !t.is_opening_balance).collect();
                let total_dr: f64 = real.iter().filter_map(|t| t.debit).sum();
                let total_cr: f64 = real.iter().filter_map(|t| t.credit).sum();
                let row_models = build_txn_rows(&real);
                let mut bank_set: std::collections::BTreeSet<String> = real.iter().map(|t| t.bank_name.clone()).collect();
                let bank_names: Vec<SharedString> = std::iter::once(SharedString::from("All Banks"))
                    .chain(bank_set.iter().map(|b| SharedString::from(b.as_str())))
                    .collect();

                h.set_transaction_rows(slint::ModelRc::new(slint::VecModel::from(row_models)));
                h.set_bank_names(slint::ModelRc::new(slint::VecModel::from(bank_names)));
                h.set_dash_credits(SharedString::from(ui::AppState::fmt_amount(Some(total_cr)).as_str()));
                h.set_dash_debits(SharedString::from(ui::AppState::fmt_amount(Some(total_dr)).as_str()));
                h.set_dash_txn_count(SharedString::from(real.len().to_string().as_str()));
                h.set_status_bank(SharedString::from(first_bank.as_str()));
                h.set_status_file(SharedString::from(format!("{} file(s)", loaded).as_str()));
                h.set_fc_all(SharedString::from(real.len().to_string().as_str()));

                {
                    let mut st = state_ref.lock().unwrap();
                    st.transactions    = all_txns.clone();
                    st.opening_balance = first_ob;
                    st.closing_balance = None;
                    st.total_debits    = total_dr;
                    st.total_credits   = total_cr;
                    st.txn_count       = real.len();
                    st.active_filter   = "all".to_string();
                    st.date_from       = String::new();
                    st.date_to         = String::new();
                    st.bank_filter     = String::new();
                    st.import_ids.extend(new_import_ids.iter());
                }

                push_dashboard(&h, &all_txns, first_ob);
                let batch_event = format!("[{}] Import — {} file(s), {} transactions loaded", audit_now(), loaded, real.len());
                {
                    let mut st = state_ref.lock().unwrap();
                    st.audit_events.push(batch_event.clone());
                }
                if client_id2 > 0 {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        let _ = db::push_audit_event(conn, client_id2, &batch_event);
                    }
                }
                log::info!("[Batch] loaded={} skipped={} errors={} total_txns={}", loaded, skipped, errors, real.len());
            });
        }
        // ── New Client ────────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_new_client(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                // Read the fields that were synced via the in-out properties
                let name   = h.get_new_client_name().to_string();
                let ledger = h.get_new_client_ledger().to_string();
                if name.trim().is_empty() {
                    log::warn!("[NewClient] name is empty — skipping");
                    return;
                }
                // Write to DB
                let new_id = {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        match db::add_client(conn, name.trim(), ledger.trim()) {
                            Ok(id) => { log::info!("[NewClient] created id={} name='{}' ledger='{}'", id, name, ledger); Some(id) }
                            Err(e) => { log::error!("[NewClient] DB error: {}", e); None }
                        }
                    } else { None }
                };
                // Update AppState and sync dashboard properties so Edit Client modal pre-fills
                if let Some(id) = new_id {
                    let mut st = state_ref.lock().unwrap();
                    st.client_id     = Some(id);
                    st.client_name   = name.trim().to_string();
                    st.tally_ledger  = ledger.trim().to_string();
                    drop(st);
                    h.set_dash_client_name(SharedString::from(name.trim()));
                    h.set_dash_client_ledger(SharedString::from(ledger.trim()));
                }
                // Refresh client dropdown
                {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        if let Ok(clients) = db::get_clients(conn) {
                            let names: Vec<SharedString> =
                                std::iter::once(SharedString::from("-- Select Client --"))
                                .chain(clients.iter().map(|c| SharedString::from(c.name.as_str())))
                                .collect();
                            h.set_client_names(slint::ModelRc::new(slint::VecModel::from(names)));
                        }
                    }
                }
            });
        }

        // ── Auto-Classify All ─────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_auto_classify(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();
                if st.transactions.is_empty() {
                    log::info!("[AutoClassify] No transactions loaded");
                    return;
                }
                let bank_ledger = st.tally_ledger.clone();
                let client_id   = st.client_id.unwrap_or(0);
                // Load stored rules from DB
                let rules = {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        db::get_rules(conn, client_id).unwrap_or_default()
                    } else { vec![] }
                };
                let changed = classifier::classify_all(&mut st.transactions, &bank_ledger, &rules);
                log::info!("[AutoClassify] classified {} transactions (rules={})", changed, rules.len());
                rebuild_rows(&h, &st);
                push_dashboard(&h, &st.transactions, st.opening_balance);
                let total_dr: f64 = st.transactions.iter().filter_map(|t| t.debit).sum();
                let total_cr: f64 = st.transactions.iter().filter_map(|t| t.credit).sum();
                h.set_dash_credits(SharedString::from(ui::AppState::fmt_amount(Some(total_cr)).as_str()));
                h.set_dash_debits(SharedString::from(ui::AppState::fmt_amount(Some(total_dr)).as_str()));
            });
        }

        // ── AI Classify ───────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_ai_classify(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                {
                    let st = state_ref.lock().unwrap();
                    if st.transactions.is_empty() {
                        log::info!("[AIClassify] No transactions loaded");
                        return;
                    }
                }
                #[cfg(feature = "ai")]
                {
                    let provider_idx = h.get_ai_provider_idx();
                    let api_key      = h.get_ai_api_key().to_string();
                    let provider     = ai_classifier::AiProvider::from_idx(provider_idx);
                    let handle2      = h.as_weak();
                    let state_ref2   = state_ref.clone();
                    h.set_ai_overlay_visible(true);
                    h.set_ai_msg("Classifying with AI…".into());
                    h.set_ai_pct(0);
                    std::thread::spawn(move || {
                        let mut txns = { state_ref2.lock().unwrap().transactions.clone() };
                        let result = ai_classifier::classify_with_ai(
                            &mut txns,
                            provider,
                            &api_key,
                            |done, total| {
                                let pct = if total > 0 { (done * 100 / total) as i32 } else { 0 };
                                let msg = format!("AI: {}/{}", done, total);
                                let handle2_c = handle2.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(h2) = handle2_c.upgrade() {
                                        h2.set_ai_pct(pct);
                                        h2.set_ai_msg(SharedString::from(msg.as_str()));
                                    }
                                });
                            },
                        );
                        match result {
                            Ok(n) => log::info!("[AIClassify] classified {} transactions", n),
                            Err(e) => log::error!("[AIClassify] error: {}", e),
                        }
                        let handle3 = handle2.clone();
                        let state_ref3 = state_ref2.clone();
                        let txns_done = txns;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(h2) = handle3.upgrade() {
                                let mut st = state_ref3.lock().unwrap();
                                st.transactions = txns_done;
                                h2.set_ai_overlay_visible(false);
                                rebuild_rows(&h2, &st);
                                push_dashboard(&h2, &st.transactions, st.opening_balance);
                            }
                        });
                    });
                }
                #[cfg(not(feature = "ai"))]
                {
                    let _ = h;
                    log::info!("[AIClassify] built without ai feature");
                }
            });
        }
        // ── View Rules ────────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_view_rules(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let st = state_ref.lock().unwrap();
                let client_id = st.client_id.unwrap_or(0);
                drop(st);
                let db = db_ref.lock().unwrap();
                if let Some(conn) = db.as_ref() {
                    match db::get_rules(conn, client_id) {
                        Ok(rules) => {
                            // Format: "PATTERN  |  VENDOR  |  HEAD  |  TYPE"
                            let recs: Vec<SharedString> = rules.iter().map(|r| {
                                SharedString::from(format!(
                                    "{}  |  {}  |  {}  |  {}",
                                    r.pattern,
                                    if r.vendor.is_empty() { "—" } else { &r.vendor },
                                    if r.account_head.is_empty() { "—" } else { &r.account_head },
                                    if r.txn_type.is_empty() { "—" } else { &r.txn_type },
                                ).as_str())
                            }).collect();
                            h.set_rule_records(slint::ModelRc::new(slint::VecModel::from(recs)));
                            log::info!("[ViewRules] loaded {} rules for client_id={}", rules.len(), client_id);
                        }
                        Err(e) => log::error!("[ViewRules] DB error: {}", e),
                    }
                }
            });
        }

        // ── Import History ─────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_import_history(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let st = state_ref.lock().unwrap();
                let client_id = st.client_id.unwrap_or(0);
                drop(st);
                let db = db_ref.lock().unwrap();
                if let Some(conn) = db.as_ref() {
                    match db::get_imports(conn, client_id) {
                        Ok(imports) => {
                            // Format: "FILENAME  |  DATE  |  TXNS rows"
                            let recs: Vec<SharedString> = imports.iter().map(|imp| {
                                // imported_at is ISO datetime e.g. "2024-06-05 14:32:00"
                                let date = &imp.imported_at[..imp.imported_at.len().min(16)];
                                SharedString::from(format!(
                                    "{}  |  {}  |  {} transactions",
                                    imp.file_name, date, imp.txn_count
                                ).as_str())
                            }).collect();
                            h.set_import_records(slint::ModelRc::new(slint::VecModel::from(recs)));
                            log::info!("[ImportHistory] loaded {} records for client_id={}", imports.len(), client_id);
                        }
                        Err(e) => log::error!("[ImportHistory] DB error: {}", e),
                    }
                }
            });
        }
        {
            app.on_do_import_ledgers(|| stub_callback("import-ledgers"));
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_filter_changed(move |f| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();
                st.active_filter = f.to_string();
                rebuild_rows(&h, &st);
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_date_filter_apply(move |from, to| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();
                st.date_from = from.to_string();
                st.date_to   = to.to_string();
                rebuild_rows(&h, &st);
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_date_preset(move |preset| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();
                let (from, to) = preset_range(preset.as_str());
                st.date_from = from;
                st.date_to   = to;
                rebuild_rows(&h, &st);
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_bank_filter(move |bank| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();
                st.bank_filter = if bank == "All Banks" { String::new() } else { bank.to_string() };
                rebuild_rows(&h, &st);
            });
        }
        // ── Export Excel (XLSX primary, CSV fallback) ─────────────────────────
        {
            let state_ref = app_state.clone();
            app.on_do_export_excel(move || {
                let st = state_ref.lock().unwrap();
                if st.transactions.is_empty() {
                    log::warn!("[ExportExcel] No transactions to export");
                    return;
                }
                let client_name = if st.client_name.is_empty() { "Export".to_string() } else { st.client_name.clone() };
                let suggested = format!("BankStatement_{}.xlsx", client_name.replace(' ', "_"));
                let path = match rfd::FileDialog::new()
                    .set_title("Save Export")
                    .set_file_name(&suggested)
                    .add_filter("Excel Workbook", &["xlsx"])
                    .add_filter("CSV (Excel)", &["csv"])
                    .save_file()
                {
                    Some(p) => p,
                    None    => return,
                };
                let ext = path.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if ext == "csv" {
                    match export::excel::export_csv(
                        &st.transactions, &client_name, &st.tally_ledger,
                        &st.file_name, st.opening_balance, st.closing_balance, &path,
                    ) {
                        Ok(n)  => log::info!("[ExportExcel] CSV: {} rows → {:?}", n, path),
                        Err(e) => log::error!("[ExportExcel] CSV failed: {}", e),
                    }
                } else {
                    match export::excel::export_xlsx(
                        &st.transactions, &client_name, &st.tally_ledger,
                        &st.file_name, st.opening_balance, st.closing_balance, &path,
                    ) {
                        Ok(n)  => log::info!("[ExportExcel] XLSX: {} rows → {:?}", n, path),
                        Err(e) => log::error!("[ExportExcel] XLSX failed: {}", e),
                    }
                }
            });
        }

        // ── Export Tally XML ──────────────────────────────────────────────────
        {
            let state_ref = app_state.clone();
            app.on_do_export_tally(move || {
                let st = state_ref.lock().unwrap();
                if st.transactions.is_empty() {
                    log::warn!("[ExportTally] No transactions to export");
                    return;
                }
                let client_name = st.client_name.clone();
                let opts = export::tally::TallyOpts {
                    company:            client_name.clone(),
                    gstin:              String::new(),
                    fy:                 String::new(),
                    bank_ledger:        st.tally_ledger.clone(),
                    date_from:          if st.date_from.is_empty() { None } else { Some(st.date_from.clone()) },
                    date_to:            if st.date_to.is_empty()   { None } else { Some(st.date_to.clone()) },
                    only_classified:    true,
                    include_ledgers:    true,
                    include_narrations: true,
                    include_ob:         false,
                    skip_low_conf:      false,
                };
                let xml = export::tally::generate(&st.transactions, &opts, st.opening_balance);
                let suggested = format!("TallyExport_{}.xml", client_name.replace(' ', "_"));
                let path = match rfd::FileDialog::new()
                    .set_title("Save Tally XML")
                    .set_file_name(&suggested)
                    .add_filter("Tally XML", &["xml"])
                    .save_file()
                {
                    Some(p) => p,
                    None    => return,
                };
                match std::fs::write(&path, xml.as_bytes()) {
                    Ok(_) => log::info!("[ExportTally] wrote {:?}", path),
                    Err(e) => log::error!("[ExportTally] write failed: {}", e),
                }
            });
        }

        // ── Accounting Export Wizard ──────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_export_accounting(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let st = state_ref.lock().unwrap();
                if st.transactions.is_empty() {
                    log::warn!("[ExportAccounting] No transactions to export");
                    return;
                }
                // Wizard fields are now exposed on AppWindow via in-out bindings;
                // read them directly from the handle.
                let sw_idx  = h.get_wiz_sw_idx();
                let company = h.get_wiz_company().to_string();
                let gstin   = h.get_wiz_gstin().to_string();
                let from    = h.get_wiz_date_from().to_string();
                let to      = h.get_wiz_date_to().to_string();

                let software = export::accounting::Software::from_idx(sw_idx);
                let opts = export::accounting::AccountingOpts {
                    software,
                    company:            if company.is_empty() { st.client_name.clone() } else { company },
                    gstin,
                    fy:                 String::new(),
                    state_code:         "MH".to_string(),
                    currency:           "INR".to_string(),
                    bank_ledger:        st.tally_ledger.clone(),
                    date_from:          if from.is_empty() { None } else { Some(from) },
                    date_to:            if to.is_empty()   { None } else { Some(to) },
                    include_ob:         false,
                    include_gst:        true,
                    include_ledgers:    true,
                    include_narrations: true,
                    only_classified:    true,
                    skip_low_conf:      false,
                };
                let content  = export::accounting::generate(&st.transactions, &opts, st.opening_balance);
                let ext      = software.ext();
                let label    = software.label();
                let suggested = format!("AccountingExport_{}.{}", st.client_name.replace(' ', "_"), ext);
                let path = match rfd::FileDialog::new()
                    .set_title(&format!("Save {} Export", label))
                    .set_file_name(&suggested)
                    .add_filter(label, &[ext])
                    .save_file()
                {
                    Some(p) => p,
                    None    => return,
                };
                match std::fs::write(&path, content.as_bytes()) {
                    Ok(_)  => log::info!("[ExportAccounting] {} written to {:?}", label, path),
                    Err(e) => log::error!("[ExportAccounting] write failed: {}", e),
                }
            });
        }

        {
            app.on_do_reimport_excel(|| stub_callback("reimport-excel"));
        }

        // ── Reset Dedupe ──────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_reset_dedupe(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();
                if st.transactions.is_empty() { return; }
                for t in st.transactions.iter_mut() {
                    t.dup_flag = false;
                    t.tags.retain(|tag| tag != "DUP");
                }
                classifier::detect_duplicates(&mut st.transactions);
                let dup_count  = st.transactions.iter().filter(|t| t.dup_flag).count();
                let event_str  = format!("[{}] Reset Dedupe — {} duplicate(s) redetected", audit_now(), dup_count);
                let client_id  = st.client_id;
                let txns_snap  = st.transactions.clone();
                st.audit_events.push(event_str.clone());
                rebuild_rows(&h, &st);
                h.set_status_bank(SharedString::from(
                    format!("Dedupe reset — {} duplicate(s) detected", dup_count).as_str()
                ));
                log::info!("[ResetDedupe] {} duplicates after reset", dup_count);
                drop(st);
                if let Some(cid) = client_id {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        let _ = db::update_dup_flags(conn, &txns_snap);
                        let _ = db::push_audit_event(conn, cid, &event_str);
                    }
                }
            });
        }

        // ── Reconcile ─────────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_reconcile(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                // Pick a Tally daybook export (Excel)
                let path = match rfd::FileDialog::new()
                    .set_title("Open Tally Daybook Export (Excel)")
                    .add_filter("Excel", &["xlsx", "xls", "xlsm"])
                    .pick_file()
                { Some(p) => p, None => return };

                let wb = match calamine::open_workbook_auto(&path) {
                    Ok(w) => w, Err(e) => {
                        log::error!("[Reconcile] cannot open file: {}", e);
                        h.set_status_bank(SharedString::from("Reconcile: failed to open Excel file"));
                        return;
                    }
                };

                // Parse Tally rows: extract (date_str, amount) pairs
                let tally_entries: Vec<(String, f64)> = reconcile_parse_tally(wb);
                if tally_entries.is_empty() {
                    h.set_recon_status(SharedString::from("No valid entries found in the Tally export. Ensure the file has Date and Debit/Credit columns."));
                    return;
                }

                // Compare against bank transactions
                let st = state_ref.lock().unwrap();
                let bank: Vec<(String, f64)> = st.transactions.iter()
                    .filter(|t| !t.is_opening_balance)
                    .map(|t| (t.date.clone(), t.debit.or(t.credit).unwrap_or(0.0)))
                    .collect();

                // Load recon tolerance from settings
                let (recon_days, recon_pct) = {
                    let dbc = db_ref.lock().unwrap();
                    if let Some(c) = dbc.as_ref() {
                        let cfg = settings::Settings::load(c);
                        (cfg.recon_days as i64, cfg.recon_pct)
                    } else { (3, 0.5) }
                };

                let (matched, likely, unmatched_tally, tally_status, bank_used) =
                    reconcile_match(&tally_entries, &bank, recon_days, recon_pct);
                let bank_only = bank.len().saturating_sub(matched + likely);
                let status = format!(
                    "Tally: {} entries | Bank: {} transactions\n\u{2022} Exact matches: {}\n\u{2022} Likely matches (\u{00B1}{} days): {}\n\u{2022} Tally entries unmatched: {}\n\u{2022} Bank-only entries (no Tally): {}",
                    tally_entries.len(), bank.len(), matched, recon_days, likely, unmatched_tally, bank_only
                );

                // Build CSV for later export
                let mut csv = String::from("Source,Date,Amount,Status\n");
                for (i, (td, ta)) in tally_entries.iter().enumerate() {
                    csv.push_str(&format!("Tally,{},{:.2},{}\n", td, ta, tally_status[i]));
                }
                for (i, (bd, ba)) in bank.iter().enumerate() {
                    let s = if bank_used[i] { "Matched" } else { "Bank-only" };
                    csv.push_str(&format!("Bank,{},{:.2},{}\n", bd, ba, s));
                }

                drop(st);
                h.set_recon_matched(SharedString::from(matched.to_string().as_str()));
                h.set_recon_likely(SharedString::from(likely.to_string().as_str()));
                h.set_recon_unmatched(SharedString::from(unmatched_tally.to_string().as_str()));
                h.set_recon_bank_only(SharedString::from(bank_only.to_string().as_str()));
                h.set_recon_status(SharedString::from(status.as_str()));

                let event_str = format!("[{}] Reconcile — {} matched, {} likely, {} unmatched", audit_now(), matched, likely, unmatched_tally);
                let client_id = {
                    let mut st2 = state_ref.lock().unwrap();
                    st2.audit_events.push(event_str.clone());
                    st2.recon_csv = csv;
                    st2.client_id
                };
                log::info!("[Reconcile] matched={} likely={} unmatched={} bank_only={}", matched, likely, unmatched_tally, bank_only);
                if let Some(cid) = client_id {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        let _ = db::push_audit_event(conn, cid, &event_str);
                    }
                }
            });
        }

        // ── Batch Monitor ─────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_batch_monitor(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let st = state_ref.lock().unwrap();
                let import_count = st.import_ids.len();
                let txn_count = st.transactions.iter().filter(|t| !t.is_opening_balance).count();
                let classified = st.transactions.iter()
                    .filter(|t| matches!(t.status, parser::TransactionStatus::Classified)).count();
                let unreviewed = st.transactions.iter()
                    .filter(|t| matches!(t.status, parser::TransactionStatus::Unreviewed)).count();
                let log_msg = if txn_count == 0 {
                    "No files loaded yet. Use \"Batch Process Folder\" in the toolbar to load files.".to_string()
                } else {
                    format!(
                        "Session summary:\n\u{2022} {} import(s) in this session\n\u{2022} {} transactions loaded\n\u{2022} {} classified  |  {} unreviewed",
                        import_count, txn_count, classified, unreviewed
                    )
                };
                h.set_batch_log(SharedString::from(log_msg.as_str()));
                log::info!("[BatchMonitor] imports={} txns={} classified={}", import_count, txn_count, classified);
            });
        }

        // ── Load All from DB ──────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_load_all(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let client_id = state_ref.lock().unwrap().client_id;
                let Some(cid) = client_id else {
                    h.set_batch_log(SharedString::from(
                        "No client selected. Select a client first."
                    ));
                    return;
                };
                let txns = {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        db::get_transactions(conn, cid).unwrap_or_default()
                    } else { vec![] }
                };
                let opening_bal = txns.iter().find(|t| t.is_opening_balance).and_then(|t| t.balance);
                let txn_count   = txns.iter().filter(|t| !t.is_opening_balance).count();
                {
                    let mut st     = state_ref.lock().unwrap();
                    st.transactions    = txns.clone();
                    st.opening_balance = opening_bal;
                    st.active_filter   = "all".to_string();
                    st.date_from       = String::new();
                    st.date_to         = String::new();
                    st.bank_filter     = String::new();
                }
                let st = state_ref.lock().unwrap();
                rebuild_rows(&h, &st);
                drop(st);
                if !txns.is_empty() {
                    push_dashboard(&h, &txns, opening_bal);
                }
                let msg = format!("Loaded {} transactions from database for current client.", txn_count);
                h.set_batch_log(SharedString::from(msg.as_str()));
                h.set_status_bank(SharedString::from(
                    format!("All transactions loaded — {} total", txn_count).as_str()
                ));
                log::info!("[LoadAll] cid={} txns={}", cid, txn_count);
            });
        }

        // ── Audit Trail ───────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_audit_trail(move |action_idx: i32, type_idx: i32| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let client_id = state_ref.lock().unwrap().client_id;

                // Load from DB when a client is active, else fall back to in-memory
                let all_events: Vec<String> = if let Some(cid) = client_id {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        db::get_audit_events(conn, cid).unwrap_or_default()
                    } else {
                        state_ref.lock().unwrap().audit_events.iter().rev().cloned().collect()
                    }
                } else {
                    state_ref.lock().unwrap().audit_events.iter().rev().cloned().collect()
                };

                let filtered: Vec<SharedString> = all_events.iter()
                    .filter(|e| {
                        let u = e.to_uppercase();
                        let action_ok = match action_idx {
                            1 => u.contains("ADD") || u.contains("CREATE") || u.contains("MANUAL"),
                            2 => u.contains("EDIT") || u.contains("RESET") || u.contains("SAVE") || u.contains("CLASSIFY"),
                            3 => u.contains("DELETE"),
                            4 => u.contains("IMPORT") || u.contains("BATCH"),
                            5 => u.contains("EXPORT"),
                            6 => u.contains("CLASSIFY"),
                            7 => u.contains("RULE"),
                            _ => true,
                        };
                        let type_ok = match type_idx {
                            1 => u.contains("ADD") || u.contains("DEDUPE") || u.contains("RECONCILE") || u.contains("MANUAL"),
                            2 => u.contains("CLIENT"),
                            3 => u.contains("RULE"),
                            4 => u.contains("IMPORT") || u.contains("BATCH"),
                            5 => u.contains("EXPORT"),
                            _ => true,
                        };
                        action_ok && type_ok
                    })
                    .map(|e| SharedString::from(e.as_str()))
                    .collect();

                h.set_audit_records(slint::ModelRc::new(slint::VecModel::from(filtered)));
                log::info!("[AuditTrail] action_idx={} type_idx={} total_events={}", action_idx, type_idx, all_events.len());
            });
        }

        // ── Export Reconciliation CSV ─────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_export_recon_csv(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let csv = state_ref.lock().unwrap().recon_csv.clone();
                if csv.is_empty() {
                    h.set_status_bank(SharedString::from("Run a reconciliation first before exporting."));
                    return;
                }
                let path = match rfd::FileDialog::new()
                    .set_title("Save Reconciliation CSV")
                    .set_file_name("Reconciliation.csv")
                    .add_filter("CSV", &["csv"])
                    .save_file()
                {
                    Some(p) => p,
                    None    => return,
                };
                match std::fs::write(&path, csv.as_bytes()) {
                    Ok(_)  => {
                        h.set_status_bank(SharedString::from("Reconciliation CSV exported."));
                        log::info!("[ExportReconCSV] written → {:?}", path);
                    }
                    Err(e) => {
                        h.set_status_bank(SharedString::from("Export failed — check logs."));
                        log::error!("[ExportReconCSV] write failed: {}", e);
                    }
                }
            });
        }

        // ── Download Audit Logs ────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_download_logs(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let client_id = state_ref.lock().unwrap().client_id;
                let events: Vec<String> = if let Some(cid) = client_id {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        db::get_audit_events(conn, cid).unwrap_or_default()
                    } else {
                        state_ref.lock().unwrap().audit_events.iter().rev().cloned().collect()
                    }
                } else {
                    state_ref.lock().unwrap().audit_events.iter().rev().cloned().collect()
                };
                if events.is_empty() {
                    h.set_status_bank(SharedString::from("No audit events to export."));
                    return;
                }
                let path = match rfd::FileDialog::new()
                    .set_title("Save Audit Log")
                    .set_file_name("AuditLog.txt")
                    .add_filter("Text file", &["txt"])
                    .save_file()
                {
                    Some(p) => p,
                    None    => return,
                };
                let content = events.join("\n");
                match std::fs::write(&path, content.as_bytes()) {
                    Ok(_)  => {
                        h.set_status_bank(SharedString::from(
                            format!("Audit log exported — {} events.", events.len()).as_str()
                        ));
                        log::info!("[DownloadLogs] {} events written → {:?}", events.len(), path);
                    }
                    Err(e) => {
                        h.set_status_bank(SharedString::from("Export failed — check logs."));
                        log::error!("[DownloadLogs] write failed: {}", e);
                    }
                }
            });
        }

        // ── Settings ──────────────────────────────────────────────────────────
        {
            let db_ref    = db_conn.clone();
            let state_ref = app_state.clone();
            app.on_do_settings(move || {
                let db = db_ref.lock().unwrap();
                if let Some(conn) = db.as_ref() {
                    let cfg = settings::Settings::load(conn);
                    let mut st = state_ref.lock().unwrap();
                    st.ai_provider = cfg.ai_provider.clone();
                    st.ai_api_key  = cfg.ai_api_key.clone();
                    st.ai_enabled  = cfg.ai_enabled;
                    log::info!("[Settings] loaded: provider='{}' enabled={}", cfg.ai_provider, cfg.ai_enabled);
                    drop(db); drop(st);
                }
            });
        }

        // ── Settings Save (AI modal) ──────────────────────────────────────────
        {
            let db_ref    = db_conn.clone();
            let state_ref = app_state.clone();
            app.on_do_settings_save(move |provider: SharedString, key: SharedString, enabled: bool| {
                let db = db_ref.lock().unwrap();
                if let Some(conn) = db.as_ref() {
                    let provider_str = match provider.as_str() {
                        "1" => "claude",
                        "2" => "gemini",
                        _   => "openai",
                    };
                    let mut cfg = settings::Settings::load(conn);
                    cfg.ai_provider = provider_str.to_string();
                    cfg.ai_api_key  = key.to_string();
                    cfg.ai_enabled  = enabled;
                    match cfg.save(conn) {
                        Ok(_)  => log::info!("[SettingsSave] provider='{}' enabled={}", provider_str, enabled),
                        Err(e) => log::error!("[SettingsSave] error: {}", e),
                    }
                    let mut st = state_ref.lock().unwrap();
                    st.ai_provider = provider_str.to_string();
                    st.ai_api_key  = key.to_string();
                    st.ai_enabled  = enabled;
                }
            });
        }

        // ── Settings Save All (Application Settings modal) ────────────────────
        {
            let handle    = app.as_weak();
            let db_ref    = db_conn.clone();
            app.on_do_settings_save_all(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let db = db_ref.lock().unwrap();
                if let Some(conn) = db.as_ref() {
                    let mut cfg = settings::Settings::load(conn);
                    cfg.narr_enabled     = h.get_settings_narr_enabled();
                    cfg.narr_title_case  = h.get_settings_narr_title_case();
                    cfg.narr_preserve    = h.get_settings_narr_preserve();
                    cfg.gst_enabled      = h.get_settings_gst_enabled();
                    cfg.gst_auto_ledgers = h.get_settings_gst_auto_ledgers();
                    cfg.recon_days       = h.get_settings_recon_days().parse::<i32>().unwrap_or(3);
                    cfg.recon_pct        = h.get_settings_recon_pct().parse::<f64>().unwrap_or(0.5);
                    let log_idx = h.get_settings_log_level();
                    cfg.log_level = match log_idx {
                        1 => "DEBUG",
                        2 => "WARN",
                        3 => "ERROR",
                        _ => "INFO",
                    }.to_string();
                    match cfg.save(conn) {
                        Ok(_)  => log::info!("[SettingsSaveAll] saved recon_days={} recon_pct={}", cfg.recon_days, cfg.recon_pct),
                        Err(e) => log::error!("[SettingsSaveAll] error: {}", e),
                    }
                }
            });
        }

        // ── Clear Logs ────────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let db_ref    = db_conn.clone();
            let state_ref = app_state.clone();
            app.on_do_clear_logs(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let db = db_ref.lock().unwrap();
                if let Some(conn) = db.as_ref() {
                    let client_id = state_ref.lock().unwrap().client_id;
                    let result = if let Some(cid) = client_id {
                        db::clear_audit_events(conn, cid)
                    } else {
                        db::clear_all_audit_events(conn)
                    };
                    match result {
                        Ok(_)  => {
                            log::info!("[ClearLogs] cleared");
                            h.set_toast_msg(slint::SharedString::from("Logs cleared"));
                            h.set_toast_kind(1);
                        }
                        Err(e) => log::error!("[ClearLogs] error: {}", e),
                    }
                }
            });
        }

        // ── Backup Rules ──────────────────────────────────────────────────────
        {
            let db_ref    = db_conn.clone();
            let state_ref = app_state.clone();
            let handle    = app.as_weak();
            app.on_do_backup_rules(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let client_id = state_ref.lock().unwrap().client_id;
                let Some(cid) = client_id else {
                    h.set_toast_msg(slint::SharedString::from("Select a client first"));
                    h.set_toast_kind(2);
                    return;
                };
                let db = db_ref.lock().unwrap();
                if let Some(conn) = db.as_ref() {
                    match db::export_rules_json(conn, cid) {
                        Ok(json) => {
                            let path = format!("rules_backup_{}.json", cid);
                            match std::fs::write(&path, json) {
                                Ok(_) => {
                                    log::info!("[BackupRules] saved to {}", path);
                                    h.set_toast_msg(slint::SharedString::from(format!("Rules backed up to {}", path).as_str()));
                                    h.set_toast_kind(1);
                                }
                                Err(e) => {
                                    log::error!("[BackupRules] write error: {}", e);
                                    h.set_toast_msg(slint::SharedString::from("Backup write failed"));
                                    h.set_toast_kind(2);
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("[BackupRules] export error: {}", e);
                            h.set_toast_msg(slint::SharedString::from("Rules export failed"));
                            h.set_toast_kind(2);
                        }
                    }
                }
            });
        }

        // ── Restore Rules ─────────────────────────────────────────────────────
        {
            let db_ref    = db_conn.clone();
            let state_ref = app_state.clone();
            let handle    = app.as_weak();
            app.on_do_restore_rules(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let client_id = state_ref.lock().unwrap().client_id;
                let Some(cid) = client_id else {
                    h.set_toast_msg(slint::SharedString::from("Select a client first"));
                    h.set_toast_kind(2);
                    return;
                };
                let path = rfd::FileDialog::new()
                    .set_title("Select Rules Backup JSON")
                    .add_filter("JSON", &["json"])
                    .pick_file();
                let Some(path) = path else { return; };
                match std::fs::read_to_string(&path) {
                    Ok(json) => {
                        let db = db_ref.lock().unwrap();
                        if let Some(conn) = db.as_ref() {
                            match db::import_rules_json(conn, cid, &json) {
                                Ok(n) => {
                                    log::info!("[RestoreRules] imported {} rules", n);
                                    h.set_toast_msg(slint::SharedString::from(format!("{} rules restored", n).as_str()));
                                    h.set_toast_kind(1);
                                }
                                Err(e) => {
                                    log::error!("[RestoreRules] error: {}", e);
                                    h.set_toast_msg(slint::SharedString::from("Rules restore failed"));
                                    h.set_toast_kind(2);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("[RestoreRules] read error: {}", e);
                        h.set_toast_msg(slint::SharedString::from("Could not read backup file"));
                        h.set_toast_kind(2);
                    }
                }
            });
        }

        // ── Select Client ─────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_select_client(move |name: SharedString| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let name_str = name.to_string();
                if name_str.is_empty() || name_str == "-- Select Client --" { return; }

                let client = {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        db::get_client_by_name(conn, &name_str).ok().flatten()
                    } else { None }
                };

                let Some(client) = client else {
                    log::warn!("[SelectClient] client '{}' not found in DB", name_str);
                    return;
                };

                // Load transactions from DB for this client
                let txns = {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        db::get_transactions(conn, client.id).unwrap_or_default()
                    } else { vec![] }
                };

                // Load AI settings
                let cfg = {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        settings::Settings::load(conn)
                    } else { settings::Settings::default() }
                };

                let opening_bal = txns.iter()
                    .find(|t| t.is_opening_balance)
                    .and_then(|t| t.balance);

                // Load persisted audit events for this client
                let audit_events = {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        db::get_audit_events(conn, client.id).unwrap_or_default()
                    } else { vec![] }
                };

                {
                    let mut st = state_ref.lock().unwrap();
                    st.client_id    = Some(client.id);
                    st.client_name  = client.name.clone();
                    st.tally_ledger = client.tally_ledger.clone();
                    st.transactions  = txns.clone();
                    st.opening_balance = opening_bal;
                    st.active_filter = "all".to_string();
                    st.date_from     = String::new();
                    st.date_to       = String::new();
                    st.bank_filter   = String::new();
                    st.ai_provider   = cfg.ai_provider.clone();
                    st.ai_api_key    = cfg.ai_api_key.clone();
                    st.ai_enabled    = cfg.ai_enabled;
                    // audit_events from DB are already newest-first; store them reversed so
                    // in-memory order matches the push-then-rev pattern used elsewhere
                    st.audit_events  = audit_events.into_iter().rev().collect();
                }

                // Sync AI settings to UI
                h.set_ai_provider_idx(match cfg.ai_provider.as_str() {
                    "claude" => 1, "gemini" => 2, _ => 0,
                });
                h.set_ai_api_key(SharedString::from(cfg.ai_api_key.as_str()));

                // Load import history
                let imports = {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        db::get_imports(conn, client.id).unwrap_or_default()
                    } else { vec![] }
                };
                let import_recs: Vec<SharedString> = imports.iter().map(|imp| {
                    let date = &imp.imported_at[..imp.imported_at.len().min(16)];
                    SharedString::from(format!("{}  |  {}  |  {} transactions",
                        imp.file_name, date, imp.txn_count).as_str())
                }).collect();
                h.set_import_records(slint::ModelRc::new(slint::VecModel::from(import_recs)));
                {
                    let mut st = state_ref.lock().unwrap();
                    st.import_ids = imports.iter().map(|i| i.id).collect();
                }

                // Update UI summary
                let st = state_ref.lock().unwrap();
                h.set_dash_client_name(SharedString::from(client.name.as_str()));
                h.set_dash_client_ledger(SharedString::from(client.tally_ledger.as_str()));
                h.set_status_bank(SharedString::from(
                    if txns.is_empty() { "No transactions — load a file to begin" }
                    else { "Loaded from database" }
                ));
                rebuild_rows(&h, &st);
                if !txns.is_empty() {
                    push_dashboard(&h, &st.transactions, st.opening_balance);
                }
                drop(st);

                // Persist last used client
                let db = db_ref.lock().unwrap();
                if let Some(conn) = db.as_ref() {
                    let _ = db::set_setting(conn, settings::KEY_LAST_CLIENT, &client.id.to_string());
                }

                log::info!("[SelectClient] loaded client '{}' id={} txns={}", name_str, client.id, txns.len());
            });
        }

        // ── Delete Client ─────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_delete_client(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let client_id = state_ref.lock().unwrap().client_id;
                let Some(cid) = client_id else {
                    log::warn!("[DeleteClient] no client selected");
                    return;
                };
                let db = db_ref.lock().unwrap();
                if let Some(conn) = db.as_ref() {
                    let _ = db::delete_client(conn, cid);
                    // Cascade deletes transactions/imports/rules via FK ON DELETE CASCADE
                    log::info!("[DeleteClient] deleted client id={}", cid);
                }
                drop(db);
                // Clear state
                {
                    let mut st = state_ref.lock().unwrap();
                    st.client_id    = None;
                    st.client_name  = String::new();
                    st.tally_ledger = String::new();
                    st.transactions  = vec![];
                    st.import_ids    = vec![];
                }
                // Refresh dropdown
                {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        if let Ok(clients) = db::get_clients(conn) {
                            let names: Vec<SharedString> =
                                std::iter::once(SharedString::from("-- Select Client --"))
                                .chain(clients.iter().map(|c| SharedString::from(c.name.as_str())))
                                .collect();
                            h.set_client_names(slint::ModelRc::new(slint::VecModel::from(names)));
                        }
                    }
                }
                h.set_transaction_rows(slint::ModelRc::new(slint::VecModel::from(Vec::<TxnRow>::new())));
                h.set_import_records(slint::ModelRc::new(slint::VecModel::from(Vec::<SharedString>::new())));
                h.set_status_bank(SharedString::from("Client deleted"));
            });
        }

        // ── Edit Client ───────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_edit_client(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let cid = state_ref.lock().unwrap().client_id;
                let Some(cid) = cid else {
                    log::warn!("[EditClient] no client selected");
                    return;
                };
                let new_name   = h.get_edit_client_name().to_string();
                let new_ledger = h.get_edit_client_ledger().to_string();
                if new_name.trim().is_empty() {
                    h.set_status_bank(SharedString::from("Client name cannot be empty"));
                    return;
                }
                let updated = {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        db::update_client(conn, cid, &new_name, &new_ledger).ok()
                    } else { None }
                };
                if updated.is_some() {
                    let event_str = format!("[{}] Edit Client — name='{}' ledger='{}'",
                        audit_now(), new_name, new_ledger);
                    {
                        let mut st = state_ref.lock().unwrap();
                        st.client_name  = new_name.clone();
                        st.tally_ledger = new_ledger.clone();
                        st.audit_events.push(event_str.clone());
                    }
                    h.set_dash_client_name(SharedString::from(new_name.as_str()));
                    h.set_dash_client_ledger(SharedString::from(new_ledger.as_str()));
                    // Refresh dropdown and persist audit event
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        if let Ok(clients) = db::get_clients(conn) {
                            let names: Vec<SharedString> =
                                std::iter::once(SharedString::from("-- Select Client --"))
                                .chain(clients.iter().map(|c| SharedString::from(c.name.as_str())))
                                .collect();
                            h.set_client_names(slint::ModelRc::new(slint::VecModel::from(names)));
                        }
                        let _ = db::push_audit_event(conn, cid, &event_str);
                    }
                    h.set_status_bank(SharedString::from("Client updated"));
                    log::info!("[EditClient] updated cid={} name='{}' ledger='{}'", cid, new_name, new_ledger);
                } else {
                    h.set_status_bank(SharedString::from("Failed to update client"));
                    log::error!("[EditClient] db::update_client failed for cid={}", cid);
                }
            });
        }

        // ── Reload Import ─────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_reload_import(move |idx: i32| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let import_ids = { state_ref.lock().unwrap().import_ids.clone() };
                let Some(&import_id) = import_ids.get(idx as usize) else {
                    log::warn!("[ReloadImport] idx {} out of range (import_ids len={})", idx, import_ids.len());
                    return;
                };
                let txns = {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        db::get_transactions_for_import(conn, import_id).unwrap_or_default()
                    } else { vec![] }
                };
                let opening_bal = txns.iter().find(|t| t.is_opening_balance).and_then(|t| t.balance);
                {
                    let mut st = state_ref.lock().unwrap();
                    st.transactions    = txns.clone();
                    st.opening_balance = opening_bal;
                    st.active_filter   = "all".to_string();
                    st.date_from       = String::new();
                    st.date_to         = String::new();
                    st.bank_filter     = String::new();
                }
                let st = state_ref.lock().unwrap();
                rebuild_rows(&h, &st);
                if !txns.is_empty() {
                    push_dashboard(&h, &txns, opening_bal);
                }
                h.set_status_bank(SharedString::from("Import reloaded from database"));
                log::info!("[ReloadImport] import_id={} txns={}", import_id, txns.len());
            });
        }

        // ── Add Row ────────────────────────────────────────────────────────────
        {
            let handle = app.as_weak();
            app.on_do_add_row(move || {
                // Fields are cleared in Slint before this callback fires.
                // This hook exists for any future Rust-side pre-open preparation.
                let h = match handle.upgrade() { Some(h) => h, None => return };
                log::info!("[AddRow] modal opened — {} transactions in session",
                    h.get_transaction_rows().row_count());
            });
        }

        // ── Row editing callbacks ─────────────────────────────────────────────

        // Helper: convert visible row index (filtered) → absolute index in st.transactions
        // We need this because the UI row idx is into the filtered list, not st.transactions.
        fn visible_to_abs(st: &ui::AppState, vis_idx: usize) -> Option<usize> {
            let mut vis = 0usize;
            for (abs, t) in st.transactions.iter().enumerate() {
                if t.is_opening_balance { continue; }
                let pass = match st.active_filter.as_str() {
                    "unreviewed"   => matches!(t.status, parser::TransactionStatus::Unreviewed),
                    "suspense"     => matches!(t.status, parser::TransactionStatus::Suspense),
                    "high"         => matches!(t.status, parser::TransactionStatus::Classified) && t.confidence >= 0.7,
                    "duplicates"   => t.dup_flag,
                    "gst"          => t.tags.iter().any(|g| { let u = g.to_uppercase(); u.contains("GST") || u.contains("TAX") }),
                    "needs_review" => matches!(t.status, parser::TransactionStatus::NeedsReview),
                    _              => true,
                };
                if !pass { continue; }
                // date filter
                if !st.date_from.is_empty() || !st.date_to.is_empty() {
                    let td = match parse_date_ymd(&t.date) { Some(d) => d, None => { vis += 1; if vis - 1 == vis_idx { return Some(abs); } continue; }};
                    if !st.date_from.is_empty() { if let Some(fd) = parse_date_ymd(&st.date_from) { if td < fd { continue; } } }
                    if !st.date_to.is_empty()   { if let Some(tod) = parse_date_ymd(&st.date_to)   { if td > tod { continue; } } }
                }
                if !st.bank_filter.is_empty() && t.bank_name != st.bank_filter { continue; }
                if vis == vis_idx { return Some(abs); }
                vis += 1;
            }
            None
        }

        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();

            app.on_do_row_click(move |idx| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let st = state_ref.lock().unwrap();

                if let Some(abs) = visible_to_abs(&st, idx as usize) {
                    let t = &st.transactions[abs];
                    h.set_edit_txn_idx(idx);
                    h.set_edit_txn_bank(SharedString::from(t.bank_name.as_str()));
                    h.set_edit_txn_date(SharedString::from(t.date.as_str()));
                    h.set_edit_txn_narr(SharedString::from(t.narration.as_str()));
                    h.set_edit_txn_dr(SharedString::from(fmt_cell(t.debit).as_str()));
                    h.set_edit_txn_cr(SharedString::from(fmt_cell(t.credit).as_str()));
                }
                log::info!("[RowClick] vis_idx={}", idx);
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();

            app.on_do_save_txn(move |idx, vendor, head, typ, learn| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();

                if let Some(abs) = visible_to_abs(&st, idx as usize) {
                    let client_id = st.client_id.unwrap_or(0);
                    let t = &mut st.transactions[abs];
                    if !vendor.is_empty() { t.vendor = vendor.to_string(); }
                    if !head.is_empty()   { t.account_head = head.to_string(); }
                    if !typ.is_empty() {
                        t.txn_type = match typ.as_str() {
                            "Receipt" => parser::VoucherType::Receipt,
                            "Payment" => parser::VoucherType::Payment,
                            "Contra"  => parser::VoucherType::Contra,
                            _         => t.txn_type.clone(),
                        };
                    }
                    t.status     = parser::TransactionStatus::Classified;
                    t.confidence = 1.0;

                    // Save & Learn: derive a narration pattern and persist as a rule
                    if learn {
                        let pattern = {
                            // Port of JS _pattern(): strip long digit runs, take first 30 chars uppercase
                            let stripped = regex::Regex::new(r"\b\d{6,}\b").unwrap()
                                .replace_all(&t.narration, "").to_string();
                            stripped.trim().to_uppercase().chars().take(30).collect::<String>()
                        };
                        if !pattern.is_empty() {
                            let db = db_ref.lock().unwrap();
                            if let Some(conn) = db.as_ref() {
                                match db::add_rule(conn, client_id, &pattern, &vendor, &head, &typ) {
                                    Ok(_)  => log::info!("[SaveLearn] rule saved: pattern='{}' head='{}' vendor='{}'", pattern, head, vendor),
                                    Err(e) => log::error!("[SaveLearn] DB error: {}", e),
                                }
                            }
                        }
                    }
                    // Persist classification change to DB
                    let t_id       = t.id.clone();
                    let t_vendor   = t.vendor.clone();
                    let t_head     = t.account_head.clone();
                    let t_type_str = t.txn_type.to_string();
                    let t_status   = t.status.to_string();
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        let _ = db::upsert_transaction_classification(
                            conn, &t_id, &t_vendor, &t_head, &t_type_str, &t_status, 1.0,
                        );
                    }
                    log::info!("[SaveTxn] abs={} vendor='{}' head='{}' type='{}' learn={}", abs, vendor, head, typ, learn);
                }
                rebuild_rows(&h, &st);
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();

            app.on_do_delete_txn(move |idx| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();
                let mut audit: Option<(i64, String, String)> = None;

                if let Some(abs) = visible_to_abs(&st, idx as usize) {
                    let t = &st.transactions[abs];
                    let txn_id = t.id.clone();
                    let narr: String = t.narration.chars().take(60).collect();
                    let event_str = format!("[{}] Delete Transaction — id='{}' narr='{}'",
                        audit_now(), txn_id, narr);
                    if let Some(cid) = st.client_id {
                        audit = Some((cid, event_str.clone(), txn_id));
                    }
                    st.audit_events.push(event_str);
                    st.transactions.remove(abs);
                    log::info!("[DeleteTxn] abs={} removed", abs);
                }
                rebuild_rows(&h, &st);
                drop(st);
                if let Some((cid, event_str, txn_id)) = audit {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        let _ = db::push_audit_event(conn, cid, &event_str);
                        let _ = db::delete_transaction(conn, &txn_id);
                    }
                }
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();

            app.on_do_mark_suspense(move |idx| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();

                if let Some(abs) = visible_to_abs(&st, idx as usize) {
                    st.transactions[abs].status = parser::TransactionStatus::Suspense;
                    log::info!("[MarkSuspense] abs={}", abs);
                }
                rebuild_rows(&h, &st);
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();

            app.on_do_add_txn(move |date, refno, narr, dr, cr, vendor, head, typ| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();

                let debit_val  = dr.parse::<f64>().ok().filter(|v| *v > 0.0);
                let credit_val = cr.parse::<f64>().ok().filter(|v| *v > 0.0);

                let new_txn = parser::Transaction {
                    id:          format!("manual-{}", st.transactions.len()),
                    import_id:   None,
                    date:        date.to_string(),
                    date_ts:     0,
                    narration:   narr.to_string(),
                    reference:   refno.to_string(),
                    debit:       debit_val,
                    credit:      credit_val,
                    balance:     None,
                    vendor:      vendor.to_string(),
                    account_head: head.to_string(),
                    txn_type:    match typ.as_str() {
                        "Receipt" => parser::VoucherType::Receipt,
                        "Payment" => parser::VoucherType::Payment,
                        "Contra"  => parser::VoucherType::Contra,
                        _         => parser::VoucherType::Unknown,
                    },
                    confidence:  1.0,
                    status:      parser::TransactionStatus::Manual,
                    tags:        vec![],
                    bank_name:   st.bank_name.clone(),
                    account_no:  st.account_no.clone(),
                    is_opening_balance: false,
                    dup_flag:    false,
                    prev_balance: None,
                    balance_ok:  None,
                };
                let new_txn_for_db = new_txn.clone();
                st.transactions.push(new_txn);
                let client_id = st.client_id;
                let event_str = format!("[{}] Manual Add — date='{}' narr='{}'", audit_now(), date, narr);
                st.audit_events.push(event_str.clone());
                log::info!("[AddTxn] date='{}' narr='{}' dr={:?} cr={:?}", date, narr, debit_val, credit_val);
                rebuild_rows(&h, &st);
                drop(st);
                if let Some(cid) = client_id {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        let _ = db::upsert_transactions(conn, cid, None, &[new_txn_for_db]);
                        let _ = db::push_audit_event(conn, cid, &event_str);
                    }
                }
            });
        }

        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();

            app.on_do_pdf_pwd_confirm(move |pwd: SharedString| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                h.set_pdf_pwd_visible(false);

                let (pending_path, pending_name) = {
                    let mut st = state_ref.lock().unwrap();
                    (st.pending_pdf_path.take(), std::mem::take(&mut st.pending_pdf_name))
                };

                let Some(path) = pending_path else {
                    log::warn!("[PdfPwd] confirm fired but no pending path");
                    return;
                };

                let pwd_bytes: Vec<u8> = pwd.as_str().as_bytes().to_vec();
                let file_name = if pending_name.is_empty() {
                    path.file_name().map_or_else(
                        || "unknown".to_string(),
                        |n| n.to_string_lossy().into_owned(),
                    )
                } else {
                    pending_name
                };

                h.set_status_bank(SharedString::from("Unlocking PDF\u{2026}"));

                let stage1 = match parser::text_extractor::extract_pages_with_password(&path, &pwd_bytes) {
                    Ok(rows) if !rows.is_empty() => parser::pdf_parser::parse_pdf_rows(rows, &file_name),
                    Ok(_) => None,
                    Err(e) => {
                        let emsg = e.to_string();
                        let msg = if emsg.to_lowercase().contains("incorrect") {
                            "Incorrect PDF password \u{2014} please try again".to_string()
                        } else {
                            format!("PDF unlock failed: {}", emsg)
                        };
                        h.set_status_bank(SharedString::from(msg.as_str()));
                        return;
                    }
                };

                let parse_result = if stage1.is_some() {
                    stage1
                } else {
                    let full_text = parser::text_extractor::extract_full_text_with_password(&path, &pwd_bytes);
                    if full_text.trim().is_empty() {
                        h.set_status_bank(SharedString::from("PDF unlocked but no text found"));
                        return;
                    }
                    let ocr = parser::ocr_parser::parse_ocr_text(&full_text, &file_name);
                    let real_count = ocr.transactions.iter().filter(|t| !t.is_opening_balance).count();
                    if real_count > 0 {
                        Some(ocr)
                    } else {
                        let preprocessed = parser::ocr_parser::preprocess_multiline(&full_text);
                        if !preprocessed.trim().is_empty() {
                            let ml = parser::ocr_parser::parse_ocr_text(&preprocessed, &file_name);
                            let ml_count = ml.transactions.iter().filter(|t| !t.is_opening_balance).count();
                            if ml_count > 0 { Some(ml) } else { None }
                        } else { None }
                    }
                };

                let result = match parse_result {
                    Some(r) => r,
                    None => {
                        h.set_status_bank(SharedString::from("No transactions found after unlock"));
                        return;
                    }
                };

                apply_parse_result(&h, &state_ref, &db_ref, result, &file_name);
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();

            app.on_do_pdf_pwd_cancel(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                h.set_pdf_pwd_visible(false);
                {
                    let mut st = state_ref.lock().unwrap();
                    st.pending_pdf_path = None;
                    st.pending_pdf_name = String::new();
                }
                h.set_status_bank(SharedString::from("PDF unlock cancelled"));
            });
        }
        {
            app.on_do_ai_cancel(|| {
                log::info!("[AICancel] user cancelled AI classification");
            });
        }

        // ── Dashboard filter callback ──────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();

            app.on_do_dash_filter(move |from, to, bank, vendor, head| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let st = state_ref.lock().unwrap();
                let txns      = st.transactions.clone();
                let opening   = st.opening_balance;
                drop(st);

                // If all filters empty just re-use the full set
                let filtered_txns: Vec<parser::Transaction> = if from.is_empty() && to.is_empty()
                    && bank.is_empty() && vendor.is_empty() && head.is_empty()
                {
                    txns.clone()
                } else {
                    let filter = analytics::DashFilter {
                        from:   from.as_str(),
                        to:     to.as_str(),
                        bank:   bank.as_str(),
                        vendor: vendor.as_str(),
                        head:   head.as_str(),
                    };
                    analytics::filter_txns(&txns, &filter)
                        .into_iter().cloned().collect()
                };

                push_dashboard(&h, &filtered_txns, opening);
                log::info!("[DashFilter] filtered to {} txns", filtered_txns.len());
            });
        }

        log::info!("Slint event loop starting…");
        app.run()?;
    }

    #[cfg(not(feature = "slint-ui"))]
    {
        log::warn!("Built without slint-ui feature — no window will open.");
    }

    Ok(())
}
