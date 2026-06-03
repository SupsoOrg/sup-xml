//! Tier 1 tree-walking helpers — element-aware iteration over a doc.
//!
//! libxml2 trees mix element and non-element (text, comment, PI) nodes
//! as siblings.  These helpers skip the non-element nodes, which is
//! what consumers nearly always want when traversing structured data.
//!
//! All five functions are NULL-safe and run in O(siblings-skipped).
//! No allocation; pointers returned point straight into the document's
//! arena (released by [`xmlFreeDoc`](crate::parse::xmlFreeDoc)).

use std::os::raw::c_ulong;
use std::ptr;

use sup_xml_tree::dom::{Node, NodeKind};

// ── helpers ────────────────────────────────────────────────────────────────

#[inline]
fn node_ptr<'a>(n: &'a Node<'a>) -> *mut Node<'static> {
    n as *const Node<'a> as *mut Node<'static>
}

// ── element-aware iteration ────────────────────────────────────────────────

/// libxml2 `xmlFirstElementChild`.  Returns the first child of `parent`
/// whose kind is `XML_ELEMENT_NODE`, skipping any leading text/comment/PI
/// siblings.  NULL if `parent` is NULL or has no element child.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlFirstElementChild(parent: *mut Node<'static>) -> *mut Node<'static> {
    if parent.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts parent is a valid pointer into a live doc.
    let p = unsafe { &*parent };
    let mut cur = p.first_child.get();
    while let Some(n) = cur {
        if matches!(n.kind, NodeKind::Element) {
            return node_ptr(n);
        }
        cur = n.next_sibling.get();
    }
    ptr::null_mut()
}

/// libxml2 `xmlLastElementChild`.  Mirror of `xmlFirstElementChild` —
/// walks backwards from `parent->last`, skipping non-elements.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlLastElementChild(parent: *mut Node<'static>) -> *mut Node<'static> {
    if parent.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: see xmlFirstElementChild.
    let p = unsafe { &*parent };
    let mut cur = p.last_child.get();
    while let Some(n) = cur {
        if matches!(n.kind, NodeKind::Element) {
            return node_ptr(n);
        }
        cur = n.prev_sibling.get();
    }
    ptr::null_mut()
}

/// libxml2 `xmlNextElementSibling`.  Returns the next element sibling
/// of `node`, skipping non-element siblings.  NULL when no further
/// element exists in the sibling chain.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlNextElementSibling(node: *mut Node<'static>) -> *mut Node<'static> {
    if node.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: caller asserts node is a valid pointer into a live doc.
    let n = unsafe { &*node };
    let mut cur = n.next_sibling.get();
    while let Some(s) = cur {
        if matches!(s.kind, NodeKind::Element) {
            return node_ptr(s);
        }
        cur = s.next_sibling.get();
    }
    ptr::null_mut()
}

/// libxml2 `xmlPreviousElementSibling`.  Mirror of
/// `xmlNextElementSibling` walking backwards through prev pointers.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlPreviousElementSibling(node: *mut Node<'static>) -> *mut Node<'static> {
    if node.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: see xmlNextElementSibling.
    let n = unsafe { &*node };
    let mut cur = n.prev_sibling.get();
    while let Some(s) = cur {
        if matches!(s.kind, NodeKind::Element) {
            return node_ptr(s);
        }
        cur = s.prev_sibling.get();
    }
    ptr::null_mut()
}

