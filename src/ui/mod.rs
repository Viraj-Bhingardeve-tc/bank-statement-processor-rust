// ui/mod.rs — UI application state and Slint bridge helpers.
// AppState holds the runtime data that drives the Slint UI.
// Phase 2: basic structure only.  Fields populated in Phase 3+.

/// Per-file result for the batch monitor table.
#[derive(Debug, Clone, Default)]
pub struct BatchFileResult {
    pub file:    String,
    pub bank:    String,
    pub account: String,
    pub period:  String,
    pub txns:    usize,
    pub ok:      bool,
    pub err_msg: String,
}

/// Runtime state shared between Rust logic and the Slint UI.
#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub client_id:       Option<i64>,
    pub client_name:     String,
    pub tally_ledger:    String,   // Tally bank ledger name for the current client
    pub file_name:       String,
    pub bank_name:       String,
    pub account_no:      String,

    pub opening_balance: Option<f64>,
    pub closing_balance: Option<f64>,
    pub total_debits:    f64,
    pub total_credits:   f64,
    pub txn_count:       usize,
    pub unreviewed:      usize,
    pub vendor_count:    usize,
    pub has_mismatch:    bool,

    // Full transaction list for dashboard re-aggregation on filter change
    pub transactions:    Vec<crate::parser::Transaction>,

    // Active filter state — kept in sync with Slint UI
    pub active_filter:  String,   // "all" | "unreviewed" | "suspense" | "high" | "duplicates" | "gst" | "needs_review"
    pub date_from:      String,   // DD/MM/YYYY
    pub date_to:        String,   // DD/MM/YYYY
    pub bank_filter:    String,   // "" means All Banks
    pub dedup_enabled:  bool,     // mirrors the Dedupe checkbox in the toolbar

    // Export wizard state (synced from the UI before generating)
    pub wiz_sw_idx:    i32,   // 0=Tally 1=Zoho 2=QB 3=Odoo 4=Excel 5=XML
    pub wiz_company:   String,
    pub wiz_gstin:     String,
    pub wiz_date_from: String,  // ISO YYYY-MM-DD
    pub wiz_date_to:   String,

    // Import history: parallel vec to the UI import-records list (DB import ids)
    pub import_ids: Vec<i64>,
    // Batch monitor: per-file results from last batch folder processing
    pub batch_file_results: Vec<BatchFileResult>,
    // Rules: parallel vec to the UI rule-records list (DB rule ids)
    pub rule_ids: Vec<i64>,

    // AI settings (loaded from DB settings table)
    pub ai_provider:  String,   // "openai" | "claude" | "gemini"
    pub ai_api_key:   String,
    pub ai_enabled:   bool,

    // Audit trail — in-memory event log, newest appended last
    pub audit_events: Vec<String>,

    // Reconcile — CSV text built after the last run; empty until first reconcile
    pub recon_csv: String,

    // PDF password — path waiting for a password prompt
    pub pending_pdf_path: Option<std::path::PathBuf>,
    pub pending_pdf_name: String,
}

impl AppState {
    /// Format an optional f64 as Indian locale amount string (e.g. "₹ 1,23,456.78").
    pub fn fmt_amount(v: Option<f64>) -> String {
        match v {
            None    => "₹ —".to_string(),
            Some(n) => format!("₹ {}", fmt_inr(n)),
        }
    }
}

/// Format a float in Indian numbering system with 2 decimal places.
pub fn fmt_inr(amount: f64) -> String {
    let abs   = amount.abs();
    let sign  = if amount < 0.0 { "-" } else { "" };
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
        assert_eq!(fmt_inr(1000.0),      "1,000.00");
        assert_eq!(fmt_inr(100000.0),    "1,00,000.00");
        assert_eq!(fmt_inr(1234567.89),  "12,34,567.89");
        assert_eq!(fmt_inr(0.5),         "0.50");
    }

    #[test]
    fn fmt_inr_negative() {
        assert_eq!(fmt_inr(-1000.0), "-1,000.00");
    }
}
