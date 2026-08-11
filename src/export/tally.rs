// export/tally.rs — Tally Prime / ERP9 TDML XML export.
// Ports AccountingExportEngine.generate('tally', ...) from the original app.

use crate::parser::{Transaction, VoucherType};
use crate::tally_group_engine;
use std::collections::BTreeSet;

// ── Options mirroring the Tally Export modal UI ───────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct TallyOpts {
    pub company: String,
    // Verified against the original Electron app's tallyExportEngine
    // (accounting-export-engine.js's Tally exporter): the TDML voucher
    // import format it produces has no GSTIN/financial-year field at all —
    // only SVCURRENTCOMPANY. These two are collected by the shared export
    // wizard UI but genuinely don't apply to this specific output format,
    // in the original app as well as this port. Intentionally unread here;
    // see gen_generic_xml in accounting.rs for the format that does use them.
    #[allow(dead_code)]
    pub gstin: String,
    #[allow(dead_code)]
    pub fy: String, // e.g. "2024-25"
    pub bank_ledger: String,       // Tally ledger for the bank account
    pub date_from: Option<String>, // ISO YYYY-MM-DD
    pub date_to: Option<String>,
    pub only_classified: bool,
    pub include_ledgers: bool,
    pub include_narrations: bool,
    pub include_ob: bool,
    pub skip_low_conf: bool,
    /// Requirement #10: exclude rows the within-batch duplicate detector
    /// flagged (`Transaction.dup_flag`) — same opt-in pattern as
    /// `skip_low_conf`, defaulting to `false` (unchanged prior behavior:
    /// duplicate-flagged rows were always included with no way to exclude
    /// them, a real risk of double-posting a voucher in Tally).
    pub skip_duplicates: bool,
}

// ── Date helpers ──────────────────────────────────────────────────────────────

fn to_tally_date(s: &str) -> String {
    // DD/MM/YYYY → YYYYMMDD (Tally format)
    let p: Vec<&str> = s.split('/').collect();
    if p.len() == 3 && p[2].len() == 4 {
        return format!("{}{}{}", p[2], p[1].zfill2(), p[0].zfill2());
    }
    s.replace(['-', '/'], "")
}

pub fn to_iso_date(s: &str) -> String {
    // DD/MM/YYYY → YYYY-MM-DD
    let p: Vec<&str> = s.split('/').collect();
    if p.len() == 3 && p[2].len() == 4 {
        return format!("{}-{}-{}", p[2], p[1].zfill2(), p[0].zfill2());
    }
    s.to_string()
}

/// Is `s` a plausible `DD/MM/YYYY` calendar date — 3 numeric parts, a
/// 4-digit year, month 1-12, day 1-31? (Not a full calendar check — e.g.
/// 31/02/2026 passes — matching the level of rigor the rest of this parser
/// already applies to dates; see `to_iso_date` above, which accepts the same
/// shape without range-checking day/month at all.)
fn is_plausible_ddmmyyyy(s: &str) -> bool {
    let p: Vec<&str> = s.split('/').collect();
    if p.len() != 3 || p[2].len() != 4 {
        return false;
    }
    let dd: Option<u32> = p[0].parse().ok();
    let mm: Option<u32> = p[1].parse().ok();
    let yyyy_ok = p[2].parse::<i32>().is_ok();
    matches!(dd, Some(1..=31)) && matches!(mm, Some(1..=12)) && yyyy_ok
}

