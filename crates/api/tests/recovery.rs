//! Tests for the tree-parser recovery path —
//! `parse_str_with_recovered` / `parse_bytes_with_recovered`.
//!
//! Strict mode (`recovery_mode: false`) still fatals on the same
//! inputs.  Recovery mode logs each violation to the returned
//! `Vec<XmlError>` and continues, producing a Document whose
//! serialized form preserves user data.

use sup_xml::{
    parse_bytes_with_recovered, parse_str, parse_str_with_recovered,
    serialize_to_string, ErrorLevel, NodeKind, ParseOptions,
};

fn recover_opts() -> ParseOptions {
    ParseOptions { recovery_mode: true, ..ParseOptions::default() }
}

// ── strict mode still rejects these inputs ────────────────────────────────────

#[test]
fn strict_mode_rejects_bare_ampersand_in_text() {
    let err = parse_str("<r>tom & jerry</r>", &ParseOptions::default()).unwrap_err();
    assert_eq!(err.level, ErrorLevel::Fatal);
}

#[test]
fn strict_mode_rejects_unclosed_at_eof() {
    let err = parse_str("<r><a>hello", &ParseOptions::default()).unwrap_err();
    // Arena parser reports unclosed-at-EOF as recoverable Error in strict
    // mode (it would be Fatal in legacy); either is a rejection.
    assert!(matches!(err.level, ErrorLevel::Error | ErrorLevel::Fatal));
}

// ── recovery: bare `&` in text content ────────────────────────────────────────

#[test]
fn recover_logs_bare_ampersand_and_keeps_it_literal() {
    let (doc, recovered) = parse_str_with_recovered("<r>tom & jerry</r>", &recover_opts());
    let doc = doc.expect("recovery_mode should build a tree");

    assert_eq!(recovered.len(), 1);
    assert!(
        recovered[0].message.contains("bare '&'"),
        "unexpected recovered message: {}", recovered[0].message,
    );
    assert_eq!(recovered[0].level, ErrorLevel::Error);

    // The `&` survived into the tree as a literal text character.
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    let mut text = String::new();
    for child in root.children() {
        if child.kind == NodeKind::Text { text.push_str(child.content()); }
    }
    assert_eq!(text, "tom & jerry");
}

#[test]
fn recover_bare_ampersand_serializes_back_as_amp_entity() {
    let (doc, _recovered) = parse_str_with_recovered("<r>tom & jerry</r>", &recover_opts());
    let s = serialize_to_string(&doc.unwrap());
    assert!(s.contains("tom &amp; jerry"), "got: {s}");
}

// ── recovery: unclosed element at EOF ─────────────────────────────────────────

#[test]
fn recover_logs_one_error_per_unclosed_level() {
    let (doc, recovered) = parse_str_with_recovered("<r><a>hello", &recover_opts());
    let doc = doc.expect("recovery_mode should build a tree");

    assert_eq!(recovered.len(), 2);
    assert!(recovered[0].message.contains("unclosed element"));
    assert!(recovered[0].message.contains("<a>"));
    assert!(recovered[1].message.contains("<r>"));
    assert!(recovered.iter().all(|e| e.level == ErrorLevel::Error));

    // The tree is fully closed.
    let root = doc.root();
    assert_eq!(root.kind, NodeKind::Element);
    assert_eq!(root.name(), "r");
    let inner = root.children().next().unwrap();
    assert_eq!(inner.kind, NodeKind::Element);
    assert_eq!(inner.name(), "a");
    assert_eq!(inner.children().count(), 1);
}

#[test]
fn recover_unclosed_serializes_to_closed_xml() {
    let (doc, _) = parse_str_with_recovered("<r><a>hello", &recover_opts());
    let s = serialize_to_string(&doc.unwrap());
    assert!(s.contains("<a>hello</a>"));
    assert!(s.contains("</r>"));
}

// ── recovery: combined bare-`&` and unclosed-at-EOF ──────────────────────────

#[test]
fn recover_combined_bare_amp_and_unclosed() {
    let (doc, recovered) = parse_str_with_recovered(
        "<r>tom & jerry<unclosed>", &recover_opts(),
    );
    let doc = doc.expect("recovery_mode should build a tree");

    // 1 bare `&` + 2 unclosed levels = 3 recovered errors, in source order.
    assert_eq!(recovered.len(), 3);
    assert!(recovered[0].message.contains("bare '&'"));
    assert!(recovered[1].message.contains("<unclosed>"));
    assert!(recovered[2].message.contains("<r>"));

    let s = serialize_to_string(&doc);
    assert!(s.contains("tom &amp; jerry"));
    assert!(s.contains("<unclosed/>") || s.contains("<unclosed></unclosed>"));
    assert!(s.contains("</r>"));
}

// ── strict-mode parity: no recovery sink populated when off ──────────────────

#[test]
fn strict_mode_returns_empty_recovered_list_on_success() {
    let (doc, recovered) = parse_str_with_recovered("<r/>", &ParseOptions::default());
    assert!(doc.is_ok());
    assert!(recovered.is_empty());
}

#[test]
fn strict_mode_returns_err_and_empty_recovered_list_on_failure() {
    let (doc, recovered) = parse_str_with_recovered(
        "<r>tom & jerry</r>", &ParseOptions::default(),
    );
    assert!(doc.is_err());
    assert!(recovered.is_empty(), "strict mode shouldn't populate the recovery sink");
}

// ── bytes-input variant works the same ───────────────────────────────────────

#[test]
fn parse_bytes_with_recovered_handles_bare_amp() {
    let (doc, recovered) = parse_bytes_with_recovered(
        b"<r>tom & jerry</r>", &recover_opts(),
    );
    assert!(doc.is_ok());
    assert_eq!(recovered.len(), 1);
}

#[test]
fn parse_bytes_with_recovered_rejects_invalid_utf8() {
    // `\xFF` is never valid UTF-8.  Should fail fast with an Encoding-domain
    // fatal and no recovered errors (we never reached the parser).
    let (doc, recovered) = parse_bytes_with_recovered(
        b"<r>\xFF</r>", &recover_opts(),
    );
    let err = doc.unwrap_err();
    assert_eq!(err.level, ErrorLevel::Fatal);
    assert!(recovered.is_empty());
}

// ── error metadata: line/column carry through ────────────────────────────────

#[test]
fn recovered_error_carries_line_info() {
    let (_, recovered) = parse_str_with_recovered("<r>\n  tom & jerry</r>", &recover_opts());
    let err = &recovered[0];
    assert_eq!(err.line, Some(2));
    assert!(err.column.is_some());
}
