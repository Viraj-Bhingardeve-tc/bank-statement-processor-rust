// main.rs — Entry point for the Bank Statement Processor (Rust + Slint).
//
// Boot sequence:
//   1. Initialise logger
//   2. Open (or create) the SQLite database
//   3. Create the Slint AppWindow
//   4. Wire callbacks: do-login, do-load-file, all toolbar/footer actions
//   5. Run the Slint event loop

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Every module below lives in the library crate (`lib.rs`), not as a separate
// bin-only copy — `use` instead of `mod` so it's compiled exactly once. Two
// separate `mod` declarations pointing at the same source files (one here,
// one in lib.rs) used to compile every one of these modules — and their
// `#[cfg(test)]` blocks — into *both* the lib test binary and the bin test
// binary. `cargo test` runs both binaries as separate OS processes, and nothing
// in those tests' cross-process guards (e.g. `db`'s keyring-touching tests use
// an in-process `Mutex` to serialize against each other) protects against two
// *different processes* hitting the same real file path or OS keyring entry
// at the same time — which is exactly what caused an intermittent
// "Cannot open encrypted database ... unreadable with the stored key" failure
// in `db::tests::real_file_database_opens_idempotently_across_repeated_opens`
// under a plain `cargo test`. Depending on the lib crate's single compiled
// copy instead removes the duplicate test binary entirely, closing the race
// at its root instead of trying to widen the lock's reach across processes.
use bank_statement_processor::{
    auth, analytics, classifier, db, export, migration, narration_cleaner,
    tally_group_engine, parser, reconciliation, settings, ui,
};
#[cfg(feature = "ai")]
use bank_statement_processor::ai_classifier;

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
//
// The actual matching engine (scoring, greedy bipartite assignment, Tally
// grid parsing, CSV report) lives in `reconciliation.rs` as pure, unit-tested
// logic. What's left here is just the calamine file I/O glue: open the
// workbook, read its first sheet into a plain string grid, and hand that to
// `reconciliation::parse_tally_grid`.

