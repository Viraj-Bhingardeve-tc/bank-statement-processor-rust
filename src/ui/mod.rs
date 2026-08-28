// ui/mod.rs — UI application state and Slint bridge helpers.
// AppState holds the runtime data that drives the Slint UI.
// Phase 2: basic structure only.  Fields populated in Phase 3+.

/// Per-file result for the batch monitor table.
#[derive(Debug, Clone, Default)]
pub struct BatchFileResult {
    pub file: String,
    pub bank: String,
    pub account: String,
    pub period: String,
    pub txns: usize,
    pub ok: bool,
    pub err_msg: String,
}

/// Requirement #11: the Batch Monitor modal's session summary line — e.g.
/// "Session: 2 import(s)  |  184 transactions  |  120 classified  |  64
/// unreviewed" — or blank when nothing has been loaded yet. Extracted out of
/// `main.rs`'s `refresh_batch_monitor_display` so this piece of "does the
/// monitor actually show something meaningful" logic is unit-testable
/// without the Slint bindings that function otherwise depends on.
pub fn batch_session_summary(
    import_count: usize,
    txn_count: usize,
    classified: usize,
    unreviewed: usize,
) -> String {
    if txn_count == 0 {
        String::new()
    } else {
        format!(
            "Session: {} import(s)  |  {} transactions  |  {} classified  |  {} unreviewed",
            import_count, txn_count, classified, unreviewed
        )
    }
}

/// State for a batch-folder import that's paused waiting for a PDF password
/// — or for a user-requested pause/abort (see `paused`/`aborted` below).
/// Old app's batch loop can `await` a per-file password prompt inline
/// (parser.js, single-threaded async); Rust's batch loop runs one file per
/// `continue_batch` call, rescheduling itself via a zero-delay Slint timer
/// instead of looping synchronously — this both lets a password-protected
/// file save its in-progress accumulators here and show the password modal
/// (reusing `pending_pdf_path`/`pending_pdf_name`, resuming from `remaining`
/// once `on_do_pdf_pwd_confirm`/`on_do_pdf_pwd_cancel` fires), and gives
/// Pause/Abort a real point between files where they can actually take
/// effect — see `continue_batch`'s doc comment in main.rs.
#[derive(Debug, Clone, Default)]
pub struct BatchProgress {
    pub remaining: std::collections::VecDeque<std::path::PathBuf>,
    pub all_txns: Vec<crate::parser::Transaction>,
    pub loaded: usize,
    pub skipped: usize,
    pub errors: usize,
    pub first_bank: String,
    pub first_ob: Option<f64>,
    pub new_import_ids: Vec<i64>,
    pub batch_results: Vec<BatchFileResult>,
    pub persisted_hashes: std::collections::HashSet<String>,
    pub client_id: Option<i64>,
    /// User clicked "Pause" — `continue_batch` checks this before starting
    /// the next file and, if set, stops rescheduling itself entirely
    /// (no polling) until "Resume" is clicked, which re-invokes it directly.
    pub paused: bool,
    /// User clicked "Abort" — takes effect at the next file boundary, same
    /// granularity as the old app (`_aborted` checked once per loop
    /// iteration, never mid-file). Kept distinct from just dropping
    /// `batch_progress` so `finish_batch` can report an accurate "N of M
    /// processed, aborted" summary instead of a generic completion message.
    pub aborted: bool,
}

/// Snapshot of editable transaction fields for the undo stack.
#[derive(Debug, Clone)]
pub struct UndoEntry {
    pub txn_id: String,
    pub vendor: String,
    pub head: String,
    pub txn_type: crate::parser::VoucherType,
    pub status: crate::parser::TransactionStatus,
    pub confidence: f64,
}

