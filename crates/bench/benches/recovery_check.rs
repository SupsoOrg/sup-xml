//! Diagnostic: compare recovery behaviour of sup-xml vs libxml2.
//!
//! Both parsers offer a "keep going past non-fatal errors" mode —
//! libxml2 calls it `XML_PARSE_RECOVER`, sup-xml calls it
//! `recovery_mode: true`.  This bench feeds the same
//! deliberately-malformed XML inputs to both and reports:
//!
//!   - Strict mode verdict (default settings, no recovery): did the
//!     parser reject the input?
//!   - Recover mode verdict: did the parser produce something
//!     usable?  Was the root element name what we'd expect?
//!   - For sup-xml only, how many errors were logged via
//!     `recovered_errors()`.
//!
//! Useful to verify that our recovery decisions roughly match
//! libxml2's — the closer they are, the more drop-in sup-xml is
//! for libxml2-using code that relies on RECOVER semantics.
//!
//! Run with:
//!     cargo bench -p sup-xml-bench --bench recovery_check

use std::os::raw::{c_char, c_int};
use std::ptr::NonNull;

use sup_xml::{BytesEvent, ParseOptions, XmlBytesReader};

// ── libxml2 FFI shim ──────────────────────────────────────
//
// Bindings for the few calls we need.  All `unsafe`; the bench
// itself stays a single safe-call wrapper around them.

#[allow(non_camel_case_types)]
enum XmlDoc {}
#[allow(non_camel_case_types)]
enum XmlNode {}

unsafe extern "C" {
    fn xmlParseMemory(buffer: *const c_char, size: c_int) -> *mut XmlDoc;
    fn xmlReadMemory(buffer: *const c_char, size: c_int,
                     url: *const c_char, encoding: *const c_char,
                     options: c_int) -> *mut XmlDoc;
    fn xmlFreeDoc(doc: *mut XmlDoc);
    fn xmlDocGetRootElement(doc: *mut XmlDoc) -> *mut XmlNode;
    fn xmlSetGenericErrorFunc(ctx: *mut std::ffi::c_void,
                              handler: Option<extern "C" fn()>);
}

// libxml2 parser option bits (from parser.h):
const XML_PARSE_RECOVER:   c_int = 1 << 0;   // recover on errors
const XML_PARSE_NOERROR:   c_int = 1 << 5;   // suppress error reports
const XML_PARSE_NOWARNING: c_int = 1 << 6;   // suppress warnings

#[derive(Debug)]
struct LibxmlVerdict {
    accepted: bool,
    has_root: bool,
}

fn libxml_strict(bytes: &[u8]) -> LibxmlVerdict {
    // SAFETY: pointer + length to a borrowed slice; doc freed
    // immediately if non-null.  No threading.
    // Why unsafe: only way to call into C.
    unsafe {
        let doc = xmlParseMemory(bytes.as_ptr() as *const c_char, bytes.len() as c_int);
        if doc.is_null() {
            LibxmlVerdict { accepted: false, has_root: false }
        } else {
            let root = xmlDocGetRootElement(doc);
            let v = LibxmlVerdict { accepted: true, has_root: !root.is_null() };
            xmlFreeDoc(doc);
            v
        }
    }
}

fn libxml_recover(bytes: &[u8]) -> LibxmlVerdict {
    // SAFETY: same shape as libxml_strict; we suppress error
    // output via NOERROR | NOWARNING so the bench output isn't
    // polluted by libxml2's stderr complaints on each bad input.
    unsafe {
        let opts = XML_PARSE_RECOVER | XML_PARSE_NOERROR | XML_PARSE_NOWARNING;
        let doc = xmlReadMemory(
            bytes.as_ptr() as *const c_char, bytes.len() as c_int,
            std::ptr::null(), std::ptr::null(),
            opts,
        );
        if doc.is_null() {
            LibxmlVerdict { accepted: false, has_root: false }
        } else {
            let root = xmlDocGetRootElement(doc);
            let v = LibxmlVerdict { accepted: true, has_root: !root.is_null() };
            xmlFreeDoc(doc);
            v
        }
    }
}

#[derive(Debug)]
struct SuperVerdict {
    accepted: bool,
    start_count: u32,
    recovered_errors: usize,
}

fn super_strict(bytes: &[u8]) -> SuperVerdict {
    let mut r = match XmlBytesReader::from_bytes(bytes) {
        Ok(r) => r.with_options(ParseOptions::default()),
        Err(_) => return SuperVerdict { accepted: false, start_count: 0, recovered_errors: 0 },
    };
    let mut starts = 0u32;
    loop {
        match r.next() {
            Ok(BytesEvent::Eof) => return SuperVerdict { accepted: true, start_count: starts, recovered_errors: 0 },
            Ok(BytesEvent::StartElement(_)) => starts += 1,
            Ok(_) => continue,
            Err(_) => return SuperVerdict { accepted: false, start_count: starts, recovered_errors: 0 },
        }
    }
}

