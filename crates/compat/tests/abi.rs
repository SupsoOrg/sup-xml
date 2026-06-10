//! C ABI test harness for `libsup_xml_compat` (the cdylib that
//! eventually ships as `libsupxml2.so` / `libxml2.so.2`).
//!
//! Walks every `c-tests/*.c` file, compiles it against the workspace's
//! cdylib output, runs the resulting binary, and asserts:
//!
//!   - the binary's exit code is 0
//!   - its stdout matches `c-tests/expected/<name>.txt` (whitespace-
//!     trimmed equality)
//!
//! Each `.c` file becomes one `cargo test` test case — `cargo test
//! --test abi` lists them all by name.  See `c-tests/README.md` for
//! how to add new tests.
//!
//! ## Platform support
//!
//! macOS and Linux only.  Windows isn't in scope for the v0.1 ABI shim.

// The harness shells out to a Unix `cc` with GNU linker flags, links the
// cdylib by its `lib*.so` / `.dylib` name, and `#include`s real libxml2
// headers — none of which apply on an MSVC Windows runner.  Compile the
// whole harness out there rather than fail looking for a `.so`/`.dylib`.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Force a fresh build of the `sup-xml-compat` cdylib before any
/// C test compiles against it.  `cargo test --test abi` only builds
/// the test binary (which links the rlib); it does NOT always rebuild
/// the separate cdylib target.  Without this, stale cdylib symbols
/// cause spurious link failures when new ABI functions are added.
///
/// Runs at most once per test process; cargo's own dependency tracking
/// makes the call a no-op when the cdylib is already current.
fn ensure_cdylib_fresh() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // `cdylib-exports` is opt-in (off by default) so Rust callers
        // pulling compat into their dep graph don't shadow the real
        // libxml2's symbols.  The C-ABI tests are the one consumer
        // that *want* those exports — turn them on explicitly here.
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "sup-xml-compat", "--lib",
                   "--features", "cdylib-exports"])
            .status()
            .expect("invoke cargo build");
        assert!(status.success(), "cargo build of sup-xml-compat failed");
    });
}

// ── helpers ───────────────────────────────────────────────────────────────

fn workspace_root() -> PathBuf {
    // crates/compat/ → workspace root is two up
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_owned()
}

fn cdylib_dir() -> PathBuf {
    // Tests run under `cargo test`, which builds to `target/<profile>/`.
    // `cargo test` runs the test binary at `target/debug/deps/...`; the
    // cdylib lives at `target/debug/libsup_xml_compat.{dylib,so}`.
    //
    // `OUT_DIR` is build-script-only.  At test time we resolve the path
    // from the test binary's location via `std::env::current_exe`.
    let exe = std::env::current_exe().expect("current_exe");
    // exe is .../target/debug/deps/abi-<hash>
    let deps_dir = exe.parent().expect("exe has parent (deps)");
    let target_profile_dir = deps_dir.parent().expect("deps has parent (target/<profile>)");
    target_profile_dir.to_owned()
}

fn cdylib_path() -> PathBuf {
    let dir = cdylib_dir();
    let candidates = [
        "libsup_xml_compat.dylib",   // macOS
        "libsup_xml_compat.so",      // Linux
    ];
    for name in candidates {
        let p = dir.join(name);
        if p.exists() {
            return p;
        }
    }
    panic!(
        "cdylib not found in {} — expected one of {:?}.  \
         Did `cargo test` build the compat crate?",
        dir.display(), candidates,
    );
}

fn c_compiler() -> String {
    std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
}

