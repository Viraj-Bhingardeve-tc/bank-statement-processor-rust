// export/excel.rs — Export transactions to CSV or real XLSX.

use crate::parser::Transaction;
use std::path::Path;

// ── INR formatter (no ₹ symbol for CSV cells) ────────────────────────────────

fn fmt_amt(v: Option<f64>) -> String {
    match v {
        None => String::new(),
        Some(n) => {
            let (sign, abs) = if n < 0.0 { ("-", -n) } else { ("", n) };
            let int_part = abs as u64;
            let frac = ((abs - int_part as f64) * 100.0).round() as u64;
            let s = int_part.to_string();
            let formatted = if s.len() <= 3 {
                s.clone()
            } else {
                let (first, rest) = s.split_at(s.len() - 3);
                let mut parts = vec![rest.to_string()];
                let mut rem = first;
                while rem.len() > 2 {
                    let (r, chunk) = rem.split_at(rem.len() - 2);
                    parts.push(chunk.to_string());
                    rem = r;
                }
                if !rem.is_empty() {
                    parts.push(rem.to_string());
                }
                parts.reverse();
                parts.join(",")
            };
            format!("{}{}.{:02}", sign, formatted, frac)
        }
    }
}

// ── CSV helpers ───────────────────────────────────────────────────────────────

fn csv_val(v: &str) -> String {
    if v.contains(',') || v.contains('"') || v.contains('\n') {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

fn csv_row(cells: &[&str]) -> String {
    cells
        .iter()
        .map(|c| csv_val(c))
        .collect::<Vec<_>>()
        .join(",")
}

// ── Posting ledger / party group (mirrors export/accounting.rs + tally.rs) ───

fn posting_ledger(t: &Transaction) -> &str {
    if !t.account_head.is_empty() {
        return &t.account_head;
    }
    if !t.vendor.is_empty() {
        return &t.vendor;
    }
    "Unclassified"
}

/// "SDr"/"SCr" for party (vendor-as-ledger) rows, empty for category-head rows —
/// mirrors Electron's `t.partyGroup`, which Rust doesn't track as a stored field.
fn party_group(t: &Transaction) -> &'static str {
    if !t.account_head.is_empty() {
        return "";
    }
    if t.vendor.is_empty() {
        return "";
    }
    if t.credit.is_some() && t.debit.is_none() {
        "SDr"
    } else {
        "SCr"
    }
}

/// Per (bank, account) breakdown: opening balance, closing balance, txn count.
struct AccountBreakdown {
    bank_name: String,
    account_no: String,
    opening_bal: Option<f64>,
    closing_bal: Option<f64>,
    txn_count: usize,
}

fn account_breakdowns(real: &[&Transaction]) -> Vec<AccountBreakdown> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut groups: std::collections::HashMap<(String, String), Vec<&Transaction>> =
        std::collections::HashMap::new();
    for t in real {
        let key = (t.bank_name.clone(), t.account_no.clone());
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(t);
    }
    order
        .into_iter()
        .map(|key| {
            let txns = &groups[&key];
            AccountBreakdown {
                bank_name: key.0,
                account_no: key.1,
                opening_bal: txns.first().and_then(|t| t.prev_balance),
                closing_bal: txns.last().and_then(|t| t.balance),
                txn_count: txns.len(),
            }
        })
        .collect()
}

// ── Main export function ──────────────────────────────────────────────────────

