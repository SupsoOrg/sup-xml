//! Serialize a [`sup_xml_tree::dom::Document`] back to XML text.
//!
//! Mirrors the existing [`crate::serializer`] module — same option type
//! ([`crate::SerializeOptions`]), same escaping rules, same compact and
//! pretty-print layouts — but walks the arena tree (`Node::children()` /
//! `Node::attributes()` iterators) rather than the legacy `Vec<Node>` /
//! `Vec<Attribute>` fields.
//!
//! # Example
//!
//! ```
//! use sup_xml_core::{parse_str, serialize_to_string, ParseOptions};
//! let doc = parse_str("<r><a id='1'/></r>", &ParseOptions::default()).unwrap();
//! let xml = serialize_to_string(&doc);
//! assert!(xml.contains(r#"<a id="1"/>"#));
//! ```

use sup_xml_tree::dom::{Document, Node, NodeKind};

use crate::output::{XmlBuf, OutputCharset};

// ── options ──────────────────────────────────────────────────────────────────

/// Options that control how a [`Document`] is serialized back to XML text.
///
/// # Example
/// ```
/// use sup_xml_core::{parse_str, serialize_with, SerializeOptions, ParseOptions};
///
/// let doc = parse_str("<root><child/></root>", &ParseOptions::default()).unwrap();
/// let opts = SerializeOptions { format: true, ..SerializeOptions::default() };
/// println!("{}", serialize_with(&doc, &opts));
/// ```
#[derive(Debug, Clone)]
pub struct SerializeOptions {
    /// Emit `<?xml version="…" encoding="…"?>` as the first line.
    /// Default: `true`.  Has no effect when [`html_mode`](Self::html_mode)
    /// is on (HTML5 has no XML declaration).
    pub write_xml_decl: bool,
    /// Pretty-print with newlines and indentation.  When `false` (the
    /// default), whitespace text nodes are preserved exactly as parsed.
    /// When `true`, whitespace-only text nodes between elements are dropped
    /// and fresh indentation is added.  Default: `false`.
    pub format: bool,
    /// The string used for one indentation level when `format` is `true`.
    /// Default: two spaces (`"  "`).
    pub indent: String,
    /// Emit HTML5-style serialization rather than XML.  Differences:
    ///
    /// - Void elements (`<br>`, `<img>`, `<input>`, …) emit *with no
    ///   end tag and no self-closing slash* — `<br>` not `<br/>`.
    /// - Raw-text elements (`<script>`, `<style>`) emit their text
    ///   content verbatim — no entity escaping.  This is required
    ///   because the HTML5 tokenizer treats those tags' contents as
    ///   literal text up to the close tag.
    /// - Empty non-void elements emit as `<div></div>`, not `<div/>`.
    ///   HTML5 doesn't support self-closing on non-void elements;
    ///   browsers parse `<div/>` as `<div>` with no close.
    /// - Boolean attributes use shorthand: `<input disabled>` instead
    ///   of `<input disabled="">` or `<input disabled="disabled">`.
    ///   Detected when the attribute value is empty or matches the
    ///   attribute name (case-insensitive) — these are
    ///   spec-equivalent forms in HTML5.
    /// - The XML declaration is suppressed regardless of
    ///   [`write_xml_decl`](Self::write_xml_decl).
    /// - If the document carries `html_metadata` with a captured
    ///   DOCTYPE, that DOCTYPE is emitted as the first line.
    ///   For documents without a stored DOCTYPE we don't fabricate
    ///   one — caller's responsibility to insert `<!DOCTYPE html>`
    ///   if they want it.
    ///
    /// Default: `false`.  Turn on when emitting output meant to be
    /// consumed by browsers / HTML parsers rather than XML parsers.
    pub html_mode: bool,
    /// Serialize per libxml2's `xhtmlNodeDumpOutput` (XML syntax with
    /// XHTML accommodations), used when the document's DOCTYPE identifies
    /// it as XHTML.  Distinct from [`html_mode`](Self::html_mode): output
    /// is still well-formed XML, but a root `<html>` carrying no namespace
    /// gains `xmlns="http://www.w3.org/1999/xhtml"`, an empty non-void
    /// element is written `<tag></tag>` (browsers mis-parse `<tag/>`), and
    /// an empty void element (`br`, `img`, …) is written `<tag />`.
    ///
    /// Default: `false`.
    pub xhtml: bool,
    /// The charset the output bytes will ultimately be encoded into.
    /// Characters it cannot represent are written as numeric character
    /// references in text / attribute content — matching libxml2, where
    /// a non-UTF-8 output encoding (or no encoding at all, which defaults
    /// to ASCII) escapes anything outside its repertoire.  Default:
    /// [`OutputCharset::Utf8`] (no extra escaping).
    pub out_charset: OutputCharset,
}

