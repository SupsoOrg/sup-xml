//! XInclude — `<xi:include href="..."/>` processing.
//!
//! Walks a node's subtree, finds every `xi:include` element in the
//! XInclude namespace (`http://www.w3.org/2001/XInclude`), parses the
//! referenced resource, and replaces the include element with the
//! included content.
//!
//! # Supported subset
//!
//! * `xi:include href="…"` with `parse="xml"` (default) — parse the
//!   referenced file and splice in its root element.
//! * `xi:include href="…" parse="text"` — read the referenced file
//!   as UTF-8 text and substitute a single text node.
//! * `xpointer="…"` fragment selection (XInclude § 4.2) — the
//!   `xpointer(EXPR)`, `element(…)`, and bare-ID schemes, resolved by
//!   [`sup_xml_core::xinclude::resolve_xpointer`] against the included
//!   document; the selected node(s) are spliced instead of the whole
//!   root.  An unsupported or empty selector fails the call (`-1`)
//!   rather than silently inlining the whole document.
//! * Nested `xi:include` inside an included document — expanded
//!   recursively, bounded by `MAX_XINCLUDE_DEPTH`.
//! * `xi:fallback` (XInclude § 4.4) — when the primary resource can't
//!   be loaded or its xpointer doesn't resolve, the include's
//!   `<xi:fallback>` children are spliced instead; an empty fallback
//!   removes the include.  Only with no fallback does the call fail.
//!
//! # Not yet supported
//!
//! * Non-UTF-8 encodings for `parse="text"` (we read bytes as UTF-8;
//!   non-UTF-8 input produces the lossy `String::from_utf8_lossy`
//!   substitution).
//! * Network-fetched hrefs (only local filesystem paths work).
//!
//! # Base URL resolution
//!
//! libxml2 resolves a relative `href` against `node->doc->URL` when
//! set, falling back to the current working directory otherwise.
//! We do the same: read `doc->url` (offset 136) and treat it as the
//! base when present, otherwise pass the href through unchanged so
//! `std::fs::read` resolves it via CWD.

use std::ffi::CStr;
use std::os::raw::{c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;

use sup_xml_core::entity_resolver::EntityResolver;
use sup_xml_core::options::ParseOptions;
use sup_xml_core::parser::parse_bytes;
use sup_xml_tree::dom::{Node, NodeKind, XmlDoc};

/// The XInclude namespace URI.
const XINCLUDE_NS: &str = "http://www.w3.org/2001/XInclude";

/// libxml2 `xmlXIncludeProcessTree(tree)` — process every
/// `xi:include` element in the subtree rooted at `tree`.  Returns
/// the number of substitutions on success, or `-1` on error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXIncludeProcessTree(tree: *mut Node<'static>) -> c_int {
    unsafe { xmlXIncludeProcessTreeFlagsData(tree, 0, ptr::null_mut()) }
}

/// `xmlXIncludeProcessFlags(doc, flags)` — XInclude over the whole
/// doc with the given parser-options bitmask.  We ignore flags and
/// process from the doc's root element.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXIncludeProcessFlags(
    doc:    *mut sup_xml_tree::dom::XmlDoc,
    flags:  c_int,
) -> c_int {
    if doc.is_null() { return -1; }
    // SAFETY: caller asserts doc came from a parse entry.
    let root = unsafe { (*doc).children.get() };
    if root.is_null() { return 0; }
    unsafe { xmlXIncludeProcessTreeFlagsData(root, flags, ptr::null_mut()) }
}

