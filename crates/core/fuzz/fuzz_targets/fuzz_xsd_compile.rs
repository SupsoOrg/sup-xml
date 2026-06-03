#![no_main]

//! Fuzz target — feed arbitrary bytes (interpreted as UTF-8 strings) to
//! [`Schema::compile_str`] and assert that the schema compiler never
//! panics.  Errors are fine; crashes are bugs.
//!
//! The compiler walks the input as XML, parses XSD constructs out of it,
//! and resolves type references.  Any panic — assertion, unwrap on a
//! malformed input, integer overflow, OOB slice — counts as a finding
//! the fuzzer needs to surface.

use libfuzzer_sys::fuzz_target;
use sup_xml_core::xsd::Schema;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        // Result is intentionally discarded.  We only care that the
        // call returns at all (no panic, no infinite loop — libFuzzer
        // detects hangs separately via its own timeout).
        let _ = Schema::compile_str(s);
    }
});
