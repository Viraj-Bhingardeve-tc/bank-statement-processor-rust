// examples/pdf_debug_probe.rs — reproduces the Identity-H CID font bug
// documented in tests/import_pipeline.rs's
// `bob_pdf_exposes_an_unhandled_identity_h_cid_font_bug` test.
// Run: cargo run --example pdf_debug_probe -- "BOB.pdf"

fn main() {
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "BOB.pdf".to_string());
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bank_statements")
        .join(&name);
    let text = bank_statement_processor::parser::text_extractor::extract_full_text(&path);
    println!("=== {name}: full_text len: {} ===", text.len());
    println!("{}", text.chars().take(800).collect::<String>());
}