fn super_recover(bytes: &[u8]) -> SuperVerdict {
    let opts = ParseOptions { recovery_mode: true, ..ParseOptions::default() };
    let mut r = match XmlBytesReader::from_bytes(bytes) {
        Ok(r) => r.with_options(opts),
        Err(_) => return SuperVerdict { accepted: false, start_count: 0, recovered_errors: 0 },
    };
    let mut starts = 0u32;
    let accepted = loop {
        match r.next() {
            Ok(BytesEvent::Eof) => break true,
            Ok(BytesEvent::StartElement(_)) => starts += 1,
            Ok(_) => continue,
            Err(_) => break false,
        }
    };
    SuperVerdict {
        accepted,
        start_count: starts,
        recovered_errors: r.recovered_errors().len(),
    }
}

const CASES: &[(&str, &str)] = &[
    ("unclosed nested",         "<r><a><b>"),
    ("unclosed root",           "<r>"),
    ("mismatched end tag",      "<a><b></a>"),
    ("mismatched, deep",        "<a><b><c></a>"),
    ("orphan end tag",          "</orphan>"),
    ("two roots",               "<a/><b/>"),
    ("undefined entity in text","<r>before &xyz; after</r>"),
    ("empty document",          ""),

    // Cases recovery should still NOT silently accept (would
    // diverge from spec / libxml2 wildly).
    ("bare < in text",          "<r>1 < 2</r>"),
    ("bare & in text",          "<r>tom & jerry</r>"),
    ("]]> in text",             "<r>oops]]>more</r>"),
    ("malformed XML decl",      "<?xml?><r/>"),
    ("text at doc level",       "hello<r/>"),
];

fn main() {
    // Suppress libxml2's stderr noise (it shouts on every error
    // even with NOERROR set — depends on libxml2 version).
    // SAFETY: passing null handler + null context tells libxml2
    // to discard error reports; documented behaviour.
    // Why unsafe: C interop.
    unsafe { xmlSetGenericErrorFunc(std::ptr::null_mut(), None); }
    let _ = NonNull::<u8>::dangling; // silence unused-import in some toolchains

    println!();
    println!("Recovery comparison: sup-xml (recovery_mode=true) vs libxml2 (XML_PARSE_RECOVER)");
    println!();
    println!("{:<28}  {:^16}  {:^16}  {:^24}  {:^16}",
             "case", "libxml2 strict", "libxml2 recover", "sup-xml recover", "sup-xml strict");
    println!("{:─<28}  {:─^16}  {:─^16}  {:─^24}  {:─^16}",
             "", "", "", "", "");

    let mut div = 0;
    let mut both_recover = 0;
    let mut sx_only_recover = 0;
    let mut lx_only_recover = 0;

    for (label, src) in CASES {
        let lx_s = libxml_strict(src.as_bytes());
        let lx_r = libxml_recover(src.as_bytes());
        let sx_s = super_strict(src.as_bytes());
        let sx_r = super_recover(src.as_bytes());

        // `has_root` distinguishes "OK with a tree" from "OK with
        // an empty result" — useful for cases like the empty
        // document where libxml2 returns NULL even with RECOVER.
        let fmt_lx = |v: &LibxmlVerdict| match (v.accepted, v.has_root) {
            (true,  true)  => "OK".to_string(),
            (true,  false) => "OK no-root".to_string(),
            (false, _)     => "REJECT".to_string(),
        };
        let lx_s_str = fmt_lx(&lx_s);
        let lx_r_str = fmt_lx(&lx_r);
        let sx_s_str = if sx_s.accepted { "OK" } else { "REJECT" };
        let sx_r_str = if sx_r.accepted {
            format!("OK ({} starts, {} errs)", sx_r.start_count, sx_r.recovered_errors)
        } else {
            format!("REJECT ({} errs)", sx_r.recovered_errors)
        };

        if lx_r.accepted && !sx_r.accepted { lx_only_recover += 1; div += 1; }
        else if !lx_r.accepted && sx_r.accepted { sx_only_recover += 1; div += 1; }
        else if lx_r.accepted && sx_r.accepted { both_recover += 1; }

        println!("{:<28}  {:^16}  {:^16}  {:<24}  {:^16}",
                 label, &lx_s_str, &lx_r_str, sx_r_str, sx_s_str);
    }

    println!();
    println!("Summary:");
    println!("  Both recovered:    {}", both_recover);
    println!("  libxml2 only:      {}", lx_only_recover);
    println!("  sup-xml only:     {}", sx_only_recover);
    println!("  Total divergences: {}", div);
}
