//! Miscellaneous node accessors used heavily by lxml.
//!
//!   - [`xmlGetLineNo`] — line number of a node (sourceline).
//!   - [`xmlGetNodePath`] — `/r/a[2]/b` style XPath-ish locator.
//!   - [`xmlNodeGetBase`] — `xml:base` lookup walking ancestors.
//!   - [`xmlNodeSetName`] — rename a node.
//!   - [`xmlNodeSetBase`] — set `xml:base` (writes an attribute).
//!   - [`xmlNodeBufGetContent`] — flat-text into an `xmlBuffer`.
//!   - [`xmlHasFeature`] — feature-test (always 0 for us).

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_long};
use std::ptr;

use sup_xml_tree::dom::{Node, NodeKind};

use crate::alloc::{alloc_registered_buffer, alloc_registered_cstring};
use crate::outbuf::xmlBuffer;

#[inline]
unsafe fn node_ref<'a>(n: *const Node<'static>) -> Option<&'a Node<'a>> {
    if n.is_null() { None } else { Some(unsafe { &*(n as *const Node<'a>) }) }
}

/// `xmlGetLineNo(node)` — return the line number recorded by the parser,
/// or -1 if unknown.  The ABI `node.line` slot is a `u16` (libxml2's
/// `unsigned short`) and saturates at 65535; the parser also records the
/// uncapped line in `node.full_line`, which we return in preference so
/// `sourceline` is correct in files longer than 65535 lines.  Nodes built
/// through the tree API (not the parser) leave `full_line` at 0 and fall
/// back to `line`.
///
/// Deliberate divergence (see crate-level docs, "Behavioral divergences"):
/// past 65535 lines libxml2 keeps the saturated 65535 and recurses into a
/// text child / sibling, returning *that* node's line — so `<br/>` reports
/// its neighbour's position.  We return the node's own real line instead.
/// `test_large_sourceline_XML` pins libxml2's recursion artifact.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetLineNo(node: *const Node<'static>) -> c_long {
    match unsafe { node_ref(node) } {
        Some(n) if n.full_line != 0 => n.full_line as c_long,
        Some(n) => n.line as c_long,
        None => -1,
    }
}

/// `xmlGetNodePath(node)` — build an XPath-ish locator string for the
/// node.  Walks from the node up to the document root; child elements
/// get a `[N]` suffix when there are siblings with the same name.
///
/// Returns a `xmlChar*` the caller releases with `xmlFree`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetNodePath(
    node: *const Node<'static>,
) -> *mut c_char {
    let n = match unsafe { node_ref(node) } { Some(n) => n, None => return ptr::null_mut() };
    // Walk up, prepending segments to a Vec<String>.  Stop at the
    // document node: top-level elements have their `parent` pointer
    // set to the doc-cast-as-Node so `xmlUnlinkNode` on
    // prolog/epilogue siblings can find the doc's `children` head
    // (see `Document::into_xml_doc`).  For path computation we
    // ignore it — libxml2's `xmlGetNodePath` produces `/root`, not
    // `/*/root`.
    let mut segments: Vec<String> = Vec::new();
    let mut cur: Option<&Node<'_>> = Some(n);
    while let Some(c) = cur {
        if matches!(c.kind, NodeKind::Document) {
            break;
        }
        segments.push(segment_for(c));
        cur = c.parent.get();
    }
    segments.reverse();
    let path = format!("/{}", segments.join("/"));
    alloc_registered_cstring(path.as_bytes())
}

fn segment_for(n: &Node<'_>) -> String {
    match n.kind {
        NodeKind::Element => {
            // Count preceding siblings with the same name.
            let my_name = n.name();
            let mut idx = 1usize;
            let mut sib = n.prev_sibling.get();
            while let Some(s) = sib {
                if matches!(s.kind, NodeKind::Element) && s.name() == my_name {
                    idx += 1;
                }
                sib = s.prev_sibling.get();
            }
            // Check for trailing siblings of same name — if none, omit [1].
            let mut has_trailing = false;
            let mut nx = n.next_sibling.get();
            while let Some(s) = nx {
                if matches!(s.kind, NodeKind::Element) && s.name() == my_name {
                    has_trailing = true;
                    break;
                }
                nx = s.next_sibling.get();
            }
            if idx == 1 && !has_trailing {
                my_name.to_string()
            } else {
                format!("{my_name}[{idx}]")
            }
        }
        NodeKind::Text     => "text()".to_string(),
        NodeKind::CData    => "text()".to_string(),
        NodeKind::Comment  => "comment()".to_string(),
        NodeKind::Pi       => format!("processing-instruction('{}')", n.name()),
        _                  => "*".to_string(),
    }
}

/// `xmlNodeGetBase(doc, node)` — return `xml:base` for `node` or the
/// nearest ancestor that has one.  Returns a `xmlChar*` (caller
/// xmlFrees) or NULL if none in scope.  `doc` is unused — the
/// information lives on the nodes themselves.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNodeGetBase(
    doc:   *const std::os::raw::c_void,
    node:  *const Node<'static>,
) -> *mut c_char {
    // HTML documents take their base from the first `<base href>` inside
    // `<html><head>`, not from `xml:base` — matching libxml2's HTML branch
    // of `xmlNodeGetBase`.  A document with no `<base>` falls through to the
    // document-URL fallback below (where lxml's `.base` would also land).
    if !doc.is_null() {
        let xd = doc as *const sup_xml_tree::dom::XmlDoc;
        if unsafe { (*xd)._doc.is_html() } {
            if let Some(href) = unsafe { html_base_href(xd) } {
                return alloc_registered_cstring(href.as_bytes());
            }
            return unsafe { doc_url_or_null(doc) };
        }
    }
    let mut cur = unsafe { node_ref(node) };
    while let Some(n) = cur {
        // Stop at the doc node — top-level elements' `parent`
        // points there but it carries no `xml:base` attribute.
        if matches!(n.kind, NodeKind::Document) {
            break;
        }
        if matches!(n.kind, NodeKind::Element) {
            for attr in n.attributes() {
                if attr.name() == "xml:base" || local_tail(attr.name()) == "base" {
                    let val = attr.value();
                    if !val.is_empty() {
                        return alloc_registered_cstring(val.as_bytes());
                    }
                }
            }
        }
        cur = n.parent.get();
    }
    // Fall back to the document's URL (libxml2 §8.5).  This is
    // what xsl:import / xsl:include resolution relies on: the
    // base for a relative href is the URL of the stylesheet
    // that contains the import.
    unsafe { doc_url_or_null(doc) }
}

/// The document's `URL` (offset 136) as a freshly-registered `xmlChar*`,
/// or NULL when the doc is NULL / carries no URL.
unsafe fn doc_url_or_null(doc: *const std::os::raw::c_void) -> *mut c_char {
    if doc.is_null() {
        return ptr::null_mut();
    }
    let url = unsafe { *((doc as *const u8).add(136) as *const *const c_char) };
    if url.is_null() {
        return ptr::null_mut();
    }
    let s = unsafe { std::ffi::CStr::from_ptr(url) };
    if s.to_bytes().is_empty() {
        return ptr::null_mut();
    }
    alloc_registered_cstring(s.to_bytes())
}

/// Walk `<html><head>` for the first `<base>` element and return its
/// `href` attribute value.  Mirrors libxml2's HTML `xmlNodeGetBase`
/// traversal (descend through `html`/`head`, return on `base`).
unsafe fn html_base_href(xd: *const sup_xml_tree::dom::XmlDoc) -> Option<String> {
    let mut cur = unsafe { node_ref((*xd).children.get()) };
    while let Some(n) = cur {
        if !matches!(n.kind, NodeKind::Element) {
            cur = n.next_sibling.get();
            continue;
        }
        match n.name() {
            name if name.eq_ignore_ascii_case("html") || name.eq_ignore_ascii_case("head") => {
                cur = n.first_child.get();
            }
            name if name.eq_ignore_ascii_case("base") => {
                return n.attributes()
                    .find(|a| a.name().eq_ignore_ascii_case("href"))
                    .map(|a| a.value().to_string());
            }
            _ => cur = n.next_sibling.get(),
        }
    }
    None
}

fn local_tail(s: &str) -> &str {
    match s.rfind(':') {
        Some(i) => &s[i + 1..],
        None    => s,
    }
}

/// `xmlNodeSetBase(node, base)` — set the `xml:base` attribute on
/// `node`.  No-op if `node` isn't an element.  We construct via the
/// existing attribute mutation path.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNodeSetBase(
    node: *mut Node<'static>,
    base: *const c_char,
) {
    if node.is_null() {
        return;
    }
    // `xml:base` is `base` in the XML namespace.  Set it through
    // xmlSetNsProp with that namespace (libxml2 resolves the `xml:`
    // prefix the same way), so the attribute is stored as local `base`
    // with the prefix on `attr->ns`.  A bare xmlSetProp("xml:base")
    // would create an un-namespaced attribute named "xml:base", which
    // `xmlGetNsProp` / lxml's `.get('{xml-ns}base')` can't find.
    let n = match unsafe { node_ref(node) } { Some(n) => n, None => return };
    let doc_ptr = n.doc.get() as *mut sup_xml_tree::dom::XmlDoc;
    let d = match unsafe { crate::mutate::doc_ref(doc_ptr) } { Some(d) => d, None => return };
    let xml_ns = d.bump_new_namespace(Some("xml"), "http://www.w3.org/XML/1998/namespace");
    let xml_ns_ptr = xml_ns as *const sup_xml_tree::dom::Namespace<'_>
        as *mut sup_xml_tree::dom::Namespace<'static>;
    let name = match std::ffi::CString::new("base") {
        Ok(c) => c,
        Err(_) => return,
    };
    let _ = unsafe { crate::mutate::xmlSetNsProp(node, xml_ns_ptr, name.as_ptr(), base) };
}

