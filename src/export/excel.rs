// export/excel.rs — Export transactions to CSV or real XLSX.

use crate::analytics;
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
// `pub` so the Main Screen transaction table (main.rs) can derive its
// "Ledger for Posting" column from the same rule instead of duplicating it.

pub fn posting_ledger(t: &Transaction) -> &str {
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
        "Expense Head",
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
        // Requirement #8: reuse the same Expense Head / basis derivation the
        // Main Screen and Dashboard already use (Requirements #2/#7) instead
        // of duplicating the logic or leaving the column blank.
        let expense_head = analytics::expense_head_label(t);
        let basis = analytics::classification_basis(t);
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
            &expense_head,
            party_group(t),
            &t.txn_type.to_string(),
            &t.status.to_string(),
            &t.tags.join("; "),
            &format!("{:.2}", t.confidence),
            &t.classification_source,
            &basis,
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
        "Expense Head",
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
        // Requirement #8: reuse the same Expense Head / basis derivation the
        // Main Screen and Dashboard already use (Requirements #2/#7) instead
        // of duplicating the logic or leaving the column blank.
        let expense_head = analytics::expense_head_label(t);
        let basis = analytics::classification_basis(t);
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
        ws.write(row, 9, expense_head.as_str())?;
        ws.write(row, 10, party_group(t))?;
        ws.write(row, 11, t.txn_type.to_string().as_str())?;
        ws.write(row, 12, t.status.to_string().as_str())?;
        ws.write(row, 13, t.tags.join("; ").as_str())?;
        ws.write(row, 14, t.confidence)?;
        ws.write(row, 15, t.classification_source.as_str())?;
        ws.write(row, 16, basis.as_str())?;
        ws.write(row, 17, classified_by)?;
        ws.write(row, 18, t.bank_name.as_str())?;
        ws.write(row, 19, t.account_no.as_str())?;
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

// ── Tests (Requirement #8: does Export to Excel actually work end-to-end?) ────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{TransactionStatus, VoucherType};
    use calamine::{open_workbook_auto, Data, DataType, Reader};
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique path per test/run in the OS temp dir — avoids collisions
    /// between parallel test threads without adding a `tempfile` dependency.
    fn temp_path(name: &str, ext: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "bsp_export_test_{}_{}_{}.{}",
            std::process::id(),
            name,
            n,
            ext
        ))
    }

    /// Three real-shaped transactions: a keyword-classified expense (has an
    /// account head), a bare party payment (vendor only, no account head —
    /// must resolve to Sundry Creditors per Requirement #2), and a rule-
    /// matched receipt — plus one opening-balance row that must never appear
    /// in the exported "Transactions" sheet.
    fn sample_txns() -> Vec<Transaction> {
        vec![
            Transaction {
                is_opening_balance: true,
                balance: Some(10_000.0),
                ..Transaction::new("ob")
            },
            Transaction {
                date: "05/04/2024".to_string(),
                narration: "AIRTEL POSTPAID BILL".to_string(),
                reference: "REF001".to_string(),
                debit: Some(999.0),
                account_head: "Telephone Expense".to_string(),
                vendor: "Airtel".to_string(),
                status: TransactionStatus::Classified,
                confidence: 0.45,
                classification_source: "keyword".to_string(),
                txn_type: VoucherType::Payment,
                bank_name: "HDFC Bank".to_string(),
                account_no: "1234XXXXXX5678".to_string(),
                ..Transaction::new("t1")
            },
            Transaction {
                date: "10/04/2024".to_string(),
                narration: "NEFT/AB123/ABC TRADERS".to_string(),
                reference: "AB123".to_string(),
                debit: Some(2_500.0),
                vendor: "ABC Traders".to_string(),
                status: TransactionStatus::Unreviewed,
                confidence: 0.0,
                txn_type: VoucherType::Payment,
                bank_name: "HDFC Bank".to_string(),
                account_no: "1234XXXXXX5678".to_string(),
                ..Transaction::new("t2")
            },
            Transaction {
                date: "15/04/2024".to_string(),
                narration: "SALARY CREDIT".to_string(),
                credit: Some(50_000.0),
                account_head: "Salaries".to_string(),
                vendor: "Employer".to_string(),
                status: TransactionStatus::Classified,
                confidence: 0.9,
                classification_source: "rule".to_string(),
                txn_type: VoucherType::Receipt,
                bank_name: "HDFC Bank".to_string(),
                account_no: "1234XXXXXX5678".to_string(),
                ..Transaction::new("t3")
            },
        ]
    }

    #[test]
    fn xlsx_file_is_actually_created_and_opens_successfully() {
        let path = temp_path("basic", "xlsx");
        let n = export_xlsx(&sample_txns(), "Test Client", "HDFC Bank", "stmt.xlsx", Some(10_000.0), Some(56_501.0), &path)
            .expect("export_xlsx must succeed");
        assert_eq!(n, 3, "opening-balance row must not be counted");
        assert!(path.exists(), "the .xlsx file must actually be written to disk");

        // Round-trip: does a real spreadsheet reader open it without error?
        let workbook = open_workbook_auto(&path);
        assert!(
            workbook.is_ok(),
            "a real xlsx reader must be able to open the exported file: {:?}",
            workbook.err()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn xlsx_transactions_sheet_has_expected_headers_and_no_lost_rows() {
        let path = temp_path("headers", "xlsx");
        export_xlsx(&sample_txns(), "Test Client", "HDFC Bank", "stmt.xlsx", None, None, &path).unwrap();

        let mut wb = open_workbook_auto(&path).unwrap();
        let range = wb.worksheet_range("Transactions").expect("Transactions sheet must exist");
        let rows: Vec<Vec<Data>> = range.rows().map(|r| r.to_vec()).collect();

        // Header row + 3 data rows (opening balance excluded) — no silent row loss.
        assert_eq!(rows.len(), 4, "expected 1 header row + 3 transaction rows");

        let headers: Vec<String> = rows[0].iter().map(|c| c.to_string()).collect();
        assert_eq!(
            headers,
            vec![
                "Date", "Narration", "Reference", "Debit", "Credit", "Balance", "Vendor",
                "Account Head", "Ledger for Posting", "Expense Head", "Party Group", "Type",
                "Status", "Tags", "Confidence", "Classification Source", "Classification Reason",
                "Classified By", "Bank Name", "Account No",
            ]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn xlsx_vendor_ledger_and_expense_head_match_the_main_screen_derivation() {
        let path = temp_path("derivation", "xlsx");
        export_xlsx(&sample_txns(), "Test Client", "HDFC Bank", "stmt.xlsx", None, None, &path).unwrap();

        let mut wb = open_workbook_auto(&path).unwrap();
        let range = wb.worksheet_range("Transactions").unwrap();
        let rows: Vec<Vec<Data>> = range.rows().map(|r| r.to_vec()).collect();

        // Row 1 (data row for t1): keyword-classified expense. Ledger for
        // Posting is the specific account head; Expense Head is the broader
        // Tally *group* that account head resolves into (tally_group_engine)
        // — same distinction Requirements #1/#2 established on the Main Screen.
        let r1 = &rows[1];
        assert_eq!(r1[6].to_string(), "Airtel", "Vendor column");
        assert_eq!(r1[8].to_string(), "Telephone Expense", "Ledger for Posting (has an account head)");
        assert_eq!(r1[9].to_string(), "Indirect Expenses", "Expense Head is the Tally group for Telephone Expense");

        // Row 2 (data row for t2): bare party payment, no account head —
        // must resolve to Sundry Creditors (Requirement #2), not be blank.
        let r2 = &rows[2];
        assert_eq!(r2[6].to_string(), "ABC Traders", "Vendor column");
        assert_eq!(r2[8].to_string(), "ABC Traders", "Ledger for Posting falls back to the vendor name");
        assert_eq!(
            r2[9].to_string(),
            "Sundry Creditors",
            "Expense Head for a bare party debit must be Sundry Creditors, not blank"
        );

        // Row 3 (data row for t3): rule-matched receipt.
        let r3 = &rows[3];
        assert_eq!(r3[8].to_string(), "Salaries", "Ledger for Posting");
        assert_eq!(r3[9].to_string(), "Indirect Expenses", "Expense Head resolves via tally_group_engine for a real account head");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn xlsx_debit_credit_balance_and_dates_are_correct() {
        let path = temp_path("amounts", "xlsx");
        export_xlsx(&sample_txns(), "Test Client", "HDFC Bank", "stmt.xlsx", None, None, &path).unwrap();

        let mut wb = open_workbook_auto(&path).unwrap();
        let range = wb.worksheet_range("Transactions").unwrap();
        let rows: Vec<Vec<Data>> = range.rows().map(|r| r.to_vec()).collect();

        let r1 = &rows[1];
        assert_eq!(r1[0].to_string(), "05/04/2024", "Date");
        assert_eq!(r1[3].as_f64(), Some(999.0), "Debit");
        assert_eq!(r1[4], Data::Empty, "Credit must be empty, not zero, for a debit-only row");

        let r3 = &rows[3];
        assert_eq!(r3[4].as_f64(), Some(50_000.0), "Credit");
        assert_eq!(r3[3], Data::Empty, "Debit must be empty, not zero, for a credit-only row");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn xlsx_classification_reason_column_is_populated_from_the_real_basis() {
        // Requirement #7 integration: the "Classification Reason" column
        // existed as a header before this fix but was always written blank
        // — now it carries analytics::classification_basis(t), not fabricated
        // text and not silently empty.
        let path = temp_path("basis", "xlsx");
        export_xlsx(&sample_txns(), "Test Client", "HDFC Bank", "stmt.xlsx", None, None, &path).unwrap();

        let mut wb = open_workbook_auto(&path).unwrap();
        let range = wb.worksheet_range("Transactions").unwrap();
        let rows: Vec<Vec<Data>> = range.rows().map(|r| r.to_vec()).collect();

        assert_eq!(rows[1][16].to_string(), "Matched a built-in keyword pattern");
        assert_eq!(
            rows[2][16].to_string(),
            "Not yet classified — no matching rule or keyword found"
        );
        assert_eq!(rows[3][16].to_string(), "Matched a saved classification rule");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn csv_export_also_creates_a_file_and_includes_expense_head_and_basis() {
        let path = temp_path("basic", "csv");
        let n = export_csv(&sample_txns(), "Test Client", "HDFC Bank", "stmt.xlsx", Some(10_000.0), Some(56_501.0), &path)
            .expect("export_csv must succeed");
        assert_eq!(n, 3);
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).expect("must be readable as UTF-8 text");
        assert!(
            content.contains("Expense Head"),
            "header row must include the new Expense Head column"
        );
        assert!(
            content.contains("Sundry Creditors"),
            "the bare party payment must show Sundry Creditors as its Expense Head, not be blank"
        );
        assert!(
            content.contains("Matched a saved classification rule"),
            "Classification Reason must carry the real basis text, not stay blank"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_never_panics_and_never_loses_rows_on_an_empty_transaction_set() {
        let path = temp_path("empty", "xlsx");
        let n = export_xlsx(&[], "Test Client", "HDFC Bank", "stmt.xlsx", None, None, &path)
            .expect("exporting zero transactions must not error");
        assert_eq!(n, 0);
        assert!(path.exists(), "an (empty) workbook must still be written");
        let _ = std::fs::remove_file(&path);
    }
}
