//! Tests covering how the parser handles non-UTF-8 inputs.
//!
//! By default the parser auto-detects the input's encoding (BOM → XML 1.0
//! Appendix F autodetect → `<?xml encoding="..."?>`) and transcodes to
//! UTF-8 before parsing.  Callers who want to require UTF-8 input set
//! `ParseOptions { auto_transcode: false, .. }`; those inputs are then
//! rejected with an `ErrorDomain::Encoding` error.
//!
//! The [`sup_xml::encoding`] module is still exposed for callers who
//! want to run the transcoding step separately (e.g. cache the decoded
//! bytes, inspect what was detected, share the same buffer across
//! multiple parses).

use sup_xml::{encoding, parse_bytes, ErrorDomain, NodeKind, ParseOptions};

/// Hand-written tiny ISO-8859-1 document.
///
/// Contents:
///   <?xml version="1.0" encoding="ISO-8859-1"?><r>café</r>
///
/// The `é` is byte 0xE9 in ISO-8859-1.  Standalone 0xE9 is **not** valid UTF-8
/// (UTF-8 reserves 0xC0–0xFD as multi-byte lead bytes that must be followed by
/// continuation bytes in 0x80–0xBF), so our UTF-8 upfront check rejects it.
const ISO_8859_1_MINIMAL: &[u8] =
    b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><r>caf\xe9</r>";

#[test]
fn iso_8859_1_minimal_rejected_when_auto_transcode_disabled() {
    let opts = ParseOptions { auto_transcode: false, ..Default::default() };
    let err = parse_bytes(ISO_8859_1_MINIMAL, &opts)
        .expect_err("strict UTF-8 mode rejects Latin-1");
    assert_eq!(
        err.domain,
        ErrorDomain::Encoding,
        "expected an Encoding error, got: {err:?}",
    );
    assert!(
        err.message.contains("invalid UTF-8"),
        "expected message to mention invalid UTF-8, got: {:?}",
        err.message,
    );
}

/// Real-world ISO-8859-1 fixture from the bench corpus.
///
/// `transitions_tutorial.xml` declares `encoding="ISO-8859-1"` and contains
/// byte 0x85 (Windows-1252 horizontal ellipsis) at offset 5591, which is also
/// invalid as standalone UTF-8.
const TRANSITIONS_TUTORIAL: &[u8] =
    include_bytes!("../../../tests/assets/xml/transitions_tutorial.xml");

#[test]
fn transitions_tutorial_rejected_when_auto_transcode_disabled() {
    let opts = ParseOptions { auto_transcode: false, ..Default::default() };
    let err = parse_bytes(TRANSITIONS_TUTORIAL, &opts)
        .expect_err("strict UTF-8 mode rejects the ISO-8859-1 fixture");
    assert_eq!(
        err.domain,
        ErrorDomain::Encoding,
        "expected an Encoding error, got: {err:?}",
    );
    assert!(
        err.message.contains("invalid UTF-8"),
        "expected message to mention invalid UTF-8, got: {:?}",
        err.message,
    );
}

// ── Tier 1 transcoding: legacy-encoded docs parse via encoding::transcode_to_utf8 ──

#[test]
fn iso_8859_1_minimal_parses_via_manual_transcoding() {
    // Manual two-step pattern: transcode externally, then parse with
    // auto_transcode off so the parser doesn't re-detect the (now stale)
    // `encoding="ISO-8859-1"` declaration and try to decode the already-
    // UTF-8 bytes a second time.  Useful when the caller wants to inspect
    // the detected encoding or share decoded bytes across multiple parses.
    let utf8 = encoding::transcode_to_utf8(ISO_8859_1_MINIMAL)
        .expect("Latin-1 transcodes cleanly");
    let opts = ParseOptions { auto_transcode: false, ..Default::default() };
    let doc  = parse_bytes(&utf8, &opts).expect("transcoded UTF-8 parses");
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    assert_eq!(root.name(), "r");
    let text = root.children().find_map(|n| n.text_content()).unwrap_or("");
    assert_eq!(text, "café", "got: {text:?}");
}

#[test]
fn transitions_tutorial_xml_parses_via_default_auto_transcode() {
    // Real-world ISO-8859-1 fixture from the bench corpus.  With auto-
    // transcode on (the default), parse_bytes handles it in one step.
    let doc = parse_bytes(TRANSITIONS_TUTORIAL, &ParseOptions::default())
        .expect("default parses Latin-1 fixture");
    assert_eq!(doc.root().kind, NodeKind::Element, "expected a root element");
}

