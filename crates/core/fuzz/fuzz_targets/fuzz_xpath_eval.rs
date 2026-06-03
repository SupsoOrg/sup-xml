#![no_main]

//! Fuzz target — feed arbitrary UTF-8 input as an XPath expression and
//! evaluate it against a fixed, modestly-shaped document.  Errors are
//! fine; panics, infinite loops, OOB indexing, and arithmetic overflows
//! are bugs.
//!
//! Why a fixed document?  The evaluator (`xpath::eval`) has far more
//! panic surface than the parser: every built-in function (`substring`,
//! `translate`, `count`, `number(...)`, `format-number` via EXSLT, the
//! date family, etc.) is a separate cluster of integer arithmetic,
//! UTF-8 indexing, and edge-case branches.  Static XPaths against a
//! known doc exercise *evaluator* paths in a way the parser-only target
//! can't reach.
//!
//! The fixture is intentionally varied — elements with text/CDATA,
//! attributes, mixed-namespace nodes, deep nesting, numeric and string
//! content — so that wildcards, predicates, axes, and node-tests all
//! land on something interesting.

use libfuzzer_sys::fuzz_target;
use std::cell::OnceCell;
use sup_xml_core::{parse_str, ParseOptions};
use sup_xml_core::xpath::XPathContext;
use sup_xml_tree::dom::Document;

const FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<catalog xmlns:ex="urn:example" xml:lang="en">
  <book id="b1" price="9.99">
    <title>The Pragmatic Programmer</title>
    <author>Hunt</author>
    <author>Thomas</author>
    <year>1999</year>
    <tags><tag>classic</tag><tag>craft</tag></tags>
  </book>
  <book id="b2" price="42">
    <title lang="en">Compilers</title>
    <author>Aho</author>
    <author>Sethi</author>
    <author>Ullman</author>
    <year>2006</year>
    <ex:rating>5</ex:rating>
    <!-- a comment node for comment() tests -->
    <?xml-stylesheet href="x.xsl"?>
    <desc><![CDATA[<not really xml>]]></desc>
  </book>
  <book id="b3" price="0">
    <title/>
    <year>-1</year>
    <empty></empty>
    <nested><a><b><c><d>deep</d></c></b></a></nested>
  </book>
  <unicode>café αβγ 中文 𝛼</unicode>
</catalog>"#;

// `Document` owns a `bumpalo::Bump` arena and so is `!Sync` — can't sit in
// a `static OnceLock`.  libFuzzer is single-threaded, so a `thread_local!`
// with `OnceCell` gives us "parse once, evaluate many times" without
// needing `Sync`.  We leak the cell's contents into a `&'static Document`
// via `Box::leak` so `XPathContext::new(&'doc Document)` is happy and we
// don't pay per-iteration parse cost.
thread_local! {
    static DOC: OnceCell<&'static Document> = const { OnceCell::new() };
}

fn doc() -> &'static Document {
    DOC.with(|cell| {
        *cell.get_or_init(|| {
            let d = parse_str(FIXTURE, &ParseOptions::default())
                .expect("fixture parses");
            Box::leak(Box::new(d))
        })
    })
}

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let d = doc();
    let ctx = XPathContext::new(d);
    let _ = ctx.eval(s);
});
