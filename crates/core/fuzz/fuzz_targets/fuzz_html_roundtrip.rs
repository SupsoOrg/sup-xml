#![no_main]

//! Fuzz target — parse arbitrary bytes as HTML, serialize the resulting tree
//! back to HTML, then parse that output again.  Asserts the HTML serializer
//! never panics on a recovered tree and that re-parsing its own output is
//! crash-free.
//!
//! HTML serialization is *not* required to be idempotent — recovery rewrites
//! the tree (implied `<html>`/`<body>`, void-element handling, raw-text
//! elements) — so this only asserts no panic, not output equality.  It
//! exercises the serializer's void-element / raw-text / boolean-attribute
//! paths against trees the example-based tests don't reach, and confirms the
//! serializer's output is itself parseable.

use libfuzzer_sys::fuzz_target;
use sup_xml_core::html::parse_html_bytes;
use sup_xml_core::serialize_html_to_string;

fuzz_target!(|data: &[u8]| {
    let Ok(doc) = parse_html_bytes(data) else { return };
    let html = serialize_html_to_string(&doc);
    let _ = parse_html_bytes(html.as_bytes());
});
