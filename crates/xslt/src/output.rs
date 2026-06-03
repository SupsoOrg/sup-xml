//! Output serialisers — turn a [`ResultTree`] into bytes per
//! `xsl:output`'s method (XML / HTML / text).
//!
//! XSLT 1.0 §16 specifies three output methods.  The differences:
//!
//! * **XML** (the default): standard XML serialisation.  Empty
//!   elements use self-closing syntax (`<br/>`).  Standard
//!   `<`/`>`/`&` escaping in text; also `"` inside attribute
//!   values.  XML declaration is emitted iff `omit-xml-declaration`
//!   is `no` or the version is non-1.0.
//! * **HTML**: HTML5-ish.  Void elements (`<br>`, `<img>`, etc.)
//!   emit no closing tag *and* no self-closing slash.  No
//!   escaping inside `<script>` / `<style>`.  Attribute minimisation
//!   is allowed (e.g. `selected` instead of `selected="selected"`)
//!   but we keep them fully written for safety.
//! * **Text**: concatenate every text node, no markup, no
//!   escaping.

use std::fmt::Write;

use crate::ast::OutputSpec;
use crate::error::XsltError;
use crate::result_tree::{ResultNode, ResultTree};

/// Pick the serialisation method based on the stylesheet's
/// effective `<xsl:output>` settings, with the XSLT 1.0 default
/// fallback: if no method was specified and the result tree's
/// first child is `<html>`, use HTML; otherwise XML.  (Real
/// libxslt does this fallback dance too.)
fn effective_method(tree: &ResultTree) -> &str {
    if let Some(m) = tree.output.method.as_deref() { return m; }
    if let Some(ResultNode::Element { name, .. }) = tree.children.iter()
        .find(|n| matches!(n, ResultNode::Element { .. }))
    {
        if name.local.eq_ignore_ascii_case("html") && name.uri.is_empty() {
            return "html";
        }
    }
    "xml"
}

impl ResultTree {
    /// Serialise the result tree to a string using the effective
    /// output method.  XSLT 1.0 §16 — method = xml | html | text.
    pub fn to_string(&self) -> Result<String, XsltError> {
        match effective_method(self) {
            "html" => Ok(serialize_html(self)),
            "text" => Ok(serialize_text(self)),
            _      => Ok(serialize_xml(self)),
        }
    }

    /// Write the serialised result to any [`io::Write`] sink.
    pub fn write_to(&self, w: &mut dyn std::io::Write) -> Result<(), XsltError> {
        let s = self.to_string()?;
        w.write_all(s.as_bytes())
            .map_err(|e| XsltError::InvalidStylesheet(format!("write failed: {e}")))
    }
}

// ── XML serialiser ────────────────────────────────────────────────

