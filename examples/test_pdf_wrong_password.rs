// examples/test_pdf_wrong_password.rs — verify the wrong-password error path
// against a real encrypted PDF, without needing to know its real password.
// Confirms the error text contains "incorrect" (lowercased), which is what
// main.rs's on_do_pdf_pwd_confirm handler matches on to decide "keep the
// modal open for a retry" vs. "hard failure, close the modal".
// Run: cargo run --example test_pdf_wrong_password --no-default-features -- "<path>"

use bank_statement_processor::parser::text_extractor;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: test_pdf_wrong_password <path>");
    let path = std::path::Path::new(&path);

    let wrong_pwd = b"definitely-not-the-real-password-12345";

    match text_extractor::extract_pages_with_password(path, wrong_pwd) {
        Ok(rows) => {
            println!(
                "UNEXPECTED: wrong password was accepted, got {} rows",
                rows.len()
            );
        }
        Err(e) => {
            let emsg = e.to_string();
            println!("Error message: {emsg:?}");
            let matches_app_check = emsg.to_lowercase().contains("incorrect");
            println!(
                "Contains 'incorrect' (what main.rs's handler checks for retry-vs-fail): {}",
                matches_app_check
            );
        }
    }

    let full_text = text_extractor::extract_full_text_with_password(path, wrong_pwd);
    println!(
        "extract_full_text_with_password with wrong password: {} chars (should be 0/empty)",
        full_text.len()
    );
}
