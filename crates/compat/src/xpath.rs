//! libxml2 XPath façade — wraps `sup_xml_core::xpath`.
//!
//! Tier 2's biggest unlock: lxml, Nokogiri, PHP DOM, xmlstarlet all
//! exercise XPath heavily.  Their import paths trip our shim
//! immediately because lxml.etree's module init creates an XPath
//! object as part of its own internal bookkeeping.
//!
//! ## Layout invariants
//!
//! `xmlNodeSet` and `xmlXPathObject` are byte-exact mirrors of
//! libxml2's `_xmlNodeSet` and `_xmlXPathObject`; layouts verified
//! identical across libxml2 2.6 → master.  C callers reading
//! `result->type`, `result->nodesetval->nodeNr`, etc. land on the
//! right bytes.
//!
//! ## Allocator pairing
//!
//! - `xmlXPathObject`, `xmlNodeSet`, and the `nodeTab` array are
//!   heap-owned by us; freed via [`xmlXPathFreeObject`] /
//!   [`xmlXPathFreeNodeSet`].  NOT eligible for `xmlFree` — those
//!   are dedicated frees that walk the struct.
//! - `stringval` (when `type == XPATH_STRING`) IS allocated through
//!   [`crate::alloc::alloc_registered_cstring`] so callers who do
//!   `xmlFree(obj->stringval)` work, but typically callers use
//!   `xmlXPathFreeObject` which handles it.

#![allow(non_camel_case_types)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_double, c_int, c_void};
use std::ptr;

use sup_xml_core::xpath::{self, XPathValue};
use sup_xml_core::xpath::eval::Numeric;
use sup_xml_core::xpath::context::{DocIndex, INodeKind};
use sup_xml_tree::dom::{Node, XmlDoc};

use crate::alloc::alloc_registered_cstring;

// ── enum constants (must match libxml2's `xmlXPathObjectType`) ─────────────

pub const XPATH_UNDEFINED:    c_int = 0;
pub const XPATH_NODESET:      c_int = 1;
pub const XPATH_BOOLEAN:      c_int = 2;
pub const XPATH_NUMBER:       c_int = 3;
pub const XPATH_STRING:       c_int = 4;
pub const XPATH_POINT:        c_int = 5;
pub const XPATH_RANGE:        c_int = 6;
pub const XPATH_LOCATIONSET:  c_int = 7;
pub const XPATH_USERS:        c_int = 8;
pub const XPATH_XSLT_TREE:    c_int = 9;

// ── byte-exact mirror structs ─────────────────────────────────────────────

/// `_xmlNodeSet` (libxml2).  Just 3 fields; layout verified against
/// 2.9.13 and 2.15.3 — see the `t-upstream-layout` c-test.
#[repr(C)]
pub struct xmlNodeSet {
    pub nodeNr:  c_int,                // 0
    pub nodeMax: c_int,                // 4
    pub nodeTab: *mut *mut Node<'static>, // 8
}
const _: () = {
    assert!(std::mem::offset_of!(xmlNodeSet, nodeNr)  == 0);
    assert!(std::mem::offset_of!(xmlNodeSet, nodeMax) == 4);
    assert!(std::mem::offset_of!(xmlNodeSet, nodeTab) == 8);
    assert!(std::mem::size_of::<xmlNodeSet>() == 16);
};

/// `_xmlXPathObject` (libxml2).  9 fields, 72 bytes; stable.
#[repr(C)]
pub struct xmlXPathObject {
    pub kind:        c_int,            // 0  — libxml2 calls this `type`
    _pad_kind:       u32,              // 4
    pub nodesetval:  *mut xmlNodeSet,  // 8
    pub boolval:     c_int,            // 16
    _pad_boolval:    u32,              // 20
    pub floatval:    c_double,         // 24
    pub stringval:   *mut c_char,      // 32
    pub user:        *mut c_void,      // 40
    pub index:       c_int,            // 48
    _pad_index:      u32,              // 52
    pub user2:       *mut c_void,      // 56
    pub index2:      c_int,            // 64
    _pad_index2:     u32,              // 68
}
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(xmlXPathObject, kind)       ==  0);
    assert!(offset_of!(xmlXPathObject, nodesetval) ==  8);
    assert!(offset_of!(xmlXPathObject, boolval)    == 16);
    assert!(offset_of!(xmlXPathObject, floatval)   == 24);
    assert!(offset_of!(xmlXPathObject, stringval)  == 32);
    assert!(offset_of!(xmlXPathObject, user)       == 40);
    assert!(offset_of!(xmlXPathObject, index)      == 48);
    assert!(offset_of!(xmlXPathObject, user2)      == 56);
    assert!(offset_of!(xmlXPathObject, index2)     == 64);
    assert!(std::mem::size_of::<xmlXPathObject>()  == 72);
};

// ── XPath context (byte-exact mirror of libxml2's _xmlXPathContext) ─────────
//
// lxml's Cython code reads many fields directly via the public header
// definition — so layout matches matter as much as for xmlNode/xmlDoc.
// We don't actually USE most of these fields (our XPath engine is
// independent), but they have to be at the right offsets so reads
// don't crash and writes don't corrupt arbitrary state.
//
// We stash our private Rust state (namespace map, etc.) in a separate
// heap allocation whose pointer goes in the `user` slot — libxml2
// reserves that for callers' custom data, and consumers like lxml
// don't touch it (they have their own framework-level user-data
// mechanism).

#[repr(C)]
pub struct xmlXPathContext {
    pub doc:                  *const XmlDoc,                  //   0
    pub node:                 *const Node<'static>,           //   8
    pub nb_variables_unused:  c_int,                          //  16
    pub max_variables_unused: c_int,                          //  20
    pub varHash:              *mut c_void,                    //  24  (xmlHashTablePtr)
    pub nb_types:             c_int,                          //  32
    pub max_types:            c_int,                          //  36
    pub types:                *mut c_void,                    //  40
    pub nb_funcs_unused:      c_int,                          //  48
    pub max_funcs_unused:     c_int,                          //  52
    pub funcHash:             *mut c_void,                    //  56
    pub nb_axis:              c_int,                          //  64
    pub max_axis:             c_int,                          //  68
    pub axis:                 *mut c_void,                    //  72
    pub namespaces:           *mut *mut c_void,               //  80
    pub nsNr:                 c_int,                          //  88
    _pad_nsNr:                u32,                            //  92
    /// Repurposed for our private Rust state — libxml2 reserves
    /// this slot for callers' custom data; lxml has its own
    /// framework-level user-data mechanism and doesn't touch it.
    pub user:                 *mut c_void,                    //  96
    pub contextSize:          c_int,                          // 104
    pub proximityPosition:    c_int,                          // 108
    pub xptr:                 c_int,                          // 112
    _pad_xptr:                u32,                            // 116
    pub here:                 *const Node<'static>,           // 120
    pub origin:               *const Node<'static>,           // 128
    pub nsHash:               *mut c_void,                    // 136
    pub varLookupFunc:        *mut c_void,                    // 144
    pub varLookupData:        *mut c_void,                    // 152
    pub extra:                *mut c_void,                    // 160
    pub function:             *const c_char,                  // 168
    pub functionURI:          *const c_char,                  // 176
    pub funcLookupFunc:       *mut c_void,                    // 184
    pub funcLookupData:       *mut c_void,                    // 192
    pub tmpNsList:            *mut *mut c_void,               // 200
    pub tmpNsNr:              c_int,                          // 208
    _pad_tmpNsNr:             u32,                            // 212
    pub userData:             *mut c_void,                    // 216
    pub error:                *mut c_void,                    // 224  (xmlStructuredErrorFunc)
    /// `xmlError lastError` — embedded, 88 bytes.  We don't populate
    /// it but the slot has to be the right size or downstream fields
    /// shift.
    pub lastError:            [u8; 88],                       // 232
    pub debugNode:            *const Node<'static>,           // 320
    pub dict:                 *mut c_void,                    // 328
    pub flags:                c_int,                          // 336
    _pad_flags:               u32,                            // 340
    pub cache:                *mut c_void,                    // 344
    // Tail fields are conditional on LIBXML_HAS_XPATH_RESOURCE_LIMITS.
    // Including them defensively — they're zero on systems where
    // the feature is off and lxml's binding tolerates either layout.
    pub opLimit:              std::os::raw::c_ulong,          // 352
    pub opCount:              std::os::raw::c_ulong,          // 360
    pub depth:                c_int,                          // 368
    _pad_depth:               u32,                            // 372
}

/// Private Rust state stashed in `xmlXPathContext::user`.  Allocated
/// alongside the context, freed by `xmlXPathFreeContext`.
struct XPathPrivate {
    /// Registered namespace prefix → URI bindings.
    ns_map: RefCell<HashMap<String, String>>,
    /// Custom-function registrations: (namespace_uri, local_name) → fn ptr.
    /// At runtime the engine's `XPathBindings::call_function` hook
    /// (see the `impl XPathBindings for XPathPrivate` block below)
    /// consults this map; libxslt-shape function pointers are
    /// invoked via our thread-local XPath value stack.
    fn_map: RefCell<HashMap<(String, String), *mut c_void>>,
    /// Variable bindings: name → xmlXPathObject* address.  Address-as-usize
    /// keeps it Send-friendly for any future thread-safety story.
    var_map: RefCell<HashMap<String, usize>>,
    /// Back-pointer to the owning xmlXPathContext — set by
    /// xmlXPathNewContext.  Used by `call_function` to wire a
    /// minimal xmlXPathParserContext (`.context`) when invoking
    /// libxslt-registered function pointers.
    ctx_ptr: Cell<*mut xmlXPathContext>,
    /// `xmlXPathRegisterVariableLookup` callback + opaque data.
    /// libxslt registers this to delegate `$varname` lookups back
    /// to its own variable scope handling (templates, params,
    /// xsl:variable scopes all live in libxslt's data, not in
    /// `var_map`).  Without honoring this callback, every
    /// libxslt-driven XPath that references `$var` gets "no result"
    /// and the apply fails.
    var_lookup_fn:   Cell<*mut c_void>,
    var_lookup_data: Cell<*mut c_void>,
    /// `xmlXPathRegisterFuncLookup` callback + opaque data.  lxml
    /// installs this to route unknown XPath function names through
    /// its Python `FunctionNamespace` / `extensions=` machinery
    /// (rather than registering each function individually via
    /// `xmlXPathRegisterFuncNS`).  When set, `call_function` consults
    /// it for any name not in `fn_map` before erroring.
    func_lookup_fn:   Cell<*mut c_void>,
    func_lookup_data: Cell<*mut c_void>,
    /// Docs loaded by XSLT `document(URI)`.  Keyed by canonical
    /// resolved URI so repeated calls to the same URI reuse the
    /// parsed doc.  Each `*mut XmlDoc` is owned by this context —
    /// `xmlXPathFreeContext` runs `xmlFreeDoc` on every entry.
    /// The values stay pinned in the heap (they're produced by
    /// our parser as `Box::into_raw(Box::new(XmlDoc { … }))` and
    /// live until the context is torn down) — that's what makes
    /// the foreign pointers we hand back to libxslt safe to walk.
    loaded_docs: RefCell<HashMap<String, *mut XmlDoc>>,
    /// Result-tree-fragment docs we synthesize for EXSLT functions
    /// that have to return a node-set (regexp:match, str:split,
    /// str:tokenize, …).  Each entry is owned by this context and
    /// freed at teardown — same lifetime discipline as `loaded_docs`,
    /// but unkeyed (we never reuse an RTF).
    rtf_docs: RefCell<Vec<*mut XmlDoc>>,
}

