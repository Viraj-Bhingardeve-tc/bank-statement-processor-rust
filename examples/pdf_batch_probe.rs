// examples/pdf_batch_probe.rs — one-shot diagnostic across all PDF fixtures,
// used to finalize tests/import_pipeline.rs's known-good/known-bad split.
// Run: cargo run --example pdf_batch_probe

use bank_statement_processor::parser::{ocr_parser, pdf_parser, text_extractor};

const ALL: &[&str] = &[
    "Bank of Maharashtra.pdf",
    "BOB.pdf",
    "Cosmos Co-operative.pdf",
    "ICICI Bank Wealth management.pdf",
    "ICICI Bank.pdf",
    "IDBI Bank.pdf",
    "IDFCFIRSTBankstatement.pdf",
    "Kotak Bank.pdf",
    "Mahanager Co-operative bank.pdf",
    "SBI.pdf",
    "Union Bank.pdf",
];

fn main() {
    for name in ALL {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/bank_statements")
            .join(name);
        let rows = match text_extractor::extract_pages(&path) {
            Ok(r) => r,
            Err(e) => {
                println!("{name}: extract_pages ERROR: {e:#}");
                continue;
            }
        };
        if rows.is_empty() {
            println!("{name}: STAGE1 zero rows -> checking full_text");
            let ft = text_extractor::extract_full_text(&path);
            println!(
                "  full_text len={}, id-h={}",
                ft.len(),
                ft.contains("Identity-H Unimplemented")
            );
            continue;
        }
        match pdf_parser::parse_pdf_rows(rows, name) {
            Some(r) => {
                let n = r
                    .transactions
                    .iter()
                    .filter(|t| !t.is_opening_balance)
                    .count();
                println!("{name}: STAGE1 OK - {n} txns, bank={}", r.bank_name);
            }
            None => {
                let ft = text_extractor::extract_full_text(&path);
                let idh = ft.contains("Identity-H Unimplemented");
                let ocr = ocr_parser::parse_ocr_text(&ft, name);
                let n1 = ocr
                    .transactions
                    .iter()
                    .filter(|t| !t.is_opening_balance)
                    .count();
                let pre = ocr_parser::preprocess_multiline(&ft);
                let ml = ocr_parser::parse_ocr_text(&pre, name);
                let n2 = ml
                    .transactions
                    .iter()
                    .filter(|t| !t.is_opening_balance)
                    .count();
                println!(
                    "{name}: STAGE1 None -> full_text len={}, id-h={}, stage2={n1}, stage2ml={n2}",
                    ft.len(),
                    idh
                );
            }
        }
    }
}
