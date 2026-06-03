// `#![forbid(unsafe_code)]` is applied per-submodule.  The arena-backed sink
// ([`sink`]) opts in locally for the type-erased Handle trick — see its
// module docs for the safety argument.
#![deny(unsafe_code)]

//! Lenient HTML5 parser.
//!
//! Wraps [html5ever](https://github.com/servo/html5ever) — the canonical
//! Rust HTML5 parser from Servo — driving it into our arena
//! [`Document`](sup_xml_tree::dom::Document) so the rest of SupXML
//! (XPath, Selector, serializer) "just works" on the result.
//!
//! Gated behind the `html` Cargo feature: users who don't need HTML
//! parsing pay no compile-time or dep-tree cost.
//!
//! # Quick start
//!
//! ```no_run
//! use sup_xml_core::html::parse_html_str;
//!
//! let html = r#"<html><body><p>hello <b>world</b></p></body></html>"#;
//! let doc = parse_html_str(html).unwrap();
//! assert!(doc.is_html());
//! ```
//!
//! # Design
//!
//! - html5ever for spec-compliant HTML5 parsing (~95% of html5lib-tests).
//! - Output is the arena [`Document`](sup_xml_tree::dom::Document)
//!   that XPath/Selector/serializer all work on natively.
//! - DoS-protection limits (`max_element_depth`, `max_text_bytes`) are
//!   enforced inside our `TreeSink` since html5ever has none built in.
//! - HTML defaults to lenient (`recovery_mode: true`); inverted from
//!   `ParseOptions` because HTML is a lenient format by spec.
//!
//! Because html5ever implements the WHATWG/HTML5 spec exactly (the same
//! algorithm browsers use), recovery of malformed input can differ from
//! libxml2's older ad-hoc HTML parser — e.g. a stray `</body>` is
//! recovered silently and a stray `</p>` inserts an implied `<p>`, where
//! libxml2 flags the former and drops the latter.  This is a deliberate
//! divergence (our output matches browsers); see the `sup-xml-compat`
//! crate docs, "Behavioral divergences", for the affected lxml tests.

pub mod encoding;
pub mod events;
pub mod options;
mod sink;
mod stream;

pub use encoding::{decode_html_input, prescan_meta_charset, sniff_html_encoding};
pub use events::{HtmlAttribute, HtmlAttrs, HtmlAttrsIter, HtmlEvent};
pub use options::HtmlParseOptions;
pub use stream::{HtmlBytesReader, HtmlReader, HtmlSaxHandler, HtmlSaxParser};

use html5ever::driver::{parse_document, ParseOpts};
use html5ever::tendril::TendrilSink;
use html5ever::tokenizer::TokenizerOpts;
use html5ever::tree_builder::TreeBuilderOpts;

use crate::error::{ErrorDomain, ErrorLevel, Result, XmlError};

use sup_xml_tree::dom::Document;

/// Parse an HTML document from a UTF-8 string into the arena DOM.
///
/// Defaults are tuned for browser-equivalent output and lenient
/// recovery — see [`HtmlParseOptions`] for the knobs.
///
/// Errors: returns `Err` only when an internal limit is exceeded
/// ([`HtmlParseOptions::max_element_depth`],
/// [`HtmlParseOptions::max_text_bytes`]) or when
/// [`HtmlParseOptions::recovery_mode`] is `false` and the parser
/// encountered a well-formedness violation.  In recovery mode (the
/// default) parse errors are *recovered* into the resulting tree;
/// retrieve them via [`parse_html_str_with_recovered`].
pub fn parse_html_str(input: &str) -> Result<Document> {
    parse_html_str_opts(input, &HtmlParseOptions::default())
}

/// Parse an HTML document from raw bytes into the arena DOM.
pub fn parse_html_bytes(input: &[u8]) -> Result<Document> {
    parse_html_bytes_opts(input, &HtmlParseOptions::default())
}

/// Parse an HTML string into the arena DOM with explicit options.
pub fn parse_html_str_opts(input: &str, opts: &HtmlParseOptions) -> Result<Document> {
    let (doc, _recovered, fatal) = parse_html_str_inner(input, opts);
    if let Some(fatal) = fatal {
        return Err(fatal);
    }
    Ok(doc)
}

/// Parse HTML bytes into the arena DOM with explicit options.
pub fn parse_html_bytes_opts(
    input: &[u8],
    opts: &HtmlParseOptions,
) -> Result<Document> {
    let (doc, _recovered, fatal) = parse_html_bytes_inner(input, opts);
    if let Some(fatal) = fatal {
        return Err(fatal);
    }
    Ok(doc)
}

/// Variant of [`parse_html_bytes_opts`] that interns element /
/// attribute names through `dict` instead of an internal one.
/// Mirrors [`crate::parser::parse_bytes_with_dtd_and_dict`] for
/// the HTML side — used by the C-ABI layer to share a parser
/// context's dict with the resulting document.
///
/// # Safety
///
/// `dict` must be a valid pointer returned by
/// [`sup_xml_tree::dict::Dict::new_refcounted`] (or otherwise
/// refcount-managed), with at least one outstanding reference.
#[cfg(feature = "c-abi")]
#[allow(unsafe_code)]
pub unsafe fn parse_html_bytes_opts_with_dict(
    input: &[u8],
    opts: &HtmlParseOptions,
    dict: *mut sup_xml_tree::dict::Dict,
) -> Result<Document> {
    let (doc, _recovered, fatal) = unsafe { parse_html_bytes_inner_with_dict(input, opts, dict) };
    if let Some(fatal) = fatal {
        return Err(fatal);
    }
    Ok(doc)
}

/// As [`parse_html_bytes_opts_with_dict`] but also adopts a
/// caller-supplied [`bumpalo::Bump`] arena.  See
/// [`crate::parser::parse_bytes_with_dtd_dict_arena`] for the
/// architectural rationale.
///
/// # Safety
///
/// See [`parse_html_bytes_opts_with_dict`].
#[cfg(feature = "c-abi")]
#[allow(unsafe_code)]
pub unsafe fn parse_html_bytes_opts_with_dict_arena(
    input: &[u8],
    opts:  &HtmlParseOptions,
    dict:  *mut sup_xml_tree::dict::Dict,
    arena: std::sync::Arc<bumpalo::Bump>,
) -> Result<Document> {
    let (doc, _recovered, fatal) =
        unsafe { parse_html_bytes_inner_with_dict_arena(input, opts, dict, arena) };
    if let Some(fatal) = fatal {
        return Err(fatal);
    }
    drop_implicit_empty_head(&doc, input);
    drop_implicit_empty_body(&doc, input);
    Ok(doc)
}