/// Locate libxml2 headers so the `t-libxml2-headers` test can
/// `#include <libxml/parser.h>`.  Tries (in order):
///   1. `pkg-config --cflags libxml-2.0` (Homebrew / system pkg-config)
///   2. The macOS SDK's `usr/include` (Xcode bundles libxml2 there)
///   3. Common Linux/BSD paths
/// Returns the include directories to pass via `-I`.
fn libxml2_include_paths() -> Vec<PathBuf> {
    // 1. pkg-config.
    if let Ok(out) = Command::new("pkg-config")
        .args(["--cflags-only-I", "libxml-2.0"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            let mut paths = Vec::new();
            for tok in s.split_whitespace() {
                if let Some(p) = tok.strip_prefix("-I") {
                    if !p.is_empty() {
                        paths.push(PathBuf::from(p));
                    }
                }
            }
            if !paths.is_empty() {
                return paths;
            }
        }
    }
    // 2. macOS SDK.
    if cfg!(target_os = "macos") {
        if let Ok(out) = Command::new("xcrun").arg("--show-sdk-path").output() {
            if out.status.success() {
                let sdk = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let inc = PathBuf::from(format!("{sdk}/usr/include"));
                if inc.join("libxml/parser.h").exists() {
                    return vec![inc];
                }
            }
        }
    }
    // 3. Common Unix paths.
    for p in ["/usr/include/libxml2", "/usr/local/include/libxml2",
              "/opt/homebrew/include/libxml2"] {
        let pb = PathBuf::from(p);
        if pb.join("libxml/parser.h").exists() {
            return vec![pb];
        }
    }
    panic!(
        "libxml2 headers not found.  Install libxml2 development headers \
         (`brew install libxml2` on macOS — Xcode's SDK normally has them too)."
    );
}

fn c_tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("c-tests")
}

/// Build one C test against the cdylib.  Returns the output binary path.
fn build_c_test(c_path: &Path) -> PathBuf {
    let stem = c_path.file_stem().unwrap().to_str().unwrap();
    let cdylib = cdylib_path();
    let cdylib_dir = cdylib.parent().unwrap();
    let out_dir = cdylib_dir.join("abi-tests");
    std::fs::create_dir_all(&out_dir).expect("mkdir abi-tests");
    let out_bin = out_dir.join(format!("abi-{stem}"));

    // Compile: `cc <c-test>.c -L<cdylib-dir> -lsup_xml_compat -o <out>`.
    // On macOS we add `-Wl,-rpath,<cdylib-dir>` so the binary finds the
    // dylib at run time without needing DYLD_LIBRARY_PATH set (DYLD_*
    // env vars are stripped by macOS SIP for some processes).
    let mut cmd = Command::new(c_compiler());
    cmd.arg(c_path)
        .arg("-L").arg(cdylib_dir)
        .arg("-lsup_xml_compat")
        .arg("-o").arg(&out_bin);
    if cfg!(target_os = "macos") {
        cmd.arg(format!("-Wl,-rpath,{}", cdylib_dir.display()));
    } else {
        cmd.arg(format!("-Wl,-rpath,{}", cdylib_dir.display()));
    }
    // Tests that #include real libxml2 headers need the right -I path
    // (and ONLY those; pulling libxml2's headers into other tests would
    // risk shadowing our own declarations).  We do NOT link against
    // -lxml2 — the symbol resolutions must all hit `libsup_xml_compat`.
    //
    // Add a stem here when introducing a new test whose .c uses real
    // `<libxml/...>` headers.
    if matches!(stem, "t-libxml2-headers" | "t-upstream-layout") {
        for inc in libxml2_include_paths() {
            cmd.arg("-I").arg(inc);
        }
    }
    let output = cmd.output().expect("invoke cc");
    if !output.status.success() {
        panic!(
            "failed to compile {}:\n  stderr: {}\n  stdout: {}",
            c_path.display(),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout),
        );
    }
    out_bin
}

/// Run a c-test binary and return (stdout, exit_code).
fn run_c_test(bin: &Path) -> (String, i32) {
    let cdylib_dir = cdylib_path().parent().unwrap().to_owned();
    let mut cmd = Command::new(bin);
    // Belt-and-suspenders: also set the OS-specific library-path env
    // var so the dynamic loader can find the dylib even when rpath
    // doesn't apply (which can happen on some macOS configurations).
    if cfg!(target_os = "macos") {
        cmd.env("DYLD_LIBRARY_PATH", &cdylib_dir);
    } else {
        cmd.env("LD_LIBRARY_PATH", &cdylib_dir);
    }
    let output = cmd.output().expect("run c-test");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let code = output.status.code().unwrap_or(-1);
    (stdout, code)
}