/// Validate and ISO-convert the export dialog's From/To Date fields
/// (Requirement #9 — used by both the Tally-only "Quick Export" modal and
/// the multi-format export wizard, so a bad date is rejected identically
/// everywhere instead of being silently mis-filtered).
///
/// An empty field means "no bound on that side" and is always valid. On
/// success, returns `(from_iso, to_iso)` — each `""` when that side was left
/// blank — ready to drop straight into `TallyOpts`/`AccountingOpts.date_from`/
/// `date_to` (via `Some(..)` when non-empty). On failure, returns a message
/// to show the user; the caller must not proceed with export in that case.
pub fn validate_and_convert_date_range(from_raw: &str, to_raw: &str) -> Result<(String, String), String> {
    let from_raw = from_raw.trim();
    let to_raw = to_raw.trim();

    if !from_raw.is_empty() && !is_plausible_ddmmyyyy(from_raw) {
        return Err("Invalid From Date — use DD/MM/YYYY".to_string());
    }
    if !to_raw.is_empty() && !is_plausible_ddmmyyyy(to_raw) {
        return Err("Invalid To Date — use DD/MM/YYYY".to_string());
    }

    let from_iso = if from_raw.is_empty() {
        String::new()
    } else {
        to_iso_date(from_raw)
    };
    let to_iso = if to_raw.is_empty() {
        String::new()
    } else {
        to_iso_date(to_raw)
    };

    // Compare as ISO strings — lexicographic order matches calendar order
    // for zero-padded YYYY-MM-DD, exactly like `in_range`'s own comparisons.
    if !from_iso.is_empty() && !to_iso.is_empty() && from_iso.as_str() > to_iso.as_str() {
        return Err("From Date must be on or before To Date".to_string());
    }

    Ok((from_iso, to_iso))
}

trait Zfill2 {
    fn zfill2(&self) -> String;
}
impl Zfill2 for &str {
    fn zfill2(&self) -> String {
        format!("{:0>2}", self)
    }
}

fn in_range(date_str: &str, from: &Option<String>, to: &Option<String>) -> bool {
    if date_str.is_empty() {
        return false;
    }
    let iso = to_iso_date(date_str);
    if let Some(f) = from {
        if iso.as_str() < f.as_str() {
            return false;
        }
    }
    if let Some(t) = to {
        if iso.as_str() > t.as_str() {
            return false;
        }
    }
    true
}

// ── XML escaping ──────────────────────────────────────────────────────────────

fn x(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// ── Voucher type resolution ───────────────────────────────────────────────────

fn voucher_type(t: &Transaction) -> &'static str {
    match &t.txn_type {
        VoucherType::Receipt => "Receipt",
        VoucherType::Contra => "Contra",
        VoucherType::Payment => "Payment",
        _ => {
            let n = t.narration.to_uppercase();
            if n.contains("ATM") || n.contains("CASH WDL") || n.contains("CASH DEP") {
                "Contra"
            } else if t.credit.is_some() && t.debit.is_none() {
                "Receipt"
            } else {
                "Payment"
            }
        }
    }
}

/// Ledger used for the contra entry (bank side).
fn bank_ledger_name(t: &Transaction, bank_ledger: &str) -> String {
    if !bank_ledger.is_empty() {
        return bank_ledger.to_string();
    }
    if !t.bank_name.is_empty() {
        return format!("{} A/c", t.bank_name);
    }
    "Bank Account".to_string()
}

/// Posting ledger (expense/income/party head).
fn posting_ledger(t: &Transaction) -> String {
    if !t.account_head.is_empty() {
        return t.account_head.clone();
    }
    if !t.vendor.is_empty() {
        return t.vendor.clone();
    }
    "Unclassified".to_string()
}

// ── Generate TDML XML ─────────────────────────────────────────────────────────