impl Default for SerializeOptions {
    fn default() -> Self {
        Self {
            write_xml_decl: true,
            format: false,
            indent: "  ".to_string(),
            html_mode: false,
            xhtml: false,
            out_charset: OutputCharset::Utf8,
        }
    }
}

// HTML5 helpers — duplicated from `crate::serializer` rather than re-exported
// to keep the two modules independent.  Tiny; if they ever drift apart that's
// a bug we'd catch in the round-trip tests below.
fn is_html_void_element(name: &str) -> bool {
    matches!(name,
        "area" | "base" | "br" | "col" | "embed" | "hr" | "img" | "input" |
        "link" | "meta" | "param" | "source" | "track" | "wbr" |
        "keygen" | "menuitem"
    )
}
fn is_html_raw_text_element(name: &str) -> bool {
    matches!(name, "script" | "style")
}
/// libxml2's set of HTML boolean attributes (`htmlBooleanAttrs`).  When
/// an attribute's name is one of these, the HTML serializer emits just
/// the name and drops the value entirely — `selected="bar"` becomes
/// `selected` — regardless of what the value was.
fn is_html_boolean_attr(name: &str) -> bool {
    const BOOLEAN_ATTRS: &[&str] = &[
        "checked", "compact", "declare", "defer", "disabled", "ismap",
        "multiple", "nohref", "noresize", "noshade", "nowrap", "readonly",
        "selected",
    ];
    BOOLEAN_ATTRS.iter().any(|b| name.eq_ignore_ascii_case(b))
}

// ── public entry points ─────────────────────────────────────────────────────

/// Serialize an arena [`Document`] to a compact XML string with default options.
pub fn serialize_to_string(doc: &Document) -> String {
    serialize_with(doc, &SerializeOptions::default())
}

/// Serialize an arena [`Document`] to a compact UTF-8 byte vector.
pub fn serialize_to_bytes(doc: &Document) -> Vec<u8> {
    serialize_to_string(doc).into_bytes()
}

/// Serialize an arena [`Document`] with pretty-printing (newlines + indentation).
pub fn serialize_formatted(doc: &Document) -> String {
    serialize_with(doc, &SerializeOptions {
        write_xml_decl: true,
        format:         true,
        indent:         "  ".to_string(),
        html_mode:      false,
        xhtml:          false,
        out_charset:    OutputCharset::Utf8,
    })
}

/// Serialize an arena [`Document`] with full control via [`SerializeOptions`].
pub fn serialize_with(doc: &Document, opts: &SerializeOptions) -> String {
    let mut s = Serializer { buf: XmlBuf::with_charset(4096, opts.out_charset), opts };
    s.write_document(doc);
    s.buf.into_string()
}

/// Serialize a single [`Node`] (and its descendants) without the
/// surrounding document machinery (no XML declaration, no DOCTYPE).
/// Used by the libxml2 ABI shim's `xmlNodeDump` and by callers that
/// want a subtree string.
pub fn serialize_node_to_string(node: &Node<'_>, opts: &SerializeOptions) -> String {
    let mut s = Serializer { buf: XmlBuf::with_charset(256, opts.out_charset), opts };
    s.write_node(node, 0);
    s.buf.into_string()
}

/// Serialize an arena [`Document`] as HTML5.  Emits the DOCTYPE if the
/// document carries one, never emits an XML declaration, uses HTML5 void-
/// element / raw-text / boolean-attribute conventions.  Same shape as
/// [`crate::serialize_html_to_string`] but accepts the arena tree.
pub fn serialize_html_to_string(doc: &Document) -> String {
    serialize_with(doc, &SerializeOptions {
        html_mode:      true,
        write_xml_decl: false,
        ..SerializeOptions::default()
    })
}