pub fn serialize_xml(tree: &ResultTree) -> String {
    let mut out = String::new();
    if should_emit_xml_decl(&tree.output) {
        let _ = write!(out, r#"<?xml version="{}" encoding="{}""#,
            tree.output.version.as_deref().unwrap_or("1.0"),
            tree.output.encoding.as_deref().unwrap_or("UTF-8"),
        );
        if let Some(s) = tree.output.standalone {
            let _ = write!(out, r#" standalone="{}""#, if s { "yes" } else { "no" });
        }
        out.push_str("?>\n");
    }
    if let Some(dt_sys) = tree.output.doctype_system.as_deref() {
        if let Some(root) = first_element_name(&tree.children) {
            if let Some(pubid) = tree.output.doctype_public.as_deref() {
                let _ = writeln!(out, r#"<!DOCTYPE {root} PUBLIC "{pubid}" "{dt_sys}">"#);
            } else {
                let _ = writeln!(out, r#"<!DOCTYPE {root} SYSTEM "{dt_sys}">"#);
            }
        }
    }
    for child in &tree.children {
        serialize_xml_node(child, &mut out, &tree.output, "", &tree.character_map);
    }
    out
}

fn should_emit_xml_decl(output: &OutputSpec) -> bool {
    // libxslt default: emit for xml method unless omit=yes.
    !output.omit_xml_declaration.unwrap_or(false)
}

fn first_element_name(nodes: &[ResultNode]) -> Option<String> {
    nodes.iter().find_map(|n| match n {
        ResultNode::Element { name, .. } => Some(name.to_qname_string()),
        _ => None,
    })
}

/// Serialize one result-tree node.
///
/// `parent_default_ns` is the URI bound to the default namespace in
/// the surrounding scope (`""` if none) — used to suppress redundant
/// `xmlns=""` declarations on elements whose surrounding scope
/// already has no default namespace.
fn serialize_xml_node(
    node:        &ResultNode,
    out:         &mut String,
    opts:        &OutputSpec,
    parent_default_ns: &str,
    cmap:        &[(char, String)],
) {
    let xml_11   = opts.version.as_deref() == Some("1.1");
    let enc_cap  = encoding_capability(opts.encoding.as_deref());
    match node {
        ResultNode::Element { name, namespaces, attributes, children } => {
            let q = name.to_qname_string();
            out.push('<');
            out.push_str(&q);
            // Compute the default namespace this element actually
            // contributes (used both to suppress redundant decls here
            // and to thread down to children).  When the element has
            // no explicit default-namespace binding in `namespaces`,
            // it inherits the parent's.
            let mut child_default_ns: &str = parent_default_ns;
            for (prefix, uri) in namespaces {
                match prefix {
                    // The `xml` prefix is bound by the XML spec itself
                    // to the XML namespace URI; redeclaration is
                    // forbidden by XML Namespaces § 3 ("Prefix `xml`
                    // is by definition bound to ..."). Suppress it
                    // here so result trees that carry the binding
                    // through (e.g. `xml:space`-bearing elements)
                    // don't emit a redundant decl.
                    Some(p) if p == "xml"
                        && uri == "http://www.w3.org/XML/1998/namespace" => {}
                    Some(p) => { let _ = write!(out, r#" xmlns:{p}="{}""#, escape_attr(uri, xml_11, enc_cap)); }
                    None => {
                        // Suppress a default-namespace declaration that
                        // would have no observable effect — same URI as
                        // the surrounding scope already has bound.
                        // Notably an `xmlns=""` undeclaration when the
                        // surrounding default is already empty.
                        if uri != parent_default_ns {
                            let _ = write!(out, r#" xmlns="{}""#, escape_attr(uri, xml_11, enc_cap));
                        }
                        child_default_ns = uri.as_str();
                    }
                }
            }
            for (aname, value) in attributes {
                let _ = write!(out, r#" {}="{}""#,
                    aname.to_qname_string(),
                    escape_attr_with_map(value, xml_11, enc_cap, cmap));
            }
            if children.is_empty() {
                out.push_str("/>");
                return;
            }
            out.push('>');
            // CDATA-section elements: text children of these are
            // wrapped in <![CDATA[...]]> rather than escaped.
            let is_cdata = opts.cdata_section_elements.iter()
                .any(|q| q.uri == name.uri && q.local == name.local);
            for c in children {
                if is_cdata {
                    if let ResultNode::Text { content, .. } = c {
                        out.push_str("<![CDATA[");
                        out.push_str(&content.replace("]]>", "]]]]><![CDATA[>"));
                        out.push_str("]]>");
                        continue;
                    }
                }
                serialize_xml_node(c, out, opts, child_default_ns, cmap);
            }
            out.push_str("</");
            out.push_str(&q);
            out.push('>');
        }
        ResultNode::Text { content, dose } => {
            if *dose {
                out.push_str(content);
            } else {
                out.push_str(&escape_text_with_map(content, xml_11, enc_cap, cmap));
            }
        }
        ResultNode::Comment(s) => {
            let _ = write!(out, "<!--{}-->", s);
        }
        ResultNode::ProcessingInstruction { target, data } => {
            if data.is_empty() {
                let _ = write!(out, "<?{target}?>");
            } else {
                let _ = write!(out, "<?{target} {data}?>");
            }
        }
        // A parentless attribute has no serialization in element
        // content (XSLT consumes it via copy-of / apply-templates);
        // emit nothing rather than malformed output.
        ResultNode::Attribute { .. } => {}
    }
}

/// XML 1.1 § 2.11 restricted chars, plus NEL (#x85) and LSEP
/// (#x2028).  The restricted set MUST be NCR-escaped in serialized
/// output per the XML 1.1 spec; NEL and LSEP are technically
/// allowed unescaped but the XML 1.1 input parser normalises both
/// to LF, so a round-trip needs them as NCRs.  Tab/LF/CR are not in
/// this set — `escape_attr` handles those independently.
#[inline]
fn xml_11_must_escape(c: char) -> bool {
    matches!(c as u32,
        0x01..=0x08 | 0x0B..=0x0C | 0x0E..=0x1F |
        0x7F..=0x84 | 0x85 | 0x86..=0x9F |
        0x2028
    )
}

/// Coverage of a named output encoding: the largest Unicode codepoint
/// that the encoding can represent directly.  Codepoints above this
/// must be emitted as numeric character references (XSLT 1.0 §16
/// "output that uses a character encoding cannot directly represent
/// ... must escape").  `None` means UTF-8 / UTF-16 / unknown — every
/// codepoint passes through unescaped.
fn encoding_capability(enc: Option<&str>) -> Option<u32> {
    let name = enc.unwrap_or("UTF-8").to_ascii_lowercase();
    let norm: String = name.chars().filter(|c| !matches!(c, '-' | '_' | ' ')).collect();
    match norm.as_str() {
        // 7-bit ASCII — only codepoints ≤ 0x7F are representable.
        "ascii" | "usascii" | "iso646us" => Some(0x7F),
        // Single-byte ISO/Windows family — represent ≤ 0xFF (the high
        // half maps to a code-page-specific character, but a numeric
        // reference is always safe and round-trippable).
        "iso88591" | "latin1" | "l1" | "cp1252" | "windows1252" => Some(0xFF),
        // Multi-byte encodings that cover the full BMP and beyond
        // (UTF-8, UTF-16, UTF-32) — no escape needed for content
        // characters at all.
        _ => None,
    }
}

#[inline]
fn must_ncr_escape(c: char, enc_cap: Option<u32>) -> bool {
    matches!(enc_cap, Some(max) if c as u32 > max)
}

fn escape_text(s: &str, xml_11: bool, enc_cap: Option<u32>) -> String {
    escape_text_with_map(s, xml_11, enc_cap, &[])
}

fn escape_attr(s: &str, xml_11: bool, enc_cap: Option<u32>) -> String {
    escape_attr_with_map(s, xml_11, enc_cap, &[])
}

/// Look up `c` in the (small) character-map list.  Linear scan;
/// the map sizes encountered in XSLT 2.0 stylesheets are tiny
/// (typically <10 entries) and the lookup happens once per
/// character of serialized output.
#[inline]
fn cmap_lookup<'a>(c: char, cmap: &'a [(char, String)]) -> Option<&'a str> {
    for (k, v) in cmap {
        if *k == c { return Some(v.as_str()); }
    }
    None
}

fn escape_text_with_map(
    s: &str,
    xml_11: bool,
    enc_cap: Option<u32>,
    cmap: &[(char, String)],
) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if let Some(replacement) = cmap_lookup(c, cmap) {
            out.push_str(replacement);
            continue;
        }
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            // Literal `\r` would be rewritten to `\n` by the receiving
            // parser's XML § 2.11 end-of-line normalization, so always
            // escape it as a character reference for round-trip.
            '\r' => out.push_str("&#xD;"),
            c if xml_11 && xml_11_must_escape(c) => {
                let _ = write!(out, "&#{};", c as u32);
            }
            c if must_ncr_escape(c, enc_cap) => {
                let _ = write!(out, "&#{};", c as u32);
            }
            _   => out.push(c),
        }
    }
    out
}

fn escape_attr_with_map(
    s: &str,
    xml_11: bool,
    enc_cap: Option<u32>,
    cmap: &[(char, String)],
) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if let Some(replacement) = cmap_lookup(c, cmap) {
            out.push_str(replacement);
            continue;
        }
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c if xml_11 && xml_11_must_escape(c) => {
                let _ = write!(out, "&#{};", c as u32);
            }
            c if must_ncr_escape(c, enc_cap) => {
                let _ = write!(out, "&#{};", c as u32);
            }
            _   => out.push(c),
        }
    }
    out
}