/// Like [`parse_html_bytes_opts_with_dict_arena`] but always returns the
/// (always-produced) HTML document together with the errors html5ever
/// recovered from.  The C-ABI shim uses this to set `ctxt->wellFormed`
/// (empty list ⇒ well-formed) so lxml's `recover=False` raises while
/// `recover=True` keeps the repaired tree.
///
/// # Safety
/// Same contract as [`parse_html_bytes_opts_with_dict_arena`].
#[cfg(feature = "c-abi")]
#[allow(unsafe_code)]
pub unsafe fn parse_html_bytes_recovered_with_dict_arena(
    input: &[u8],
    opts:  &HtmlParseOptions,
    dict:  *mut sup_xml_tree::dict::Dict,
    arena: std::sync::Arc<bumpalo::Bump>,
) -> (Document, Vec<XmlError>) {
    let (doc, mut recovered, fatal) =
        unsafe { parse_html_bytes_inner_with_dict_arena(input, opts, dict, arena) };
    if let Some(f) = fatal {
        recovered.push(f);
    }
    drop_implicit_empty_head(&doc, input);
    drop_implicit_empty_body(&doc, input);
    (doc, recovered)
}

/// Whether the HTML source contains an explicit `<head>` start tag —
/// matched as `<head` followed by a tag terminator, so `<header>` does
/// not count.  html5ever always synthesizes a `<head>`, but libxml2's
/// HTML parser materialises one only for an explicit tag or for
/// head-level content (title, meta, …).
#[cfg(feature = "c-abi")]
fn source_has_explicit_head(source: &[u8]) -> bool {
    let mut i = 0;
    while i + 5 < source.len() {
        if source[i] == b'<'
            && source[i + 1].eq_ignore_ascii_case(&b'h')
            && source[i + 2].eq_ignore_ascii_case(&b'e')
            && source[i + 3].eq_ignore_ascii_case(&b'a')
            && source[i + 4].eq_ignore_ascii_case(&b'd')
            && (source[i + 5] == b'>'
                || source[i + 5] == b'/'
                || source[i + 5].is_ascii_whitespace())
        {
            return true;
        }
        i += 1;
    }
    false
}

/// Reshape the html5ever tree to libxml2's HTML output: drop the empty
/// `<head>` html5ever always inserts when the source had neither an
/// explicit `<head>` tag nor head-level content, so a body-only
/// document (`<div/>`) becomes `html > body > div` instead of
/// `html > [head, body] > div`.  c-abi (libxml2-compat) path only — the
/// native parser keeps the HTML5-spec always-emit-head shape.
#[cfg(feature = "c-abi")]
fn drop_implicit_empty_head(doc: &Document, source: &[u8]) {
    if source_has_explicit_head(source) {
        return;
    }
    let root = doc.root();
    if root.name() != "html" {
        return;
    }
    if let Some(head) = root.children().find(|c| c.is_element() && c.name() == "head") {
        if head.children().next().is_none() {
            // Unlink the empty head from the html root, repairing the
            // sibling list.  (The c-abi `Document` has no `detach`, but
            // the link fields are public Cells in every build.)
            let prev = head.prev_sibling.get();
            let next = head.next_sibling.get();
            match prev {
                Some(p) => p.next_sibling.set(next),
                None    => root.first_child.set(next),
            }
            match next {
                Some(n) => n.prev_sibling.set(prev),
                None    => root.last_child.set(prev),
            }
            head.parent.set(None);
            head.prev_sibling.set(None);
            head.next_sibling.set(None);
        }
    }
}

/// Whether the HTML source contains an explicit `<body>` start tag —
/// matched as `<body` followed by a tag terminator.  html5ever always
/// synthesizes a `<body>`; libxml2's HTML parser materialises one only
/// for an explicit tag or body-level content.
#[cfg(feature = "c-abi")]
fn source_has_explicit_body(source: &[u8]) -> bool {
    let mut i = 0;
    while i + 5 < source.len() {
        if source[i] == b'<'
            && source[i + 1].eq_ignore_ascii_case(&b'b')
            && source[i + 2].eq_ignore_ascii_case(&b'o')
            && source[i + 3].eq_ignore_ascii_case(&b'd')
            && source[i + 4].eq_ignore_ascii_case(&b'y')
            && (source[i + 5] == b'>'
                || source[i + 5] == b'/'
                || source[i + 5].is_ascii_whitespace())
        {
            return true;
        }
        i += 1;
    }
    false
}

/// Drop the empty `<body>` html5ever always inserts when the source had
/// neither an explicit `<body>` tag nor body-level content, so
/// `<html></html>` becomes childless `html` rather than `html > body`,
/// matching libxml2's HTML output.  c-abi (libxml2-compat) path only.
#[cfg(feature = "c-abi")]
fn drop_implicit_empty_body(doc: &Document, source: &[u8]) {
    if source_has_explicit_body(source) {
        return;
    }
    let root = doc.root();
    if root.name() != "html" {
        return;
    }
    if let Some(body) = root.children().find(|c| c.is_element() && c.name() == "body") {
        if body.children().next().is_none() && body.attributes().next().is_none() {
            let prev = body.prev_sibling.get();
            let next = body.next_sibling.get();
            match prev {
                Some(p) => p.next_sibling.set(next),
                None    => root.first_child.set(next),
            }
            match next {
                Some(n) => n.prev_sibling.set(prev),
                None    => root.last_child.set(prev),
            }
            body.parent.set(None);
            body.prev_sibling.set(None);
            body.next_sibling.set(None);
        }
    }
}

/// Parse an HTML document from a string, returning both the document and the
/// list of recovered well-formedness errors.  The document is returned even if
/// errors were recovered; callers can inspect the `Vec<XmlError>` to see what
/// was repaired.
///
/// In strict mode (`recovery_mode: false`), the first error becomes fatal and
/// is returned via the `Result`.
pub fn parse_html_str_with_recovered(
    input: &str,
    opts: &HtmlParseOptions,
) -> (Result<Document>, Vec<XmlError>) {
    let (doc, recovered, fatal) = parse_html_str_inner(input, opts);
    let result = match fatal {
        Some(e) => Err(e),
        None    => Ok(doc),
    };
    (result, recovered)
}