// ── serializer ──────────────────────────────────────────────────────────────

struct Serializer<'o> {
    buf:  XmlBuf,
    opts: &'o SerializeOptions,
}

impl Serializer<'_> {
    fn write_document(&mut self, doc: &Document) {
        if self.opts.html_mode {
            // HTML5: emit DOCTYPE if the document carries one, never an
            // XML declaration.
            if let Some(meta) = &doc.html_metadata {
                if let Some(dt) = &meta.doctype {
                    self.buf.push_str("<!DOCTYPE ");
                    self.buf.push_str(&dt.name);
                    if !dt.public_id.is_empty() {
                        self.buf.push_str(" PUBLIC \"");
                        self.buf.push_str(&dt.public_id);
                        self.buf.push_byte(b'"');
                        if !dt.system_id.is_empty() {
                            self.buf.push_str(" \"");
                            self.buf.push_str(&dt.system_id);
                            self.buf.push_byte(b'"');
                        }
                    } else if !dt.system_id.is_empty() {
                        self.buf.push_str(" SYSTEM \"");
                        self.buf.push_str(&dt.system_id);
                        self.buf.push_byte(b'"');
                    }
                    self.buf.push_byte(b'>');
                    if self.opts.format {
                        self.buf.push_byte(b'\n');
                    }
                }
            }
        } else if self.opts.write_xml_decl {
            self.buf.push_str("<?xml version=\"");
            self.buf.push_str(&doc.version);
            self.buf.push_byte(b'"');
            // Only emit `encoding=` when the doc carries one.
            // libxml2's serializer behaves the same — when
            // doc->encoding is NULL (no `<?xml encoding="…"?>`
            // declaration in source), the attribute is omitted.
            if !doc.encoding.is_empty() {
                self.buf.push_str(" encoding=\"");
                self.buf.push_str(&doc.encoding);
                self.buf.push_byte(b'"');
            }
            if let Some(sa) = doc.standalone {
                self.buf.push_str(if sa { " standalone=\"yes\"" } else { " standalone=\"no\"" });
            }
            self.buf.push_str("?>");
            if self.opts.format {
                self.buf.push_byte(b'\n');
            }
        }
        self.write_node(doc.root(), 0);
        if self.opts.format {
            self.buf.push_byte(b'\n');
        }
    }

    fn write_node(&mut self, node: &Node<'_>, depth: usize) {
        match node.kind {
            NodeKind::Element => self.write_element(node, depth),
            NodeKind::Text    => self.buf.push_escaped_text(node.content()),
            NodeKind::Comment => {
                self.buf.push_str("<!--");
                self.buf.push_str(node.content());
                self.buf.push_str("-->");
            }
            NodeKind::CData => {
                self.buf.push_str("<![CDATA[");
                // A literal `]]>` in the content would close the section
                // early; libxml2 splits it so the `]]` ends one CDATA and
                // the `>` opens the next (`]]>` → `]]]]><![CDATA[>`).
                let content = node.content();
                if content.contains("]]>") {
                    self.buf.push_str(&content.replace("]]>", "]]]]><![CDATA[>"));
                } else {
                    self.buf.push_str(content);
                }
                self.buf.push_str("]]>");
            }
            NodeKind::Pi => {
                self.buf.push_str("<?");
                self.buf.push_str(node.name());
                // libxml2 emits the separating space whenever `content`
                // is non-NULL — even for an empty data section — and omits
                // it for a no-data PI (NULL content).
                if let Some(c) = node.content_opt() {
                    self.buf.push_byte(b' ');
                    self.buf.push_str(c);
                }
                self.buf.push_str("?>");
            }
            NodeKind::EntityRef => {
                // `content` was populated with the literal `&name;`
                // source form by the parser when emitting the ref —
                // write it verbatim to round-trip without expansion.
                self.buf.push_str(node.content());
            }
            NodeKind::DtdDecl => {
                // Raw internal-subset markup declarations — emit
                // verbatim (already newline-terminated per declaration).
                self.buf.push_str(node.content());
            }
            // The internal-subset node itself carries no markup of its
            // own (the `<!DOCTYPE …>` header is emitted by the compat
            // serializer / lxml via `doc->intSubset`); skip it when it
            // appears as a document-level sibling.
            NodeKind::Dtd => {}
            // c-abi-only discriminants.  `Attribute` only sits on the
            // `xmlAttr` struct (which we never reach through a `Node`
            // pointer).  `Document` shows up when a C-ABI consumer
            // hands us the document itself cast to a `Node*` — e.g.
            // libxml2's `xmlNodeDumpOutput(buf, doc, doc_as_node, …)`.
            // In that case "serialize the node" means "serialize each
            // child", since the document itself emits no markup.
            NodeKind::Attribute => unreachable!("Attribute kind never appears on a Node"),
            NodeKind::Document | NodeKind::DocumentFragment => {
                // Both are transparent containers — serialize children
                // without emitting any markup of their own.  Fragment
                // is produced by `xmlNewDocFragment` in the compat
                // shim; Document is reached when a C consumer casts
                // the doc itself to xmlNode* and asks to dump it.
                for child in node.children() {
                    self.write_node(child, depth);
                }
            }
        }
    }

    fn write_element(&mut self, el: &Node<'_>, depth: usize) {
        let html_mode = self.opts.html_mode;
        let name = el.name();

        self.buf.push_byte(b'<');
        // In c-abi mode `Node::name` is the local part only (libxml2
        // convention); the prefix lives on the namespace.  Re-prepend
        // `prefix:` here so serialisation reconstructs the QName that
        // the parser ingested.  Non-c-abi keeps the QName in `name`
        // directly — no prefix prepend needed.
        #[cfg(feature = "c-abi")]
        if let Some(ns) = el.namespace.get() {
            if let Some(prefix) = ns.prefix() {
                self.buf.push_str(prefix);
                self.buf.push_byte(b':');
            }
        }
        self.buf.push_str(name);
        // XHTML serialization (libxml2 `xhtmlNodeDumpOutput`): a root
        // `<html>` that carries no namespace is given the XHTML namespace
        // declaration, matching the spec's strictly-conforming form.
        #[cfg(feature = "c-abi")]
        if self.opts.xhtml
            && name == "html"
            && el.namespace.get().is_none()
            && el.ns_def.get().is_none()
        {
            self.buf.push_str(" xmlns=\"http://www.w3.org/1999/xhtml\"");
        }
        // In the c-abi build, namespace declarations live on the
        // separate `ns_def` chain (libxml2 convention) rather than
        // in the attribute list.  Emit them first so the resulting
        // serialization carries the same `xmlns[:p]="..."` syntax
        // regardless of which build the consumer parsed through.
        #[cfg(feature = "c-abi")]
        {
            let mut ns_cur = el.ns_def.get();
            while let Some(ns) = ns_cur {
                // The `xml` prefix is predefined by XML 1.0 §3.7 and
                // must never be re-declared in serialization — emitting
                // `xmlns:xml="…"` is non-conforming output.  Skip it
                // silently if some consumer pushed one onto ns_def.
                let skip = matches!(ns.prefix(), Some("xml"))
                    && ns.href() == "http://www.w3.org/XML/1998/namespace";
                if skip {
                    ns_cur = ns.next.get();
                    continue;
                }
                self.buf.push_byte(b' ');
                match ns.prefix() {
                    None    => self.buf.push_str("xmlns"),
                    Some(p) => {
                        self.buf.push_str("xmlns:");
                        self.buf.push_str(p);
                    }
                }
                self.buf.push_str("=\"");
                self.buf.push_escaped_attr(ns.href());
                self.buf.push_byte(b'"');
                ns_cur = ns.next.get();
            }
        }
        for attr in el.attributes() {
            self.buf.push_byte(b' ');
            // Same prefix reconstruction as element names (c-abi
            // only): libxml2 stores the attribute's local name in
            // `name` and the prefix on `ns`.  Without prepending
            // we'd emit `a="…"` instead of `ns0:a="…"`.
            #[cfg(feature = "c-abi")]
            if let Some(ns) = attr.namespace.get() {
                if let Some(prefix) = ns.prefix() {
                    self.buf.push_str(prefix);
                    self.buf.push_byte(b':');
                }
            }
            self.buf.push_str(attr.name());
            // HTML minimizes an attribute to its name alone when the name
            // is a known boolean attribute (libxml2's `htmlIsBooleanAttr`,
            // value dropped regardless) or when it carries no value at all
            // (`<tag attribute>`, `el.set(name, None)`).
            if html_mode && (is_html_boolean_attr(attr.name()) || attr.value().is_empty()) {
                continue;
            }
            self.buf.push_str("=\"");
            self.buf.push_escaped_attr(attr.value());
            self.buf.push_byte(b'"');
        }

        let empty = el.first_child.get().is_none();

        if html_mode {
            // Void elements: no end tag, no self-closing slash.
            if is_html_void_element(name) {
                self.buf.push_byte(b'>');
                return;
            }
            // Empty non-void element: explicit `<tag></tag>` (HTML5 forbids
            // self-closing on non-void elements).
            if empty {
                self.buf.push_str("></");
                self.write_element_qname(el);
                self.buf.push_byte(b'>');
                return;
            }
        } else if self.opts.xhtml && empty {
            // XHTML (libxml2 `xhtmlNodeDumpOutput`): a void element is
            // `<br />`; any other empty element is `<tag></tag>`, since
            // browsers parse the XML self-closing form `<tag/>` of a
            // non-void HTML element as an unterminated open tag.
            if is_html_void_element(name) {
                self.buf.push_str(" />");
            } else {
                self.buf.push_str("></");
                self.write_element_qname(el);
                self.buf.push_byte(b'>');
            }
            return;
        } else if empty {
            // XML: empty element gets `/>` shorthand.
            self.buf.push_str("/>");
            return;
        }

        self.buf.push_byte(b'>');

        // HTML5 raw-text elements: script and style content is verbatim.
        if html_mode && is_html_raw_text_element(name) {
            for child in el.children() {
                if child.kind == NodeKind::Text {
                    self.buf.push_str(child.content());
                }
            }
            self.buf.push_str("</");
            self.write_element_qname(el);
            self.buf.push_byte(b'>');
            return;
        }

        if self.opts.format {
            if !self.opts.html_mode && contains_text_child(el) {
                // libxml2's XML pretty-print rule: when an element
                // has *any* text/cdata/entity-ref child, formatting
                // is disabled inside it — every child is emitted
                // verbatim (no indent added, no whitespace trimmed,
                // no whitespace-only nodes dropped).  Matches
                // libxml2 2.15.3 xmlsave.c lines 1050-1062
                // (sets `ctxt->format = 0` and remembers the
                // `unformattedNode`).
                //
                // HTML mode has its own gating via
                // `html_skip_indent` below — keep them separate
                // because the rules differ in the edge cases.
                for child in el.children() {
                    self.write_node(child, depth + 1);
                }
            } else if is_inline(el) {
                // Single non-empty text child rendered inline.
                let text = el.children().find_map(|c| match c.kind {
                    NodeKind::Text => Some(c.content().trim()),
                    _              => None,
                }).unwrap_or("");
                self.buf.push_escaped_text(text);
            } else if self.opts.html_mode && html_skip_indent(el) {
                // libxml2's HTMLtree.c rule: skip the newline/indent
                // dance for the opening *and* closing tag when the
                // element has a single child, when the first/last
                // child is text/entity-ref, or when the element is a
                // formatting-sensitive container ("p", "pre", "param").
                // Matches the htmlNodeDumpInternal logic
                // (libxml2 2.15.3 HTMLtree.c lines 969-977 and 1085-1091).
                for child in el.children() {
                    self.write_node(child, depth + 1);
                }
            } else {
                // libxml2's HTML serializer inserts newlines between
                // block children but never indentation — leading
                // whitespace can change how inline content renders, so
                // HTMLtree.c emits a bare '\n' where xmlsave.c would
                // also write `level` indent strings.
                let indent = !self.opts.html_mode;
                self.buf.push_byte(b'\n');
                for child in el.children() {
                    // Skip whitespace-only text nodes when pretty-printing.
                    if child.kind == NodeKind::Text && child.content().trim().is_empty() {
                        continue;
                    }
                    if indent {
                        self.write_indent(depth + 1);
                    }
                    self.write_node(child, depth + 1);
                    self.buf.push_byte(b'\n');
                }
                if indent {
                    self.write_indent(depth);
                }
            }
        } else {
            for child in el.children() {
                self.write_node(child, depth + 1);
            }
        }

        self.buf.push_str("</");
        self.write_element_qname(el);
        self.buf.push_byte(b'>');
    }

    /// Emit `prefix:local` for an element, or just `local` when no
    /// namespace prefix is set.  In non-c-abi builds where the
    /// element name still carries the QName, this is equivalent to
    /// `push_str(el.name())`.
    #[inline]
    fn write_element_qname(&mut self, el: &Node<'_>) {
        #[cfg(feature = "c-abi")]
        if let Some(ns) = el.namespace.get() {
            if let Some(prefix) = ns.prefix() {
                self.buf.push_str(prefix);
                self.buf.push_byte(b':');
            }
        }
        self.buf.push_str(el.name());
    }

    fn write_indent(&mut self, depth: usize) {
        for _ in 0..depth {
            self.buf.push_str(&self.opts.indent);
        }
    }
}

