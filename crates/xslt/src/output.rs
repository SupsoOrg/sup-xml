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

use crate::ast::{OutputSpec, QName, Standalone};
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
        // XSLT 3.0 §26.1 default-method detection: a root `html` element
        // selects the html method (no namespace) or the xhtml method (in
        // the XHTML namespace).
        if name.local.eq_ignore_ascii_case("html") {
            if name.uri.is_empty() { return "html"; }
            if name.uri == "http://www.w3.org/1999/xhtml" { return "xhtml"; }
        }
    }
    "xml"
}

impl ResultTree {
    /// Serialise the result tree to a string using the effective
    /// output method.  XSLT 1.0 §16 — method = xml | html | xhtml |
    /// text.
    ///
    /// Method-dependent defaults follow the XSLT/XQuery Serialization
    /// spec: `indent`, `escape-uri-attributes`, and
    /// `include-content-type` all default to `yes` for the html and
    /// xhtml output methods, and to `no` / not-applicable for xml.
    pub fn to_string(&self) -> Result<String, XsltError> {
        let method = effective_method(self);
        let html_family = matches!(method, "html" | "xhtml");
        let indent = self.output.indent.unwrap_or(html_family);
        let escape_uri = html_family
            && self.output.escape_uri_attributes.unwrap_or(true);
        let content_type = html_family
            && self.output.include_content_type.unwrap_or(true);

        // include-content-type: splice a <meta http-equiv> into the
        // <head>.  Only clone the child list when an injection is
        // actually performed.
        let owned;
        let children: &[ResultNode] = if content_type {
            owned = with_content_type_meta(&self.children, &self.output, method == "xhtml");
            &owned
        } else {
            &self.children
        };

        let mut out = match method {
            "html"  => serialize_html(children, &self.output, indent, escape_uri),
            "text"  => serialize_text(children),
            // The xhtml output method uses XML syntax with the
            // html-family parameter defaults applied above, plus the
            // XHTML empty-element rules (non-void elements keep an
            // explicit end tag; void elements minimise to `<br />`).
            "xhtml" => serialize_xml(children, &self.output, &self.character_map,
                                     indent, escape_uri, true),
            _       => serialize_xml(children, &self.output, &self.character_map,
                                     indent, escape_uri, false),
        };
        // `byte-order-mark="yes"` (XSLT 2.0 §20) prefixes the output
        // with U+FEFF, ahead of any XML declaration.
        if self.output.byte_order_mark == Some(true) {
            out.insert(0, '\u{feff}');
        }
        Ok(out)
    }

    /// Write the serialised result to any [`io::Write`] sink.
    pub fn write_to(&self, w: &mut dyn std::io::Write) -> Result<(), XsltError> {
        let s = self.to_string()?;
        w.write_all(s.as_bytes())
            .map_err(|e| XsltError::InvalidStylesheet(format!("write failed: {e}")))
    }
}

// ── XML serialiser ────────────────────────────────────────────────