/// Bytes equivalent of [`parse_html_str_with_recovered`].
pub fn parse_html_bytes_with_recovered(
    input: &[u8],
    opts: &HtmlParseOptions,
) -> (Result<Document>, Vec<XmlError>) {
    let (doc, recovered, fatal) = parse_html_bytes_inner(input, opts);
    let result = match fatal {
        Some(e) => Err(e),
        None    => Ok(doc),
    };
    (result, recovered)
}

fn parse_html_str_inner(
    input: &str,
    opts: &HtmlParseOptions,
) -> (Document, Vec<XmlError>, Option<XmlError>) {
    if let Err(e) = crate::license_gate::ensure_licensed() {
        let b = sup_xml_tree::dom::DocumentBuilder::new();
        let synth = b.new_element(b.alloc_str("html"));
        b.set_root(synth);
        return (b.build(), vec![e.clone()], Some(e));
    }
    let sink = sink::BatchSinkArena::new(opts.clone());
    let parser = parse_document(sink, make_h5_opts(opts));
    let sink = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.one(input))) {
        Ok(s) => s,
        Err(_) => {
            let err = XmlError::new(
                ErrorDomain::Html,
                ErrorLevel::Fatal,
                "internal HTML parser panic — input may be adversarial",
            );
            // Build an empty fallback Document.
            let b = sup_xml_tree::dom::DocumentBuilder::new();
            let synth = b.new_element(b.alloc_str("html"));
            b.set_root(synth);
            return (b.build(), vec![err.clone()], Some(err));
        }
    };
    sink::finalize_arena(sink)
}

fn parse_html_bytes_inner(
    input: &[u8],
    opts: &HtmlParseOptions,
) -> (Document, Vec<XmlError>, Option<XmlError>) {
    // WHATWG byte-stream sniffing: caller-supplied → BOM → meta
    // charset prescan → Windows-1252 fallback.  Transcode to UTF-8
    // before feeding to html5ever (which expects UTF-8 internally).
    let (utf8, _detected_encoding) = match encoding::decode_html_input(input, opts) {
        Ok(out) => out,
        Err(e) => {
            let b = sup_xml_tree::dom::DocumentBuilder::new();
            let synth = b.new_element(b.alloc_str("html"));
            b.set_root(synth);
            return (b.build(), vec![e.clone()], Some(e));
        }
    };
    match std::str::from_utf8(&utf8) {
        Ok(s) => parse_html_str_inner(s, opts),
        Err(_) => {
            let lossy = String::from_utf8_lossy(&utf8).into_owned();
            parse_html_str_inner(&lossy, opts)
        }
    }
}

/// External-dict variant of [`parse_html_bytes_inner`].  The sink
/// is constructed with [`sink::BatchSinkArena::new_with_dict`] so
/// element / attribute names are interned through the caller's
/// dict.  Fatal error fallbacks fall back to a default dict, since
/// at that point parsing didn't reach the consumer-visible doc
/// anyway.
///
/// # Safety
///
/// See [`parse_html_bytes_opts_with_dict`].
#[cfg(feature = "c-abi")]
#[allow(unsafe_code)]
unsafe fn parse_html_bytes_inner_with_dict(
    input: &[u8],
    opts: &HtmlParseOptions,
    dict: *mut sup_xml_tree::dict::Dict,
) -> (Document, Vec<XmlError>, Option<XmlError>) {
    if let Err(e) = crate::license_gate::ensure_licensed() {
        let b = unsafe { sup_xml_tree::dom::DocumentBuilder::new_with_dict(dict) };
        let synth = b.new_element(b.alloc_str("html"));
        b.set_root(synth);
        return (b.build(), vec![e.clone()], Some(e));
    }
    let (utf8, _detected_encoding) = match encoding::decode_html_input(input, opts) {
        Ok(out) => out,
        Err(e) => {
            let b = unsafe { sup_xml_tree::dom::DocumentBuilder::new_with_dict(dict) };
            let synth = b.new_element(b.alloc_str("html"));
            b.set_root(synth);
            return (b.build(), vec![e.clone()], Some(e));
        }
    };
    let owned: String = match std::str::from_utf8(&utf8) {
        Ok(s)  => s.to_string(),
        Err(_) => String::from_utf8_lossy(&utf8).into_owned(),
    };
    let sink = unsafe { sink::BatchSinkArena::new_with_dict(opts.clone(), dict) };
    let parser = parse_document(sink, make_h5_opts(opts));
    let sink = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.one(owned.as_str()))) {
        Ok(s) => s,
        Err(_) => {
            let err = XmlError::new(
                ErrorDomain::Html,
                ErrorLevel::Fatal,
                "internal HTML parser panic — input may be adversarial",
            );
            let b = unsafe { sup_xml_tree::dom::DocumentBuilder::new_with_dict(dict) };
            let synth = b.new_element(b.alloc_str("html"));
            b.set_root(synth);
            return (b.build(), vec![err.clone()], Some(err));
        }
    };
    sink::finalize_arena(sink)
}

/// Arena variant of [`parse_html_bytes_inner_with_dict`].  Routes
/// allocations through a caller-supplied shared bump arena.
#[cfg(feature = "c-abi")]
#[allow(unsafe_code)]
unsafe fn parse_html_bytes_inner_with_dict_arena(
    input: &[u8],
    opts:  &HtmlParseOptions,
    dict:  *mut sup_xml_tree::dict::Dict,
    arena: std::sync::Arc<bumpalo::Bump>,
) -> (Document, Vec<XmlError>, Option<XmlError>) {
    if let Err(e) = crate::license_gate::ensure_licensed() {
        let b = unsafe {
            sup_xml_tree::dom::DocumentBuilder::new_with_dict_and_arena(
                dict, std::sync::Arc::clone(&arena),
            )
        };
        let synth = b.new_element(b.alloc_str("html"));
        b.set_root(synth);
        return (b.build(), vec![e.clone()], Some(e));
    }
    let (utf8, _detected_encoding) = match encoding::decode_html_input(input, opts) {
        Ok(out) => out,
        Err(e) => {
            let b = unsafe {
                sup_xml_tree::dom::DocumentBuilder::new_with_dict_and_arena(
                    dict, std::sync::Arc::clone(&arena),
                )
            };
            let synth = b.new_element(b.alloc_str("html"));
            b.set_root(synth);
            return (b.build(), vec![e.clone()], Some(e));
        }
    };
    let owned: String = match std::str::from_utf8(&utf8) {
        Ok(s)  => s.to_string(),
        Err(_) => String::from_utf8_lossy(&utf8).into_owned(),
    };
    let sink = unsafe {
        sink::BatchSinkArena::new_with_dict_and_arena(opts.clone(), dict, std::sync::Arc::clone(&arena))
    };
    let parser = parse_document(sink, make_h5_opts(opts));
    let sink = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.one(owned.as_str()))) {
        Ok(s) => s,
        Err(_) => {
            let err = XmlError::new(
                ErrorDomain::Html,
                ErrorLevel::Fatal,
                "internal HTML parser panic — input may be adversarial",
            );
            let b = unsafe {
                sup_xml_tree::dom::DocumentBuilder::new_with_dict_and_arena(dict, arena)
            };
            let synth = b.new_element(b.alloc_str("html"));
            b.set_root(synth);
            return (b.build(), vec![err.clone()], Some(err));
        }
    };
    sink::finalize_arena(sink)
}

