// lib.rs — Public library surface for the Bank Statement Processor.
//
// Exposes the parser (and future engine) modules so they can be unit-tested
// with `cargo test --lib --no-default-features` without compiling the Slint UI.

pub mod parser;
