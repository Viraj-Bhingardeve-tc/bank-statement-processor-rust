// build.rs — Compile Slint UI files into Rust code.
//
// Guarded by the "slint-ui" feature so that
// `cargo test --lib --no-default-features` skips Slint compilation and
// the heavy slint-build crate is not pulled in.

fn main() {
    // The cfg() check is evaluated at compile time by rustc, so slint_build
    // is only referenced (and linked) when the feature is actually active.
    #[cfg(feature = "slint-ui")]
    slint_build::compile("ui/app.slint")
        .expect("Slint UI compilation failed — check ui/*.slint for syntax errors");
}