#[test]
fn windows_1252_ellipsis_parses_via_default_auto_transcode() {
    // Byte 0x85 in Windows-1252 is U+2026 (horizontal ellipsis).
    let bytes: &[u8] =
        b"<?xml version=\"1.0\" encoding=\"Windows-1252\"?><r>foo\x85bar</r>";
    let doc  = parse_bytes(bytes, &ParseOptions::default()).expect("default parses Windows-1252");
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let text = root.children().find_map(|n| n.text_content()).unwrap_or("");
    assert_eq!(text, "foo…bar", "got: {text:?}");
}

#[test]
fn truly_unknown_encoding_label_returns_encoding_error() {
    // GB2312, Shift_JIS, etc. are all handled by Tier 3 (encoding_rs).  To
    // exercise the error path we use a deliberately fake encoding name that
    // no real registry knows about.
    let bytes: &[u8] =
        b"<?xml version=\"1.0\" encoding=\"definitely-not-a-real-encoding\"?><r/>";
    let err = encoding::transcode_to_utf8(bytes)
        .expect_err("nonsense encoding label must error");
    assert_eq!(err.domain, ErrorDomain::Encoding);
}

// ── auto_transcode option: single-call parsing of non-UTF-8 input ─────────────

#[test]
fn auto_transcode_parses_iso_8859_1_directly() {
    let opts = ParseOptions { auto_transcode: true, ..Default::default() };
    let doc  = parse_bytes(ISO_8859_1_MINIMAL, &opts)
        .expect("auto_transcode handles Latin-1");
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let text = root.children().find_map(|n| n.text_content()).unwrap_or("");
    assert_eq!(text, "café", "got: {text:?}");
}

#[test]
fn auto_transcode_parses_utf16_be_directly() {
    // BOM FE FF, then "<r/>" each as 2 BE bytes: 00 3C 00 72 00 2F 00 3E
    let bytes: &[u8] = &[
        0xFE, 0xFF,
        0x00, 0x3C, 0x00, 0x72, 0x00, 0x2F, 0x00, 0x3E,
    ];
    let opts = ParseOptions { auto_transcode: true, ..Default::default() };
    let doc  = parse_bytes(bytes, &opts)
        .expect("auto_transcode handles UTF-16BE");
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    assert_eq!(root.name(), "r");
}

#[test]
fn auto_transcode_parses_gb2312_directly_via_encoding_rs() {
    let bytes: &[u8] =
        b"<?xml version=\"1.0\" encoding=\"GB2312\"?><r>\xD6\xD0</r>";
    let opts = ParseOptions { auto_transcode: true, ..Default::default() };
    let doc  = parse_bytes(bytes, &opts)
        .expect("auto_transcode + encoding_rs handles GB2312");
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let text = root.children().find_map(|n| n.text_content()).unwrap_or("");
    assert_eq!(text, "中", "got: {text:?}");
}

#[test]
fn default_options_auto_transcode_is_on() {
    // Pin the default — flipping it back to off would break callers who
    // expect libxml2-style behaviour out of the box.
    assert!(ParseOptions::default().auto_transcode,
            "auto_transcode must be on by default for libxml2 parity");
}

#[test]
fn parse_bytes_default_path_handles_iso_8859_1() {
    // parse_bytes with default options flips auto_transcode on, so this
    // should round-trip Latin-1 without the caller passing custom options.
    let doc  = parse_bytes(ISO_8859_1_MINIMAL, &ParseOptions::default())
        .expect("default parses Latin-1");
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let text = root.children().find_map(|n| n.text_content()).unwrap_or("");
    assert_eq!(text, "café", "got: {text:?}");
}

#[test]
fn auto_transcode_is_zero_cost_for_utf8_input() {
    // UTF-8 input goes through transcode_to_utf8 as Cow::Borrowed, so the
    // option costs only a ~100-byte detection scan and should produce
    // identical results to the strict path.
    let utf8: &[u8] = b"<r><a>1</a><b>2</b></r>";
    let strict = parse_bytes(utf8, &ParseOptions::default()).unwrap();
    let auto   = parse_bytes(utf8, &ParseOptions { auto_transcode: true, ..Default::default() }).unwrap();
    // Compare via a structural check — both should produce a single root <r>
    // with two element children named "a" and "b".
    for doc in [&strict, &auto] {
        let root = doc.root();
        assert_eq!(root.kind, NodeKind::Element);
        assert_eq!(root.name(), "r");
        let kids: Vec<&str> = root.children()
            .filter(|n| n.kind == NodeKind::Element)
            .map(|n| n.name())
            .collect();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0], "a");
        assert_eq!(kids[1], "b");
    }
}