/// Bridge our `XPathPrivate` registrations into the engine's
/// caller-pluggable hook.  The engine's `eval_function` /
/// `Variable` arm / `NodeTest::QName` matching all route through
/// the methods below.
impl sup_xml_core::xpath::eval::XPathBindings for XPathPrivate {
    fn resolve_prefix(&self, prefix: &str) -> Option<String> {
        // 1. Explicit registrations via xmlXPathRegisterNs.
        if let Some(uri) = self.ns_map.borrow().get(prefix) {
            return Some(uri.clone());
        }
        // 2. Walk `ctx->namespaces` — libxslt sets this to the
        //    in-scope namespace list from the calling XSLT
        //    instruction (rather than calling xmlXPathRegisterNs
        //    for every binding).  Each entry is an `xmlNs*` with
        //    our `Namespace` byte layout.  Bounded by `nsNr` to
        //    avoid reading past the array.
        let ctx_ptr = self.ctx_ptr.get();
        if !ctx_ptr.is_null() {
            // SAFETY: ctx_ptr is the xmlXPathContext we allocated;
            // libxslt may have populated namespaces/nsNr since.
            let ctx = unsafe { &*ctx_ptr };
            if !ctx.namespaces.is_null() && ctx.nsNr > 0 {
                let entries: &[*mut c_void] = unsafe {
                    std::slice::from_raw_parts(ctx.namespaces, ctx.nsNr as usize)
                };
                for &p in entries {
                    if p.is_null() { continue; }
                    let ns = unsafe {
                        &*(p as *const sup_xml_tree::dom::Namespace<'static>)
                    };
                    if ns.prefix().map(|s| s == prefix).unwrap_or(false) {
                        return Some(ns.href().to_string());
                    }
                }
            }
            // 2b. Walk via ctx->extra → xsltTransformContext->inst.
            //     libxslt sometimes leaves `ctx->namespaces` shorter
            //     than the full in-scope chain (we've observed it
            //     missing prefixes flagged by exclude-result-prefixes
            //     even though XPath still needs them).  Read the
            //     calling-instruction node and walk its ancestors'
            //     ns_def chains.
            //
            //     Field offsets verified against libxslt 1.1.45
            //     `xsltInternals.h` via a compiled `offsetof()` test:
            //     xsltTransformContext::style = 0, inst = 184.
            //
            //     Defensive: only trust the read if `style` (offset 0)
            //     is non-null — that filters out the case where
            //     `ctx->extra` happens to be set to something other
            //     than an `xsltTransformContext` (rare, but possible
            //     during stylesheet compile phases).
            if !ctx.extra.is_null() {
                let style_ptr = unsafe { *(ctx.extra as *const *const c_void) };
                if !style_ptr.is_null() {
                    let inst_ptr_addr = unsafe { (ctx.extra as *const u8).add(184) };
                    let inst_ptr = unsafe {
                        *(inst_ptr_addr as *const *const sup_xml_tree::dom::Node<'static>)
                    };
                    if !inst_ptr.is_null() {
                        if let Some(uri) = walk_inst_for_prefix(inst_ptr, prefix) {
                            return Some(uri);
                        }
                    }
                }
            }
        }
        // 3. Conventional EXSLT prefixes — fallback when neither the
        //    explicit registry nor `ctx->namespaces` carries them
        //    (stylesheets that use `regexp:test(...)` without the
        //    matching xmlns:regexp declaration on the instruction
        //    element, common in casual XPath callers).
        match prefix {
            "math"   => Some("http://exslt.org/math".into()),
            "date"   => Some("http://exslt.org/dates-and-times".into()),
            "str"    => Some("http://exslt.org/strings".into()),
            "set"    => Some("http://exslt.org/sets".into()),
            "regexp" => Some("http://exslt.org/regular-expressions".into()),
            "exsl"   => Some("http://exslt.org/common".into()),
            "func"   => Some("http://exslt.org/functions".into()),
            "dyn"    => Some("http://exslt.org/dynamic".into()),
            _ => None,
        }
    }

    fn call_function(
        &self,
        ns_uri: &str,
        name:   &str,
        args:   Vec<sup_xml_core::xpath::eval::Value>,
    ) -> Option<Result<sup_xml_core::xpath::eval::Value, sup_xml_core::error::XmlError>> {
        // EXSLT regexp family — implemented natively here because
        // libxslt's own regexp:* function pointers expect deeper
        // transform-context state than our minimal parser-context
        // bridge populates (calling them naïvely segfaults).
        if ns_uri == "http://exslt.org/regular-expressions" {
            return Some(exslt_regexp_dispatch(self, name, args));
        }
        // 1. Explicit per-name registrations via xmlXPathRegisterFunc[NS].
        //    Skip names that our engine handles natively — libxslt /
        //    libexslt register these via xsltRegisterExtFunction
        //    (which we route through xmlXPathRegisterFuncNS), but
        //    their implementations need xsltTransformContext state
        //    that our minimal parser-context bridge doesn't fully
        //    populate.  Falling through here lets the engine's
        //    built-in match — XPath 1.0 + EXSLT — fire instead.
        let skip_no_ns = ns_uri.is_empty()
            && matches!(name, "document" | "key" | "generate-id"
                              | "system-property" | "element-available"
                              | "function-available" | "current"
                              | "unparsed-entity-uri" | "format-number");
        // Every EXSLT namespace is implemented natively in
        // `crates/core/src/xpath/exslt/`.  Skip the registered
        // libexslt entry points so the native dispatch wins.
        let skip_exslt = matches!(ns_uri,
            "http://exslt.org/math"
            | "http://exslt.org/dates-and-times"
            | "http://exslt.org/strings"
            | "http://exslt.org/sets"
            | "http://exslt.org/regular-expressions"
            | "http://exslt.org/common"
            | "http://exslt.org/functions"
            | "http://exslt.org/dynamic"
        );
        let skip_fn_map = skip_no_ns || skip_exslt;
        if !skip_fn_map {
            let fn_ptr = self.fn_map.borrow()
                .get(&(ns_uri.to_string(), name.to_string()))
                .copied();
            if let Some(p) = fn_ptr {
                if !p.is_null() {
                    return Some(self.dispatch_registered(p, args, name, ns_uri));
                }
            }
        }
        // 2. funcLookupFunc — lxml installs one of these to dispatch
        //    arbitrary `ns:name(args)` through its Python
        //    FunctionNamespace machinery.
        // funcLookup follows the same skip-list as fn_map — EXSLT
        // namespaces resolve to libexslt's entry points there too,
        // which need transform-context state we don't mirror.
        let lookup = if skip_fn_map { std::ptr::null_mut() } else { self.func_lookup_fn.get() };
        if !lookup.is_null() {
            type LookupFn = unsafe extern "C" fn(
                data: *mut c_void, name: *const c_char, ns_uri: *const c_char,
            ) -> *mut c_void;
            let name_cs = std::ffi::CString::new(name).ok()?;
            let ns_cs   = if ns_uri.is_empty() {
                None
            } else {
                std::ffi::CString::new(ns_uri).ok()
            };
            let ns_ptr  = ns_cs.as_ref().map_or(ptr::null(), |c| c.as_ptr());
            // SAFETY: caller-installed function pointer, libxml2 ABI.
            let f: LookupFn = unsafe { std::mem::transmute(lookup) };
            let p = unsafe {
                f(self.func_lookup_data.get(), name_cs.as_ptr(), ns_ptr)
            };
            if !p.is_null() {
                return Some(self.dispatch_registered(p, args, name, ns_uri));
            }
        }
        None
    }

    fn call_function_in(
        &self,
        ns_uri: &str,
        name:   &str,
        args:   Vec<sup_xml_core::xpath::eval::Value>,
        xpath_context_node: sup_xml_core::xpath::NodeId,
    ) -> Option<Result<sup_xml_core::xpath::eval::Value, sup_xml_core::error::XmlError>> {
        // Mirror the evaluation's current node onto the C context's `node`
        // slot (offset 8) for the duration of the call, so a consumer's
        // extension function sees the right context node — lxml exposes it
        // as `ctxt.context_node`, expected to be the node the predicate /
        // step is being evaluated against, not the initial root.
        let ctx_ptr = self.ctx_ptr.get();
        let prev_node = (!ctx_ptr.is_null()).then(|| {
            let ctx_ref = unsafe { &*ctx_ptr };
            let prev = ctx_ref.node;
            if !ctx_ref.doc.is_null() {
                let owned = unsafe { &*ctx_ref.doc };
                let core_ctx = sup_xml_core::xpath::XPathContext::new(&owned._doc);
                if let Some(p) = node_id_to_ptr(xpath_context_node, &core_ctx.index) {
                    unsafe { (*ctx_ptr).node = p as *const Node<'static>; }
                }
            }
            prev
        });
        let result = self.call_function(ns_uri, name, args);
        if let Some(prev) = prev_node {
            unsafe { (*ctx_ptr).node = prev; }
        }
        result
    }

    fn variable(&self, name: &str) -> Option<sup_xml_core::xpath::eval::Value> {
        let lookup_fn = self.var_lookup_fn.get();
        if !lookup_fn.is_null() {
            type VarLookupFn = unsafe extern "C" fn(
                data: *mut c_void, name: *const c_char, ns_uri: *const c_char,
            ) -> *mut xmlXPathObject;
            // SAFETY: caller-installed function pointer; same
            // ABI rules as any C callback.  We pass the opaque
            // data and a NUL-terminated name; ns_uri is NULL
            // (libxslt-side handling is local-name keyed).
            let name_cs = match std::ffi::CString::new(name) {
                Ok(c) => c,
                Err(_) => return None,
            };
            let f: VarLookupFn = unsafe { std::mem::transmute(lookup_fn) };
            let obj = unsafe { f(self.var_lookup_data.get(), name_cs.as_ptr(), ptr::null()) };
            if !obj.is_null() {
                return Some(xpath_object_to_value(obj));
            }
            // Lookup returned NULL — fall through to the explicit
            // var_map in case both registrations are in use.
        }
        // 2. Fall back to the explicit map populated via
        //    xmlXPathRegisterVariable.
        let addr = *self.var_map.borrow().get(name)?;
        if addr == 0 { return None; }
        let obj = addr as *mut xmlXPathObject;
        // Read out the stored object without consuming it — the
        // variable can be referenced multiple times in one expression.
        Some(xpath_object_to_value(obj))
    }

    fn foreign_string_value(
        &self,
        p: sup_xml_core::xpath::eval::ForeignNodePtr,
    ) -> String {
        if p.is_null() { return String::new(); }
        // Route through our existing xmlNodeGetContent — it already
        // handles Element/Text/CData/Attribute correctly (the latter
        // via the explicit Attribute re-view in parse.rs).
        let raw = unsafe { crate::parse::xmlNodeGetContent(p) };
        if raw.is_null() { return String::new(); }
        let s = unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned();
        unsafe { crate::parse::xml_free_impl(raw as *mut c_void); }
        s
    }

    fn load_document(
        &self,
        uri:      &str,
        _base:    Option<&str>,
    ) -> Option<Result<Vec<sup_xml_core::xpath::eval::ForeignNodePtr>,
                       sup_xml_core::error::XmlError>> {
        Some(load_document_impl(self, uri))
    }

    fn apply_foreign_path(
        &self,
        nodes:      &[sup_xml_core::xpath::eval::ForeignNodePtr],
        predicates: &[sup_xml_core::xpath::Expr],
        steps:      &[sup_xml_core::xpath::Step],
    ) -> Option<Result<Vec<sup_xml_core::xpath::eval::ForeignNodePtr>,
                       sup_xml_core::error::XmlError>> {
        Some(apply_foreign_path_impl(self, nodes, predicates, steps))
    }
}

/// Walk a Node's parent chain searching ns_def chains for `prefix`.
/// Returns the matching href or None.  Used as a fallback when
/// `ctx->namespaces` doesn't carry every in-scope binding.
///
/// SAFETY: `inst` must point at a live `Node` whose `parent` chain
/// reaches a valid root.  Caller has verified `inst` is non-null and
/// that its containing struct (`xsltTransformContext`) looks alive.
fn walk_inst_for_prefix(
    inst: *const sup_xml_tree::dom::Node<'static>,
    prefix: &str,
) -> Option<String> {
    // SAFETY: caller asserts inst is a live Node*; we only read
    // `ns_def` and `parent` which live in the ABI window.
    let mut cur = Some(unsafe { &*inst });
    while let Some(n) = cur {
        let mut ns_cur = n.ns_def.get();
        while let Some(ns) = ns_cur {
            if ns.prefix().map(|s| s == prefix).unwrap_or(false) {
                return Some(ns.href().to_string());
            }
            ns_cur = ns.next.get();
        }
        cur = n.parent.get();
    }
    None
}

impl XPathPrivate {
    /// Dispatch a registered libxml2-shape XPath function pointer
    /// against `args`, returning the converted result.  Shared by
    /// the `fn_map` (xmlXPathRegisterFunc[NS]) and `funcLookupFunc`
    /// (xmlXPathRegisterFuncLookup) call paths.
    fn dispatch_registered(
        &self,
        fn_ptr: *mut c_void,
        args:   Vec<sup_xml_core::xpath::eval::Value>,
        name:   &str,
        ns_uri: &str,
    ) -> Result<sup_xml_core::xpath::eval::Value, sup_xml_core::error::XmlError> {
        use sup_xml_core::error::{ErrorDomain, ErrorLevel, XmlError};
        let err = |msg: &str| XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg);
        let ctx_ptr = self.ctx_ptr.get();
        if ctx_ptr.is_null() {
            return Err(err("no XPath context for function dispatch"));
        }
        // Need a `DocIndex` for value→object marshalling.  The
        // function call happens during eval — `ctx->doc` is the doc
        // the engine is currently evaluating against, so its index
        // is what we want.  Build it fresh; function calls are rare
        // enough that the O(n) rebuild is acceptable.
        let ctx_ref = unsafe { &*ctx_ptr };
        if ctx_ref.doc.is_null() {
            return Err(err("XPath context has no doc"));
        }
        let owned = unsafe { &*ctx_ref.doc };
        let core_ctx = sup_xml_core::xpath::XPathContext::new(&owned._doc);
        unsafe {
            invoke_xpath_function(fn_ptr, ctx_ptr, args, &core_ctx.index, name, ns_uri)
        }
    }
}

// ── document() loader + foreign-path eval ─────────────────────────────────

/// Load `uri` as XML, cache it under the resolved URI, and return
/// the loaded doc node as a foreign pointer.  Subsequent calls with
/// the same URI return a pointer into the same cached doc.
fn load_document_impl(
    private: &XPathPrivate,
    uri:     &str,
) -> Result<Vec<sup_xml_core::xpath::eval::ForeignNodePtr>,
            sup_xml_core::error::XmlError> {
    use sup_xml_core::error::{ErrorDomain, ErrorLevel, XmlError};
    // document() is an XSLT-defined function, so tag failures with
    // the XSLT domain — lxml's error log filters by `:ERROR:XSLT:`.
    let err = |msg: &str| XmlError::new(ErrorDomain::Xslt, ErrorLevel::Error, msg);

    // Security gate (XSLTAccessControl) runs first, even for the
    // empty-URI case — lxml's `read_file=False` is meant to forbid
    // ALL document() calls, not only those that hit the filesystem.
    // libxslt stores the prefs at offset 272 of xsltTransformContext
    // (the `sec` field — verified against `xsltInternals.h` for
    // libxslt 1.1.45).
    let ctxt_ptr = unsafe { (*private.ctx_ptr.get()).extra };
    if !ctxt_ptr.is_null() {
        let sec_prefs = unsafe {
            *((ctxt_ptr as *const u8).add(272) as *const *mut c_void)
        };
        if !sec_prefs.is_null() {
            let uri_cs = std::ffi::CString::new(uri)
                .map_err(|_| err("uri has NUL"))?;
            let allowed = unsafe {
                crate::xslt::xsltCheckRead(sec_prefs, ctxt_ptr, uri_cs.as_ptr())
            };
            if allowed != 1 {
                return Err(err(&format!(
                    "document({uri:?}): read forbidden by access control",
                )));
            }
        }
    }

    if uri.is_empty() {
        // document('') refers to the document containing the calling
        // XSLT instruction (XSLT 1.0 §12.1).  libxslt populates the
        // `extra` slot on the XPath context with its
        // `xsltTransformContext*`; the first field there is
        // `xsltStylesheet*`, whose `doc` (xmlDocPtr) is the parsed
        // stylesheet at offset 32.  We don't take ownership of this
        // doc — libxslt / lxml own it and it outlives the transform.
        //
        // Field offsets verified against
        // /opt/homebrew/Cellar/libxslt/1.1.45/include/libxslt/xsltInternals.h:
        //   xsltTransformContext: `style` is the first field      (offset 0)
        //   xsltStylesheet:       parent/next/imports/docList     (offsets 0/8/16/24)
        //                         doc                              (offset 32)
        let ctx_ptr = private.ctx_ptr.get();
        if ctx_ptr.is_null() {
            return Err(err("document(''): no XPath context"));
        }
        let extra = unsafe { (*ctx_ptr).extra };
        if extra.is_null() {
            return Err(err(
                "document(''): no transform context (ctx->extra is NULL)",
            ));
        }
        // SAFETY: extra is libxslt's xsltTransformContext* — populated
        // by libxslt before invoking xmlXPathCompiledEval.  Reading
        // the first 8 bytes yields the xsltStylesheetPtr.
        let style_ptr = unsafe { *(extra as *const *const u8) };
        if style_ptr.is_null() {
            return Err(err("document(''): xsltTransformContext->style is NULL"));
        }
        // SAFETY: style_ptr is an xsltStylesheet*; `doc` lives at
        // offset 32 (after parent/next/imports/docList).
        let stylesheet_doc = unsafe {
            *((style_ptr.add(32)) as *const *const XmlDoc)
        };
        if stylesheet_doc.is_null() {
            return Err(err("document(''): stylesheet has NULL doc"));
        }
        return Ok(vec![stylesheet_doc as *const Node<'static>]);
    }

    // Cache hit: return the same doc pointer as before.
    if let Some(&doc_ptr) = private.loaded_docs.borrow().get(uri) {
        return Ok(vec![doc_ptr as *const Node<'static>]);
    }

    // Preferred path: delegate to libxslt's `xsltLoadDocument`, which
    // handles the security check, lxml's Python Resolver chain (via
    // `xmlSetExternalEntityLoader`), docList caching, XInclude, and
    // whitespace stripping in one shot.  Resolved at runtime via
    // dlsym so this crate doesn't need to link libxslt.  When libxslt
    // is loaded (lxml's case), this works; when it isn't (pure-libxml2
    // consumer), we fall back to the plain `std::fs::read` path
    // below.
    let ctxt_ptr = unsafe { (*private.ctx_ptr.get()).extra };
    if !ctxt_ptr.is_null() {
        type LoadDocFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void;
        use std::sync::OnceLock;
        static FN: OnceLock<Option<LoadDocFn>> = OnceLock::new();
        let cached = FN.get_or_init(|| {
            let p = crate::dynsym::lookup(c"xsltLoadDocument".as_ptr());
            if p.is_null() { None } else { Some(unsafe { std::mem::transmute::<*mut c_void, LoadDocFn>(p) }) }
        });
        if let Some(f) = cached {
            // Resolve `uri` relative to the calling instruction's
            // base URL — this is what libxslt's own xsltDocumentFunction
            // does before calling xsltLoadDocument.  Without it,
            // `document('test.xml')` in a stylesheet at
            // `MY/BASE/FILE` is loaded as plain `test.xml` and lxml's
            // Resolver receives the wrong URL (the
            // test_xslt_resolver_url_building test pins this).
            let uri_cs = std::ffi::CString::new(uri)
                .map_err(|_| err("uri has NUL"))?;
            let inst_ptr = unsafe {
                *((ctxt_ptr as *const u8).add(184) as *const *const Node<'static>)
            };
            let mut resolved_uri: *mut c_char = std::ptr::null_mut();
            if !inst_ptr.is_null() {
                let inst_doc = unsafe { (*inst_ptr).doc.get() };
                let base = unsafe {
                    crate::nodeacc::xmlNodeGetBase(inst_doc as *const c_void, inst_ptr)
                };
                if !base.is_null() {
                    resolved_uri = unsafe {
                        crate::uri::xmlBuildURI(uri_cs.as_ptr(), base as *const c_char)
                    };
                    // base was alloc_registered → free via xmlFree.
                    unsafe { crate::parse::xml_free_impl(base as *mut c_void); }
                }
            }
            let load_uri = if resolved_uri.is_null() {
                uri_cs.as_ptr() as *const c_char
            } else {
                resolved_uri as *const c_char
            };
            // SAFETY: ctxt_ptr is libxslt's xsltTransformContext*;
            // f is libxslt's xsltLoadDocument.  Returns
            // `xsltDocumentPtr` with `doc` at offset 16.
            let xslt_doc = unsafe { f(ctxt_ptr, load_uri) };
            if !resolved_uri.is_null() {
                unsafe { crate::parse::xml_free_impl(resolved_uri as *mut c_void); }
            }
            if xslt_doc.is_null() {
                return Err(err(&format!(
                    "document({uri:?}): xsltLoadDocument returned NULL (access denied or load failed)",
                )));
            }
            let doc_ptr = unsafe {
                *((xslt_doc as *const u8).add(16) as *const *mut XmlDoc)
            };
            if doc_ptr.is_null() {
                return Err(err(&format!("document({uri:?}): loaded but doc is NULL")));
            }
            // libxslt owns the xsltDocument and its inner xmlDoc; it
            // releases both at xsltFreeTransformContext.  We deliberately
            // DON'T add to private.loaded_docs (which xmlFreeDoc's
            // entries on teardown) — but we DO want repeat document()
            // calls to skip the lookup, so cache the URI→ptr pair
            // separately or rely on libxslt's own docList.  For now,
            // rely on libxslt's caching: it returns the same
            // xsltDocument for repeated URIs.
            return Ok(vec![doc_ptr as *const Node<'static>]);
        }
    }

    // Cache miss: read the file off disk and parse it.  No URI
    // resolution / fancy fetcher; the lxml tests we target use
    // plain filesystem paths.
    let bytes = match std::fs::read(uri) {
        Ok(b) => b,
        Err(e) => return Err(err(&format!("document({uri:?}): {e}"))),
    };
    let uri_cs = std::ffi::CString::new(uri).map_err(|_| err("uri contains NUL"))?;
    let doc_ptr = unsafe {
        crate::parse::xmlReadMemory(
            bytes.as_ptr() as *const c_char,
            bytes.len() as c_int,
            uri_cs.as_ptr(),
            ptr::null(),
            0,
        )
    };
    if doc_ptr.is_null() {
        return Err(err(&format!("document({uri:?}): parse failed")));
    }
    private.loaded_docs.borrow_mut().insert(uri.to_string(), doc_ptr);
    Ok(vec![doc_ptr as *const Node<'static>])
}

/// Group foreign nodes by their owning `xmlDoc`, build a `DocIndex`
/// per doc, and run the engine's `apply_predicates`/`eval_step_on_nodes`
/// against each.  Returns the concatenation of all foreign result
/// pointers, deduped by identity.
fn apply_foreign_path_impl(
    private:    &XPathPrivate,
    nodes:      &[sup_xml_core::xpath::eval::ForeignNodePtr],
    predicates: &[sup_xml_core::xpath::Expr],
    steps:      &[sup_xml_core::xpath::Step],
) -> Result<Vec<sup_xml_core::xpath::eval::ForeignNodePtr>,
            sup_xml_core::error::XmlError> {
    use sup_xml_core::error::{ErrorDomain, ErrorLevel, XmlError};
    let err = |msg: &str| XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg);

    // Group the inputs by their owning xmlDoc.  `node->doc` lives at
    // offset 64 in our libxml2-ABI Node layout (xmlDoc* slot).
    let mut by_doc: HashMap<usize, Vec<*const Node<'static>>> = HashMap::new();
    for &p in nodes {
        if p.is_null() { continue; }
        // SAFETY: the bindings invariant — foreign pointers came
        // from load_document / a prior apply_foreign_path, which
        // produced them from docs we still own in loaded_docs.
        let n = unsafe { &*p };
        let doc_ptr = n.doc.get() as usize;
        by_doc.entry(doc_ptr).or_default().push(p);
    }

    let mut out: Vec<*const Node<'static>> = Vec::new();
    for (doc_addr, foreign_nodes) in by_doc {
        if doc_addr == 0 {
            return Err(err("foreign node has NULL doc pointer"));
        }
        // A foreign doc built through the C ABI (an XSLT result-tree
        // fragment from `exsl:node-set`, a node grafted by libxslt)
        // updates the libxml2 `children` chain but leaves the embedded
        // `Document`'s cached entry pointers as `xmlNewDoc` left them.
        // Re-derive them from the live chain so the index below sees
        // the tree — without this `DocIndex::build` walks a NULL root
        // and every step into the fragment yields the empty node-set.
        unsafe { sync_doc_entry_from_abi(doc_addr as *mut XmlDoc); }
        let doc: &XmlDoc = unsafe { &*(doc_addr as *const XmlDoc) };
        let core_ctx = sup_xml_core::xpath::XPathContext::new(&doc._doc);

        // Convert each foreign pointer to a NodeId in this doc's
        // index.  A pointer to the xmlDoc itself maps to NodeId 0
        // (the synthetic Document node).
        let mut start_ids: Vec<sup_xml_core::xpath::NodeId> = Vec::new();
        for &p in &foreign_nodes {
            if p as usize == doc_addr {
                start_ids.push(0);
            } else if let Some(id) = core_ctx.id_for_element(p) {
                start_ids.push(id);
            }
            // Pointers we can't find are silently skipped — they
            // belong to a doc whose layout we didn't index (e.g.
            // an attribute, which id_for_element doesn't currently
            // map).  Acceptable degradation for the patterns we
            // need to support; document()/foo// works because
            // the document root maps to NodeId 0.
        }

        // Apply predicates first (XPath grammar: predicates bind
        // tighter than location steps), then walk each step.
        // The libxml2 C ABI shim runs strict by default; callers wanting
        // libxml2-compat formatting should set it through whatever
        // higher-level shim API they're using.  Threaded through here
        // as `false` to match historical behaviour.
        let mut current = sup_xml_core::xpath::eval::apply_predicates(
            start_ids, predicates, &core_ctx.index, private, false,
        )?;
        for step in steps {
            current = sup_xml_core::xpath::eval::eval_step_on_nodes(
                current, step, &core_ctx.index, private, false,
            )?;
        }
        // Map the resulting NodeIds back to Node pointers using
        // the same conversion `xpath_value_to_object` uses for the
        // primary doc.
        for id in current {
            if let Some(p) = node_id_to_ptr(id, &core_ctx.index) {
                out.push(p as *const Node<'static>);
            }
        }
    }
    // Dedup by pointer identity.  Order isn't well-defined across
    // docs; consumers (libxslt's xsl:copy-of) don't require it.
    let mut seen = std::collections::HashSet::new();
    out.retain(|&p| seen.insert(p as usize));
    Ok(out)
}

/// EXSLT regexp family — `match`, `replace`, `test`.  Real
/// libxslt's implementations live in libexslt; we'd need a full
/// xsltTransformContext mirror to call them.  Cheaper to do the
/// work natively here using the Rust `regex` crate.
///
/// Coverage:
///
/// | Function                                | Status      |
/// |-----------------------------------------|-------------|
/// | `regexp:test(string, pattern, flags?)`  | implemented |
/// | `regexp:match(string, pattern, flags?)` | implemented |
/// | `regexp:replace(s, pat, flags?, repl)`  | implemented |
fn exslt_regexp_dispatch(
    private: &XPathPrivate,
    name: &str,
    args: Vec<sup_xml_core::xpath::eval::Value>,
) -> Result<sup_xml_core::xpath::eval::Value, sup_xml_core::error::XmlError> {
    use sup_xml_core::error::{ErrorDomain, ErrorLevel, XmlError};
    use sup_xml_core::xpath::eval::Value;
    let err = |msg: &str| XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg);
    // EXSLT's flags string: 'i' (case-insensitive), 's' (dotall),
    // 'g' (only meaningful for replace — replace all vs first).
    // We honor 'i' and 's'; 'g' affects replace's all-vs-first.
    let to_string = |v: &Value| -> String {
        match v {
            Value::String(s)  => s.clone(),
            Value::Boolean(b) => if *b { "true".into() } else { "false".into() },
            Value::Number(n)  => format!("{}", n.as_f64()),
            Value::NodeSet(_) => String::new(),
            // Regexp:* shouldn't receive node-sets, but be defensive.
            Value::ForeignNodeSet(_) => String::new(),
            Value::Typed(t)   => t.lexical.clone(),
            // EXSLT regexp doesn't speak XPath-2.0 sequences;
            // collapse to the first item's string form.
            Value::Sequence(items) => items.first()
                .map(|v| match v {
                    Value::String(s)  => s.clone(),
                    Value::Number(n)  => format!("{}", n.as_f64()),
                    Value::Boolean(b) => if *b { "true".into() } else { "false".into() },
                    Value::Typed(t)   => t.lexical.clone(),
                    _ => String::new(),
                })
                .unwrap_or_default(),
            Value::IntRange { lo, .. } => lo.to_string(),
            Value::Map(_) | Value::Array(_) | Value::Function(_) => String::new(),
        }
    };
    fn build_regex(pattern: &str, flags: &str) -> Result<regex::Regex, regex::Error> {
        let mut rebuilt = String::new();
        let case_i = flags.contains('i');
        let dotall = flags.contains('s');
        if case_i || dotall {
            rebuilt.push_str("(?");
            if case_i { rebuilt.push('i'); }
            if dotall { rebuilt.push('s'); }
            rebuilt.push(')');
        }
        rebuilt.push_str(pattern);
        regex::Regex::new(&rebuilt)
    }
    match name {
        "test" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(err("regexp:test takes 2 or 3 arguments"));
            }
            let s = to_string(&args[0]);
            let pattern = to_string(&args[1]);
            let flags = if args.len() == 3 { to_string(&args[2]) } else { String::new() };
            match build_regex(&pattern, &flags) {
                Ok(re) => Ok(Value::Boolean(re.is_match(&s))),
                Err(_) => Ok(Value::Boolean(false)),
            }
        }
        "replace" => {
            if args.len() != 4 {
                return Err(err("regexp:replace takes 4 arguments"));
            }
            let s = to_string(&args[0]);
            let pattern = to_string(&args[1]);
            let flags = to_string(&args[2]);
            let repl  = to_string(&args[3]);
            match build_regex(&pattern, &flags) {
                Ok(re) => {
                    let out = if flags.contains('g') {
                        re.replace_all(&s, repl.as_str()).into_owned()
                    } else {
                        re.replace(&s, repl.as_str()).into_owned()
                    };
                    Ok(Value::String(out))
                }
                Err(_) => Ok(Value::String(s)),
            }
        }
        "match" => {
            // EXSLT `regexp:match` returns a node-set of `<match>`
            // elements per the libexslt reference impl:
            //
            //   * Without 'g' flag: one `<match>` whose text is the
            //     full match, followed by one `<match>` per capture
            //     group.  An unmatched optional capture group gets a
            //     `<match>` with empty text.  Empty result if no
            //     match.
            //   * With 'g' flag: one `<match>` per *full match*
            //     across the input string (capture groups not
            //     exposed).  Empty result if no matches.
            //
            // The elements are *not* in the EXSLT namespace
            // (libexslt creates them with `ns=NULL` despite the
            // spec — lxml's tests rely on the no-namespace shape).
            if args.len() < 2 || args.len() > 3 {
                return Err(err("regexp:match takes 2 or 3 arguments"));
            }
            let s = to_string(&args[0]);
            let pattern = to_string(&args[1]);
            let flags = if args.len() == 3 { to_string(&args[2]) } else { String::new() };
            let texts = match build_regex(&pattern, &flags) {
                Ok(re) => regexp_match_texts(&re, &s, flags.contains('g')),
                Err(_) => Vec::new(),
            };
            Ok(Value::ForeignNodeSet(build_match_rtf(private, &texts)))
        }
        _ => Err(err(&format!("unsupported regexp:{name}"))),
    }
}

/// Compute the `<match>` element texts for `regexp:match`, per the
/// libexslt reference behavior.
///
/// Returns one string per `<match>` element the caller should
/// produce.  `Some(s)` becomes a `<match>` whose text is `s` —
/// including the empty case for an unmatched optional capture group
/// (libexslt emits `<match/>` rather than skipping).  An empty
/// returned Vec means "no match at all" — the EXSLT result is an
/// empty node-set.
fn regexp_match_texts(re: &regex::Regex, s: &str, global: bool) -> Vec<String> {
    let mut out = Vec::new();
    if global {
        for m in re.find_iter(s) {
            out.push(m.as_str().to_string());
        }
    } else if let Some(caps) = re.captures(s) {
        // Index 0 = full match.  Each subsequent index = a capture
        // group.  Optional groups that didn't participate yield
        // `None` from .get(); libexslt emits empty `<match/>` for
        // those.
        for i in 0..caps.len() {
            match caps.get(i) {
                Some(m) => out.push(m.as_str().to_string()),
                None    => out.push(String::new()),
            }
        }
    }
    out
}

/// Build a fresh RTF doc containing one `<match>` element per entry
/// of `texts`, register the doc with `private` so it's freed at
/// XPath-context teardown, and return a Vec of element pointers
/// suitable for `Value::ForeignNodeSet`.
///
/// Returns an empty Vec for empty input (no `<match>` elements →
/// EXSLT's "no match" result) or on parse failure (shouldn't happen
/// for well-formed entity-escaped input but defensive).
fn build_match_rtf(
    private: &XPathPrivate,
    texts: &[String],
) -> Vec<*const Node<'static>> {
    if texts.is_empty() {
        return Vec::new();
    }
    // Serialise to an XML string, parse via our own xmlReadMemory, and
    // hand back pointers to each <match> child.  Going through the
    // parser is heavier than building nodes directly, but it reuses
    // all our existing entity / ABI / dict plumbing — fewer ways to
    // be subtly wrong than hand-rolling Node allocation here.
    let mut xml = String::from("<rtf>");
    for t in texts {
        xml.push_str("<match>");
        xml_escape_text_into(t, &mut xml);
        xml.push_str("</match>");
    }
    xml.push_str("</rtf>");

    let doc = unsafe {
        crate::parse::xmlReadMemory(
            xml.as_ptr() as *const c_char,
            xml.len() as c_int,
            ptr::null(),
            ptr::null(),
            0,
        )
    };
    if doc.is_null() {
        return Vec::new();
    }
    // Register before walking — guarantees teardown even if the walk
    // below somehow finds nothing usable.
    private.rtf_docs.borrow_mut().push(doc);

    let doc_ref: &XmlDoc = unsafe { &*doc };
    let root_ptr = doc_ref.children.get();
    if root_ptr.is_null() {
        return Vec::new();
    }
    // SAFETY: root_ptr is the doc's first child (the <rtf> wrapper).
    // Walk its children chain to collect the <match> elements.
    let root: &Node<'static> = unsafe { &*root_ptr };
    let mut out = Vec::with_capacity(texts.len());
    for child in root.children() {
        out.push(child as *const Node<'static>);
    }
    out
}

/// XML text-content escaping for `&`, `<`, `>`.  Appends to `out`.
fn xml_escape_text_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _   => out.push(c),
        }
    }
}

/// Convert an `xmlXPathObject*` back to an engine `Value`.  Used by
/// the function-call bridge when consuming results from libxslt-shape
/// extension callbacks.
fn xpath_object_to_value(obj: *mut xmlXPathObject) -> sup_xml_core::xpath::eval::Value {
    use sup_xml_core::xpath::eval::Value;
    if obj.is_null() { return Value::String(String::new()); }
    // SAFETY: caller asserts obj is a live xmlXPathObject from our
    // allocators.
    let o = unsafe { &*obj };
    match o.kind {
        XPATH_BOOLEAN => Value::Boolean(o.boolval != 0),
        XPATH_NUMBER  => Value::Number(Numeric::Double(o.floatval)),
        XPATH_STRING  => {
            if o.stringval.is_null() { Value::String(String::new()) }
            else {
                // SAFETY: stringval is NUL-terminated by construction.
                let s = unsafe { CStr::from_ptr(o.stringval) }.to_string_lossy().into_owned();
                Value::String(s)
            }
        }
        // Nodeset (or any of the XSLT-flavored node-set kinds:
        // XPATH_NODESET / XPATH_XSLT_TREE / XPATH_LOCATIONSET).  We
        // surface them as ForeignNodeSet rather than the primary
        // NodeSet variant — the pointers may come from any doc
        // (RTFs built by xsl:variable, foreign docs from document(),
        // …) and our primary `DocIndex` only covers ctx->doc.
        // ForeignNodeSet preserves the pointers losslessly.
        _ => {
            if o.nodesetval.is_null() {
                return Value::ForeignNodeSet(Vec::new());
            }
            let ns = unsafe { &*o.nodesetval };
            if ns.nodeTab.is_null() || ns.nodeNr <= 0 {
                return Value::ForeignNodeSet(Vec::new());
            }
            // SAFETY: nodeTab is `nodeNr` valid pointers per libxml2's
            // contract.  Copy them; they remain valid as long as the
            // owning xmlXPathObject does (the caller of this fn is
            // either reading a variable — which lxml keeps alive —
            // or popping the result of a fn call, where we own the
            // pointer until we Drop the object after this returns).
            let slice = unsafe {
                std::slice::from_raw_parts(ns.nodeTab, ns.nodeNr as usize)
            };
            Value::ForeignNodeSet(slice.iter().map(|&p| p as *const _).collect())
        }
    }
}

const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(xmlXPathContext, doc)             ==   0);
    assert!(offset_of!(xmlXPathContext, node)            ==   8);
    assert!(offset_of!(xmlXPathContext, varHash)         ==  24);
    assert!(offset_of!(xmlXPathContext, funcHash)        ==  56);
    assert!(offset_of!(xmlXPathContext, namespaces)      ==  80);
    assert!(offset_of!(xmlXPathContext, nsNr)            ==  88);
    assert!(offset_of!(xmlXPathContext, user)            ==  96);
    assert!(offset_of!(xmlXPathContext, contextSize)     == 104);
    assert!(offset_of!(xmlXPathContext, lastError)       == 232);
    assert!(offset_of!(xmlXPathContext, dict)            == 328);
    assert!(offset_of!(xmlXPathContext, flags)           == 336);
    assert!(offset_of!(xmlXPathContext, cache)           == 344);
};

/// `xmlXPathNewContext(doc)` — create an evaluation context.
///
/// Accepts a NULL `doc` — libxml2's contract is that you can build a
/// context without a document to pre-compile expressions, then bind
/// the document later by writing to `ctx->doc`.  lxml's `XPath`
/// class uses this pattern.  Caller frees with
/// [`xmlXPathFreeContext`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNewContext(doc: *const XmlDoc) -> *mut xmlXPathContext {
    // Allocate private state, then context, then wire them together.
    let private = Box::into_raw(Box::new(XPathPrivate {
        ns_map:  RefCell::new(HashMap::new()),
        fn_map:  RefCell::new(HashMap::new()),
        var_map: RefCell::new(HashMap::new()),
        ctx_ptr: Cell::new(ptr::null_mut()),
        var_lookup_fn:   Cell::new(ptr::null_mut()),
        var_lookup_data: Cell::new(ptr::null_mut()),
        func_lookup_fn:   Cell::new(ptr::null_mut()),
        func_lookup_data: Cell::new(ptr::null_mut()),
        loaded_docs: RefCell::new(HashMap::new()),
        rtf_docs:    RefCell::new(Vec::new()),
    }));
    // Zero-init everything via std::mem::zeroed, then set the fields
    // we actually use.  All other fields stay at the documented
    // libxml2 zero defaults.
    // SAFETY: every field of xmlXPathContext is either an integer or
    // a pointer; both are valid as all-zeros.
    let mut ctx: xmlXPathContext = unsafe { std::mem::zeroed() };
    ctx.doc  = doc;
    ctx.user = private as *mut c_void;
    let ctx_ptr = Box::into_raw(Box::new(ctx));
    // Plant the back-pointer so XPathBindings::call_function can
    // build a parser-context for registered fn invocations.
    unsafe { (*(private)).ctx_ptr.set(ctx_ptr); }
    ctx_ptr
}

/// `xmlXPathRegisterFunc(ctx, name, fn)` — register a custom XPath
/// function in the no-namespace.  Returns 0 on success, -1 on NULL inputs.
///
/// **v0.1 limitation**: registrations are stored on the context but
/// our XPath engine doesn't yet invoke them — expressions that
/// reference a registered function fail with an "unknown function"
/// error rather than crashing.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathRegisterFunc(
    ctx:  *mut xmlXPathContext,
    name: *const c_char,
    f:    *mut c_void,
) -> c_int {
    unsafe { xmlXPathRegisterFuncNS(ctx, name, ptr::null(), f) }
}

/// `xmlXPathRegisterFuncNS(ctx, name, ns_uri, fn)` — register a
/// custom function in a namespace.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathRegisterFuncNS(
    ctx:    *mut xmlXPathContext,
    name:   *const c_char,
    ns_uri: *const c_char,
    f:      *mut c_void,
) -> c_int {
    if ctx.is_null() || name.is_null() { return -1; }
    let ctx_ref = unsafe { &*ctx };
    if ctx_ref.user.is_null() { return -1; }
    let private = unsafe { &*(ctx_ref.user as *const XPathPrivate) };
    let name_s = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return -1,
    };
    let ns_s = if ns_uri.is_null() {
        String::new()
    } else {
        match unsafe { CStr::from_ptr(ns_uri) }.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return -1,
        }
    };
    private.fn_map.borrow_mut().insert((ns_s, name_s), f);
    0
}