/// `xmlNodeBufGetContent(buffer, node)` — append `node`'s flat text
/// content to `buffer`.  Returns 0 on success, -1 on NULL inputs.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNodeBufGetContent(
    buffer: *mut xmlBuffer,
    node:   *const Node<'static>,
) -> c_int {
    if buffer.is_null() || node.is_null() {
        return -1;
    }
    let n = match unsafe { node_ref(node) } { Some(n) => n, None => return -1 };
    let mut buf = Vec::<u8>::new();
    collect_content(n, &mut buf);
    // Append into the xmlBuffer via outbuf's append (private — we
    // duplicate the inline logic here to avoid exposing it).
    unsafe {
        // Use xmlBufferWriteChar for each chunk?  Or write a single
        // contiguous block?  CString interface needs NUL-terminated
        // input, so allocate one.
        let c = std::ffi::CString::new(buf).unwrap_or_default();
        crate::outbuf::xmlBufferWriteChar(buffer, c.as_ptr());
    }
    0
}

fn collect_content(n: &Node<'_>, out: &mut Vec<u8>) {
    match n.kind {
        NodeKind::Text | NodeKind::CData => out.extend_from_slice(n.content().as_bytes()),
        NodeKind::Element => {
            for c in n.children() {
                collect_content(c, out);
            }
        }
        _ => {}
    }
}