fn run_one(c_path: &Path) {
    ensure_cdylib_fresh();
    let bin = build_c_test(c_path);
    let (stdout, exit_code) = run_c_test(&bin);

    let stem = c_path.file_stem().unwrap().to_str().unwrap();
    let expected_path = c_tests_dir().join("expected").join(format!("{stem}.txt"));
    let expected = std::fs::read_to_string(&expected_path)
        .unwrap_or_else(|e| panic!("missing expected file {}: {e}", expected_path.display()));

    let got_trim = stdout.trim();
    let exp_trim = expected.trim();
    assert_eq!(
        got_trim, exp_trim,
        "{} stdout mismatch\n  expected: {:?}\n  got:      {:?}",
        c_path.display(), exp_trim, got_trim,
    );
    assert_eq!(exit_code, 0, "{} exited with code {exit_code}", c_path.display());
}

// ── tests ─────────────────────────────────────────────────────────────────
//
// Each `t_*` function is a single cargo test that builds + runs one
// c-test file.  Adding a new test is a two-file change (add `.c` +
// `expected/.txt`) plus one entry here.  Eventually we'll auto-discover
// them via a build script or rstest, but keeping the list explicit
// for now makes the test surface easy to read in `cargo test`'s output.

#[test]
fn t_link_02() {
    let _ = workspace_root(); // currently unused; kept for future tests
    run_one(&c_tests_dir().join("t-link-02.c"));
}

/// T-ERR-01: parse malformed input, inspect xmlGetLastError, verify
/// xmlResetLastError clears it.
#[test]
fn t_err_01() {
    run_one(&c_tests_dir().join("t-err-01.c"));
}

/// T-ERR-02: xmlSetStructuredErrorFunc callback gets invoked with the
/// right domain/code/user_data; unregister works.
#[test]
fn t_err_02() {
    run_one(&c_tests_dir().join("t-err-02.c"));
}

/// T-ERR-03: C-side `_Static_assert` checks of xmlError byte layout.
/// Test passes iff every field is at the documented libxml2 offset.
#[test]
fn t_err_03() {
    run_one(&c_tests_dir().join("t-err-03.c"));
}

/// T-LAYOUT-03: C-side `_Static_assert` checks of xmlDoc byte layout.
/// Pairs with the const-offset assertions on `XmlDoc` in
/// `crates/tree/src/dom.rs` — both sides must agree.
#[test]
fn t_layout_03() {
    run_one(&c_tests_dir().join("t-layout-03.c"));
}

/// T-PARSE-01: end-to-end parse → root → content → free.  Smallest
/// interesting libxml2 surface.
#[test]
fn t_parse_01() {
    run_one(&c_tests_dir().join("t-parse-01.c"));
}

/// T-PARSE-06: malformed XML returns NULL, last-error queryable.
#[test]
fn t_parse_06() {
    run_one(&c_tests_dir().join("t-parse-06.c"));
}

/// T-WALK-02: xmlFirst/Last/Next/PreviousElementSibling skip non-elements.
#[test]
fn t_walk_02() {
    run_one(&c_tests_dir().join("t-walk-02.c"));
}

/// T-WALK-03: xmlChildElementCount matches a manual count, skipping
/// text/comment/PI siblings.
#[test]
fn t_walk_03() {
    run_one(&c_tests_dir().join("t-walk-03.c"));
}

/// T-WALK-04: xmlNodeGetContent flattens mixed content across nested
/// elements, ignoring comments and PIs.
#[test]
fn t_walk_04() {
    run_one(&c_tests_dir().join("t-walk-04.c"));
}

/// T-WALK-06: read xmlDoc properties (version/encoding/standalone/
/// children) via direct field access at the byte-exact offsets.
#[test]
fn t_walk_06() {
    run_one(&c_tests_dir().join("t-walk-06.c"));
}

/// T-ATTR-01: xmlGetProp + xmlHasProp basic semantics.
#[test]
fn t_attr_01() {
    run_one(&c_tests_dir().join("t-attr-01.c"));
}

/// T-ATTR-05: xmlGetNoNsProp ignores namespaced attributes; xmlGetNsProp
/// matches by namespace URI.
#[test]
fn t_attr_05() {
    run_one(&c_tests_dir().join("t-attr-05.c"));
}

/// T-MEM-02: xmlFree on an arena-resident pointer is a safe no-op.
/// Registry-based dual-pointer detection at work.
#[test]
fn t_mem_02() {
    run_one(&c_tests_dir().join("t-mem-02.c"));
}

