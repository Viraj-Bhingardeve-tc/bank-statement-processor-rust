// build.rs — Compile Slint UI files into Rust code, and copy the OpenSSL
// DLLs that rusqlite's "bundled-sqlcipher" feature links against next to
// every binary that needs them at runtime.
//
// Slint compilation is guarded by the "slint-ui" feature so that
// `cargo test --lib --no-default-features` skips it and the heavy
// slint-build crate is not pulled in.

fn main() {
    // The cfg() check is evaluated at compile time by rustc, so slint_build
    // is only referenced (and linked) when the feature is actually active.
    #[cfg(feature = "slint-ui")]
    slint_build::compile("ui/app.slint")
        .expect("Slint UI compilation failed — check ui/*.slint for syntax errors");

    copy_sqlcipher_runtime_dlls();
}

/// rusqlite's "bundled-sqlcipher" feature (see Cargo.toml) links SQLCipher's
/// crypto calls dynamically against libcrypto-3-x64.dll, found at build time
/// via OPENSSL_DIR (set in .cargo/config.toml). That DLL is NOT statically
/// linked in — every binary this crate produces (the app .exe, and every
/// `cargo test` test binary) needs it discoverable at runtime via Windows'
/// standard DLL search order, which checks the binary's own directory first.
///
/// Without this, `cargo clean` (which wipes target/) or a fresh checkout on
/// a machine where the DLL doesn't happen to already be on PATH reproduces
/// exactly the failure this function exists to prevent: the process refuses
/// to start at all with STATUS_DLL_NOT_FOUND (0xC0000135), before any of
/// this application's code — including a hypothetical in-app diagnostic —
/// gets a chance to run. See src/db/encryption.rs's RUNTIME DEPENDENCY
/// comment for the full empirical writeup of that failure mode.
fn copy_sqlcipher_runtime_dlls() {
    if !cfg!(windows) {
        return;
    }
    let Ok(openssl_dir) = std::env::var("OPENSSL_DIR") else {
        println!("cargo:warning=OPENSSL_DIR not set — cannot copy SQLCipher's runtime DLLs; the built binary may fail to start with STATUS_DLL_NOT_FOUND unless a compatible libcrypto-3-x64.dll is already on PATH");
        return;
    };
    let dll_src_dir = std::path::Path::new(&openssl_dir).join("bin");
    let dlls = ["libcrypto-3-x64.dll", "libssl-3-x64.dll"];

    // OUT_DIR is target/<profile>/build/<pkg>-<hash>/out — walk up to the
    // actual target/<profile> directory where binaries are placed, and also
    // copy into .../deps, where `cargo test` puts its test executables.
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set by cargo");
    let Some(profile_dir) = std::path::Path::new(&out_dir).ancestors().nth(3) else {
        println!("cargo:warning=could not locate target/<profile> from OUT_DIR={out_dir}; SQLCipher runtime DLLs not copied");
        return;
    };

    for dest_dir in [profile_dir.to_path_buf(), profile_dir.join("deps")] {
        if !dest_dir.is_dir() {
            continue;
        }
        for dll in dlls {
            let src = dll_src_dir.join(dll);
            let dest = dest_dir.join(dll);
            if !src.is_file() {
                println!("cargo:warning={} not found under OPENSSL_DIR/bin ({}) — SQLCipher runtime DLL not bundled", dll, dll_src_dir.display());
                continue;
            }
            // Skip the copy if an identical-size file is already there —
            // avoids rewriting a multi-MB file on every incremental build.
            if dest.is_file()
                && std::fs::metadata(&dest).ok().map(|m| m.len())
                    == std::fs::metadata(&src).ok().map(|m| m.len())
            {
                continue;
            }
            if let Err(e) = std::fs::copy(&src, &dest) {
                println!("cargo:warning=failed to copy {} to {}: {}", src.display(), dest.display(), e);
            }
        }
    }

    println!("cargo:rerun-if-env-changed=OPENSSL_DIR");
}
