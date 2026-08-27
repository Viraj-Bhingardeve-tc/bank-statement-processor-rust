// examples/dump_text.rs — dump raw extract_full_text output for a fixture
use bank_statement_processor::parser::text_extractor;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = args.get(1).map(|s| s.as_str()).unwrap_or("BOB.pdf");
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bank_statements")
        .join(name);
    let ft = text_extractor::extract_full_text(&path);
    println!("=== full_text ({} chars) ===", ft.len());
    for (i, line) in ft.lines().enumerate().take(80) {
        println!("[{:03}] {:?}", i, line);
    }
}