/// T-LAYOUT-04: C-side `_Static_assert` checks of xmlNs byte layout.
#[test]
fn t_layout_04() {
    run_one(&c_tests_dir().join("t-layout-04.c"));
}

/// T-NS-01: default namespace from root in scope for descendants.
/// Reads child->ns->href via byte-exact field access.
#[test]
fn t_ns_01() {
    run_one(&c_tests_dir().join("t-ns-01.c"));
}

/// T-NS-03 + T-NS-04: xmlSearchNs by prefix and xmlSearchNsByHref by URI.
#[test]
fn t_ns_03() {
    run_one(&c_tests_dir().join("t-ns-03.c"));
}

/// T-SER-01: parse → dump → reparse → dump round-trip stability.
#[test]
fn t_ser_01() {
    run_one(&c_tests_dir().join("t-ser-01.c"));
}

/// T-SER-02: xmlDocDumpFormatMemory(format=1) produces indented output.
#[test]
fn t_ser_02() {
    run_one(&c_tests_dir().join("t-ser-02.c"));
}

/// T-PARSE-11: xmlInitParser is idempotent.  Multi-threaded coverage
/// is in the Rust unit test `init::tests::init_is_thread_safe`.
#[test]
fn t_parse_11() {
    run_one(&c_tests_dir().join("t-parse-11.c"));
}

/// T-LINK-01: dlopen the cdylib and dlsym every expected symbol.
/// The expected list lives in the C test source — updating Rust
/// without updating that list will cause this to fail loudly.
#[test]
fn t_link_01() {
    run_one(&c_tests_dir().join("t-link-01.c"));
}

/// T-LINK-03: un-implemented libxml2 symbols are ABSENT from the
/// cdylib.  Pinned by `src/symbols.{txt,ld}` + `build.rs`.
#[test]
fn t_link_03() {
    run_one(&c_tests_dir().join("t-link-03.c"));
}

/// A "real" libxml2 user — compiled against the actual libxml2
/// headers (`#include <libxml/parser.h>`) but linked against
/// `libsup_xml_compat`.  Validates byte-exact struct layouts and
/// function ABIs against the canonical headers.
#[test]
fn t_libxml2_headers() {
    run_one(&c_tests_dir().join("t-libxml2-headers.c"));
}

/// T-UPSTREAM-LAYOUT: `_Static_assert` byte-exact offsets / sizes /
/// discriminants against the *actual installed* libxml2 headers
/// (`<libxml/xmlerror.h>` for `xmlError`, `<libxml/tree.h>` for
/// `xmlElementType`).  Distinct from `t-err-03` / `t-layout-03` /
/// `t-layout-04` because those use local re-typedef'd structs, which
/// can only catch *internal* layout mismatches.  This test fails if
/// upstream libxml2 ever changes the public surface our compat
/// layer commits to.
#[test]
fn t_upstream_layout() {
    run_one(&c_tests_dir().join("t-upstream-layout.c"));
}

/// T-SAVE-01: round-trip a parsed doc through the xmlSave* streaming
/// serializer (xmlSaveToFilename → xmlSaveDoc → xmlSaveClose), read
/// the file back, verify it contains the expected elements.  Locks
/// in end-to-end C-ABI coverage for the xmlSave family we shipped.
#[test]
fn t_save_01() {
    run_one(&c_tests_dir().join("t-save-01.c"));
}

/// T-MEM-03: install caller-supplied allocator hooks via xmlMemSetup,
/// allocate + free through the `xmlMalloc` / `xmlFree` fn-pointer
/// globals, verify the hooks fired.  Locks in the allocator-override
/// contract from the C side.
#[test]
fn t_mem_03() {
    run_one(&c_tests_dir().join("t-mem-03.c"));
}

/// T-RECOVER-01: pass `XML_PARSE_RECOVER` to `xmlReadMemory` on
/// deliberately-malformed XML; verify a partial tree comes back
/// instead of NULL.  Regression gate for the `map_libxml2_options`
/// translator (the bit was silently dropped before this session).
#[test]
fn t_recover_01() {
    run_one(&c_tests_dir().join("t-recover-01.c"));
}