/// Export transactions to a CSV file at `path`.
/// The file contains 4 sections separated by blank lines, mimicking multiple
/// Excel sheets: Transactions, Summary, Receipt Heads, Payment Heads.
pub fn export_csv(
    txns: &[Transaction],
    client_name: &str,
    bank_ledger: &str,
    file_name: &str,
    opening_bal: Option<f64>,
    closing_bal: Option<f64>,
    path: impl AsRef<Path>,
) -> anyhow::Result<usize> {
    use std::io::Write as _;

    let real: Vec<&Transaction> = txns.iter().filter(|t| !t.is_opening_balance).collect();

    let total_dr: f64 = real.iter().filter_map(|t| t.debit).sum();
    let total_cr: f64 = real.iter().filter_map(|t| t.credit).sum();

    let mut lines: Vec<String> = Vec::new();

    // ── Sheet: Transactions ───────────────────────────────────────────────────
    lines.push("=== TRANSACTIONS ===".to_string());
    lines.push(csv_row(&[
        "Date",
        "Narration",
        "Reference",
        "Debit",
        "Credit",
        "Balance",
        "Vendor",
        "Account Head",
        "Ledger for Posting",
        "Party Group",
        "Type",
        "Status",
        "Tags",
        "Confidence",
        "Classification Source",
        "Classification Reason",
        "Classified By",
        "Bank Name",
        "Account No",
    ]));
    for t in &real {
        let classified_by = if t.classification_source.is_empty() {
            "local"
        } else {
            &t.classification_source
        };
        lines.push(csv_row(&[
            &t.date,
            &t.narration,
            &t.reference,
            &fmt_amt(t.debit),
            &fmt_amt(t.credit),
            &fmt_amt(t.balance),
            &t.vendor,
            &t.account_head,
            posting_ledger(t),
            party_group(t),
            &t.txn_type.to_string(),
            &t.status.to_string(),
            &t.tags.join("; "),
            &format!("{:.2}", t.confidence),
            &t.classification_source,
            "",
            classified_by,
            &t.bank_name,
            &t.account_no,
        ]));
    }

    // ── Sheet: Summary ────────────────────────────────────────────────────────
    lines.push(String::new());
    lines.push("=== SUMMARY ===".to_string());
    let calc_cl = opening_bal.map(|ob| (ob + total_cr - total_dr).round() / 1.0);
    let stated_cl = closing_bal;
    let recon = match (calc_cl, stated_cl) {
        (Some(c), Some(s)) if (c - s).abs() < 0.5 => "RECONCILED".to_string(),
        (Some(c), Some(s)) => format!("MISMATCH (diff: {:.2})", (c - s).abs()),
        _ => "N/A".to_string(),
    };

    let summary_rows: Vec<(&str, String)> = vec![
        ("Client", client_name.to_string()),
        ("Bank Ledger", bank_ledger.to_string()),
        ("File", file_name.to_string()),
        ("Total Receipts (Credit)", fmt_amt(Some(total_cr))),
        ("Total Payments (Debit)", fmt_amt(Some(total_dr))),
        ("Net", fmt_amt(Some(total_cr - total_dr))),
        ("Opening Balance", fmt_amt(opening_bal)),
        ("Closing Balance (Stated)", fmt_amt(stated_cl)),
        ("Closing Balance (Calc.)", fmt_amt(calc_cl)),
        ("Reconciliation", recon),
        (
            "Duplicate Transactions",
            real.iter().filter(|t| t.dup_flag).count().to_string(),
        ),
        (
            "GST Transactions",
            real.iter()
                .filter(|t| t.tags.iter().any(|g| g == "GST"))
                .count()
                .to_string(),
        ),
    ];
    for row in &summary_rows {
        lines.push(csv_row(&[row.0, row.1.as_str()]));
    }

    let breakdowns = account_breakdowns(&real);
    if breakdowns.len() > 1 {
        lines.push(String::new());
        lines.push("=== BANK ACCOUNT BREAKDOWNS ===".to_string());
        lines.push(csv_row(&[
            "Bank Name",
            "Account No",
            "Opening Balance",
            "Closing Balance",
            "Transactions",
        ]));
        for b in &breakdowns {
            lines.push(csv_row(&[
                &b.bank_name,
                &b.account_no,
                &fmt_amt(b.opening_bal),
                &fmt_amt(b.closing_bal),
                &b.txn_count.to_string(),
            ]));
        }
    }

    // ── Sheet: Receipt Heads ──────────────────────────────────────────────────
    lines.push(String::new());
    lines.push("=== RECEIPT HEADS ===".to_string());
    lines.push(csv_row(&["Account Head", "Amount"]));
    let mut rec_map: std::collections::BTreeMap<&str, f64> = std::collections::BTreeMap::new();
    for t in &real {
        if (t.txn_type.to_string() == "Receipt" || t.account_head.to_lowercase().contains("income"))
            && t.credit.is_some()
            && !t.account_head.is_empty()
        {
            *rec_map.entry(t.account_head.as_str()).or_default() += t.credit.unwrap_or(0.0);
        }
    }
    let mut rec_vec: Vec<(&&str, &f64)> = rec_map.iter().collect();
    rec_vec.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (head, amt) in rec_vec {
        lines.push(csv_row(&[head, &fmt_amt(Some(*amt))]));
    }

    // ── Sheet: Payment Heads ──────────────────────────────────────────────────
    lines.push(String::new());
    lines.push("=== PAYMENT HEADS ===".to_string());
    lines.push(csv_row(&["Account Head", "Amount"]));
    let mut pay_map: std::collections::BTreeMap<&str, f64> = std::collections::BTreeMap::new();
    for t in &real {
        if (t.txn_type.to_string() == "Payment"
            || t.account_head.to_lowercase().contains("expense"))
            && t.debit.is_some()
            && !t.account_head.is_empty()
        {
            *pay_map.entry(t.account_head.as_str()).or_default() += t.debit.unwrap_or(0.0);
        }
    }
    let mut pay_vec: Vec<(&&str, &f64)> = pay_map.iter().collect();
    pay_vec.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (head, amt) in pay_vec {
        lines.push(csv_row(&[head, &fmt_amt(Some(*amt))]));
    }

    // ── Write ─────────────────────────────────────────────────────────────────
    let content = lines.join("\r\n");
    let mut f = std::fs::File::create(path.as_ref())?;
    // Write BOM for Excel UTF-8 detection
    f.write_all(b"\xEF\xBB\xBF")?;
    f.write_all(content.as_bytes())?;

    Ok(real.len())
}