/// libxml2 `xmlChildElementCount`.  Counts element children of
/// `parent` (skipping text/comment/PI).  Returns 0 on NULL.
///
/// O(children) — single pass through the sibling list.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlChildElementCount(parent: *mut Node<'static>) -> c_ulong {
    if parent.is_null() {
        return 0;
    }
    // SAFETY: see xmlFirstElementChild.
    let p = unsafe { &*parent };
    let mut count: c_ulong = 0;
    let mut cur = p.first_child.get();
    while let Some(n) = cur {
        if matches!(n.kind, NodeKind::Element) {
            count += 1;
        }
        cur = n.next_sibling.get();
    }
    count
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_char;
    use std::os::raw::c_int;

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

    /// Document: `<r><!-- c --><a/>text<b/><c/>tail</r>`
    /// Root `r` has three element children (`a`, `b`, `c`) and three
    /// non-element siblings (comment + two text nodes).  Element-aware
    /// walks should see exactly the three elements in document order.
    #[test]
    fn element_walk_skips_non_elements() {
        let doc = parse(b"<r><!-- c --><a/>text<b/><c/>tail</r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        assert!(!root.is_null());

        // First / Last
        let first = unsafe { xmlFirstElementChild(root) };
        let last  = unsafe { xmlLastElementChild(root) };
        assert!(!first.is_null());
        assert!(!last.is_null());
        assert_eq!(unsafe { &*first }.name(), "a");
        assert_eq!(unsafe { &*last  }.name(), "c");

        // Forward chain: a → b → c → NULL
        let b = unsafe { xmlNextElementSibling(first) };
        assert!(!b.is_null());
        assert_eq!(unsafe { &*b }.name(), "b");
        let c = unsafe { xmlNextElementSibling(b) };
        assert!(!c.is_null());
        assert_eq!(unsafe { &*c }.name(), "c");
        let end = unsafe { xmlNextElementSibling(c) };
        assert!(end.is_null());

        // Backward chain: c → b → a → NULL
        let b2 = unsafe { xmlPreviousElementSibling(c) };
        assert!(!b2.is_null());
        assert_eq!(unsafe { &*b2 }.name(), "b");
        let a2 = unsafe { xmlPreviousElementSibling(b2) };
        assert!(!a2.is_null());
        assert_eq!(unsafe { &*a2 }.name(), "a");
        let start = unsafe { xmlPreviousElementSibling(a2) };
        assert!(start.is_null());

        // Count
        let n = unsafe { xmlChildElementCount(root) };
        assert_eq!(n, 3);

        unsafe { xmlFreeDoc(doc); }
    }

    /// A parent with only text/comment children has 0 element children.
    #[test]
    fn count_of_non_element_parent() {
        let doc = parse(b"<r>just text<!-- and comment --></r>");
        let root = unsafe { xmlDocGetRootElement(doc) };
        assert_eq!(unsafe { xmlChildElementCount(root) }, 0);
        assert!(unsafe { xmlFirstElementChild(root) }.is_null());
        assert!(unsafe { xmlLastElementChild(root)  }.is_null());
        unsafe { xmlFreeDoc(doc); }
    }

    /// All five entry points are NULL-safe.
    #[test]
    fn null_safety() {
        let null = ptr::null_mut();
        assert!(unsafe { xmlFirstElementChild(null)    }.is_null());
        assert!(unsafe { xmlLastElementChild(null)     }.is_null());
        assert!(unsafe { xmlNextElementSibling(null)   }.is_null());
        assert!(unsafe { xmlPreviousElementSibling(null) }.is_null());
        assert_eq!(unsafe { xmlChildElementCount(null) }, 0);
    }

    /// Deeply nested doc walked via direct field reads (T-WALK-05 in
    /// Rust form).  No recursion in our C-callable functions — they
    /// don't traverse depth, only sibling chains — but exercising a
    /// deep tree confirms the field-read path doesn't blow the stack.
    ///
    /// Depth is 200 (under the parser's default 256 cap).  Bumping
    /// past that needs XML_PARSE_HUGE support in xmlReadMemory —
    /// follow-up work.
    #[test]
    fn deeply_nested_walk() {
        let mut s = String::with_capacity(2_000);
        let depth = 200;
        for _ in 0..depth { s.push_str("<n>"); }
        for _ in 0..depth { s.push_str("</n>"); }
        let doc = parse(s.as_bytes());
        let root = unsafe { xmlDocGetRootElement(doc) };
        assert!(!root.is_null());

        // Walk down using iterative field reads — no recursion.
        let mut cur = unsafe { xmlFirstElementChild(root) };
        let mut levels_walked = 1;  // root counts
        while !cur.is_null() {
            levels_walked += 1;
            cur = unsafe { xmlFirstElementChild(cur) };
        }
        assert_eq!(levels_walked, depth);

        unsafe { xmlFreeDoc(doc); }
    }
}
