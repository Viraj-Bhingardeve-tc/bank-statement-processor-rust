// export/tally.rs — Tally Prime / ERP9 TDML XML export.
// Ports AccountingExportEngine.generate('tally', ...) from the original app.

use crate::parser::{Transaction, VoucherType};
use crate::tally_group_engine;
use std::collections::{BTreeMap, BTreeSet};

// ── Options mirroring the Tally Export modal UI ───────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct TallyOpts {
    pub company:            String,
    pub gstin:              String,
    pub fy:                 String,         // e.g. "2024-25"
    pub bank_ledger:        String,         // Tally ledger for the bank account
    pub date_from:          Option<String>, // ISO YYYY-MM-DD
    pub date_to:            Option<String>,
    pub only_classified:    bool,
    pub include_ledgers:    bool,
    pub include_narrations: bool,
    pub include_ob:         bool,
    pub skip_low_conf:      bool,
}

// ── Date helpers ──────────────────────────────────────────────────────────────

fn to_tally_date(s: &str) -> String {
    // DD/MM/YYYY → YYYYMMDD (Tally format)
    let p: Vec<&str> = s.split('/').collect();
    if p.len() == 3 && p[2].len() == 4 {
        return format!("{}{}{}", p[2], p[1].zfill2(), p[0].zfill2());
    }
    s.replace('-', "").replace('/', "")
}

fn to_iso_date(s: &str) -> String {
    // DD/MM/YYYY → YYYY-MM-DD
    let p: Vec<&str> = s.split('/').collect();
    if p.len() == 3 && p[2].len() == 4 {
        return format!("{}-{}-{}", p[2], p[1].zfill2(), p[0].zfill2());
    }
    s.to_string()
}

trait Zfill2 { fn zfill2(&self) -> String; }
impl Zfill2 for &str {
    fn zfill2(&self) -> String { format!("{:0>2}", self) }
}

fn in_range(date_str: &str, from: &Option<String>, to: &Option<String>) -> bool {
    if date_str.is_empty() { return false; }
    let iso = to_iso_date(date_str);
    if let Some(f) = from { if iso.as_str() < f.as_str() { return false; } }
    if let Some(t) = to   { if iso.as_str() > t.as_str() { return false; } }
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
        VoucherType::Contra  => "Contra",
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
    if !bank_ledger.is_empty() { return bank_ledger.to_string(); }
    if !t.bank_name.is_empty() { return format!("{} A/c", t.bank_name); }
    "Bank Account".to_string()
}

/// Posting ledger (expense/income/party head).
fn posting_ledger(t: &Transaction) -> String {
    if !t.account_head.is_empty()  { return t.account_head.clone(); }
    if !t.vendor.is_empty()        { return t.vendor.clone(); }
    "Unclassified".to_string()
}

// ── Generate TDML XML ─────────────────────────────────────────────────────────

pub fn generate(txns: &[Transaction], opts: &TallyOpts, opening_bal: Option<f64>) -> String {
    let real: Vec<&Transaction> = txns.iter()
        .filter(|t| !t.is_opening_balance)
        .filter(|t| in_range(&t.date, &opts.date_from, &opts.date_to))
        .filter(|t| {
            if opts.only_classified {
                !matches!(t.status, crate::parser::TransactionStatus::Unreviewed)
                    && !matches!(t.status, crate::parser::TransactionStatus::Suspense)
            } else { true }
        })
        .filter(|t| {
            if opts.skip_low_conf { t.confidence >= 0.4 } else { true }
        })
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
    out.push_str(&format!("        <STATICVARIABLES><SVCURRENTCOMPANY>{}</SVCURRENTCOMPANY></STATICVARIABLES>\n", x(&opts.company)));
    out.push_str("      </REQUESTDESC>\n");
    out.push_str("      <REQUESTDATA>\n");

    // ── Ledger creation entries ───────────────────────────────────────────────
    if opts.include_ledgers {
        let mut seen: BTreeSet<String> = BTreeSet::new();

        // Bank ledger itself
        if !opts.bank_ledger.is_empty() && seen.insert(opts.bank_ledger.clone()) {
            out.push_str(&ledger_master(&opts.bank_ledger, tally_group_engine::GROUP_BANK_ACCOUNTS));
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
                out.push_str(&ob_voucher(date, &opts.bank_ledger, &opts.company, ob, opts));
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
        x(name), x(name), x(group)
    )
}

fn ob_voucher(date: &str, bank_ledger: &str, _company: &str, ob: f64, opts: &TallyOpts) -> String {
    let td = to_tally_date(date);
    let narr = if opts.include_narrations { "Opening Balance" } else { "" };
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
        td, x(narr), x(bank_ledger), ob, ob
    )
}

