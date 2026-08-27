// examples/dump_txns.rs — dump actual parsed Transactions (via the real
// two-stage pipeline) for a fixture, to spot-check Debit/Credit mapping,
// narration/reference separation, and balance-vs-amount correctness.
// Run: cargo run --example dump_txns -- "BOB.pdf" [n]

use bank_statement_processor::parser::{ocr_parser, pdf_parser, text_extractor, ParseResult};

fn parse(path: &std::path::Path, name: &str) -> Option<ParseResult> {
    let rows = text_extractor::extract_pages(path).ok()?;
    if !rows.is_empty() {
        if let Some(r) = pdf_parser::parse_pdf_rows(rows, name) {
            return Some(r);
        }
    }
    let ft = text_extractor::extract_full_text(path);
    if ft.trim().is_empty() {
        return None;
    }
    let ocr = ocr_parser::parse_ocr_text(&ft, name);
    if ocr.transactions.iter().any(|t| !t.is_opening_balance) {
        return Some(ocr);
    }
    let pre = ocr_parser::preprocess_multiline(&ft);
    let ml = ocr_parser::parse_ocr_text(&pre, name);
    Some(ml)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = args.get(1).map(|s| s.as_str()).unwrap_or("BOB.pdf");
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(20);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bank_statements")
        .join(name);

    match parse(&path, name) {
        None => println!("{name}: no result"),
        Some(r) => {
            println!(
                "bank={} opening_balance={:?} total_txns={}",
                r.bank_name,
                r.opening_balance,
                r.transactions.len()
            );
            let real: Vec<_> = r.transactions.iter().filter(|t| !t.is_opening_balance).collect();
            println!("real_txns={}", real.len());
            for t in real.iter().take(n) {
                println!(
                    "{} | D={:>12} C={:>12} Bal={:>14} | ref={:?} | {}",
                    t.date,
                    t.debit.map(|v| format!("{v:.2}")).unwrap_or_default(),
                    t.credit.map(|v| format!("{v:.2}")).unwrap_or_default(),
                    t.balance.map(|v| format!("{v:.2}")).unwrap_or_default(),
                    t.reference,
                    t.narration
                );
            }
            if real.len() > n {
                println!("... ({} more)", real.len() - n);
                println!("--- last {n} ---");
                for t in real.iter().skip(real.len().saturating_sub(n)) {
                    println!(
                        "{} | D={:>12} C={:>12} Bal={:>14} | ref={:?} | {}",
                        t.date,
                        t.debit.map(|v| format!("{v:.2}")).unwrap_or_default(),
                        t.credit.map(|v| format!("{v:.2}")).unwrap_or_default(),
                        t.balance.map(|v| format!("{v:.2}")).unwrap_or_default(),
                        t.reference,
                        t.narration
                    );
                }
            }
            // Balance reconciliation check — uses each txn's own `prev_balance`
            // (stamped by compute_prev_balances in the file's true
            // chronological order, which may be reversed from display order),
            // not a naive walk over display order.
            let mut mism = 0usize;
            let mut checked = 0usize;
            for t in &real {
                if let (Some(pb), Some(bal)) = (t.prev_balance, t.balance) {
                    checked += 1;
                    let expected = pb + t.credit.unwrap_or(0.0) - t.debit.unwrap_or(0.0);
                    if (expected - bal).abs() > 0.01 {
                        mism += 1;
                    }
                }
            }
            println!("reconciliation: {mism} mismatches / {checked} checked");
            let both = real.iter().filter(|t| t.debit.is_some() && t.credit.is_some()).count();
            let neither = real.iter().filter(|t| t.debit.is_none() && t.credit.is_none()).count();
            println!("both D&C set: {both}, neither set: {neither}");
        }
    }
}