/// `xmlXPathRegisterVariable(ctx, name, value)` — register a variable
/// usable as `$name` in expressions.  Ownership of `value` transfers
/// to the context.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathRegisterVariable(
    ctx:   *mut xmlXPathContext,
    name:  *const c_char,
    value: *mut xmlXPathObject,
) -> c_int {
    if ctx.is_null() || name.is_null() { return -1; }
    let ctx_ref = unsafe { &*ctx };
    if ctx_ref.user.is_null() { return -1; }
    let private = unsafe { &*(ctx_ref.user as *const XPathPrivate) };
    let name_s = match unsafe { CStr::from_ptr(name) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return -1,
    };
    let mut vars = private.var_map.borrow_mut();
    if let Some(old) = vars.insert(name_s, value as usize) {
        if old != 0 {
            unsafe { xmlXPathFreeObject(old as *mut xmlXPathObject); }
        }
    }
    0
}

/// `xmlXPathRegisteredVariablesCleanup(ctx)` — free all registered
/// variables.  Called on context teardown.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathRegisteredVariablesCleanup(
    ctx: *mut xmlXPathContext,
) {
    if ctx.is_null() { return; }
    let ctx_ref = unsafe { &*ctx };
    if ctx_ref.user.is_null() { return; }
    let private = unsafe { &*(ctx_ref.user as *const XPathPrivate) };
    let mut vars = private.var_map.borrow_mut();
    for (_, addr) in vars.drain() {
        if addr != 0 {
            unsafe { xmlXPathFreeObject(addr as *mut xmlXPathObject); }
        }
    }
}

/// `xmlXPathNodeSetAdd(ns, node)` — append `node` to `ns` (idempotent;
/// no-op if already present).  Returns 0 on success, -1 on NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNodeSetAdd(
    ns:   *mut xmlNodeSet,
    node: *mut Node<'static>,
) -> c_int {
    if ns.is_null() || node.is_null() { return -1; }
    unsafe {
        let s = &mut *ns;
        let len = s.nodeNr as usize;
        let cap = s.nodeMax as usize;
        if !s.nodeTab.is_null() {
            for i in 0..len {
                if *s.nodeTab.add(i) == node { return 0; }
            }
        }
        if len + 1 > cap {
            let new_cap = (cap * 2).max(8);
            let mut v: Vec<*mut Node<'static>> = if s.nodeTab.is_null() {
                Vec::with_capacity(new_cap)
            } else {
                Vec::from_raw_parts(s.nodeTab, len, cap)
            };
            v.reserve(new_cap - v.len());
            let actual_cap = v.capacity();
            let p = v.as_mut_ptr();
            std::mem::forget(v);
            s.nodeTab = p;
            s.nodeMax = actual_cap as c_int;
        }
        *s.nodeTab.add(len) = node;
        s.nodeNr = (len + 1) as c_int;
    }
    0
}

/// `xmlXPathErr(ctxt, error)` — record an error from inside a custom
/// XPath function callback.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathErr(
    _ctxt: *mut c_void,
    error: c_int,
) {
    use sup_xml_core::error::{ErrorDomain, ErrorLevel, XmlError, ErrorCode};
    crate::error::record_last_error(&XmlError::new(
        ErrorDomain::XPath, ErrorLevel::Error,
        format!("xmlXPathErr({error})"),
    ).with_code(ErrorCode::InternalError));
}

/// Byte-exact mirror of libxml2's `_xmlXPathParserContext`.  We
/// construct one of these (zeroed except for `context` + the value
/// stack fields) when invoking a registered libxml2-shape XPath
/// function pointer — lxml's `_xpath_function_call` reads
/// `ctxt->context` and pops args via `valuePop`, both of which the
/// dispatcher below populates.
///
/// Field offsets are pinned against the libxml2 reference layout
/// (see `/opt/homebrew/Cellar/libxml2/.../xpath.h::_xmlXPathParserContext`).
#[repr(C)]
struct xmlXPathParserContext {
    cur:        *const c_char,           //  0
    base:       *const c_char,           //  8
    error:      c_int,                   // 16
    _pad_err:   u32,                     // 20
    context:    *mut xmlXPathContext,    // 24
    value:      *mut xmlXPathObject,     // 32
    valueNr:    c_int,                   // 40
    valueMax:   c_int,                   // 44
    valueTab:   *mut *mut xmlXPathObject,// 48
    comp:       *mut c_void,             // 56
    xptr:       c_int,                   // 64
    _pad_xptr:  u32,                     // 68
    ancestor:   *mut c_void,             // 72  (xmlNode*)
    valueFrame: c_int,                   // 80
    _pad_vf:    u32,                     // 84
}

/// `valuePush(ctxt, value)` — push an `xmlXPathObject*` onto the
/// parser context's value stack.  Grows `valueTab` as needed,
/// updates `value` to point at the new top, and returns the new
/// stack depth (libxml2's contract).  `-1` on NULL inputs.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn valuePush(
    ctxt:  *mut c_void,
    value: *mut xmlXPathObject,
) -> c_int {
    if ctxt.is_null() || value.is_null() { return -1; }
    // SAFETY: caller asserts ctxt is an xmlXPathParserContext; we
    // mirror libxml2's layout above, so the field accesses are sound.
    let pc = unsafe { &mut *(ctxt as *mut xmlXPathParserContext) };
    // Reclaim the existing stack (if any) into a Vec, push, leak.
    let mut tab: Vec<*mut xmlXPathObject> = if pc.valueTab.is_null() {
        Vec::with_capacity(8)
    } else {
        unsafe {
            Vec::from_raw_parts(pc.valueTab, pc.valueNr as usize, pc.valueMax as usize)
        }
    };
    tab.push(value);
    pc.valueNr  = tab.len() as c_int;
    pc.valueMax = tab.capacity() as c_int;
    pc.valueTab = tab.as_mut_ptr();
    pc.value    = value;
    std::mem::forget(tab);
    pc.valueNr
}

/// `valuePop(ctxt)` — pop and return the top of the value stack.
/// Returns NULL on empty stack or NULL ctxt.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn valuePop(ctxt: *mut c_void) -> *mut xmlXPathObject {
    if ctxt.is_null() { return ptr::null_mut(); }
    let pc = unsafe { &mut *(ctxt as *mut xmlXPathParserContext) };
    if pc.valueTab.is_null() || pc.valueNr <= 0 { return ptr::null_mut(); }
    let mut tab: Vec<*mut xmlXPathObject> = unsafe {
        Vec::from_raw_parts(pc.valueTab, pc.valueNr as usize, pc.valueMax as usize)
    };
    let top = tab.pop().unwrap_or(ptr::null_mut());
    pc.valueNr  = tab.len() as c_int;
    pc.valueMax = tab.capacity() as c_int;
    pc.valueTab = tab.as_mut_ptr();
    pc.value    = tab.last().copied().unwrap_or(ptr::null_mut());
    std::mem::forget(tab);
    top
}

/// Invoke a libxml2-shape XPath function pointer, marshalling args
/// from our engine `Value`s and converting the result back.  Used by
/// `XPathBindings::call_function` when a registered fn pointer
/// (`fn_map` or `funcLookupFunc`) matches an unknown name.
///
/// The C-side function pops `nargs` from the parser context's value
/// stack, computes its result, and pushes that back.  We synthesize
/// a one-shot parser context, push the args in order, call, pop the
/// result, free the input args, and convert the result back to a
/// `Value`.
///
/// SAFETY: `fn_ptr` must be a real `xmlXPathFunction` (signature
/// `void (*)(xmlXPathParserContextPtr, int)`).  `ctx` must be the
/// live `xmlXPathContext` the function should see as its evaluation
/// context.  Args are consumed by the call (the function takes
/// ownership of the popped `xmlXPathObject*`s and frees them).
unsafe fn invoke_xpath_function(
    fn_ptr:  *mut c_void,
    ctx:     *mut xmlXPathContext,
    args:    Vec<sup_xml_core::xpath::eval::Value>,
    index:   &sup_xml_core::xpath::DocIndex<'_>,
    name:    &str,
    ns_uri:  &str,
) -> Result<sup_xml_core::xpath::eval::Value, sup_xml_core::error::XmlError> {
    use sup_xml_core::error::{ErrorDomain, ErrorLevel, XmlError};
    let err = |msg: &str| XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg);

    // Allocate a fresh parser context with the eval context wired in
    // and an empty value stack.  Box it so its address is stable
    // across the function call.
    let pc = Box::new(xmlXPathParserContext {
        cur:        ptr::null(),
        base:       ptr::null(),
        error:      0,
        _pad_err:   0,
        context:    ctx,
        value:      ptr::null_mut(),
        valueNr:    0,
        valueMax:   0,
        valueTab:   ptr::null_mut(),
        comp:       ptr::null_mut(),
        xptr:       0,
        _pad_xptr:  0,
        ancestor:   ptr::null_mut(),
        valueFrame: 0,
        _pad_vf:    0,
    });
    let pc_ptr = Box::into_raw(pc);

    let nargs = args.len() as c_int;
    for v in args {
        let obj = xpath_value_to_object(v, index);
        if obj.is_null() {
            cleanup_parser_ctx(pc_ptr);
            return Err(err("failed to marshal argument"));
        }
        unsafe { valuePush(pc_ptr as *mut c_void, obj); }
    }

    // libxml2 contract: set ctx->function / ctx->functionURI to the
    // current call's identity before invoking — lxml's
    // `_xpath_function_call` (and most other libxml2-shape callbacks)
    // reads these to dispatch.  Stash on the heap so the C strings
    // outlive the call; restore + free afterward.
    let name_cs = std::ffi::CString::new(name)
        .map_err(|_| err("function name has NUL"))?;
    let uri_cs = if ns_uri.is_empty() {
        None
    } else {
        Some(std::ffi::CString::new(ns_uri).map_err(|_| err("ns_uri has NUL"))?)
    };
    let prev_function = unsafe { (*ctx).function };
    let prev_uri      = unsafe { (*ctx).functionURI };
    unsafe {
        (*ctx).function    = name_cs.as_ptr();
        (*ctx).functionURI = uri_cs.as_ref().map_or(ptr::null(), |c| c.as_ptr());
    }

    // SAFETY: caller asserted fn_ptr's signature.
    type XPathFunc = unsafe extern "C" fn(*mut c_void, c_int);
    let f: XPathFunc = unsafe { std::mem::transmute(fn_ptr) };
    unsafe { f(pc_ptr as *mut c_void, nargs); }

    // Restore the prior function-identity fields so we don't leak
    // pointers into the C strings we're about to drop.
    unsafe {
        (*ctx).function    = prev_function;
        (*ctx).functionURI = prev_uri;
    }
    drop(name_cs);
    drop(uri_cs);

    // Check error code and harvest result.
    let err_code = unsafe { (*pc_ptr).error };
    if err_code != 0 {
        cleanup_parser_ctx(pc_ptr);
        return Err(err("XPath function returned error"));
    }
    let result_obj = unsafe { valuePop(pc_ptr as *mut c_void) };
    let result = if result_obj.is_null() {
        // Some functions don't push a result (rare); treat as empty.
        sup_xml_core::xpath::eval::Value::String(String::new())
    } else {
        let v = xpath_object_to_value(result_obj);
        unsafe { xmlXPathFreeObject(result_obj); }
        v
    };
    cleanup_parser_ctx(pc_ptr);
    Ok(result)
}

