// main.rs — Entry point for the Bank Statement Processor (Rust + Slint).
//
// Boot sequence:
//   1. Initialise logger
//   2. Open (or create) the SQLite database
//   3. Create the Slint AppWindow
//   4. Wire callbacks: do-login, do-load-file, do-batch-folder
//   5. Run the Slint event loop

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth;
mod db;
mod parser;
mod ui;

use std::sync::{Arc, Mutex};

#[cfg(feature = "slint-ui")]
use slint::SharedString;

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

// ── Helpers (slint-ui only) ───────────────────────────────────────────────────

/// Format an optional f64 as an Indian-locale amount string (no ₹ prefix).
/// Returns empty string for None (table cell stays blank).
#[cfg(feature = "slint-ui")]
fn fmt_cell(v: Option<f64>) -> String {
    match v {
        None    => String::new(),
        Some(n) => ui::fmt_inr(n),
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .init();

    log::info!("Bank Statement Processor starting…");

    // Database (non-fatal if it fails — parser still works)
    let db_path = {
        let mut p = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("."));
        p.pop();
        p.push("bsp_data.db");
        p
    };
    match db::open(&db_path) {
        Ok(_)    => log::info!("Database ready at {:?}", db_path),
        Err(err) => log::warn!("Database init failed (non-fatal): {}", err),
    }

    // ── Slint UI ──────────────────────────────────────────────────────────────
    #[cfg(feature = "slint-ui")]
    {
        let app = AppWindow::new()?;

        // ── AppState shared between callbacks ─────────────────────────────────
        let app_state: Arc<Mutex<ui::AppState>> =
            Arc::new(Mutex::new(ui::AppState::default()));

        // ── Login callback ────────────────────────────────────────────────────
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

        // ── Load File callback ────────────────────────────────────────────────
        {
            let handle     = app.as_weak();
            let state_ref  = app_state.clone();

            app.on_do_load_file(move || {
                // 1. Native file picker
                let path = match rfd::FileDialog::new()
                    .set_title("Open Bank Statement")
                    .add_filter("Bank Statements", &["pdf", "xlsx", "xls", "xlsm"])
                    .add_filter("PDF", &["pdf"])
                    .add_filter("Excel", &["xlsx", "xls", "xlsm"])
                    .pick_file()
                {
                    Some(p) => p,
                    None    => return, // user cancelled
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

                // Show the filename immediately so the user knows something is happening
                h.set_status_file(SharedString::from(file_name.as_str()));
                h.set_status_bank(SharedString::from("Parsing…"));

                // 2. Parse the file
                let parse_result: Option<parser::ParseResult> =
                    if ["xlsx", "xls", "xlsm"].contains(&ext.as_str()) {
                        // ── Excel ──────────────────────────────────────────────
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
                        // ── PDF — two-stage parsing ────────────────────────────
                        //
                        // Stage 1: extract text lines as fixed-width rows (all X=0)
                        //   → try FW format detection → extract_fw_transactions
                        //
                        // Stage 2 (fallback): feed raw text to parse_ocr_text
                        //   → date-anchored line-by-line parsing; works when FW fails.
                        //
                        // NOTE: lopdf gives no X/Y positions.  Column-based PDFs
                        // (HDFC, SBI, ICICI) need pdfium-render for accurate column
                        // detection — Phase 5.

                        // ── Stage 1: structured row parsing ───────────────────
                        let stage1 = match parser::text_extractor::extract_pages(&path) {
                            Ok(rows) => {
                                println!("[PDF] Stage-1 extract_pages → {} rows", rows.len());
                                if rows.is_empty() {
                                    println!("[PDF] Stage-1: 0 rows — lopdf extracted no text");
                                    None
                                } else {
                                    // Print first 5 rows for inspection
                                    for (i, row) in rows.iter().take(5).enumerate() {
                                        let txt: Vec<&str> = row.iter().map(|it| it.text.as_str()).collect();
                                        println!("[PDF] row[{}]: {:?}", i, txt.join(" | "));
                                    }
                                    let r = parser::pdf_parser::parse_pdf_rows(rows, &file_name);
                                    println!("[PDF] Stage-1 parse_pdf_rows → {:?}",
                                        r.as_ref().map(|pr| pr.transactions.len()));
                                    r
                                }
                            }
                            Err(e) => {
                                println!("[PDF] Stage-1 extract_pages error: {}", e);
                                log::error!("PDF extract error: {}", e);
                                None
                            }
                        };

                        if stage1.is_some() {
                            stage1
                        } else {
                            // ── Stage 2: OCR-text fallback ─────────────────────
                            println!("[PDF] Stage-1 returned None → trying OCR text fallback");
                            let full_text = parser::text_extractor::extract_full_text(&path);
                            println!("[PDF] Stage-2 full_text: {} chars, first 300: {:?}",
                                full_text.len(),
                                &full_text.chars().take(300).collect::<String>());

                            if full_text.trim().is_empty() {
                                println!("[PDF] Stage-2: empty text — likely scanned PDF");
                                h.set_status_bank(SharedString::from(
                                    "Scanned PDF — OCR not yet supported in Phase 4",
                                ));
                                return;
                            }

                            // Stage 2a: plain OCR parsing (works when date + amounts on same line)
                            let ocr = parser::ocr_parser::parse_ocr_text(&full_text, &file_name);
                            let real_count = ocr.transactions.iter()
                                .filter(|t| !t.is_opening_balance)
                                .count();
                            println!("[PDF] Stage-2a parse_ocr_text → {} real transactions", real_count);

                            if real_count > 0 {
                                Some(ocr)
                            } else {
                                // Stage 2b: multi-line preprocessor (BOM, SBI, Mahanagar style)
                                // Each transaction spans multiple lines; preprocess into
                                // "DATE narration AMT BAL" single-line format, then re-parse.
                                let preprocessed =
                                    parser::ocr_parser::preprocess_multiline(&full_text);
                                println!("[PDF] Stage-2b preprocessed: {} lines",
                                    preprocessed.lines().count());
                                if !preprocessed.trim().is_empty() {
                                    // Print first 5 preprocessed lines for inspection
                                    for (i, l) in preprocessed.lines().take(5).enumerate() {
                                        println!("[PDF]   pre[{}]: {}", i, l);
                                    }
                                    let ml = parser::ocr_parser::parse_ocr_text(
                                        &preprocessed, &file_name,
                                    );
                                    let ml_count = ml.transactions.iter()
                                        .filter(|t| !t.is_opening_balance)
                                        .count();
                                    println!("[PDF] Stage-2b multiline → {} real transactions", ml_count);
                                    if ml_count > 0 {
                                        Some(ml)
                                    } else {
                                        println!("[PDF] All stages failed — PDF may use embedded/unreadable fonts");
                                        h.set_status_bank(SharedString::from(
                                            "PDF font not readable by lopdf — needs pdfium (Phase 5)",
                                        ));
                                        return;
                                    }
                                } else {
                                    println!("[PDF] Stage-2b: no preprocessed lines");
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

                // 3. Compute summary statistics
                let real: Vec<&parser::Transaction> = result
                    .transactions
                    .iter()
                    .filter(|t| !t.is_opening_balance)
                    .collect();

                let total_dr: f64 = real.iter().filter_map(|t| t.debit).sum();
                let total_cr: f64 = real.iter().filter_map(|t| t.credit).sum();

                log::info!(
                    "Summary: bank='{}' txns={} dr={:.2} cr={:.2} ob={:?} cb={:?}",
                    result.bank_name, real.len(), total_dr, total_cr,
                    result.opening_balance, result.closing_balance
                );

                // Diagnostic: confirm we reached model-building
                println!("[UI] Building table model: bank='{}' txns={} ob={:?} cb={:?}",
                    result.bank_name, real.len(),
                    result.opening_balance, result.closing_balance);

                // 4. Build [[StandardListViewItem]] table model
                //    Outer model: each element is one row (ModelRc<StandardListViewItem>)
                //    Inner model: each element is one cell (StandardListViewItem { text })
                let row_models: Vec<slint::ModelRc<slint::StandardListViewItem>> = real
                    .iter()
                    .map(|t| {
                        let narration_display: String =
                            t.narration.chars().take(70).collect();

                        // StandardListViewItem is #[non_exhaustive] — use From<&str>
                        let cells: Vec<slint::StandardListViewItem> = vec![
                            slint::StandardListViewItem::from(t.date.as_str()),
                            slint::StandardListViewItem::from(narration_display.as_str()),
                            slint::StandardListViewItem::from(fmt_cell(t.debit).as_str()),
                            slint::StandardListViewItem::from(fmt_cell(t.credit).as_str()),
                            slint::StandardListViewItem::from(fmt_cell(t.balance).as_str()),
                            slint::StandardListViewItem::from(t.reference.as_str()),
                        ];
                        slint::ModelRc::new(slint::VecModel::from(cells))
                    })
                    .collect();

                let table_model =
                    slint::ModelRc::new(slint::VecModel::from(row_models));

                println!("[UI] Pushing {} row models to Slint set_transaction_rows", real.len());

                // 5. Push everything to the Slint UI
                h.set_transaction_rows(table_model);

                h.set_status_file(SharedString::from(file_name.as_str()));
                h.set_status_bank(SharedString::from(result.bank_name.as_str()));
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
                h.set_dash_txn_count(SharedString::from(
                    real.len().to_string().as_str(),
                ));
                h.set_dash_vendors(SharedString::from("—")); // Phase 7

                // Update in-memory app state
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
                }

                log::info!("UI updated with {} transactions", real.len());
            });
        }

        // ── Batch Folder callback (Phase 9) ───────────────────────────────────
        {
            let handle = app.as_weak();
            app.on_do_batch_folder(move || {
                if let Some(h) = handle.upgrade() {
                    log::info!("Batch Folder — Phase 9");
                    let _ = h;
                }
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
