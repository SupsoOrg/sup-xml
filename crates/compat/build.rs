//! Pin the cdylib's public ABI to the exact list of libxml2 functions
//! we implement.  Without this, every Rust `pub extern "C" fn` would
//! become a public symbol, plus Rust-stdlib helpers and internal
//! exports would leak out.
//!
//! Two files drive the policy (kept in sync by hand for now):
//!   - `src/symbols.txt` — macOS, `-exported_symbols_list` format
//!     (one underscore-prefixed name per line)
//!   - `src/symbols.ld`  — Linux, version-script format
//!
//! Sister test `c-tests/t-link-03.c` confirms that *unimplemented*
//! libxml2 symbols (xmlXPathEval, xmlSchemaParse, etc.) are absent
//! from the cdylib — the version script is what makes that the case.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_CDYLIB_EXPORTS");

    // The symbol-pinning linker scripts only make sense when our
    // `extern "C"` items are actually being exported under their
    // libxml2 names — i.e. when the `cdylib-exports` cargo feature
    // is on.  With the feature off (the default for rlib/inspection
    // builds), Rust mangles the names; the linker would fail on
    // `_xmlXPatherror` etc. trying to enforce a symbol list whose
    // entries don't exist as exports.
    if std::env::var_os("CARGO_FEATURE_CDYLIB_EXPORTS").is_none() {
        return;
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let manifest  = std::env::var("CARGO_MANIFEST_DIR").unwrap();

    match target_os.as_str() {
        "macos" | "ios" => {
            let path = format!("{manifest}/src/symbols.txt");
            // Pass through to `ld` for the cdylib artifact only.
            // `-exported_symbols_list` hides everything not listed.
            println!(
                "cargo:rustc-cdylib-link-arg=-Wl,-exported_symbols_list,{path}"
            );
            println!("cargo:rerun-if-changed={path}");
        }
        "linux" | "android" | "freebsd" | "netbsd" | "openbsd" => {
            let path = format!("{manifest}/src/symbols.ld");
            println!(
                "cargo:rustc-cdylib-link-arg=-Wl,--version-script={path}"
            );
            println!("cargo:rerun-if-changed={path}");
        }
        _ => {
            // Windows / unknown: skip — version-script equivalents are
            // platform-specific and Tier 1 is Unix-only for v0.1.
        }
    }
}