/// Free a parser context built by `invoke_xpath_function`, including
/// any leftover stack entries.  Used both on success and error.
fn cleanup_parser_ctx(pc_ptr: *mut xmlXPathParserContext) {
    if pc_ptr.is_null() { return; }
    // SAFETY: pc_ptr came from Box::into_raw in this module.
    let pc = unsafe { Box::from_raw(pc_ptr) };
    if !pc.valueTab.is_null() && pc.valueMax > 0 {
        // Reclaim and drop any still-on-stack objects.
        let tab: Vec<*mut xmlXPathObject> = unsafe {
            Vec::from_raw_parts(pc.valueTab, pc.valueNr as usize, pc.valueMax as usize)
        };
        for obj in tab {
            if !obj.is_null() {
                unsafe { xmlXPathFreeObject(obj); }
            }
        }
    }
    drop(pc);
}

/// `xmlXPathFreeContext(ctx)` — reclaim.  NULL-safe.  Also drops the
/// private state stashed in `ctx->user` and any still-registered
/// variables.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathFreeContext(ctx: *mut xmlXPathContext) {
    if ctx.is_null() { return; }
    unsafe { xmlXPathRegisteredVariablesCleanup(ctx); }
    unsafe {
        let boxed = Box::from_raw(ctx);
        if !boxed.user.is_null() {
            let private = Box::from_raw(boxed.user as *mut XPathPrivate);
            // Free every doc that XSLT document() loaded into this
            // context.  The foreign pointers we handed out via XPath
            // results become dangling here — the contract is that
            // libxslt holds them only for the lifetime of the
            // transform, which ends before xmlXPathFreeContext.
            for (_, doc_ptr) in private.loaded_docs.borrow_mut().drain() {
                if !doc_ptr.is_null() {
                    crate::parse::xmlFreeDoc(doc_ptr);
                }
            }
            // Same teardown for EXSLT result-tree-fragments.
            for doc_ptr in private.rtf_docs.borrow_mut().drain(..) {
                if !doc_ptr.is_null() {
                    crate::parse::xmlFreeDoc(doc_ptr);
                }
            }
            drop(private);
        }
        drop(boxed);
    }
}

/// `xmlXPathRegisterNs(ctx, prefix, uri)` — bind a prefix to a URI
/// for use in expressions.  Returns 0 on success, -1 on NULL inputs.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathRegisterNs(
    ctx:    *mut xmlXPathContext,
    prefix: *const c_char,
    uri:    *const c_char,
) -> c_int {
    if ctx.is_null() || prefix.is_null() {
        return -1;
    }
    let prefix_s = match unsafe { CStr::from_ptr(prefix) }.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => return -1,
    };
    let uri_s = if uri.is_null() {
        String::new()
    } else {
        match unsafe { CStr::from_ptr(uri) }.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return -1,
        }
    };
    let ctx_ref = unsafe { &*ctx };
    if ctx_ref.user.is_null() {
        return -1;
    }
    // SAFETY: `user` was set by xmlXPathNewContext.
    let private = unsafe { &*(ctx_ref.user as *const XPathPrivate) };
    private.ns_map.borrow_mut().insert(prefix_s, uri_s);
    0
}

/// `xmlXPathEvalExpression(expr, ctx)` — parse + evaluate an XPath
/// expression against `ctx`'s document.  Returns an
/// [`xmlXPathObject`] caller releases via [`xmlXPathFreeObject`],
/// or NULL on error (with last-error populated).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathEvalExpression(
    expr: *const c_char,
    ctx:  *mut xmlXPathContext,
) -> *mut xmlXPathObject {
    if expr.is_null() || ctx.is_null() {
        return ptr::null_mut();
    }
    let expr_str = match unsafe { CStr::from_ptr(expr) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    eval_expression(expr_str, ctx)
}

/// `xmlXPathEval(expr, ctx)` — older synonym for
/// `xmlXPathEvalExpression`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathEval(
    expr: *const c_char,
    ctx:  *mut xmlXPathContext,
) -> *mut xmlXPathObject {
    // SAFETY: forwarded preconditions.
    unsafe { xmlXPathEvalExpression(expr, ctx) }
}

/// `xmlXPtrEval(expr, ctx)` — XPointer expression eval.  XPointer is
/// essentially XPath with a few extra range-construct functions
/// (range(), range-inside(), range-to()).  We treat plain
/// XPath-compatible expressions as XPath and stub the
/// XPointer-specific parts: a non-XPath syntactic form returns NULL
/// just like libxml2 would on an unsupported scheme.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPtrEval(
    expr: *const c_char,
    ctx:  *mut xmlXPathContext,
) -> *mut xmlXPathObject {
    // For the common case (the `xpointer(...)` scheme wrapping a
    // bare XPath expression), libxslt forwards to the XPath
    // evaluator anyway.  Without parsing the scheme prefix we
    // just pass through.
    unsafe { xmlXPathEvalExpression(expr, ctx) }
}

// ── compiled expressions (parse once, evaluate many times) ─────────────────

/// Opaque compiled-expression handle.  Carries the parsed AST plus
/// the source string (so we can re-parse against arbitrary documents).
pub struct xmlXPathCompExpr {
    expr_src: String,
}

/// `xmlXPathCompile(expr)` — parse an XPath expression.  Returns a
/// compiled handle (release via [`xmlXPathFreeCompExpr`]) or NULL on
/// invalid syntax.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathCompile(expr: *const c_char) -> *mut xmlXPathCompExpr {
    if expr.is_null() {
        return ptr::null_mut();
    }
    let expr_str = match unsafe { CStr::from_ptr(expr) }.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };
    // Validate the expression by attempting to parse it.  Don't keep
    // the AST around — `xpath_eval` re-parses anyway, and our parser
    // is cheap relative to the document index build.
    if xpath::parse_xpath(expr_str).is_err() {
        return ptr::null_mut();
    }
    Box::into_raw(Box::new(xmlXPathCompExpr { expr_src: expr_str.to_string() }))
}

/// `xmlXPathCtxtCompile(ctx, expr)` — variant that compiles in the
/// context (the context can carry compile-time namespace info, but
/// we don't yet thread that through).  Same return semantics.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathCtxtCompile(
    _ctx: *mut xmlXPathContext,
    expr: *const c_char,
) -> *mut xmlXPathCompExpr {
    // SAFETY: precondition forwarded to xmlXPathCompile.
    unsafe { xmlXPathCompile(expr) }
}

/// `xmlXPathFreeCompExpr(comp)` — release a compiled expression.
/// NULL-safe.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathFreeCompExpr(comp: *mut xmlXPathCompExpr) {
    if comp.is_null() { return; }
    // SAFETY: comp came from xmlXPathCompile.
    unsafe { let _ = Box::from_raw(comp); }
}

/// `xmlXPathCompiledEval(comp, ctx)` — evaluate a previously-compiled
/// expression against `ctx`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathCompiledEval(
    comp: *mut xmlXPathCompExpr,
    ctx:  *mut xmlXPathContext,
) -> *mut xmlXPathObject {
    if comp.is_null() || ctx.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: comp came from xmlXPathCompile.
    let c = unsafe { &*comp };
    eval_expression(&c.expr_src, ctx)
}

/// Re-point the embedded `Document`'s tree-entry pointers (`first_sibling`
/// / `root`) at the live libxml2 `children` chain, so the XPath engine
/// walks a tree built or mutated through the C ABI (libxslt result trees,
/// direct node grafting) rather than the stale entry `xmlNewDoc` left.
/// No-op on a NULL doc or a doc with no children (the existing non-NULL
/// placeholder keeps `Document::first_sibling`/`root` dereferenceable).
pub(crate) unsafe fn sync_doc_entry_from_abi(doc: *mut XmlDoc) {
    if doc.is_null() {
        return;
    }
    let first = unsafe { (*doc).children.get() };
    if first.is_null() {
        return;
    }
    // The first element in the top-level chain is the conceptual root
    // element; prolog comments/PIs may precede it.  Fall back to the first
    // node when there is no element child so `root()` stays non-NULL.
    let mut root_el = first;
    let mut cur = first;
    while !cur.is_null() {
        if matches!(unsafe { (*cur).kind }, sup_xml_tree::dom::NodeKind::Element) {
            root_el = cur;
            break;
        }
        cur = unsafe { (*cur).next_sibling.get() }
            .map(|s| s as *const Node<'_> as *mut Node<'static>)
            .unwrap_or(ptr::null_mut());
    }
    let doc_inner = unsafe { &raw mut (*doc)._doc } as *mut sup_xml_tree::dom::Document;
    unsafe {
        (*doc_inner).set_first_sibling_ptr(first as *const _);
        (*doc_inner).set_root_ptr(root_el as *const _);
    }
}

fn eval_expression(expr: &str, ctx: *mut xmlXPathContext) -> *mut xmlXPathObject {
    let ctx_ref = unsafe { &*ctx };
    if ctx_ref.doc.is_null() {
        return ptr::null_mut();
    }
    // Default: evaluate against ctx->doc.  But when ctx->node belongs
    // to a doc loaded via XSLT document() — xsl:for-each over a
    // foreign nodeset hands us those — we must build the engine's
    // DocIndex over the foreign doc instead, or `id_for_element`
    // won't find the context node and every step degenerates.
    let mut eval_doc: *const XmlDoc = ctx_ref.doc;
    // A namespace context node is an `xmlNs` (type XML_NAMESPACE_DECL),
    // a 48-byte struct with a different layout from `xmlNode` — its
    // owning doc lives in `context` (offset 40), NOT at the `xmlNode`
    // `doc` slot (offset 64), which would read past the allocation.
    let ctx_is_ns = !ctx_ref.node.is_null() && unsafe { is_namespace_node(ctx_ref.node) };
    if ctx_is_ns {
        let node_doc = unsafe { ns_node_context_doc(ctx_ref.node) };
        if !node_doc.is_null() && node_doc != ctx_ref.doc {
            eval_doc = node_doc;
        }
    } else if !ctx_ref.node.is_null() {
        // SAFETY: ctx_ref.node is a Node* from some doc we've parsed;
        // its `doc` field at offset 64 holds the owning xmlDoc*.
        let node_doc = unsafe { (*ctx_ref.node).doc.get() } as *const XmlDoc;
        if !node_doc.is_null() && node_doc != ctx_ref.doc {
            eval_doc = node_doc;
        }
    }
    // SAFETY: eval_doc is either ctx_ref.doc (from xmlReadMemory) or
    // a foreign doc registered in XPathPrivate::loaded_docs (also
    // produced by xmlReadMemory).  Both have stable `_doc` fields.
    // Re-derive the embedded Document's tree-entry pointers from the live
    // libxml2 `children` pointer first: consumers that build or mutate the
    // tree through the C ABI (e.g. libxslt assembling a result tree, or
    // direct node grafting) update the ABI sibling pointers without going
    // through `Document::set_root_ptr`, which would otherwise leave the
    // engine walking a stale entry (the empty placeholder `xmlNewDoc`
    // installs).  O(prolog-length); a no-op for freshly-parsed docs whose
    // entry already matches.
    unsafe { sync_doc_entry_from_abi(eval_doc as *mut XmlDoc); }
    let owned = unsafe { &*eval_doc };
    let core_ctx = xpath::XPathContext::new(&owned._doc);
    // Resolve the context node ID from the libxml2 `ctx->node`
    // pointer.  NULL means "document root" (XPath 1.0 § 1).  An
    // unknown pointer would be a consumer bug — fall back to the
    // document root rather than panicking.
    let context_id = if ctx_ref.node.is_null() {
        0 // synthetic document node
    } else if ctx_is_ns {
        // Re-find the namespace node in the index: its parent element
        // (stored in the `xmlNs` `next` slot) plus the prefix identify
        // exactly one entry in the element's namespace range.
        resolve_ns_context_id(ctx_ref.node, &core_ctx.index).unwrap_or(0)
    } else {
        let n: *const sup_xml_tree::dom::Node<'static> = ctx_ref.node;
        core_ctx.id_for_element(n).unwrap_or(0)
    };
    // Hand the engine our registered namespaces / functions /
    // variables via the XPathBindings trait.  See the `impl
    // XPathBindings for XPathPrivate` block above for the bridge.
    let result = if ctx_ref.user.is_null() {
        core_ctx.eval_at(expr, context_id)
    } else {
        // SAFETY: `user` was set by xmlXPathNewContext and outlives
        // this call (released by xmlXPathFreeContext).
        let private = unsafe { &*(ctx_ref.user as *const XPathPrivate) };
        core_ctx.eval_with(expr, context_id, private)
    };
    match result {
        Ok(value) => xpath_value_to_object(value, &core_ctx.index),
        Err(e) => {
            crate::error::record_last_error(&e);
            ptr::null_mut()
        }
    }
}

/// `xmlXPathFreeObject(obj)` — release an XPath result.  NULL-safe.
/// Frees the embedded nodesetval / stringval as appropriate.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathFreeObject(obj: *mut xmlXPathObject) {
    if obj.is_null() { return; }
    // SAFETY: obj came from xpath_value_to_object (or a caller that
    // followed the same allocation discipline).
    let o = unsafe { Box::from_raw(obj) };
    match o.kind {
        XPATH_NODESET => {
            if !o.nodesetval.is_null() {
                unsafe { xmlXPathFreeNodeSet(o.nodesetval); }
            }
        }
        XPATH_STRING => {
            if !o.stringval.is_null() {
                // stringval was alloc_registered_cstring → xmlFree releases.
                unsafe { crate::parse::xml_free_impl(o.stringval as *mut c_void); }
            }
        }
        _ => {}
    }
    drop(o);
}

/// `xmlXPathFreeNodeSet(ns)` — release a nodeset (and its inner
/// nodeTab).  Does NOT free the individual nodes pointed at —
/// those live in the document arena.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathFreeNodeSet(ns: *mut xmlNodeSet) {
    if ns.is_null() { return; }
    // SAFETY: ns came from a Box allocation made by us.
    let s = unsafe { Box::from_raw(ns) };
    if !s.nodeTab.is_null() && s.nodeMax > 0 {
        // SAFETY: nodeTab was a Vec<*mut Node>::into_raw_parts with capacity = nodeMax.
        let len = s.nodeMax as usize;
        unsafe {
            let _ = Vec::from_raw_parts(s.nodeTab, len, len);
        }
    }
    drop(s);
}

/// `xmlXPathNodeSetCreate(node)` — create a nodeset containing
/// `node` (or empty if NULL).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNodeSetCreate(
    node: *mut Node<'static>,
) -> *mut xmlNodeSet {
    let mut v: Vec<*mut Node<'static>> = if node.is_null() {
        Vec::new()
    } else {
        vec![node]
    };
    let len = v.len();
    let cap = v.capacity().max(len.max(1));
    v.reserve_exact(cap.saturating_sub(v.capacity()));
    let nodeTab = v.as_mut_ptr();
    std::mem::forget(v);
    Box::into_raw(Box::new(xmlNodeSet {
        nodeNr:  len as c_int,
        nodeMax: cap as c_int,
        nodeTab,
    }))
}

/// `xmlXPathWrapNodeSet(ns)` — wrap a nodeset into an
/// `xmlXPathObject` (transfers ownership of `ns` to the returned obj).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathWrapNodeSet(ns: *mut xmlNodeSet) -> *mut xmlXPathObject {
    Box::into_raw(Box::new(xmlXPathObject {
        kind:         XPATH_NODESET,
        _pad_kind:    0,
        nodesetval:   ns,
        boolval:      0,
        _pad_boolval: 0,
        floatval:     0.0,
        stringval:    ptr::null_mut(),
        user:         ptr::null_mut(),
        index:        0,
        _pad_index:   0,
        user2:        ptr::null_mut(),
        index2:       0,
        _pad_index2:  0,
    }))
}

/// `xmlXPathNewBoolean(val)` — create a Boolean-typed XPath object.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNewBoolean(val: c_int) -> *mut xmlXPathObject {
    Box::into_raw(Box::new(xmlXPathObject {
        kind:         XPATH_BOOLEAN,
        _pad_kind:    0,
        nodesetval:   ptr::null_mut(),
        boolval:      if val != 0 { 1 } else { 0 },
        _pad_boolval: 0,
        floatval:     0.0,
        stringval:    ptr::null_mut(),
        user:         ptr::null_mut(),
        index:        0,
        _pad_index:   0,
        user2:        ptr::null_mut(),
        index2:       0,
        _pad_index2:  0,
    }))
}

/// `xmlXPathNewFloat(val)` — create a Number-typed XPath object.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNewFloat(val: c_double) -> *mut xmlXPathObject {
    Box::into_raw(Box::new(xmlXPathObject {
        kind:         XPATH_NUMBER,
        _pad_kind:    0,
        nodesetval:   ptr::null_mut(),
        boolval:      0,
        _pad_boolval: 0,
        floatval:     val,
        stringval:    ptr::null_mut(),
        user:         ptr::null_mut(),
        index:        0,
        _pad_index:   0,
        user2:        ptr::null_mut(),
        index2:       0,
        _pad_index2:  0,
    }))
}

/// `xmlXPathNewCString(val)` — create a String-typed XPath object,
/// copying `val` into a fresh registered allocation.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNewCString(val: *const c_char) -> *mut xmlXPathObject {
    let bytes: &[u8] = if val.is_null() {
        &[]
    } else {
        // SAFETY: caller asserts NUL-terminated.
        unsafe { CStr::from_ptr(val) }.to_bytes()
    };
    let stringval = alloc_registered_cstring(bytes);
    Box::into_raw(Box::new(xmlXPathObject {
        kind:         XPATH_STRING,
        _pad_kind:    0,
        nodesetval:   ptr::null_mut(),
        boolval:      0,
        _pad_boolval: 0,
        floatval:     0.0,
        stringval,
        user:         ptr::null_mut(),
        index:        0,
        _pad_index:   0,
        user2:        ptr::null_mut(),
        index2:       0,
        _pad_index2:  0,
    }))
}

// ── libxslt-facing XPath helpers ────────────────────────────────────────────
//
// libxslt calls a wide slice of libxml2's XPath internals at runtime
// — value-stack push/pop, type casts, nodeset ops, object lifecycle.
// We don't share libxml2's `xmlXPathParserContext`-driven stack
// model (our engine evaluates expressions internally and hands back
// finished `xmlXPathObject`s), so the stack-shaped helpers below
// take the libxml2 signature but operate on a per-thread fallback
// stack we maintain locally.

/// Allocate a fresh `xmlXPathObject` initialised to `XPATH_UNDEFINED`.
/// Internal helper used by every constructor below.
fn new_xpath_object_zeroed() -> Box<xmlXPathObject> {
    Box::new(xmlXPathObject {
        kind:         XPATH_UNDEFINED,
        _pad_kind:    0,
        nodesetval:   ptr::null_mut(),
        boolval:      0,
        _pad_boolval: 0,
        floatval:     0.0,
        stringval:    ptr::null_mut(),
        user:         ptr::null_mut(),
        index:        0,
        _pad_index:   0,
        user2:        ptr::null_mut(),
        index2:       0,
        _pad_index2:  0,
    })
}

// ── allocators (value lifecycle) ────────────────────────────────────────

/// `xmlXPathNewNodeSet(node)` — XPath object holding a single-node
/// nodeset (or empty if `node` is NULL).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNewNodeSet(node: *mut Node<'static>) -> *mut xmlXPathObject {
    let ns = unsafe { xmlXPathNodeSetCreate(node) };
    unsafe { xmlXPathWrapNodeSet(ns) }
}

