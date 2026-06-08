// export/excel.rs — Export transactions to a CSV file (opens in Excel).
// Generates 4 sheets as separate sections in one CSV, or as a proper XLSX
// when the excel-export feature is enabled.
// Mirrors the original app.js _exportExcel() which uses SheetJS.

use crate::parser::Transaction;
use std::path::Path;

// ── INR formatter (no ₹ symbol for CSV cells) ────────────────────────────────

fn fmt_amt(v: Option<f64>) -> String {
    match v {
        None    => String::new(),
        Some(n) => {
            let (sign, abs) = if n < 0.0 { ("-", -n) } else { ("", n) };
            let int_part = abs as u64;
            let frac     = ((abs - int_part as f64) * 100.0).round() as u64;
            let s = int_part.to_string();
            let formatted = if s.len() <= 3 {
                s.clone()
            } else {
                let (first, rest) = s.split_at(s.len() - 3);
                let mut parts = vec![rest.to_string()];
                let mut rem   = first;
                while rem.len() > 2 {
                    let (r, chunk) = rem.split_at(rem.len() - 2);
                    parts.push(chunk.to_string());
                    rem = r;
                }
                if !rem.is_empty() { parts.push(rem.to_string()); }
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
    cells.iter().map(|c| csv_val(c)).collect::<Vec<_>>().join(",")
}

// ── Main export function ──────────────────────────────────────────────────────

/// Export transactions to a CSV file at `path`.
/// The file contains 4 sections separated by blank lines, mimicking multiple
/// Excel sheets: Transactions, Summary, Receipt Heads, Payment Heads.
pub fn export_csv(
    txns:        &[Transaction],
    client_name: &str,
    bank_ledger: &str,
    file_name:   &str,
    opening_bal: Option<f64>,
    closing_bal: Option<f64>,
    path:        impl AsRef<Path>,
) -> anyhow::Result<usize> {
    use std::io::Write as _;

    let real: Vec<&Transaction> = txns.iter()
        .filter(|t| !t.is_opening_balance)
        .collect();

    let total_dr: f64 = real.iter().filter_map(|t| t.debit).sum();
    let total_cr: f64 = real.iter().filter_map(|t| t.credit).sum();

    let mut lines: Vec<String> = Vec::new();

    // ── Sheet: Transactions ───────────────────────────────────────────────────
    lines.push("=== TRANSACTIONS ===".to_string());
    lines.push(csv_row(&[
        "Date","Narration","Reference","Debit","Credit","Balance",
        "Vendor","Account Head","Type","Status","Tags","Confidence",
        "Bank Name","Account No"
    ]));
    for t in &real {
        lines.push(csv_row(&[
            &t.date,
            &t.narration,
            &t.reference,
            &fmt_amt(t.debit),
            &fmt_amt(t.credit),
            &fmt_amt(t.balance),
            &t.vendor,
            &t.account_head,
            &t.txn_type.to_string(),
            &t.status.to_string(),
            &t.tags.join("; "),
            &format!("{:.2}", t.confidence),
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
        ("Client",                 client_name.to_string()),
        ("Bank Ledger",            bank_ledger.to_string()),
        ("File",                   file_name.to_string()),
        ("Total Receipts (Credit)",fmt_amt(Some(total_cr))),
        ("Total Payments (Debit)", fmt_amt(Some(total_dr))),
        ("Net",                    fmt_amt(Some(total_cr - total_dr))),
        ("Opening Balance",        fmt_amt(opening_bal)),
        ("Closing Balance (Stated)", fmt_amt(stated_cl)),
        ("Closing Balance (Calc.)", fmt_amt(calc_cl)),
        ("Reconciliation",         recon),
        ("Duplicate Transactions", real.iter().filter(|t| t.dup_flag).count().to_string()),
        ("GST Transactions",       real.iter().filter(|t| t.tags.iter().any(|g| g == "GST")).count().to_string()),
    ];
    for row in &summary_rows {
        lines.push(csv_row(&[row.0, row.1.as_str()]));
    }

    // ── Sheet: Receipt Heads ──────────────────────────────────────────────────
    lines.push(String::new());
    lines.push("=== RECEIPT HEADS ===".to_string());
    lines.push(csv_row(&["Account Head","Amount"]));
    let mut rec_map: std::collections::BTreeMap<&str, f64> = std::collections::BTreeMap::new();
    for t in &real {
        if (t.txn_type.to_string() == "Receipt" || t.account_head.to_lowercase().contains("income"))
            && t.credit.is_some() && !t.account_head.is_empty()
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
    lines.push(csv_row(&["Account Head","Amount"]));
    let mut pay_map: std::collections::BTreeMap<&str, f64> = std::collections::BTreeMap::new();
    for t in &real {
        if (t.txn_type.to_string() == "Payment" || t.account_head.to_lowercase().contains("expense"))
            && t.debit.is_some() && !t.account_head.is_empty()
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