pub fn serialize_xml(
    children:   &[ResultNode],
    output:     &OutputSpec,
    cmap:       &[(char, String)],
    indent:     bool,
    escape_uri: bool,
    xhtml:      bool,
) -> String {
    let mut out = String::new();
    if should_emit_xml_decl(output) {
        let _ = write!(out, r#"<?xml version="{}" encoding="{}""#,
            output.version.as_deref().unwrap_or("1.0"),
            output.encoding.as_deref().unwrap_or("UTF-8"),
        );
        // `standalone="omit"` (and an absent attribute) suppress the
        // pseudo-attribute entirely; only yes/no are emitted.
        match output.standalone {
            Some(Standalone::Yes) => { let _ = write!(out, r#" standalone="yes""#); }
            Some(Standalone::No)  => { let _ = write!(out, r#" standalone="no""#); }
            Some(Standalone::Omit) | None => {}
        }
        out.push_str("?>\n");
    }
    if let Some(dt_sys) = output.doctype_system.as_deref() {
        if let Some(root) = first_element_name(children) {
            if let Some(pubid) = output.doctype_public.as_deref() {
                let _ = writeln!(out, r#"<!DOCTYPE {root} PUBLIC "{pubid}" "{dt_sys}">"#);
            } else {
                let _ = writeln!(out, r#"<!DOCTYPE {root} SYSTEM "{dt_sys}">"#);
            }
        }
    } else if xhtml && output.html_version.is_some_and(|v| v >= 5.0)
        && first_element_name(children).is_some()
    {
        // XSLT 3.0 §26.2 — html-version=5 on the xhtml method emits the
        // HTML5 doctype (no system/public identifier).
        out.push_str("<!DOCTYPE html>\n");
    }
    // `indent="yes"` (XSLT 1.0 §16.1): pretty-print element-only
    // content. Mixed content (any text-node child) suppresses
    // formatting for that element and its whole subtree so text is
    // preserved verbatim.
    for child in children {
        serialize_xml_node(child, &mut out, output, "", cmap, indent, escape_uri, xhtml, 0);
    }
    if indent && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Two spaces per nesting level — libxslt's default indent step.
fn push_indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
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
#[allow(clippy::too_many_arguments)]
fn serialize_xml_node(
    node:        &ResultNode,
    out:         &mut String,
    opts:        &OutputSpec,
    parent_default_ns: &str,
    cmap:        &[(char, String)],
    format:      bool,
    escape_uri:  bool,
    xhtml:       bool,
    level:       usize,
) {
    let xml_11   = opts.version.as_deref() == Some("1.1");
    let enc_cap  = encoding_capability(opts.encoding.as_deref());
    match node {
        ResultNode::Element { name, namespaces, attributes, children, .. } => {
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
                    render_attr_value(name, aname, value, escape_uri, xml_11, enc_cap, cmap));
            }
            if children.is_empty() {
                // XHTML (XSLT 3.0 Serialization §): an empty element whose
                // content model is not EMPTY must keep an explicit end tag
                // (`<p></p>`); only HTML void elements minimise, written
                // `<br />` per libxml2's xhtmlNodeDumpOutput.  Plain XML
                // self-closes every empty element.
                if xhtml {
                    if is_xhtml_void(name) {
                        out.push_str(" />");
                    } else {
                        out.push_str("></");
                        out.push_str(&q);
                        out.push('>');
                    }
                } else {
                    out.push_str("/>");
                }
                return;
            }
            out.push('>');
            // CDATA-section elements: text children of these are
            // wrapped in <![CDATA[...]]> rather than escaped.
            let is_cdata = opts.cdata_section_elements.iter()
                .any(|q| q.uri == name.uri && q.local == name.local);
            // Only element-only content is indented; the presence of a
            // text child collapses formatting for this element so its
            // character data round-trips unchanged.
            let child_format = format
                && !children.iter().any(|c| matches!(c, ResultNode::Text { .. }));
            for c in children {
                if child_format {
                    out.push('\n');
                    push_indent(out, level + 1);
                }
                if is_cdata {
                    if let ResultNode::Text { content, .. } = c {
                        out.push_str("<![CDATA[");
                        out.push_str(&content.replace("]]>", "]]]]><![CDATA[>"));
                        out.push_str("]]>");
                        continue;
                    }
                }
                serialize_xml_node(c, out, opts, child_default_ns, cmap, child_format, escape_uri, xhtml, level + 1);
            }
            if child_format {
                out.push('\n');
                push_indent(out, level);
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

// ── URI-attribute escaping (html / xhtml output methods) ──────────

/// Render an attribute value, applying `fn:escape-html-uri` first
/// when `escape_uri` is in effect and the attribute is URI-valued
/// (Serialization spec, Appendix "List of URI Attributes").  The
/// `%HH`-escaped result is then passed through ordinary attribute
/// escaping so reserved markup characters are still protected.
#[allow(clippy::too_many_arguments)]
fn render_attr_value(
    element:    &QName,
    attr:       &QName,
    value:      &str,
    escape_uri: bool,
    xml_11:     bool,
    enc_cap:    Option<u32>,
    cmap:       &[(char, String)],
) -> String {
    if escape_uri
        && attr.uri.is_empty()
        && is_uri_attribute(&element.local.to_ascii_lowercase(),
                            &attr.local.to_ascii_lowercase())
    {
        escape_attr_with_map(&escape_html_uri(value), xml_11, enc_cap, cmap)
    } else {
        escape_attr_with_map(value, xml_11, enc_cap, cmap)
    }
}

/// `fn:escape-html-uri` (XPath/XQuery F&O §6.4): printable ASCII
/// (`#x20`–`#x7E`) is left untouched; every other character is
/// percent-escaped, one `%HH` per byte of its UTF-8 encoding.  This
/// is deliberately confined to non-ASCII characters because escaping
/// ASCII characters in a URI is not always appropriate.
fn escape_html_uri(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for c in s.chars() {
        if matches!(c as u32, 0x20..=0x7E) {
            out.push(c);
        } else {
            for b in c.encode_utf8(&mut buf).bytes() {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Whether `(element, attr)` (both lowercase, no namespace) is a
/// URI-valued attribute per the Serialization spec, Appendix "List
/// of URI Attributes".  The list is element-specific: e.g. `value`
/// is a URI only on `input`, `name` only on `a`.
fn is_uri_attribute(element: &str, attr: &str) -> bool {
    match attr {
        "action"     => element == "form",
        "archive"    => element == "object",
        "background" => element == "body",
        "cite"       => matches!(element, "blockquote" | "del" | "ins" | "q"),
        "classid"    => element == "object",
        "codebase"   => matches!(element, "applet" | "object"),
        "data"       => element == "object",
        "datasrc"    => matches!(element,
            "button" | "div" | "input" | "object" | "select" | "span" | "table" | "textarea"),
        "for"        => element == "script",
        "formaction" => matches!(element, "button" | "input"),
        "href"       => matches!(element, "a" | "area" | "base" | "link"),
        "icon"       => element == "command",
        "longdesc"   => matches!(element, "frame" | "iframe" | "img"),
        "manifest"   => element == "html",
        "name"       => element == "a",
        "poster"     => element == "video",
        "profile"    => element == "head",
        "src"        => matches!(element,
            "audio" | "embed" | "frame" | "iframe" | "img" | "input" | "script"
            | "source" | "track" | "video"),
        "usemap"     => matches!(element, "img" | "input" | "object"),
        "value"      => element == "input",
        _ => false,
    }
}

// ── include-content-type (html / xhtml output methods) ────────────

/// Return the result tree's children with a
/// `<meta http-equiv="Content-Type" content="…; charset=…">` element
/// spliced in as the first child of the `head` element (Serialization
/// spec §6 / libxslt behavior).  Any existing content-type `meta` in
/// that head is removed first.  When there is no `head` element the
/// children are returned with no meta added.
fn with_content_type_meta(children: &[ResultNode], output: &OutputSpec, xhtml: bool) -> Vec<ResultNode> {
    let charset = output.encoding.as_deref().unwrap_or("UTF-8");
    let media   = output.media_type.as_deref().unwrap_or("text/html");
    let content = format!("{media}; charset={charset}");
    // The meta SHOULD share the head's namespace: no namespace for
    // the html output method, the XHTML namespace for xhtml.
    let ns = if xhtml { "http://www.w3.org/1999/xhtml" } else { "" };
    let mut result = children.to_vec();
    inject_content_type(&mut result, &content, ns);
    result
}

/// Walk `nodes` for the first `head` element in namespace `ns`,
/// replacing any content-type meta among its children with a fresh
/// one.  Returns whether a head was found.
fn inject_content_type(nodes: &mut [ResultNode], content: &str, ns: &str) -> bool {
    for node in nodes.iter_mut() {
        if let ResultNode::Element { name, children, .. } = node {
            if name.local.eq_ignore_ascii_case("head") && name.uri == ns {
                children.retain(|c| !is_content_type_meta(c));
                children.insert(0, content_type_meta(content, ns));
                return true;
            }
            if inject_content_type(children, content, ns) {
                return true;
            }
        }
    }
    false
}

/// Whether `node` is a `<meta http-equiv="Content-Type" …>` element
/// (the http-equiv name is matched case-insensitively, as HTML
/// requires).
fn is_content_type_meta(node: &ResultNode) -> bool {
    matches!(node, ResultNode::Element { name, attributes, .. }
        if name.local.eq_ignore_ascii_case("meta")
        && attributes.iter().any(|(a, v)|
            a.local.eq_ignore_ascii_case("http-equiv")
            && v.eq_ignore_ascii_case("content-type")))
}

fn content_type_meta(content: &str, ns: &str) -> ResultNode {
    let attr = |local: &str| QName { prefix: None, local: local.into(), uri: String::new() };
    ResultNode::Element {
        name: QName { prefix: None, local: "meta".into(), uri: ns.into() },
        namespaces: Vec::new(),
        attributes: vec![
            (attr("http-equiv"), "Content-Type".into()),
            (attr("content"),    content.into()),
        ],
        children: Vec::new(),
        schema_type: None,
        attr_types: Vec::new(),
    }
}

// ── HTML serialiser ───────────────────────────────────────────────

/// HTML5 void elements — emitted without a closing tag and
/// without `/>`.  The list is from the HTML5 spec § "Void
/// elements"; lowercase canonical form.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input",
    "link", "meta", "param", "source", "track", "wbr",
];

/// Is `name` an HTML void element, as recognised by the xhtml output
/// method?  XHTML elements carry the XHTML namespace (or, leniently,
/// no namespace); the local name is matched case-insensitively.
fn is_xhtml_void(name: &crate::ast::QName) -> bool {
    if !name.uri.is_empty() && name.uri != "http://www.w3.org/1999/xhtml" {
        return false;
    }
    let local_lc = name.local.to_ascii_lowercase();
    VOID_ELEMENTS.iter().any(|v| *v == local_lc)
}

/// Elements whose text content must NOT be escaped (XSLT 1.0 §16
/// HTML output method).
const RAW_TEXT_ELEMENTS: &[&str] = &["script", "style"];

pub fn serialize_html(
    children:   &[ResultNode],
    output:     &OutputSpec,
    indent:     bool,
    escape_uri: bool,
) -> String {
    let mut out = String::new();
    if let Some(dt_sys) = output.doctype_system.as_deref() {
        if let Some(root) = first_element_name(children) {
            if let Some(pubid) = output.doctype_public.as_deref() {
                let _ = writeln!(out, r#"<!DOCTYPE {root} PUBLIC "{pubid}" "{dt_sys}">"#);
            } else {
                let _ = writeln!(out, r#"<!DOCTYPE {root} SYSTEM "{dt_sys}">"#);
            }
        }
    } else if let Some(pubid) = output.doctype_public.as_deref() {
        if let Some(root) = first_element_name(children) {
            let _ = writeln!(out, r#"<!DOCTYPE {root} PUBLIC "{pubid}">"#);
        }
    }
    for c in children { serialize_html_node(c, &mut out, indent, escape_uri, 0); }
    if indent && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn serialize_html_node(node: &ResultNode, out: &mut String, format: bool, escape_uri: bool, level: usize) {
    match node {
        ResultNode::Element { name, namespaces, attributes, children, .. } => {
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
                    aname.to_qname_string(),
                    render_attr_value(name, aname, value, escape_uri, false, None, &[]));
            }
            // Void elements: close with `>`, no children, no closing tag.
            if name.uri.is_empty() && VOID_ELEMENTS.iter().any(|v| *v == local_lc) {
                out.push('>');
                return;
            }
            out.push('>');
            let raw_text = name.uri.is_empty()
                && RAW_TEXT_ELEMENTS.iter().any(|v| *v == local_lc);
            // As for XML, only element-only content is indented; a text
            // child (including the unescaped body of script/style)
            // collapses formatting so content round-trips unchanged.
            let child_format = format
                && !children.iter().any(|c| matches!(c, ResultNode::Text { .. }));
            for c in children {
                if child_format {
                    out.push('\n');
                    push_indent(out, level + 1);
                }
                if raw_text {
                    if let ResultNode::Text { content, .. } = c {
                        out.push_str(content);
                        continue;
                    }
                }
                serialize_html_node(c, out, child_format, escape_uri, level + 1);
            }
            // An empty element serializes as `<title></title>` — no
            // internal indentation (HTML 5 §8 / XSLT serialization).
            if child_format && !children.is_empty() {
                out.push('\n');
                push_indent(out, level);
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

pub fn serialize_text(children: &[ResultNode]) -> String {
    let mut out = String::new();
    for c in children { append_text(c, &mut out); }
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
            schema_type: None,
            attr_types: Vec::new(),
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
            schema_type: None,
            attr_types: Vec::new(),
        }],None);
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

    fn tree_indented(nodes: Vec<ResultNode>) -> ResultTree {
        let mut spec = OutputSpec::default();
        spec.omit_xml_declaration = Some(true);
        spec.indent = Some(true);
        ResultTree { children: nodes, output: spec, character_map: Vec::new(), secondary: Vec::new() }
    }

    #[test]
    fn xml_indent_yes_pretty_prints_element_only_content() {
        let t = tree_indented(vec![elt("root", vec![
            elt("a", vec![elt("b", vec![])]),
            elt("c", vec![]),
        ])]);
        assert_eq!(
            t.to_string().unwrap(),
            "<root>\n  <a>\n    <b/>\n  </a>\n  <c/>\n</root>\n",
        );
    }

    #[test]
    fn xml_indent_yes_preserves_mixed_content() {
        // An element with a text child is left untouched, and that
        // collapses formatting for its whole subtree.
        let t = tree_indented(vec![elt("root", vec![
            elt("p", vec![text("hello "), elt("b", vec![text("world")])]),
        ])]);
        assert_eq!(
            t.to_string().unwrap(),
            "<root>\n  <p>hello <b>world</b></p>\n</root>\n",
        );
    }

    #[test]
    fn xml_indent_off_by_default() {
        let t = tree_of(vec![elt("root", vec![elt("a", vec![])])], None);
        assert_eq!(t.to_string().unwrap(), "<root><a/></root>");
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

    fn tree_indented_method(nodes: Vec<ResultNode>, method: &str) -> ResultTree {
        let mut spec = OutputSpec::default();
        spec.method = Some(method.into());
        spec.indent = Some(true);
        // Keep these indentation-focused tests independent of the
        // include-content-type meta injection (exercised separately).
        spec.include_content_type = Some(false);
        ResultTree { children: nodes, output: spec, character_map: Vec::new(), secondary: Vec::new() }
    }

    #[test]
    fn html_indent_yes_pretty_prints_element_only_content() {
        let t = tree_indented_method(vec![
            elt("html", vec![
                elt("head", vec![elt("meta", vec![])]),
                elt("body", vec![elt("p", vec![text("hi")])]),
            ]),
        ], "html");
        assert_eq!(
            t.to_string().unwrap(),
            "<html>\n  <head>\n    <meta>\n  </head>\n  <body>\n    <p>hi</p>\n  </body>\n</html>\n",
        );
    }

    #[test]
    fn html_indent_yes_preserves_mixed_content_and_raw_text() {
        let t = tree_indented_method(vec![
            elt("body", vec![
                elt("p", vec![text("a "), elt("b", vec![text("c")]), text(" d")]),
                elt("script", vec![text("if (a < b) x();")]),
            ]),
        ], "html");
        assert_eq!(
            t.to_string().unwrap(),
            "<body>\n  <p>a <b>c</b> d</p>\n  <script>if (a < b) x();</script>\n</body>\n",
        );
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
        spec.standalone = Some(Standalone::Yes);
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
        spec.standalone = Some(Standalone::No);
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
            schema_type: None,
            attr_types: Vec::new(),
        }],None);
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
            schema_type: None,
            attr_types: Vec::new(),
        }],None);
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
            schema_type: None,
            attr_types: Vec::new(),
        }],Some("html"));
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

    // ── serialization-parameter defaults (XSLT 2.0 §20) ─────────
    //
    // Defaults are method-dependent: `indent`, `escape-uri-attributes`,
    // and `include-content-type` default to `yes` for the html and
    // xhtml output methods and to `no` / not-applicable for xml.

    fn out_tree(nodes: Vec<ResultNode>, spec: OutputSpec) -> ResultTree {
        ResultTree { children: nodes, output: spec, character_map: Vec::new(), secondary: Vec::new() }
    }

    fn elt_attrs(name: &str, attrs: &[(&str, &str)], children: Vec<ResultNode>) -> ResultNode {
        ResultNode::Element {
            name: QName { prefix: None, local: name.into(), uri: String::new() },
            namespaces: Vec::new(),
            attributes: attrs.iter().map(|(k, v)| (
                QName { prefix: None, local: (*k).into(), uri: String::new() },
                (*v).to_string(),
            )).collect(),
            children,
            schema_type: None,
            attr_types: Vec::new(),
        }
    }

    // standalone -----------------------------------------------------

    #[test]
    fn standalone_omit_suppresses_pseudo_attribute() {
        let spec = OutputSpec { standalone: Some(Standalone::Omit), ..Default::default() };
        let s = out_tree(vec![elt("r", vec![])], spec).to_string().unwrap();
        assert!(s.starts_with(r#"<?xml version="1.0" encoding="UTF-8"?>"#), "got: {s}");
        assert!(!s.contains("standalone"), "omit must not emit a standalone pseudo-attr: {s}");
    }

    #[test]
    fn standalone_absent_suppresses_pseudo_attribute() {
        let s = out_tree(vec![elt("r", vec![])], OutputSpec::default()).to_string().unwrap();
        assert!(!s.contains("standalone"), "got: {s}");
    }

    // indent ---------------------------------------------------------

    #[test]
    fn indent_defaults_to_no_for_xml_method() {
        let spec = OutputSpec {
            method: Some("xml".into()),
            omit_xml_declaration: Some(true),
            ..Default::default()
        };
        let s = out_tree(vec![elt("root", vec![elt("a", vec![])])], spec).to_string().unwrap();
        assert_eq!(s, "<root><a/></root>");
    }

    #[test]
    fn indent_defaults_to_yes_for_html_method() {
        let spec = OutputSpec {
            method: Some("html".into()),
            include_content_type: Some(false), // isolate from meta injection
            ..Default::default()
        };
        let s = out_tree(vec![elt("html", vec![
            elt("body", vec![elt("p", vec![])]),
        ])], spec).to_string().unwrap();
        assert!(s.contains("<html>\n  <body>\n    <p>"), "html should indent by default: {s}");
    }

    #[test]
    fn indent_defaults_to_yes_for_xhtml_method() {
        let spec = OutputSpec {
            method: Some("xhtml".into()),
            omit_xml_declaration: Some(true),
            include_content_type: Some(false),
            ..Default::default()
        };
        // The xhtml method keeps an explicit end tag on empty non-void
        // elements (`<a></a>`), unlike the plain xml method (`<a/>`).
        let s = out_tree(vec![elt("root", vec![elt("a", vec![])])], spec).to_string().unwrap();
        assert_eq!(s, "<root>\n  <a></a>\n</root>\n");
    }

    #[test]
    fn indent_defaults_to_yes_when_html_method_auto_detected() {
        // No explicit method; a root <html> selects the html method,
        // which carries the indent=yes default.
        let spec = OutputSpec { include_content_type: Some(false), ..Default::default() };
        let s = out_tree(vec![elt("html", vec![elt("body", vec![elt("p", vec![])])])], spec)
            .to_string().unwrap();
        assert!(s.contains("<html>\n  <body>"), "got: {s}");
    }

    #[test]
    fn indent_no_overrides_html_default() {
        let spec = OutputSpec {
            method: Some("html".into()),
            indent: Some(false),
            include_content_type: Some(false),
            ..Default::default()
        };
        let s = out_tree(vec![elt("html", vec![elt("body", vec![])])], spec).to_string().unwrap();
        assert_eq!(s, "<html><body></body></html>");
    }

    // escape-uri-attributes -----------------------------------------

    #[test]
    fn escape_uri_attributes_default_escapes_non_ascii_in_href() {
        let spec = OutputSpec {
            method: Some("html".into()),
            include_content_type: Some(false),
            ..Default::default()
        };
        // é (U+00E9) → %C3%A9; ASCII characters (incl. space, ?) are
        // left intact per fn:escape-html-uri.
        let node = elt_attrs("a", &[("href", "/caf\u{e9}?x=1 2")], vec![]);
        let s = out_tree(vec![node], spec).to_string().unwrap();
        assert!(s.contains(r#"href="/caf%C3%A9?x=1 2""#), "got: {s}");
    }

    #[test]
    fn escape_uri_attributes_no_disables_escaping() {
        let spec = OutputSpec {
            method: Some("html".into()),
            escape_uri_attributes: Some(false),
            include_content_type: Some(false),
            ..Default::default()
        };
        let node = elt_attrs("a", &[("href", "/caf\u{e9}")], vec![]);
        let s = out_tree(vec![node], spec).to_string().unwrap();
        assert!(s.contains("href=\"/caf\u{e9}\""), "got: {s}");
    }

    #[test]
    fn escape_uri_attributes_only_applies_to_uri_valued_attributes() {
        // `title` is not a URI attribute → never escaped.
        let spec = OutputSpec {
            method: Some("html".into()),
            include_content_type: Some(false),
            ..Default::default()
        };
        let node = elt_attrs("a", &[("title", "caf\u{e9}")], vec![]);
        let s = out_tree(vec![node], spec).to_string().unwrap();
        assert!(s.contains("title=\"caf\u{e9}\""), "got: {s}");
    }

    #[test]
    fn escape_uri_attributes_is_element_specific() {
        // `href` is a URI attribute on <a> but not on an arbitrary
        // element, so it is escaped on <a> and left alone elsewhere.
        let spec = OutputSpec {
            method: Some("html".into()),
            include_content_type: Some(false),
            ..Default::default()
        };
        let s = out_tree(vec![elt("body", vec![
            elt_attrs("a",   &[("href", "/\u{e9}")], vec![]),
            elt_attrs("span", &[("href", "/\u{e9}")], vec![]),
        ])], spec).to_string().unwrap();
        assert!(s.contains("<a href=\"/%C3%A9\">"), "a/href escaped: {s}");
        assert!(s.contains("<span href=\"/\u{e9}\">"), "span/href untouched: {s}");
    }

    #[test]
    fn escape_uri_attributes_not_applied_for_xml_method() {
        let spec = OutputSpec {
            method: Some("xml".into()),
            omit_xml_declaration: Some(true),
            ..Default::default()
        };
        let node = elt_attrs("a", &[("href", "/caf\u{e9}")], vec![]);
        let s = out_tree(vec![node], spec).to_string().unwrap();
        assert!(s.contains("href=\"/caf\u{e9}\""), "got: {s}");
    }

    // include-content-type ------------------------------------------

    #[test]
    fn include_content_type_default_inserts_meta_into_head() {
        let spec = OutputSpec { method: Some("html".into()), ..Default::default() };
        let s = out_tree(vec![elt("html", vec![
            elt("head", vec![elt("title", vec![text("T")])]),
            elt("body", vec![]),
        ])], spec).to_string().unwrap();
        assert!(
            s.contains(r#"<meta http-equiv="Content-Type" content="text/html; charset=UTF-8">"#),
            "got: {s}");
        // Spec: inserted as the FIRST child of head, before <title>.
        assert!(s.find("http-equiv").unwrap() < s.find("<title>").unwrap(),
            "meta must precede title: {s}");
    }

    #[test]
    fn include_content_type_uses_media_type_and_encoding() {
        let spec = OutputSpec {
            method: Some("html".into()),
            media_type: Some("application/xhtml+xml".into()),
            encoding: Some("ISO-8859-1".into()),
            ..Default::default()
        };
        let s = out_tree(vec![elt("html", vec![elt("head", vec![])])], spec).to_string().unwrap();
        assert!(s.contains(r#"content="application/xhtml+xml; charset=ISO-8859-1""#), "got: {s}");
    }

    #[test]
    fn include_content_type_replaces_existing_content_type_meta() {
        let spec = OutputSpec { method: Some("html".into()), ..Default::default() };
        let existing = elt_attrs("meta",
            &[("http-equiv", "Content-Type"), ("content", "text/html; charset=stale")], vec![]);
        let s = out_tree(vec![elt("html", vec![elt("head", vec![existing])])], spec)
            .to_string().unwrap();
        assert!(!s.contains("charset=stale"), "stale meta must be removed: {s}");
        assert_eq!(s.matches("http-equiv").count(), 1, "exactly one content-type meta: {s}");
    }

    #[test]
    fn include_content_type_no_disables_meta() {
        let spec = OutputSpec {
            method: Some("html".into()),
            include_content_type: Some(false),
            ..Default::default()
        };
        let s = out_tree(vec![elt("html", vec![elt("head", vec![])])], spec).to_string().unwrap();
        assert!(!s.contains("http-equiv"), "got: {s}");
    }

    #[test]
    fn include_content_type_noop_without_head() {
        let spec = OutputSpec { method: Some("html".into()), ..Default::default() };
        let s = out_tree(vec![elt("html", vec![elt("body", vec![])])], spec).to_string().unwrap();
        assert!(!s.contains("http-equiv"), "no head → no meta: {s}");
    }

    #[test]
    fn include_content_type_not_applied_for_xml_method() {
        let spec = OutputSpec {
            method: Some("xml".into()),
            omit_xml_declaration: Some(true),
            ..Default::default()
        };
        let s = out_tree(vec![elt("html", vec![elt("head", vec![])])], spec).to_string().unwrap();
        assert!(!s.contains("http-equiv"), "xml method must not inject meta: {s}");
    }
}