#[cfg(feature = "slint-ui")]
fn read_workbook_grid(mut wb: calamine::Sheets<std::io::BufReader<std::fs::File>>) -> Vec<Vec<String>> {
    use calamine::Reader;
    let sheet_name = match wb.sheet_names().first() {
        Some(n) => n.to_string(),
        None    => return vec![],
    };
    let range = match wb.worksheet_range(&sheet_name) {
        Ok(r)  => r,
        Err(_) => return vec![],
    };
    range.rows()
        .map(|row| row.iter().map(|c| c.to_string()).collect())
        .collect()
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
fn match_status(t: &parser::Transaction, s: &str) -> bool {
    match s {
        "unreviewed"   => matches!(t.status, parser::TransactionStatus::Unreviewed),
        "suspense"     => matches!(t.status, parser::TransactionStatus::Suspense),
        "high"         => matches!(t.status, parser::TransactionStatus::Classified) && t.confidence >= 0.7,
        "duplicates"   => t.dup_flag,
        "gst"          => t.tags.iter().any(|g| { let u = g.to_uppercase(); u.contains("GST") || u.contains("TAX") }),
        "needs_review" => analytics::effective_needs_review(t),
        "credits"      => t.credit.is_some() && t.debit.is_none(),
        "debits"       => t.debit.is_some() && t.credit.is_none(),
        _              => true,
    }
}

fn apply_txn_filters<'a>(
    txns: &'a [parser::Transaction],
    statuses: &[String],
    from:    &str,
    to:      &str,
    bank:    &str,
    vendor:  &str,
    head:    &str,
) -> Vec<&'a parser::Transaction> {
    txns.iter()
        .filter(|t| !t.is_opening_balance)
        .filter(|t| {
            if statuses.is_empty() { return true; }
            statuses.iter().any(|s| match_status(t, s.as_str()))
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
        .filter(|t| vendor.is_empty() || t.vendor == vendor)
        .filter(|t| head.is_empty() || t.account_head == head)
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
        let (val_ok, val_reasons) = analytics::validate_transaction(t);
        let effective_needs_review = analytics::effective_needs_review(t);
        let row_color: i32 = if effective_needs_review && !matches!(t.status, parser::TransactionStatus::NeedsReview) {
            3  // yellow — validation failure
        } else {
            match t.status {
                parser::TransactionStatus::NeedsReview => 3,
                parser::TransactionStatus::Suspense    => 4,
                parser::TransactionStatus::Manual      => 6,
                _ if t.dup_flag                        => 5,
                parser::TransactionStatus::Classified  => if t.confidence >= 0.7 { 1 } else { 2 },
                _                                      => 0,
            }
        };
        let review_text = if !val_ok { val_reasons.as_str() } else { "" };
        let status_display = if effective_needs_review && matches!(t.status, parser::TransactionStatus::Unreviewed) {
            "needs_review"
        } else {
            &t.status.to_string()
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
            status_text:  SharedString::from(status_display),
            tags:         SharedString::from(t.tags.join(" ").as_str()),
            review:       SharedString::from(review_text),
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
    let review     = real.iter().filter(|t| analytics::effective_needs_review(t)).count();
    [all, unreviewed, suspense, high, duplicates, gst, review]
}

// ── Rebuild visible TxnRow model + filter counts from current AppState filters ─
#[cfg(feature = "slint-ui")]
fn sync_fs_props(h: &AppWindow, st: &ui::AppState) {
    let has = |s: &str| st.filter_statuses.iter().any(|x| x == s);
    h.set_fs_unreviewed(has("unreviewed"));
    h.set_fs_suspense(has("suspense"));
    h.set_fs_high(has("high"));
    h.set_fs_dups(has("duplicates"));
    h.set_fs_gst(has("gst"));
    h.set_fs_review(has("needs_review"));
    h.set_fs_credits(has("credits"));
    h.set_fs_debits(has("debits"));
}

fn rebuild_rows(h: &AppWindow, st: &ui::AppState) {
    let filtered = apply_txn_filters(
        &st.transactions,
        &st.filter_statuses,
        &st.date_from,
        &st.date_to,
        &st.bank_filter,
        &st.vendor_filter,
        &st.head_filter,
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
    sync_fs_props(h, st);

    log::info!("[Filter] showing {} / {} txns  (statuses={:?} from='{}' to='{}' bank='{}')",
        filtered.len(), st.transactions.iter().filter(|t| !t.is_opening_balance).count(),
        st.filter_statuses, st.date_from, st.date_to, st.bank_filter);
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
    h.set_dash_opening(SharedString::from(fmt_amt(s.opening_bal).as_str()));
    h.set_dash_closing(SharedString::from(fmt_amt(s.closing_bal).as_str()));
    // dash-suspense shows a ₹ amount (sum of suspense-txn amounts), not a row
    // count — matches old app's Suspense summary card (app.js:2179-2183),
    // which reads "Amount in suspense: ₹X", not a transaction count.
    h.set_dash_suspense(SharedString::from(fmt_amt(Some(s.suspense_amount)).as_str()));
    h.set_dash_has_suspense(s.suspense_amount > 0.0);
    h.set_dash_needs_review(SharedString::from(s.needs_review_count.to_string().as_str()));
    h.set_dash_duplicates(SharedString::from(s.duplicate_count.to_string().as_str()));
    h.set_dash_gst_count(SharedString::from(s.gst_count.to_string().as_str()));

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
            let (range_from, range_to) = data.monthly.keys.get(i)
                .and_then(|k| analytics::month_key_to_range(k))
                .unwrap_or_default();
            DashMonthBar {
                label:      SharedString::from(lbl.as_str()),
                credit_h:   (cr / scale) as f32,
                debit_h:    (dr / scale) as f32,
                credit_str: SharedString::from(fmt_amt(Some(cr)).as_str()),
                debit_str:  SharedString::from(fmt_amt(Some(dr)).as_str()),
                range_from: SharedString::from(range_from.as_str()),
                range_to:   SharedString::from(range_to.as_str()),
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
    let banks_opts: Vec<SharedString> = std::iter::once(SharedString::from("All Banks"))
        .chain(unique_banks(txns).into_iter().map(|s| SharedString::from(s.as_str())))
        .collect();
    let vendor_opts: Vec<SharedString> = std::iter::once(SharedString::from("All Vendors"))
        .chain(unique_vendors(txns).into_iter().map(|s| SharedString::from(s.as_str())))
        .collect();
    let head_opts: Vec<SharedString> = std::iter::once(SharedString::from("All Expense Heads"))
        .chain(unique_heads(txns).into_iter().map(|s| SharedString::from(s.as_str())))
        .collect();
    let _ = (banks_opts.len(), vendor_opts.len(), head_opts.len());

    h.set_dash_filter_banks(slint::ModelRc::new(slint::VecModel::from(banks_opts)));
    h.set_dash_filter_vendors(slint::ModelRc::new(slint::VecModel::from(vendor_opts)));
    h.set_dash_filter_heads(slint::ModelRc::new(slint::VecModel::from(head_opts)));
}

// ── Populate summary-panel extras: conf quality, GST amounts, parties, ledgers ─

#[cfg(feature = "slint-ui")]
fn push_summary_extras(h: &AppWindow, txns: &[parser::Transaction]) {
    use std::collections::HashMap;

    let real: Vec<&parser::Transaction> = txns.iter()
        .filter(|t| !t.is_opening_balance)
        .collect();
    if real.is_empty() {
        h.set_dash_parties(slint::ModelRc::new(slint::VecModel::<SummaryListRow>::from(vec![])));
        h.set_dash_per_account(slint::ModelRc::new(slint::VecModel::<SummaryListRow>::from(vec![])));
        h.set_dash_rec_ledgers(slint::ModelRc::new(slint::VecModel::<SummaryListRow>::from(vec![])));
        h.set_dash_pay_ledgers(slint::ModelRc::new(slint::VecModel::<SummaryListRow>::from(vec![])));
        return;
    }

    // ── Classification quality ────────────────────────────────────────────────
    let total = real.len() as f64;
    let hi_cnt  = real.iter().filter(|t| t.confidence >= 0.8).count();
    let med_cnt = real.iter().filter(|t| t.confidence >= 0.4 && t.confidence < 0.8).count();
    let lo_cnt  = real.iter().filter(|t| t.confidence < 0.4).count();
    h.set_dash_conf_hi_frac((hi_cnt as f32 / total as f32).min(1.0));
    h.set_dash_conf_med_frac((med_cnt as f32 / total as f32).min(1.0));
    h.set_dash_conf_hi_count(SharedString::from(hi_cnt.to_string().as_str()));
    h.set_dash_conf_med_count(SharedString::from(med_cnt.to_string().as_str()));
    h.set_dash_conf_lo_count(SharedString::from(lo_cnt.to_string().as_str()));

    // ── Classification source breakdown ───────────────────────────────────────
    let ai_cnt  = real.iter().filter(|t| t.classification_source == "ai").count();
    let rule_cnt = real.iter().filter(|t| t.classification_source == "rule").count();
    let kw_cnt  = real.iter().filter(|t| t.classification_source == "keyword").count();
    h.set_dash_cq_ai(SharedString::from(ai_cnt.to_string().as_str()));
    h.set_dash_cq_rule(SharedString::from(rule_cnt.to_string().as_str()));
    h.set_dash_cq_kw(SharedString::from(kw_cnt.to_string().as_str()));

    // ── GST paid / received ───────────────────────────────────────────────────
    let gst_txns: Vec<&&parser::Transaction> = real.iter()
        .filter(|t| t.tags.iter().any(|g| { let u = g.to_uppercase(); u.contains("GST") || u.contains("TAX") }))
        .collect();
    let gst_paid: f64 = gst_txns.iter().filter_map(|t| t.debit).sum();
    let gst_recv: f64 = gst_txns.iter().filter_map(|t| t.credit).sum();
    h.set_dash_gst_paid(SharedString::from(
        ui::AppState::fmt_amount(if gst_paid > 0.0 { Some(gst_paid) } else { None }).as_str()));
    h.set_dash_gst_recv(SharedString::from(
        ui::AppState::fmt_amount(if gst_recv > 0.0 { Some(gst_recv) } else { None }).as_str()));

    // ── Recurring parties (top 6 by count) ───────────────────────────────────
    let mut party_map: HashMap<String, (usize, f64)> = HashMap::new();
    for t in &real {
        if t.vendor.is_empty() { continue; }
        let e = party_map.entry(t.vendor.clone()).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += t.credit.unwrap_or(0.0) + t.debit.unwrap_or(0.0);
    }
    let mut parties: Vec<(String, usize, f64)> = party_map.into_iter()
        .map(|(k, (cnt, amt))| (k, cnt, amt))
        .collect();
    parties.sort_by(|a, b| b.1.cmp(&a.1));
    parties.truncate(6);
    let party_rows: Vec<SummaryListRow> = parties.iter().map(|(name, cnt, amt)| {
        let nm = if name.chars().count() > 20 {
            format!("{}…", name.chars().take(19).collect::<String>())
        } else { name.clone() };
        SummaryListRow {
            lbl: SharedString::from(nm.as_str()),
            val: SharedString::from(format!("{}×  ₹{}", cnt, ui::fmt_inr(*amt)).as_str()),
            is_debit: false,
            key: SharedString::from(name.as_str()),
        }
    }).collect();
    h.set_dash_parties(slint::ModelRc::new(slint::VecModel::from(party_rows)));

    // ── Per-account breakdown (opening/closing balance, txn count) ───────────
    let mut acc_order: Vec<(String, String)> = Vec::new();
    for t in txns {
        let key = (t.bank_name.clone(), t.account_no.clone());
        if (!t.bank_name.is_empty() || !t.account_no.is_empty()) && !acc_order.contains(&key) {
            acc_order.push(key);
        }
    }
    let account_rows: Vec<SummaryListRow> = acc_order.iter().map(|(bank, acct)| {
        let acc_txns: Vec<&parser::Transaction> = txns.iter()
            .filter(|t| &t.bank_name == bank && &t.account_no == acct)
            .collect();
        let opening = acc_txns.iter().find(|t| t.is_opening_balance).and_then(|t| t.balance);
        let mut non_ob: Vec<&&parser::Transaction> = acc_txns.iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        non_ob.sort_by_key(|t| t.date_ts);
        let closing = non_ob.iter().rev().find_map(|t| t.balance);
        let label = [bank.as_str(), acct.as_str()].iter()
            .filter(|s| !s.is_empty())
            .cloned().collect::<Vec<_>>().join(" · ");
        let mut parts: Vec<String> = Vec::new();
        if let Some(ob) = opening { parts.push(format!("Open ₹{}", ui::fmt_inr(ob))); }
        if let Some(cb) = closing { parts.push(format!("Close ₹{}", ui::fmt_inr(cb))); }
        parts.push(format!("{} txns", non_ob.len()));
        SummaryListRow {
            lbl: SharedString::from(label.as_str()),
            val: SharedString::from(parts.join("  ·  ").as_str()),
            is_debit: false,
            key: SharedString::from(bank.as_str()),
        }
    }).collect();
    h.set_dash_per_account(slint::ModelRc::new(slint::VecModel::from(account_rows)));

    // ── Ledger breakdowns (top 8 by amount) ──────────────────────────────────
    let mut rec_map: HashMap<String, f64> = HashMap::new();
    let mut pay_map: HashMap<String, f64> = HashMap::new();
    for t in &real {
        if t.account_head.is_empty() { continue; }
        if let Some(cr) = t.credit {
            *rec_map.entry(t.account_head.clone()).or_insert(0.0) += cr;
        }
        if let Some(dr) = t.debit {
            *pay_map.entry(t.account_head.clone()).or_insert(0.0) += dr;
        }
    }
    let make_ledger_rows = |map: HashMap<String, f64>| -> Vec<SummaryListRow> {
        let mut v: Vec<(String, f64)> = map.into_iter().collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.truncate(8);
        v.iter().map(|(k, amt)| {
            let nm = if k.chars().count() > 22 {
                format!("{}…", k.chars().take(21).collect::<String>())
            } else { k.clone() };
            SummaryListRow {
                lbl: SharedString::from(nm.as_str()),
                val: SharedString::from(format!("₹{}", ui::fmt_inr(*amt)).as_str()),
                is_debit: false,
                key: SharedString::from(k.as_str()),
            }
        }).collect()
    };
    h.set_dash_rec_ledgers(slint::ModelRc::new(slint::VecModel::from(make_ledger_rows(rec_map))));
    h.set_dash_pay_ledgers(slint::ModelRc::new(slint::VecModel::from(make_ledger_rows(pay_map))));
}

// ── OCR pipelines (run off the UI thread — see on_do_load_file) ───────────────
//
// Pure parsing logic, no Slint handle access, safe to call from a spawned
// thread. Previously this work (including the Tesseract subprocess call,
// which can take seconds per page) ran synchronously inside the
// on_do_load_file Slint callback, freezing the whole UI with no feedback —
// the built ocr-visible/ocr-msg/ocr-pct progress overlay existed in the
// .slint file but nothing ever drove it. See
// PRODUCTION_READINESS_AUDIT_2026-06-22.md Phase 2 item 8.

#[cfg(feature = "slint-ui")]
// `progress(pct, msg)` reports real pipeline-stage checkpoints. Unlike old
// app's Tesseract.js (which OCRs page-by-page in JS and can report percent
// within a single page), Rust shells out to a system Tesseract binary as one
// blocking call with no incremental hook — so this can't reproduce per-page
// percentages, but it replaces the previous static "0%" (set once and never
// updated) with real, monotonically-increasing stage progress.
fn run_pdf_ocr_pipeline<F: Fn(i32, &str)>(
    path: &std::path::Path,
    file_name: &str,
    progress: F,
) -> Result<parser::ParseResult, String> {
    progress(10, "Extracting embedded text\u{2026}");
    let full_text = parser::text_extractor::extract_full_text(path);
    let effective_text = if full_text.trim().is_empty() {
        progress(25, "Running Tesseract OCR\u{2026}");
        match parser::ocr_extractor::extract_via_tesseract(path) {
            Some(t) if !t.trim().is_empty() => t,
            _ => return Err("Scanned PDF — install Tesseract for OCR support".to_string()),
        }
    } else {
        full_text
    };

    progress(70, "Correcting OCR text\u{2026}");
    let ocr = parser::ocr_parser::parse_ocr_text(&effective_text, file_name);
    let real_count = ocr.transactions.iter().filter(|t| !t.is_opening_balance).count();
    if real_count > 0 {
        progress(100, "Done");
        return Ok(ocr);
    }

    progress(85, "Retrying with multi-line narration handling\u{2026}");
    let preprocessed = parser::ocr_parser::preprocess_multiline(&effective_text);
    if !preprocessed.trim().is_empty() {
        let ml = parser::ocr_parser::parse_ocr_text(&preprocessed, file_name);
        let ml_count = ml.transactions.iter().filter(|t| !t.is_opening_balance).count();
        if ml_count > 0 {
            progress(100, "Done");
            return Ok(ml);
        }
    }
    Err("No transactions found — PDF may use embedded fonts".to_string())
}

#[cfg(feature = "slint-ui")]
fn run_image_ocr_pipeline<F: Fn(i32, &str)>(
    path: &std::path::Path,
    file_name: &str,
    progress: F,
) -> Result<parser::ParseResult, String> {
    progress(20, "Running Tesseract OCR\u{2026}");
    match parser::ocr_extractor::extract_image_via_tesseract(path) {
        Some(text) if !text.trim().is_empty() => {
            progress(75, "Correcting OCR text\u{2026}");
            let ocr = parser::ocr_parser::parse_ocr_text(&text, file_name);
            let real_count = ocr.transactions.iter().filter(|t| !t.is_opening_balance).count();
            if real_count > 0 {
                progress(100, "Done");
                Ok(ocr)
            } else {
                Err("No transactions found in image — check image quality".to_string())
            }
        }
        _ => Err("Image OCR failed — install Tesseract for image support".to_string()),
    }
}

/// Cross-import dedup + persistence + UI refresh — the common tail shared by
/// every successful parse path in on_do_load_file, whether it returned
/// synchronously (Excel, plain-text PDF) or via the background OCR pipelines
/// above.
#[cfg(feature = "slint-ui")]
fn finish_load_file(
    h: &AppWindow,
    state_ref: &Arc<Mutex<ui::AppState>>,
    db_ref: &Arc<Mutex<Option<rusqlite::Connection>>>,
    mut result: parser::ParseResult,
    file_name: &str,
) {
    // Cross-import dedup — catches the same statement being re-loaded in a
    // separate session, not just duplicate rows within one load. Synthetic
    // (opening-balance) rows are always kept.
    if h.get_dedup_enabled() {
        let client_id = state_ref.lock().unwrap().client_id;
        if let Some(cid) = client_id {
            let known = {
                let db = db_ref.lock().unwrap();
                db.as_ref().and_then(|conn| db::get_dedupe_hashes(conn, cid).ok())
                    .unwrap_or_default()
            };
            let before = result.transactions.iter().filter(|t| !t.is_opening_balance).count();
            result.transactions.retain(|t| t.is_opening_balance || !known.contains(&t.hash()));
            let after = result.transactions.iter().filter(|t| !t.is_opening_balance).count();
            let skipped = before.saturating_sub(after);
            if after == 0 {
                h.set_toast_msg(SharedString::from(
                    "All transactions in this file already loaded (dedupe)"));
                h.set_toast_kind(2);
                h.set_status_bank(SharedString::from("Dedupe: nothing new to load"));
                return;
            }
            if skipped > 0 {
                h.set_toast_msg(SharedString::from(
                    format!("Dedupe: skipped {} duplicate row(s)", skipped).as_str()));
                h.set_toast_kind(2);
            }
            let new_hashes: Vec<String> = result.transactions.iter()
                .filter(|t| !t.is_opening_balance)
                .map(|t| t.hash())
                .collect();
            let db = db_ref.lock().unwrap();
            if let Some(conn) = db.as_ref() {
                // Not user-facing-critical (the transactions themselves are
                // about to be persisted by apply_parse_result below) — but a
                // failure here means future imports won't recognize these rows
                // as already-seen, so it's worth a clear log even without a toast.
                if let Err(e) = db::add_dedupe_hashes(conn, cid, &new_hashes) {
                    log::error!("[LoadFile] failed to persist dedupe hashes: {}", e);
                }
            }
        }
    }

    apply_parse_result(h, state_ref, db_ref, result, file_name);
}

#[cfg(feature = "slint-ui")]
fn apply_parse_result(
    h: &AppWindow,
    state_ref: &Arc<Mutex<ui::AppState>>,
    db_ref: &Arc<Mutex<Option<rusqlite::Connection>>>,
    mut result: parser::ParseResult,
    file_name: &str,
) {
    // Canonicalize vendor/account-head names (port of Electron's _normalizeVendors,
    // run as step 0 of _postProcess) so narration variants of the same real-world
    // party collapse into one ledger name before any counts/breakdowns are built.
    parser::party_master::normalize_vendors(&mut result.transactions);

    let cfg = {
        let db = db_ref.lock().unwrap();
        db.as_ref().map(|conn| settings::Settings::load(conn)).unwrap_or_default()
    };

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

    // Run narration cleaner on all transactions — gated by the Settings
    // screen's "Enable narration cleaning" toggle. When disabled, build a
    // neutral (zero-confidence, empty-party, cleaned==original) meta per
    // transaction instead of skipping the vector: every downstream site below
    // already falls back to the raw transaction whenever confidence is below
    // its 0.4 threshold or party is blank, so this single conditional is
    // enough to make narr_enabled=false a true no-op end to end, matching the
    // old Electron app's `cleanBatch()` early-return when disabled.
    let narration_strs: Vec<String> = real.iter().map(|t| t.narration.clone()).collect();
    let cleaned_narrations: Vec<narration_cleaner::NarrationMeta> = if cfg.narr_enabled {
        narration_cleaner::clean_batch_with(&narration_strs, cfg.narr_title_case)
    } else {
        narration_strs.iter().map(|n| narration_cleaner::NarrationMeta {
            original: n.clone(), cleaned: n.clone(), txn_type: "OTHER".to_string(),
            party: String::new(), payment_ref: String::new(), confidence: 0.0,
        }).collect()
    };

    // Compute Tally group for each transaction.
    let tally_inputs: Vec<(String, String, bool, f64)> = real.iter().enumerate().map(|(idx, t)| {
        let narr = cleaned_narrations[idx].cleaned.clone();
        let is_credit = t.credit.is_some();
        let amount = t.credit.unwrap_or(0.0) + t.debit.unwrap_or(0.0);
        (t.account_head.clone(), narr, is_credit, amount)
    }).collect();
    let tally_groups = tally_group_engine::classify_batch(&tally_inputs, None);

    // Validate each transaction (port of Electron _validateTransaction).
    let validation: Vec<(bool, String)> = real.iter().map(|t| analytics::validate_transaction(t)).collect();

    let mut bank_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let row_models: Vec<TxnRow> = real
        .iter()
        .enumerate()
        .map(|(idx, t)| {
            bank_set.insert(t.bank_name.clone());
            let meta = &cleaned_narrations[idx];
            // "Preserve original narration" (narr_preserve) keeps the table's
            // Narration column showing the bank's raw text verbatim even when
            // the cleaner is confident — it only affects this display column;
            // vendor suggestion and Tally-group classification below still
            // benefit from the cleaned/party-extracted text either way.
            let narr: String = if !cfg.narr_preserve && meta.confidence >= 0.4 && !meta.cleaned.is_empty() {
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
            let (val_ok, ref val_reasons) = validation[idx];
            let effective_needs_review = analytics::effective_needs_review(t);
            let row_color: i32 = if effective_needs_review && !matches!(t.status, parser::TransactionStatus::NeedsReview) {
                3  // yellow — validation failure
            } else {
                match t.status {
                    parser::TransactionStatus::NeedsReview => 3,
                    parser::TransactionStatus::Suspense    => 4,
                    parser::TransactionStatus::Manual      => 6,
                    _ if t.dup_flag                        => 5,
                    parser::TransactionStatus::Classified  => {
                        if t.confidence >= 0.7 { 1 } else { 2 }
                    }
                    _ => 0,
                }
            };
            let tally_group = tally_groups[idx].as_str();
            let review_text = if !val_ok { val_reasons.as_str() } else { "" };
            let status_display = if effective_needs_review && matches!(t.status, parser::TransactionStatus::Unreviewed) {
                "needs_review"
            } else {
                &t.status.to_string()
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
                vendor:       SharedString::from(vendor_display.as_str()),
                ledger:       SharedString::from(t.account_head.as_str()),
                expense_head: SharedString::from(tally_group),
                status_text:  SharedString::from(status_display),
                tags:         SharedString::from(t.tags.join(" ").as_str()),
                review:       SharedString::from(review_text),
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
    // suspense/needs_review/duplicates/gst counts are set by push_dashboard() below,
    // which recomputes them live from analytics::compute() on every dashboard refresh.
    h.set_dash_calc_closing(SharedString::from(ui::AppState::fmt_amount(calc_closing).as_str()));
    h.set_dash_has_mismatch(has_mismatch);
    h.set_dash_mismatch(SharedString::from(mismatch_str.as_str()));
    push_summary_extras(h, &result.transactions);

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
                    match db::upsert_transactions(conn, cid, Some(iid), &result.transactions) {
                        Ok(n) => log::info!("[LoadFile] persisted {} txns import_id={}", n, iid),
                        Err(e) => {
                            log::error!("[LoadFile] failed to persist transactions: {}", e);
                            h.set_toast_msg(SharedString::from(
                                format!("Save failed — transactions were NOT saved: {}", e).as_str(),
                            ));
                            h.set_toast_kind(2);
                        }
                    }
                }
                // Auto-seed unique account heads into the ledgers table
                let mut heads_seen = std::collections::HashSet::new();
                let heads_with_groups: Vec<(String, String)> = real.iter()
                    .filter(|t| !t.account_head.is_empty())
                    .filter(|t| heads_seen.insert(t.account_head.to_lowercase()))
                    .map(|t| (t.account_head.clone(), tally_group_engine::classify(
                        &t.account_head, &t.narration, t.credit.is_some(),
                        t.credit.unwrap_or(0.0) + t.debit.unwrap_or(0.0), None,
                    )))
                    .collect();
                if !heads_with_groups.is_empty() {
                    if let Err(e) = db::auto_seed_ledgers(conn, cid, &heads_with_groups) {
                        log::error!("[LoadFile] failed to auto-seed ledgers: {}", e);
                    }
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
        st.filter_statuses.clear();
        st.undo_stack.clear();
        h.set_can_undo(false);
        st.date_from       = String::new();
        st.date_to         = String::new();
        st.bank_filter     = String::new();
        st.vendor_filter   = String::new();
        st.head_filter     = String::new();
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
    push_summary_extras(h, &txns_all);

    log::info!("UI updated with {} transactions", real.len());
}

// ── Batch folder processing (resumable across a PDF password prompt) ─────────
//
// Old app's batch loop can `await` a per-file password prompt inline
// (parser.js's async generator style); Rust's batch loop runs synchronously
// on the UI thread, so a password-protected file instead pauses the batch —
// saving its in-progress accumulators in `ui::BatchProgress` — shows the
// existing single-file password modal, and resumes from `remaining` once
// `on_do_pdf_pwd_confirm`/`on_do_pdf_pwd_cancel` fires (see those handlers).

enum BatchFileOutcome {
    Parsed(parser::ParseResult),
    NeedsPassword,
    Failed,
}

/// Try to parse one batch file. Mirrors the dispatch logic previously inline
/// in `on_do_batch_folder`'s loop body.
#[cfg(feature = "slint-ui")]
fn try_parse_batch_file(path: &std::path::Path, file_name: &str) -> BatchFileOutcome {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();

    if ["xlsx", "xls", "xlsm"].contains(&ext.as_str()) {
        match parser::excel_parser::parse_excel_file(path) {
            Ok(r) if !r.transactions.is_empty() => BatchFileOutcome::Parsed(r),
            _ => BatchFileOutcome::Failed,
        }
    } else if ext == "pdf" {
        match parser::text_extractor::extract_pages(path) {
            Ok(pages) if !pages.is_empty() => {
                match parser::pdf_parser::parse_pdf_rows(pages, file_name) {
                    Some(r) if !r.transactions.is_empty() => BatchFileOutcome::Parsed(r),
                    _ => BatchFileOutcome::Failed,
                }
            }
            Ok(_) => {
                let text = parser::text_extractor::extract_full_text(path);
                if !text.trim().is_empty() {
                    let r = parser::ocr_parser::parse_ocr_text(&text, file_name);
                    if r.transactions.iter().any(|t| !t.is_opening_balance) {
                        BatchFileOutcome::Parsed(r)
                    } else {
                        BatchFileOutcome::Failed
                    }
                } else {
                    match parser::ocr_extractor::extract_via_tesseract(path) {
                        Some(t) => {
                            let r = parser::ocr_parser::parse_ocr_text(&t, file_name);
                            if r.transactions.iter().any(|t| !t.is_opening_balance) {
                                BatchFileOutcome::Parsed(r)
                            } else {
                                BatchFileOutcome::Failed
                            }
                        }
                        None => BatchFileOutcome::Failed,
                    }
                }
            }
            Err(e) => {
                let emsg = e.to_string();
                if emsg.contains("password-protected") || emsg.to_lowercase().contains("encrypt") {
                    BatchFileOutcome::NeedsPassword
                } else {
                    BatchFileOutcome::Failed
                }
            }
        }
    } else if parser::ocr_extractor::IMAGE_EXTS.contains(&ext.as_str()) {
        match parser::ocr_extractor::extract_image_via_tesseract(path) {
            Some(t) => {
                let r = parser::ocr_parser::parse_ocr_text(&t, file_name);
                if r.transactions.iter().any(|t| !t.is_opening_balance) {
                    BatchFileOutcome::Parsed(r)
                } else {
                    BatchFileOutcome::Failed
                }
            }
            None => BatchFileOutcome::Failed,
        }
    } else {
        BatchFileOutcome::Failed
    }
}

/// Record a successfully parsed batch file into the in-progress accumulators —
/// mirrors what was previously the inline `Some(r) if !r.transactions.is_empty()`
/// arm of `on_do_batch_folder`'s match.
#[cfg(feature = "slint-ui")]
fn record_batch_success(
    bp: &mut ui::BatchProgress,
    db_ref: &Arc<Mutex<Option<rusqlite::Connection>>>,
    path: &std::path::Path,
    file_name: &str,
    r: parser::ParseResult,
) {
    let r_bank    = r.bank_name.clone();
    let r_account = r.account_no.clone();
    let r_ob      = r.opening_balance;
    let r_txns    = r.transactions.clone();
    let r_cnt     = r_txns.iter().filter(|t| !t.is_opening_balance).count();
    let non_ob: Vec<&parser::Transaction> = r_txns.iter().filter(|t| !t.is_opening_balance).collect();
    let first_d = non_ob.first().map(|t| t.date.as_str()).unwrap_or("").to_string();
    let last_d  = non_ob.last().map(|t| t.date.as_str()).unwrap_or("").to_string();
    let period  = if first_d.is_empty() { "—".to_string() }
                  else if first_d == last_d { first_d.clone() }
                  else { format!("{} - {}", first_d, last_d) };

    bp.batch_results.push(ui::BatchFileResult {
        file:    file_name.chars().take(35).collect::<String>(),
        bank:    r_bank.clone(),
        account: r_account.clone(),
        period,
        txns:    r_cnt,
        ok:      true,
        err_msg: String::new(),
    });

    let mut existing_hashes: std::collections::HashSet<String> =
        bp.all_txns.iter().map(|t| t.hash()).collect();
    existing_hashes.extend(bp.persisted_hashes.iter().cloned());
    let new_txns: Vec<parser::Transaction> = r.transactions.into_iter()
        .filter(|t| t.is_opening_balance || !existing_hashes.contains(&t.hash()))
        .collect();
    let kept_cnt = new_txns.iter().filter(|t| !t.is_opening_balance).count();
    bp.skipped += r_cnt.saturating_sub(kept_cnt);
    bp.all_txns.extend(new_txns.clone());
    bp.loaded += 1;
    if bp.first_bank.is_empty() { bp.first_bank = r_bank.clone(); }
    if bp.first_ob.is_none() { bp.first_ob = r_ob; }

    if let Some(client_id) = bp.client_id.filter(|&c| c > 0) {
        let db = db_ref.lock().unwrap();
        if let Some(conn) = db.as_ref() {
            match db::save_import(conn, client_id, file_name, &r_bank, &r_account, r_cnt) {
                Ok(iid) => {
                    if let Err(e) = db::upsert_transactions(conn, client_id, Some(iid), &new_txns) {
                        log::error!("[Batch] failed to persist transactions for {:?}: {}", path, e);
                        if let Some(last) = bp.batch_results.last_mut() {
                            last.ok = false;
                            last.err_msg = format!("Parsed but save failed: {}", e);
                        }
                    } else {
                        bp.new_import_ids.push(iid);
                    }
                }
                Err(e) => log::error!("[Batch] failed to record import history for {:?}: {}", path, e),
            }
        }
    }
}

#[cfg(feature = "slint-ui")]
fn record_batch_failure(bp: &mut ui::BatchProgress, file_name: &str, err_msg: &str) {
    bp.errors += 1;
    log::warn!("[Batch] failed: {} — {}", file_name, err_msg);
    bp.batch_results.push(ui::BatchFileResult {
        file:    file_name.chars().take(35).collect::<String>(),
        bank:    String::new(),
        account: String::new(),
        period:  String::new(),
        txns:    0,
        ok:      false,
        err_msg: err_msg.to_string(),
    });
}

/// Finalize a batch once every file in `remaining` has been processed —
/// mirrors what was previously the tail of `on_do_batch_folder` after its loop.
#[cfg(feature = "slint-ui")]
fn finish_batch(
    h: &AppWindow,
    state_ref: &Arc<Mutex<ui::AppState>>,
    db_ref: &Arc<Mutex<Option<rusqlite::Connection>>>,
) {
    let bp = { state_ref.lock().unwrap().batch_progress.take() };
    let Some(bp) = bp else { return };
    let ui::BatchProgress {
        mut all_txns, loaded, skipped, errors, first_bank, first_ob,
        new_import_ids, batch_results, client_id, remaining, aborted, ..
    } = bp;
    let unprocessed = remaining.len();

    // Reset in every finish path (including the early "nothing loaded"
    // return below) — otherwise an aborted-before-any-success batch would
    // leave Pause/Abort stuck enabled with nothing left for them to control.
    h.set_batch_running(false);
    h.set_batch_paused(false);

    if all_txns.is_empty() {
        let msg = if aborted {
            "Batch aborted \u{2014} no transactions loaded"
        } else {
            "Batch: no transactions loaded"
        };
        h.set_status_bank(SharedString::from(msg));
        return;
    }

    if let Some(cid) = client_id {
        let new_hashes: Vec<String> = all_txns.iter()
            .filter(|t| !t.is_opening_balance)
            .map(|t| t.hash())
            .collect();
        let db = db_ref.lock().unwrap();
        if let Some(conn) = db.as_ref() {
            if let Err(e) = db::add_dedupe_hashes(conn, cid, &new_hashes) {
                log::error!("[Batch] failed to persist dedupe hashes: {}", e);
            }
        }
    }

    let (bank_ledger, client_id2) = {
        let st = state_ref.lock().unwrap();
        (st.tally_ledger.clone(), st.client_id.unwrap_or(0))
    };
    let (rules, cfg) = {
        let db = db_ref.lock().unwrap();
        match db.as_ref() {
            Some(conn) => (db::get_rules(conn, client_id2).unwrap_or_default(), settings::Settings::load(conn)),
            None       => (vec![], settings::Settings::default()),
        }
    };
    let dedup_on = h.get_dedup_enabled();
    classifier::classify_all(&mut all_txns, &bank_ledger, &rules, dedup_on, cfg.gst_enabled, cfg.gst_auto_ledgers);
    parser::party_master::normalize_vendors(&mut all_txns);

    let real: Vec<&parser::Transaction> = all_txns.iter().filter(|t| !t.is_opening_balance).collect();
    let total_dr: f64 = real.iter().filter_map(|t| t.debit).sum();
    let total_cr: f64 = real.iter().filter_map(|t| t.credit).sum();
    let row_models = build_txn_rows(&real);
    let bank_set: std::collections::BTreeSet<String> = real.iter().map(|t| t.bank_name.clone()).collect();
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
        st.filter_statuses.clear();
        st.date_from       = String::new();
        st.date_to         = String::new();
        st.bank_filter     = String::new();
        st.vendor_filter   = String::new();
        st.head_filter     = String::new();
        st.import_ids.extend(new_import_ids.iter());
        st.batch_file_results = batch_results;
    }

    push_dashboard(h, &all_txns, first_ob);
    push_summary_extras(h, &all_txns);
    let batch_event = format!("[{}] Import — {} file(s), {} transactions loaded", audit_now(), loaded, real.len());
    {
        let mut st = state_ref.lock().unwrap();
        st.audit_events.push(batch_event.clone());
    }
    if client_id2 > 0 {
        let db = db_ref.lock().unwrap();
        if let Some(conn) = db.as_ref() {
            if let Err(e) = db::push_audit_event(conn, client_id2, &batch_event) {
                log::error!("[Batch] failed to persist audit event: {}", e);
            }
        }
    }
    log::info!(
        "[Batch] loaded={} skipped={} errors={} total_txns={} aborted={} unprocessed={}",
        loaded, skipped, errors, real.len(), aborted, unprocessed
    );
    let (summary, toast_kind) = batch_summary_message(loaded, skipped, errors, aborted, unprocessed);
    h.set_toast_msg(SharedString::from(summary.as_str()));
    h.set_toast_kind(toast_kind);
}

/// Builds `finish_batch`'s final toast message and kind (1 = success, 3 =
/// warning/error) — pulled out as a pure function so it's testable without a
/// live `AppWindow` (main.rs has no Slint-independent test scaffolding, so
/// keeping this logic Slint-free is what makes it testable at all).
fn batch_summary_message(
    loaded: usize,
    skipped: usize,
    errors: usize,
    aborted: bool,
    unprocessed: usize,
) -> (String, i32) {
    let mut summary = if aborted {
        format!("Batch aborted: {} file(s) loaded", loaded)
    } else {
        format!("{} file(s) loaded", loaded)
    };
    if skipped > 0 {
        summary.push_str(&format!(", {} dupe(s) skipped", skipped));
    }
    if errors > 0 {
        summary.push_str(&format!(", {} file(s) failed", errors));
    }
    if aborted && unprocessed > 0 {
        summary.push_str(&format!(", {} file(s) not processed", unprocessed));
    }
    let toast_kind = if errors > 0 || aborted { 3 } else { 1 };
    (summary, toast_kind)
}

/// Process one file from `batch_progress.remaining`, then schedule the next
/// step as a fresh zero-delay Slint timer tick instead of looping
/// synchronously in this same call. This is the whole mechanism behind
/// Pause/Abort actually working: Slint callbacks (including the Pause/Abort
/// buttons' own click handlers) cannot run while this function itself is
/// still executing on the UI thread, so a tight `loop { ... }` processing
/// every file in one call — which is what this function used to be — made
/// the buttons physically unable to ever receive a click until the entire
/// batch had already finished. Returning control to the event loop between
/// every file (mirroring the old app's single-threaded `while` + `await`
/// loop in `batch-processor.js`, just via a Slint timer instead of a JS
/// microtask) gives Pause/Abort a real point where they take effect.
///
/// Called again to resume: from a fresh `on_do_batch_folder` start, from
/// `on_do_pdf_pwd_confirm`/`on_do_pdf_pwd_cancel` after a password prompt
/// resolves, and from `on_do_batch_pause` when "Resume" is clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchStep {
    /// Stop entirely — either the queue is exhausted (checked separately,
    /// after this) or the user aborted.
    Finish,
    /// User paused — do nothing and don't reschedule.
    WaitPaused,
    /// Normal case — go process the next file.
    ProcessNext,
}

/// Pure decision function for what `continue_batch` should do next, given
/// the current pause/abort flags — extracted so it's unit-testable without
/// a live `AppWindow`/`Connection` (see `batch_summary_message` for the same
/// rationale). Abort takes priority over pause: an aborted-while-paused
/// batch must still finish, not sit frozen forever waiting for a resume
/// that will never un-abort it.
fn batch_step(aborted: bool, paused: bool) -> BatchStep {
    if aborted {
        BatchStep::Finish
    } else if paused {
        BatchStep::WaitPaused
    } else {
        BatchStep::ProcessNext
    }
}

#[cfg(feature = "slint-ui")]
fn continue_batch(
    h: &AppWindow,
    state_ref: &Arc<Mutex<ui::AppState>>,
    db_ref: &Arc<Mutex<Option<rusqlite::Connection>>>,
) {
    let (aborted, paused) = {
        let st = state_ref.lock().unwrap();
        match st.batch_progress.as_ref() {
            Some(bp) => (bp.aborted, bp.paused),
            None => return,
        }
    };
    match batch_step(aborted, paused) {
        BatchStep::Finish => {
            finish_batch(h, state_ref, db_ref);
            return;
        }
        // Stop making progress and don't reschedule — on_do_batch_pause's
        // "Resume" click is what calls continue_batch again to pick back up.
        BatchStep::WaitPaused => return,
        BatchStep::ProcessNext => {}
    }

    let next_path = {
        let mut st = state_ref.lock().unwrap();
        match st.batch_progress.as_mut() {
            Some(bp) => bp.remaining.pop_front(),
            None => return,
        }
    };
    let Some(path) = next_path else {
        finish_batch(h, state_ref, db_ref);
        return;
    };
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

    match try_parse_batch_file(&path, &file_name) {
        BatchFileOutcome::Parsed(r) => {
            let mut st = state_ref.lock().unwrap();
            if let Some(bp) = st.batch_progress.as_mut() {
                record_batch_success(bp, db_ref, &path, &file_name, r);
            }
        }
        BatchFileOutcome::Failed => {
            let mut st = state_ref.lock().unwrap();
            if let Some(bp) = st.batch_progress.as_mut() {
                record_batch_failure(bp, &file_name, "Parse failed");
            }
        }
        BatchFileOutcome::NeedsPassword => {
            {
                let mut st = state_ref.lock().unwrap();
                st.pending_pdf_path = Some(path.clone());
                st.pending_pdf_name = file_name.clone();
            }
            h.set_pdf_pwd_visible(true);
            h.set_pdf_pwd_prompt(SharedString::from(
                format!("'{}' is password-protected. Enter the PDF password:", file_name).as_str(),
            ));
            h.set_status_bank(SharedString::from(
                format!("PDF password required for {}\u{2026}", file_name).as_str(),
            ));
            return; // paused — resumes via on_do_pdf_pwd_confirm/on_do_pdf_pwd_cancel
        }
    }

    let handle2    = h.as_weak();
    let state_ref2 = state_ref.clone();
    let db_ref2    = db_ref.clone();
    // `Timer::single_shot` is a free function backed by a thread-local timer
    // registry, not tied to any handle's lifetime — unlike the instance
    // `Timer::start()` API, where the `Timer` must stay alive for the
    // duration or its `Drop` impl deregisters the callback before it fires.
    slint::Timer::single_shot(std::time::Duration::from_millis(0), move || {
        if let Some(h2) = handle2.upgrade() {
            continue_batch(&h2, &state_ref2, &db_ref2);
        }
    });
}

/// Maps the Settings screen's "Log Level" choice to a `log` crate filter.
fn log_level_filter(level: &str) -> log::LevelFilter {
    match level {
        "DEBUG" => log::LevelFilter::Debug,
        "WARN"  => log::LevelFilter::Warn,
        "ERROR" => log::LevelFilter::Error,
        _       => log::LevelFilter::Info,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // env_logger's own internal filter is fixed for the life of the process
    // once `.init()` runs, so it's built permissive ("debug") here — actual
    // verbosity is controlled entirely afterwards via `log::set_max_level`,
    // which the `log` crate's macros check as a hard ceiling on every call and
    // which can be (and is, below and in on_do_settings_save_all) changed at
    // any time. That's what lets the Settings screen's "Log Level" actually
    // take effect immediately, in both directions, without a restart. An
    // explicit `RUST_LOG` env var still overrides this, same as before.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("debug"),
    )
    .init();
    log::set_max_level(log::LevelFilter::Info); // matches the previous fixed default until Settings load below

    log::info!("Bank Statement Processor starting…");

    let db_path = {
        let mut p = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("."));
        p.pop();
        p.push("bsp_data.db");
        p
    };
    // Captured so the UI can surface a startup DB failure to the user below,
    // instead of it being visible only in the log (the previous behavior —
    // the app would silently run in a no-database mode with no on-screen
    // indication anything was wrong). See db/encryption.rs's RUNTIME
    // DEPENDENCY comment for why a *missing crypto DLL* specifically can
    // never reach this point at all (the process wouldn't have launched);
    // this path is for every other way db::open() can fail.
    let mut db_open_error: Option<String> = None;
    let db_conn: Arc<Mutex<Option<rusqlite::Connection>>> = Arc::new(Mutex::new(
        match db::open(&db_path) {
            Ok(c) => {
                log::info!("Database ready at {:?}", db_path);
                log::info!("[db] {}", db::diagnostics(&c));
                Some(c)
            }
            Err(err) => {
                log::error!("Database init failed: {}", err);
                db_open_error = Some(err.to_string());
                None
            }
        }
    ));

    // ── Slint UI ──────────────────────────────────────────────────────────────
    #[cfg(feature = "slint-ui")]
    {
        let app = AppWindow::new()?;
        // Login screen shown by default (logged-in = false, set by Slint default)

        if let Some(err) = &db_open_error {
            app.set_toast_msg(SharedString::from(
                format!("Database unavailable — imports/saves will not work until this is fixed: {err}").as_str(),
            ));
            app.set_toast_kind(2);
        }

        let app_state: Arc<Mutex<ui::AppState>> =
            Arc::new(Mutex::new(ui::AppState { dedup_enabled: true, ..Default::default() }));

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
                app.set_settings_state_idx(cfg.default_state_idx);
                log::set_max_level(log_level_filter(&cfg.log_level));
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

                h.set_login_loading(true);
                let ok = auth::validate_credentials(&email, &password);
                h.set_login_loading(false);
                if ok {
                    log::info!("Login successful for {}", email);
                    h.set_logged_in(true);
                    h.set_login_error("".into());
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
                    if remaining == 0 {
                        // Auto-quit ~1.5s after the 3rd failed attempt, matching old
                        // app's lockout (login.html:196-201, main.js:222-229) instead
                        // of leaving a permanently-exhausted, un-retryable screen up.
                        h.set_login_closing(true);
                        slint::Timer::single_shot(std::time::Duration::from_millis(1500), || {
                            let _ = slint::quit_event_loop();
                        });
                    }
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
                    .add_filter("Bank Statements", &["pdf", "xlsx", "xls", "xlsm", "png", "jpg", "jpeg", "tiff", "tif", "bmp"])
                    .add_filter("PDF", &["pdf"])
                    .add_filter("Excel", &["xlsx", "xls", "xlsm"])
                    .add_filter("Images (OCR)", &["png", "jpg", "jpeg", "tiff", "tif", "bmp"])
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

                // Excel and (non-scanned) PDF parsing are fast enough to stay
                // synchronous on the UI thread — only the genuinely slow paths
                // (Tesseract OCR, which can take seconds per page) are
                // backgrounded below.
                if ["xlsx", "xls", "xlsm"].contains(&ext.as_str()) {
                    match parser::excel_parser::parse_excel_file(&path) {
                        Ok(r) => {
                            log::info!("Excel OK: {} rows", r.transactions.len());
                            finish_load_file(&h, &state_ref, &db_ref, r, &file_name);
                        }
                        Err(e) => {
                            log::error!("Excel parse error: {}", e);
                            h.set_status_bank(SharedString::from("Excel parse error — see log"));
                        }
                    }
                    return;
                }

                if ext == "pdf" {
                    // Stage 1: structured row parsing — fast, no OCR.
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

                    if let Some(r) = stage1 {
                        finish_load_file(&h, &state_ref, &db_ref, r, &file_name);
                        return;
                    }

                    // Stage 2: needs OCR — background thread, mirrors the
                    // AI-classify pattern (main.rs on_do_ai_classify) of
                    // spawning + marshaling results back via
                    // invoke_from_event_loop instead of blocking the UI thread.
                    h.set_ocr_visible(true);
                    h.set_ocr_msg(SharedString::from("Scanned PDF — running OCR…"));
                    h.set_ocr_pct(0);
                    let handle2     = handle.clone();
                    let state_ref2  = state_ref.clone();
                    let db_ref2     = db_ref.clone();
                    let path2       = path.clone();
                    let file_name2  = file_name.clone();
                    std::thread::spawn(move || {
                        let progress_handle = handle2.clone();
                        let outcome = run_pdf_ocr_pipeline(&path2, &file_name2, move |pct, msg| {
                            let h = progress_handle.clone();
                            let msg = msg.to_string();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(h) = h.upgrade() {
                                    h.set_ocr_pct(pct);
                                    h.set_ocr_msg(SharedString::from(msg.as_str()));
                                }
                            });
                        });
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(h2) = handle2.upgrade() else { return };
                            h2.set_ocr_visible(false);
                            match outcome {
                                Ok(r) => finish_load_file(&h2, &state_ref2, &db_ref2, r, &file_name2),
                                Err(msg) => h2.set_status_bank(SharedString::from(msg.as_str())),
                            }
                        });
                    });
                    return;
                }

                if parser::ocr_extractor::IMAGE_EXTS.contains(&ext.as_str()) {
                    h.set_ocr_visible(true);
                    h.set_ocr_msg(SharedString::from("Image — running OCR…"));
                    h.set_ocr_pct(0);
                    let handle2     = handle.clone();
                    let state_ref2  = state_ref.clone();
                    let db_ref2     = db_ref.clone();
                    let path2       = path.clone();
                    let file_name2  = file_name.clone();
                    std::thread::spawn(move || {
                        let progress_handle = handle2.clone();
                        let outcome = run_image_ocr_pipeline(&path2, &file_name2, move |pct, msg| {
                            let h = progress_handle.clone();
                            let msg = msg.to_string();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(h) = h.upgrade() {
                                    h.set_ocr_pct(pct);
                                    h.set_ocr_msg(SharedString::from(msg.as_str()));
                                }
                            });
                        });
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(h2) = handle2.upgrade() else { return };
                            h2.set_ocr_visible(false);
                            match outcome {
                                Ok(r) => finish_load_file(&h2, &state_ref2, &db_ref2, r, &file_name2),
                                Err(msg) => h2.set_status_bank(SharedString::from(msg.as_str())),
                            }
                        });
                    });
                    return;
                }

                log::warn!("Unsupported extension: {}", ext);
                h.set_status_bank(SharedString::from("Unsupported file type"));
            });
        }

        // ── Batch Folder Processing ───────────────────────────────────────────
        // Processing itself lives in continue_batch/finish_batch (module scope)
        // so it can pause mid-batch for a PDF password prompt and resume from
        // on_do_pdf_pwd_confirm/on_do_pdf_pwd_cancel — see those handlers.
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_batch_folder(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };

                let paths = match rfd::FileDialog::new()
                    .set_title("Select Bank Statement Files (multiple)")
                    .add_filter("Bank Statements", &["pdf","xlsx","xls","xlsm","png","jpg","jpeg","tiff","tif","bmp"])
                    .add_filter("Images (OCR)", &["png","jpg","jpeg","tiff","tif","bmp"])
                    .pick_files()
                {
                    Some(p) if !p.is_empty() => p,
                    _ => return,
                };

                let (all_txns, batch_client_id) = {
                    let st = state_ref.lock().unwrap();
                    (st.transactions.clone(), st.client_id)
                };
                let persisted_hashes: std::collections::HashSet<String> = batch_client_id
                    .and_then(|cid| {
                        let db = db_ref.lock().unwrap();
                        db.as_ref().and_then(|conn| db::get_dedupe_hashes(conn, cid).ok())
                    })
                    .unwrap_or_default();

                {
                    let mut st = state_ref.lock().unwrap();
                    st.batch_progress = Some(ui::BatchProgress {
                        remaining: paths.into_iter().collect(),
                        all_txns,
                        loaded: 0,
                        skipped: 0,
                        errors: 0,
                        first_bank: String::new(),
                        first_ob: None,
                        new_import_ids: vec![],
                        batch_results: vec![],
                        persisted_hashes,
                        client_id: batch_client_id,
                        paused: false,
                        aborted: false,
                    });
                }
                h.set_batch_running(true);
                h.set_batch_paused(false);
                continue_batch(&h, &state_ref, &db_ref);
            });
        }
        // ── Batch Pause/Resume, Abort ────────────────────────────────────────────
        // Real pause/abort, matching the old app's BatchProcessor.pause()/
        // resume()/abort() (a single toggling "Pause"/"Resume" button plus a
        // separate "Abort") — see continue_batch's doc comment for why this
        // only works now that batch processing yields to the event loop
        // between files instead of running as one blocking synchronous call.
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_batch_pause(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let now_paused = {
                    let mut st = state_ref.lock().unwrap();
                    match st.batch_progress.as_mut() {
                        Some(bp) => {
                            bp.paused = !bp.paused;
                            bp.paused
                        }
                        None => return,
                    }
                };
                h.set_batch_paused(now_paused);
                if now_paused {
                    h.set_status_bank(SharedString::from("Batch paused"));
                } else {
                    h.set_status_bank(SharedString::from("Batch resumed\u{2026}"));
                    continue_batch(&h, &state_ref, &db_ref);
                }
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_batch_abort(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                {
                    let mut st = state_ref.lock().unwrap();
                    match st.batch_progress.as_mut() {
                        Some(bp) => bp.aborted = true,
                        None => return,
                    }
                }
                // Takes effect immediately rather than waiting for
                // continue_batch's own aborted-check on its next tick —
                // finish_batch's `.take()` makes any already-scheduled
                // continue_batch timer callback a safe no-op (it will see
                // batch_progress == None and return without doing anything).
                finish_batch(&h, &state_ref, &db_ref);
            });
        }
        // ── New Client ────────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            // Returns true on success (Slint only clears/closes the modal then) — matches
            // old app's _saveClient(), which keeps modalNewClient open and toasts on any
            // validation failure (app.js:359-368) instead of silently discarding input.
            app.on_do_new_client(move || -> bool {
                let h = match handle.upgrade() { Some(h) => h, None => return false };
                let name   = h.get_new_client_name().to_string();
                let ledger = h.get_new_client_ledger().to_string();
                if name.trim().is_empty() {
                    h.set_toast_msg(SharedString::from("Enter client name"));
                    h.set_toast_kind(2);
                    return false;
                }
                if ledger.trim().is_empty() {
                    h.set_toast_msg(SharedString::from("Enter Tally bank ledger name"));
                    h.set_toast_kind(2);
                    return false;
                }
                {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        if let Ok(clients) = db::get_clients(conn) {
                            if clients.iter().any(|c| c.name.eq_ignore_ascii_case(name.trim())) {
                                drop(db);
                                h.set_toast_msg(SharedString::from("Client already exists"));
                                h.set_toast_kind(2);
                                return false;
                            }
                        }
                    }
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
                let Some(id) = new_id else {
                    h.set_toast_msg(SharedString::from("Failed to create client"));
                    h.set_toast_kind(2);
                    return false;
                };
                // Update AppState and sync dashboard properties so Edit Client modal pre-fills
                {
                    let mut st = state_ref.lock().unwrap();
                    st.client_id     = Some(id);
                    st.client_name   = name.trim().to_string();
                    st.tally_ledger  = ledger.trim().to_string();
                }
                h.set_dash_client_name(SharedString::from(name.trim()));
                h.set_dash_client_ledger(SharedString::from(ledger.trim()));
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
                h.set_toast_msg(SharedString::from(format!("Client \"{}\" created", name.trim())));
                h.set_toast_kind(1);
                true
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
                // Load stored rules + settings from DB
                let (rules, cfg) = {
                    let db = db_ref.lock().unwrap();
                    match db.as_ref() {
                        Some(conn) => (db::get_rules(conn, client_id).unwrap_or_default(), settings::Settings::load(conn)),
                        None       => (vec![], settings::Settings::default()),
                    }
                };
                let dedup_on2 = h.get_dedup_enabled();
                let changed = classifier::classify_all(&mut st.transactions, &bank_ledger, &rules, dedup_on2, cfg.gst_enabled, cfg.gst_auto_ledgers);
                parser::party_master::normalize_vendors(&mut st.transactions);
                log::info!("[AutoClassify] classified {} transactions (rules={})", changed, rules.len());
                rebuild_rows(&h, &st);
                push_dashboard(&h, &st.transactions, st.opening_balance);
                push_summary_extras(&h, &st.transactions);
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
                    let scope        = ai_classifier::AiScope::from_idx(h.get_ai_scope_idx());
                    let handle2      = h.as_weak();
                    let state_ref2   = state_ref.clone();
                    // Reset (and grab a clone of) the shared cancel flag before this run
                    // starts — on_do_ai_cancel sets it from the UI thread; the spawned
                    // thread below checks it between AI batches.
                    let cancel_flag = {
                        let st = state_ref.lock().unwrap();
                        st.ai_cancel.store(false, std::sync::atomic::Ordering::Relaxed);
                        st.ai_cancel.clone()
                    };
                    h.set_ai_overlay_visible(true);
                    h.set_ai_msg("Classifying with AI…".into());
                    h.set_ai_pct(0);
                    std::thread::spawn(move || {
                        let mut txns = { state_ref2.lock().unwrap().transactions.clone() };
                        let result = ai_classifier::classify_with_ai(
                            &mut txns,
                            provider,
                            &api_key,
                            scope,
                            &cancel_flag,
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
                        // Old app always toasts the outcome of an AI-classify run
                        // (app.js:3721-3729: success/cancelled/error) — mirror that
                        // here instead of leaving failures visible only in the log.
                        let was_cancelled = cancel_flag.load(std::sync::atomic::Ordering::Relaxed);
                        let (toast_msg, toast_kind) = match &result {
                            Ok(n) if was_cancelled => {
                                log::info!("[AIClassify] cancelled — {} transactions classified before stopping", n);
                                ("AI classification cancelled".to_string(), 3)
                            }
                            Ok(n) => {
                                log::info!("[AIClassify] classified {} transactions", n);
                                (format!("AI classified {} transaction(s)", n), 1)
                            }
                            Err(e) => {
                                log::error!("[AIClassify] error: {}", e);
                                (format!("AI error: {}", e), 2)
                            }
                        };
                        let handle3 = handle2.clone();
                        let state_ref3 = state_ref2.clone();
                        let txns_done = txns;
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(h2) = handle3.upgrade() {
                                let mut st = state_ref3.lock().unwrap();
                                st.transactions = txns_done;
                                parser::party_master::normalize_vendors(&mut st.transactions);
                                h2.set_ai_overlay_visible(false);
                                rebuild_rows(&h2, &st);
                                push_dashboard(&h2, &st.transactions, st.opening_balance);
                                push_summary_extras(&h2, &st.transactions);
                                h2.set_toast_msg(SharedString::from(toast_msg.as_str()));
                                h2.set_toast_kind(toast_kind);
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
        // ── Delete Rule ───────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_delete_rule(move |idx| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let st = state_ref.lock().unwrap();
                let client_id = st.client_id.unwrap_or(0);
                drop(st);
                let db = db_ref.lock().unwrap();
                let Some(conn) = db.as_ref() else { return };
                let rules = match db::get_rules(conn, client_id) {
                    Ok(r) => r,
                    Err(e) => { log::error!("[DeleteRule] DB error: {}", e); return; }
                };
                let Some(rule) = rules.get(idx as usize) else {
                    log::warn!("[DeleteRule] idx {} out of range (len={})", idx, rules.len());
                    return;
                };
                if let Err(e) = db::delete_rule(conn, rule.id) {
                    log::error!("[DeleteRule] failed to delete rule {}: {}", rule.id, e);
                    return;
                }
                log::info!("[DeleteRule] deleted rule id={}", rule.id);
                match db::get_rules(conn, client_id) {
                    Ok(remaining) => {
                        let recs: Vec<SharedString> = remaining.iter().map(|r| {
                            SharedString::from(format!(
                                "{}  |  {}  |  {}  |  {}",
                                r.pattern,
                                if r.vendor.is_empty() { "—" } else { &r.vendor },
                                if r.account_head.is_empty() { "—" } else { &r.account_head },
                                if r.txn_type.is_empty() { "—" } else { &r.txn_type },
                            ).as_str())
                        }).collect();
                        h.set_rule_records(slint::ModelRc::new(slint::VecModel::from(recs)));
                    }
                    Err(e) => log::error!("[DeleteRule] reload DB error: {}", e),
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
        // ── Delete Import ────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_delete_import(move |idx| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let st = state_ref.lock().unwrap();
                let client_id = st.client_id.unwrap_or(0);
                drop(st);
                let db = db_ref.lock().unwrap();
                let Some(conn) = db.as_ref() else { return };
                let imports = match db::get_imports(conn, client_id) {
                    Ok(i) => i,
                    Err(e) => { log::error!("[DeleteImport] DB error: {}", e); return; }
                };
                let Some(import) = imports.get(idx as usize) else {
                    log::warn!("[DeleteImport] idx {} out of range (len={})", idx, imports.len());
                    return;
                };
                if let Err(e) = db::delete_import(conn, import.id) {
                    log::error!("[DeleteImport] failed to delete import {}: {}", import.id, e);
                    return;
                }
                log::info!("[DeleteImport] deleted import id={}", import.id);
                drop(db);
                let mut st = state_ref.lock().unwrap();
                st.import_ids.retain(|&id| id != import.id);
                drop(st);
                let db = db_ref.lock().unwrap();
                let Some(conn) = db.as_ref() else { return };
                match db::get_imports(conn, client_id) {
                    Ok(remaining) => {
                        let recs: Vec<SharedString> = remaining.iter().map(|imp| {
                            let date = &imp.imported_at[..imp.imported_at.len().min(16)];
                            SharedString::from(format!(
                                "{}  |  {}  |  {} transactions",
                                imp.file_name, date, imp.txn_count
                            ).as_str())
                        }).collect();
                        h.set_import_records(slint::ModelRc::new(slint::VecModel::from(recs)));
                    }
                    Err(e) => log::error!("[DeleteImport] reload DB error: {}", e),
                }
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_import_ledgers(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let cid = { state_ref.lock().unwrap().client_id.unwrap_or(0) };
                if cid == 0 {
                    h.set_toast_msg(SharedString::from("Select a client first"));
                    h.set_toast_kind(2);
                    return;
                }
                let path = match rfd::FileDialog::new()
                    .set_title("Import Ledgers (Excel or CSV)")
                    .add_filter("Excel / CSV", &["xlsx", "xls", "xlsm", "csv"])
                    .pick_file()
                {
                    Some(p) => p,
                    None    => return,
                };

                let is_csv = path.extension().and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("csv")).unwrap_or(false);

                let rows: Vec<Vec<String>> = if is_csv {
                    let mut reader = match csv::ReaderBuilder::new()
                        .has_headers(false)
                        .flexible(true)
                        .from_path(&path)
                    {
                        Ok(r) => r,
                        Err(e) => {
                            log::error!("[ImportLedgers] CSV open failed: {}", e);
                            h.set_toast_msg(SharedString::from("Could not open file"));
                            h.set_toast_kind(2);
                            return;
                        }
                    };
                    reader.records()
                        .filter_map(|rec| rec.ok())
                        .map(|rec| rec.iter().map(|c| c.trim().to_string()).collect())
                        .collect()
                } else {
                    use calamine::{open_workbook_auto, Reader};
                    let mut wb = match open_workbook_auto(&path) {
                        Ok(wb) => wb,
                        Err(e) => {
                            log::error!("[ImportLedgers] open failed: {}", e);
                            h.set_toast_msg(SharedString::from("Could not open file"));
                            h.set_toast_kind(2);
                            return;
                        }
                    };
                    let sheet_name = match wb.sheet_names().first() {
                        Some(n) => n.clone(),
                        None => { h.set_toast_msg(SharedString::from("File has no sheets")); h.set_toast_kind(2); return; }
                    };
                    let range = match wb.worksheet_range(&sheet_name) {
                        Ok(r) => r,
                        Err(e) => {
                            log::error!("[ImportLedgers] sheet error: {}", e);
                            h.set_toast_msg(SharedString::from("Could not read sheet"));
                            h.set_toast_kind(2);
                            return;
                        }
                    };
                    range.rows().map(|row| {
                        row.iter().map(|c| c.to_string().trim().to_string()).collect()
                    }).collect()
                };

                if rows.len() < 2 {
                    h.set_toast_msg(SharedString::from("File appears empty"));
                    h.set_toast_kind(2);
                    return;
                }

                let header: Vec<String> = rows[0].iter().map(|s| s.to_lowercase()).collect();
                let name_keys  = ["name","ledger name","ledger","ledgername"];
                let group_keys = ["under","group","under group","ledger group","group name","parent"];
                let name_col  = header.iter().position(|h| name_keys.iter().any(|k| h == k));
                let group_col = header.iter().position(|h| group_keys.iter().any(|k| h == k));

                let name_col = match name_col {
                    Some(c) => c,
                    None => {
                        h.set_toast_msg(SharedString::from("No 'Name' or 'Ledger Name' column found"));
                        h.set_toast_kind(2);
                        return;
                    }
                };

                let mut entries: Vec<(String, String)> = Vec::new();
                for row in rows.iter().skip(1) {
                    let name  = row.get(name_col).cloned().unwrap_or_default();
                    let group = group_col.and_then(|c| row.get(c)).cloned().unwrap_or_default();
                    if !name.is_empty() { entries.push((name, group)); }
                }

                if entries.is_empty() {
                    h.set_toast_msg(SharedString::from("No ledger rows found"));
                    h.set_toast_kind(2);
                    return;
                }

                let db = db_ref.lock().unwrap();
                if let Some(conn) = db.as_ref() {
                    match db::import_ledgers(conn, cid, &entries) {
                        Ok(added) => {
                            let total = entries.len();
                            let msg = format!("Imported {} new ledgers ({} already existed)", added, total - added);
                            log::info!("[ImportLedgers] {}", msg);
                            h.set_toast_msg(SharedString::from(msg.as_str()));
                            h.set_toast_kind(1);
                        }
                        Err(e) => {
                            log::error!("[ImportLedgers] DB insert failed: {}", e);
                            h.set_toast_msg(SharedString::from("Failed to save ledgers to database"));
                            h.set_toast_kind(2);
                        }
                    }
                }
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_filter_changed(move |f| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();
                let fs = f.to_string();
                if fs == "all" {
                    st.active_filter = "all".to_string();
                    st.filter_statuses.clear();
                } else {
                    // Single-select from drill-down or legacy callers
                    st.active_filter = fs.clone();
                    st.filter_statuses = vec![fs];
                }
                rebuild_rows(&h, &st);
            });
        }
        // ── Toggle status filter (multi-select) ───────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_toggle_status(move |s| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();
                let s = s.to_string();
                if let Some(pos) = st.filter_statuses.iter().position(|x| x == &s) {
                    st.filter_statuses.remove(pos);
                } else {
                    st.filter_statuses.push(s);
                }
                st.active_filter = if st.filter_statuses.is_empty() {
                    "all".to_string()
                } else if st.filter_statuses.len() == 1 {
                    st.filter_statuses[0].clone()
                } else {
                    "multi".to_string()
                };
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
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_vendor_filter(move |vendor| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();
                st.vendor_filter = vendor.to_string();
                rebuild_rows(&h, &st);
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_head_filter(move |head| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();
                st.head_filter = head.to_string();
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
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_export_tally(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let st = state_ref.lock().unwrap();
                if st.transactions.is_empty() {
                    log::warn!("[ExportTally] No transactions to export");
                    h.set_toast_msg(SharedString::from("No transactions to export"));
                    h.set_toast_kind(2);
                    return;
                }
                let company = h.get_wiz_company().to_string();
                let client_name = if company.is_empty() { st.client_name.clone() } else { company };
                if client_name.trim().is_empty() {
                    log::warn!("[ExportTally] company name is empty");
                    h.set_toast_msg(SharedString::from("Enter Tally company name"));
                    h.set_toast_kind(2);
                    return;
                }
                let gstin   = h.get_wiz_gstin().to_string();
                let from    = h.get_wiz_date_from().to_string();
                let to      = h.get_wiz_date_to().to_string();
                let opts = export::tally::TallyOpts {
                    company:            client_name.clone(),
                    gstin,
                    fy:                 String::new(),
                    bank_ledger:        st.tally_ledger.clone(),
                    date_from:          if from.is_empty() { None } else { Some(from) },
                    date_to:            if to.is_empty()   { None } else { Some(to) },
                    only_classified:    h.get_wiz_opt_classified(),
                    include_ledgers:    h.get_wiz_opt_ledger(),
                    include_narrations: h.get_wiz_opt_narr(),
                    include_ob:         h.get_wiz_opt_ob(),
                    skip_low_conf:      h.get_wiz_opt_skip_low(),
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
                    Ok(_) => {
                        log::info!("[ExportTally] wrote {:?}", path);
                        h.set_toast_msg(SharedString::from("Tally XML exported"));
                        h.set_toast_kind(1);
                    }
                    Err(e) => {
                        log::error!("[ExportTally] write failed: {}", e);
                        h.set_toast_msg(SharedString::from(format!("Export error: {}", e)));
                        h.set_toast_kind(2);
                    }
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

                const FY_OPTS:    [&str; 4] = ["2024-25", "2023-24", "2022-23", "2021-22"];
                const STATE_OPTS: [&str; 9] = ["MH", "GJ", "DL", "KA", "TN", "TG", "RJ", "UP", "WB"];
                let fy = FY_OPTS.get(h.get_wiz_fy_idx() as usize).copied().unwrap_or("2024-25").to_string();
                let state_code = STATE_OPTS.get(h.get_wiz_state_idx() as usize).copied().unwrap_or("MH").to_string();

                let software = export::accounting::Software::from_idx(sw_idx);
                let opts = export::accounting::AccountingOpts {
                    software,
                    company:            if company.is_empty() { st.client_name.clone() } else { company },
                    gstin,
                    fy,
                    state_code,
                    currency:           "INR".to_string(),
                    bank_ledger:        st.tally_ledger.clone(),
                    date_from:          if from.is_empty() { None } else { Some(from) },
                    date_to:            if to.is_empty()   { None } else { Some(to) },
                    include_ob:         h.get_wiz_opt_ob(),
                    include_gst:        h.get_wiz_opt_gst(),
                    include_ledgers:    h.get_wiz_opt_ledger(),
                    include_narrations: h.get_wiz_opt_narr(),
                    only_classified:    h.get_wiz_opt_classified(),
                    skip_low_conf:      h.get_wiz_opt_skip_low(),
                };

                let validation = export::accounting::validate(&st.transactions, &opts);
                if !validation.can_export() {
                    let msg = format!("Cannot export: {}", validation.errors.join("; "));
                    log::warn!("[ExportAccounting] blocked: {}", msg);
                    h.set_toast_msg(SharedString::from(msg.as_str()));
                    h.set_toast_kind(2);
                    return;
                }
                if !validation.warnings.is_empty() {
                    log::warn!("[ExportAccounting] warnings: {}", validation.warnings.join("; "));
                }

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

        // ── Export wizard preview ─────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_export_preview(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let st = state_ref.lock().unwrap();
                if st.transactions.is_empty() {
                    h.set_export_preview_text(SharedString::from("No transactions loaded"));
                    h.set_wiz_can_export(false);
                    return;
                }
                let sw_idx  = h.get_wiz_sw_idx();
                let company = h.get_wiz_company().to_string();
                let gstin   = h.get_wiz_gstin().to_string();
                let from = h.get_wiz_date_from().to_string();
                let to   = h.get_wiz_date_to().to_string();

                const FY_OPTS:    [&str; 4] = ["2024-25", "2023-24", "2022-23", "2021-22"];
                const STATE_OPTS: [&str; 9] = ["MH", "GJ", "DL", "KA", "TN", "TG", "RJ", "UP", "WB"];
                let fy = FY_OPTS.get(h.get_wiz_fy_idx() as usize).copied().unwrap_or("2024-25").to_string();
                let state_code = STATE_OPTS.get(h.get_wiz_state_idx() as usize).copied().unwrap_or("MH").to_string();

                let software = export::accounting::Software::from_idx(sw_idx);
                let opts = export::accounting::AccountingOpts {
                    software,
                    company:            if company.is_empty() { st.client_name.clone() } else { company },
                    gstin,
                    fy,
                    state_code,
                    currency:           "INR".to_string(),
                    bank_ledger:        st.tally_ledger.clone(),
                    date_from:          if from.is_empty() { None } else { Some(from) },
                    date_to:            if to.is_empty()   { None } else { Some(to) },
                    include_ob:         h.get_wiz_opt_ob(),
                    include_gst:        h.get_wiz_opt_gst(),
                    include_ledgers:    h.get_wiz_opt_ledger(),
                    include_narrations: h.get_wiz_opt_narr(),
                    only_classified:    h.get_wiz_opt_classified(),
                    skip_low_conf:      h.get_wiz_opt_skip_low(),
                };

                let validation = export::accounting::validate(&st.transactions, &opts);

                let tally_opts = export::tally::TallyOpts {
                    only_classified: opts.only_classified,
                    skip_low_conf:   opts.skip_low_conf,
                    date_from: opts.date_from.clone(),
                    date_to:   opts.date_to.clone(),
                    ..Default::default()
                };
                let p = export::tally::count_preview(&st.transactions, &tally_opts);
                let mut parts = vec![
                    format!("{} transactions", p.total),
                    format!("{} Payments", p.payment),
                    format!("{} Receipts", p.receipt),
                ];
                if p.contra > 0 { parts.push(format!("{} Contra", p.contra)); }
                if p.gst    > 0 { parts.push(format!("{} GST (\u{20b9}{:.2})", p.gst, p.gst_amount)); }
                if p.skipped > 0 { parts.push(format!("{} skipped", p.skipped)); }
                let mut text = parts.join("  •  ");
                if !validation.errors.is_empty() {
                    text.push_str(&format!("\nErrors: {}", validation.errors.join("; ")));
                }
                if !validation.warnings.is_empty() {
                    text.push_str(&format!("\nWarnings: {}", validation.warnings.join("; ")));
                }
                h.set_export_preview_text(SharedString::from(text.as_str()));
                h.set_wiz_can_export(validation.can_export());
                log::info!("[ExportPreview] {}", text);
            });
        }

        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_reimport_excel(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let path = match rfd::FileDialog::new()
                    .set_title("Re-import Classified Excel")
                    .add_filter("Excel", &["xlsx", "xls", "xlsm"])
                    .pick_file()
                {
                    Some(p) => p,
                    None    => return,
                };

                use calamine::{open_workbook_auto, Reader};
                let mut wb = match open_workbook_auto(&path) {
                    Ok(wb) => wb,
                    Err(e) => {
                        h.set_toast_msg(SharedString::from(format!("Cannot open file: {}", e).as_str()));
                        h.set_toast_kind(2);
                        return;
                    }
                };

                let range = match wb.worksheet_range("Transactions") {
                    Ok(r) => r,
                    Err(_) => {
                        h.set_toast_msg(SharedString::from("'Transactions' sheet not found — export from this app first"));
                        h.set_toast_kind(2);
                        return;
                    }
                };

                let rows: Vec<Vec<String>> = range.rows().map(|row| {
                    row.iter().map(|c| c.to_string().trim().to_string()).collect()
                }).collect();

                if rows.len() < 2 {
                    h.set_toast_msg(SharedString::from("Transactions sheet is empty"));
                    h.set_toast_kind(2);
                    return;
                }

                let hdr: Vec<String> = rows[0].iter().map(|s| s.to_lowercase()).collect();
                let col = |k: &str| hdr.iter().position(|h| h == k);

                let i_date = col("date");
                let i_narr = col("narration");
                let i_ref  = col("reference");
                let i_dr   = col("debit");
                let i_cr   = col("credit");
                let i_bal  = col("balance");
                let i_vend = col("vendor");
                let i_head = col("account head");
                let i_type = col("type");
                let i_stat = col("status");
                let i_tags = col("tags");
                let i_conf = col("confidence");
                let i_bank = col("bank name");
                let i_acct = col("account no");

                if i_date.is_none() || i_narr.is_none() {
                    h.set_toast_msg(SharedString::from("Date or Narration column missing"));
                    h.set_toast_kind(2);
                    return;
                }
                if i_vend.is_none() || i_head.is_none() {
                    h.set_toast_msg(SharedString::from("Vendor and Account Head columns required — export from this app first"));
                    h.set_toast_kind(2);
                    return;
                }

                let parse_amt = |s: &str| -> Option<f64> {
                    if s.is_empty() { return None; }
                    let clean: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-').collect();
                    clean.parse::<f64>().ok().filter(|v| *v != 0.0)
                };

                let mut txns: Vec<parser::Transaction> = Vec::new();
                for row in rows.iter().skip(1) {
                    let get = |idx: Option<usize>| idx.and_then(|i| row.get(i)).cloned().unwrap_or_default();
                    let narr = get(i_narr);
                    let date = get(i_date);
                    if narr.is_empty() && date.is_empty() { continue; }

                    let vendor = get(i_vend);
                    let head   = get(i_head);
                    let type_s = get(i_type);
                    let stat_s = get(i_stat).to_lowercase();
                    let conf_s = get(i_conf);
                    let tags_s = get(i_tags);

                    let status = match stat_s.as_str() {
                        "suspense"     => parser::TransactionStatus::Suspense,
                        "classified"   => parser::TransactionStatus::Classified,
                        "needs_review" | "review" => parser::TransactionStatus::NeedsReview,
                        _ => if vendor.is_empty() && head.is_empty() {
                            parser::TransactionStatus::Unreviewed
                        } else {
                            parser::TransactionStatus::Classified
                        },
                    };
                    let confidence: f64 = conf_s.parse().unwrap_or(if matches!(status, parser::TransactionStatus::Classified) { 1.0 } else { 0.0 });
                    let classification_source = if matches!(status, parser::TransactionStatus::Classified) { "user".to_string() } else { String::new() };
                    let txn_type = match type_s.as_str() {
                        "Payment"  | "payment"  => parser::VoucherType::Payment,
                        "Receipt"  | "receipt"  => parser::VoucherType::Receipt,
                        "Contra"   | "contra"   => parser::VoucherType::Contra,
                        "Journal"  | "journal"  => parser::VoucherType::Journal,
                        _ => parser::VoucherType::Payment,
                    };
                    let tags: Vec<String> = tags_s.split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();

                    txns.push(parser::Transaction {
                        id:           format!("ri_{}", txns.len()),
                        import_id:    None,
                        date:         date.clone(),
                        date_ts:      0,
                        narration:    narr,
                        reference:    get(i_ref),
                        debit:        parse_amt(&get(i_dr)),
                        credit:       parse_amt(&get(i_cr)),
                        balance:      parse_amt(&get(i_bal)),
                        vendor:       vendor.clone(),
                        account_head: head.clone(),
                        txn_type,
                        status,
                        confidence,
                        classification_source,
                        tags,
                        bank_name:    get(i_bank),
                        account_no:   get(i_acct),
                        is_opening_balance: false,
                        dup_flag:     false,
                        prev_balance: None,
                        balance_ok:   None,
                        gst_rate:     None,
                        gst_amount:   None,
                        gst_type:     None,
                    });
                }

                if txns.is_empty() {
                    h.set_toast_msg(SharedString::from("No transactions found in sheet"));
                    h.set_toast_kind(2);
                    return;
                }

                let classified_cnt = txns.iter().filter(|t| matches!(t.status, parser::TransactionStatus::Classified)).count();
                let total_cnt = txns.len();
                let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

                {
                    let mut st = state_ref.lock().unwrap();
                    st.transactions  = txns.clone();
                    st.active_filter = "all".to_string();
                    st.filter_statuses.clear();
                    st.date_from     = String::new();
                    st.date_to       = String::new();
                    st.bank_filter   = String::new();
                    st.vendor_filter = String::new();
                    st.head_filter   = String::new();
                    st.file_name     = file_name.clone();
                }

                let result = parser::ParseResult {
                    transactions:       txns,
                    bank_name:          String::new(),
                    account_no:         String::new(),
                    opening_balance:    None,
                    closing_balance:    None,
                    source_name:        file_name.clone(),
                    col_map:            parser::ColumnMap::default(),
                    header_row_idx:     0,
                    noise_row_count:    0,
                    rejected_row_count: 0,
                };
                apply_parse_result(&h, &state_ref, &db_ref, result, &file_name);

                let msg = format!("Re-imported {} transactions ({} pre-classified)", total_cnt, classified_cnt);
                h.set_toast_msg(SharedString::from(msg.as_str()));
                h.set_toast_kind(1);
                log::info!("[ReimportExcel] {}", msg);
            });
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
                push_dashboard(&h, &st.transactions, st.opening_balance);
                h.set_status_bank(SharedString::from(
                    format!("Dedupe reset — {} duplicate(s) detected", dup_count).as_str()
                ));
                log::info!("[ResetDedupe] {} duplicates after reset", dup_count);
                drop(st);
                if let Some(cid) = client_id {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        if let Err(e) = db::update_dup_flags(conn, cid, &txns_snap) {
                            log::error!("[Dedup] failed to persist dup flags: {}", e);
                        }
                        if let Err(e) = db::push_audit_event(conn, cid, &event_str) {
                            log::error!("[Dedup] failed to persist audit event: {}", e);
                        }
                        // Also clears the persisted cross-import dedup history (port of
                        // Electron's btnResetDedupe -> DB.resetDedupeHashes), so previously
                        // loaded statements are no longer treated as duplicates on re-import.
                        if let Err(e) = db::reset_dedupe_hashes(conn, cid) {
                            log::error!("[Dedup] failed to reset dedupe hashes: {}", e);
                        }
                    }
                }
            });
        }

        // ── Reconcile: step 1 — Import Tally Export ────────────────────────────
        // Parses the picked Tally daybook Excel export into vouchers and stores
        // them in AppState. Does not run any matching yet — that's a separate,
        // explicit "Run Reconciliation" step (mirrors the old Electron app's
        // `_openReconcile()` / `_runReconciliation()` split), which lets a user
        // re-run matching (e.g. after adjusting the recon tolerance in
        // Settings) without re-picking the file every time.
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_reconcile(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let path = match rfd::FileDialog::new()
                    .set_title("Open Tally Daybook Export (Excel)")
                    .add_filter("Excel", &["xlsx", "xls", "xlsm"])
                    .pick_file()
                { Some(p) => p, None => return };

                let wb = match calamine::open_workbook_auto(&path) {
                    Ok(w) => w,
                    Err(e) => {
                        log::error!("[Reconcile] cannot open file: {}", e);
                        h.set_toast_msg(SharedString::from("Could not open the Tally export file"));
                        h.set_toast_kind(2);
                        return;
                    }
                };

                let grid = read_workbook_grid(wb);
                let vouchers = reconciliation::parse_tally_grid(&grid);
                if vouchers.is_empty() {
                    h.set_toast_msg(SharedString::from(
                        "No valid entries found. The file needs a Date column and a Particulars/Narration/Description column."));
                    h.set_toast_kind(2);
                    return;
                }

                let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or("Tally export").to_string();
                let label = format!("Loaded {} entries from {}", vouchers.len(), file_name);
                log::info!("[Reconcile] imported {} Tally vouchers from {:?}", vouchers.len(), path);

                {
                    let mut st = state_ref.lock().unwrap();
                    st.recon_vouchers   = vouchers;
                    st.recon_file_label = label.clone();
                    st.recon_csv        = String::new();
                }
                h.set_recon_file_label(SharedString::from(label.as_str()));
                h.set_recon_has_vouchers(true);
                h.set_recon_has_report(false);
                h.set_recon_status(SharedString::from("Ready to reconcile \u{2014} click \"Run Reconciliation\"."));
                h.set_recon_matched(SharedString::from("0"));
                h.set_recon_likely(SharedString::from("0"));
                h.set_recon_possible(SharedString::from("0"));
                h.set_recon_unmatched(SharedString::from("0"));
                h.set_recon_bank_only(SharedString::from("0"));
                h.set_toast_msg(SharedString::from(label.as_str()));
                h.set_toast_kind(1);
            });
        }

        // ── Reconcile: step 2 — Run Reconciliation ─────────────────────────────
        // Matches the currently-loaded bank transactions against the vouchers
        // imported in step 1, using the full tiered-confidence greedy
        // bipartite matcher in `reconciliation.rs` (port of the old app's
        // `ReconciliationEngine.reconcile()` — exact/fuzzy amount+date,
        // narration similarity, reference-number bonus, 4 status tiers).
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();
            app.on_do_run_reconciliation(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };

                let (vouchers, bank) = {
                    let st = state_ref.lock().unwrap();
                    if st.recon_vouchers.is_empty() {
                        drop(st);
                        h.set_toast_msg(SharedString::from("Import a Tally export first"));
                        h.set_toast_kind(2);
                        return;
                    }
                    let bank: Vec<reconciliation::BankEntry> = st.transactions.iter()
                        .filter(|t| !t.is_opening_balance)
                        .map(|t| reconciliation::BankEntry {
                            date:      t.date.clone(),
                            amount:    t.debit.or(t.credit).unwrap_or(0.0),
                            narration: t.narration.clone(),
                            reference: t.reference.clone(),
                        })
                        .collect();
                    (st.recon_vouchers.clone(), bank)
                };

                if bank.is_empty() {
                    h.set_toast_msg(SharedString::from("No bank transactions loaded to reconcile against"));
                    h.set_toast_kind(2);
                    return;
                }

                let (recon_days, recon_pct) = {
                    let dbc = db_ref.lock().unwrap();
                    if let Some(c) = dbc.as_ref() {
                        let cfg = settings::Settings::load(c);
                        (cfg.recon_days as i64, cfg.recon_pct)
                    } else { (3, 0.5) }
                };
                let cfg    = reconciliation::ReconConfig::new(recon_days, recon_pct);
                let report = reconciliation::reconcile(&bank, &vouchers, &cfg);

                let matched            = report.matched_count();
                let likely             = report.likely_count();
                let possible           = report.possible_count();
                let unmatched_vouchers = report.unmatched_vouchers.len();
                let bank_only          = report.unmatched_bank.len();

                let status = format!(
                    "Bank: {} transactions | Tally: {} entries\n\u{2022} Matched: {}\n\u{2022} Likely (\u{00B1}{} days): {}\n\u{2022} Possible: {}\n\u{2022} Tally entries unmatched: {}\n\u{2022} Bank-only entries (no Tally): {}",
                    bank.len(), vouchers.len(), matched, recon_days, likely, possible, unmatched_vouchers, bank_only
                );
                let csv = reconciliation::report_to_csv(&bank, &vouchers, &report);

                h.set_recon_matched(SharedString::from(matched.to_string().as_str()));
                h.set_recon_likely(SharedString::from(likely.to_string().as_str()));
                h.set_recon_possible(SharedString::from(possible.to_string().as_str()));
                h.set_recon_unmatched(SharedString::from(unmatched_vouchers.to_string().as_str()));
                h.set_recon_bank_only(SharedString::from(bank_only.to_string().as_str()));
                h.set_recon_status(SharedString::from(status.as_str()));
                h.set_recon_has_report(true);

                let event_str = format!(
                    "[{}] Reconcile \u{2014} {} matched, {} likely, {} possible, {} unmatched",
                    audit_now(), matched, likely, possible, unmatched_vouchers
                );
                log::info!("[Reconcile] matched={} likely={} possible={} unmatched_vouchers={} bank_only={}",
                    matched, likely, possible, unmatched_vouchers, bank_only);

                let client_id = {
                    let mut st2 = state_ref.lock().unwrap();
                    st2.audit_events.push(event_str.clone());
                    st2.recon_csv = csv;
                    st2.client_id
                };
                h.set_toast_msg(SharedString::from(format!(
                    "Reconciliation complete: {} matched, {} likely, {} possible, {} unmatched",
                    matched, likely, possible, unmatched_vouchers
                ).as_str()));
                h.set_toast_kind(1);

                if let Some(cid) = client_id {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        if let Err(e) = db::push_audit_event(conn, cid, &event_str) {
                            log::error!("[Audit] failed to persist audit event: {}", e);
                        }
                    }
                }
            });
        }

        // ── Legacy Data Migration: step 1 — pick + preview the export file ─────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_migration_pick_file(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let path = match rfd::FileDialog::new()
                    .set_title("Select Legacy Export (JSON)")
                    .add_filter("JSON", &["json"])
                    .pick_file()
                { Some(p) => p, None => return };

                match migration::preview(&path) {
                    Ok(detected) => {
                        let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or("export.json").to_string();
                        let mut lines = vec![format!("File: {}", file_name), String::new(), "Found:".to_string()];
                        for (name, count) in &detected.entity_counts {
                            lines.push(format!("  \u{2022} {}: {}", name, count));
                        }
                        if detected.is_empty() {
                            lines.push(String::new());
                            lines.push("Warning: no records found in any recognized category. Check that this is the right file.".to_string());
                        }
                        let preview_text = lines.join("\n");
                        {
                            let mut st = state_ref.lock().unwrap();
                            st.migration_export_path = Some(path.clone());
                        }
                        h.set_migration_file_label(SharedString::from(file_name.as_str()));
                        h.set_migration_preview_text(SharedString::from(preview_text.as_str()));
                        h.set_migration_has_preview(true);
                        h.set_migration_has_report(false);
                    }
                    Err(e) => {
                        log::error!("[Migration] preview failed: {}", e);
                        h.set_toast_msg(SharedString::from(format!("Could not read export file: {}", e).as_str()));
                        h.set_toast_kind(2);
                        h.set_migration_has_preview(false);
                    }
                }
            });
        }

        // ── Legacy Data Migration: step 2 — run it ──────────────────────────────
        // Runs on a background thread (mirrors the OCR pattern) since a real
        // migration touches disk (backup copy + potentially thousands of
        // transaction rows) and must not freeze the UI. `migration::migrate`
        // owns its own DB connection lifecycle (see its doc comment) — this
        // app's live connection is closed before the call and a fresh one
        // reopened after, since SQLite runs in WAL mode here and replacing
        // the on-disk files underneath an open connection is not safe.
        {
            let handle     = app.as_weak();
            let state_ref  = app_state.clone();
            let db_ref     = db_conn.clone();
            let db_path_mg = db_path.clone();
            app.on_do_migration_run(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let export_path = state_ref.lock().unwrap().migration_export_path.clone();
                let Some(export_path) = export_path else {
                    h.set_toast_msg(SharedString::from("Select an export file first"));
                    h.set_toast_kind(2);
                    return;
                };

                h.set_migration_running(true);
                h.set_migration_pct(0);
                h.set_migration_status(SharedString::from("Starting\u{2026}"));
                h.set_migration_has_report(false);

                {
                    let mut db = db_ref.lock().unwrap();
                    *db = None;
                }

                let handle2    = handle.clone();
                let db_ref2    = db_ref.clone();
                let db_path2   = db_path_mg.clone();
                let state_ref2 = state_ref.clone();
                std::thread::spawn(move || {
                    let progress_handle = handle2.clone();
                    let report = migration::migrate(
                        &export_path,
                        &db_path2,
                        &migration::MigrationOptions::default(),
                        move |pct, phase| {
                            let h   = progress_handle.clone();
                            let msg = phase.to_string();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(h) = h.upgrade() {
                                    h.set_migration_pct(pct);
                                    h.set_migration_status(SharedString::from(msg.as_str()));
                                }
                            });
                        },
                    );

                    // Reopen the app's live connection regardless of outcome
                    // — callers must never leave db_ref permanently empty.
                    let reopened = db::open(&db_path2);
                    if let Err(e) = &reopened {
                        log::error!("[Migration] failed to reopen the database after migrating: {}", e);
                    }
                    {
                        let mut db = db_ref2.lock().unwrap();
                        *db = reopened.ok();
                    }

                    let db_ref3 = db_ref2.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(h2) = handle2.upgrade() else { return };
                        h2.set_migration_running(false);
                        match report {
                            Ok(r) => {
                                let md = r.to_markdown();
                                {
                                    let mut st = state_ref2.lock().unwrap();
                                    st.migration_report_md = md.clone();
                                }
                                h2.set_migration_report_text(SharedString::from(md.as_str()));
                                h2.set_migration_success(r.success);
                                h2.set_migration_has_report(true);
                                h2.set_toast_msg(SharedString::from(r.one_line_summary().as_str()));
                                h2.set_toast_kind(if r.success { 1 } else { 2 });
                                log::info!("[Migration] {}", r.one_line_summary());
                            }
                            Err(e) => {
                                log::error!("[Migration] hard failure: {}", e);
                                h2.set_toast_msg(SharedString::from(format!("Migration could not run: {}", e).as_str()));
                                h2.set_toast_kind(2);
                            }
                        }

                        // Refresh the client dropdown — migration may have
                        // added new clients that should appear immediately.
                        let db = db_ref3.lock().unwrap();
                        if let Some(conn) = db.as_ref() {
                            if let Ok(clients) = db::get_clients(conn) {
                                let names: Vec<SharedString> =
                                    std::iter::once(SharedString::from("-- Select Client --"))
                                    .chain(clients.iter().map(|c| SharedString::from(c.name.as_str())))
                                    .collect();
                                h2.set_client_names(slint::ModelRc::new(slint::VecModel::from(names)));
                            }
                        }
                    });
                });
            });
        }

        // ── Legacy Data Migration: save the full report to disk ────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            app.on_do_migration_save_report(move || {
                let h  = match handle.upgrade() { Some(h) => h, None => return };
                let md = state_ref.lock().unwrap().migration_report_md.clone();
                if md.is_empty() {
                    h.set_toast_msg(SharedString::from("Run a migration first"));
                    h.set_toast_kind(2);
                    return;
                }
                let path = match rfd::FileDialog::new()
                    .set_title("Save Migration Report")
                    .set_file_name("migration-report.md")
                    .add_filter("Markdown", &["md"])
                    .save_file()
                { Some(p) => p, None => return };

                match std::fs::write(&path, md) {
                    Ok(_) => {
                        h.set_toast_msg(SharedString::from(format!("Report saved to {}", path.display()).as_str()));
                        h.set_toast_kind(1);
                    }
                    Err(e) => {
                        log::error!("[Migration] failed to save report: {}", e);
                        h.set_toast_msg(SharedString::from("Could not save the report file"));
                        h.set_toast_kind(2);
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
                    String::new()
                } else {
                    format!(
                        "Session: {} import(s)  |  {} transactions  |  {} classified  |  {} unreviewed",
                        import_count, txn_count, classified, unreviewed
                    )
                };
                h.set_batch_log(SharedString::from(log_msg.as_str()));

                // Build per-file rows model
                let file_rows: Vec<BatchFileRow> = st.batch_file_results.iter().map(|r| BatchFileRow {
                    file:    SharedString::from(r.file.as_str()),
                    bank:    SharedString::from(if r.bank.is_empty() { "Unknown" } else { r.bank.as_str() }),
                    account: SharedString::from(r.account.as_str()),
                    period:  SharedString::from(r.period.as_str()),
                    txns:    r.txns as i32,
                    status:  SharedString::from(if r.ok { "OK" } else { "FAIL" }),
                    is_ok:   r.ok,
                    err_msg: SharedString::from(r.err_msg.as_str()),
                }).collect();
                h.set_batch_file_rows(slint::ModelRc::new(slint::VecModel::from(file_rows)));
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
                    st.filter_statuses.clear();
                    st.date_from       = String::new();
                    st.date_to         = String::new();
                    st.bank_filter     = String::new();
                    st.vendor_filter   = String::new();
                    st.head_filter     = String::new();
                }
                let st = state_ref.lock().unwrap();
                rebuild_rows(&h, &st);
                drop(st);
                if !txns.is_empty() {
                    push_dashboard(&h, &txns, opening_bal);
                    push_summary_extras(&h, &txns);
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

        // ── Undo Last Edit ────────────────────────────────────────────────────
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();

            app.on_do_undo_last_edit(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();

                let entry = match st.undo_stack.pop() {
                    Some(e) => e,
                    None => {
                        log::info!("[Undo] nothing to undo");
                        h.set_can_undo(false);
                        return;
                    }
                };
                h.set_can_undo(!st.undo_stack.is_empty());

                if let Some(pos) = st.transactions.iter().position(|t| t.id == entry.txn_id) {
                    let t = &mut st.transactions[pos];
                    t.vendor       = entry.vendor;
                    t.account_head = entry.head;
                    t.txn_type     = entry.txn_type;
                    t.status       = entry.status;
                    t.confidence   = entry.confidence;

                    let t_id       = t.id.clone();
                    let t_vendor   = t.vendor.clone();
                    let t_head     = t.account_head.clone();
                    let t_type_str = t.txn_type.to_string();
                    let t_status   = t.status.to_string();
                    let t_source   = t.classification_source.clone();
                    let t_conf     = t.confidence;
                    let client_id  = st.client_id;
                    log::info!("[Undo] restored txn id='{}'", t_id);

                    let event_str = format!("[{}] Undo Last Edit — id='{}' vendor='{}' head='{}'",
                        audit_now(), t_id, t_vendor, t_head);
                    st.audit_events.push(event_str.clone());

                    rebuild_rows(&h, &st);
                    push_dashboard(&h, &st.transactions, st.opening_balance);
                    drop(st);

                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        if let Some(cid) = client_id {
                            if let Err(e) = db::upsert_transaction_classification(
                                conn, cid, &t_id, &t_vendor, &t_head, &t_type_str, &t_status, t_conf, &t_source,
                            ) {
                                log::error!("[Undo] failed to persist restored classification: {}", e);
                                h.set_toast_msg(SharedString::from(
                                    format!("Undo shown but not saved: {}", e).as_str()));
                                h.set_toast_kind(2);
                            }
                            if let Err(e) = db::push_audit_event(conn, cid, &event_str) {
                                log::error!("[Undo] failed to persist audit event: {}", e);
                            }
                        } else {
                            log::warn!("[Undo] no active client — restored classification shown but not persisted");
                        }
                    }
                } else {
                    log::warn!("[Undo] txn id='{}' not found", entry.txn_id);
                }
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
                    cfg.default_state_idx = h.get_settings_state_idx();
                    match cfg.save(conn) {
                        Ok(_) => {
                            log::set_max_level(log_level_filter(&cfg.log_level));
                            log::info!("[SettingsSaveAll] saved recon_days={} recon_pct={} log_level={}", cfg.recon_days, cfg.recon_pct, cfg.log_level);
                            h.set_toast_msg(SharedString::from("Settings saved"));
                            h.set_toast_kind(1);
                        }
                        Err(e) => {
                            log::error!("[SettingsSaveAll] error: {}", e);
                            h.set_toast_msg(SharedString::from(format!("Failed to save settings: {}", e)));
                            h.set_toast_kind(2);
                        }
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
                    st.filter_statuses.clear();
                    st.date_from     = String::new();
                    st.date_to       = String::new();
                    st.bank_filter   = String::new();
                    st.vendor_filter = String::new();
                    st.head_filter   = String::new();
                    st.ai_provider   = cfg.ai_provider.clone();
                    st.ai_api_key    = cfg.ai_api_key.clone();
                    st.ai_enabled    = cfg.ai_enabled;
                    // audit_events from DB are already newest-first; store them reversed so
                    // in-memory order matches the push-then-rev pattern used elsewhere
                    st.audit_events  = audit_events.into_iter().rev().collect();
                    // Reconciliation state is per-session and client-specific — a
                    // previously-imported Tally file or match results from another
                    // client must not silently carry over onto this one.
                    st.recon_vouchers   = Vec::new();
                    st.recon_file_label = String::new();
                    st.recon_csv        = String::new();
                }

                // Sync AI settings to UI
                h.set_ai_provider_idx(match cfg.ai_provider.as_str() {
                    "claude" => 1, "gemini" => 2, _ => 0,
                });
                h.set_ai_api_key(SharedString::from(cfg.ai_api_key.as_str()));

                // Reset reconciliation UI back to its pre-import state
                h.set_recon_file_label(SharedString::from(""));
                h.set_recon_has_vouchers(false);
                h.set_recon_has_report(false);
                h.set_recon_status(SharedString::from(""));
                h.set_recon_matched(SharedString::from("0"));
                h.set_recon_likely(SharedString::from("0"));
                h.set_recon_possible(SharedString::from("0"));
                h.set_recon_unmatched(SharedString::from("0"));
                h.set_recon_bank_only(SharedString::from("0"));

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
                    push_summary_extras(&h, &st.transactions);
                }
                drop(st);

                // Persist last used client
                let db = db_ref.lock().unwrap();
                if let Some(conn) = db.as_ref() {
                    if let Err(e) = db::set_setting(conn, settings::KEY_LAST_CLIENT, &client.id.to_string()) {
                        log::error!("[SelectClient] failed to remember last-used client: {}", e);
                    }
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
                    // Cascade deletes transactions/imports/rules via FK ON DELETE CASCADE
                    if let Err(e) = db::delete_client(conn, cid) {
                        log::error!("[DeleteClient] failed to delete client id={}: {}", cid, e);
                        h.set_toast_msg(SharedString::from(
                            format!("Delete failed — client was NOT deleted: {}", e).as_str()));
                        h.set_toast_kind(2);
                        return;
                    }
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
                        if let Err(e) = db::push_audit_event(conn, cid, &event_str) {
                            log::error!("[EditClient] failed to persist audit event: {}", e);
                        }
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
                    st.filter_statuses.clear();
                    st.date_from       = String::new();
                    st.date_to         = String::new();
                    st.bank_filter     = String::new();
                    st.vendor_filter   = String::new();
                    st.head_filter     = String::new();
                }
                let st = state_ref.lock().unwrap();
                rebuild_rows(&h, &st);
                if !txns.is_empty() {
                    push_dashboard(&h, &txns, opening_bal);
                    push_summary_extras(&h, &txns);
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
                let pass = if st.filter_statuses.is_empty() {
                    true
                } else {
                    st.filter_statuses.iter().any(|s| match_status(t, s.as_str()))
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
                    // Push undo snapshot before modifying
                    let undo_entry = {
                        let t = &st.transactions[abs];
                        ui::UndoEntry {
                            txn_id:     t.id.clone(),
                            vendor:     t.vendor.clone(),
                            head:       t.account_head.clone(),
                            txn_type:   t.txn_type.clone(),
                            status:     t.status.clone(),
                            confidence: t.confidence,
                        }
                    };
                    st.undo_stack.push(undo_entry);
                    h.set_can_undo(true);
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
                    // Old app's _saveTxn(learn) sets confidence 1.0 only for Save & Learn;
                    // a plain Save (no rule persisted) gets 0.75 — a real trust signal
                    // that Rust previously collapsed to 1.0 for both (app.js:2273-2334).
                    t.confidence = if learn { 1.0 } else { 0.75 };
                    t.classification_source = "user".to_string();

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
                                    Ok(true)  => log::info!("[SaveLearn] rule saved: pattern='{}' head='{}' vendor='{}'", pattern, head, vendor),
                                    Ok(false) => log::info!("[SaveLearn] rule already exists, skipped: pattern='{}'", pattern),
                                    Err(e)    => log::error!("[SaveLearn] DB error: {}", e),
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
                    let t_source   = t.classification_source.clone();
                    let event_str = format!("[{}] Edit Transaction — id='{}' vendor='{}' head='{}' type='{}'",
                        audit_now(), t_id, t_vendor, t_head, t_type_str);
                    st.audit_events.push(event_str.clone());
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        if let Err(e) = db::upsert_transaction_classification(
                            conn, client_id, &t_id, &t_vendor, &t_head, &t_type_str, &t_status, 1.0, &t_source,
                        ) {
                            log::error!("[SaveTxn] failed to persist classification: {}", e);
                            h.set_toast_msg(SharedString::from(
                                format!("Shown but not saved: {}", e).as_str()));
                            h.set_toast_kind(2);
                        }
                        if let Err(e) = db::push_audit_event(conn, client_id, &event_str) {
                            log::error!("[SaveTxn] failed to persist audit event: {}", e);
                        }
                    }
                    log::info!("[SaveTxn] abs={} vendor='{}' head='{}' type='{}' learn={}", abs, vendor, head, typ, learn);
                }
                rebuild_rows(&h, &st);
                push_dashboard(&h, &st.transactions, st.opening_balance);
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
                push_dashboard(&h, &st.transactions, st.opening_balance);
                drop(st);
                if let Some((cid, event_str, txn_id)) = audit {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        if let Err(e) = db::push_audit_event(conn, cid, &event_str) {
                            log::error!("[DeleteTxn] failed to persist audit event: {}", e);
                        }
                        // The row was already removed from in-memory state and the
                        // UI rebuilt above (optimistic update) — if the DB delete
                        // fails, the row will reappear on next reload from the DB,
                        // so the user needs to know now rather than be silently misled.
                        if let Err(e) = db::delete_transaction(conn, cid, &txn_id) {
                            log::error!("[DeleteTxn] failed to delete txn id={}: {}", txn_id, e);
                            h.set_toast_msg(SharedString::from(
                                format!("Delete failed — row will reappear on reload: {}", e).as_str()));
                            h.set_toast_kind(2);
                        }
                    }
                }
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();

            // Port of old app's differentiated confirm() message (app.js:2498-2500):
            // manually-added rows get a soft prompt, auto-imported rows get a stronger warning.
            app.on_do_request_delete_txn_confirm(move |idx| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let st = state_ref.lock().unwrap();
                let is_manual = visible_to_abs(&st, idx as usize)
                    .map(|abs| matches!(st.transactions[abs].status, parser::TransactionStatus::Manual))
                    .unwrap_or(false);
                drop(st);
                let message = if is_manual {
                    "Delete this manually added transaction?"
                } else {
                    "\u{26A0} You are deleting an auto-imported transaction. This cannot be undone.\n\nAre you sure?"
                };
                h.set_confirm_title(SharedString::from("Delete Transaction"));
                h.set_confirm_message(SharedString::from(message));
                h.set_confirm_action(SharedString::from("delete-txn"));
                h.set_confirm_payload(idx);
                h.set_modal_state(SharedString::from("confirm"));
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
                push_dashboard(&h, &st.transactions, st.opening_balance);
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
                    classification_source: "user".to_string(),
                    tags:        vec![],
                    bank_name:   st.bank_name.clone(),
                    account_no:  st.account_no.clone(),
                    is_opening_balance: false,
                    dup_flag:    false,
                    prev_balance: None,
                    balance_ok:  None,
                    gst_rate:    None,
                    gst_amount:  None,
                    gst_type:    None,
                };
                let new_txn_for_db = new_txn.clone();
                st.transactions.push(new_txn);
                let client_id = st.client_id;
                let event_str = format!("[{}] Manual Add — date='{}' narr='{}'", audit_now(), date, narr);
                st.audit_events.push(event_str.clone());
                log::info!("[AddTxn] date='{}' narr='{}' dr={:?} cr={:?}", date, narr, debit_val, credit_val);
                rebuild_rows(&h, &st);
                push_dashboard(&h, &st.transactions, st.opening_balance);
                drop(st);
                if let Some(cid) = client_id {
                    let db = db_ref.lock().unwrap();
                    if let Some(conn) = db.as_ref() {
                        if let Err(e) = db::upsert_transactions(conn, cid, None, &[new_txn_for_db]) {
                            log::error!("[AddTxn] failed to persist new row: {}", e);
                            h.set_toast_msg(SharedString::from(
                                format!("Row shown but not saved: {}", e).as_str()));
                            h.set_toast_kind(2);
                        }
                        if let Err(e) = db::push_audit_event(conn, cid, &event_str) {
                            log::error!("[AddTxn] failed to persist audit event: {}", e);
                        }
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

                // Peek (don't take yet) — a wrong password re-shows this same
                // modal in place for another attempt, matching old app's
                // unlimited in-place retry (parser.js:1830-1832) instead of
                // requiring the user to re-pick the file from the OS dialog.
                let (pending_path, pending_name, is_batch) = {
                    let st = state_ref.lock().unwrap();
                    (st.pending_pdf_path.clone(), st.pending_pdf_name.clone(), st.batch_progress.is_some())
                };

                let Some(path) = pending_path else {
                    log::warn!("[PdfPwd] confirm fired but no pending path");
                    h.set_pdf_pwd_visible(false);
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
                        if emsg.to_lowercase().contains("incorrect") {
                            // Leave pending_pdf_path/name set — the modal stays open
                            // (Slint side keeps pdf-pwd-visible true) for a retry.
                            h.set_pdf_pwd_input(SharedString::from(""));
                            h.set_pdf_pwd_prompt(SharedString::from(
                                format!("Incorrect password for '{}' \u{2014} please try again:", file_name).as_str(),
                            ));
                            h.set_status_bank(SharedString::from("Incorrect PDF password \u{2014} please try again"));
                            return;
                        }
                        h.set_pdf_pwd_visible(false);
                        {
                            let mut st = state_ref.lock().unwrap();
                            st.pending_pdf_path = None;
                            st.pending_pdf_name = String::new();
                        }
                        let msg = format!("PDF unlock failed: {}", emsg);
                        if is_batch {
                            {
                                let mut st = state_ref.lock().unwrap();
                                if let Some(bp) = st.batch_progress.as_mut() {
                                    record_batch_failure(bp, &file_name, &msg);
                                }
                            }
                            continue_batch(&h, &state_ref, &db_ref);
                        } else {
                            h.set_status_bank(SharedString::from(msg.as_str()));
                        }
                        return;
                    }
                };

                // From here on the password was accepted — close the modal and
                // clear the pending-path state before proceeding.
                h.set_pdf_pwd_visible(false);
                {
                    let mut st = state_ref.lock().unwrap();
                    st.pending_pdf_path = None;
                    st.pending_pdf_name = String::new();
                }

                let parse_result = if stage1.is_some() {
                    stage1
                } else {
                    let full_text = parser::text_extractor::extract_full_text_with_password(&path, &pwd_bytes);
                    if full_text.trim().is_empty() {
                        None
                    } else {
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
                    }
                };

                if is_batch {
                    {
                        let mut st = state_ref.lock().unwrap();
                        if let Some(bp) = st.batch_progress.as_mut() {
                            match parse_result {
                                Some(r) => record_batch_success(bp, &db_ref, &path, &file_name, r),
                                None => record_batch_failure(bp, &file_name, "No transactions found after unlock"),
                            }
                        }
                    }
                    continue_batch(&h, &state_ref, &db_ref);
                } else {
                    match parse_result {
                        Some(r) => apply_parse_result(&h, &state_ref, &db_ref, r, &file_name),
                        None => h.set_status_bank(SharedString::from("No transactions found after unlock")),
                    }
                }
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();
            let db_ref    = db_conn.clone();

            app.on_do_pdf_pwd_cancel(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                h.set_pdf_pwd_visible(false);

                let (pending_name, is_batch) = {
                    let mut st = state_ref.lock().unwrap();
                    let name = std::mem::take(&mut st.pending_pdf_name);
                    st.pending_pdf_path = None;
                    (name, st.batch_progress.is_some())
                };

                if is_batch {
                    // A cancelled password prompt fails just this one file —
                    // matches old app's per-file try/catch isolation in batch
                    // mode (other files still complete).
                    {
                        let mut st = state_ref.lock().unwrap();
                        if let Some(bp) = st.batch_progress.as_mut() {
                            record_batch_failure(bp, &pending_name, "Password required \u{2014} cancelled");
                        }
                    }
                    continue_batch(&h, &state_ref, &db_ref);
                } else {
                    h.set_status_bank(SharedString::from("PDF unlock cancelled"));
                }
            });
        }
        {
            let state_ref = app_state.clone();
            app.on_do_ai_cancel(move || {
                let st = state_ref.lock().unwrap();
                st.ai_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
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

#[cfg(test)]
mod batch_pause_abort_tests {
    use super::*;

    #[test]
    fn normal_batch_processes_the_next_file() {
        assert_eq!(batch_step(false, false), BatchStep::ProcessNext);
    }

    #[test]
    fn paused_batch_waits_without_processing() {
        assert_eq!(batch_step(false, true), BatchStep::WaitPaused);
    }

    #[test]
    fn aborted_batch_finishes() {
        assert_eq!(batch_step(true, false), BatchStep::Finish);
    }

    #[test]
    fn abort_takes_priority_over_a_simultaneous_pause() {
        // A user could pause, then abort while still paused — must not get
        // stuck waiting forever for a resume that will never come.
        assert_eq!(batch_step(true, true), BatchStep::Finish);
    }

    #[test]
    fn clean_completion_message_has_no_abort_wording() {
        let (msg, kind) = batch_summary_message(5, 0, 0, false, 0);
        assert_eq!(msg, "5 file(s) loaded");
        assert_eq!(kind, 1, "a clean run must toast as success");
    }

    #[test]
    fn completion_message_reports_skipped_and_failed_counts() {
        let (msg, kind) = batch_summary_message(3, 2, 1, false, 0);
        assert!(msg.contains("3 file(s) loaded"), "got: {msg}");
        assert!(msg.contains("2 dupe(s) skipped"), "got: {msg}");
        assert!(msg.contains("1 file(s) failed"), "got: {msg}");
        assert_eq!(kind, 3, "any failure must toast as a warning, not success");
    }

    #[test]
    fn aborted_message_reports_unprocessed_count_and_toasts_as_warning() {
        let (msg, kind) = batch_summary_message(4, 0, 0, true, 6);
        assert!(msg.starts_with("Batch aborted:"), "got: {msg}");
        assert!(msg.contains("4 file(s) loaded"), "got: {msg}");
        assert!(msg.contains("6 file(s) not processed"), "got: {msg}");
        assert_eq!(kind, 3, "an aborted run must never toast as plain success");
    }

    #[test]
    fn aborted_with_nothing_left_unprocessed_omits_the_unprocessed_clause() {
        // Abort clicked right as the very last file finished — nothing was
        // actually left behind, so the message shouldn't claim otherwise.
        let (msg, _kind) = batch_summary_message(4, 0, 0, true, 0);
        assert!(!msg.contains("not processed"), "got: {msg}");
    }
}