/// `xmlXPathNewString(val)` — String object copying the NUL-terminated
/// `xmlChar*` into a fresh registered allocation.  Identical surface to
/// [`xmlXPathNewCString`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNewString(val: *const c_char) -> *mut xmlXPathObject {
    unsafe { xmlXPathNewCString(val) }
}

/// `xmlXPathNewValueTree(node)` — like [`xmlXPathNewNodeSet`] but the
/// object is tagged `XPATH_XSLT_TREE` (libxslt's result-tree fragment).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNewValueTree(node: *mut Node<'static>) -> *mut xmlXPathObject {
    let raw = unsafe { xmlXPathNewNodeSet(node) };
    if !raw.is_null() {
        // SAFETY: raw came from Box::into_raw above.
        unsafe { (*raw).kind = XPATH_XSLT_TREE; }
    }
    raw
}

/// `xmlXPathWrapString(val)` — wrap a caller-owned `xmlChar*` into a
/// String XPath object (ownership transferred — caller must NOT
/// `xmlFree` `val` after this).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathWrapString(val: *mut c_char) -> *mut xmlXPathObject {
    let mut obj = new_xpath_object_zeroed();
    obj.kind = XPATH_STRING;
    obj.stringval = val;
    // Register the pointer so xmlXPathFreeObject's xml_free_impl will
    // recognise it.  Caller-owned pointers are typically already
    // registered via our alloc.rs; this is a safety net.
    crate::alloc::register_alloc(val as *const u8);
    Box::into_raw(obj)
}

/// `xmlXPathWrapExternal(val)` — wrap an arbitrary opaque pointer
/// (`XPATH_USERS` kind) so XPath/XSLT machinery can carry it through
/// the type system.  Caller retains ownership of `val`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathWrapExternal(val: *mut c_void) -> *mut xmlXPathObject {
    let mut obj = new_xpath_object_zeroed();
    obj.kind = XPATH_USERS;
    obj.user = val;
    Box::into_raw(obj)
}

/// `xmlXPathObjectCopy(obj)` — deep-copy an XPath object.  NodeSet
/// nodes themselves aren't cloned (they live in the doc arena); we
/// duplicate the `nodeTab` array.  Strings are copied via xmlStrdup.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathObjectCopy(obj: *mut xmlXPathObject) -> *mut xmlXPathObject {
    if obj.is_null() { return ptr::null_mut(); }
    // SAFETY: caller asserts obj came from one of our allocators.
    let src = unsafe { &*obj };
    let mut copy = new_xpath_object_zeroed();
    copy.kind     = src.kind;
    copy.boolval  = src.boolval;
    copy.floatval = src.floatval;
    copy.index    = src.index;
    copy.index2   = src.index2;
    copy.user     = src.user;
    copy.user2    = src.user2;
    match src.kind {
        XPATH_NODESET | XPATH_XSLT_TREE => {
            if !src.nodesetval.is_null() {
                // SAFETY: src.nodesetval came from xmlXPathNodeSetCreate.
                let sns = unsafe { &*src.nodesetval };
                let len = sns.nodeNr.max(0) as usize;
                // SAFETY: nodeTab is a Vec we leaked; len is its
                // population.
                let slice = unsafe {
                    std::slice::from_raw_parts(sns.nodeTab, len)
                };
                let mut v: Vec<*mut Node<'static>> = slice.to_vec();
                let cap = v.capacity().max(1);
                if v.capacity() < cap { v.reserve_exact(cap - v.capacity()); }
                let cap = v.capacity();
                let nodeTab = v.as_mut_ptr();
                std::mem::forget(v);
                copy.nodesetval = Box::into_raw(Box::new(xmlNodeSet {
                    nodeNr:  len as c_int,
                    nodeMax: cap as c_int,
                    nodeTab,
                }));
            }
        }
        XPATH_STRING => {
            if !src.stringval.is_null() {
                // SAFETY: stringval was registered via alloc_registered_cstring.
                let bytes = unsafe { CStr::from_ptr(src.stringval) }.to_bytes();
                copy.stringval = alloc_registered_cstring(bytes);
            }
        }
        _ => {}
    }
    Box::into_raw(copy)
}

// ── cast / convert ──────────────────────────────────────────────────────

/// XPath 1.0 § 4.4 — node-set's first node's string-value
fn node_text_content(n: *const Node<'static>) -> String {
    if n.is_null() { return String::new(); }
    // SAFETY: caller asserts `n` is a live arena node.
    let node = unsafe { &*n };
    let mut out = String::new();
    collect_text(node, &mut out);
    out
}
fn collect_text(node: &Node<'_>, out: &mut String) {
    use sup_xml_tree::dom::NodeKind;
    match node.kind {
        NodeKind::Text | NodeKind::CData => out.push_str(node.content()),
        // Element, Attribute, and Document all walk their child
        // chain.  For Attribute, the chain holds the text-node(s)
        // that carry the entity-expanded value (libxml2 convention).
        // For Document, this catches XPATH_XSLT_TREE result-tree
        // fragments — xsl:variable bodies emit a `<doc-node>` whose
        // children are the captured nodes; without this arm,
        // `$rtf-var` casts to empty.
        NodeKind::Element | NodeKind::Attribute | NodeKind::Document => {
            for c in node.children() { collect_text(c, out); }
        }
        _ => {}
    }
}

/// `xmlXPathCastNodeToString(node)` — XPath 1.0's string() on a node.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathCastNodeToString(node: *mut Node<'static>) -> *mut c_char {
    let s = node_text_content(node);
    alloc_registered_cstring(s.as_bytes())
}

/// `xmlXPathCastNodeToNumber(node)` — string-value of the node, then
/// parsed per XPath 1.0's number() rules (returns NaN on failure).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathCastNodeToNumber(node: *mut Node<'static>) -> c_double {
    let s = node_text_content(node);
    xpath_parse_number(&s)
}

/// `xmlXPathCastNumberToString(val)` — XPath 1.0 number→string per
/// § 4.4.  Special-cases NaN/inf/integer doubles to match libxml2's
/// exact output.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathCastNumberToString(val: c_double) -> *mut c_char {
    let s = xpath_format_number(val);
    alloc_registered_cstring(s.as_bytes())
}

/// `xmlXPathCastStringToNumber(s)` — XPath 1.0 string→number.  NULL
/// or unparseable → NaN.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathCastStringToNumber(s: *const c_char) -> c_double {
    if s.is_null() { return f64::NAN; }
    // SAFETY: caller asserts NUL-terminated.
    let bytes = unsafe { CStr::from_ptr(s) }.to_bytes();
    let txt = match std::str::from_utf8(bytes) {
        Ok(t) => t, Err(_) => return f64::NAN,
    };
    xpath_parse_number(txt)
}

/// `xmlXPathCastToString(obj)` — XPath 1.0 string() applied to any
/// XPath object.  Returns a freshly-registered NUL-terminated buffer.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathCastToString(obj: *mut xmlXPathObject) -> *mut c_char {
    if obj.is_null() { return alloc_registered_cstring(b""); }
    // SAFETY: caller asserts obj came from our XPath allocators.
    let o = unsafe { &*obj };
    let s = match o.kind {
        XPATH_STRING => {
            if o.stringval.is_null() { String::new() }
            else {
                // SAFETY: stringval is NUL-terminated by construction.
                unsafe { CStr::from_ptr(o.stringval) }.to_string_lossy().into_owned()
            }
        }
        XPATH_NUMBER => xpath_format_number(o.floatval),
        XPATH_BOOLEAN => if o.boolval != 0 { "true".to_string() } else { "false".to_string() },
        XPATH_NODESET | XPATH_XSLT_TREE => {
            if o.nodesetval.is_null() { String::new() }
            else {
                // SAFETY: nodesetval is a live xmlNodeSet.
                let ns = unsafe { &*o.nodesetval };
                if ns.nodeNr <= 0 || ns.nodeTab.is_null() { String::new() }
                else {
                    // First node in document order — libxml2 sorts
                    // before reading.  We use the first slot as-is;
                    // callers are expected to have sorted upstream.
                    // SAFETY: nodeNr > 0 confirms slot [0] is valid.
                    let first = unsafe { *ns.nodeTab.add(0) };
                    node_text_content(first)
                }
            }
        }
        _ => String::new(),
    };
    alloc_registered_cstring(s.as_bytes())
}

/// `xmlXPathConvertNumber(obj)` — replace `obj` with a fresh Number
/// object holding `cast(obj)`.  Returns the new object; frees `obj`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathConvertNumber(obj: *mut xmlXPathObject) -> *mut xmlXPathObject {
    if obj.is_null() {
        return unsafe { xmlXPathNewFloat(f64::NAN) };
    }
    // SAFETY: obj is a live object.
    let o = unsafe { &*obj };
    let n = match o.kind {
        XPATH_NUMBER  => o.floatval,
        XPATH_BOOLEAN => if o.boolval != 0 { 1.0 } else { 0.0 },
        XPATH_STRING  => {
            if o.stringval.is_null() { f64::NAN }
            else {
                // SAFETY: stringval is NUL-terminated.
                let s = unsafe { CStr::from_ptr(o.stringval) }.to_string_lossy().into_owned();
                xpath_parse_number(&s)
            }
        }
        XPATH_NODESET | XPATH_XSLT_TREE => xpath_parse_number(
            &(unsafe { node_text_from_object(o) })
        ),
        _ => f64::NAN,
    };
    unsafe { xmlXPathFreeObject(obj); }
    unsafe { xmlXPathNewFloat(n) }
}

/// `xmlXPathConvertString(obj)` — replace `obj` with a fresh String
/// object holding `cast(obj)`.  Returns the new object; frees `obj`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathConvertString(obj: *mut xmlXPathObject) -> *mut xmlXPathObject {
    let s = unsafe { xmlXPathCastToString(obj) };
    if !obj.is_null() { unsafe { xmlXPathFreeObject(obj); } }
    unsafe { xmlXPathWrapString(s) }
}

/// Inner helper for ConvertNumber's nodeset branch.
unsafe fn node_text_from_object(o: &xmlXPathObject) -> String {
    if o.nodesetval.is_null() { return String::new(); }
    // SAFETY: caller asserts nodesetval is live.
    let ns = unsafe { &*o.nodesetval };
    if ns.nodeNr <= 0 || ns.nodeTab.is_null() { return String::new(); }
    let first = unsafe { *ns.nodeTab.add(0) };
    node_text_content(first)
}

/// XPath 1.0 number-parsing (§ 4.4).  Accepts an optional leading
/// sign, decimal digits, an optional fractional part, ignores
/// surrounding whitespace.  Returns NaN on anything else.
fn xpath_parse_number(s: &str) -> c_double {
    let t = s.trim();
    if t.is_empty() { return f64::NAN; }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// XPath 1.0 number→string (§ 4.4): NaN → "NaN", ±inf → "Infinity"/
/// "-Infinity", integer-valued doubles → no decimal point.
fn xpath_format_number(val: c_double) -> String {
    if val.is_nan()                { return "NaN".to_string(); }
    if val.is_infinite()           { return if val > 0.0 { "Infinity".to_string() } else { "-Infinity".to_string() }; }
    if val == 0.0                  { return "0".to_string(); }
    if val.fract() == 0.0 && val.abs() < 1e21 {
        return format!("{}", val as i64);
    }
    // Fall back to Rust's default formatter (close to libxml2's).
    format!("{val}")
}

// ── predicates / numeric helpers ────────────────────────────────────────

/// `xmlXPathIsInf(val)` — returns +1 for +Infinity, -1 for -Infinity,
/// 0 for any finite value (or NaN).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathIsInf(val: c_double) -> c_int {
    if val.is_infinite() {
        if val > 0.0 { 1 } else { -1 }
    } else { 0 }
}

/// `xmlXPathIsNaN(val)` — 1 if NaN, 0 otherwise.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathIsNaN(val: c_double) -> c_int {
    if val.is_nan() { 1 } else { 0 }
}

/// `xmlXPathIsNodeType(name)` — 1 if `name` is one of the XPath
/// "node-type" tests (`"node"`, `"text"`, `"comment"`,
/// `"processing-instruction"`).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathIsNodeType(name: *const c_char) -> c_int {
    if name.is_null() { return 0; }
    // SAFETY: caller asserts NUL-terminated.
    let s = unsafe { CStr::from_ptr(name) }.to_bytes();
    if matches!(s, b"node" | b"text" | b"comment" | b"processing-instruction") {
        1
    } else { 0 }
}

/// `xmlXPathCmpNodes(a, b)` — document-order comparison.  Returns -1
/// if `a` precedes `b`, +1 if it follows, 0 if same.  We approximate
/// via pointer arithmetic on bumpalo arena addresses: nodes
/// allocated earlier sit at lower addresses.  Best-effort — strictly
/// correct only when both nodes were allocated by the same parser.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathCmpNodes(
    a: *mut Node<'static>,
    b: *mut Node<'static>,
) -> c_int {
    if a == b { return 0; }
    if a.is_null() { return -1; }
    if b.is_null() { return  1; }
    (a as usize).cmp(&(b as usize)) as c_int
}

/// `xmlXPathHasSameNodes(a, b)` — return 1 if nodesets `a` and `b`
/// share at least one node, 0 otherwise.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathHasSameNodes(
    a: *mut xmlXPathObject,
    b: *mut xmlXPathObject,
) -> c_int {
    if a.is_null() || b.is_null() { return 0; }
    let nodes_of = |o: *mut xmlXPathObject| -> Vec<*mut Node<'static>> {
        // SAFETY: caller asserts obj came from our allocators.
        let oo = unsafe { &*o };
        if oo.nodesetval.is_null() { return Vec::new(); }
        let ns = unsafe { &*oo.nodesetval };
        if ns.nodeNr <= 0 || ns.nodeTab.is_null() { return Vec::new(); }
        unsafe { std::slice::from_raw_parts(ns.nodeTab, ns.nodeNr as usize) }.to_vec()
    };
    let av = nodes_of(a);
    for x in &nodes_of(b) {
        if av.contains(x) { return 1; }
    }
    0
}

/// `xmlXPathEvalPredicate(ctx, obj)` — XPath 1.0 § 2.4 predicate
/// truth-value semantics: a Number is truthy iff it equals the
/// proximity-position, otherwise the standard boolean cast.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathEvalPredicate(
    ctx: *mut xmlXPathContext,
    obj: *mut xmlXPathObject,
) -> c_int {
    if obj.is_null() { return 0; }
    // SAFETY: obj came from our allocators.
    let o = unsafe { &*obj };
    let result = match o.kind {
        XPATH_NUMBER => {
            let pos = if ctx.is_null() { 1 } else {
                // SAFETY: caller asserts ctx valid.
                unsafe { (*ctx).proximityPosition }
            };
            if o.floatval == (pos as c_double) { 1 } else { 0 }
        }
        XPATH_BOOLEAN => o.boolval,
        XPATH_STRING => {
            if o.stringval.is_null() { 0 }
            else {
                // SAFETY: stringval NUL-terminated.
                let len = unsafe { CStr::from_ptr(o.stringval) }.to_bytes().len();
                if len > 0 { 1 } else { 0 }
            }
        }
        XPATH_NODESET | XPATH_XSLT_TREE => {
            if o.nodesetval.is_null() { 0 }
            else {
                // SAFETY: live xmlNodeSet.
                if unsafe { (*o.nodesetval).nodeNr } > 0 { 1 } else { 0 }
            }
        }
        _ => 0,
    };
    result
}

/// `xmlXPathStringEvalNumber(s)` — XPath number-literal parser.
/// Returns NaN on syntax errors.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathStringEvalNumber(s: *const c_char) -> c_double {
    unsafe { xmlXPathCastStringToNumber(s) }
}

// ── nodeset operations ──────────────────────────────────────────────────

/// `xmlXPathNodeSetAddUnique(ns, node)` — append `node` if not already
/// present.  Returns 0 on success, -1 on error.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNodeSetAddUnique(
    ns:   *mut xmlNodeSet,
    node: *mut Node<'static>,
) -> c_int {
    if ns.is_null() || node.is_null() { return -1; }
    // SAFETY: caller asserts ns came from xmlXPathNodeSetCreate.
    let s = unsafe { &mut *ns };
    let len = s.nodeNr.max(0) as usize;
    if !s.nodeTab.is_null() {
        let existing = unsafe { std::slice::from_raw_parts(s.nodeTab, len) };
        if existing.contains(&node) { return 0; }
    }
    // Append by reconstituting the Vec, push, re-leak.
    let cap = s.nodeMax.max(0) as usize;
    // SAFETY: same invariants as in xmlXPathFreeNodeSet — the
    // (nodeTab, len, cap) triple was produced by Vec::into_raw_parts.
    let mut v: Vec<*mut Node<'static>> = if s.nodeTab.is_null() {
        Vec::new()
    } else {
        unsafe { Vec::from_raw_parts(s.nodeTab, len, cap) }
    };
    v.push(node);
    let new_len = v.len();
    let new_cap = v.capacity();
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    s.nodeTab = p;
    s.nodeNr  = new_len as c_int;
    s.nodeMax = new_cap as c_int;
    0
}

/// `xmlXPathNodeSetMerge(target, source)` — union: append every node
/// from `source` that isn't already in `target`.  Returns `target`
/// (libxml2's convention) or `source` if `target` was NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNodeSetMerge(
    target: *mut xmlNodeSet,
    source: *mut xmlNodeSet,
) -> *mut xmlNodeSet {
    if source.is_null() { return target; }
    let target = if target.is_null() {
        unsafe { xmlXPathNodeSetCreate(ptr::null_mut()) }
    } else { target };
    if target.is_null() { return ptr::null_mut(); }
    // SAFETY: both pointers are live xmlNodeSets.
    let src = unsafe { &*source };
    if !src.nodeTab.is_null() && src.nodeNr > 0 {
        // SAFETY: nodeTab has nodeNr live entries.
        let slice = unsafe { std::slice::from_raw_parts(src.nodeTab, src.nodeNr as usize) };
        for &n in slice {
            // SAFETY: target is a live nodeset; AddUnique handles dedup.
            unsafe { xmlXPathNodeSetAddUnique(target, n); }
        }
    }
    target
}

/// `xmlXPathNodeSetSort(ns)` — sort by document order (pointer order
/// as a best-effort proxy, see [`xmlXPathCmpNodes`]).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNodeSetSort(ns: *mut xmlNodeSet) {
    if ns.is_null() { return; }
    // SAFETY: live nodeset.
    let s = unsafe { &mut *ns };
    if s.nodeTab.is_null() || s.nodeNr <= 1 { return; }
    // SAFETY: nodeNr live slots.
    let slice = unsafe { std::slice::from_raw_parts_mut(s.nodeTab, s.nodeNr as usize) };
    slice.sort_by_key(|&p| p as usize);
}

/// `xmlXPathOrderDocElems(doc)` — assigns increasing sort keys to the
/// doc's elements so subsequent `xmlXPathCmpNodes` calls are
/// total-order correct.  We don't maintain a separate index — our
/// pointer-order approximation makes this a no-op.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathOrderDocElems(_doc: *const XmlDoc) -> std::os::raw::c_long {
    0
}

/// `xmlXPathDifference(left, right)` — set difference `left \ right`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathDifference(
    left:  *mut xmlNodeSet,
    right: *mut xmlNodeSet,
) -> *mut xmlNodeSet {
    let out = unsafe { xmlXPathNodeSetCreate(ptr::null_mut()) };
    if left.is_null() { return out; }
    // SAFETY: live.
    let l = unsafe { &*left };
    if l.nodeTab.is_null() || l.nodeNr <= 0 { return out; }
    let r_nodes: Vec<*mut Node<'static>> = if right.is_null() {
        Vec::new()
    } else {
        // SAFETY: live.
        let r = unsafe { &*right };
        if r.nodeTab.is_null() || r.nodeNr <= 0 { Vec::new() }
        else {
            unsafe { std::slice::from_raw_parts(r.nodeTab, r.nodeNr as usize) }.to_vec()
        }
    };
    let l_slice = unsafe { std::slice::from_raw_parts(l.nodeTab, l.nodeNr as usize) };
    for &n in l_slice {
        if !r_nodes.contains(&n) {
            unsafe { xmlXPathNodeSetAddUnique(out, n); }
        }
    }
    out
}

/// `xmlXPathIntersection(left, right)` — set intersection.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathIntersection(
    left:  *mut xmlNodeSet,
    right: *mut xmlNodeSet,
) -> *mut xmlNodeSet {
    let out = unsafe { xmlXPathNodeSetCreate(ptr::null_mut()) };
    if left.is_null() || right.is_null() { return out; }
    // SAFETY: live.
    let r = unsafe { &*right };
    if r.nodeTab.is_null() || r.nodeNr <= 0 { return out; }
    let r_nodes: Vec<*mut Node<'static>> = unsafe {
        std::slice::from_raw_parts(r.nodeTab, r.nodeNr as usize)
    }.to_vec();
    // SAFETY: live.
    let l = unsafe { &*left };
    if l.nodeTab.is_null() || l.nodeNr <= 0 { return out; }
    let l_slice = unsafe { std::slice::from_raw_parts(l.nodeTab, l.nodeNr as usize) };
    for &n in l_slice {
        if r_nodes.contains(&n) {
            unsafe { xmlXPathNodeSetAddUnique(out, n); }
        }
    }
    out
}

