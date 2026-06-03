fn main() {
    // ── libxml2 ─────────────────────────────────────────────────────────
    // Try pkg-config first; it knows the right -L path on Homebrew / system SDK.
    if pkg_config::probe_library("libxml-2.0").is_err() {
        // Fallback: macOS ships libxml2 as part of the system frameworks.
        println!("cargo:rustc-link-lib=xml2");
    }


    // ── pugixml ────────────────────────────────────────────────────────
    // pugixml is C++-only; we wrap it in a tiny shim with `extern "C"`.
    // pkg-config emits `cargo:rustc-link-lib=pugixml` and the right -L,
    // and gives us the include path we need to compile the shim against.
    let pugi = pkg_config::probe_library("pugixml")
        .expect("pugixml not found via pkg-config — install with `brew install pugixml`");

    let mut shim = cc::Build::new();
    shim.cpp(true).std("c++17").file("pugixml_shim.cc");
    for inc in &pugi.include_paths {
        shim.include(inc);
    }
    shim.compile("pugixml_shim");

    println!("cargo:rerun-if-changed=pugixml_shim.cc");

    // ── expat ──────────────────────────────────────────────────────────
    // expat is a SAX parser, no DOM build.  We use it as the
    // "parser-only ceiling" reference in head-to-heads against our
    // DOM-building paths.
    let expat = pkg_config::probe_library("expat")
        .expect("expat not found via pkg-config — install with `brew install expat`");
    let mut expat_shim = cc::Build::new();
    expat_shim.file("expat_shim.c");
    for inc in &expat.include_paths {
        expat_shim.include(inc);
    }
    expat_shim.compile("expat_shim");

    println!("cargo:rerun-if-changed=expat_shim.c");
    println!("cargo:rerun-if-changed=build.rs");
}