/// `xmlHasFeature(feature)` — runtime feature test over libxml2's
/// `xmlFeature` enum.  Reports the optional capabilities sup-xml
/// actually implements; lxml builds `etree.LIBXML_FEATURES` from this.
/// Capabilities we don't provide — network (FTP/HTTP), iconv, gzip/lzma
/// compression, ICU, the catalog resolver, loadable modules, and the
/// debug builds — report 0.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlHasFeature(feature: c_int) -> c_int {
    const SUPPORTED: &[c_int] = &[
        1,  // XML_WITH_THREAD
        2,  // XML_WITH_TREE
        3,  // XML_WITH_OUTPUT
        4,  // XML_WITH_PUSH
        5,  // XML_WITH_READER
        6,  // XML_WITH_PATTERN
        7,  // XML_WITH_WRITER
        11, // XML_WITH_VALID
        12, // XML_WITH_HTML
        14, // XML_WITH_C14N
        16, // XML_WITH_XPATH
        18, // XML_WITH_XINCLUDE
        21, // XML_WITH_UNICODE
        22, // XML_WITH_REGEXP
        25, // XML_WITH_SCHEMAS
        26, // XML_WITH_SCHEMATRON
    ];
    c_int::from(SUPPORTED.contains(&feature))
}

// ── node-introspection helpers ──────────────────────────────────────────