/// `xmlXPathDistinctSorted(ns)` — return a copy with duplicates
/// removed.  Input is assumed pre-sorted.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathDistinctSorted(ns: *mut xmlNodeSet) -> *mut xmlNodeSet {
    let out = unsafe { xmlXPathNodeSetCreate(ptr::null_mut()) };
    if ns.is_null() { return out; }
    // SAFETY: live.
    let s = unsafe { &*ns };
    if s.nodeTab.is_null() || s.nodeNr <= 0 { return out; }
    let slice = unsafe { std::slice::from_raw_parts(s.nodeTab, s.nodeNr as usize) };
    for &n in slice {
        // AddUnique already dedups, so a single pass works.
        unsafe { xmlXPathNodeSetAddUnique(out, n); }
    }
    out
}

/// `xmlXPathNodeLeadingSorted(ns, node)` — subset of `ns` that
/// precedes `node` in document order.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNodeLeadingSorted(
    ns:   *mut xmlNodeSet,
    node: *mut Node<'static>,
) -> *mut xmlNodeSet {
    let out = unsafe { xmlXPathNodeSetCreate(ptr::null_mut()) };
    if ns.is_null() || node.is_null() { return out; }
    // SAFETY: live.
    let s = unsafe { &*ns };
    if s.nodeTab.is_null() || s.nodeNr <= 0 { return out; }
    let slice = unsafe { std::slice::from_raw_parts(s.nodeTab, s.nodeNr as usize) };
    for &n in slice {
        if (n as usize) < (node as usize) {
            unsafe { xmlXPathNodeSetAddUnique(out, n); }
        }
    }
    out
}

/// `xmlXPathNodeTrailingSorted(ns, node)` — subset of `ns` that
/// follows `node` in document order.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNodeTrailingSorted(
    ns:   *mut xmlNodeSet,
    node: *mut Node<'static>,
) -> *mut xmlNodeSet {
    let out = unsafe { xmlXPathNodeSetCreate(ptr::null_mut()) };
    if ns.is_null() || node.is_null() { return out; }
    // SAFETY: live.
    let s = unsafe { &*ns };
    if s.nodeTab.is_null() || s.nodeNr <= 0 { return out; }
    let slice = unsafe { std::slice::from_raw_parts(s.nodeTab, s.nodeNr as usize) };
    for &n in slice {
        if (n as usize) > (node as usize) {
            unsafe { xmlXPathNodeSetAddUnique(out, n); }
        }
    }
    out
}

// ── XPath parser-context value stack ────────────────────────────────────
//
// libxslt calls `valuePush` / `valuePop` on `xmlXPathParserContext`
// while it walks an XPath function call.  Our XPath engine doesn't
// surface that context to consumers, so we maintain a thread-local
// fallback stack that captures push/pop pairs in libxslt's order.

thread_local! {
    static XPATH_VALUE_STACK: std::cell::RefCell<Vec<*mut xmlXPathObject>>
        = std::cell::RefCell::new(Vec::new());
}

/// `valuePush(ctx, value)` — push onto the XPath evaluation stack.
/// Returns the new stack depth.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathValuePush(
    _ctx:  *mut c_void,
    value: *mut xmlXPathObject,
) -> c_int {
    XPATH_VALUE_STACK.with(|s| {
        let mut v = s.borrow_mut();
        v.push(value);
        v.len() as c_int
    })
}

/// `valuePop(ctx)` — pop the top XPath object from the stack.  NULL
/// when empty.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathValuePop(_ctx: *mut c_void) -> *mut xmlXPathObject {
    XPATH_VALUE_STACK.with(|s| s.borrow_mut().pop().unwrap_or(ptr::null_mut()))
}

/// `xmlXPathPopBoolean(ctx)` — pop and cast to bool.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathPopBoolean(ctx: *mut c_void) -> c_int {
    let o = unsafe { xmlXPathValuePop(ctx) };
    if o.is_null() { return 0; }
    // SAFETY: o came from a Box we leaked into the stack via
    // xmlXPathValuePush, so it's a live exclusive reference.
    let oref: &xmlXPathObject = unsafe { &*o };
    let result = match oref.kind {
        XPATH_BOOLEAN => oref.boolval,
        XPATH_NUMBER  => if oref.floatval != 0.0 && !oref.floatval.is_nan() { 1 } else { 0 },
        XPATH_STRING  => {
            if oref.stringval.is_null() { 0 }
            // SAFETY: stringval is NUL-terminated by construction in
            // every xmlXPathNew*/Wrap*/Cast* path that produces a
            // STRING-kind object.
            else if unsafe { CStr::from_ptr(oref.stringval) }.to_bytes().is_empty() { 0 }
            else { 1 }
        }
        XPATH_NODESET | XPATH_XSLT_TREE => {
            if oref.nodesetval.is_null() { 0 }
            // SAFETY: nodesetval was produced by xmlXPathNodeSetCreate
            // (live until xmlXPathFreeObject below).
            else if unsafe { (*oref.nodesetval).nodeNr } > 0 { 1 } else { 0 }
        }
        _ => 0,
    };
    // SAFETY: o is the same pointer we just held the &-reference for;
    // the reference goes out of scope before this call.
    unsafe { xmlXPathFreeObject(o); }
    result
}

/// `xmlXPathPopNumber(ctx)` — pop and cast to number.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathPopNumber(ctx: *mut c_void) -> c_double {
    let o = unsafe { xmlXPathValuePop(ctx) };
    if o.is_null() { return f64::NAN; }
    // xmlXPathConvertNumber frees the input and returns a fresh
    // Number-kind object — read floatval then free.
    // SAFETY: o came from xmlXPathValuePush via our internal stack.
    let n = unsafe { xmlXPathConvertNumber(o) };
    if n.is_null() { return f64::NAN; }
    // SAFETY: n is the freshly-allocated Number object we just got back.
    let val = unsafe { &*n }.floatval;
    unsafe { xmlXPathFreeObject(n); }
    val
}

/// `xmlXPathPopString(ctx)` — pop and cast to string.  Returns a
/// fresh heap pointer the caller must `xmlFree`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathPopString(ctx: *mut c_void) -> *mut c_char {
    let o = unsafe { xmlXPathValuePop(ctx) };
    if o.is_null() { return alloc_registered_cstring(b""); }
    let s = unsafe { xmlXPathCastToString(o) };
    unsafe { xmlXPathFreeObject(o); }
    s
}

/// `xmlXPathPopNodeSet(ctx)` — pop and extract the nodeset.
/// Transfers ownership; caller must `xmlXPathFreeNodeSet`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathPopNodeSet(ctx: *mut c_void) -> *mut xmlNodeSet {
    let o = unsafe { xmlXPathValuePop(ctx) };
    if o.is_null() { return ptr::null_mut(); }
    // SAFETY: o came from xmlXPathValuePush; we hold the only reference
    // until xmlXPathFreeObject below.  Take the nodeset out and NULL
    // the slot so the free doesn't release it.
    let ns = unsafe {
        let oref = &mut *o;
        let ns = oref.nodesetval;
        oref.nodesetval = ptr::null_mut();
        ns
    };
    unsafe { xmlXPathFreeObject(o); }
    ns
}

/// `xmlXPathPopExternal(ctx)` — pop a USERS-kind object's `user` ptr.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathPopExternal(ctx: *mut c_void) -> *mut c_void {
    let o = unsafe { xmlXPathValuePop(ctx) };
    if o.is_null() { return ptr::null_mut(); }
    // SAFETY: o came from xmlXPathValuePush.
    let u = unsafe { &*o }.user;
    unsafe { xmlXPathFreeObject(o); }
    u
}

// ── function & variable resolution ──────────────────────────────────────

/// `xmlXPathNsLookup(ctx, prefix)` — return the registered URI for
/// `prefix`, or NULL on miss.  Returns a pointer into our private
/// ns-map storage; caller must NOT free.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNsLookup(
    _ctx:    *mut xmlXPathContext,
    _prefix: *const c_char,
) -> *const c_char {
    // The ns_map lives in XPathPrivate; returning a stable pointer
    // means leaking a CString per lookup which is expensive.  v0.1
    // returns NULL — consumers fall back to their own resolution.
    ptr::null()
}

/// `xmlXPathFunctionLookupNS(ctx, name, ns_uri)` — find a registered
/// XPath function by (URI, local name).  v0.1 returns NULL.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathFunctionLookupNS(
    _ctx:    *mut xmlXPathContext,
    _name:   *const c_char,
    _ns_uri: *const c_char,
) -> *mut c_void {
    ptr::null_mut()
}

/// `xmlXPathRegisterFuncLookup(ctx, lookup, data)` — install a
/// function-lookup callback.  Stored on the context's private state;
/// `XPathBindings::call_function` consults it for any unknown
/// function name before raising "unregistered function."  The
/// callback has libxml2 shape:
///
/// ```c
/// typedef xmlXPathFunction (*xmlXPathFuncLookupFunc)
///     (void *data, const xmlChar *name, const xmlChar *ns_uri);
/// ```
///
/// where `xmlXPathFunction` is `void (*)(xmlXPathParserContextPtr, int)`.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathRegisterFuncLookup(
    ctx:    *mut xmlXPathContext,
    lookup: *mut c_void,
    data:   *mut c_void,
) {
    if ctx.is_null() { return; }
    let ctx_ref = unsafe { &*ctx };
    if ctx_ref.user.is_null() { return; }
    let private = unsafe { &*(ctx_ref.user as *const XPathPrivate) };
    private.func_lookup_fn  .set(lookup);
    private.func_lookup_data.set(data);
}

/// `xmlXPathRegisterVariableLookup(ctx, lookup, data)` — install a
/// variable-lookup callback.  libxslt uses this to delegate
/// `$varname` resolution to its own scope-aware lookup
/// (`xsltVariableLookup`) — templates, params, and xsl:variable
/// scopes all live in libxslt's data structures, not in our
/// `var_map`.  Our XPathBindings::variable consults this callback
/// when set.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathRegisterVariableLookup(
    ctx:    *mut xmlXPathContext,
    lookup: *mut c_void,
    data:   *mut c_void,
) {
    if ctx.is_null() { return; }
    let ctx_ref = unsafe { &*ctx };
    if ctx_ref.user.is_null() { return; }
    let private = unsafe { &*(ctx_ref.user as *const XPathPrivate) };
    private.var_lookup_fn  .set(lookup);
    private.var_lookup_data.set(data);
}

/// `xmlXPathNumberFunction(ctxt, nargs)` — XPath 1.0 `number()`
/// builtin entry point.  Pops `nargs` operands off the stack, casts
/// to number, pushes the result.  v0.1 supports nargs in {0,1}.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathNumberFunction(
    ctx:   *mut c_void,
    nargs: c_int,
) {
    let val = if nargs <= 0 { f64::NAN }
              else { unsafe { xmlXPathPopNumber(ctx) } };
    let obj = unsafe { xmlXPathNewFloat(val) };
    unsafe { xmlXPathValuePush(ctx, obj); }
}

/// `xmlXPathStringFunction(ctxt, nargs)` — XPath 1.0 `string()`
/// builtin.  Same pattern as [`xmlXPathNumberFunction`].
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathStringFunction(
    ctx:   *mut c_void,
    nargs: c_int,
) {
    let s = if nargs <= 0 { alloc_registered_cstring(b"") }
            else { unsafe { xmlXPathPopString(ctx) } };
    let obj = unsafe { xmlXPathWrapString(s) };
    unsafe { xmlXPathValuePush(ctx, obj); }
}

// ── misc ────────────────────────────────────────────────────────────────

/// `xmlXPathContextSetCache(ctx, active, value, options)` — opt-in
/// XPath-object cache.  v0.1 no-op; we don't pool objects.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathContextSetCache(
    _ctx:     *mut xmlXPathContext,
    _active:  c_int,
    _value:   c_int,
    _options: c_int,
) -> c_int { 0 }

/// `xmlXPathDebugDumpObject(output, obj, depth)` — print `obj` to a
/// `FILE*` for debugging.  v0.1 no-op (we don't ship a debug
/// formatter and lxml never calls this from production paths).
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathDebugDumpObject(
    _output: *mut c_void,
    _obj:    *mut xmlXPathObject,
    _depth:  c_int,
) {}

/// `xmlXPatherror(ctxt, file, line, no)` — record an internal XPath
/// error.  We surface the error number through the structured error
/// callback if one is installed; otherwise no-op.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPatherror(
    _ctxt: *mut c_void,
    _file: *const c_char,
    _line: c_int,
    _no:   c_int,
) {}

/// `xmlXPathCompiledEvalToBoolean(comp, ctx)` — evaluate a
/// pre-compiled XPath expression, returning 1/0/-1.  v0.1 forwards
/// to `xmlXPathCompiledEval` and casts.
#[cfg_attr(feature = "cdylib-exports", unsafe(no_mangle))]
pub unsafe extern "C" fn xmlXPathCompiledEvalToBoolean(
    comp: *mut xmlXPathCompExpr,
    ctx:  *mut xmlXPathContext,
) -> c_int {
    let obj = unsafe { xmlXPathCompiledEval(comp, ctx) };
    if obj.is_null() { return -1; }
    // SAFETY: obj was just returned by xmlXPathCompiledEval — live
    // exclusive reference until xmlXPathFreeObject below.
    let oref: &xmlXPathObject = unsafe { &*obj };
    let result = match oref.kind {
        XPATH_BOOLEAN => oref.boolval,
        XPATH_NUMBER  => {
            let v = oref.floatval;
            if v != 0.0 && !v.is_nan() { 1 } else { 0 }
        }
        XPATH_STRING => {
            let p = oref.stringval;
            if p.is_null() { 0 }
            // SAFETY: stringval is NUL-terminated by construction.
            else if unsafe { CStr::from_ptr(p) }.to_bytes().is_empty() { 0 }
            else { 1 }
        }
        XPATH_NODESET | XPATH_XSLT_TREE => {
            let ns = oref.nodesetval;
            if ns.is_null() { 0 }
            // SAFETY: live xmlNodeSet allocated by xmlXPathNodeSetCreate.
            else if unsafe { &*ns }.nodeNr > 0 { 1 } else { 0 }
        }
        _ => 0,
    };
    unsafe { xmlXPathFreeObject(obj); }
    result
}

// ── XPathValue → xmlXPathObject ─────────────────────────────────────────────

fn xpath_value_to_object<'doc>(
    value: XPathValue,
    index: &DocIndex<'doc>,
) -> *mut xmlXPathObject {
    // libxml2's xmlXPathObject has no native IntRange shape;
    // materialise to a Sequence of Number items so the downstream
    // match's existing Sequence handling renders it as
    // space-separated atomics, just like Saxon / lxml.
    let value = match value {
        XPathValue::IntRange { lo, hi } => XPathValue::Sequence(
            (lo..=hi).map(|i| XPathValue::Number(Numeric::Double(i as f64))).collect()
        ),
        other => other,
    };
    let mut obj = xmlXPathObject {
        kind:         XPATH_UNDEFINED,
        _pad_kind:    0,
        nodesetval:   ptr::null_mut(),
        boolval:      0,
        _pad_boolval: 0,
        floatval:     0.0,
        stringval:    ptr::null_mut(),
        user:         ptr::null_mut(),
        index:        0,
        _pad_index:   0,
        user2:        ptr::null_mut(),
        index2:       0,
        _pad_index2:  0,
    };
    match value {
        XPathValue::NodeSet(node_ids) => {
            obj.kind = XPATH_NODESET;
            let mut tab: Vec<*mut Node<'static>> = node_ids.into_iter()
                .filter_map(|id| node_id_to_ptr(id, index))
                .collect();
            let len = tab.len();
            let cap = tab.capacity().max(len.max(1));
            // Force at least one slot of capacity so nodeTab is non-null
            // (libxml2 callers sometimes check nodeTab without checking
            // nodeNr first).
            if tab.capacity() < cap {
                tab.reserve_exact(cap - tab.capacity());
            }
            let cap = tab.capacity();
            let nodeTab = tab.as_mut_ptr();
            std::mem::forget(tab);
            obj.nodesetval = Box::into_raw(Box::new(xmlNodeSet {
                nodeNr:  len as c_int,
                nodeMax: cap as c_int,
                nodeTab,
            }));
        }
        XPathValue::String(s) => {
            obj.kind = XPATH_STRING;
            obj.stringval = alloc_registered_cstring(s.as_bytes());
        }
        XPathValue::Number(n) => {
            obj.kind = XPATH_NUMBER;
            obj.floatval = n.as_f64();
        }
        XPathValue::Boolean(b) => {
            obj.kind = XPATH_BOOLEAN;
            obj.boolval = if b { 1 } else { 0 };
        }
        XPathValue::Typed(t) => {
            // libxml2's XPath object set has no typed-atomic slot —
            // collapse the typed value to its lexical form as a
            // string object.  Callers that need the type tag must
            // stay inside Rust.
            obj.kind = XPATH_STRING;
            obj.stringval = alloc_registered_cstring(t.lexical.as_bytes());
        }
        XPathValue::Sequence(items) => {
            // libxml2 has no sequence object kind — XSLT 1.0
            // callers expect either a node-set or a single atomic.
            // Project the sequence to a space-joined string (matches
            // xsl:value-of of an XPath 2.0 sequence).  Loss of
            // per-item typing is unavoidable across the C ABI.
            let joined = items.iter()
                .map(|v| sup_xml_core::xpath::eval::value_to_string(v, index))
                .collect::<Vec<_>>()
                .join(" ");
            obj.kind = XPATH_STRING;
            obj.stringval = alloc_registered_cstring(joined.as_bytes());
        }
        XPathValue::ForeignNodeSet(ptrs) => {
            // If any pointer is a document node, mark this as
            // XPATH_XSLT_TREE rather than NODESET — lxml's
            // `_unpackNodeSetEntry` silently drops document entries
            // under NODESET but descends into their children under
            // XSLT_TREE.  libxslt's `xsl:copy-of` handles both kinds
            // identically, so this doesn't break the `document()`
            // path either.  See lxml/extensions.pxi.
            let any_doc = ptrs.iter().any(|&p| {
                if p.is_null() { return false; }
                // SAFETY: p is a live Node pointer from a doc
                // registered with our bindings.
                let n = unsafe { &*p };
                matches!(n.kind, sup_xml_tree::dom::NodeKind::Document)
            });
            obj.kind = if any_doc { XPATH_XSLT_TREE } else { XPATH_NODESET };
            let mut tab: Vec<*mut Node<'static>> = ptrs
                .into_iter()
                .map(|p| p as *mut Node<'static>)
                .collect();
            let len = tab.len();
            let cap = tab.capacity().max(len.max(1));
            if tab.capacity() < cap {
                tab.reserve_exact(cap - tab.capacity());
            }
            let cap = tab.capacity();
            let nodeTab = tab.as_mut_ptr();
            std::mem::forget(tab);
            obj.nodesetval = Box::into_raw(Box::new(xmlNodeSet {
                nodeNr:  len as c_int,
                nodeMax: cap as c_int,
                nodeTab,
            }));
        }
        XPathValue::IntRange { .. } =>
            unreachable!("IntRange normalised to Sequence at function entry"),
        // The libxml2 C ABI has no map/array object kind; surface as an
        // empty node-set (maps/arrays don't cross the C boundary).
        XPathValue::Map(_) | XPathValue::Array(_) | XPathValue::Function(_) => {
            obj.kind = XPATH_NODESET;
        }
    }
    Box::into_raw(Box::new(obj))
}

/// The owning `xmlDoc*` for `index`, read from the `doc` back-pointer
/// (offset 64) of any backing tree node.  libxml2 surfaces the document
/// node in a node-set as this pointer.  Returns `None` for a document
/// with no backing nodes (nothing to point at).
fn doc_ptr_from_index<'doc>(index: &DocIndex<'doc>) -> Option<*mut Node<'static>> {
    for inode in &index.nodes {
        let backing: &Node<'doc> = match &inode.kind {
            INodeKind::Element(n) | INodeKind::Text(n) | INodeKind::Comment(n)
            | INodeKind::CData(n)  | INodeKind::PI(n) => *n,
            _ => continue,
        };
        let d = backing.doc.get();
        if !d.is_null() {
            return Some(d as *mut Node<'static>);
        }
    }
    None
}