pub fn generate(txns: &[Transaction], opts: &TallyOpts, opening_bal: Option<f64>) -> String {
    let real: Vec<&Transaction> = txns
        .iter()
        .filter(|t| !t.is_opening_balance)
        .filter(|t| in_range(&t.date, &opts.date_from, &opts.date_to))
        .filter(|t| {
            if opts.only_classified {
                !matches!(t.status, crate::parser::TransactionStatus::Unreviewed)
                    && !matches!(t.status, crate::parser::TransactionStatus::Suspense)
            } else {
                true
            }
        })
        .filter(|t| {
            if opts.skip_low_conf {
                t.confidence >= 0.4
            } else {
                true
            }
        })
        .filter(|t| !opts.skip_duplicates || !t.dup_flag)
        .collect();

    let mut out = String::with_capacity(64 * 1024);

    // XML header
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<ENVELOPE>\n");
    out.push_str("  <HEADER>\n");
    out.push_str("    <TALLYREQUEST>Import Data</TALLYREQUEST>\n");
    out.push_str("  </HEADER>\n");
    out.push_str("  <BODY>\n");
    out.push_str("    <IMPORTDATA>\n");
    out.push_str("      <REQUESTDESC>\n");
    out.push_str("        <REPORTNAME>Vouchers</REPORTNAME>\n");
    out.push_str(&format!(
        "        <STATICVARIABLES><SVCURRENTCOMPANY>{}</SVCURRENTCOMPANY></STATICVARIABLES>\n",
        x(&opts.company)
    ));
    out.push_str("      </REQUESTDESC>\n");
    out.push_str("      <REQUESTDATA>\n");

    // ── Ledger creation entries ───────────────────────────────────────────────
    if opts.include_ledgers {
        let mut seen: BTreeSet<String> = BTreeSet::new();

        // Bank ledger itself
        if !opts.bank_ledger.is_empty() && seen.insert(opts.bank_ledger.clone()) {
            out.push_str(&ledger_master(
                &opts.bank_ledger,
                tally_group_engine::GROUP_BANK_ACCOUNTS,
            ));
        }

        // All posting/party ledgers — mirrors Electron's _tallyParent(): a party
        // ledger (posting ledger fell back to the vendor name, no real account
        // head) resolves by voucher direction; an expense/income ledger goes
        // through the real TallyGroupEngine keyword classifier.
        for t in &real {
            let pl = posting_ledger(t);
            if seen.insert(pl.clone()) {
                let is_party = t.account_head.is_empty() && !t.vendor.is_empty();
                let grp = if is_party {
                    if voucher_type(t) == "Receipt" {
                        tally_group_engine::GROUP_SUNDRY_DEBTORS.to_string()
                    } else {
                        tally_group_engine::GROUP_SUNDRY_CREDITORS.to_string()
                    }
                } else {
                    let is_credit = t.credit.is_some();
                    let amount = t.credit.unwrap_or(0.0) + t.debit.unwrap_or(0.0);
                    tally_group_engine::classify(&pl, &t.narration, is_credit, amount, None)
                };
                out.push_str(&ledger_master(&pl, &grp));
            }
        }
    }

    // ── Opening Balance journal entry ─────────────────────────────────────────
    if opts.include_ob {
        if let Some(ob) = opening_bal {
            if !real.is_empty() {
                let date = &real[0].date;
                out.push_str(&ob_voucher(
                    date,
                    &opts.bank_ledger,
                    &opts.company,
                    ob,
                    opts,
                ));
            }
        }
    }

    // ── Transaction vouchers ──────────────────────────────────────────────────
    for t in &real {
        out.push_str(&txn_voucher(t, opts));
    }

    out.push_str("      </REQUESTDATA>\n");
    out.push_str("    </IMPORTDATA>\n");
    out.push_str("  </BODY>\n");
    out.push_str("</ENVELOPE>\n");

    out
}

fn ledger_master(name: &str, group: &str) -> String {
    format!(
        "        <TALLYMESSAGE xmlns:UDF=\"TallyUDF\">\
         <LEDGER NAME=\"{}\" ACTION=\"Create\">\
         <NAME>{}</NAME>\
         <PARENT>{}</PARENT>\
         </LEDGER></TALLYMESSAGE>\n",
        x(name),
        x(name),
        x(group)
    )
}

fn ob_voucher(date: &str, bank_ledger: &str, _company: &str, ob: f64, opts: &TallyOpts) -> String {
    let td = to_tally_date(date);
    let narr = if opts.include_narrations {
        "Opening Balance"
    } else {
        ""
    };
    format!(
        "        <TALLYMESSAGE xmlns:UDF=\"TallyUDF\">\
         <VOUCHER VCHTYPE=\"Journal\" ACTION=\"Create\">\
         <DATE>{}</DATE><NARRATION>{}</NARRATION>\
         <VOUCHERTYPENAME>Journal</VOUCHERTYPENAME>\
         <ALLLEDGERENTRIES.LIST>\
         <LEDGERNAME>{}</LEDGERNAME>\
         <ISDEEMEDPOSITIVE>Yes</ISDEEMEDPOSITIVE>\
         <AMOUNT>-{:.2}</AMOUNT>\
         </ALLLEDGERENTRIES.LIST>\
         <ALLLEDGERENTRIES.LIST>\
         <LEDGERNAME>Capital Account</LEDGERNAME>\
         <ISDEEMEDPOSITIVE>No</ISDEEMEDPOSITIVE>\
         <AMOUNT>{:.2}</AMOUNT>\
         </ALLLEDGERENTRIES.LIST>\
         </VOUCHER></TALLYMESSAGE>\n",
        td,
        x(narr),
        x(bank_ledger),
        ob,
        ob
    )
}

