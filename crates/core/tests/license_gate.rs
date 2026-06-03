//! Proves the parse-time license gate rejects when no certificate is
//! reachable.
//!
//! This lives in its own test binary so that clearing the license
//! environment affects nothing else, and the process-wide verdict cache
//! (a `OnceLock`) initializes to "unlicensed" on the single parse below.

use std::path::PathBuf;
use sup_xml_core::{ParseOptions, parse_str};

#[test]
fn parsing_without_a_license_is_rejected() {
    // Neutralise every place the gate looks for a certificate BEFORE the
    // first parse initializes the cached verdict: `SUPSO_LICENSE` (set
    // workspace-wide via .cargo/config.toml), and HOME / USERPROFILE /
    // cwd so the default `.supso/license_certificates/` search finds
    // nothing.  Safe: single test, single-threaded, no parse has run yet.
    let empty: PathBuf =
        std::env::temp_dir().join(format!("supso-unlicensed-{}", std::process::id()));
    std::fs::create_dir_all(&empty).unwrap();
    unsafe {
        std::env::remove_var("SUPSO_LICENSE");
        std::env::set_var("HOME", &empty);
        std::env::set_var("USERPROFILE", &empty);
    }
    std::env::set_current_dir(&empty).unwrap();

    let err = parse_str("<r/>", &ParseOptions::default())
        .expect_err("parsing must be rejected without a valid license");
    let msg = err.to_string();
    assert!(msg.contains("license"), "expected a license error, got: {msg}");
}