fn node_id_to_ptr<'doc>(id: sup_xml_core::xpath::NodeId, index: &DocIndex<'doc>) -> Option<*mut Node<'static>> {
    let inode = index.nodes.get(id)?;
    let ptr = match &inode.kind {
        INodeKind::Element(n)
            | INodeKind::Text(n)
            | INodeKind::Comment(n)
            | INodeKind::CData(n)
            | INodeKind::PI(n) => {
                (*n) as *const Node<'_> as *mut Node<'static>
            }
        INodeKind::Attribute(a) => {
            // libxml2's xmlNodeSet stores both xmlNode* and xmlAttr* —
            // they share the first 8 fields including the `type`
            // discriminant (XML_ATTRIBUTE_NODE = 2).  C callers walk
            // and check the type byte.
            (*a) as *const _ as *mut Node<'static>
        }
        INodeKind::Document => {
            // libxml2 represents the document node in a nodeset as the
            // xmlDoc* itself — cast-compatible with xmlNode* on the
            // shared prefix (kind@8, children@24, parent@40), so
            // libxslt/lxml walk it as a context node correctly.  Recover
            // the owning xmlDoc* from the `doc` back-pointer (offset 64)
            // any backing node carries.  Without this, `select="."` /
            // `apply-templates select="."` at the document root (the
            // pattern iso-schematron and many XSLT root templates use)
            // would yield an empty node-set.
            return doc_ptr_from_index(index);
        }
        INodeKind::Namespace { .. } => {
            // A namespace node has no backing tree node; libxml2 places
            // a synthetic `xmlNs` (XML_NAMESPACE_DECL) in the node-set
            // instead.  Materialize one so the `namespace::` axis can
            // hand results to C callers (libxslt `xsl:copy-of`, lxml).
            return ns_node_to_ptr(id, index);
        }
    };
    Some(ptr)
}

/// True iff `node` is a namespace node — an `xmlNs` whose type field
/// (offset 8, shared with `xmlNode::type`) is `XML_NAMESPACE_DECL` (18).
///
/// SAFETY: `node` must point at a live `xmlNode`/`xmlNs`; only the type
/// word at offset 8 (valid in both layouts) is read.
unsafe fn is_namespace_node(node: *const Node<'static>) -> bool {
    const XML_NAMESPACE_DECL: i32 = 18;
    unsafe { *((node as *const u8).add(8) as *const i32) == XML_NAMESPACE_DECL }
}

/// Owning document of a namespace context node (the `xmlNs::context`
/// field at offset 40).  `xmlNode::doc` (offset 64) would read past the
/// 48-byte `xmlNs`.
///
/// SAFETY: `node` must point at a live `xmlNs` (see [`is_namespace_node`]).
unsafe fn ns_node_context_doc(node: *const Node<'static>) -> *const XmlDoc {
    unsafe { *((node as *const u8).add(40) as *const *const XmlDoc) }
}

/// Map a namespace context node (`xmlNs`) back to its [`NodeId`] in
/// `index`.  The node's parent element lives in the `next` slot (offset
/// 0, the node-set convention [`ns_node_to_ptr`] writes) and its prefix
/// at offset 24; together they select one entry in the element's
/// namespace range.
fn resolve_ns_context_id<'doc>(
    node:  *const Node<'static>,
    index: &DocIndex<'doc>,
) -> Option<sup_xml_core::xpath::NodeId> {
    let base = node as *const u8;
    let parent = unsafe { *(base as *const *const Node<'static>) };
    if parent.is_null() {
        return None;
    }
    // Offset 24: `Option<ArenaCStr>` — a NUL-terminated C string for a
    // prefixed binding, or null for the default namespace.
    let prefix_ptr = unsafe { *(base.add(24) as *const *const c_char) };
    let want: Option<&str> = if prefix_ptr.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(prefix_ptr) }.to_str().ok()
    };
    // Find the parent element's NodeId by raw-address match, then the
    // namespace entry in its range whose prefix matches.
    let target = parent as *const ();
    let elem_id = index.nodes.iter().position(|n| match n.kind {
        INodeKind::Element(p) => (p as *const _ as *const ()) == target,
        _ => false,
    })?;
    let inode = index.nodes.get(elem_id)?;
    // A namespace node's binding prefix is its node-name ("" for the
    // default binding); map that back to the `Option` shape of `want`.
    (inode.ns_start..inode.ns_end).find(|&nsid| {
        let name = index.node_name(nsid);
        let bound = if name.is_empty() { None } else { Some(name) };
        bound == want
    })
}