/// `xmlNodeGetSpacePreserve(node)` — walk `node` and its ancestors
/// looking for an `xml:space` attribute.  Returns:
/// * `1`  if the nearest in-scope value is `"preserve"`
/// * `0`  if it is `"default"`
/// * `-1` if no `xml:space` is in scope (or `node` is NULL)
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNodeGetSpacePreserve(node: *const Node<'static>) -> c_int {
    let mut cur = unsafe { node_ref(node) };
    while let Some(n) = cur {
        if matches!(n.kind, NodeKind::Document) { break; }
        if matches!(n.kind, NodeKind::Element) {
            for attr in n.attributes() {
                if attr.name() == "xml:space" || local_tail(attr.name()) == "space" {
                    return match attr.value() {
                        "preserve" => 1,
                        "default"  => 0,
                        _          => -1,
                    };
                }
            }
        }
        cur = n.parent.get();
    }
    -1
}

/// `xmlNodeListGetString(doc, list, inLine)` — flatten a sibling
/// chain to a single string.  `inLine == 0` would re-escape entity
/// references in the output; we always entity-decode at parse time so
/// the flag is a no-op for us.  Returns a freshly-allocated string
/// (caller `xmlFree`s); NULL on NULL `list`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNodeListGetString(
    _doc: *const std::os::raw::c_void,
    list: *const Node<'static>,
    _in_line: c_int,
) -> *mut c_char {
    if list.is_null() {
        return ptr::null_mut();
    }
    let mut out: Vec<u8> = Vec::new();
    let mut cur = unsafe { node_ref(list) };
    while let Some(n) = cur {
        match n.kind {
            NodeKind::Text | NodeKind::CData => {
                out.extend_from_slice(n.content().as_bytes());
            }
            NodeKind::Element => {
                // Recurse into element children — matches libxml2's
                // behaviour of flattening nested text.
                if let Some(child) = n.first_child.get() {
                    let inner_ptr = child as *const Node<'_> as *const Node<'static>;
                    let inner = unsafe { xmlNodeListGetString(ptr::null(), inner_ptr, _in_line) };
                    if !inner.is_null() {
                        let bytes = unsafe { CStr::from_ptr(inner) }.to_bytes().to_vec();
                        unsafe { crate::parse::xml_free_impl(inner as *mut std::os::raw::c_void); }
                        out.extend_from_slice(&bytes);
                    }
                }
            }
            _ => {}
        }
        cur = n.next_sibling.get();
    }
    alloc_registered_cstring(&out)
}