fn txn_voucher(t: &Transaction, opts: &TallyOpts) -> String {
    let vt = voucher_type(t);
    let td = to_tally_date(&t.date);
    let narr = if opts.include_narrations {
        t.narration.as_str()
    } else {
        ""
    };
    let bank = bank_ledger_name(t, &opts.bank_ledger);
    let ledger = posting_ledger(t);
    let amt = t.debit.or(t.credit).unwrap_or(0.0);

    // For Receipt: bank Dr, posting Cr  → bank ispositive=Yes, posting ispositive=No
    // For Payment: posting Dr, bank Cr → posting ispositive=No, bank ispositive=Yes
    // For Contra:  bank Dr, bank Cr (cash/ATM)
    let (dr_ledger, cr_ledger, _is_receipt) = match vt {
        "Receipt" => (bank.as_str(), ledger.as_str(), true),
        "Contra" => (bank.as_str(), "Cash", true),
        _ => (ledger.as_str(), bank.as_str(), false),
    };

    format!(
        "        <TALLYMESSAGE xmlns:UDF=\"TallyUDF\">\
         <VOUCHER VCHTYPE=\"{vt}\" ACTION=\"Create\">\
         <DATE>{td}</DATE>\
         <NARRATION>{narr}</NARRATION>\
         <VOUCHERTYPENAME>{vt}</VOUCHERTYPENAME>\
         <ALLLEDGERENTRIES.LIST>\
         <LEDGERNAME>{dr}</LEDGERNAME>\
         <ISDEEMEDPOSITIVE>Yes</ISDEEMEDPOSITIVE>\
         <AMOUNT>-{amt:.2}</AMOUNT>\
         </ALLLEDGERENTRIES.LIST>\
         <ALLLEDGERENTRIES.LIST>\
         <LEDGERNAME>{cr}</LEDGERNAME>\
         <ISDEEMEDPOSITIVE>No</ISDEEMEDPOSITIVE>\
         <AMOUNT>{amt:.2}</AMOUNT>\
         </ALLLEDGERENTRIES.LIST>\
         </VOUCHER></TALLYMESSAGE>\n",
        vt = vt,
        td = td,
        narr = x(narr),
        amt = amt,
        dr = x(dr_ledger),
        cr = x(cr_ledger),
    )
}

// ── Count preview ─────────────────────────────────────────────────────────────

pub struct TallyPreview {
    pub total: usize,
    pub payment: usize,
    pub receipt: usize,
    pub contra: usize,
    pub gst: usize,
    /// Sum of gst_amount across GST-tagged transactions in this preview —
    /// backs the "verify CGST/SGST/IGST split" warning with a real figure
    /// instead of just a voucher count.
    pub gst_amount: f64,
    pub skipped: usize,
}

pub fn count_preview(txns: &[Transaction], opts: &TallyOpts) -> TallyPreview {
    let total_real = txns.iter().filter(|t| !t.is_opening_balance).count();
    let filtered: Vec<&Transaction> = txns
        .iter()
        .filter(|t| !t.is_opening_balance)
        .filter(|t| in_range(&t.date, &opts.date_from, &opts.date_to))
        .filter(|t| {
            if opts.only_classified {
                !matches!(t.status, crate::parser::TransactionStatus::Unreviewed)
                    && !matches!(t.status, crate::parser::TransactionStatus::Suspense)
            } else {
                true
            }
        })
        .filter(|t| {
            if opts.skip_low_conf {
                t.confidence >= 0.4
            } else {
                true
            }
        })
        .filter(|t| !opts.skip_duplicates || !t.dup_flag)
        .collect();

    let payment = filtered
        .iter()
        .filter(|t| voucher_type(t) == "Payment")
        .count();
    let receipt = filtered
        .iter()
        .filter(|t| voucher_type(t) == "Receipt")
        .count();
    let contra = filtered
        .iter()
        .filter(|t| voucher_type(t) == "Contra")
        .count();
    let gst_txns: Vec<&&Transaction> = filtered
        .iter()
        .filter(|t| t.tags.iter().any(|g| g == "GST"))
        .collect();
    let gst = gst_txns.len();
    let gst_amount = gst_txns.iter().filter_map(|t| t.gst_amount).sum();

    TallyPreview {
        total: filtered.len(),
        payment,
        receipt,
        contra,
        gst,
        gst_amount,
        skipped: total_real - filtered.len(),
    }
}