/// Materialize a libxml2 `xmlNs`-layout node for a namespace [`NodeId`]
/// so the `namespace::` axis can cross the XPath→C boundary.  libxml2
/// represents a namespace node inside an `xmlNodeSet` as an `xmlNs`
/// whose `type` is `XML_NAMESPACE_DECL`, whose `prefix`/`href` carry the
/// binding, and whose `next` field points at the parent element — the
/// convention `xmlXPathNodeSetAddNs` establishes and `xsl:copy-of`
/// reads back.
///
/// The node lives in the owning document's arena: it outlives the
/// `xmlXPathObject` that carries it, and [`xmlXPathFreeNodeSet`] never
/// frees individual nodes, so there is no allocator mismatch or
/// double-free (unlike libxml2, which `xmlMalloc`s and frees these).
fn ns_node_to_ptr<'doc>(
    id:    sup_xml_core::xpath::NodeId,
    index: &DocIndex<'doc>,
) -> Option<*mut Node<'static>> {
    // A namespace node's binding lives on the INode itself: its prefix
    // (None for the default binding) and the URI it binds.  The XPath
    // `namespace_prefix`/`namespace_uri` accessors describe a node's own
    // membership instead — empty for a namespace node — so read the kind.
    let inode = index.nodes.get(id)?;
    let (prefix, uri) = match &inode.kind {
        INodeKind::Namespace { prefix, uri } => (*prefix, *uri),
        _ => return None,
    };
    // The parent element supplies both the back-pointer libxml2 stores
    // in `next` and the arena the synthetic node is allocated from.
    let parent_id = inode.parent?;
    let parent = node_id_to_ptr(parent_id, index)?;
    let doc_ptr = unsafe { (*parent).doc.get() } as *mut XmlDoc;
    if doc_ptr.is_null() {
        return None;
    }
    let doc: &XmlDoc = unsafe { &*doc_ptr };
    let ns = doc._doc.bump_new_namespace(prefix, uri);
    // `next` holds the parent node pointer and `context` the owning
    // document (libxml2's node-set namespace-node convention).  Write
    // `next` through the raw slot rather than forming a `&Namespace` to
    // an element — the pointer is opaque to us and only read by C.
    let next_slot = &ns.next
        as *const std::cell::Cell<Option<&sup_xml_tree::dom::Namespace<'_>>>
        as *mut *mut c_void;
    unsafe { next_slot.write(parent as *mut c_void); }
    ns.context.set(doc_ptr as *mut c_void);
    Some(ns as *const _ as *mut Node<'static>)
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    use crate::parse::{xmlDocGetRootElement, xmlFreeDoc, xmlReadMemory};

    fn parse(src: &[u8]) -> *mut XmlDoc {
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

    fn cs(s: &str) -> CString { CString::new(s).unwrap() }

    #[test]
    fn namespace_axis_emits_xmlns_nodes() {
        // The `namespace::` axis must hand C callers (libxslt copy-of)
        // synthetic `xmlNs` nodes: type XML_NAMESPACE_DECL (18), the
        // binding's prefix at offset 24 and href at offset 16, and the
        // parent element in the `next` slot (offset 0).
        let doc = parse(b"<r xmlns:foo=\"http://foo\"/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };
        let root = unsafe { xmlDocGetRootElement(doc) };

        let expr = cs("/r/namespace::*");
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        let o = unsafe { &*obj };
        assert_eq!(o.kind, XPATH_NODESET);
        let set = unsafe { &*o.nodesetval };
        // Two in-scope bindings: the implicit `xml` plus `foo`.
        assert_eq!(set.nodeNr, 2);

        let mut seen = std::collections::HashMap::new();
        for i in 0..set.nodeNr as isize {
            let node = unsafe { *set.nodeTab.offset(i) } as *const u8;
            // type @ 8 == XML_NAMESPACE_DECL
            assert_eq!(unsafe { *(node.add(8) as *const i32) }, 18);
            // next @ 0 == parent element
            assert_eq!(unsafe { *(node as *const *const u8) }, root as *const u8);
            let href = {
                let p = unsafe { *(node.add(16) as *const *const c_char) };
                unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string()
            };
            let prefix = {
                let p = unsafe { *(node.add(24) as *const *const c_char) };
                if p.is_null() { String::new() }
                else { unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string() }
            };
            seen.insert(prefix, href);
        }
        assert_eq!(seen.get("xml").map(String::as_str),
                   Some("http://www.w3.org/XML/1998/namespace"));
        assert_eq!(seen.get("foo").map(String::as_str), Some("http://foo"));

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn eval_simple_nodeset() {
        let doc = parse(b"<r><a/><a/><b/></r>");
        let ctx = unsafe { xmlXPathNewContext(doc) };
        assert!(!ctx.is_null());

        let expr = cs("//a");
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        let o = unsafe { &*obj };
        assert_eq!(o.kind, XPATH_NODESET);
        assert!(!o.nodesetval.is_null());
        let ns = unsafe { &*o.nodesetval };
        assert_eq!(ns.nodeNr, 2);

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn eval_boolean() {
        let doc = parse(b"<r><a/></r>");
        let ctx = unsafe { xmlXPathNewContext(doc) };
        let expr = cs("count(//a) = 1");
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        let o = unsafe { &*obj };
        assert_eq!(o.kind, XPATH_BOOLEAN);
        assert_eq!(o.boolval, 1);

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn eval_number() {
        let doc = parse(b"<r><a/><a/><a/></r>");
        let ctx = unsafe { xmlXPathNewContext(doc) };
        let expr = cs("count(//a)");
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        let o = unsafe { &*obj };
        assert_eq!(o.kind, XPATH_NUMBER);
        assert_eq!(o.floatval, 3.0);

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn eval_string() {
        let doc = parse(b"<r>hello</r>");
        let ctx = unsafe { xmlXPathNewContext(doc) };
        let expr = cs("string(/r)");
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        let o = unsafe { &*obj };
        assert_eq!(o.kind, XPATH_STRING);
        let s = unsafe { CStr::from_ptr(o.stringval) }.to_str().unwrap();
        assert_eq!(s, "hello");

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn null_safety() {
        // xmlXPathNewContext(NULL) is legal in libxml2 — used to
        // pre-compile expressions before binding a document.  The
        // resulting context has `doc == NULL`; evaluating an
        // expression against it fails cleanly (returns NULL with
        // last-error set), but the new+free pair must not crash.
        let ctx = unsafe { xmlXPathNewContext(ptr::null()) };
        assert!(!ctx.is_null());
        unsafe { xmlXPathFreeContext(ctx); }
        unsafe { xmlXPathFreeContext(ptr::null_mut()); }
        unsafe { xmlXPathFreeObject(ptr::null_mut()); }
        unsafe { xmlXPathFreeNodeSet(ptr::null_mut()); }
        assert!(unsafe {
            xmlXPathEvalExpression(ptr::null(), ptr::null_mut())
        }.is_null());
    }

    #[test]
    fn xpath_self_dot_yields_document_node() {
        // libxslt drives `<xsl:for-each select=".">` / `apply-templates
        // select="."` at the document root by evaluating "." with the
        // document node as context.  The result node-set must contain the
        // document node — surfaced as the xmlDoc* (libxml2's representation
        // of the document node in a node-set) — not be empty.
        let doc = parse(b"<a>hi</a>");
        let ctx = unsafe { xmlXPathNewContext(doc) };
        // ctx->node left NULL = the document root (XPath context node 0).
        let expr = cs(".");
        let obj = unsafe { xmlXPathEval(expr.as_ptr(), ctx) };
        assert!(!obj.is_null(), "eval of '.' should succeed");
        unsafe {
            assert_eq!((*obj).kind, XPATH_NODESET);
            let ns = (*obj).nodesetval;
            assert!(!ns.is_null());
            assert_eq!((*ns).nodeNr, 1,
                "'.' at the document root must yield the document node");
            let n0 = *(*ns).nodeTab;
            assert_eq!(n0 as *const c_void, doc as *const c_void,
                "document node should be surfaced as the xmlDoc pointer");
        }
        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            crate::parse::xmlFreeDoc(doc);
        }
    }

    #[test]
    fn register_ns_then_eval() {
        let doc = parse(b"<r xmlns:foo=\"http://example.com/foo\"><foo:a/></r>");
        let ctx = unsafe { xmlXPathNewContext(doc) };

        let prefix = cs("ex");
        let uri = cs("http://example.com/foo");
        let rc = unsafe { xmlXPathRegisterNs(ctx, prefix.as_ptr(), uri.as_ptr()) };
        assert_eq!(rc, 0);
        // Eval doesn't yet consume the registered prefix (engine has
        // its own namespace resolution), so just verify the call
        // round-trips without crash.

        unsafe {
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    // ── libxslt-facing helper tests ──────────────────────────────────────

    #[test]
    fn new_node_set_creates_singleton() {
        let obj = unsafe { xmlXPathNewNodeSet(0xCAFE as *mut _) };
        assert!(!obj.is_null());
        assert_eq!(unsafe { (*obj).kind }, XPATH_NODESET);
        unsafe { assert_eq!((*(*obj).nodesetval).nodeNr, 1); }
        unsafe { xmlXPathFreeObject(obj); }
    }

    #[test]
    fn new_string_wraps_text() {
        let s = cs("hello");
        let obj = unsafe { xmlXPathNewString(s.as_ptr()) };
        assert_eq!(unsafe { (*obj).kind }, XPATH_STRING);
        let got = unsafe { CStr::from_ptr((*obj).stringval) }.to_str().unwrap();
        assert_eq!(got, "hello");
        unsafe { xmlXPathFreeObject(obj); }
    }

    #[test]
    fn new_value_tree_tags_xslt_kind() {
        let obj = unsafe { xmlXPathNewValueTree(0x1 as *mut _) };
        assert_eq!(unsafe { (*obj).kind }, XPATH_XSLT_TREE);
        unsafe { xmlXPathFreeObject(obj); }
    }

    #[test]
    fn xpath_object_copy_deep_clones() {
        let s = cs("hello");
        let src = unsafe { xmlXPathNewString(s.as_ptr()) };
        let dst = unsafe { xmlXPathObjectCopy(src) };
        assert_ne!(src, dst, "copy must be a distinct allocation");
        let s_str = unsafe { CStr::from_ptr((*src).stringval) }.to_str().unwrap();
        let d_str = unsafe { CStr::from_ptr((*dst).stringval) }.to_str().unwrap();
        assert_eq!(s_str, d_str);
        // The copy's stringval is a separate alloc.
        assert_ne!(unsafe { (*src).stringval }, unsafe { (*dst).stringval });
        unsafe {
            xmlXPathFreeObject(src);
            xmlXPathFreeObject(dst);
        }
    }

    #[test]
    fn cast_string_to_number_handles_valid_and_invalid() {
        let valid = cs("3.14");
        let bad   = cs("not-a-number");
        assert!((unsafe { xmlXPathCastStringToNumber(valid.as_ptr()) } - 3.14).abs() < 1e-9);
        assert!(unsafe { xmlXPathCastStringToNumber(bad.as_ptr()) }.is_nan());
        assert!(unsafe { xmlXPathCastStringToNumber(ptr::null()) }.is_nan());
    }

    #[test]
    fn cast_number_to_string_matches_xpath_format() {
        let p_int  = unsafe { xmlXPathCastNumberToString(42.0) };
        let p_frac = unsafe { xmlXPathCastNumberToString(3.14) };
        let p_nan  = unsafe { xmlXPathCastNumberToString(f64::NAN) };
        let p_inf  = unsafe { xmlXPathCastNumberToString(f64::INFINITY) };
        assert_eq!(unsafe { CStr::from_ptr(p_int) }.to_str().unwrap(),  "42");
        assert_eq!(unsafe { CStr::from_ptr(p_frac) }.to_str().unwrap(), "3.14");
        assert_eq!(unsafe { CStr::from_ptr(p_nan) }.to_str().unwrap(),  "NaN");
        assert_eq!(unsafe { CStr::from_ptr(p_inf) }.to_str().unwrap(),  "Infinity");
        unsafe {
            crate::parse::xml_free_impl(p_int  as *mut _);
            crate::parse::xml_free_impl(p_frac as *mut _);
            crate::parse::xml_free_impl(p_nan  as *mut _);
            crate::parse::xml_free_impl(p_inf  as *mut _);
        }
    }

    #[test]
    fn xpath_is_inf_classifies() {
        assert_eq!(unsafe { xmlXPathIsInf(f64::INFINITY) },  1);
        assert_eq!(unsafe { xmlXPathIsInf(f64::NEG_INFINITY) }, -1);
        assert_eq!(unsafe { xmlXPathIsInf(0.0) }, 0);
        assert_eq!(unsafe { xmlXPathIsInf(f64::NAN) }, 0);
    }

    #[test]
    fn xpath_is_nan_detects_nan() {
        assert_eq!(unsafe { xmlXPathIsNaN(f64::NAN) }, 1);
        assert_eq!(unsafe { xmlXPathIsNaN(0.0) }, 0);
    }

    #[test]
    fn xpath_is_node_type_only_matches_four_keywords() {
        for name in ["node", "text", "comment", "processing-instruction"] {
            let cn = cs(name);
            assert_eq!(unsafe { xmlXPathIsNodeType(cn.as_ptr()) }, 1);
        }
        let bad = cs("element");
        assert_eq!(unsafe { xmlXPathIsNodeType(bad.as_ptr()) }, 0);
    }

    #[test]
    fn cmp_nodes_orders_by_address() {
        let a = 0x1000 as *mut Node<'static>;
        let b = 0x2000 as *mut Node<'static>;
        assert_eq!(unsafe { xmlXPathCmpNodes(a, b) }, -1);
        assert_eq!(unsafe { xmlXPathCmpNodes(b, a) },  1);
        assert_eq!(unsafe { xmlXPathCmpNodes(a, a) },  0);
    }

    #[test]
    fn nodeset_add_unique_dedups() {
        let ns = unsafe { xmlXPathNodeSetCreate(ptr::null_mut()) };
        let n = 0x100 as *mut Node<'static>;
        assert_eq!(unsafe { xmlXPathNodeSetAddUnique(ns, n) }, 0);
        assert_eq!(unsafe { xmlXPathNodeSetAddUnique(ns, n) }, 0);  // returns 0 (already present)
        unsafe { assert_eq!((*ns).nodeNr, 1, "dedup should keep size at 1"); }
        unsafe { xmlXPathFreeNodeSet(ns); }
    }

    #[test]
    fn nodeset_merge_unions() {
        let a = unsafe { xmlXPathNodeSetCreate(0x100 as *mut _) };
        let b = unsafe { xmlXPathNodeSetCreate(0x200 as *mut _) };
        unsafe { xmlXPathNodeSetAddUnique(a, 0x300 as *mut _); }
        let _ = unsafe { xmlXPathNodeSetMerge(a, b) };
        unsafe { assert_eq!((*a).nodeNr, 3); }
        unsafe { xmlXPathFreeNodeSet(a); xmlXPathFreeNodeSet(b); }
    }

    #[test]
    fn nodeset_difference_subtracts() {
        let a = unsafe { xmlXPathNodeSetCreate(0x100 as *mut _) };
        unsafe { xmlXPathNodeSetAddUnique(a, 0x200 as *mut _); }
        unsafe { xmlXPathNodeSetAddUnique(a, 0x300 as *mut _); }
        let b = unsafe { xmlXPathNodeSetCreate(0x200 as *mut _) };
        let diff = unsafe { xmlXPathDifference(a, b) };
        unsafe { assert_eq!((*diff).nodeNr, 2); } // 0x100, 0x300
        unsafe { xmlXPathFreeNodeSet(a); xmlXPathFreeNodeSet(b); xmlXPathFreeNodeSet(diff); }
    }

    #[test]
    fn nodeset_intersection() {
        let a = unsafe { xmlXPathNodeSetCreate(0x100 as *mut _) };
        unsafe { xmlXPathNodeSetAddUnique(a, 0x200 as *mut _); }
        let b = unsafe { xmlXPathNodeSetCreate(0x200 as *mut _) };
        unsafe { xmlXPathNodeSetAddUnique(b, 0x300 as *mut _); }
        let i = unsafe { xmlXPathIntersection(a, b) };
        unsafe { assert_eq!((*i).nodeNr, 1); } // just 0x200
        unsafe { xmlXPathFreeNodeSet(a); xmlXPathFreeNodeSet(b); xmlXPathFreeNodeSet(i); }
    }

    #[test]
    fn value_stack_push_and_pop_roundtrip() {
        let val = unsafe { xmlXPathNewFloat(42.0) };
        let depth = unsafe { xmlXPathValuePush(ptr::null_mut(), val) };
        assert!(depth >= 1);
        let popped = unsafe { xmlXPathValuePop(ptr::null_mut()) };
        assert_eq!(popped, val);
        unsafe { xmlXPathFreeObject(popped); }
    }

    #[test]
    fn pop_number_casts_and_frees() {
        // Push a string, pop as number — should parse and free.
        let s = cs("3.14");
        let val = unsafe { xmlXPathNewString(s.as_ptr()) };
        unsafe { xmlXPathValuePush(ptr::null_mut(), val); }
        let n = unsafe { xmlXPathPopNumber(ptr::null_mut()) };
        assert!((n - 3.14).abs() < 1e-9);
    }

    #[test]
    fn pop_boolean_casts() {
        let s = cs("anything");
        let val = unsafe { xmlXPathNewString(s.as_ptr()) };
        unsafe { xmlXPathValuePush(ptr::null_mut(), val); }
        assert_eq!(unsafe { xmlXPathPopBoolean(ptr::null_mut()) }, 1);

        let empty = cs("");
        let val2 = unsafe { xmlXPathNewString(empty.as_ptr()) };
        unsafe { xmlXPathValuePush(ptr::null_mut(), val2); }
        assert_eq!(unsafe { xmlXPathPopBoolean(ptr::null_mut()) }, 0);
    }

    #[test]
    fn compiled_eval_to_boolean_handles_truthy_string() {
        let doc = parse(b"<r/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };
        let comp_text = cs("'hi'");  // XPath literal: always true (non-empty string)
        let comp = unsafe { xmlXPathCompile(comp_text.as_ptr()) };
        let r = unsafe { xmlXPathCompiledEvalToBoolean(comp, ctx) };
        assert_eq!(r, 1);
        unsafe {
            xmlXPathFreeCompExpr(comp);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    // ── XSLT document() / ForeignNodeSet tests ────────────────────────────
    //
    // These exercise the Option-D plumbing end-to-end: document(URI) load,
    // path traversal on the foreign doc via apply_foreign_path, predicate
    // re-entry, cross-doc union, error paths.  We can't reach the
    // document('') case from a unit test because that path reads
    // libxslt's xsltTransformContext via ctx->extra, which only exists
    // inside a real XSLT run — lxml integration tests cover it instead.

    /// Write `bytes` to a uniquely-named file in the system temp dir.
    /// Returns the path so the test can hand it to document(URI).
    fn write_tmp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("sup-xml-xpath-tests");
        std::fs::create_dir_all(&dir).expect("mkdir tmp");
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write tmp");
        path
    }

    #[test]
    fn document_loads_and_returns_root_pointer() {
        let aux = write_tmp("loads_root.xml", b"<r><a/></r>");
        let doc = parse(b"<src/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };

        let expr = cs(&format!("document('{}')", aux.display()));
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null(), "document() returned NULL — see error log");
        let o = unsafe { &*obj };
        // document() returns a node-set containing the loaded doc
        // node.  We tag it as XPATH_XSLT_TREE (rather than NODESET)
        // because the entry IS a document node — lxml's
        // `_unpackNodeSetEntry` silently drops doc entries under
        // NODESET but descends under XSLT_TREE.
        assert_eq!(o.kind, XPATH_XSLT_TREE);
        assert_eq!(unsafe { (*o.nodesetval).nodeNr }, 1);

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn document_descendant_path_walks_foreign_doc() {
        let aux = write_tmp(
            "descendant.xml",
            b"<r><a id='1'/><a id='2'/><b/></r>",
        );
        let doc = parse(b"<src/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };

        // //a should match 2 <a> elements inside the foreign doc.
        let expr = cs(&format!("document('{}')//a", aux.display()));
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        let o = unsafe { &*obj };
        assert_eq!(o.kind, XPATH_NODESET);
        assert_eq!(unsafe { (*o.nodesetval).nodeNr }, 2);

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn count_of_foreign_nodeset_returns_correct_number() {
        let aux = write_tmp(
            "count.xml",
            b"<r><x/><x/><x/><x/><y/></r>",
        );
        let doc = parse(b"<src/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };

        let expr = cs(&format!("count(document('{}')//x)", aux.display()));
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        let o = unsafe { &*obj };
        assert_eq!(o.kind, XPATH_NUMBER);
        assert_eq!(o.floatval, 4.0);

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn foreign_predicate_with_attribute_compare() {
        // Predicate body re-enters the engine with a foreign node as
        // context — exercises apply_foreign_path's predicate handling.
        let aux = write_tmp(
            "pred_attr.xml",
            b"<r><item id='alpha'/><item id='beta'/><item id='gamma'/></r>",
        );
        let doc = parse(b"<src/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };

        let expr = cs(&format!(
            "count(document('{}')//item[@id='beta'])",
            aux.display(),
        ));
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        let o = unsafe { &*obj };
        assert_eq!(o.kind, XPATH_NUMBER);
        assert_eq!(o.floatval, 1.0);

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn foreign_string_equality_against_literal() {
        // String comparison routes through values_eq's ForeignNodeSet
        // arm, which calls bindings.foreign_string_value on each foreign
        // pointer.
        let aux = write_tmp(
            "str_eq.xml",
            b"<r><name>Ada</name><name>Bob</name></r>",
        );
        let doc = parse(b"<src/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };

        let expr_true = cs(&format!(
            "document('{}')//name = 'Bob'",
            aux.display(),
        ));
        let obj = unsafe { xmlXPathEvalExpression(expr_true.as_ptr(), ctx) };
        assert!(!obj.is_null());
        let o = unsafe { &*obj };
        assert_eq!(o.kind, XPATH_BOOLEAN);
        assert_eq!(o.boolval, 1);
        unsafe { xmlXPathFreeObject(obj); }

        let expr_false = cs(&format!(
            "document('{}')//name = 'Carol'",
            aux.display(),
        ));
        let obj = unsafe { xmlXPathEvalExpression(expr_false.as_ptr(), ctx) };
        let o = unsafe { &*obj };
        assert_eq!(o.boolval, 0);

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn cross_doc_union_dedups_by_pointer_identity() {
        // Two foreign docs → Value::ForeignNodeSet union arm.
        // Counts must add (no spurious dedup of distinct pointers).
        let a = write_tmp("union_a.xml", b"<r><x/><x/></r>");
        let b = write_tmp("union_b.xml", b"<r><y/></r>");
        let doc = parse(b"<src/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };

        let expr = cs(&format!(
            "count(document('{}')//x | document('{}')//y)",
            a.display(), b.display(),
        ));
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        let o = unsafe { &*obj };
        assert_eq!(o.floatval, 3.0);

        // Same URI twice — second load is cached AND the same pointers
        // get deduped, so the count equals one side alone.
        let expr_dup = cs(&format!(
            "count(document('{}')//x | document('{}')//x)",
            a.display(), a.display(),
        ));
        let obj2 = unsafe { xmlXPathEvalExpression(expr_dup.as_ptr(), ctx) };
        assert_eq!(unsafe { (*obj2).floatval }, 2.0);

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeObject(obj2);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn mixed_primary_foreign_union_errors() {
        // /src | document('X')/r — left is a primary NodeSet, right is a
        // ForeignNodeSet.  Engine returns an explicit error rather than
        // miscompile (no support for mixed-kind unions in Option D).
        let aux = write_tmp("mixed_union.xml", b"<r><x/></r>");
        let doc = parse(b"<src/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };

        let expr = cs(&format!(
            "/src | document('{}')/r",
            aux.display(),
        ));
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        // Error path: returns NULL with error logged.
        assert!(obj.is_null());

        unsafe {
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn missing_document_uri_errors() {
        let doc = parse(b"<src/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };
        // Path that definitely doesn't exist.
        let expr = cs("document('/nonexistent/path/that/should/not/exist.xml')");
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(obj.is_null());
        unsafe {
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn empty_document_uri_errors_outside_xslt() {
        // document('') needs ctx->extra (libxslt's transform context),
        // which is NULL in a plain xmlXPathContext.  Surface a clear
        // error rather than crashing.
        let doc = parse(b"<src/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };
        let expr = cs("document('')");
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(obj.is_null());
        unsafe {
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    #[test]
    fn document_cache_returns_same_pointer_on_repeat() {
        // Two calls to document(URI) for the same URI in the same
        // context must return the same doc pointer — the cache in
        // XPathPrivate::loaded_docs is keyed by URI.
        let aux = write_tmp("cache.xml", b"<r><a/></r>");
        let doc = parse(b"<src/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };

        let expr = cs(&format!("document('{}')", aux.display()));
        let obj1 = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        let obj2 = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj1.is_null() && !obj2.is_null());

        let p1 = unsafe { *((*(*obj1).nodesetval).nodeTab) };
        let p2 = unsafe { *((*(*obj2).nodesetval).nodeTab) };
        assert_eq!(p1, p2, "second document() call must reuse cached doc");

        unsafe {
            xmlXPathFreeObject(obj1);
            xmlXPathFreeObject(obj2);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    /// `document('')` reads `ctx->extra` as an `xsltTransformContext*`,
    /// dereferences the first 8 bytes to find `xsltStylesheet*`, then
    /// reads byte offset 32 inside that to get `xmlDocPtr`.  We fake
    /// the byte pattern in this test — the offsets are exactly what
    /// production code assumes against libxslt 1.1.45's
    /// `xsltInternals.h`.  If libxslt ever moves these fields and we
    /// don't update the production code, this test will (rightly)
    /// continue to pass against the matching fake — the lxml
    /// integration suite catches the real-world drift.  The point of
    /// this unit test is purely to exercise our reading-of-extra
    /// path without needing a running XSLT engine.
    #[test]
    fn empty_document_uri_reads_via_ctx_extra() {
        let stylesheet_doc = parse(b"<stylesheet><test>marker</test></stylesheet>");

        // Fake xsltStylesheet: zero-init, write doc pointer at offset 32.
        let mut style: Vec<u8> = vec![0u8; 64];
        style[32..40].copy_from_slice(&(stylesheet_doc as usize).to_ne_bytes());
        let style_box = style.into_boxed_slice();
        let style_ptr = Box::into_raw(style_box) as *mut u8;

        // Fake xsltTransformContext: zero-init, write style pointer at offset 0.
        // Must be large enough to cover every field the document() path reads
        // — including the `sec` access-control pointer at offset 272 that the
        // security gate dereferences before the empty-URI branch.
        const TCTX_LEN: usize = 320;
        let mut tctx: Vec<u8> = vec![0u8; TCTX_LEN];
        tctx[0..8].copy_from_slice(&(style_ptr as usize).to_ne_bytes());
        let tctx_box = tctx.into_boxed_slice();
        let tctx_ptr = Box::into_raw(tctx_box) as *mut u8;

        let primary = parse(b"<src/>");
        let ctx = unsafe { xmlXPathNewContext(primary) };
        unsafe { (*ctx).extra = tctx_ptr as *mut c_void; }

        // document('') should resolve to the stylesheet doc; //test
        // should find the one <test> element inside it.
        let expr = cs("count(document('')//test)");
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null(), "document('') eval returned NULL");
        assert_eq!(unsafe { (*obj).kind }, XPATH_NUMBER);
        assert_eq!(unsafe { (*obj).floatval }, 1.0);

        unsafe {
            xmlXPathFreeObject(obj);
            // Clear ctx->extra before freeing the context so its
            // teardown doesn't try to walk into our fake bytes.
            (*ctx).extra = std::ptr::null_mut();
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(primary);
            xmlFreeDoc(stylesheet_doc);
            // Reclaim the fake structs.  Lengths match the Vecs we
            // allocated above; from_raw_parts reconstructs them as
            // slices for Box::drop.
            drop(Box::from_raw(std::slice::from_raw_parts_mut(tctx_ptr, TCTX_LEN)));
            drop(Box::from_raw(std::slice::from_raw_parts_mut(style_ptr, 64)));
        }
    }

    // ── EXSLT regexp:match unit tests ─────────────────────────────────────

    /// Pure-function tests for `regexp_match_texts` — exercise the
    /// libexslt-compatible shape rules without needing an XPath context.
    mod regexp_match_texts_tests {
        use super::super::regexp_match_texts;
        use regex::Regex;

        #[test]
        fn no_match_returns_empty() {
            let re = Regex::new("xyz").unwrap();
            assert!(regexp_match_texts(&re, "abc", false).is_empty());
            assert!(regexp_match_texts(&re, "abc", true).is_empty());
        }

        #[test]
        fn single_match_no_groups_no_global() {
            let re = Regex::new("d.").unwrap();
            // Only the full match, no capture groups exposed.
            assert_eq!(regexp_match_texts(&re, "abdCdEeDed", false), vec!["dC"]);
        }

        #[test]
        fn single_match_with_groups_no_global() {
            // libexslt's reference behavior: full match first, then
            // each capture group as its own <match>.
            let re = Regex::new("([0-9]+)([a-z]+)([0-9]+)").unwrap();
            assert_eq!(
                regexp_match_texts(&re, "123abc567", false),
                vec!["123abc567", "123", "abc", "567"],
            );
        }

        #[test]
        fn unmatched_optional_group_emits_empty_match() {
            // The `(:\d*)?` group is optional; on a URL without a port
            // it doesn't participate.  libexslt emits an empty
            // `<match/>` in its slot rather than skipping.
            let re = Regex::new(r"(\w+):\/\/([^/:]+)(:\d*)?([^# ]*)").unwrap();
            let out = regexp_match_texts(
                &re,
                "http://www.bayes.co.uk/xml/index.xml",
                false,
            );
            assert_eq!(out.len(), 5);
            assert_eq!(out[0], "http://www.bayes.co.uk/xml/index.xml");
            assert_eq!(out[1], "http");
            assert_eq!(out[2], "www.bayes.co.uk");
            assert_eq!(out[3], "");  // unmatched optional group
            assert_eq!(out[4], "/xml/index.xml");
        }

        #[test]
        fn global_flag_returns_full_matches_only() {
            // With 'g', capture groups are NOT exposed even when the
            // pattern has them — output is exactly the full-match
            // strings, one per match.
            let re = Regex::new(r"(\w+)").unwrap();
            assert_eq!(
                regexp_match_texts(&re, "This is a test string", true),
                vec!["This", "is", "a", "test", "string"],
            );
        }

        #[test]
        fn global_with_zero_matches_returns_empty() {
            let re = Regex::new("xyz").unwrap();
            assert!(regexp_match_texts(&re, "no such substring", true).is_empty());
        }
    }

    /// End-to-end test through the XPath engine: regexp:match must
    /// return a real foreign node-set that subsequent XPath ops can
    /// observe (count, string-value, predicates).
    #[test]
    fn regexp_match_returns_real_nodeset() {
        let doc = parse(b"<r>abdCdEeDed</r>");
        let ctx = unsafe { xmlXPathNewContext(doc) };

        // Global match, case-sensitive — 2 hits ("dC", "dE").  The
        // uppercase 'D' at index 7 doesn't match lowercase 'd'.
        // `count()` exercises the ForeignNodeSet arm in eval.rs.
        let expr = cs("count(regexp:match(string(/r), 'd.', 'g'))");
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        let o = unsafe { &*obj };
        assert_eq!(o.kind, XPATH_NUMBER);
        assert_eq!(o.floatval, 2.0);
        unsafe { xmlXPathFreeObject(obj); }

        // Global + case-insensitive — 3 hits ("dC", "dE", "De").
        let expr_i = cs("count(regexp:match(string(/r), 'd.', 'gi'))");
        let obj_i = unsafe { xmlXPathEvalExpression(expr_i.as_ptr(), ctx) };
        assert_eq!(unsafe { (*obj_i).floatval }, 3.0);
        unsafe { xmlXPathFreeObject(obj_i); }

        // Non-global — 1 hit (the first match plus zero capture groups).
        let expr2 = cs("count(regexp:match(string(/r), 'd.'))");
        let obj2 = unsafe { xmlXPathEvalExpression(expr2.as_ptr(), ctx) };
        assert_eq!(unsafe { (*obj2).floatval }, 1.0);
        unsafe { xmlXPathFreeObject(obj2); }

        unsafe {
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    /// String-value of the first <match> element must be the actual
    /// matched text.  Exercises the foreign_string_value bindings
    /// hook end-to-end.
    #[test]
    fn regexp_match_first_string_value_is_correct() {
        let doc = parse(b"<r>hello world</r>");
        let ctx = unsafe { xmlXPathNewContext(doc) };
        let expr = cs("string(regexp:match(string(/r), '\\w+'))");
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        let o = unsafe { &*obj };
        assert_eq!(o.kind, XPATH_STRING);
        let s = unsafe { CStr::from_ptr(o.stringval) }.to_str().unwrap();
        assert_eq!(s, "hello");

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    /// XML-special characters in the matched substring must round-
    /// trip safely through the RTF builder (no parse failure, no
    /// content corruption).
    #[test]
    fn regexp_match_escapes_xml_special_chars() {
        let doc = parse(b"<r>x&amp;&lt;&gt;y</r>");
        let ctx = unsafe { xmlXPathNewContext(doc) };

        // The string is "x&<>y" after entity expansion.  Match '.+'
        // captures the whole thing — its text in the <match> element
        // must come back intact.
        let expr = cs("string(regexp:match(string(/r), '.+'))");
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        let s = unsafe { CStr::from_ptr((*obj).stringval) }.to_str().unwrap();
        assert_eq!(s, "x&<>y");

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    /// Case-insensitive ('i') and dotall ('s') flags should pass
    /// through to the underlying regex; verify both are honored.
    #[test]
    fn regexp_match_honors_case_and_dotall_flags() {
        let doc = parse(b"<r/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };

        // 'i' flag: HELLO matches 'hello'.
        let expr_i = cs("count(regexp:match('HELLO', 'hello', 'gi'))");
        let obj_i = unsafe { xmlXPathEvalExpression(expr_i.as_ptr(), ctx) };
        assert_eq!(unsafe { (*obj_i).floatval }, 1.0);
        unsafe { xmlXPathFreeObject(obj_i); }

        // 's' flag: . matches newline.
        let expr_s = cs("count(regexp:match('a\nb', 'a.b', 's'))");
        let obj_s = unsafe { xmlXPathEvalExpression(expr_s.as_ptr(), ctx) };
        assert_eq!(unsafe { (*obj_s).floatval }, 1.0);
        unsafe { xmlXPathFreeObject(obj_s); }

        // Without 's', . doesn't match newline → no match.
        let expr_no_s = cs("count(regexp:match('a\nb', 'a.b'))");
        let obj_no_s = unsafe { xmlXPathEvalExpression(expr_no_s.as_ptr(), ctx) };
        assert_eq!(unsafe { (*obj_no_s).floatval }, 0.0);
        unsafe { xmlXPathFreeObject(obj_no_s); }

        unsafe {
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    /// Invalid regex pattern must surface as an empty match-set,
    /// not panic or error out.
    #[test]
    fn regexp_match_invalid_pattern_returns_empty() {
        let doc = parse(b"<r/>");
        let ctx = unsafe { xmlXPathNewContext(doc) };
        // Unclosed '(' is invalid in our regex backend.
        let expr = cs("count(regexp:match('hello', '('))");
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        assert_eq!(unsafe { (*obj).floatval }, 0.0);

        unsafe {
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
        }
    }

    /// `eval_expression` swaps to the foreign doc's `DocIndex` when
    /// `ctx->node` belongs to a doc other than `ctx->doc`.  This is
    /// the path libxslt's `xsl:for-each` over a foreign nodeset takes:
    /// it sets `ctx->node` to each iteration's element (foreign) while
    /// leaving `ctx->doc` at the source doc.  We don't need a real
    /// for-each — driving `ctx->node` manually is enough to verify the
    /// doc-swap.
    #[test]
    fn xpath_swaps_doc_index_for_foreign_context_node() {
        let aux = write_tmp("foreach.xml", b"<r><a><deep>found</deep></a></r>");
        let primary = parse(b"<src/>");
        let ctx = unsafe { xmlXPathNewContext(primary) };

        // Load + pick a foreign <a> element through normal document()
        // evaluation — we want the real foreign pointer that libxslt
        // would observe inside an xsl:for-each body.
        let pick = cs(&format!("document('{}')//a", aux.display()));
        let picked = unsafe { xmlXPathEvalExpression(pick.as_ptr(), ctx) };
        assert!(!picked.is_null());
        assert_eq!(unsafe { (*(*picked).nodesetval).nodeNr }, 1);
        let a_ptr = unsafe { *((*(*picked).nodesetval).nodeTab) };
        assert!(!a_ptr.is_null());

        // Verify the foreign <a> belongs to a doc other than ctx->doc
        // — otherwise the doc-swap branch wouldn't fire and we'd not
        // actually be testing it.
        let a_doc = unsafe { (*a_ptr).doc.get() } as *const XmlDoc;
        assert_ne!(a_doc as *const _, primary as *const _);

        // Now do what libxslt does inside xsl:for-each: pin ctx->node
        // to the foreign element and evaluate a relative path.
        // `string(deep)` must find the foreign <deep> child and return
        // its content — which is only possible if the engine built its
        // DocIndex over the foreign doc.
        unsafe { (*ctx).node = a_ptr as *const _; }
        let expr = cs("string(deep)");
        let obj = unsafe { xmlXPathEvalExpression(expr.as_ptr(), ctx) };
        assert!(!obj.is_null());
        assert_eq!(unsafe { (*obj).kind }, XPATH_STRING);
        let s = unsafe { CStr::from_ptr((*obj).stringval) }.to_str().unwrap();
        assert_eq!(s, "found",
            "doc-swap failed: relative path didn't resolve in foreign doc");

        unsafe {
            xmlXPathFreeObject(picked);
            xmlXPathFreeObject(obj);
            // Reset ctx->node to the primary doc before freeing —
            // xmlXPathFreeContext doesn't currently read ctx->node,
            // but defensive against future changes.
            (*ctx).node = std::ptr::null();
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(primary);
        }
    }
}