/// Runtime state shared between Rust logic and the Slint UI.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub client_id: Option<i64>,
    pub client_name: String,
    pub tally_ledger: String, // Tally bank ledger name for the current client
    pub file_name: String,
    pub bank_name: String,
    pub account_no: String,

    pub opening_balance: Option<f64>,
    pub closing_balance: Option<f64>,
    pub total_debits: f64,
    pub total_credits: f64,
    pub txn_count: usize,
    pub unreviewed: usize,
    pub vendor_count: usize,
    pub has_mismatch: bool,

    // Full transaction list for dashboard re-aggregation on filter change
    pub transactions: Vec<crate::parser::Transaction>,

    // Active filter state — kept in sync with Slint UI
    pub active_filter: String, // "all" | "unreviewed" | "suspense" | "high" | "duplicates" | "gst" | "needs_review" | "multi"
    pub filter_statuses: Vec<String>, // OR-logic multi-status set; empty = "all"
    pub date_from: String,     // DD/MM/YYYY
    pub date_to: String,       // DD/MM/YYYY
    pub bank_filter: String,   // "" means All Banks
    pub vendor_filter: String, // "" means no vendor filter
    pub head_filter: String,   // "" means no ledger/head filter
    pub dedup_enabled: bool,   // mirrors the Dedupe checkbox in the toolbar

    // Export wizard state (synced from the UI before generating)
    pub wiz_sw_idx: i32, // 0=Tally 1=Zoho 2=QB 3=Odoo 4=Excel 5=XML
    pub wiz_company: String,
    pub wiz_gstin: String,
    pub wiz_date_from: String, // ISO YYYY-MM-DD
    pub wiz_date_to: String,

    // Import history: parallel vec to the UI import-records list (DB import ids)
    pub import_ids: Vec<i64>,
    // Batch monitor: per-file results from last batch folder processing
    pub batch_file_results: Vec<BatchFileResult>,
    // Rules: parallel vec to the UI rule-records list (DB rule ids)
    pub rule_ids: Vec<i64>,

    // AI settings (loaded from DB settings table)
    pub ai_provider: String, // "openai" | "claude" | "gemini"
    pub ai_api_key: String,
    pub ai_enabled: bool,

    // Audit trail — in-memory event log, newest appended last
    pub audit_events: Vec<String>,
    // Undo stack — most recent edit at the end (pop from end = undo last)
    pub undo_stack: Vec<UndoEntry>,

    // Reconcile — vouchers parsed from the last "Import Tally Export" click,
    // kept around so "Run Reconciliation" can (re-)match against them without
    // re-picking the file (e.g. after tweaking the recon tolerance settings).
    pub recon_vouchers: Vec<crate::reconciliation::Voucher>,
    pub recon_file_label: String,
    // CSV text built after the last successful reconciliation run; empty until then.
    pub recon_csv: String,

    // Legacy data migration — export file picked by "Select Export File…",
    // kept around so "Start Migration" doesn't need to re-pick it, and the
    // full report text from the last run (for "Save Full Report…").
    pub migration_export_path: Option<std::path::PathBuf>,
    pub migration_report_md: String,

    // PDF password — path waiting for a password prompt
    pub pending_pdf_path: Option<std::path::PathBuf>,
    pub pending_pdf_name: String,
    // Set while a batch-folder import is paused waiting for a PDF password —
    // see BatchProgress doc comment.
    pub batch_progress: Option<BatchProgress>,
    // Batch pre-flight review (2026-08-25): the supported, deduplicated file
    // list staged by "Choose Folder"/"Choose Multiple Files" and shown in
    // the "batch-review" modal — `do_batch_process_reviewed` reads this
    // (not the raw OS dialog result) to actually start the batch, so
    // whatever the user saw listed is exactly what runs.
    pub batch_review: Vec<std::path::PathBuf>,

    // Set true to abort an in-flight AI classification run (checked between
    // batches in ai_classifier::classify_with_ai). Reset to false at the start
    // of each new run. Shared via Arc so the Cancel button's handler (which
    // doesn't hold the classification thread) can reach the same flag.
    pub ai_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl AppState {
    /// Format an optional f64 as Indian locale amount string (e.g. "₹ 1,23,456.78").
    pub fn fmt_amount(v: Option<f64>) -> String {
        match v {
            None => "₹ —".to_string(),
            Some(n) => format!("₹ {}", fmt_inr(n)),
        }
    }
}

