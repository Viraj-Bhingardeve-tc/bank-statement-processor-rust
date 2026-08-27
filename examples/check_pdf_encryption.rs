// examples/check_pdf_encryption.rs — report whether a PDF is encrypted,
// without needing the password. Temporary diagnostic for the
// password-protected-PDF import feature.
// Run: cargo run --example check_pdf_encryption -- "<path>"

fn main() {
    let path = std::env::args().nth(1).expect("usage: check_pdf_encryption <path>");
    let doc = lopdf::Document::load(&path);
    match doc {
        Ok(d) => {
            println!("Loaded OK. is_encrypted() = {}", d.is_encrypted());
            println!("pages (may be 0 if encrypted/unreadable): {}", d.get_pages().len());
        }
        Err(e) => {
            println!("Load failed: {e}");
        }
    }
}
