//! Probe what libxml2's `XML_PARSE_RECOVER` actually produces for
//! the recovery cases below (bare `&` in text, `]]>` in text,
//! malformed XML declaration).  We need to know the exact tree
//! libxml2 builds in order to implement matching recovery in
//! sup-xml.
//!
//! For each input, we walk the libxml2 tree and report:
//!   - whether parsing produced a doc
//!   - the root element name (if any)
//!   - the concatenated text content of the root element
//!   - the count of immediate child element nodes
//!
//! Run with:
//!     cargo bench -p sup-xml-bench --bench libxml2_recovery_inspector

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};

// libxml2 xmlNode struct — first ~9 fields are stable across
// versions and described in the public header.  We use #[repr(C)]
// + matching layout so we can read `name` and walk `children` /
// `next` without going through helper functions.
#[allow(non_camel_case_types)]
enum XmlDoc {}
#[allow(non_camel_case_types)]
#[repr(C)]
struct XmlNode {
    _private:    *mut c_void,
    type_:       c_int,
    name:        *const c_char,    // xmlChar* = unsigned char*
    children:    *mut XmlNode,
    last:        *mut XmlNode,
    parent:      *mut XmlNode,
    next:        *mut XmlNode,
    prev:        *mut XmlNode,
    doc:         *mut XmlDoc,
    // (more fields follow that we don't read)
}

unsafe extern "C" {
    fn xmlReadMemory(buffer: *const c_char, size: c_int,
                     url: *const c_char, encoding: *const c_char,
                     options: c_int) -> *mut XmlDoc;
    fn xmlFreeDoc(doc: *mut XmlDoc);
    fn xmlDocGetRootElement(doc: *mut XmlDoc) -> *mut XmlNode;
    fn xmlNodeGetContent(node: *const XmlNode) -> *mut u8;   // xmlChar*
    // NOTE: libxml2's `xmlFree` is a global *function pointer*
    // (`xmlFreeFunc xmlFree`), not a function — calling it as
    // `extern "C" fn xmlFree(...)` segfaults.  We leak the
    // xmlNodeGetContent allocations instead; this is a diagnostic
    // bench that processes ~15 small cases and exits, so a few
    // hundred bytes of leak is fine.
}

const XML_PARSE_RECOVER:   c_int = 1 << 0;
const XML_PARSE_NOERROR:   c_int = 1 << 5;
const XML_PARSE_NOWARNING: c_int = 1 << 6;

const XML_ELEMENT_NODE: c_int = 1;

#[derive(Debug, Default)]
struct LibxmlInspect {
    accepted:        bool,
    root_name:       Option<String>,
    root_text:       Option<String>,
    root_child_elems: usize,
}

fn inspect(input: &[u8]) -> LibxmlInspect {
    // SAFETY: pointer + length to a borrowed slice; doc freed
    // immediately when out of scope.  Single-threaded.  Why
    // unsafe: only way to call into C.
    unsafe {
        let opts = XML_PARSE_RECOVER | XML_PARSE_NOERROR | XML_PARSE_NOWARNING;
        let doc = xmlReadMemory(
            input.as_ptr() as *const c_char, input.len() as c_int,
            std::ptr::null(), std::ptr::null(),
            opts,
        );
        if doc.is_null() {
            return LibxmlInspect::default();
        }
        let mut out = LibxmlInspect { accepted: true, ..Default::default() };
        let root = xmlDocGetRootElement(doc);
        if !root.is_null() {
            // Root name — null-terminated UTF-8 xmlChar* string.
            let name_ptr = (*root).name as *const c_char;
            if !name_ptr.is_null() {
                if let Ok(s) = CStr::from_ptr(name_ptr).to_str() {
                    out.root_name = Some(s.to_string());
                }
            }
            // Concatenated text content of the root.  We
            // intentionally leak the libxml2 allocation; see the
            // FFI block for why xmlFree can't be called.
            let content = xmlNodeGetContent(root);
            if !content.is_null() {
                let cstr = CStr::from_ptr(content as *const c_char);
                if let Ok(s) = cstr.to_str() {
                    out.root_text = Some(s.to_string());
                }
            }
            // Count direct element children.
            let mut child = (*root).children;
            while !child.is_null() {
                if (*child).type_ == XML_ELEMENT_NODE {
                    out.root_child_elems += 1;
                }
                child = (*child).next;
            }
        }
        xmlFreeDoc(doc);
        out
    }
}

const CASES: &[(&str, &str)] = &[
    // 1. Bare `&` in text content.
    ("bare & in text",                 "<r>tom & jerry</r>"),
    ("bare & at start of text",        "<r>& jerry</r>"),
    ("bare & followed by hash",        "<r>price & #5</r>"),
    ("multiple bare &",                "<r>a & b & c</r>"),

    // 2. `]]>` in text content.
    ("]]> in middle of text",          "<r>oops]]>more</r>"),
    ("]]> at start of text",           "<r>]]>after</r>"),
    ("]]> at end of text",             "<r>before]]></r>"),
    ("multiple ]]>",                   "<r>a]]>b]]>c</r>"),

    // 3. Malformed XML declaration.
    ("empty XML decl",                 "<?xml?><r/>"),
    ("XML decl no version",            "<?xml encoding='UTF-8'?><r/>"),
    ("XML decl bad version",           "<?xml version='1.0 ' ?><r/>"),
    ("XML decl after content",         "<r/><?xml version='1.0'?>"),

    // For comparison: well-formed inputs to confirm our inspector
    // works the way we think.
    ("well-formed sanity",             "<r>plain text</r>"),
    ("well-formed with entity",        "<r>tom &amp; jerry</r>"),
    ("well-formed with cdata",         "<r><![CDATA[any chars]]></r>"),

    // Stray-`<` and doc-level text cases — exploratory, not yet
    // recovered in sup-xml; we probe libxml2's behaviour here to
    // decide what the matching tree should look like.
    ("bare < in text",                 "<r>1 < 2</r>"),
    ("bare < followed by name char",   "<r>1 <foo></r>"),
    ("bare < at end of text",          "<r>aaa<</r>"),
    ("text at doc level (before root)","hello<r/>"),
    ("text at doc level (after root)", "<r/>trailing text"),
    ("text at doc level (both)",       "before<r/>after"),
];

fn main() {
    println!();
    println!("libxml2 XML_PARSE_RECOVER inspector — what tree does it build?");
    println!();
    println!("{:<32}  {:<8}  {:<8}  {:<6}  {}",
             "case", "accept", "root", "kids", "root text content");
    println!("{:─<32}  {:─<8}  {:─<8}  {:─<6}  {}",
             "", "", "", "", "─".repeat(40));

    for (label, src) in CASES {
        let v = inspect(src.as_bytes());
        let accept = if v.accepted { "OK" } else { "REJECT" };
        let root = v.root_name.as_deref().unwrap_or("-");
        let kids = if v.accepted { v.root_child_elems.to_string() } else { "-".into() };
        let text = v.root_text.as_deref().unwrap_or("-");
        let text_disp: String = text.chars().take(60).collect();
        println!("{:<32}  {:<8}  {:<8}  {:<6}  {:?}",
                 label, accept, root, kids, text_disp);
    }
}