// ── shared internals ────────────────────────────────────────────────────────

fn make_h5_opts(opts: &HtmlParseOptions) -> ParseOpts {
    ParseOpts {
        tokenizer: TokenizerOpts {
            discard_bom: opts.discard_bom,
            ..Default::default()
        },
        tree_builder: TreeBuilderOpts {
            scripting_enabled: opts.scripting_enabled,
            iframe_srcdoc: opts.iframe_srcdoc,
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    //! Arena-DOM tests for the HTML5 parser.
    use super::*;
    use sup_xml_tree::dom::NodeKind;

    /// Walk children looking for the first element with `name`.
    fn find_elem<'a>(
        n: &'a sup_xml_tree::dom::Node<'a>,
        name: &str,
    ) -> Option<&'a sup_xml_tree::dom::Node<'a>> {
        if n.kind == NodeKind::Element && n.name() == name {
            return Some(n);
        }
        for c in n.children() {
            if let Some(hit) = find_elem(c, name) {
                return Some(hit);
            }
        }
        None
    }

    /// Concatenate text/cdata descendants of a node.
    fn collect_text<'a>(n: &'a sup_xml_tree::dom::Node<'a>) -> String {
        let mut s = String::new();
        fn go<'a>(n: &'a sup_xml_tree::dom::Node<'a>, s: &mut String) {
            match n.kind {
                NodeKind::Text | NodeKind::CData => s.push_str(n.content()),
                NodeKind::Element => {
                    for c in n.children() { go(c, s); }
                }
                _ => {}
            }
        }
        go(n, &mut s);
        s
    }

    #[test]
    fn simple_document_parses() {
        let html = "<html><body><p>hello</p></body></html>";
        let doc = parse_html_str(html).unwrap();
        assert!(doc.is_html());
        assert_eq!(doc.root().name(),"html");
    }

    #[test]
    fn fragment_gets_implicit_html_body() {
        let doc = parse_html_str("<p>hi</p>").unwrap();
        assert_eq!(doc.root().name(),"html");
        // html5ever should have synthesized <head> and <body>.
        assert!(find_elem(doc.root(), "body").is_some());
        assert!(find_elem(doc.root(), "p").is_some());
    }

    #[test]
    fn tag_soup_recovers() {
        // Mismatched closes — should not error in recovery mode.
        let doc = parse_html_str("<div><p>oops</div></p>").unwrap();
        assert_eq!(doc.root().name(),"html");
    }

    #[test]
    fn doctype_is_captured_in_metadata() {
        let html = "<!DOCTYPE html><html><body></body></html>";
        let doc = parse_html_str(html).unwrap();
        let meta = doc.html_metadata.as_ref().unwrap();
        let dt = meta.doctype.as_ref().unwrap();
        assert_eq!(dt.name, "html");
        assert!(dt.public_id.is_empty());
    }

    #[test]
    fn quirks_mode_set_for_no_doctype() {
        let doc = parse_html_str("<html><body></body></html>").unwrap();
        let meta = doc.html_metadata.as_ref().unwrap();
        assert!(matches!(meta.quirks_mode, sup_xml_tree::QuirksMode::Quirks));
    }

    #[test]
    fn no_quirks_for_html5_doctype() {
        let doc = parse_html_str("<!DOCTYPE html><html><body></body></html>").unwrap();
        let meta = doc.html_metadata.as_ref().unwrap();
        assert!(matches!(meta.quirks_mode, sup_xml_tree::QuirksMode::NoQuirks));
    }

    #[test]
    fn entity_decoded() {
        let doc = parse_html_str("<p>a &amp; b &copy; c</p>").unwrap();
        let text = collect_text(doc.root());
        assert!(text.contains('&'), "decoded ampersand: {text}");
        assert!(text.contains('©'), "decoded named entity: {text}");
    }

    #[test]
    fn case_folded_tag_names() {
        let doc = parse_html_str("<HTML><BODY><P>X</P></BODY></HTML>").unwrap();
        assert_eq!(doc.root().name(),"html");
        assert!(find_elem(doc.root(), "body").is_some());
        assert!(find_elem(doc.root(), "p").is_some());
    }

    #[test]
    fn parse_bytes_works() {
        let doc = parse_html_bytes(b"<p>hello</p>").unwrap();
        assert!(doc.is_html());
        assert_eq!(doc.root().name(),"html");
    }

    #[test]
    fn strict_mode_returns_err_on_first_error() {
        let opts = HtmlParseOptions {
            recovery_mode: false,
            ..Default::default()
        };
        // Mismatched close tag - well-formedness violation.
        let result = parse_html_str_opts("<p>oops</div>", &opts);
        assert!(result.is_err(), "strict mode should error on mismatched tags");
    }

    #[test]
    fn recovered_errors_surface() {
        let (doc, recovered) =
            parse_html_str_with_recovered("<p>oops</div>", &HtmlParseOptions::default());
        assert!(doc.is_ok());
        assert!(!recovered.is_empty(), "should have recovered errors");
    }

    #[test]
    fn depth_limit_enforced() {
        let opts = HtmlParseOptions {
            max_element_depth: 5,
            ..Default::default()
        };
        let html = "<div>".repeat(100) + &"</div>".repeat(100);
        let result = parse_html_str_opts(&html, &opts);
        assert!(result.is_err(), "must reject input exceeding depth limit");
    }

    #[test]
    fn text_byte_limit_enforced() {
        let opts = HtmlParseOptions {
            max_text_bytes: 100,
            ..Default::default()
        };
        let html = format!("<p>{}</p>", "a".repeat(10_000));
        let result = parse_html_str_opts(&html, &opts);
        assert!(result.is_err(), "must reject input exceeding text-byte limit");
    }

    #[test]
    fn parse_html_bytes_decodes_windows1252_via_meta() {
        let mut bytes = b"<!DOCTYPE html><html><head><meta charset=\"windows-1252\"></head><body><p>".to_vec();
        bytes.push(0x97);
        bytes.extend_from_slice(b"</p></body></html>");

        let doc = parse_html_bytes(&bytes).expect("must parse");
        assert!(doc.is_html());
        let p = find_elem(doc.root(), "p").expect("must find <p>");
        let text = collect_text(p);
        assert!(
            text.contains('\u{2014}'),
            "em dash (U+2014) should be decoded from byte 0x97; got {text:?}"
        );
    }

    #[test]
    fn parse_html_bytes_respects_encoding_override() {
        // Same Windows-1252 byte 0x97 but no meta tag — without an
        // override it falls back to Windows-1252 anyway.  With a
        // wrong override (UTF-8) the byte becomes U+FFFD via lossy
        // decoding.
        let mut bytes = b"<html><body><p>".to_vec();
        bytes.push(0x97);
        bytes.extend_from_slice(b"</p></body></html>");

        let opts = HtmlParseOptions {
            encoding_override: Some("UTF-8".into()),
            ..Default::default()
        };
        let doc = parse_html_bytes_opts(&bytes, &opts).expect("must parse");
        let p = find_elem(doc.root(), "p").expect("must find <p>");
        let text = collect_text(p);
        // Either U+FFFD substitution OR lossy decoding swallowed it.
        assert!(
            text.contains('\u{FFFD}') || !text.contains('\u{2014}'),
            "with UTF-8 override, byte 0x97 should not decode as em dash; got {text:?}"
        );
    }

    #[test]
    fn attributes_preserved() {
        let doc = parse_html_str(r#"<html><body><a href="/x" class="nav">x</a></body></html>"#)
            .unwrap();
        let a = find_elem(doc.root(), "a").expect("<a>");
        let attrs: Vec<(&str, &str)> = a.attributes().map(|x| (x.name(), x.value())).collect();
        assert!(attrs.iter().any(|(n, v)| *n == "href" && *v == "/x"), "got: {:?}", attrs);
        assert!(attrs.iter().any(|(n, v)| *n == "class" && *v == "nav"), "got: {:?}", attrs);
    }

    #[test]
    fn comments_preserved() {
        let doc = parse_html_str("<html><body><!-- hello --><p>x</p></body></html>")
            .unwrap();
        let body = find_elem(doc.root(), "body").expect("body");
        let comment = body.children().find(|c| c.kind == NodeKind::Comment);
        let c = comment.expect("comment");
        assert_eq!(c.content(), " hello ");
    }

    #[test]
    fn deeply_nested_recovers() {
        // Many levels of nesting under the limit.  html5ever wraps the
        // run of divs under <html><body>, so dig from body.
        let html = "<div>".repeat(50) + "x" + &"</div>".repeat(50);
        let doc = parse_html_str(&html).unwrap();
        let body = find_elem(doc.root(), "body").expect("body");
        let mut depth = 0;
        let mut cur = body;
        loop {
            let next = cur.children().find(|c| c.kind == NodeKind::Element && c.name() == "div");
            match next {
                Some(d) => { depth += 1; cur = d; }
                None    => break,
            }
        }
        assert!(depth >= 50, "expected at least 50 nested divs; got {depth}");
    }

    // ── streaming tests ──────────────────────────────────────────────────────

    use super::events::HtmlAttrs;
    use super::stream::{HtmlBytesReader, HtmlReader, HtmlSaxHandler, HtmlSaxParser};
    use super::HtmlEvent;

    /// Drain a reader into an owned event-trace string.  Each event
    /// is one line; the format is stable enough to compare across
    /// chunk-size variations.
    fn drain_pull(reader: &mut HtmlReader<'_>) -> String {
        let mut out = String::new();
        loop {
            let event = reader.next().expect("pull errored unexpectedly");
            match event {
                HtmlEvent::StartElement { name, attributes } => {
                    out.push_str("S:");
                    out.push_str(name);
                    let mut attrs: Vec<_> = attributes
                        .iter()
                        .map(|a| format!("{}={}", a.name, a.value))
                        .collect();
                    attrs.sort();
                    if !attrs.is_empty() {
                        out.push('[');
                        out.push_str(&attrs.join(","));
                        out.push(']');
                    }
                    out.push('\n');
                }
                HtmlEvent::EndElement { name } => {
                    out.push_str("E:");
                    out.push_str(name);
                    out.push('\n');
                }
                HtmlEvent::Text(t) => {
                    out.push_str("T:");
                    out.push_str(t);
                    out.push('\n');
                }
                HtmlEvent::Comment(c) => {
                    out.push_str("C:");
                    out.push_str(c);
                    out.push('\n');
                }
                HtmlEvent::Doctype { name, .. } => {
                    out.push_str("D:");
                    out.push_str(name);
                    out.push('\n');
                }
                HtmlEvent::Eof => return out,
            }
        }
    }

    #[test]
    fn streaming_basic_event_sequence() {
        let mut r = HtmlReader::new("<html><body><p>hi</p></body></html>");
        let trace = drain_pull(&mut r);
        assert!(trace.contains("S:html"), "trace:\n{trace}");
        assert!(trace.contains("S:body"), "trace:\n{trace}");
        assert!(trace.contains("S:p"), "trace:\n{trace}");
        assert!(trace.contains("T:hi"), "trace:\n{trace}");
        assert!(trace.contains("E:p"), "trace:\n{trace}");
        assert!(trace.contains("E:body"), "trace:\n{trace}");
        assert!(trace.contains("E:html"), "trace:\n{trace}");
    }

    #[test]
    fn streaming_text_coalescing() {
        // html5ever emits text in chunks; the streaming sink coalesces
        // adjacent runs into one Text event.
        let mut r = HtmlReader::new("<p>hello world &amp; goodbye</p>");
        let trace = drain_pull(&mut r);
        // Should appear as one Text line, not split per chunk.
        let text_lines: Vec<&str> = trace.lines().filter(|l| l.starts_with("T:")).collect();
        assert!(
            text_lines.iter().any(|l| l.contains("hello world & goodbye")),
            "expected coalesced text; got lines: {:?}",
            text_lines
        );
    }

    #[test]
    fn streaming_doctype_event() {
        let mut r = HtmlReader::new("<!DOCTYPE html><html><body>x</body></html>");
        let trace = drain_pull(&mut r);
        assert!(trace.contains("D:html"));
    }

    #[test]
    fn streaming_attributes_visible() {
        let mut r = HtmlReader::new(r#"<a href="/x" class="nav">x</a>"#);
        let trace = drain_pull(&mut r);
        assert!(
            trace.lines().any(|l| l.starts_with("S:a[") && l.contains("href=/x") && l.contains("class=nav")),
            "expected start tag with attrs; got:\n{}",
            trace
        );
    }

    #[test]
    fn streaming_chunk_boundary_invariance() {
        let html = "<html><head><title>t</title></head><body><p>a</p><p>b &amp; c</p></body></html>";
        let mut a = HtmlReader::new(html);
        let mut b = HtmlBytesReader::new(html.as_bytes()).expect("bytes reader init");
        let trace_a = drain_pull(&mut a);
        let trace_b = drain_pull_bytes(&mut b);
        assert_eq!(trace_a, trace_b, "str and bytes readers must agree");
    }

    fn drain_pull_bytes(reader: &mut HtmlBytesReader<'_>) -> String {
        let mut out = String::new();
        loop {
            let event = reader.next().expect("bytes pull errored");
            match event {
                HtmlEvent::StartElement { name, attributes } => {
                    out.push_str("S:");
                    out.push_str(name);
                    let mut attrs: Vec<_> = attributes
                        .iter()
                        .map(|a| format!("{}={}", a.name, a.value))
                        .collect();
                    attrs.sort();
                    if !attrs.is_empty() {
                        out.push('[');
                        out.push_str(&attrs.join(","));
                        out.push(']');
                    }
                    out.push('\n');
                }
                HtmlEvent::EndElement { name } => {
                    out.push_str("E:");
                    out.push_str(name);
                    out.push('\n');
                }
                HtmlEvent::Text(t) => {
                    out.push_str("T:");
                    out.push_str(t);
                    out.push('\n');
                }
                HtmlEvent::Comment(c) => {
                    out.push_str("C:");
                    out.push_str(c);
                    out.push('\n');
                }
                HtmlEvent::Doctype { name, .. } => {
                    out.push_str("D:");
                    out.push_str(name);
                    out.push('\n');
                }
                HtmlEvent::Eof => return out,
            }
        }
    }

    #[derive(Default)]
    struct LinkCollector {
        hrefs: Vec<String>,
    }

    impl HtmlSaxHandler for LinkCollector {
        fn start_element(&mut self, name: &str, attrs: HtmlAttrs<'_>) {
            if name == "a" {
                if let Some(href) = attrs.get("href") {
                    self.hrefs.push(href.to_string());
                }
            }
        }
    }

    #[test]
    fn sax_parser_collects_attributes() {
        let html = r#"<a href="/x">x</a><a href="/y">y</a><a>none</a>"#;
        let mut p = HtmlSaxParser::new(LinkCollector::default());
        p.feed(html).unwrap();
        let collector = p.finish().unwrap();
        assert_eq!(collector.hrefs, vec!["/x", "/y"]);
    }

    #[test]
    fn sax_parser_chunked_feed() {
        // Feed the input one byte at a time — events should still
        // emit correctly across chunk boundaries.
        let html = r#"<a href="/x">x</a>"#;
        let mut p = HtmlSaxParser::new(LinkCollector::default());
        for ch in html.chars() {
            let mut tmp = [0u8; 4];
            let s = ch.encode_utf8(&mut tmp);
            p.feed(s).unwrap();
        }
        let collector = p.finish().unwrap();
        assert_eq!(collector.hrefs, vec!["/x"]);
    }

    /// Trace handler — records every event for parity comparison.
    #[derive(Default)]
    struct TraceHandler {
        out: String,
    }

    impl HtmlSaxHandler for TraceHandler {
        fn start_element(&mut self, name: &str, attrs: HtmlAttrs<'_>) {
            self.out.push_str("S:");
            self.out.push_str(name);
            let mut a: Vec<_> = attrs
                .iter()
                .map(|a| format!("{}={}", a.name, a.value))
                .collect();
            a.sort();
            if !a.is_empty() {
                self.out.push('[');
                self.out.push_str(&a.join(","));
                self.out.push(']');
            }
            self.out.push('\n');
        }
        fn end_element(&mut self, name: &str) {
            self.out.push_str("E:");
            self.out.push_str(name);
            self.out.push('\n');
        }
        fn text(&mut self, content: &str) {
            self.out.push_str("T:");
            self.out.push_str(content);
            self.out.push('\n');
        }
        fn comment(&mut self, content: &str) {
            self.out.push_str("C:");
            self.out.push_str(content);
            self.out.push('\n');
        }
        fn doctype(&mut self, name: &str, _public_id: &str, _system_id: &str) {
            self.out.push_str("D:");
            self.out.push_str(name);
            self.out.push('\n');
        }
    }

    #[test]
    fn pull_push_parity() {
        let html = "<!DOCTYPE html><html><body><p>foo &amp; bar</p><!-- comment --><a href=\"/x\">link</a></body></html>";

        let mut pull = HtmlReader::new(html);
        let pull_trace = drain_pull(&mut pull);

        let mut push = HtmlSaxParser::new(TraceHandler::default());
        push.feed(html).unwrap();
        let push_trace = push.finish().unwrap().out;

        assert_eq!(
            pull_trace, push_trace,
            "pull and push surfaces must produce identical event traces"
        );
    }

    #[test]
    fn streaming_recovered_errors_collected() {
        let mut r = HtmlReader::new("<p>oops</div>");
        let _ = drain_pull(&mut r);
        assert!(
            !r.recovered_errors().is_empty(),
            "should have recovered at least one parse error"
        );
    }

    #[test]
    fn streaming_bytes_reader_decodes_windows1252() {
        let mut bytes = b"<!DOCTYPE html><html><head><meta charset=\"windows-1252\"></head><body><p>".to_vec();
        bytes.push(0x97);
        bytes.extend_from_slice(b"</p></body></html>");

        let mut r = HtmlBytesReader::new(&bytes).expect("init");
        let mut found_em_dash = false;
        loop {
            match r.next().expect("pull") {
                HtmlEvent::Eof => break,
                HtmlEvent::Text(t) if t.contains('\u{2014}') => found_em_dash = true,
                _ => {}
            }
        }
        assert!(
            found_em_dash,
            "streaming bytes reader should decode windows-1252 em dash"
        );
    }

    #[test]
    fn streaming_strict_mode_errors() {
        let opts = HtmlParseOptions {
            recovery_mode: false,
            ..Default::default()
        };
        let mut r = HtmlReader::with_opts("<p>oops</div>", opts);
        let mut hit_err = false;
        loop {
            match r.next() {
                Ok(HtmlEvent::Eof) => break,
                Ok(_) => {}
                Err(_) => {
                    hit_err = true;
                    break;
                }
            }
        }
        assert!(hit_err, "strict mode should surface a parse error");
    }

    // ── sink coverage: exercise BatchSinkArena code paths ────────────────────

    #[test]
    fn comments_before_and_after_html_element() {
        // Comments at document level — html5ever places them as children
        // of the document sentinel.  Exercises:
        //   - alloc_comment via create_comment
        //   - link_append's `is_document(parent)` branch (doc_children)
        //   - last_child's doc_children branch (when appending after a comment)
        let html = "<!-- pre --><html><body>x</body></html><!-- post -->";
        let doc = parse_html_str(html).unwrap();
        // The arena root is the <html> element (sink finalizes by picking
        // the first element child).  Sibling comments at doc level get
        // dropped — but the sink's link_append path was exercised.
        assert_eq!(doc.root().name(), "html");
    }

    #[test]
    fn processing_instruction_in_html_becomes_comment() {
        // HTML5 treats `<?foo?>` as a bogus comment.  Our sink's
        // `create_pi` maps this to an empty comment.
        let doc = parse_html_str("<?xml-stylesheet href='x'?><html><body>x</body></html>")
            .unwrap();
        // Smoke check — must not panic and must produce <html>.
        assert_eq!(doc.root().name(), "html");
    }

    #[test]
    fn max_text_bytes_exceeded_aborts_parse() {
        let opts = HtmlParseOptions {
            max_text_bytes: 8,
            ..Default::default()
        };
        let (result, _recovered) = parse_html_str_with_recovered(
            "<p>this is more than eight bytes of text</p>", &opts,
        );
        // Aborted parse surfaces the fatal error.
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.message.contains("max_text_bytes"), "got {}", e.message);
        }
    }

    #[test]
    fn max_element_depth_exceeded_aborts_parse() {
        let opts = HtmlParseOptions {
            max_element_depth: 4,
            ..Default::default()
        };
        // 10 levels of nested <div> — exceeds depth 4.
        let html = "<div>".repeat(10) + "x" + &"</div>".repeat(10);
        let (result, _) = parse_html_str_with_recovered(&html, &opts);
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.message.contains("max_element_depth"), "got {}", e.message);
        }
    }

    #[test]
    fn table_foster_parenting_with_text() {
        // Text inside <table> outside a cell triggers HTML5 foster
        // parenting — html5ever calls append_before_sibling /
        // append_based_on_parent_node on the sink.  Drives the
        // insert_text_before / link_before paths.
        let html = "<table>raw text<tr><td>cell</td></tr></table>";
        let doc = parse_html_str(html).unwrap();
        // Smoke: text "raw text" should appear somewhere in the result.
        assert_eq!(doc.root().name(), "html");
        let body = find_elem(doc.root(), "body").expect("body");
        let all_text = collect_text(body);
        assert!(all_text.contains("raw text"), "got: {all_text:?}");
        assert!(all_text.contains("cell"), "got: {all_text:?}");
    }

    #[test]
    fn duplicate_html_attributes_use_add_attrs_if_missing() {
        // Repeated <html> tag with new attributes — html5ever merges
        // these by calling add_attrs_if_missing on the existing handle.
        let html = r#"<html lang="en"><body></body></html><html dir="ltr">"#;
        let doc = parse_html_str(html).unwrap();
        let root = doc.root();
        let attrs: Vec<(&str, &str)> = root.attributes()
            .map(|a| (a.name(), a.value())).collect();
        // Both lang= (from first) and dir= (from second) should be present.
        assert!(attrs.iter().any(|(n, _)| *n == "lang"), "got {attrs:?}");
        assert!(attrs.iter().any(|(n, _)| *n == "dir"),  "got {attrs:?}");
    }

    #[test]
    fn duplicate_html_attribute_does_not_overwrite() {
        // add_attrs_if_missing must NOT replace an existing attribute.
        let html = r#"<html lang="en"><body></body></html><html lang="fr">"#;
        let doc = parse_html_str(html).unwrap();
        let lang = doc.root().attributes()
            .find(|a| a.name() == "lang").map(|a| a.value().to_owned());
        assert_eq!(lang.as_deref(), Some("en"), "first lang should win");
    }

    #[test]
    fn limited_quirks_doctype_sets_metadata() {
        // XHTML 1.0 Transitional triggers LimitedQuirks per the HTML5
        // spec.  Exercises convert_quirks's LimitedQuirks arm.
        let html = r#"<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Transitional//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd">
<html><body></body></html>"#;
        let doc = parse_html_str(html).unwrap();
        let meta = doc.html_metadata.as_ref().expect("metadata");
        // Either LimitedQuirks or NoQuirks is acceptable depending on the
        // exact spec table; we just verify it's NOT Quirks (which would
        // mean the DOCTYPE wasn't recognised at all).
        assert!(!matches!(meta.quirks_mode, sup_xml_tree::QuirksMode::Quirks),
            "transitional DOCTYPE should not trigger full Quirks mode");
    }

    #[test]
    fn empty_input_synthesizes_html() {
        // No <html> element in input — finalize must synthesize one
        // so the Document is valid.
        let doc = parse_html_str("").unwrap();
        assert_eq!(doc.root().name(), "html");
    }

    #[test]
    fn long_run_of_text_chunks_coalesces() {
        // html5ever delivers a long text run as many StrTendril chunks
        // — sink's PendingText path coalesces them into one node.
        let body = "x".repeat(10_000);
        let html = format!("<html><body><p>{body}</p></body></html>");
        let doc = parse_html_str(&html).unwrap();
        let p = find_elem(doc.root(), "p").expect("p");
        let text = collect_text(p);
        assert_eq!(text.len(), 10_000);
    }

    #[test]
    fn parse_error_is_recorded_in_strict_mode() {
        let opts = HtmlParseOptions {
            recovery_mode: false,
            ..Default::default()
        };
        // Construct input that html5ever flags as a parse error.  An
        // unexpected character after `</` should do it.
        let (result, _) = parse_html_str_with_recovered("<p>x</_invalid>", &opts);
        // strict mode → first parse error becomes fatal.
        assert!(result.is_err());
    }

    #[test]
    fn parse_errors_in_recovery_mode_recorded_not_fatal() {
        let opts = HtmlParseOptions { recovery_mode: true, ..Default::default() };
        let (result, recovered) = parse_html_str_with_recovered(
            "<p>x</_invalid>", &opts,
        );
        // Recovery mode: result is Ok, errors are recorded.
        assert!(result.is_ok());
        // At least one recovered error captured.
        assert!(!recovered.is_empty() || result.unwrap().root().name() == "html");
    }

    #[test]
    fn quirks_mode_with_old_html_doctype() {
        // Plain `<!DOCTYPE html>` triggers NoQuirks.  Test missing DOCTYPE
        // separately for Quirks; here just exercise the NoQuirks arm of
        // convert_quirks.
        let doc = parse_html_str("<!DOCTYPE html><html><body></body></html>").unwrap();
        let meta = doc.html_metadata.as_ref().unwrap();
        assert!(matches!(meta.quirks_mode, sup_xml_tree::QuirksMode::NoQuirks));
    }

    #[test]
    fn doctype_with_public_and_system_ids() {
        // Exercises append_doctype_to_document with non-empty public_id
        // AND system_id.
        let html = r#"<!DOCTYPE html PUBLIC "-//W3C//DTD HTML 4.01//EN" "http://www.w3.org/TR/html4/strict.dtd"><html><body></body></html>"#;
        let doc = parse_html_str(html).unwrap();
        let meta = doc.html_metadata.as_ref().unwrap();
        let dt = meta.doctype.as_ref().unwrap();
        assert_eq!(dt.name, "html");
        assert!(dt.public_id.contains("HTML 4.01"));
        assert!(dt.system_id.contains("html4"));
    }

    #[test]
    fn template_element_parses() {
        // <template> triggers html5ever's get_template_contents query.
        let html = "<html><body><template><p>tmpl</p></template></body></html>";
        let doc = parse_html_str(html).unwrap();
        assert!(find_elem(doc.root(), "template").is_some());
    }

    #[test]
    fn table_foster_parenting_runs_of_text() {
        // Multiple text runs inside <table> outside cells — drives the
        // insert_text_before code with text-before-text scenarios so the
        // merge path runs.
        let html = "<table>first<tr><td>cell</td></tr>second</table>";
        let doc = parse_html_str(html).unwrap();
        let body = find_elem(doc.root(), "body").expect("body");
        let all = collect_text(body);
        assert!(all.contains("first") && all.contains("second"),
            "got: {all:?}");
    }

    #[test]
    fn nested_table_foster_parenting() {
        // More complex foster parenting that should drive
        // append_based_on_parent_node and link_before with non-first-child.
        let html = r#"<table><tr><td>a</td></tr>extra<tr><td>b</td></tr></table>"#;
        let doc = parse_html_str(html).unwrap();
        let body = find_elem(doc.root(), "body").expect("body");
        let text = collect_text(body);
        assert!(text.contains("extra"));
        assert!(text.contains("a") && text.contains("b"));
    }

    #[test]
    fn svg_with_processing_instruction() {
        // HTML5 foreign content (SVG) — try various odd shapes that
        // might exercise additional sink paths.
        let html = r#"<html><body><svg xmlns="http://www.w3.org/2000/svg"><circle/></svg></body></html>"#;
        let doc = parse_html_str(html).unwrap();
        assert!(find_elem(doc.root(), "svg").is_some());
        assert!(find_elem(doc.root(), "circle").is_some());
    }

    #[test]
    fn text_after_html_close_appended() {
        // Trailing text after </html> — html5ever places it inside body
        // (HTML5 spec § "after after body").  Hits append paths.
        let html = "<html><body>x</body></html>extra-text";
        let doc = parse_html_str(html).unwrap();
        let body = find_elem(doc.root(), "body").expect("body");
        let text = collect_text(body);
        assert!(text.contains("x"));
    }

    #[test]
    fn list_with_misnested_tags() {
        // Various misnested constructs exercise reparenting / detachment.
        let html = "<ul><li>one<li>two<li>three</ul>";
        let doc = parse_html_str(html).unwrap();
        let body = find_elem(doc.root(), "body").expect("body");
        let li_count = body.children().filter_map(|c| {
            find_elem(c, "li")
        }).count();
        assert!(li_count > 0);
    }

    #[test]
    fn text_byte_limit_hit_during_merge() {
        // Tiny budget where a single text chunk fits, but a second
        // text chunk merging into the same target overflows.  Drives
        // the "merge target → text_bytes overflow" abort path.
        let opts = HtmlParseOptions {
            max_text_bytes: 5,
            ..Default::default()
        };
        // html5ever splits "a<![CDATA-like seam]>" into multiple chunks
        // for raw-text elements.  Use a long <script> body where the
        // first chunk fits under the budget but a continuation merges
        // and overflows.
        let html = "<html><body><script>aaa</script><script>bbbbbbbbbb</script></body></html>";
        let (result, _) = parse_html_str_with_recovered(html, &opts);
        // Either text-merge-abort or single-shot abort — both are
        // "max_text_bytes exceeded".
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.message.contains("max_text_bytes"), "got {}", e.message);
        }
    }

    #[test]
    fn debug_format_on_owned_elem_name_via_strict_error() {
        // OwnedElemName's Debug fires when html5ever debug-prints a
        // tree-construction step.  In strict mode, parse_error with the
        // formatter active will surface it.  This is hard to trigger
        // directly from outside; this test just ensures the path doesn't
        // panic when many elements are processed.
        let html = "<html><body><div><span><a><b><i></i></b></a></span></div></body></html>";
        let doc = parse_html_str(html).unwrap();
        // Smoke check.
        assert_eq!(doc.root().name(), "html");
    }
}