// ── Real XLSX export (rust_xlsxwriter) ───────────────────────────────────────

/// Export transactions to a genuine .xlsx workbook with 4 worksheets:
/// Transactions, Summary, Receipt Heads, Payment Heads.
pub fn export_xlsx(
    txns: &[Transaction],
    client_name: &str,
    bank_ledger: &str,
    file_name: &str,
    opening_bal: Option<f64>,
    closing_bal: Option<f64>,
    path: impl AsRef<Path>,
) -> anyhow::Result<usize> {
    use rust_xlsxwriter::Workbook;

    let real: Vec<&Transaction> = txns.iter().filter(|t| !t.is_opening_balance).collect();

    let total_dr: f64 = real.iter().filter_map(|t| t.debit).sum();
    let total_cr: f64 = real.iter().filter_map(|t| t.credit).sum();

    let mut workbook = Workbook::new();

    // ── Sheet 1: Transactions ─────────────────────────────────────────────────
    let ws = workbook.add_worksheet();
    ws.set_name("Transactions")?;
    let txn_headers = [
        "Date",
        "Narration",
        "Reference",
        "Debit",
        "Credit",
        "Balance",
        "Vendor",
        "Account Head",
        "Ledger for Posting",
        "Party Group",
        "Type",
        "Status",
        "Tags",
        "Confidence",
        "Classification Source",
        "Classification Reason",
        "Classified By",
        "Bank Name",
        "Account No",
    ];
    for (c, h) in txn_headers.iter().enumerate() {
        ws.write(0, c as u16, *h)?;
    }
    for (r, t) in real.iter().enumerate() {
        let row = (r + 1) as u32;
        let classified_by = if t.classification_source.is_empty() {
            "local"
        } else {
            t.classification_source.as_str()
        };
        ws.write(row, 0, t.date.as_str())?;
        ws.write(row, 1, t.narration.as_str())?;
        ws.write(row, 2, t.reference.as_str())?;
        if let Some(v) = t.debit {
            ws.write(row, 3, v)?;
        }
        if let Some(v) = t.credit {
            ws.write(row, 4, v)?;
        }
        if let Some(v) = t.balance {
            ws.write(row, 5, v)?;
        }
        ws.write(row, 6, t.vendor.as_str())?;
        ws.write(row, 7, t.account_head.as_str())?;
        ws.write(row, 8, posting_ledger(t))?;
        ws.write(row, 9, party_group(t))?;
        ws.write(row, 10, t.txn_type.to_string().as_str())?;
        ws.write(row, 11, t.status.to_string().as_str())?;
        ws.write(row, 12, t.tags.join("; ").as_str())?;
        ws.write(row, 13, t.confidence)?;
        ws.write(row, 14, t.classification_source.as_str())?;
        ws.write(row, 15, "")?;
        ws.write(row, 16, classified_by)?;
        ws.write(row, 17, t.bank_name.as_str())?;
        ws.write(row, 18, t.account_no.as_str())?;
    }

    // ── Sheet 2: Summary ──────────────────────────────────────────────────────
    let ws2 = workbook.add_worksheet();
    ws2.set_name("Summary")?;
    let calc_cl = opening_bal.map(|ob| (ob + total_cr - total_dr).round());
    let recon = match (calc_cl, closing_bal) {
        (Some(c), Some(s)) if (c - s).abs() < 0.5 => "RECONCILED",
        (Some(_), Some(_)) => "MISMATCH",
        _ => "N/A",
    };
    let dup_count = real.iter().filter(|t| t.dup_flag).count();
    let gst_count = real
        .iter()
        .filter(|t| t.tags.iter().any(|g| g == "GST"))
        .count();
    let summary: Vec<(&str, String)> = vec![
        ("Client", client_name.to_string()),
        ("Bank Ledger", bank_ledger.to_string()),
        ("Source File", file_name.to_string()),
        ("Total Credits", format!("{:.2}", total_cr)),
        ("Total Debits", format!("{:.2}", total_dr)),
        ("Net", format!("{:.2}", total_cr - total_dr)),
        (
            "Opening Balance",
            opening_bal.map_or(String::new(), |v| format!("{:.2}", v)),
        ),
        (
            "Closing Balance (Stated)",
            closing_bal.map_or(String::new(), |v| format!("{:.2}", v)),
        ),
        (
            "Closing Balance (Calc.)",
            calc_cl.map_or(String::new(), |v| format!("{:.2}", v)),
        ),
        ("Reconciliation", recon.to_string()),
        ("Duplicate Transactions", dup_count.to_string()),
        ("GST Transactions", gst_count.to_string()),
    ];
    for (r, (k, v)) in summary.iter().enumerate() {
        ws2.write(r as u32, 0, *k)?;
        ws2.write(r as u32, 1, v.as_str())?;
    }

    let breakdowns = account_breakdowns(&real);
    if breakdowns.len() > 1 {
        let mut row = (summary.len() + 1) as u32;
        ws2.write(row, 0, "Bank Account Breakdowns")?;
        row += 1;
        for (c, h) in [
            "Bank Name",
            "Account No",
            "Opening Balance",
            "Closing Balance",
            "Transactions",
        ]
        .iter()
        .enumerate()
        {
            ws2.write(row, c as u16, *h)?;
        }
        for b in &breakdowns {
            row += 1;
            ws2.write(row, 0, b.bank_name.as_str())?;
            ws2.write(row, 1, b.account_no.as_str())?;
            if let Some(v) = b.opening_bal {
                ws2.write(row, 2, v)?;
            }
            if let Some(v) = b.closing_bal {
                ws2.write(row, 3, v)?;
            }
            ws2.write(row, 4, b.txn_count as f64)?;
        }
    }

    // ── Sheet 3: Receipt Heads ────────────────────────────────────────────────
    let ws3 = workbook.add_worksheet();
    ws3.set_name("Receipt Heads")?;
    ws3.write(0, 0, "Account Head")?;
    ws3.write(0, 1, "Amount")?;
    let mut rec_map: std::collections::BTreeMap<&str, f64> = std::collections::BTreeMap::new();
    for t in &real {
        if t.credit.is_some()
            && !t.account_head.is_empty()
            && (t.txn_type.to_string() == "Receipt"
                || t.account_head.to_lowercase().contains("income"))
        {
            *rec_map.entry(t.account_head.as_str()).or_default() += t.credit.unwrap_or(0.0);
        }
    }
    let mut rec_vec: Vec<_> = rec_map.iter().collect();
    rec_vec.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (r, (head, amt)) in rec_vec.iter().enumerate() {
        ws3.write((r + 1) as u32, 0, **head)?;
        ws3.write((r + 1) as u32, 1, **amt)?;
    }

    // ── Sheet 4: Payment Heads ────────────────────────────────────────────────
    let ws4 = workbook.add_worksheet();
    ws4.set_name("Payment Heads")?;
    ws4.write(0, 0, "Account Head")?;
    ws4.write(0, 1, "Amount")?;
    let mut pay_map: std::collections::BTreeMap<&str, f64> = std::collections::BTreeMap::new();
    for t in &real {
        if t.debit.is_some()
            && !t.account_head.is_empty()
            && (t.txn_type.to_string() == "Payment"
                || t.account_head.to_lowercase().contains("expense"))
        {
            *pay_map.entry(t.account_head.as_str()).or_default() += t.debit.unwrap_or(0.0);
        }
    }
    let mut pay_vec: Vec<_> = pay_map.iter().collect();
    pay_vec.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
    for (r, (head, amt)) in pay_vec.iter().enumerate() {
        ws4.write((r + 1) as u32, 0, **head)?;
        ws4.write((r + 1) as u32, 1, **amt)?;
    }

    workbook.save(path.as_ref())?;
    Ok(real.len())
}