/// libxml2 `xmlXIncludeProcessTreeFlagsData(tree, flags, data)`.
///
/// We ignore `flags` (parser-options bitmask) and `data` (opaque
/// caller context for the structured-error callback) in v0.1.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXIncludeProcessTreeFlagsData(
    tree:   *mut Node<'static>,
    flags:  c_int,
    data:   *mut c_void,
) -> c_int {
    if tree.is_null() { return -1; }
    // SAFETY: caller asserts `tree` is a valid arena pointer.
    let root_node = unsafe { &*(tree as *const Node<'static>) };
    let doc_ptr = root_node.doc.get() as *mut XmlDoc;
    if doc_ptr.is_null() { return -1; }

    // Route resource loading through the consumer's resolvers (lxml's
    // `tree.xinclude()` registers `_local_resolver` as the external
    // entity loader and passes its parser context as `data`).  The
    // loader reads its context's `_private`, so wrap `data` in a
    // ctxt-shaped buffer at that offset.  Kept alive for the whole call.
    const XML_PARSE_DTDLOAD: c_int = 1 << 2;
    let load_dtd = (flags & XML_PARSE_DTDLOAD) != 0;
    let mut _loader_ctxt: Option<Box<[u8; 752]>> = None;
    let resolver: Option<std::sync::Arc<crate::parse::CExternalEntityLoaderResolver>> =
        crate::parse::consumer_external_entity_loader().map(|loader| {
            let mut buf = Box::new([0u8; 752]);
            const CTXT_PRIVATE_OFFSET: usize = 424;
            buf[CTXT_PRIVATE_OFFSET..CTXT_PRIVATE_OFFSET + 8]
                .copy_from_slice(&(data as usize).to_ne_bytes());
            let ctxt_ptr = buf.as_ptr() as usize;
            _loader_ctxt = Some(buf);
            std::sync::Arc::new(crate::parse::CExternalEntityLoaderResolver {
                loader, ctxt: ctxt_ptr,
            })
        });

    // Base URL for relative href resolution.  doc.url is `*const
    // c_char` (offset 136); read it as a possibly-empty path.
    let base_dir: Option<PathBuf> = unsafe {
        let url_ptr = (*doc_ptr).url;
        if url_ptr.is_null() {
            None
        } else {
            CStr::from_ptr(url_ptr).to_str().ok().and_then(|s| {
                Path::new(s).parent().map(|p| p.to_path_buf())
            })
        }
    };

    // Collect every xi:include in the subtree first, then process
    // them.  Mutating the tree while iterating would invalidate the
    // sibling-pointer walk.
    let mut includes: Vec<&Node<'static>> = Vec::new();
    collect_includes(root_node, &mut includes);

    let mut count: i32 = 0;
    for inc in includes {
        match process_one(inc, doc_ptr, base_dir.as_deref(), resolver.as_ref(), load_dtd, 0) {
            Ok(()) => count += 1,
            Err(_) => return -1,
        }
    }
    count
}

/// Bound on nested XInclude processing — an included document may itself
/// contain `xi:include`s.  Mirrors libxml2's `XINCLUDE_MAX_DEPTH` and
/// guards against an include cycle resolving to itself.
const MAX_XINCLUDE_DEPTH: u32 = 40;

/// Recursive walk filling `out` with every `xi:include` element.
fn collect_includes<'a>(node: &'a Node<'a>, out: &mut Vec<&'a Node<'a>>) {
    if is_xinclude(node) {
        out.push(node);
        // libxml2 doesn't recurse into an include's body (it gets
        // replaced wholesale), so we stop here.
        return;
    }
    for child in node.children() {
        collect_includes(child, out);
    }
}

/// True if `n` is an `<xi:include>` element in the XInclude namespace.
fn is_xinclude(n: &Node<'_>) -> bool {
    if !matches!(n.kind, NodeKind::Element) { return false; }
    // Local name match.
    let local = match n.name().rfind(':') {
        Some(i) => &n.name()[i + 1..],
        None    => n.name(),
    };
    if local != "include" { return false; }
    // Namespace match via the node's `namespace` slot.
    match n.namespace.get() {
        Some(ns) => ns.href() == XINCLUDE_NS,
        None     => false,
    }
}

/// True if `n` is an `<xi:fallback>` element in the XInclude namespace.
fn is_xi_fallback(n: &Node<'_>) -> bool {
    if !matches!(n.kind, NodeKind::Element) { return false; }
    let local = match n.name().rfind(':') {
        Some(i) => &n.name()[i + 1..],
        None    => n.name(),
    };
    if local != "fallback" { return false; }
    match n.namespace.get() {
        Some(ns) => ns.href() == XINCLUDE_NS,
        None     => false,
    }
}

#[derive(Debug)]
enum XIncludeError {
    Io,
    Parse,
    Detached,
}

/// Replace `inc` with its referenced content.
///
/// When primary resolution fails (missing/unparseable resource,
/// unresolvable xpointer), XInclude § 4.4 says to splice the include's
/// `<xi:fallback>` children instead of erroring.  Only when there is no
/// fallback does the original error propagate.
fn process_one(
    inc: &Node<'static>,
    doc_ptr: *mut XmlDoc,
    base_dir: Option<&Path>,
    resolver: Option<&std::sync::Arc<crate::parse::CExternalEntityLoaderResolver>>,
    load_dtd: bool,
    depth: u32,
) -> Result<(), XIncludeError> {
    if depth > MAX_XINCLUDE_DEPTH {
        return Err(XIncludeError::Parse);
    }
    // SAFETY: doc_ptr is non-null and valid (checked by caller).
    let doc_ref = unsafe { &(*doc_ptr)._doc };

    let new_nodes: Vec<*mut Node<'static>> =
        match resolve_primary(inc, doc_ptr, doc_ref, base_dir, resolver, load_dtd, depth) {
            Ok(nodes) => nodes,
            Err(primary_err) => {
                // XInclude § 4.4: substitute the `<xi:fallback>` children.
                // Their own relative hrefs resolve against the *including*
                // document's base, not the resource that failed to load.
                match inc.children().find(|c| is_xi_fallback(c)) {
                    Some(fb) => {
                        let mut out: Vec<*mut Node<'static>> = Vec::new();
                        for child in fb.children() {
                            out.push(copy_and_expand(
                                child, doc_ref, doc_ptr, base_dir, resolver, load_dtd, depth,
                            )?);
                        }
                        out
                    }
                    None => return Err(primary_err),
                }
            }
        };

    // SAFETY: `inc` and every node in `new_nodes` live in doc_ref's arena.
    unsafe { replace_node_with_nodes(inc as *const _ as *mut Node<'static>, &new_nodes) }
}

/// Load and expand an `xi:include`'s primary resource into a run of
/// nodes living in the destination arena.  Returns `Err` (without
/// touching the tree) on any resolution failure, so the caller can try
/// `<xi:fallback>`.
fn resolve_primary(
    inc: &Node<'static>,
    doc_ptr: *mut XmlDoc,
    doc_ref: &sup_xml_tree::dom::Document,
    base_dir: Option<&Path>,
    resolver: Option<&std::sync::Arc<crate::parse::CExternalEntityLoaderResolver>>,
    load_dtd: bool,
    depth: u32,
) -> Result<Vec<*mut Node<'static>>, XIncludeError> {
    // Pull `href`, `parse`, and `xpointer` from the include's attributes.
    let mut href: Option<&str> = None;
    let mut parse_mode = "xml";
    let mut xpointer: Option<&str> = None;
    for attr in inc.attributes() {
        match attr.name() {
            "href"     => href = Some(attr.value()),
            "parse"    => parse_mode = attr.value(),
            "xpointer" => xpointer = Some(attr.value()),
            _ => {}
        }
    }
    let Some(href_str) = href else { return Err(XIncludeError::Io); };

    // A `file://` href (lxml builds one via `path2url`) maps to a local
    // path — strip the scheme and percent-decode, like libxml2's file
    // I/O.  Other hrefs are used as-is.
    let href_local = crate::outbuf::local_path_from_file_uri(href_str);
    let href_local = href_local.as_ref();

    // Resolve href against base_dir.
    let path: PathBuf = if Path::new(href_local).is_absolute() {
        PathBuf::from(href_local)
    } else if let Some(base) = base_dir {
        base.join(href_local)
    } else {
        PathBuf::from(href_local)
    };

    // Load the referenced resource.  Prefer the consumer's resolvers
    // (so a custom `etree.Resolver` is consulted for the href), falling
    // back to reading the resolved path from disk.
    let bytes = match resolver
        .and_then(|r| r.resolve(None, &path.to_string_lossy(), None).ok())
    {
        Some(b) => b,
        None => std::fs::read(&path).map_err(|_| XIncludeError::Io)?,
    };

    if parse_mode == "text" {
        // Treat bytes as UTF-8; non-UTF-8 → lossy.  `xpointer` is not
        // meaningful with `parse="text"` (XInclude § 3.1) and is ignored.
        let text = String::from_utf8_lossy(&bytes);
        let text_node = doc_ref.new_text(doc_ref.bump().alloc_str(&text));
        text_node.doc.set(doc_ptr as *mut std::os::raw::c_void);
        return Ok(vec![text_node as *const Node<'_> as *mut Node<'static>]);
    }

    // parse="xml" (default).  Parse the referenced doc, select the target
    // subtree(s) — the whole root, or the nodes an `xpointer` fragment
    // selector picks out — and deep-copy them into the parent's arena.
    // The included document's own external DTD (and any entities) resolve
    // through the same consumer resolver.
    let opts = ParseOptions {
        namespace_aware: true,
        load_external_dtd: load_dtd,
        base_url: Some(path.to_string_lossy().into_owned()),
        external_resolver: resolver
            .map(|r| r.clone() as std::sync::Arc<dyn sup_xml_core::entity_resolver::EntityResolver>),
        ..ParseOptions::default()
    };
    let included = parse_bytes(&bytes, &opts).map_err(|_| XIncludeError::Parse)?;

    // XPointer fragment selection (XInclude § 4.2): when present, splice
    // only the selected node(s); otherwise splice the whole root.  An
    // unsupported/empty xpointer is a hard error — we must never silently
    // inline the whole document.  Each target only needs to outlive the
    // `deep_copy_into` inside `copy_and_expand`, which takes the copy out
    // into the parent arena as a raw pointer.
    let targets: Vec<&Node<'_>> = match xpointer {
        Some(xp) => sup_xml_core::xinclude::resolve_xpointer(&included, xp, href_str)
            .map_err(|_| XIncludeError::Parse)?,
        None => vec![included.root()],
    };

    // Nested xi:includes inside the copied subtree resolve their relative
    // hrefs against the included document's own location.
    let nested_base = path.parent().map(Path::to_path_buf);
    let mut copied: Vec<*mut Node<'static>> = Vec::with_capacity(targets.len());
    for tgt in targets {
        copied.push(copy_and_expand(
            tgt, doc_ref, doc_ptr, nested_base.as_deref(), resolver, load_dtd, depth,
        )?);
    }
    Ok(copied)
}

/// Deep-copy `tgt`'s subtree into the destination arena, expand any
/// nested `xi:include`s in the copy (resolving their relative hrefs
/// against `nested_base`), and return the copied node.
fn copy_and_expand(
    tgt: &Node<'_>,
    doc_ref: &sup_xml_tree::dom::Document,
    doc_ptr: *mut XmlDoc,
    nested_base: Option<&Path>,
    resolver: Option<&std::sync::Arc<crate::parse::CExternalEntityLoaderResolver>>,
    load_dtd: bool,
    depth: u32,
) -> Result<*mut Node<'static>, XIncludeError> {
    let copied_ptr = crate::mutate::deep_copy_into(doc_ref, tgt, true, true, doc_ptr);
    if copied_ptr.is_null() {
        return Err(XIncludeError::Parse);
    }
    // SAFETY: deep_copy_into returned a live arena node on success.
    let copied_node = unsafe { &*(copied_ptr as *const Node<'static>) };
    let mut nested: Vec<&Node<'static>> = Vec::new();
    collect_includes(copied_node, &mut nested);
    for inc2 in nested {
        process_one(inc2, doc_ptr, nested_base, resolver, load_dtd, depth + 1)?;
    }
    Ok(copied_ptr)
}

/// Replace `old` with the ordered run `new_nodes` in `old`'s sibling
/// list + parent, via raw pointers to sidestep the `'static`-vs-arena
/// lifetime variance.  A single-node `xi:include` (the common case)
/// passes a one-element slice; an `xpointer` selecting a nodeset passes
/// several, spliced in document order where `old` sat.  An empty run
/// (an empty `<xi:fallback/>`) simply unlinks `old`.
///
/// # Safety
/// `old` and every pointer in `new_nodes` must reference live
/// arena-resident Nodes in the *same* document.  `old` must have a
/// non-NULL parent.
unsafe fn replace_node_with_nodes(
    old: *mut Node<'static>,
    new_nodes: &[*mut Node<'static>],
) -> Result<(), XIncludeError> {
    unsafe {
        let old_ref = &*old;
        let parent = old_ref.parent.get().ok_or(XIncludeError::Detached)?;
        let prev   = old_ref.prev_sibling.get();
        let next   = old_ref.next_sibling.get();

        let refs: Vec<&Node<'static>> = new_nodes.iter().map(|&p| &*p).collect();

        // Chain the new nodes to each other and to `old`'s neighbours.
        for (i, &n) in refs.iter().enumerate() {
            n.parent.set(Some(parent));
            n.prev_sibling.set(if i == 0 { prev } else { Some(refs[i - 1]) });
            n.next_sibling.set(if i + 1 == refs.len() { next } else { Some(refs[i + 1]) });
        }

        // The run's endpoints take `old`'s place; with an empty run the
        // neighbours link directly to each other (`old` is removed).
        let first = refs.first().copied().or(next);
        let last  = refs.last().copied().or(prev);

        match prev {
            Some(p) => p.next_sibling.set(first),
            None    => parent.first_child.set(first),
        }
        match next {
            Some(n) => n.prev_sibling.set(last),
            None    => parent.last_child.set(last),
        }

        old_ref.parent.set(None);
        old_ref.prev_sibling.set(None);
        old_ref.next_sibling.set(None);
    }
    Ok(())
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::raw::c_char;

    #[test]
    fn null_safety() {
        assert_eq!(unsafe { xmlXIncludeProcessTree(ptr::null_mut()) }, -1);
    }

    #[test]
    fn parse_text_substitution() {
        // Write a UTF-8 fragment to a temp file and reference it
        // from a small doc via `parse="text"`.
        let mut tmp = std::env::temp_dir();
        tmp.push("sup_xml_xinclude_text.txt");
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(b"included contents").unwrap();
        drop(f);

        let src = format!(
            r#"<r xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="{}" parse="text"/></r>"#,
            tmp.display()
        );
        let doc_ptr = unsafe {
            crate::parse::xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(), ptr::null(), 0,
            )
        };
        assert!(!doc_ptr.is_null(), "parse failed");
        let root = unsafe { (*doc_ptr).children.get() };
        let n = unsafe { xmlXIncludeProcessTree(root) };
        assert_eq!(n, 1, "expected one substitution");
        // Don't compare full serialization in unit; just check the
        // first child is now a Text node with the included contents.
        unsafe {
            let root_node = &*(root as *const Node<'static>);
            let first = root_node.first_child.get().expect("no children after xinclude");
            assert!(matches!(first.kind, NodeKind::Text), "expected text node, got {:?}", first.kind);
            assert_eq!(first.content(), "included contents");
            crate::parse::xmlFreeDoc(doc_ptr);
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn parse_xml_substitution() {
        let mut tmp = std::env::temp_dir();
        tmp.push("sup_xml_xinclude_xml.xml");
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(b"<frag><inside/></frag>").unwrap();
        drop(f);

        let src = format!(
            r#"<r xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="{}"/></r>"#,
            tmp.display()
        );
        let doc_ptr = unsafe {
            crate::parse::xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(), ptr::null(), 0,
            )
        };
        assert!(!doc_ptr.is_null());
        let root = unsafe { (*doc_ptr).children.get() };
        let n = unsafe { xmlXIncludeProcessTree(root) };
        assert_eq!(n, 1);
        unsafe {
            let root_node = &*(root as *const Node<'static>);
            let first = root_node.first_child.get().expect("no children after xinclude");
            assert!(matches!(first.kind, NodeKind::Element));
            assert_eq!(first.name(), "frag");
            crate::parse::xmlFreeDoc(doc_ptr);
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn xpointer_selects_subtree_not_whole_root() {
        // The included doc has two children; the xpointer selects only
        // the second, so the spliced node must be `<b>`, not `<frag>`.
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("sup_xml_xinclude_xptr_{}.xml", std::process::id()));
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(b"<frag><a>A</a><b>B</b></frag>").unwrap();
        drop(f);

        let src = format!(
            r#"<r xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="{}" xpointer="xpointer(//b)"/></r>"#,
            tmp.display()
        );
        let doc_ptr = unsafe {
            crate::parse::xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(), ptr::null(), 0,
            )
        };
        assert!(!doc_ptr.is_null());
        let root = unsafe { (*doc_ptr).children.get() };
        let n = unsafe { xmlXIncludeProcessTree(root) };
        assert_eq!(n, 1);
        unsafe {
            let root_node = &*(root as *const Node<'static>);
            let first = root_node.first_child.get().expect("no children after xinclude");
            assert!(matches!(first.kind, NodeKind::Element));
            assert_eq!(first.name(), "b", "xpointer should splice <b>, not the whole <frag>");
            // Its text child must have been deep-copied too.
            let text = first.first_child.get().expect("<b>'s text child was not copied");
            assert!(matches!(text.kind, NodeKind::Text));
            assert_eq!(text.content(), "B");
            crate::parse::xmlFreeDoc(doc_ptr);
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn unresolvable_xpointer_fails_loudly() {
        // An xpointer matching nothing must return -1 rather than
        // silently inlining the whole referenced document.
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("sup_xml_xinclude_badxptr_{}.xml", std::process::id()));
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(b"<frag><a>A</a></frag>").unwrap();
        drop(f);

        let src = format!(
            r#"<r xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="{}" xpointer="xpointer(//does-not-exist)"/></r>"#,
            tmp.display()
        );
        let doc_ptr = unsafe {
            crate::parse::xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(), ptr::null(), 0,
            )
        };
        assert!(!doc_ptr.is_null());
        let root = unsafe { (*doc_ptr).children.get() };
        let n = unsafe { xmlXIncludeProcessTree(root) };
        assert_eq!(n, -1, "an empty xpointer result must be an error, not a whole-root inline");
        unsafe {
            // The include element must be untouched (not replaced by <frag>).
            let root_node = &*(root as *const Node<'static>);
            let first = root_node.first_child.get().expect("include element vanished");
            assert!(is_xinclude(first), "include must remain in place after a failed xpointer");
            crate::parse::xmlFreeDoc(doc_ptr);
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn fallback_used_when_resource_missing() {
        // href points at a nonexistent file; the <xi:fallback> content
        // must be spliced instead of failing the whole process.
        let missing = std::env::temp_dir()
            .join(format!("sup_xml_xinclude_missing_{}.xml", std::process::id()));
        let src = format!(
            r#"<r xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="{}"><xi:fallback><b>fallback!</b></xi:fallback></xi:include></r>"#,
            missing.display()
        );
        let doc_ptr = unsafe {
            crate::parse::xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(), ptr::null(), 0,
            )
        };
        assert!(!doc_ptr.is_null());
        let root = unsafe { (*doc_ptr).children.get() };
        let n = unsafe { xmlXIncludeProcessTree(root) };
        assert_eq!(n, 1, "fallback substitution should count as one success");
        unsafe {
            let root_node = &*(root as *const Node<'static>);
            let first = root_node.first_child.get().expect("no children after fallback");
            assert!(matches!(first.kind, NodeKind::Element));
            assert_eq!(first.name(), "b", "fallback <b> should be spliced");
            let text = first.first_child.get().expect("<b> text not copied");
            assert_eq!(text.content(), "fallback!");
            crate::parse::xmlFreeDoc(doc_ptr);
        }
    }

    #[test]
    fn empty_fallback_removes_include() {
        // A missing resource with an empty <xi:fallback/> drops the
        // include entirely, leaving the following sibling in place.
        let missing = std::env::temp_dir()
            .join(format!("sup_xml_xinclude_missing2_{}.xml", std::process::id()));
        let src = format!(
            r#"<r xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="{}"><xi:fallback/></xi:include><keep/></r>"#,
            missing.display()
        );
        let doc_ptr = unsafe {
            crate::parse::xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(), ptr::null(), 0,
            )
        };
        assert!(!doc_ptr.is_null());
        let root = unsafe { (*doc_ptr).children.get() };
        let n = unsafe { xmlXIncludeProcessTree(root) };
        assert_eq!(n, 1);
        unsafe {
            let root_node = &*(root as *const Node<'static>);
            let first = root_node.first_child.get().expect("all children vanished");
            assert!(matches!(first.kind, NodeKind::Element));
            assert_eq!(first.name(), "keep", "empty fallback should leave only <keep>");
            assert!(first.next_sibling.get().is_none(), "include should be gone, not just emptied");
            crate::parse::xmlFreeDoc(doc_ptr);
        }
    }

    #[test]
    fn missing_resource_without_fallback_errors() {
        let missing = std::env::temp_dir()
            .join(format!("sup_xml_xinclude_missing3_{}.xml", std::process::id()));
        let src = format!(
            r#"<r xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="{}"/></r>"#,
            missing.display()
        );
        let doc_ptr = unsafe {
            crate::parse::xmlReadMemory(
                src.as_ptr() as *const c_char,
                src.len() as c_int,
                ptr::null(), ptr::null(), 0,
            )
        };
        assert!(!doc_ptr.is_null());
        let root = unsafe { (*doc_ptr).children.get() };
        let n = unsafe { xmlXIncludeProcessTree(root) };
        assert_eq!(n, -1, "missing resource with no fallback must error");
        unsafe { crate::parse::xmlFreeDoc(doc_ptr); }
    }
}