// ── Tests (Requirement #9: Tally XML export dialog / period filtering) ────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_and_convert_date_range ───────────────────────────────────────

    #[test]
    fn both_dates_empty_is_valid_and_means_no_bound() {
        let (from, to) = validate_and_convert_date_range("", "").unwrap();
        assert_eq!(from, "");
        assert_eq!(to, "");
    }

    #[test]
    fn valid_dates_convert_to_iso() {
        let (from, to) = validate_and_convert_date_range("05/01/2026", "01/02/2026").unwrap();
        assert_eq!(from, "2026-01-05");
        assert_eq!(to, "2026-02-01");
    }

    #[test]
    fn invalid_from_date_is_rejected() {
        let err = validate_and_convert_date_range("31/13/2026", "01/02/2026").unwrap_err();
        assert!(err.contains("From Date"), "{}", err);
    }

    #[test]
    fn invalid_to_date_is_rejected() {
        let err = validate_and_convert_date_range("05/01/2026", "not-a-date").unwrap_err();
        assert!(err.contains("To Date"), "{}", err);
    }

    #[test]
    fn garbage_text_in_from_date_is_rejected() {
        let err = validate_and_convert_date_range("abc", "").unwrap_err();
        assert!(err.contains("From Date"), "{}", err);
    }

    #[test]
    fn from_date_after_to_date_is_rejected() {
        let err = validate_and_convert_date_range("01/02/2026", "05/01/2026").unwrap_err();
        assert!(err.contains("on or before"), "{}", err);
    }

    #[test]
    fn from_date_equal_to_to_date_is_valid() {
        // A single-day period must be allowed — not treated as an error.
        let result = validate_and_convert_date_range("05/01/2026", "05/01/2026");
        assert!(result.is_ok());
    }

    #[test]
    fn only_from_date_set_is_valid() {
        let (from, to) = validate_and_convert_date_range("05/01/2026", "").unwrap();
        assert_eq!(from, "2026-01-05");
        assert_eq!(to, "");
    }

    // ── Period filtering: the exact scenario from Requirement #9-D ───────────
    // Imported dates: 01-01-2026, 05-01-2026, 15-01-2026, 01-02-2026, 15-02-2026.
    // Selected period: From 05-01-2026 To 01-02-2026 (inclusive both ends).
    // Expected: only 05-01, 15-01, 01-02 are exported; 01-01 and 15-02 are not.

    fn dated_txn(id: &str, date: &str, narration: &str) -> Transaction {
        Transaction {
            date: date.to_string(),
            narration: narration.to_string(),
            debit: Some(100.0),
            account_head: "Office Expense".to_string(),
            vendor: "Vendor".to_string(),
            txn_type: VoucherType::Payment,
            ..Transaction::new(id)
        }
    }

    fn requirement_9d_sample() -> Vec<Transaction> {
        vec![
            dated_txn("t1", "01/01/2026", "BEFORE PERIOD - JAN 1"),
            dated_txn("t2", "05/01/2026", "ON FROM BOUNDARY - JAN 5"),
            dated_txn("t3", "15/01/2026", "INSIDE PERIOD - JAN 15"),
            dated_txn("t4", "01/02/2026", "ON TO BOUNDARY - FEB 1"),
            dated_txn("t5", "15/02/2026", "AFTER PERIOD - FEB 15"),
        ]
    }

    fn period_opts() -> TallyOpts {
        let (from, to) = validate_and_convert_date_range("05/01/2026", "01/02/2026").unwrap();
        TallyOpts {
            company: "Acme Co".to_string(),
            bank_ledger: "HDFC Bank".to_string(),
            date_from: Some(from),
            date_to: Some(to),
            include_ledgers: false,
            include_narrations: true,
            include_ob: false,
            ..Default::default()
        }
    }

    #[test]
    fn count_preview_includes_exactly_the_three_in_period_transactions() {
        let p = count_preview(&requirement_9d_sample(), &period_opts());
        assert_eq!(p.total, 3, "05-Jan, 15-Jan, and 01-Feb must all be included");
        assert_eq!(p.skipped, 2, "01-Jan and 15-Feb must be excluded");
    }

    #[test]
    fn from_date_boundary_is_inclusive() {
        let xml = generate(&requirement_9d_sample(), &period_opts(), None);
        assert!(
            xml.contains("ON FROM BOUNDARY"),
            "a transaction dated exactly on From Date must be included:\n{}",
            xml
        );
    }

    #[test]
    fn to_date_boundary_is_inclusive() {
        let xml = generate(&requirement_9d_sample(), &period_opts(), None);
        assert!(
            xml.contains("ON TO BOUNDARY"),
            "a transaction dated exactly on To Date must be included:\n{}",
            xml
        );
    }

    #[test]
    fn transactions_outside_the_selected_period_are_excluded() {
        let xml = generate(&requirement_9d_sample(), &period_opts(), None);
        assert!(
            !xml.contains("BEFORE PERIOD"),
            "a transaction before From Date must not appear:\n{}",
            xml
        );
        assert!(
            !xml.contains("AFTER PERIOD"),
            "a transaction after To Date must not appear:\n{}",
            xml
        );
    }

    #[test]
    fn transaction_inside_the_period_is_included() {
        let xml = generate(&requirement_9d_sample(), &period_opts(), None);
        assert!(xml.contains("INSIDE PERIOD"), "{}", xml);
    }

    #[test]
    fn empty_from_to_date_exports_the_full_dataset() {
        let mut opts = period_opts();
        opts.date_from = None;
        opts.date_to = None;
        let p = count_preview(&requirement_9d_sample(), &opts);
        assert_eq!(p.total, 5, "with no period selected, nothing should be silently dropped");
    }

    // ── skip_duplicates (Requirement #10) ─────────────────────────────────────

    #[test]
    fn duplicate_flagged_rows_are_included_by_default() {
        // Existing behavior must not change for anyone not opting in.
        let mut txns = requirement_9d_sample();
        txns[2].dup_flag = true; // "INSIDE PERIOD" row
        let xml = generate(&txns, &period_opts(), None);
        assert!(xml.contains("INSIDE PERIOD"), "default behavior must still include flagged rows:\n{}", xml);
    }

    #[test]
    fn skip_duplicates_excludes_flagged_rows_when_enabled() {
        let mut txns = requirement_9d_sample();
        txns[2].dup_flag = true; // "INSIDE PERIOD" row
        let mut opts = period_opts();
        opts.skip_duplicates = true;
        let xml = generate(&txns, &opts, None);
        assert!(!xml.contains("INSIDE PERIOD"), "flagged row must be excluded when skip_duplicates is set:\n{}", xml);
        // The other two in-period, non-flagged rows are unaffected.
        assert!(xml.contains("ON FROM BOUNDARY"));
        assert!(xml.contains("ON TO BOUNDARY"));
    }

    #[test]
    fn skip_duplicates_reduces_the_preview_count() {
        let mut txns = requirement_9d_sample();
        txns[2].dup_flag = true;
        let mut opts = period_opts();
        opts.skip_duplicates = true;
        let p = count_preview(&txns, &opts);
        assert_eq!(p.total, 2, "the flagged row must not count toward the export total");
    }

    // ── Real XML validation (not just "did generate() return a String") ─────

    #[test]
    fn generated_xml_is_well_formed_and_parses_with_a_real_xml_reader() {
        let xml = generate(&requirement_9d_sample(), &period_opts(), Some(50_000.0));
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut voucher_opens = 0usize;
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(quick_xml::events::Event::Empty(e)) | Ok(quick_xml::events::Event::Start(e)) => {
                    if e.name().as_ref() == b"VOUCHER" {
                        voucher_opens += 1;
                    }
                }
                Ok(_) => {}
                Err(e) => panic!("generated Tally XML is not well-formed: {} in:\n{}", e, xml),
            }
        }
        assert_eq!(
            voucher_opens, 3,
            "exactly the 3 in-period transactions should produce a <VOUCHER> each"
        );
    }

    #[test]
    fn empty_transaction_set_still_produces_well_formed_xml() {
        let xml = generate(&[], &TallyOpts::default(), None);
        let mut reader = quick_xml::Reader::from_str(&xml);
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("empty-input Tally XML is not well-formed: {}", e),
            }
        }
    }
}
