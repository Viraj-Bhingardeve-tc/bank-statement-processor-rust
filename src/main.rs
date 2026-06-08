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
mod parser;
mod ui;

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
                                log::error!("PDF extract error: {}", e);
                                None
                            }
                        };

                        if stage1.is_some() {
                            stage1
                        } else {
                            // Stage 2a: OCR text parsing
                            let full_text = parser::text_extractor::extract_full_text(&path);
                            if full_text.trim().is_empty() {
                                h.set_status_bank(SharedString::from(
                                    "Scanned PDF — OCR not yet supported",
                                ));
                                return;
                            }

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

                // ── Build summary statistics ───────────────────────────────────
                let real: Vec<&parser::Transaction> = result
                    .transactions
                    .iter()
                    .filter(|t| !t.is_opening_balance)
                    .collect();

                let total_dr: f64 = real.iter().filter_map(|t| t.debit).sum();
                let total_cr: f64 = real.iter().filter_map(|t| t.credit).sum();

                // Period (first–last date)
                let dates: Vec<&str> = real.iter()
                    .filter(|t| !t.date.is_empty())
                    .map(|t| t.date.as_str())
                    .collect();
                let period = if dates.len() >= 2 {
                    format!("{} – {}", dates[0], dates[dates.len() - 1])
                } else if dates.len() == 1 {
                    dates[0].to_string()
                } else {
                    "—".to_string()
                };

                // Counts
                let unreviewed_cnt = real.iter()
                    .filter(|t| matches!(t.status, parser::TransactionStatus::Unreviewed))
                    .count();
                let credit_cnt = real.iter().filter(|t| t.credit.is_some()).count();
                let debit_cnt  = real.iter().filter(|t| t.debit.is_some()).count();

                // Calculated closing balance
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
                        (Some(stated), Some(calc)) => {
                            format!("Diff: {}", ui::fmt_inr((stated - calc).abs()))
                        }
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

                // ── Build TxnRow table rows ───────────────────────────────────
                let mut bank_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
                let row_models: Vec<TxnRow> = real
                    .iter()
                    .map(|t| {
                        bank_set.insert(t.bank_name.clone());
                        let narr: String = t.narration.chars().take(80).collect();
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
                            has_dup_tag: has_dup,
                        }
                    })
                    .collect();

                let mut bank_names: Vec<SharedString> = std::iter::once(SharedString::from("All Banks"))
                    .chain(bank_set.iter().map(|b| SharedString::from(b.as_str())))
                    .collect();
                let _ = bank_names.len(); // suppress unused

                let table_model = slint::ModelRc::new(slint::VecModel::from(row_models));
                let banks_model = slint::ModelRc::new(slint::VecModel::from(bank_names));

                // ── Push to Slint UI ───────────────────────────────────────────
                h.set_transaction_rows(table_model);
                h.set_bank_names(banks_model);
                h.set_status_file(SharedString::from(file_name.as_str()));
                h.set_status_bank(SharedString::from(result.bank_name.as_str()));

                // Basic summary
                h.set_dash_bank_name(SharedString::from(result.bank_name.as_str()));
                h.set_dash_opening(SharedString::from(
                    ui::AppState::fmt_amount(result.opening_balance).as_str(),
                ));
                h.set_dash_closing(SharedString::from(
                    ui::AppState::fmt_amount(result.closing_balance).as_str(),
                ));
                h.set_dash_credits(SharedString::from(
                    ui::AppState::fmt_amount(Some(total_cr)).as_str(),
                ));
                h.set_dash_debits(SharedString::from(
                    ui::AppState::fmt_amount(Some(total_dr)).as_str(),
                ));
                h.set_dash_txn_count(SharedString::from(real.len().to_string().as_str()));
                h.set_dash_vendors(SharedString::from("—"));

                // Enhanced summary
                h.set_dash_account_no(SharedString::from(result.account_no.as_str()));
                h.set_dash_period(SharedString::from(period.as_str()));
                h.set_dash_credit_count(SharedString::from(credit_cnt.to_string().as_str()));
                h.set_dash_debit_count(SharedString::from(debit_cnt.to_string().as_str()));
                h.set_dash_unreviewed(SharedString::from(unreviewed_cnt.to_string().as_str()));
                h.set_dash_suspense(SharedString::from("0"));
                h.set_dash_needs_review(SharedString::from("0"));
                h.set_dash_duplicates(SharedString::from("0"));
                h.set_dash_gst_count(SharedString::from("0"));
                h.set_dash_calc_closing(SharedString::from(
                    ui::AppState::fmt_amount(calc_closing).as_str(),
                ));
                h.set_dash_has_mismatch(has_mismatch);
                h.set_dash_mismatch(SharedString::from(mismatch_str.as_str()));

                // Update in-memory app state and reset filters on new file load
                {
                    let mut st = state_ref.lock().unwrap();
                    st.bank_name       = result.bank_name.clone();
                    st.account_no      = result.account_no.clone();
                    st.file_name       = file_name.clone();
                    st.opening_balance = result.opening_balance;
                    st.closing_balance = result.closing_balance;
                    st.total_debits    = total_dr;
                    st.total_credits   = total_cr;
                    st.txn_count       = real.len();
                    st.unreviewed      = unreviewed_cnt;
                    st.transactions    = result.transactions.clone();
                    // Reset filters whenever a new file is loaded
                    st.active_filter   = "all".to_string();
                    st.date_from       = String::new();
                    st.date_to         = String::new();
                    st.bank_filter     = String::new();
                }

                // Build filter badge counts from the full transaction list
                let [all_cnt, unreview_cnt2, susp_cnt, high_cnt, dup_cnt, gst_cnt, rev_cnt] =
                    compute_filter_counts(&result.transactions);
                h.set_fc_all(SharedString::from(all_cnt.to_string().as_str()));
                h.set_fc_unreviewed(SharedString::from(unreview_cnt2.to_string().as_str()));
                h.set_fc_suspense(SharedString::from(susp_cnt.to_string().as_str()));
                h.set_fc_high(SharedString::from(high_cnt.to_string().as_str()));
                h.set_fc_duplicates(SharedString::from(dup_cnt.to_string().as_str()));
                h.set_fc_gst(SharedString::from(gst_cnt.to_string().as_str()));
                h.set_fc_review(SharedString::from(rev_cnt.to_string().as_str()));

                // ── Dashboard analytics ────────────────────────────────────────
                let txns_all: Vec<parser::Transaction> = result.transactions.clone();
                push_dashboard(&h, &txns_all, result.opening_balance);

                log::info!("UI updated with {} transactions", real.len());
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
                            let r_bank     = r.bank_name.clone();
                            let r_account  = r.account_no.clone();
                            let r_ob       = r.opening_balance;
                            let r_txn_cnt  = r.transactions.len();
                            let before = all_txns.len();
                            let existing_hashes: std::collections::HashSet<String> =
                                all_txns.iter().map(|t| t.hash()).collect();
                            let new_txns: Vec<parser::Transaction> = r.transactions.into_iter()
                                .filter(|t| t.is_opening_balance || !existing_hashes.contains(&t.hash()))
                                .collect();
                            skipped += before.saturating_sub(all_txns.len());
                            all_txns.extend(new_txns);
                            loaded += 1;
                            if first_bank.is_empty() { first_bank = r_bank.clone(); }
                            if first_ob.is_none() { first_ob = r_ob; }
                            let db = db_ref.lock().unwrap();
                            if let Some(conn) = db.as_ref() {
                                let client_id = { state_ref.lock().unwrap().client_id.unwrap_or(0) };
                                if client_id > 0 {
                                    let _ = db::save_import(conn, client_id, &file_name, &r_bank, &r_account, r_txn_cnt);
                                }
                            }
                        }
                        _ => { errors += 1; log::warn!("[Batch] failed to parse: {:?}", path); }
                    }
                }

                if all_txns.is_empty() { return; }

                // Classify and build model
                let bank_ledger = { state_ref.lock().unwrap().tally_ledger.clone() };
                classifier::classify_all(&mut all_txns, &bank_ledger);
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
                }

                push_dashboard(&h, &all_txns, first_ob);
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
                // Update AppState
                if let Some(id) = new_id {
                    let mut st = state_ref.lock().unwrap();
                    st.client_id     = Some(id);
                    st.client_name   = name.trim().to_string();
                    st.tally_ledger  = ledger.trim().to_string();
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
            app.on_do_auto_classify(move || {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();
                if st.transactions.is_empty() {
                    log::info!("[AutoClassify] No transactions loaded");
                    return;
                }
                let bank_ledger = st.tally_ledger.clone();
                let changed = classifier::classify_all(&mut st.transactions, &bank_ledger);
                log::info!("[AutoClassify] classified {} transactions", changed);
                rebuild_rows(&h, &st);
                push_dashboard(&h, &st.transactions, st.opening_balance);
                // Update summary stats
                let total_dr: f64 = st.transactions.iter().filter_map(|t| t.debit).sum();
                let total_cr: f64 = st.transactions.iter().filter_map(|t| t.credit).sum();
                h.set_dash_credits(SharedString::from(ui::AppState::fmt_amount(Some(total_cr)).as_str()));
                h.set_dash_debits(SharedString::from(ui::AppState::fmt_amount(Some(total_dr)).as_str()));
            });
        }
        {
            app.on_do_ai_classify(|| stub_callback("ai-classify"));
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
        // ── Export Excel (CSV with 4 sections) ───────────────────────────────
        {
            let state_ref = app_state.clone();
            app.on_do_export_excel(move || {
                let st = state_ref.lock().unwrap();
                if st.transactions.is_empty() {
                    log::warn!("[ExportExcel] No transactions to export");
                    return;
                }
                let client_name = if st.client_name.is_empty() { "Export".to_string() } else { st.client_name.clone() };
                let suggested = format!("BankStatement_{}.csv", client_name.replace(' ', "_"));
                let path = match rfd::FileDialog::new()
                    .set_title("Save Excel Export")
                    .set_file_name(&suggested)
                    .add_filter("CSV (Excel)", &["csv"])
                    .save_file()
                {
                    Some(p) => p,
                    None    => return,
                };
                match export::excel::export_csv(
                    &st.transactions, &client_name, &st.tally_ledger,
                    &st.file_name, st.opening_balance, st.closing_balance, &path,
                ) {
                    Ok(n) => log::info!("[ExportExcel] wrote {} rows → {:?}", n, path),
                    Err(e) => log::error!("[ExportExcel] failed: {}", e),
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
        {
            app.on_do_reset_dedupe(|| stub_callback("reset-dedupe"));
        }
        {
            app.on_do_reconcile(|| stub_callback("reconcile"));
        }
        {
            app.on_do_batch_monitor(|| stub_callback("batch-monitor"));
        }
        {
            app.on_do_audit_trail(|| stub_callback("audit-trail"));
        }
        {
            app.on_do_settings(|| stub_callback("settings"));
        }
        {
            app.on_do_add_row(|| stub_callback("add-row"));
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
                            let client_id = st.client_id.unwrap_or(0);
                            let db = db_ref.lock().unwrap();
                            if let Some(conn) = db.as_ref() {
                                match db::add_rule(conn, client_id, &pattern, &vendor, &head, &typ) {
                                    Ok(_)  => log::info!("[SaveLearn] rule saved: pattern='{}' head='{}' vendor='{}'", pattern, head, vendor),
                                    Err(e) => log::error!("[SaveLearn] DB error: {}", e),
                                }
                            }
                        }
                    }
                    log::info!("[SaveTxn] abs={} vendor='{}' head='{}' type='{}' learn={}", abs, vendor, head, typ, learn);
                }
                rebuild_rows(&h, &st);
            });
        }
        {
            let handle    = app.as_weak();
            let state_ref = app_state.clone();

            app.on_do_delete_txn(move |idx| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();

                if let Some(abs) = visible_to_abs(&st, idx as usize) {
                    st.transactions.remove(abs);
                    log::info!("[DeleteTxn] abs={} removed", abs);
                }
                rebuild_rows(&h, &st);
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

            app.on_do_add_txn(move |date, refno, narr, dr, cr, vendor, head, typ| {
                let h = match handle.upgrade() { Some(h) => h, None => return };
                let mut st = state_ref.lock().unwrap();

                let debit_val  = dr.parse::<f64>().ok().filter(|v| *v > 0.0);
                let credit_val = cr.parse::<f64>().ok().filter(|v| *v > 0.0);

                let new_txn = parser::Transaction {
                    id:          format!("manual-{}", st.transactions.len()),
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
                st.transactions.push(new_txn);
                log::info!("[AddTxn] date='{}' narr='{}' dr={:?} cr={:?}", date, narr, debit_val, credit_val);
                rebuild_rows(&h, &st);
            });
        }

        {
            app.on_do_pdf_pwd_confirm(|pwd| {
                log::info!("[PdfPwd] password entered (len={})", pwd.len());
            });
        }
        {
            app.on_do_pdf_pwd_cancel(|| {
                log::info!("[PdfPwd] cancelled");
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