/// Returns true if `el` has any direct text-like child (text /
/// CDATA / entity-ref).  Used to mirror libxml2's XML pretty-print
/// behaviour: as soon as one text child is present in an element,
/// formatting is disabled for that element's content so existing
/// whitespace round-trips unchanged.
fn contains_text_child(el: &Node<'_>) -> bool {
    el.children().any(|c| matches!(
        c.kind,
        NodeKind::Text | NodeKind::CData | NodeKind::EntityRef
    ))
}

/// libxml2's HTML serializer skips the newline-and-indent around an
/// element's content when:
///
/// * the element has only one child (so `cur->children == cur->last`), OR
/// * the first/last child is a text node or entity-ref, OR
/// * the element is in the "p, pre, param" formatting-sensitive family.
///
/// Returns true for any of those, signaling that the caller should
/// emit `<tag>...children...</tag>` without inserting whitespace.
/// Matches libxml2 2.15.3 `HTMLtree.c::htmlNodeDumpInternal`
/// (around lines 969-977 for the opening newline and 1085-1091 for
/// the closing one — both gates are functionally the same).
fn html_skip_indent(el: &Node<'_>) -> bool {
    // p, pre, param — libxml2's `cur->name[0] != 'p'` check covers
    // every element whose name starts with 'p' (a slight over-match,
    // but that's what the reference code does).
    let name = el.name();
    if name.as_bytes().first().copied() == Some(b'p') {
        return true;
    }
    // Count children and remember first / last.
    let mut count = 0usize;
    let mut first: Option<&Node> = None;
    let mut last:  Option<&Node> = None;
    for child in el.children() {
        if first.is_none() { first = Some(child); }
        last = Some(child);
        count += 1;
    }
    if count <= 1 { return true; }
    // Multiple children: skip only when first or last is text /
    // entity-ref (matches libxml2's `children->type != HTML_TEXT_NODE`
    // and `last->type != HTML_TEXT_NODE` gates).
    let is_text_like = |n: &Node| matches!(n.kind, NodeKind::Text | NodeKind::CData);
    if let Some(f) = first { if is_text_like(f) { return true; } }
    if let Some(l) = last  { if is_text_like(l) { return true; } }
    false
}