// ── HTML serialiser ───────────────────────────────────────────────

/// HTML5 void elements — emitted without a closing tag and
/// without `/>`.  The list is from the HTML5 spec § "Void
/// elements"; lowercase canonical form.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input",
    "link", "meta", "param", "source", "track", "wbr",
];

/// Elements whose text content must NOT be escaped (XSLT 1.0 §16
/// HTML output method).
const RAW_TEXT_ELEMENTS: &[&str] = &["script", "style"];

pub fn serialize_html(tree: &ResultTree) -> String {
    let mut out = String::new();
    if let Some(dt_sys) = tree.output.doctype_system.as_deref() {
        if let Some(root) = first_element_name(&tree.children) {
            if let Some(pubid) = tree.output.doctype_public.as_deref() {
                let _ = writeln!(out, r#"<!DOCTYPE {root} PUBLIC "{pubid}" "{dt_sys}">"#);
            } else {
                let _ = writeln!(out, r#"<!DOCTYPE {root} SYSTEM "{dt_sys}">"#);
            }
        }
    } else if let Some(pubid) = tree.output.doctype_public.as_deref() {
        if let Some(root) = first_element_name(&tree.children) {
            let _ = writeln!(out, r#"<!DOCTYPE {root} PUBLIC "{pubid}">"#);
        }
    }
    for c in &tree.children { serialize_html_node(c, &mut out); }
    out
}

fn serialize_html_node(node: &ResultNode, out: &mut String) {
    match node {
        ResultNode::Element { name, namespaces, attributes, children } => {
            let local_lc = name.local.to_lowercase();
            let q = name.to_qname_string();
            out.push('<');
            out.push_str(&q);
            for (prefix, uri) in namespaces {
                match prefix {
                    Some(p) => { let _ = write!(out, r#" xmlns:{p}="{}""#, escape_attr(uri, false, None)); }
                    None    => { let _ = write!(out, r#" xmlns="{}""#, escape_attr(uri, false, None)); }
                }
            }
            for (aname, value) in attributes {
                let _ = write!(out, r#" {}="{}""#,
                    aname.to_qname_string(), escape_attr(value, false, None));
            }
            // Void elements: close with `>`, no children, no closing tag.
            if name.uri.is_empty() && VOID_ELEMENTS.iter().any(|v| *v == local_lc) {
                out.push('>');
                return;
            }
            out.push('>');
            let raw_text = name.uri.is_empty()
                && RAW_TEXT_ELEMENTS.iter().any(|v| *v == local_lc);
            for c in children {
                if raw_text {
                    if let ResultNode::Text { content, .. } = c {
                        out.push_str(content);
                        continue;
                    }
                }
                serialize_html_node(c, out);
            }
            out.push_str("</");
            out.push_str(&q);
            out.push('>');
        }
        ResultNode::Text { content, dose } => {
            if *dose { out.push_str(content); }
            else     { out.push_str(&escape_text(content, false, None)); }
        }
        ResultNode::Comment(s) => {
            let _ = write!(out, "<!--{s}-->");
        }
        ResultNode::ProcessingInstruction { target, data } => {
            // HTML5 doesn't really have PIs but XSLT spec says
            // emit them as `<?target data>` (no `?>`).
            if data.is_empty() {
                let _ = write!(out, "<?{target}>");
            } else {
                let _ = write!(out, "<?{target} {data}>");
            }
        }
        // Parentless attribute — no element-content serialization.
        ResultNode::Attribute { .. } => {}
    }
}

// ── text serialiser ───────────────────────────────────────────────

pub fn serialize_text(tree: &ResultTree) -> String {
    let mut out = String::new();
    for c in &tree.children { append_text(c, &mut out); }
    out
}

fn append_text(node: &ResultNode, out: &mut String) {
    match node {
        ResultNode::Text { content, .. } => out.push_str(content),
        ResultNode::Element { children, .. } => {
            for c in children { append_text(c, out); }
        }
        // Comments + PIs are stripped entirely in text output.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::QName;

    fn elt(name: &str, children: Vec<ResultNode>) -> ResultNode {
        ResultNode::Element {
            name: QName { prefix: None, local: name.into(), uri: String::new() },
            namespaces: Vec::new(),
            attributes: Vec::new(),
            children,
        }
    }

    fn text(s: &str) -> ResultNode {
        ResultNode::Text { content: s.into(), dose: false }
    }

    fn tree_of(nodes: Vec<ResultNode>, method: Option<&str>) -> ResultTree {
        let mut spec = OutputSpec::default();
        spec.method = method.map(str::to_string);
        spec.omit_xml_declaration = Some(true); // simplify tests
        ResultTree { children: nodes, output: spec, character_map: Vec::new(), secondary: Vec::new() }
    }

    // ── XML ─────────────────────────────────────────────────

    #[test]
    fn xml_empty_element_self_closes() {
        let t = tree_of(vec![elt("br", vec![])], None);
        assert_eq!(t.to_string().unwrap(), "<br/>");
    }

    #[test]
    fn xml_escapes_text_specials() {
        let t = tree_of(
            vec![elt("p", vec![text("a < b && c > d")])],
            None,
        );
        assert_eq!(t.to_string().unwrap(), "<p>a &lt; b &amp;&amp; c &gt; d</p>");
    }

    #[test]
    fn xml_escapes_attr_quote_and_specials() {
        let t = tree_of(vec![ResultNode::Element {
            name: QName { prefix: None, local: "a".into(), uri: String::new() },
            namespaces: Vec::new(),
            attributes: vec![(
                QName { prefix: None, local: "href".into(), uri: String::new() },
                r#"x"&y<z"#.to_string(),
            )],
            children: Vec::new(),
        }], None);
        assert_eq!(t.to_string().unwrap(), r#"<a href="x&quot;&amp;y&lt;z"/>"#);
    }

    #[test]
    fn xml_text_dose_skips_escape() {
        let t = tree_of(
            vec![elt("p", vec![ResultNode::Text { content: "<raw/>".into(), dose: true }])],
            None,
        );
        assert_eq!(t.to_string().unwrap(), "<p><raw/></p>");
    }

    // ── HTML ────────────────────────────────────────────────

    #[test]
    fn html_void_elements_get_no_close_no_slash() {
        let t = tree_of(vec![
            elt("html", vec![
                elt("head", vec![ elt("meta", vec![]) ]),
                elt("body", vec![ elt("br", vec![]), elt("img", vec![]) ]),
            ]),
        ], Some("html"));
        let s = t.to_string().unwrap();
        assert!(s.contains("<meta>"),  "got: {s}");
        assert!(s.contains("<br>"),    "got: {s}");
        assert!(s.contains("<img>"),   "got: {s}");
        assert!(!s.contains("<br/>"),  "got: {s}");
        assert!(!s.contains("<meta/>"), "got: {s}");
    }

    #[test]
    fn html_script_content_not_escaped() {
        let t = tree_of(vec![ elt("script", vec![ text("if (a < b) alert('x');") ]) ],
            Some("html"));
        let s = t.to_string().unwrap();
        assert!(s.contains("if (a < b)"), "script body should be raw: {s}");
    }

    #[test]
    fn html_default_detected_by_root_html_element() {
        // No method= set, root is <html> → HTML serialiser.
        let t = tree_of(vec![ elt("html", vec![ elt("br", vec![]) ]) ], None);
        let s = t.to_string().unwrap();
        // HTML default emits `<br>` not `<br/>`.
        assert!(s.contains("<br>"), "got: {s}");
        assert!(!s.contains("<br/>"));
    }

    // ── text ────────────────────────────────────────────────

    #[test]
    fn text_strips_markup() {
        let t = tree_of(vec![ elt("p", vec![
            text("Hello, "),
            elt("b", vec![text("world")]),
            text("!"),
        ]) ], Some("text"));
        assert_eq!(t.to_string().unwrap(), "Hello, world!");
    }

    #[test]
    fn text_strips_comments_and_pis() {
        let t = tree_of(vec![
            ResultNode::Comment("ignored".into()),
            elt("p", vec![text("kept")]),
            ResultNode::ProcessingInstruction { target: "pi".into(), data: "ignored".into() },
        ], Some("text"));
        assert_eq!(t.to_string().unwrap(), "kept");
    }

    // ── write_to ────────────────────────────────────────────────

    #[test]
    fn write_to_io_writer() {
        let t = tree_of(vec![elt("r", vec![text("hi")])], None);
        let mut buf = Vec::<u8>::new();
        t.write_to(&mut buf).unwrap();
        assert_eq!(buf, b"<r>hi</r>");
    }

    // ── XML declaration ─────────────────────────────────────────

    #[test]
    fn xml_decl_emitted_when_not_omitted() {
        let mut spec = OutputSpec::default();
        spec.omit_xml_declaration = Some(false);
        spec.version = Some("1.0".into());
        spec.encoding = Some("UTF-8".into());
        let t = ResultTree {
            children: vec![elt("r", vec![])],
            output: spec,
            character_map: Vec::new(),
            secondary: Vec::new(),
        };
        let s = t.to_string().unwrap();
        assert!(s.starts_with(r#"<?xml version="1.0" encoding="UTF-8"?>"#), "got: {s}");
    }

    #[test]
    fn xml_decl_emits_standalone_yes() {
        let mut spec = OutputSpec::default();
        spec.omit_xml_declaration = Some(false);
        spec.standalone = Some(true);
        let t = ResultTree {
            children: vec![elt("r", vec![])],
            output: spec,
            character_map: Vec::new(),
            secondary: Vec::new(),
        };
        let s = t.to_string().unwrap();
        assert!(s.contains(r#"standalone="yes""#), "got: {s}");
    }

    #[test]
    fn xml_decl_emits_standalone_no() {
        let mut spec = OutputSpec::default();
        spec.omit_xml_declaration = Some(false);
        spec.standalone = Some(false);
        let t = ResultTree {
            children: vec![elt("r", vec![])],
            output: spec,
            character_map: Vec::new(),
            secondary: Vec::new(),
        };
        let s = t.to_string().unwrap();
        assert!(s.contains(r#"standalone="no""#), "got: {s}");
    }

    // ── XML DOCTYPE ─────────────────────────────────────────────

    #[test]
    fn xml_doctype_system() {
        let mut spec = OutputSpec::default();
        spec.omit_xml_declaration = Some(true);
        spec.doctype_system = Some("foo.dtd".into());
        let t = ResultTree {
            children: vec![elt("r", vec![])],
            output: spec,
            character_map: Vec::new(),
            secondary: Vec::new(),
        };
        let s = t.to_string().unwrap();
        assert!(s.contains(r#"<!DOCTYPE r SYSTEM "foo.dtd">"#), "got: {s}");
    }

    #[test]
    fn xml_doctype_public() {
        let mut spec = OutputSpec::default();
        spec.omit_xml_declaration = Some(true);
        spec.doctype_system = Some("foo.dtd".into());
        spec.doctype_public = Some("-//ID//PUB".into());
        let t = ResultTree {
            children: vec![elt("r", vec![])],
            output: spec,
            character_map: Vec::new(),
            secondary: Vec::new(),
        };
        let s = t.to_string().unwrap();
        assert!(s.contains(r#"<!DOCTYPE r PUBLIC "-//ID//PUB" "foo.dtd">"#), "got: {s}");
    }

    // ── XML namespace declarations ──────────────────────────────

    #[test]
    fn xml_emits_namespace_declarations() {
        let t = tree_of(vec![ResultNode::Element {
            name: QName { prefix: Some("xs".into()), local: "schema".into(), uri: "http://www.w3.org/2001/XMLSchema".into() },
            namespaces: vec![
                (Some("xs".into()), "http://www.w3.org/2001/XMLSchema".into()),
                (None, "http://example.com/default".into()),
            ],
            attributes: Vec::new(),
            children: Vec::new(),
        }], None);
        let s = t.to_string().unwrap();
        assert!(s.contains(r#"xmlns:xs="http://www.w3.org/2001/XMLSchema""#), "got: {s}");
        assert!(s.contains(r#"xmlns="http://example.com/default""#), "got: {s}");
    }

    // ── XML comments & PIs ──────────────────────────────────────

    #[test]
    fn xml_serializes_comment() {
        let t = tree_of(vec![ResultNode::Comment(" hello ".into())], None);
        assert_eq!(t.to_string().unwrap(), "<!-- hello -->");
    }

    #[test]
    fn xml_serializes_pi_no_data() {
        let t = tree_of(vec![
            ResultNode::ProcessingInstruction { target: "pi".into(), data: String::new() },
        ], None);
        assert_eq!(t.to_string().unwrap(), "<?pi?>");
    }

    #[test]
    fn xml_serializes_pi_with_data() {
        let t = tree_of(vec![
            ResultNode::ProcessingInstruction {
                target: "xml-stylesheet".into(),
                data: r#"href="s.xsl""#.into(),
            },
        ], None);
        assert_eq!(t.to_string().unwrap(),
            r#"<?xml-stylesheet href="s.xsl"?>"#);
    }

    // ── CDATA-section elements ──────────────────────────────────

    #[test]
    fn xml_cdata_section_elements_wrap_text_children() {
        let mut spec = OutputSpec::default();
        spec.omit_xml_declaration = Some(true);
        spec.cdata_section_elements = vec![
            QName { prefix: None, local: "raw".into(), uri: String::new() },
        ];
        let t = ResultTree {
            children: vec![elt("raw", vec![text("a < b & c")])],
            output: spec,
            character_map: Vec::new(),
            secondary: Vec::new(),
        };
        let s = t.to_string().unwrap();
        assert!(s.contains("<![CDATA[a < b & c]]>"), "got: {s}");
    }

    #[test]
    fn xml_cdata_section_splits_embedded_close_seq() {
        // "]]>" inside the text must be split across two CDATA blocks.
        let mut spec = OutputSpec::default();
        spec.omit_xml_declaration = Some(true);
        spec.cdata_section_elements = vec![
            QName { prefix: None, local: "raw".into(), uri: String::new() },
        ];
        let t = ResultTree {
            children: vec![elt("raw", vec![text("end ]]> here")])],
            output: spec,
            character_map: Vec::new(),
            secondary: Vec::new(),
        };
        let s = t.to_string().unwrap();
        // Implementation splits ]]> into "]]]]><![CDATA[>".
        assert!(s.contains("]]]]><![CDATA[>"), "got: {s}");
    }

    // ── escape_attr full coverage ───────────────────────────────

    #[test]
    fn xml_attr_escapes_newline_tab_cr() {
        let t = tree_of(vec![ResultNode::Element {
            name: QName { prefix: None, local: "a".into(), uri: String::new() },
            namespaces: Vec::new(),
            attributes: vec![(
                QName { prefix: None, local: "v".into(), uri: String::new() },
                "x\ny\tz\rw".to_string(),
            )],
            children: Vec::new(),
        }], None);
        let s = t.to_string().unwrap();
        assert!(s.contains("&#10;"), "got: {s}");
        assert!(s.contains("&#9;"),  "got: {s}");
        assert!(s.contains("&#13;"), "got: {s}");
    }

    // ── HTML DOCTYPE ────────────────────────────────────────────

    #[test]
    fn html_doctype_system_only() {
        let mut spec = OutputSpec::default();
        spec.method = Some("html".into());
        spec.doctype_system = Some("about:legacy-compat".into());
        let t = ResultTree {
            children: vec![elt("html", vec![])],
            output: spec,
            character_map: Vec::new(),
            secondary: Vec::new(),
        };
        let s = t.to_string().unwrap();
        assert!(s.contains(r#"<!DOCTYPE html SYSTEM "about:legacy-compat">"#), "got: {s}");
    }

    #[test]
    fn html_doctype_public_only() {
        // PUBLIC without SYSTEM → emit `<!DOCTYPE root PUBLIC "...">`.
        let mut spec = OutputSpec::default();
        spec.method = Some("html".into());
        spec.doctype_public = Some("-//W3C//DTD HTML 4.01//EN".into());
        let t = ResultTree {
            children: vec![elt("html", vec![])],
            output: spec,
            character_map: Vec::new(),
            secondary: Vec::new(),
        };
        let s = t.to_string().unwrap();
        assert!(s.contains(r#"<!DOCTYPE html PUBLIC "-//W3C//DTD HTML 4.01//EN">"#),
                "got: {s}");
    }

    #[test]
    fn html_doctype_public_and_system() {
        let mut spec = OutputSpec::default();
        spec.method = Some("html".into());
        spec.doctype_public = Some("-//W3C//DTD HTML 4.01//EN".into());
        spec.doctype_system = Some("http://www.w3.org/TR/html4/strict.dtd".into());
        let t = ResultTree {
            children: vec![elt("html", vec![])],
            output: spec,
            character_map: Vec::new(),
            secondary: Vec::new(),
        };
        let s = t.to_string().unwrap();
        assert!(s.contains(r#"<!DOCTYPE html PUBLIC "-//W3C//DTD HTML 4.01//EN" "http://www.w3.org/TR/html4/strict.dtd">"#),
                "got: {s}");
    }

    // ── HTML namespaces / comments / PIs ────────────────────────

    #[test]
    fn html_emits_namespace_declarations() {
        let t = tree_of(vec![ResultNode::Element {
            name: QName { prefix: None, local: "html".into(), uri: String::new() },
            namespaces: vec![
                (Some("svg".into()), "http://www.w3.org/2000/svg".into()),
                (None, "http://www.w3.org/1999/xhtml".into()),
            ],
            attributes: Vec::new(),
            children: Vec::new(),
        }], Some("html"));
        let s = t.to_string().unwrap();
        assert!(s.contains(r#"xmlns:svg="http://www.w3.org/2000/svg""#), "got: {s}");
        assert!(s.contains(r#"xmlns="http://www.w3.org/1999/xhtml""#), "got: {s}");
    }

    #[test]
    fn html_serializes_comment() {
        let t = tree_of(vec![
            elt("html", vec![ResultNode::Comment(" hi ".into())]),
        ], Some("html"));
        let s = t.to_string().unwrap();
        assert!(s.contains("<!-- hi -->"), "got: {s}");
    }

    #[test]
    fn html_serializes_pi_with_and_without_data() {
        let t = tree_of(vec![
            elt("html", vec![
                ResultNode::ProcessingInstruction { target: "a".into(), data: String::new() },
                ResultNode::ProcessingInstruction { target: "b".into(), data: "x".into() },
            ]),
        ], Some("html"));
        let s = t.to_string().unwrap();
        // HTML PIs end with > (not ?>).
        assert!(s.contains("<?a>"), "got: {s}");
        assert!(s.contains("<?b x>"), "got: {s}");
    }

    #[test]
    fn html_text_with_dose_skips_escape() {
        let t = tree_of(vec![
            elt("html", vec![
                ResultNode::Text { content: "<raw>".into(), dose: true },
            ]),
        ], Some("html"));
        let s = t.to_string().unwrap();
        assert!(s.contains("<raw>"), "got: {s}");
    }

    #[test]
    fn html_default_method_when_no_root_html() {
        // No method specified, root isn't <html> → falls back to XML.
        let t = tree_of(vec![elt("r", vec![])], None);
        let s = t.to_string().unwrap();
        // XML serializer emits self-closing.
        assert_eq!(s, "<r/>");
    }

    // ── text method with dose ───────────────────────────────────

    #[test]
    fn text_method_concatenates_all_text() {
        let t = tree_of(vec![
            elt("a", vec![
                text("one"),
                elt("b", vec![text("two")]),
                ResultNode::Text { content: "three".into(), dose: true }, // dose ignored in text mode
            ]),
        ], Some("text"));
        assert_eq!(t.to_string().unwrap(), "onetwothree");
    }
}