/// `xmlGetNsList(doc, node)` — collect every namespace declaration
/// in scope at `node` (walking ancestors).  Returns a NULL-terminated
/// array of `xmlNs*` pointers.  Caller releases the array via
/// `xmlFree` (the namespace objects themselves are arena-owned and
/// not freed).  Returns NULL when no namespaces are in scope.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlGetNsList(
    _doc: *const std::os::raw::c_void,
    node: *const Node<'static>,
) -> *mut *mut sup_xml_tree::dom::Namespace<'static> {
    let mut cur = unsafe { node_ref(node) };
    let mut out: Vec<*mut sup_xml_tree::dom::Namespace<'static>> = Vec::new();
    let mut seen_prefixes: Vec<Option<&str>> = Vec::new();
    while let Some(n) = cur {
        if matches!(n.kind, NodeKind::Document) { break; }
        if matches!(n.kind, NodeKind::Element) {
            let mut ns_cur = n.ns_def.get();
            while let Some(ns) = ns_cur {
                let prefix = ns.prefix();
                if !seen_prefixes.contains(&prefix) {
                    seen_prefixes.push(prefix);
                    out.push(ns as *const _ as *mut sup_xml_tree::dom::Namespace<'static>);
                }
                ns_cur = ns.next.get();
            }
        }
        cur = n.parent.get();
    }
    if out.is_empty() {
        return ptr::null_mut();
    }
    // NULL-terminate, then hand the caller a heap copy released via
    // xmlFree.  The payload is a pointer array, not a C string —
    // pointer values carry interior NUL bytes (the high bytes of every
    // heap address on 64-bit), so it must go through the byte-length-
    // authoritative `alloc_registered_buffer`.  `alloc_registered_cstring`
    // would truncate at the first interior NUL, returning an
    // under-sized, unterminated array.
    out.push(ptr::null_mut());
    let layout_size = out.len() * std::mem::size_of::<*mut sup_xml_tree::dom::Namespace<'static>>();
    let mut bytes = vec![0u8; layout_size];
    // SAFETY: bytes is a Vec<u8> of exactly the right size + alignment
    // (8 bytes for *mut on 64-bit, matched by u8 vec's allocator
    // returning 8-byte-aligned starts).  Copy pointer values in.
    unsafe {
        std::ptr::copy_nonoverlapping(
            out.as_ptr() as *const u8,
            bytes.as_mut_ptr(),
            layout_size,
        );
    }
    let result = alloc_registered_buffer(&bytes);
    result as *mut *mut sup_xml_tree::dom::Namespace<'static>
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    use crate::parse::{xmlDocGetRootElement, xmlFreeDoc, xmlReadMemory};

    fn parse(src: &[u8]) -> *mut sup_xml_tree::dom::XmlDoc {
        let doc = unsafe {
            xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(),
                ptr::null(),
                0,
            )
        };
        assert!(!doc.is_null());
        doc
    }

    #[test]
    fn line_no_returns_parsed_line() {
        // Source layout:
        //   line 1:  <r>
        //   line 2:    <a/>
        //   line 3:    <b/>
        //   line 4:  </r>
        // Verifies exact line numbers, not just non-negative — the
        // previous "line >= 0" assertion was satisfied by garbage.
        let doc = parse(b"<r>\n  <a/>\n  <b/>\n</r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let a = unsafe { crate::tree::xmlFirstElementChild(root) };
        let b = unsafe { crate::tree::xmlNextElementSibling(a) };

        assert_eq!(unsafe { xmlGetLineNo(root) }, 1, "root <r> on line 1");
        assert_eq!(unsafe { xmlGetLineNo(a) },    2, "<a/> on line 2");
        assert_eq!(unsafe { xmlGetLineNo(b) },    3, "<b/> on line 3");

        unsafe { xmlFreeDoc(doc); }
    }

    /// Regression guard for an O(N²) bug: a previous implementation
    /// called `compute_line_col` per StartElement, which rescans
    /// `src[0..offset]` each call.  Many elements across many lines
    /// exercises the incremental cursor we now use — and confirms that
    /// lines are still accurate when StartElements are not on adjacent
    /// lines.  Without the cursor advancing correctly, later elements
    /// would still report the line of the first element.
    #[test]
    fn line_no_accurate_across_many_lines() {
        // Build a 300-element doc, one element per line.  Each <e
        // i="N"/> sits on line N+1 (the prolog is line 1).
        let mut src = String::from("<root>\n");
        let n = 300;
        for i in 0..n {
            src.push_str(&format!("  <e i=\"{i}\"/>\n"));
        }
        src.push_str("</root>\n");

        let doc = parse(src.as_bytes());
        let root = unsafe { xmlDocGetRootElement(doc) };
        assert_eq!(unsafe { xmlGetLineNo(root) }, 1);

        // Walk every child and check its line number matches its position.
        let mut cur = unsafe { crate::tree::xmlFirstElementChild(root) };
        let mut expected_line: c_long = 2;
        let mut walked = 0;
        while !cur.is_null() {
            let got = unsafe { xmlGetLineNo(cur) };
            assert_eq!(got, expected_line,
                "child #{walked}: expected line {expected_line}, got {got}");
            walked += 1;
            expected_line += 1;
            cur = unsafe { crate::tree::xmlNextElementSibling(cur) };
        }
        assert_eq!(walked, n, "should have walked all {n} children");

        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn node_path_basic() {
        let doc = parse(b"<r><a/><a/><b/></r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let a1 = unsafe { crate::tree::xmlFirstElementChild(root) };
        let a2 = unsafe { crate::tree::xmlNextElementSibling(a1) };
        let b  = unsafe { crate::tree::xmlNextElementSibling(a2) };

        let p_root = unsafe { xmlGetNodePath(root) };
        let p_a1   = unsafe { xmlGetNodePath(a1) };
        let p_a2   = unsafe { xmlGetNodePath(a2) };
        let p_b    = unsafe { xmlGetNodePath(b) };

        let s_root = unsafe { CStr::from_ptr(p_root) }.to_str().unwrap();
        let s_a1   = unsafe { CStr::from_ptr(p_a1)   }.to_str().unwrap();
        let s_a2   = unsafe { CStr::from_ptr(p_a2)   }.to_str().unwrap();
        let s_b    = unsafe { CStr::from_ptr(p_b)    }.to_str().unwrap();

        assert_eq!(s_root, "/r");
        assert_eq!(s_a1, "/r/a[1]");
        assert_eq!(s_a2, "/r/a[2]");
        // `b` is unique among r's children → no [N] suffix.
        assert_eq!(s_b, "/r/b");

        unsafe {
            crate::parse::xml_free_impl(p_root as *mut _);
            crate::parse::xml_free_impl(p_a1   as *mut _);
            crate::parse::xml_free_impl(p_a2   as *mut _);
            crate::parse::xml_free_impl(p_b    as *mut _);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn node_get_base_walks_ancestors() {
        let doc = parse(
            b"<r xml:base=\"http://a/\">\
                <inner>\
                  <deep/>\
                </inner>\
              </r>",
        );
        let root = unsafe { xmlDocGetRootElement(doc) };
        let inner = unsafe { crate::tree::xmlFirstElementChild(root) };
        let deep = unsafe { crate::tree::xmlFirstElementChild(inner) };
        let base = unsafe { xmlNodeGetBase(ptr::null(), deep) };
        assert!(!base.is_null(), "expected to find xml:base");
        let s = unsafe { CStr::from_ptr(base) }.to_str().unwrap();
        assert_eq!(s, "http://a/");
        unsafe {
            crate::parse::xml_free_impl(base as *mut _);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn has_feature_reports_supported_only() {
        // XPath (16) and HTML (12) are implemented; FTP (9) and an
        // out-of-range id are not.
        assert_eq!(unsafe { xmlHasFeature(16) }, 1);
        assert_eq!(unsafe { xmlHasFeature(12) }, 1);
        assert_eq!(unsafe { xmlHasFeature(9) }, 0);
        assert_eq!(unsafe { xmlHasFeature(99) }, 0);
    }

    fn parse_b(src: &[u8]) -> *mut sup_xml_tree::dom::XmlDoc {
        let doc = unsafe {
            crate::parse::xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(), ptr::null(), 0,
            )
        };
        assert!(!doc.is_null());
        doc
    }

    #[test]
    fn space_preserve_walks_ancestors() {
        // <r xml:space="preserve"><inner>...</inner></r>
        let doc = parse_b(b"<r xml:space=\"preserve\"><inner/></r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let inner = unsafe { crate::tree::xmlFirstElementChild(root) };
        assert_eq!(unsafe { xmlNodeGetSpacePreserve(root)  }, 1);
        assert_eq!(unsafe { xmlNodeGetSpacePreserve(inner) }, 1, "inherits from ancestor");
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn space_preserve_returns_minus_one_when_absent() {
        let doc = parse_b(b"<r><c/></r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        assert_eq!(unsafe { xmlNodeGetSpacePreserve(root) }, -1);
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn node_list_get_string_flattens_text() {
        let doc = parse_b(b"<r>hello <em>bold</em> world</r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        let first_child = unsafe { (*root).first_child.get().unwrap() } as *const _;
        let s = unsafe { xmlNodeListGetString(ptr::null(), first_child, 0) };
        let bytes = unsafe { CStr::from_ptr(s) }.to_str().unwrap();
        assert_eq!(bytes, "hello bold world");
        unsafe { crate::parse::xml_free_impl(s as *mut _); }
        unsafe { xmlFreeDoc(doc); }
    }

    #[test]
    fn get_ns_list_collects_in_scope_namespaces() {
        let doc = parse_b(b"<r xmlns:a=\"urn:a\" xmlns:b=\"urn:b\"><c xmlns:a=\"urn:override\"/></r>");
        let root = unsafe { crate::parse::xmlDocGetRootElement(doc) };
        let c = unsafe { crate::tree::xmlFirstElementChild(root) };
        let arr = unsafe { xmlGetNsList(ptr::null(), c) };
        assert!(!arr.is_null(), "expected non-NULL ns list with c having an xmlns:a");
        // Walk the NULL-terminated array.  Should yield at least one
        // ns (the local `xmlns:a` override).  Whether the parent's
        // `xmlns:a`/`xmlns:b` appear depends on whether ns_def was
        // populated by the parser — c-abi build always; lean build
        // doesn't.  Bound the walk so a bug can't loop forever.
        let mut count = 0;
        unsafe {
            let mut p = arr;
            while !(*p).is_null() {
                count += 1;
                p = p.add(1);
                assert!(count < 10, "runaway ns-list walk");
            }
            crate::parse::xml_free_impl(arr as *mut _);
        }
        assert!(count >= 1, "expected at least the local xmlns:a binding");
        unsafe { crate::parse::xmlFreeDoc(doc); }
    }

}
