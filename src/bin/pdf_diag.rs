// pdf_diag.rs — test OCR pipeline on real PDFs
// Run: cargo run --bin pdf_diag --no-default-features

fn main() {
    let assets = r"C:\Users\ADMIN\Desktop\myproject\bank-statement-processing\assets";

    let test_files = [
        "Bank of Maharashtra.pdf",
        "SBI.pdf",
        "IDBI Bank.pdf",
        "Mahanager Co-operative bank.pdf",
    ];

    for name in &test_files {
        let path = std::path::PathBuf::from(assets).join(name);
        if !path.exists() {
            continue;
        }

        println!("\n{}", "=".repeat(60));
        println!("FILE: {}", name);
        println!("{}", "=".repeat(60));

        let full_text = bank_statement_processor::parser::text_extractor::extract_full_text(&path);
        println!("  Raw text: {} chars", full_text.len());

        if full_text.trim().is_empty() || full_text.contains("?Identity-H Unimplemented?") {
            println!("  SKIP: embedded/unreadable font");
            continue;
        }

        // Stage 2a: direct OCR parse
        let ocr = bank_statement_processor::parser::ocr_parser::parse_ocr_text(&full_text, name);
        let real_a: Vec<_> = ocr
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        println!("  Stage-2a (ocr_text):  {} transactions", real_a.len());

        // Stage 2b: multiline preprocess
        let pre = bank_statement_processor::parser::ocr_parser::preprocess_multiline(&full_text);
        println!("  Stage-2b preprocessed: {} lines", pre.lines().count());
        for (i, l) in pre.lines().take(5).enumerate() {
            println!("    [{:02}] {}", i, l);
        }
        let ml = bank_statement_processor::parser::ocr_parser::parse_ocr_text(&pre, name);
        let real_b: Vec<_> = ml
            .transactions
            .iter()
            .filter(|t| !t.is_opening_balance)
            .collect();
        println!("  Stage-2b (multiline): {} transactions", real_b.len());

        // Show first 3 parsed transactions
        for (i, t) in real_b.iter().take(3).enumerate() {
            println!(
                "  T{}: date={} narr='{}' dr={:?} cr={:?} bal={:?}",
                i,
                t.date,
                bank_statement_processor::text_safety::safe_prefix(&t.narration, 40),
                t.debit,
                t.credit,
                t.balance
            );
        }
        if real_a.is_empty() && real_b.is_empty() {
            println!("  *** NOTHING PARSED — first 10 raw lines:");
            for (i, l) in full_text.lines().take(10).enumerate() {
                println!("    [{}] {}", i, l.trim());
            }
        }
    }
}