fn txn_voucher(t: &Transaction, opts: &TallyOpts) -> String {
    let vt     = voucher_type(t);
    let td     = to_tally_date(&t.date);
    let narr   = if opts.include_narrations { t.narration.as_str() } else { "" };
    let bank   = bank_ledger_name(t, &opts.bank_ledger);
    let ledger = posting_ledger(t);
    let amt    = t.debit.or(t.credit).unwrap_or(0.0);

    // For Receipt: bank Dr, posting Cr  → bank ispositive=Yes, posting ispositive=No
    // For Payment: posting Dr, bank Cr → posting ispositive=No, bank ispositive=Yes
    // For Contra:  bank Dr, bank Cr (cash/ATM)
    let (dr_ledger, cr_ledger, is_receipt) = match vt {
        "Receipt" => (bank.as_str(), ledger.as_str(), true),
        "Contra"  => (bank.as_str(), "Cash",          true),
        _         => (ledger.as_str(), bank.as_str(),  false),
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
        vt = vt, td = td, narr = x(narr), amt = amt,
        dr = x(dr_ledger), cr = x(cr_ledger),
    )
}

// ── Count preview ─────────────────────────────────────────────────────────────

pub struct TallyPreview {
    pub total:   usize,
    pub payment: usize,
    pub receipt: usize,
    pub contra:  usize,
    pub gst:     usize,
    /// Sum of gst_amount across GST-tagged transactions in this preview —
    /// backs the "verify CGST/SGST/IGST split" warning with a real figure
    /// instead of just a voucher count.
    pub gst_amount: f64,
    pub skipped: usize,
}

pub fn count_preview(txns: &[Transaction], opts: &TallyOpts) -> TallyPreview {
    let total_real = txns.iter().filter(|t| !t.is_opening_balance).count();
    let filtered: Vec<&Transaction> = txns.iter()
        .filter(|t| !t.is_opening_balance)
        .filter(|t| in_range(&t.date, &opts.date_from, &opts.date_to))
        .filter(|t| {
            if opts.only_classified {
                !matches!(t.status, crate::parser::TransactionStatus::Unreviewed)
                && !matches!(t.status, crate::parser::TransactionStatus::Suspense)
            } else { true }
        })
        .filter(|t| if opts.skip_low_conf { t.confidence >= 0.4 } else { true })
        .collect();

    let payment = filtered.iter().filter(|t| voucher_type(t) == "Payment").count();
    let receipt = filtered.iter().filter(|t| voucher_type(t) == "Receipt").count();
    let contra  = filtered.iter().filter(|t| voucher_type(t) == "Contra").count();
    let gst_txns: Vec<&&Transaction> = filtered.iter().filter(|t| t.tags.iter().any(|g| g == "GST")).collect();
    let gst        = gst_txns.len();
    let gst_amount = gst_txns.iter().filter_map(|t| t.gst_amount).sum();

    TallyPreview {
        total:   filtered.len(),
        payment, receipt, contra, gst, gst_amount,
        skipped: total_real - filtered.len(),
    }
}
