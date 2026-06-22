// export/accounting.rs — Accounting export wizard: Zoho CSV, QB CSV, Odoo CSV, Generic XML.
// Mirrors AccountingExportEngine from the original app.

use crate::parser::{Transaction, TransactionStatus, VoucherType};
use crate::export::tally::{self, TallyOpts};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Software {
    Tally,
    Zoho,
    QuickBooks,
    Odoo,
    Excel,
    Xml,
}

impl Software {
    pub fn from_idx(idx: i32) -> Self {
        match idx {
            0 => Self::Tally,
            1 => Self::Zoho,
            2 => Self::QuickBooks,
            3 => Self::Odoo,
            4 => Self::Excel,
            _ => Self::Xml,
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Self::Tally | Self::Xml => "xml",
            _ => "csv",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Tally      => "Tally Prime / ERP9",
            Self::Zoho       => "Zoho Books",
            Self::QuickBooks => "QuickBooks",
            Self::Odoo       => "Odoo",
            Self::Excel      => "Excel / CSV",
            Self::Xml        => "Generic XML",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AccountingOpts {
    pub software:            Software,
    pub company:             String,
    pub gstin:               String,
    pub fy:                  String,
    pub state_code:          String,
    pub currency:            String,
    pub bank_ledger:         String,
    pub date_from:           Option<String>,
    pub date_to:             Option<String>,
    pub include_ob:          bool,
    pub include_gst:         bool,
    pub include_ledgers:     bool,
    pub include_narrations:  bool,
    pub only_classified:     bool,
    pub skip_low_conf:       bool,
}

impl Default for Software {
    fn default() -> Self { Self::Tally }
}

/// Generate export content. Returns (content_string, filename_extension).
pub fn generate(txns: &[Transaction], opts: &AccountingOpts, opening_bal: Option<f64>) -> String {
    match opts.software {
        Software::Tally => {
            let tally_opts = TallyOpts {
                company:            opts.company.clone(),
                gstin:              opts.gstin.clone(),
                fy:                 opts.fy.clone(),
                bank_ledger:        opts.bank_ledger.clone(),
                date_from:          opts.date_from.clone(),
                date_to:            opts.date_to.clone(),
                only_classified:    opts.only_classified,
                include_ledgers:    opts.include_ledgers,
                include_narrations: opts.include_narrations,
                include_ob:         opts.include_ob,
                skip_low_conf:      opts.skip_low_conf,
            };
            tally::generate(txns, &tally_opts, opening_bal)
        }
        Software::Zoho       => gen_zoho(txns, opts),
        Software::QuickBooks => gen_quickbooks(txns, opts),
        Software::Odoo       => gen_odoo(txns, opts),
        Software::Excel      => gen_generic_csv(txns, opts),
        Software::Xml        => gen_generic_xml(txns, opts),
    }
}

// ── Pre-export validation ──────────────────────────────────────────────────────
// Port of Electron's AccountingExportEngine.validate(): runs against exactly the
// same filtered set generate() would export, so warnings/errors always match
// what's about to be written. Errors block export; warnings don't.

#[derive(Debug, Clone, Default)]
pub struct ExportValidation {
    pub errors:   Vec<String>,
    pub warnings: Vec<String>,
}

impl ExportValidation {
    pub fn can_export(&self) -> bool { self.errors.is_empty() }
}

pub fn validate(txns: &[Transaction], opts: &AccountingOpts) -> ExportValidation {
    let filtered = filter_txns(txns, opts);
    let mut errors   = Vec::new();
    let mut warnings = Vec::new();

    let self_posted = filtered.iter()
        .filter(|t| !opts.bank_ledger.is_empty() && posting_ledger(t) == opts.bank_ledger.as_str())
        .count();
    let bad_date = filtered.iter()
        .filter(|t| t.date.is_empty() || t.date.split('/').count() != 3)
        .count();
    let zero_amt = filtered.iter().filter(|t| amt(t) <= 0.0).count();
    let unclassified = filtered.iter().filter(|t| posting_ledger(t) == "Unclassified").count();
    let low_conf = filtered.iter().filter(|t| t.confidence > 0.0 && t.confidence < 0.4).count();
    let gst_txns: Vec<&&Transaction> = filtered.iter()
        .filter(|t| t.tags.iter().any(|g| g == "GST" || g == "TAX"))
        .collect();
    let gst_tagged = gst_txns.len();
    let gst_amount_total: f64 = gst_txns.iter().filter_map(|t| t.gst_amount).sum();

    if self_posted > 0 {
        errors.push(format!("{} voucher(s) have identical Dr/Cr ledger — Tally will reject", self_posted));
    }
    if bad_date > 0 {
        errors.push(format!("{} voucher(s) have missing or invalid dates", bad_date));
    }
    if zero_amt > 0 {
        warnings.push(format!("{} voucher(s) have zero or negative amount", zero_amt));
    }
    if unclassified > 0 {
        warnings.push(format!("{} voucher(s) map to \"Unclassified\" ledger — verify in your accounting software", unclassified));
    }
    if low_conf > 0 {
        warnings.push(format!("{} voucher(s) have low confidence (<40%) — ledger may be incorrect", low_conf));
    }
    if gst_tagged > 0 {
        if gst_amount_total > 0.0 {
            warnings.push(format!(
                "{} GST-tagged voucher(s), estimated tax \u{20b9}{:.2} — verify CGST/SGST/IGST split",
                gst_tagged, gst_amount_total,
            ));
        } else {
            warnings.push(format!("{} GST-tagged voucher(s) — manually verify CGST/SGST/IGST split", gst_tagged));
        }
    }

    ExportValidation { errors, warnings }
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn filter_txns<'a>(txns: &'a [Transaction], opts: &AccountingOpts) -> Vec<&'a Transaction> {
    txns.iter()
        .filter(|t| !t.is_opening_balance)
        .filter(|t| in_date_range(&t.date, &opts.date_from, &opts.date_to))
        .filter(|t| {
            if opts.only_classified {
                !matches!(t.status, TransactionStatus::Unreviewed)
                && !matches!(t.status, TransactionStatus::Suspense)
            } else { true }
        })
        .filter(|t| if opts.skip_low_conf { t.confidence >= 0.4 } else { true })
        .collect()
}

fn in_date_range(date: &str, from: &Option<String>, to: &Option<String>) -> bool {
    if date.is_empty() { return true; }
    let iso = date_to_iso(date);
    if let Some(f) = from { if iso.as_str() < f.as_str() { return false; } }
    if let Some(t) = to   { if iso.as_str() > t.as_str() { return false; } }
    true
}

fn date_to_iso(s: &str) -> String {
    let p: Vec<&str> = s.split('/').collect();
    if p.len() == 3 && p[2].len() == 4 {
        return format!("{}-{:0>2}-{:0>2}", p[2], p[1], p[0]);
    }
    s.to_string()
}

fn date_to_us(s: &str) -> String {
    let p: Vec<&str> = s.split('/').collect();
    if p.len() == 3 && p[2].len() == 4 {
        return format!("{:0>2}/{:0>2}/{}", p[1], p[0], p[2]);
    }
    s.to_string()
}

fn csv_val(v: &str) -> String {
    if v.contains(',') || v.contains('"') || v.contains('\n') {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else { v.to_string() }
}

fn csv_row(cells: &[String]) -> String {
    cells.iter().map(|c| csv_val(c)).collect::<Vec<_>>().join(",")
}

fn posting_ledger(t: &Transaction) -> &str {
    if !t.account_head.is_empty() { return &t.account_head; }
    if !t.vendor.is_empty()       { return &t.vendor; }
    "Unclassified"
}

fn vt(t: &Transaction) -> &'static str {
    match &t.txn_type {
        VoucherType::Receipt => "Receipt",
        VoucherType::Payment => "Payment",
        VoucherType::Contra  => "Contra",
        _ => if t.credit.is_some() { "Receipt" } else { "Payment" },
    }
}

fn amt(t: &Transaction) -> f64 {
    t.debit.or(t.credit).unwrap_or(0.0)
}

// ── Zoho Books CSV ────────────────────────────────────────────────────────────

fn gen_zoho(txns: &[Transaction], opts: &AccountingOpts) -> String {
    let filtered = filter_txns(txns, opts);
    let mut rows: Vec<String> = vec![csv_row(&[
        "JournalDate","JournalNumber","Notes","ReferenceNumber","CurrencyCode",
        "Account","AccountType","ContactName","Description","Debit","Credit",
        "Tags","TaxName","TaxType","TaxPercentage",
    ].map(|s| s.to_string()).to_vec())];

    for (i, t) in filtered.iter().enumerate() {
        let jnum = format!("BSP-{:05}", i + 1);
        let cur = if opts.currency.is_empty() { "INR" } else { &opts.currency };
        let date = date_to_iso(&t.date);
        let notes = if opts.include_narrations { &t.narration } else { "" };
        let amt_val = amt(t);
        let ledger = posting_ledger(t);

        // Double-entry: two rows per transaction (bank side + posting side)
        let (bank_dr, bank_cr, posting_dr, posting_cr) = if vt(t) == "Receipt" {
            (amt_val, 0.0, 0.0, amt_val)
        } else {
            (0.0, amt_val, amt_val, 0.0)
        };

        // GST figures belong on the posting/expense-income leg, not the
        // bank leg — the cash movement itself isn't taxed. Previously these
        // three columns were always blank even though gst_engine computed
        // real values for every GST-tagged transaction (see
        // PRODUCTION_READINESS_AUDIT_2026-06-22.md Phase 2 item 3); only
        // populate them when the user has actually asked for GST data via
        // the export wizard's "include GST" option.
        let (tax_name, tax_type, tax_pct) = if opts.include_gst {
            match (&t.gst_type, t.gst_rate) {
                (Some(gt), Some(rate)) => (
                    gt.clone(),
                    if t.debit.is_some() { "Purchase Tax" } else { "Sales Tax" }.to_string(),
                    format!("{:.2}", rate),
                ),
                _ => (String::new(), String::new(), String::new()),
            }
        } else {
            (String::new(), String::new(), String::new())
        };

        rows.push(csv_row(&[
            date.clone(), jnum.clone(), notes.to_string(), t.reference.clone(), cur.to_string(),
            opts.bank_ledger.clone(), "Bank".to_string(), t.vendor.clone(), notes.to_string(),
            format!("{:.2}", bank_dr), format!("{:.2}", bank_cr),
            "".to_string(), "".to_string(), "".to_string(), "".to_string(),
        ]));
        rows.push(csv_row(&[
            date, jnum, notes.to_string(), t.reference.clone(), cur.to_string(),
            ledger.to_string(), "Expense".to_string(), t.vendor.clone(), notes.to_string(),
            format!("{:.2}", posting_dr), format!("{:.2}", posting_cr),
            "".to_string(), tax_name, tax_type, tax_pct,
        ]));
    }

    "\u{FEFF}".to_string() + &rows.join("\r\n")
}

// ── QuickBooks General Journal CSV ────────────────────────────────────────────

fn gen_quickbooks(txns: &[Transaction], opts: &AccountingOpts) -> String {
    let filtered = filter_txns(txns, opts);
    let mut rows: Vec<String> = vec![csv_row(&[
        "Date","JournalNo","Memo","AccountName","Debit","Credit","Name",
    ].map(|s| s.to_string()).to_vec())];

    for (i, t) in filtered.iter().enumerate() {
        let jnum = format!("BSP{:05}", i + 1);
        let date = date_to_us(&t.date);
        let memo = if opts.include_narrations { &t.narration } else { "" };
        let amt_val = amt(t);
        let ledger = posting_ledger(t);

        let (bank_dr, bank_cr, posting_dr, posting_cr) = if vt(t) == "Receipt" {
            (amt_val, 0.0, 0.0, amt_val)
        } else {
            (0.0, amt_val, amt_val, 0.0)
        };

        rows.push(csv_row(&[
            date.clone(), jnum.clone(), memo.to_string(),
            opts.bank_ledger.clone(), format!("{:.2}", bank_dr), format!("{:.2}", bank_cr),
            t.vendor.clone(),
        ]));
        rows.push(csv_row(&[
            date, jnum, memo.to_string(),
            ledger.to_string(), format!("{:.2}", posting_dr), format!("{:.2}", posting_cr),
            t.vendor.clone(),
        ]));
    }

    "\u{FEFF}".to_string() + &rows.join("\r\n")
}

// ── Odoo account.move.line CSV ────────────────────────────────────────────────

fn gen_odoo(txns: &[Transaction], opts: &AccountingOpts) -> String {
    let filtered = filter_txns(txns, opts);
    let mut rows: Vec<String> = vec![csv_row(&[
        "date","move_type","name","partner_id/name","account_id/code","debit","credit",
        "narration","ref",
    ].map(|s| s.to_string()).to_vec())];

    for t in &filtered {
        let date = date_to_iso(&t.date);
        let narr = if opts.include_narrations { &t.narration } else { "" };
        let amt_val = amt(t);
        let ledger = posting_ledger(t);

        let (bank_dr, bank_cr, posting_dr, posting_cr) = if vt(t) == "Receipt" {
            (amt_val, 0.0, 0.0, amt_val)
        } else {
            (0.0, amt_val, amt_val, 0.0)
        };

        rows.push(csv_row(&[
            date.clone(), "entry".to_string(), narr.to_string(), t.vendor.clone(),
            opts.bank_ledger.clone(), format!("{:.2}", bank_dr), format!("{:.2}", bank_cr),
            narr.to_string(), t.reference.clone(),
        ]));
        rows.push(csv_row(&[
            date, "entry".to_string(), narr.to_string(), t.vendor.clone(),
            ledger.to_string(), format!("{:.2}", posting_dr), format!("{:.2}", posting_cr),
            narr.to_string(), t.reference.clone(),
        ]));
    }

    "\u{FEFF}".to_string() + &rows.join("\r\n")
}

// ── Generic CSV ───────────────────────────────────────────────────────────────

fn gen_generic_csv(txns: &[Transaction], opts: &AccountingOpts) -> String {
    let filtered = filter_txns(txns, opts);
    let mut rows: Vec<String> = vec![csv_row(&[
        "Date","Narration","Reference","VoucherType",
        "DebitLedger","CreditLedger","Amount","Tags","Status",
    ].map(|s| s.to_string()).to_vec())];

    for t in &filtered {
        let vt_str = vt(t);
        let ledger = posting_ledger(t);
        let amt_val = amt(t);
        let (dr_ledger, cr_ledger) = if vt_str == "Receipt" {
            (opts.bank_ledger.as_str(), ledger)
        } else {
            (ledger, opts.bank_ledger.as_str())
        };

        rows.push(csv_row(&[
            date_to_iso(&t.date), t.narration.clone(), t.reference.clone(), vt_str.to_string(),
            dr_ledger.to_string(), cr_ledger.to_string(), format!("{:.2}", amt_val),
            t.tags.join("; "), t.status.to_string(),
        ]));
    }

    "\u{FEFF}".to_string() + &rows.join("\r\n")
}

// ── Generic XML ───────────────────────────────────────────────────────────────

fn x(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
     .replace('"', "&quot;").replace('\'', "&apos;")
}

fn gen_generic_xml(txns: &[Transaction], opts: &AccountingOpts) -> String {
    let filtered = filter_txns(txns, opts);
    let mut out = String::new();

    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<BankStatementExport>\n");
    out.push_str(&format!("  <Company>{}</Company>\n", x(&opts.company)));
    // GSTIN/FinancialYear/StateCode are collected by the export wizard UI
    // (main.rs's wiz_gstin/wiz_fy_idx/wiz_state_idx) and were previously
    // dropped on the floor in this format — the original Electron
    // AccountingExportEngine's generic-XML exporter emits GSTIN and
    // FinancialYear in the equivalent spot (StateCode has no upstream
    // precedent anywhere in the original export engine; added here since
    // this is this app's own custom format, not an external system's fixed
    // import schema like Zoho/QuickBooks/Odoo).
    out.push_str(&format!("  <GSTIN>{}</GSTIN>\n", x(&opts.gstin)));
    out.push_str(&format!("  <FinancialYear>{}</FinancialYear>\n", x(&opts.fy)));
    out.push_str(&format!("  <StateCode>{}</StateCode>\n", x(&opts.state_code)));
    out.push_str(&format!("  <BankLedger>{}</BankLedger>\n", x(&opts.bank_ledger)));
    out.push_str("  <Transactions>\n");

    for t in &filtered {
        let amt_val = amt(t);
        let ledger = posting_ledger(t);
        out.push_str("    <Transaction>\n");
        out.push_str(&format!("      <Date>{}</Date>\n", date_to_iso(&t.date)));
        out.push_str(&format!("      <Narration>{}</Narration>\n", x(&t.narration)));
        out.push_str(&format!("      <Reference>{}</Reference>\n", x(&t.reference)));
        out.push_str(&format!("      <VoucherType>{}</VoucherType>\n", vt(t)));
        out.push_str(&format!("      <Amount>{:.2}</Amount>\n", amt_val));
        out.push_str(&format!("      <DebitLedger>{}</DebitLedger>\n",
            x(if vt(t) == "Receipt" { &opts.bank_ledger } else { ledger })));
        out.push_str(&format!("      <CreditLedger>{}</CreditLedger>\n",
            x(if vt(t) == "Receipt" { ledger } else { &opts.bank_ledger })));
        out.push_str(&format!("      <Vendor>{}</Vendor>\n", x(&t.vendor)));
        out.push_str(&format!("      <Status>{}</Status>\n", x(&t.status.to_string())));
        out.push_str("    </Transaction>\n");
    }

    out.push_str("  </Transactions>\n");
    out.push_str("</BankStatementExport>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{Transaction, VoucherType, TransactionStatus};

    fn opts(software: Software) -> AccountingOpts {
        AccountingOpts {
            software,
            company: "Acme Co".to_string(),
            gstin: "27AAAPL1234C1ZV".to_string(),
            fy: "2024-25".to_string(),
            state_code: "MH".to_string(),
            currency: "INR".to_string(),
            bank_ledger: "HDFC Bank".to_string(),
            include_gst: true,
            ..Default::default()
        }
    }

    fn gst_txn() -> Transaction {
        let mut t = Transaction::new("t1");
        t.date = "01/04/2026".to_string();
        t.narration = "AIRTEL POSTPAID BILL".to_string();
        t.debit = Some(999.0);
        t.account_head = "Telephone Expense".to_string();
        t.txn_type = VoucherType::Payment;
        t.status = TransactionStatus::Classified;
        t.tags = vec!["GST".to_string()];
        t.gst_type = Some("CGST+SGST".to_string());
        t.gst_rate = Some(18.0);
        t.gst_amount = Some(152.37);
        t
    }

    #[test]
    fn generic_xml_includes_gstin_fy_and_state_code() {
        let xml = generate(&[gst_txn()], &opts(Software::Xml), None);
        assert!(xml.contains("<GSTIN>27AAAPL1234C1ZV</GSTIN>"), "{}", xml);
        assert!(xml.contains("<FinancialYear>2024-25</FinancialYear>"), "{}", xml);
        assert!(xml.contains("<StateCode>MH</StateCode>"), "{}", xml);
    }

    #[test]
    fn zoho_csv_populates_tax_columns_when_include_gst_is_set() {
        let csv = generate(&[gst_txn()], &opts(Software::Zoho), None);
        assert!(csv.contains("CGST+SGST"), "tax name missing from Zoho export:\n{}", csv);
        assert!(csv.contains("18.00"), "tax percentage missing from Zoho export:\n{}", csv);
    }

    #[test]
    fn zoho_csv_leaves_tax_columns_blank_when_include_gst_is_unset() {
        let mut o = opts(Software::Zoho);
        o.include_gst = false;
        let csv = generate(&[gst_txn()], &o, None);
        assert!(!csv.contains("CGST+SGST"), "tax name must not appear when include_gst is false:\n{}", csv);
    }

    #[test]
    fn tally_xml_does_not_reference_gstin_or_fy() {
        // Verified against the original Electron tallyExportEngine: the
        // TDML voucher format has no GSTIN/financial-year field at all.
        let xml = generate(&[gst_txn()], &opts(Software::Tally), None);
        assert!(!xml.contains("27AAAPL1234C1ZV"));
        assert!(!xml.contains("2024-25"));
    }
}