/// Mask an account number for display: `XXXX` + the last 4 alphanumeric
/// characters (e.g. "1234567890123456" -> "XXXX3456"), or bare `XXXX` when
/// no account number could be extracted at all. Never returns an empty
/// string — every UI surface that shows an account number should route
/// through this instead of displaying the raw (or missing) value, so the
/// full number stays available internally (state/export/DB) while only the
/// masked form reaches the screen.
pub fn mask_account_no(acct: &str) -> String {
    let digits: String = acct.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if digits.is_empty() {
        "XXXX".to_string()
    } else {
        let last4: String = {
            let mut rev: Vec<char> = digits.chars().rev().take(4).collect();
            rev.reverse();
            rev.into_iter().collect()
        };
        format!("XXXX{}", last4)
    }
}

/// Format a float in Indian numbering system with 2 decimal places.
pub fn fmt_inr(amount: f64) -> String {
    let abs = amount.abs();
    let sign = if amount < 0.0 { "-" } else { "" };
    let cents = (abs * 100.0).round() as u64;
    let paise = cents % 100;
    let rupees = cents / 100;

    if rupees == 0 {
        return format!("{}{}.{:02}", sign, 0, paise);
    }

    // Indian grouping: last 3 digits, then groups of 2
    let s = rupees.to_string();
    let len = s.len();
    let mut out = String::new();

    if len <= 3 {
        out.push_str(&s);
    } else {
        let (first, rest) = s.split_at(len - 3);
        // first part: group in 2s from right
        let first_chars: Vec<char> = first.chars().collect();
        let r = first_chars.len() % 2;
        if r != 0 {
            out.push_str(&first_chars[..r].iter().collect::<String>());
        }
        for chunk in first_chars[r..].chunks(2) {
            if !out.is_empty() {
                out.push(',');
            }
            out.push_str(&chunk.iter().collect::<String>());
        }
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(rest);
    }

    format!("{}{}.{:02}", sign, out, paise)
}

// ── Tests ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_inr_basic() {
        assert_eq!(fmt_inr(1000.0), "1,000.00");
        assert_eq!(fmt_inr(100000.0), "1,00,000.00");
        assert_eq!(fmt_inr(1234567.89), "12,34,567.89");
        assert_eq!(fmt_inr(0.5), "0.50");
    }

    #[test]
    fn fmt_inr_negative() {
        assert_eq!(fmt_inr(-1000.0), "-1,000.00");
    }

    // ── batch_session_summary (Requirement #11) ───────────────────────────────

    #[test]
    fn batch_session_summary_is_blank_when_nothing_has_been_loaded() {
        assert_eq!(batch_session_summary(0, 0, 0, 0), "");
    }

    #[test]
    fn batch_session_summary_reports_all_four_counts() {
        let s = batch_session_summary(2, 184, 120, 64);
        assert_eq!(
            s,
            "Session: 2 import(s)  |  184 transactions  |  120 classified  |  64 unreviewed"
        );
    }

    #[test]
    fn batch_session_summary_is_non_blank_whenever_transactions_are_loaded() {
        // Even with 0 imports/classified — e.g. after Batch Monitor's own
        // display is refreshed following a "Load All into Session" rather
        // than a batch folder run — the summary must still say something
        // rather than silently looking like nothing happened.
        assert_ne!(batch_session_summary(0, 5, 0, 5), "");
    }

    // ── mask_account_no ───────────────────────────────────────────────────────

    #[test]
    fn mask_account_no_shows_last_4_digits() {
        assert_eq!(mask_account_no("1234567890123456"), "XXXX3456");
        assert_eq!(mask_account_no("9876543210"), "XXXX3210");
    }

    #[test]
    fn mask_account_no_never_blank_when_unavailable() {
        assert_eq!(mask_account_no(""), "XXXX");
        assert_eq!(mask_account_no("   "), "XXXX");
    }

    #[test]
    fn mask_account_no_short_number_uses_whatever_digits_exist() {
        // Fewer than 4 characters: show them all rather than padding or failing.
        assert_eq!(mask_account_no("12"), "XXXX12");
    }

    #[test]
    fn mask_account_no_ignores_stray_whitespace_and_punctuation() {
        assert_eq!(mask_account_no("1234 5678 9012"), "XXXX9012");
        assert_eq!(mask_account_no(" 3210 "), "XXXX3210");
    }
}