/// Render inline when the element has exactly one significant child and that
/// child is text.  Matches `crate::serializer::is_inline` semantics.
fn is_inline(el: &Node<'_>) -> bool {
    let mut count = 0;
    let mut text_only = true;
    for child in el.children() {
        let is_ws_text = child.kind == NodeKind::Text && child.content().trim().is_empty();
        if is_ws_text { continue; }
        count += 1;
        if child.kind != NodeKind::Text { text_only = false; }
        if count > 1 { return false; }
    }
    count == 1 && text_only
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_str;
    use crate::options::ParseOptions;

    fn parse(xml: &str) -> Document {
        parse_str(xml, &ParseOptions::default()).expect("parse")
    }

    fn serialize_no_decl(doc: &Document) -> String {
        serialize_with(doc, &SerializeOptions {
            write_xml_decl: false,
            format:         false,
            indent:         "  ".into(),
            html_mode:      false,
            xhtml:          false,
            out_charset:    OutputCharset::Utf8,
        })
    }

    #[test]
    fn empty_element_self_closing() {
        let doc = parse("<r/>");
        assert_eq!(serialize_no_decl(&doc), "<r/>");
    }

    #[test]
    fn element_with_text() {
        let doc = parse("<r>hello</r>");
        assert_eq!(serialize_no_decl(&doc), "<r>hello</r>");
    }

    #[test]
    fn attributes_preserved_in_source_order() {
        let doc = parse(r#"<el id="1" class="x" data-y="42"/>"#);
        let out = serialize_no_decl(&doc);
        assert_eq!(out, r#"<el id="1" class="x" data-y="42"/>"#);
    }

    #[test]
    fn nested_elements() {
        let doc = parse("<a><b><c/></b></a>");
        assert_eq!(serialize_no_decl(&doc), "<a><b><c/></b></a>");
    }

    #[test]
    fn text_special_chars_escaped() {
        let doc = parse("<r>&lt;hi&amp;there&gt;</r>");
        // After parse: text content is "<hi&there>".  Re-serialized with escapes.
        let out = serialize_no_decl(&doc);
        assert!(out.contains("&lt;hi&amp;there&gt;"), "got: {out}");
    }

    #[test]
    fn attribute_quotes_escaped() {
        let doc = parse(r#"<r a="&quot;x&quot;"/>"#);
        let out = serialize_no_decl(&doc);
        assert!(out.contains("&quot;x&quot;"), "got: {out}");
    }

    #[test]
    fn cdata_preserved() {
        let doc = parse("<r><![CDATA[<raw>]]></r>");
        assert_eq!(serialize_no_decl(&doc), "<r><![CDATA[<raw>]]></r>");
    }

    #[test]
    fn comments_preserved() {
        let doc = parse("<r><!-- hi --></r>");
        assert_eq!(serialize_no_decl(&doc), "<r><!-- hi --></r>");
    }

    #[test]
    fn pi_preserved() {
        let doc = parse(r#"<r><?xml-stylesheet href="s.xsl"?></r>"#);
        assert_eq!(serialize_no_decl(&doc), r#"<r><?xml-stylesheet href="s.xsl"?></r>"#);
    }

    #[test]
    fn xml_decl_emitted_by_default() {
        let doc = parse("<r/>");
        let out = serialize_to_string(&doc);
        // No `<?xml encoding=...?>` declaration in source → emit
        // only `version` (matches libxml2's behaviour when
        // doc->encoding is NULL).
        assert!(out.starts_with("<?xml version=\"1.0\"?>"), "got: {out}");
    }

    #[test]
    fn round_trip_preserves_structure() {
        // Parse → serialize → parse again → compare structure.
        let original = r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title>X</title><id>1</id></entry></feed>"#;
        let doc1 = parse(original);
        let xml  = serialize_no_decl(&doc1);
        let doc2 = parse(&xml);
        // Element names match
        assert_eq!(doc1.root().name(), doc2.root().name());
        // Same number of children at each level
        let entry1 = doc1.root().first_child.get().unwrap();
        let entry2 = doc2.root().first_child.get().unwrap();
        assert_eq!(entry1.children().count(), entry2.children().count());
    }

    #[test]
    fn pretty_print_block_layout() {
        let doc = parse("<r><a/><b/></r>");
        let out = serialize_formatted(&doc);
        assert!(out.contains("<r>\n"));
        assert!(out.contains("  <a/>"));
        assert!(out.contains("  <b/>"));
        assert!(out.contains("\n</r>"));
    }

    #[test]
    fn pretty_print_inline_text() {
        let doc = parse("<r><title>Hello</title></r>");
        let out = serialize_formatted(&doc);
        // Inline text element stays on one line
        assert!(out.contains("<title>Hello</title>"), "got: {out}");
    }

    // ── public entry points ─────────────────────────────────────────────

    #[test]
    fn serialize_to_bytes_matches_to_string() {
        let doc = parse("<r/>");
        let bytes = serialize_to_bytes(&doc);
        let s = serialize_to_string(&doc);
        assert_eq!(bytes, s.into_bytes());
    }

    #[test]
    fn serialize_node_to_string_emits_just_the_subtree() {
        // No XML decl, no document wrapping — just the node and its
        // descendants.
        let doc = parse("<r><a id='1'><b/></a></r>");
        let a = doc.root().first_child.get().unwrap();
        let out = serialize_node_to_string(a, &SerializeOptions {
            write_xml_decl: false, format: false,
            indent: "  ".into(), html_mode: false, xhtml: false, out_charset: OutputCharset::Utf8,
        });
        assert_eq!(out, r#"<a id="1"><b/></a>"#);
    }

    // ── XML declaration variants ────────────────────────────────────────

    #[test]
    fn xml_decl_with_encoding_and_standalone() {
        let doc = parse(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><r/>"#);
        let out = serialize_to_string(&doc);
        assert!(out.starts_with("<?xml version=\"1.0\""), "got: {out}");
        assert!(out.contains("encoding=\"UTF-8\""), "got: {out}");
        assert!(out.contains("standalone=\"yes\""), "got: {out}");
    }

    #[test]
    fn xml_decl_standalone_no() {
        let doc = parse(r#"<?xml version="1.0" standalone="no"?><r/>"#);
        let out = serialize_to_string(&doc);
        assert!(out.contains("standalone=\"no\""), "got: {out}");
    }

    // ── HTML mode ───────────────────────────────────────────────────────

    #[cfg(feature = "html")]
    fn parse_html(html: &str) -> Document {
        crate::html::parse_html_str(html).expect("parse html")
    }

    #[cfg(feature = "html")]
    #[test]
    fn html_void_elements_emit_no_close_tag() {
        let doc = parse_html("<html><body><br/><img src='x'/><hr/></body></html>");
        let out = serialize_html_to_string(&doc);
        // <br>, <img>, <hr> should NOT self-close and should NOT have a close tag.
        assert!(out.contains("<br>"), "got: {out}");
        assert!(out.contains("<img"), "got: {out}");
        assert!(out.contains("<hr>"), "got: {out}");
        assert!(!out.contains("</br>"), "got: {out}");
        assert!(!out.contains("<br/>"), "got: {out}");
    }

    #[cfg(feature = "html")]
    #[test]
    fn html_empty_non_void_elements_emit_explicit_close() {
        let doc = parse_html("<html><body><div></div></body></html>");
        let out = serialize_html_to_string(&doc);
        // <div></div> not <div/>.
        assert!(out.contains("<div></div>"), "got: {out}");
        assert!(!out.contains("<div/>"), "got: {out}");
    }

    #[cfg(feature = "html")]
    #[test]
    fn html_boolean_attribute_shorthand() {
        let doc = parse_html(r#"<html><body><input disabled=""></body></html>"#);
        let out = serialize_html_to_string(&doc);
        // Empty-value attr → shorthand `<input disabled>` (no =).
        assert!(out.contains("<input disabled>") || out.contains("<input disabled "), "got: {out}");
        assert!(!out.contains("disabled=\""), "got: {out}");
    }

    #[test]
    fn html_boolean_attr_recognised_case_insensitive() {
        // Known boolean attributes minimize regardless of value;
        // recognition is by name, case-insensitively.
        assert!(is_html_boolean_attr("disabled"));
        assert!(is_html_boolean_attr("DISABLED"));
        assert!(is_html_boolean_attr("selected"));
        assert!(is_html_boolean_attr("checked"));
        assert!(!is_html_boolean_attr("href"));
        assert!(!is_html_boolean_attr("class"));
    }

    #[cfg(feature = "html")]
    #[test]
    fn html_raw_text_elements_emit_verbatim_content() {
        // <script> and <style> bodies must NOT be entity-escaped.
        let doc = parse_html(
            "<html><body><script>if (x < 3 && y > 1) {}</script></body></html>",
        );
        let out = serialize_html_to_string(&doc);
        // Raw content should NOT have entity escapes inside <script>.
        assert!(out.contains("if (x < 3 && y > 1) {}"), "got: {out}");
    }

    #[test]
    fn html_void_element_predicate() {
        assert!(is_html_void_element("br"));
        assert!(is_html_void_element("img"));
        assert!(is_html_void_element("input"));
        assert!(is_html_void_element("meta"));
        assert!(is_html_void_element("keygen"));
        assert!(is_html_void_element("menuitem"));
        assert!(!is_html_void_element("div"));
        assert!(!is_html_void_element("span"));
    }

    #[test]
    fn html_raw_text_predicate() {
        assert!(is_html_raw_text_element("script"));
        assert!(is_html_raw_text_element("style"));
        assert!(!is_html_raw_text_element("div"));
    }
}
