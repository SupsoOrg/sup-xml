//! Bumpalo-backed, libxml2-shaped DOM.
//!
//! This is the v2 tree representation that replaces the per-node-`malloc`
//! design in [`crate::node`].  It's not wired into the parser yet — that
//! happens in Milestone 2.  Until then, the rest of the codebase keeps using
//! the existing [`crate::node`] types unchanged.
//!
//! # Design
//!
//! * **One arena per document.**  A [`bumpalo::Bump`] owns all node, attribute,
//!   namespace, and string allocations.  Per-node alloc cost drops to a
//!   pointer bump; drop is free per node — the whole arena is freed at once.
//!
//! * **libxml2-shaped nodes.**  A single [`Node`] struct (not a Rust enum)
//!   with a [`NodeKind`] tag and inline fields for every variant.  Children
//!   form a doubly-linked list via `first_child`/`last_child` on the parent
//!   and `next_sibling`/`prev_sibling` + `parent` on each child.  Attributes
//!   form their own doubly-linked list on the element.  Field offsets and
//!   semantics mirror `xmlNode` so a future `extern "C"` shim can expose the
//!   same memory to libxml2 callers verbatim.
//!
//! * **Cell-of-references for links.**  All sibling/child/parent pointers
//!   are `Cell<Option<&'doc Node<'doc>>>`.  This is the standard idiomatic
//!   pattern for graph-shaped data in an arena: you get mutation without
//!   `RefCell` overhead and without raw pointers in the public API.
//!
//! * **Strings borrowed where possible.**  Names and text contents are
//!   `&'doc str`.  When the parser can borrow from the source slice
//!   directly it does; otherwise it allocates a copy in the arena.  No
//!   `Arc<str>` per name.
//!
//! # The self-referential `Document` wrapper
//!
//! [`Document`] owns a `Bump` *and* holds a root pointer (`&Node`) into that
//! same `Bump`.  That's a self-referential struct, which safe Rust doesn't
//! express directly.  The arena nodes have stable addresses (bumpalo never
//! relocates allocations), so the references are sound in principle.  We
//! encode it with one contained `unsafe` block in [`Document::root`], audited by:
//!
//! 1. The `Bump` is stored in an `Arc<Bump>` — heap-allocated, never
//!    moved while any clone of the Arc lives.  C-ABI consumers
//!    (`sup-xml-compat`) share a single thread-local arena across
//!    every document, so cross-doc node grafts (libxml2 consumers
//!    like lxml moving nodes between documents) are safe by
//!    construction — node memory outlives any individual doc.
//! 2. The root pointer is built from `&Bump::alloc(...)` of the same `Bump`,
//!    so the lifetime is genuinely `'self` (as in: tied to the `Document`).
//! 3. The `Document`'s public methods hand out references with a borrow of
//!    `&self`, which the borrow checker treats as `'self`-bounded.  Outside
//!    those methods, no `'doc` reference can escape.
//!
//! See [`Document`] for the API.

#![allow(unsafe_code)]  // see module docs § "self-referential Document wrapper"

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use bumpalo::Bump;

// ── HTML metadata ────────────────────────────────────────────────────────────

/// HTML5 quirks-mode flag set from the DOCTYPE.  Only meaningful for HTML
/// documents; XML documents always carry `None` for `Document::html_metadata`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuirksMode {
    NoQuirks,
    LimitedQuirks,
    Quirks,
}

/// Captured DOCTYPE content from an HTML document.  Stored verbatim; HTML5 does
/// not validate against DTDs, but the public/system identifiers are surfaced
/// for tools that want to introspect them (e.g. detecting XHTML vs HTML5 input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlDoctype {
    pub name: String,
    pub public_id: String,
    pub system_id: String,
}

/// HTML-specific document metadata.  Set when the document came from
/// `parse_html_*`; `None` for XML documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlMeta {
    pub quirks_mode: QuirksMode,
    pub doctype: Option<HtmlDoctype>,
}

// ── node kinds ──────────────────────────────────────────────────────────────

/// Discriminant for [`Node::kind`].
///
/// `#[repr(u32)]` chosen to match libxml2's `xmlElementType` numeric layout —
/// the future `extern "C"` shim can transmute / cast directly.  Variant values
/// are pinned to libxml2's enum order; do not reorder without a coordinated
/// ABI update.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// `<element>...</element>`.
    Element = 1,
    /// An attribute.  Only used as the `kind` of an
    /// [`Attribute`](crate::dom::Attribute) under the `c-abi` feature
    /// — never appears on a real [`Node`]; the libxml2 ABI requires
    /// xmlAttr's `type` field at offset 8 to carry this discriminant
    /// so generic walkers casting `xmlAttr*` to `xmlNode*` see the
    /// right type.
    Attribute = 2,
    /// Character data between tags.  `content` holds the (entity-expanded) text.
    Text    = 3,
    /// `<![CDATA[…]]>`.  Preserved as a distinct kind for round-trip serialization.
    CData   = 4,
    /// An unresolved entity reference — `&name;` left literal in the
    /// tree.  Emitted only when the parser is configured with
    /// `resolve_entities: false`; the default expands entity
    /// references inline.  `name` holds the entity name (e.g.
    /// `"foo"` for `&foo;`); `content` holds the literal source
    /// form `"&foo;"` so serialization round-trips by writing
    /// `content` verbatim, and lxml-compat code can return
    /// `node.text == "&foo;"`.
    ///
    /// libxml2's enum value for `XML_ENTITY_REF_NODE` is 5 —
    /// pinned here so generic walkers see the right tag.
    EntityRef = 5,
    /// `<!-- … -->`.
    Comment = 8,
    /// `<?target content?>`.
    Pi      = 7,
    /// The document root.  Only used as the `kind` of an
    /// [`XmlDoc`](crate::dom::XmlDoc) under the `c-abi` feature — never
    /// appears on a real [`Node`].  libxml2's enum value for
    /// `XML_DOCUMENT_NODE` is 9.
    Document = 9,
    /// A detached subtree root with no semantic content of its own —
    /// just a parent for an ordered list of child nodes.  Returned by
    /// `xmlNewDocFragment` and used by callers that want to build a
    /// composite subtree before grafting it into a real document.
    ///
    /// libxml2's enum value for `XML_DOCUMENT_FRAG_NODE` is 11.
    DocumentFragment = 11,
    /// The DOCTYPE internal-subset node itself (`XML_DTD_NODE` = 14).
    /// Never allocated in the arena — sup-xml models the DTD through
    /// the compat shim's 128-byte `xmlDtd` struct, which shares the
    /// node header (`kind@8`, `children@24`, `parent@40`, `next@48`,
    /// `prev@56`, `doc@64`) with [`Node`].  The shim splices that
    /// struct into the document's sibling chain at the DOCTYPE's true
    /// position; reinterpreted as a `Node` it reports this kind, so
    /// every tree walker traverses past it as an inert node (it emits
    /// no markup of its own — lxml serializes the subset via
    /// `doc->intSubset` directly).
    Dtd = 14,
    /// A document-type internal-subset declaration block (`<!ENTITY …>`,
    /// `<!ELEMENT …>`, `<!ATTLIST …>`, …).  Held as the single child of
    /// the internal-subset DTD node; `content` carries the raw markup
    /// declarations (each terminated by a newline) verbatim, which the
    /// serializer emits unescaped inside the DOCTYPE's `[ … ]`.  The
    /// discriminant sits in libxml2's declaration-type range
    /// (`XML_ELEMENT_DECL` = 15) so generic walkers treat it as a DTD
    /// declaration; sup-xml does not model per-declaration nodes.
    DtdDecl = 15,
}

// ── core types ──────────────────────────────────────────────────────────────

/// Thin NUL-terminated UTF-8 pointer with arena lifetime — c-abi-only.
///
/// Used in the [`Node`] / [`Attribute`] / [`Namespace`] structs under
/// the `c-abi` feature to match libxml2's `xmlChar*` (8-byte thin
/// pointer to NUL-terminated UTF-8) at fixed byte offsets.  On the
/// lean (default) build these fields are `&'doc str` (16 bytes —
/// ptr + len); `ArenaCStr` is the 8-byte alternative used when the
/// libxml2 ABI window applies.
///
/// Callers reach the bytes via [`as_ptr`](Self::as_ptr) (for C FFI —
/// cast to `*const xmlChar`) or [`as_str`](Self::as_str) (for Rust
/// — does a strlen + `from_utf8_unchecked`).
///
/// Builder discipline guarantees the trailing `\0` and that the
/// bytes are valid UTF-8.
#[cfg(feature = "c-abi")]
#[repr(transparent)]
#[derive(Copy, Clone)]
pub struct ArenaCStr<'doc> {
    /// Non-null because libxml2's `xmlChar*` slots use `NULL` to mean
    /// "absent" — we model the absent case explicitly with
    /// `Option<ArenaCStr>` and rely on the niche-optimization here to
    /// keep `Option<ArenaCStr>` 8 bytes wide (matches `xmlChar*` slot
    /// width in `_xmlNs` etc.).
    ptr: std::ptr::NonNull<u8>,
    _marker: std::marker::PhantomData<&'doc u8>,
}

#[cfg(feature = "c-abi")]
impl<'doc> ArenaCStr<'doc> {
    /// Construct from a raw pointer.  Caller asserts NUL-terminated
    /// UTF-8 with at least `'doc` lifetime AND non-null.  Use
    /// [`empty`](Self::empty) when "absent" is the intent.
    ///
    /// # Safety
    /// The pointed-to byte array must be NUL-terminated UTF-8 valid
    /// for the entire `'doc` lifetime, and `ptr` must be non-null.
    #[inline]
    pub unsafe fn from_raw(ptr: *const u8) -> Self {
        debug_assert!(!ptr.is_null(), "ArenaCStr::from_raw given NULL");
        // SAFETY: caller asserts non-null.
        Self {
            ptr: unsafe { std::ptr::NonNull::new_unchecked(ptr as *mut u8) },
            _marker: std::marker::PhantomData,
        }
    }

    /// Raw pointer suitable for C FFI as `*const xmlChar`.
    #[inline]
    pub fn as_ptr(&self) -> *const u8 { self.ptr.as_ptr() }

    /// Decode to `&'doc str`.  Does a strlen scan; bytes are
    /// guaranteed valid UTF-8 by builder discipline.
    pub fn as_str(&self) -> &'doc str {
        // SAFETY: builder allocates with trailing `\0`; bytes are UTF-8.
        unsafe {
            let cstr = std::ffi::CStr::from_ptr(self.ptr.as_ptr() as *const std::os::raw::c_char);
            std::str::from_utf8_unchecked(cstr.to_bytes())
        }
    }

    /// Empty-string singleton — points at a static NUL byte.
    /// Used for "this kind of node doesn't have a name/content"
    /// initialization without a per-instance allocation.
    pub fn empty() -> Self {
        static EMPTY: u8 = 0;
        Self {
            ptr: std::ptr::NonNull::from(&EMPTY),
            _marker: std::marker::PhantomData,
        }
    }

    /// Construct from the address of `xmlStringText` — the static
    /// `"text\0"` exported as a libxml2-ABI symbol.  libxslt's
    /// `xsltCopyText` and similar helpers compare `node->name`
    /// against `xmlStringText` *by pointer* to identify text nodes,
    /// so every Text node we create must point at this exact byte.
    #[inline]
    pub fn text_name() -> Self {
        // SAFETY: xml_string_text() returns the address of a
        // 'static byte array that's NUL-terminated.
        unsafe { Self::from_raw(xml_string_text()) }
    }

    /// Construct from the address of `xmlStringTextNoenc` — the
    /// disable-output-escaping marker.  Same pointer-equality
    /// contract as `text_name`.
    #[inline]
    pub fn text_noenc_name() -> Self {
        unsafe { Self::from_raw(xml_string_text_noenc()) }
    }
}

/// libxml2's `xmlStringText` — exported as a C symbol so callers
/// (libxslt, lxml) that test `node->name == xmlStringText` by
/// pointer get the canonical address.  Defined in tree (rather than
/// compat) so the tree builder can reach it when constructing text
/// nodes without a dependency cycle.
#[cfg(feature = "c-abi")]
#[unsafe(no_mangle)]
pub static xmlStringText: [u8; 5] = *b"text\0";

/// Sibling of `xmlStringText`, for text nodes with
/// `disable-output-escaping="yes"`.
#[cfg(feature = "c-abi")]
#[unsafe(no_mangle)]
pub static xmlStringTextNoenc: [u8; 10] = *b"textnoenc\0";

/// Address of `xmlStringText` for in-crate use.
#[cfg(feature = "c-abi")]
#[inline]
fn xml_string_text() -> *const u8 { (&xmlStringText) as *const _ as *const u8 }

#[cfg(feature = "c-abi")]
#[inline]
fn xml_string_text_noenc() -> *const u8 { (&xmlStringTextNoenc) as *const _ as *const u8 }

#[cfg(feature = "c-abi")]
impl std::fmt::Debug for ArenaCStr<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

/// A namespace binding (`prefix → href`).  Lifetime-bound to the document's arena.
///
/// `prefix` is `None` for the default namespace (`xmlns="…"`).
///
/// # Layout
/// On the lean (default) build, `prefix` and `href` are `&'doc str`
/// (fat slice).  Under the `c-abi` feature this struct is byte-exact
/// with libxml2's `_xmlNs` as verified against libxml2 2.9.13 and
/// 2.15.3.  We do not claim layout compatibility with libxml2
/// versions we have not verified against; the `t-upstream-layout`
/// c-test fails the build when the installed libxml2 header
/// disagrees with the offsets here:
///
/// ```text
/// _xmlNs (64-bit), the odd duck among libxml2 structs because
/// `next` precedes `_private`:
///   xmlNs         *next;            // offset  0   (chain pointer)
///   xmlNsType      type;            // offset  8   (XML_LOCAL_NAMESPACE = 18)
///   const xmlChar *href;            // offset 16
///   const xmlChar *prefix;          // offset 24
///   void          *_private;        // offset 32
///   xmlDoc        *context;         // offset 40
///   // sizeof == 48
/// ```
///
/// We populate `next` via [`DocumentBuilder::append_ns_def`] so
/// `xmlSearchNs` can walk the per-element ns_def chain.  `_private`
/// and `context` are zero-initialized; we don't use them but the
/// slots have to exist for ABI compatibility.
#[cfg(not(feature = "c-abi"))]
#[repr(C)]
#[derive(Debug)]
pub struct Namespace<'doc> {
    pub prefix: Option<&'doc str>,
    pub href:   &'doc str,
}

#[cfg(feature = "c-abi")]
#[repr(C)]
pub struct Namespace<'doc> {
    /// Next namespace declaration on the same element (chain head
    /// rooted at `Node::ns_def`).
    pub next:     Cell<Option<&'doc Namespace<'doc>>>,     //   0
    /// libxml2's `xmlNsType`.  Always `XML_LOCAL_NAMESPACE = 18`
    /// for the only kind of namespace record we produce.
    pub kind:     i32,                                          //   8
    _pad_kind:    i32,                                          //  12
    pub href:     ArenaCStr<'doc>,                            //  16
    pub prefix:   Option<ArenaCStr<'doc>>,                    //  24
    pub _private: Cell<*mut std::os::raw::c_void>,             //  32
    pub context:  Cell<*mut std::os::raw::c_void>,             //  40 (xmlDoc*)
}

#[cfg(feature = "c-abi")]
impl std::fmt::Debug for Namespace<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Namespace")
            .field("prefix", &self.prefix.map(|p| p.as_str()))
            .field("href",   &self.href.as_str())
            .finish()
    }
}

impl<'doc> Namespace<'doc> {
    /// Namespace prefix (`"xlink"`, etc.), or `None` for the default
    /// namespace declaration (`xmlns="..."`).
    ///
    /// **Prefer this method over direct `ns.prefix` field access.**
    /// On the lean rlib build it just returns the field; on the
    /// `c-abi` build the storage type changes to a thin
    /// NUL-terminated `ArenaCStr` and this method handles the
    /// conversion.
    #[inline]
    pub fn prefix(&self) -> Option<&'doc str> {
        #[cfg(not(feature = "c-abi"))]
        { self.prefix }
        #[cfg(feature = "c-abi")]
        { self.prefix.map(|p| p.as_str()) }
    }

    /// Namespace URI (the right-hand side of `xmlns="..."`).  Same
    /// rationale as [`prefix`](Self::prefix) for preferring this
    /// over direct field access.
    #[inline]
    pub fn href(&self) -> &'doc str {
        #[cfg(not(feature = "c-abi"))]
        { self.href }
        #[cfg(feature = "c-abi")]
        { self.href.as_str() }
    }
}

/// An attribute on an element.  Belongs to a doubly-linked list rooted at
/// [`Node::first_attribute`] / [`Node::last_attribute`] on its owning element.
///
/// # Layout
/// libxml2's `_xmlAttr` shares its first 8 fields with `_xmlNode`:
/// generic walkers cast `xmlAttr*` to `xmlNode*` and read `.type` /
/// `.name`.  Under the `c-abi` feature this struct's layout matches
/// `_xmlAttr` byte-exact.
///
/// ```text
/// libxml2 _xmlAttr (64-bit):
///   void           *_private;     // offset  0
///   xmlElementType  type;         // offset  8   (XML_ATTRIBUTE_NODE = 2)
///   const xmlChar  *name;         // offset 16
///   xmlNode        *children;     // offset 24   (text/entity content nodes)
///   xmlNode        *last;         // offset 32
///   xmlNode        *parent;       // offset 40   (the element)
///   xmlAttr        *next;         // offset 48
///   xmlAttr        *prev;         // offset 56
///   xmlDoc         *doc;          // offset 64
///   xmlNs          *ns;           // offset 72
///   xmlAttributeType atype;       // offset 80   (DTD type info)
///   void           *psvi;         // offset 88
///   ...
/// ```
#[cfg(not(feature = "c-abi"))]
#[repr(C)]
pub struct Attribute<'doc> {
    pub name:      &'doc str,
    pub namespace: Cell<Option<&'doc Namespace<'doc>>>,
    /// Already entity-expanded by the parser.
    pub value:     &'doc str,
    pub next:      Cell<Option<&'doc Attribute<'doc>>>,
    pub prev:      Cell<Option<&'doc Attribute<'doc>>>,
    /// Element this attribute belongs to.  `None` only for a freshly-allocated
    /// attribute not yet attached via [`DocumentBuilder::append_attribute`].
    pub parent:    Cell<Option<&'doc Node<'doc>>>,
}

/// xmlAttr-mirror layout (c-abi build).  See type-level doc for the
/// libxml2 reference layout.  Note: libxml2's `xmlAttr` puts the
/// **value** in a child text node chain (`children`/`last`), not in a
/// dedicated `value` field.  Our `value: ArenaCStr` is in the
/// sup-xml-only tail (after `psvi`).  C callers that follow
/// `xmlAttr::children` get the text node our serializer materialises;
/// callers using `xmlGetProp(...)` (the documented path) get the
/// value directly through that function.
#[cfg(feature = "c-abi")]
#[repr(C)]
pub struct Attribute<'doc> {
    pub _private:  Cell<*mut std::os::raw::c_void>,            //   0
    pub kind:      NodeKind,             // XML_ATTRIBUTE_NODE //   8
    _pad_kind:     u32,                                        //  12
    pub name:      ArenaCStr<'doc>,                          //  16
    pub children:  Cell<Option<&'doc Node<'doc>>>,         //  24  (text-node chain)
    pub last:      Cell<Option<&'doc Node<'doc>>>,         //  32
    pub parent:    Cell<Option<&'doc Node<'doc>>>,         //  40
    pub next:      Cell<Option<&'doc Attribute<'doc>>>,    //  48
    pub prev:      Cell<Option<&'doc Attribute<'doc>>>,    //  56
    pub doc:       Cell<*mut std::os::raw::c_void>,            //  64  (xmlDoc*)
    pub namespace: Cell<Option<&'doc Namespace<'doc>>>,    //  72  (`ns`)
    pub atype:     u32,                                        //  80  (xmlAttributeType)
    _pad_atype:    u32,                                        //  84
    pub psvi:      Cell<*mut std::os::raw::c_void>,            //  88
    // ── sup-xml-only tail (NOT part of ABI window) ──
    /// Already entity-expanded by the parser.  Direct slot — libxml2
    /// callers normally use `xmlGetProp(...)` rather than reading it
    /// from the struct directly.
    pub value:     ArenaCStr<'doc>,
}

impl<'doc> Attribute<'doc> {
    /// Attribute name (`xlink:href`, `id`, etc.).
    ///
    /// **Prefer this method over direct `attr.name` field access.**
    /// The `pub name` field stays stable on the lean rlib build; in
    /// the `c-abi` build it becomes a thin NUL-terminated
    /// `ArenaCStr`.  This method returns `&str` either way.
    #[inline]
    pub fn name(&self) -> &'doc str {
        #[cfg(not(feature = "c-abi"))]
        { self.name }
        #[cfg(feature = "c-abi")]
        { self.name.as_str() }
    }

    /// The attribute's local name (the part after any `prefix:`).
    ///
    /// Layout-agnostic, mirroring [`Node::local_name`]: on the lean
    /// build [`name`](Self::name) is the full QName, while on the
    /// `c-abi` build the prefix is stripped at parse time so `name()`
    /// is already local.  Callers matching a namespaced attribute by
    /// its local part should use this rather than `name()`.
    #[inline]
    pub fn local_name(&self) -> &'doc str {
        let n = self.name();
        match n.rfind(':') {
            Some(i) => &n[i + 1..],
            None    => n,
        }
    }

    /// Attribute value (entity-expanded by the parser).  Same
    /// rationale as [`name`](Self::name) for preferring this over
    /// direct field access.
    ///
    /// In the c-abi build, attribute values are stored two ways:
    /// the sup-xml-only `value` slot (set by our parser /
    /// `new_attribute` API) and the libxml2-standard text-node
    /// chain rooted at `children` (the path libxslt writes to when
    /// it builds attributes during an XSLT transform — it doesn't
    /// know about our tail field).  When `value` is empty, fall
    /// back to walking `children` so attributes built post-parse
    /// by libxslt still report the right text.
    #[inline]
    pub fn value(&self) -> &'doc str {
        #[cfg(not(feature = "c-abi"))]
        { self.value }
        #[cfg(feature = "c-abi")]
        {
            let v = self.value.as_str();
            if !v.is_empty() { return v; }
            let mut child = self.children.get();
            while let Some(c) = child {
                let s = c.content();
                if !s.is_empty() { return s; }
                child = c.next_sibling.get();
            }
            ""
        }
    }

    /// Iterate the attribute's text-node child chain — the libxml2
    /// representation of the attribute's value.  Only meaningful in
    /// the c-abi build (where `Attribute` carries `children`); the
    /// lean build holds the value directly in [`value`](Self::value).
    #[cfg(feature = "c-abi")]
    pub fn children(&self) -> ChildIter<'doc> {
        ChildIter::from_head(self.children.get())
    }
}

impl std::fmt::Debug for Attribute<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attribute")
            .field("name",  &self.name())
            .field("value", &self.value())
            .finish()
    }
}

/// A node in the XML tree.  One struct for every kind; fields are used per-kind.
///
/// # Layout (libxml2 mirror)
///
/// The struct is `#[repr(C)]` with field order chosen to align with
/// `_xmlNode` in libxml2.  When we later expose an `extern "C"` shim,
/// callers can treat `*mut Node` as `xmlNode*` (modulo a few private slots
/// we may still need to add — `_private`, `psvi`, etc.).
///
/// # Field usage by kind
///
/// | Kind         | `name`       | `content`                  | `first_*` / `last_*` |
/// |--------------|--------------|----------------------------|----------------------|
/// | `Element`    | tag name     | unused (`""`)              | children + attrs     |
/// | `Text`       | `""`         | text data                  | unused               |
/// | `CData`      | `""`         | CDATA payload              | unused               |
/// | `Comment`    | `""`         | comment text               | unused               |
/// | `Pi`         | target       | content after target       | unused               |
#[cfg(not(feature = "c-abi"))]
#[repr(C)]
pub struct Node<'doc> {
    pub kind:            NodeKind,
    pub name:            &'doc str,
    pub namespace:       Cell<Option<&'doc Namespace<'doc>>>,
    /// Linked-list head for element attributes (None for non-elements).
    pub first_attribute: Cell<Option<&'doc Attribute<'doc>>>,
    pub last_attribute:  Cell<Option<&'doc Attribute<'doc>>>,
    pub first_child:     Cell<Option<&'doc Node<'doc>>>,
    pub last_child:      Cell<Option<&'doc Node<'doc>>>,
    pub parent:          Cell<Option<&'doc Node<'doc>>>,
    pub next_sibling:    Cell<Option<&'doc Node<'doc>>>,
    pub prev_sibling:    Cell<Option<&'doc Node<'doc>>>,
    /// Text/CData/Comment/Pi payload.  `None` mirrors libxml2's NULL
    /// `content` — notably a processing instruction with no data
    /// (`<?foo?>`), which serializes without the trailing space that a
    /// PI carrying a (possibly empty) data section gets.
    pub content:         Cell<Option<&'doc str>>,
    /// 1-based source line of the opening tag.  0 for nodes constructed
    /// programmatically.
    pub line:            u32,
}

/// libxml2-shape Node (c-abi build).  Byte-exact match with `_xmlNode`
/// for the public window (offsets 0..120).  See type-level doc and
/// `thoughts/c_abi_implementation_plan.md` for the offset table.
///
/// Field naming choices for the c-abi build:
/// - `first_child` / `last_child` rather than libxml2's `children` /
///   `last` (the field SLOT matches; we use the more descriptive
///   sup-xml name).
/// - `first_attribute` rather than libxml2's `properties` (same
///   reasoning).
/// - `namespace` rather than libxml2's `ns` (same).
/// - `ns_def` matches libxml2's name verbatim.
#[cfg(feature = "c-abi")]
#[repr(C)]
pub struct Node<'doc> {
    pub _private:        Cell<*mut std::os::raw::c_void>,            //   0
    pub kind:            NodeKind,                  // xmlElementType //   8
    _pad_kind:           u32,                                        //  12
    pub name:            ArenaCStr<'doc>,                          //  16
    pub first_child:     Cell<Option<&'doc Node<'doc>>>,         //  24
    pub last_child:      Cell<Option<&'doc Node<'doc>>>,         //  32
    pub parent:          Cell<Option<&'doc Node<'doc>>>,         //  40
    pub next_sibling:    Cell<Option<&'doc Node<'doc>>>,         //  48
    pub prev_sibling:    Cell<Option<&'doc Node<'doc>>>,         //  56
    pub doc:             Cell<*mut std::os::raw::c_void>,            //  64  (xmlDoc*)
    pub namespace:       Cell<Option<&'doc Namespace<'doc>>>,    //  72
    pub content:         Cell<Option<ArenaCStr<'doc>>>,            //  80
    pub first_attribute: Cell<Option<&'doc Attribute<'doc>>>,    //  88
    pub ns_def:          Cell<Option<&'doc Namespace<'doc>>>,    //  96
    pub psvi:            Cell<*mut std::os::raw::c_void>,            // 104
    pub line:            u16,                                        // 112
    pub extra:           u16,                                        // 114
    _pad_extra:          [u8; 4],                                    // 116..120
    // ── sup-xml-only tail (NOT part of ABI window) ──
    /// Tail of the attribute linked list for O(1) append.  libxml2
    /// walks `first_attribute->next->...->NULL` to find the tail;
    /// we cache it for the parse hot path.
    pub last_attribute:  Cell<Option<&'doc Attribute<'doc>>>,
    /// Full-width source line, uncapped.  The ABI `line` field is a
    /// `u16` (libxml2's `unsigned short`) so it saturates at 65535; this
    /// shadow copy keeps the real line for files past that, which
    /// `xmlGetLineNo` returns in preference.  `0` for nodes not produced
    /// by the parser (created via the tree API), which fall back to
    /// `line`.  libxml2 instead stashes big lines in `psvi` on text nodes
    /// and recurses in `xmlGetLineNo`; a dedicated field is exact (no
    /// neighbour-line guessing for childless nodes like `<br/>`).
    pub full_line:       u32,
}

// ── compile-time layout assertions (c-abi build) ────────────────────────────
//
// These `const _:` blocks verify that our `#[repr(C)]` struct layouts
// match libxml2 byte-exact at every field offset in the public ABI
// window.  Drift = compile error.
//
// The offsets below are pinned to libxml2 2.9.13 and 2.15.3, the
// versions we have verified compat against.  We do NOT make a
// forward-compatibility claim — if libxml2 ships a version that
// appends fields to `_xmlNode` / `_xmlAttr` / `_xmlDoc`, our Box
// allocations would be undersized for callers reading at the new
// offsets and compat needs a coordinated bump on our side.
//
// Two checks defend against drifting unawares:
//   - `crates/compat/c-tests/t-upstream-layout.c` compiles
//     `_Static_assert(offsetof(...))` against the *live installed*
//     `<libxml/...>` headers.  If the dev/CI host has a libxml2
//     version newer than what we've pinned, the build breaks at the
//     exact offset that moved.
//   - `crates/compat/c-tests/t-layout-{03,04}.c` plus `t-err-03.c`
//     use locally-typedef'd reference structs to verify these Rust
//     offsets match compat's expected C-side view.
//   - `crates/compat/c-tests/t-libxml2-headers.c` uses the real
//     headers at runtime — would have caught the earlier mistake
//     where we mis-encoded `_xmlNs`.

#[cfg(feature = "c-abi")]
const _: () = {
    use std::mem::offset_of;

    // ── _xmlNode (64-bit) ──
    assert!(offset_of!(Node<'static>, _private)        ==   0, "Node::_private @ 0");
    assert!(offset_of!(Node<'static>, kind)            ==   8, "Node::kind @ 8");
    assert!(offset_of!(Node<'static>, name)            ==  16, "Node::name @ 16");
    assert!(offset_of!(Node<'static>, first_child)     ==  24, "Node::first_child (children) @ 24");
    assert!(offset_of!(Node<'static>, last_child)      ==  32, "Node::last_child (last) @ 32");
    assert!(offset_of!(Node<'static>, parent)          ==  40, "Node::parent @ 40");
    assert!(offset_of!(Node<'static>, next_sibling)    ==  48, "Node::next_sibling (next) @ 48");
    assert!(offset_of!(Node<'static>, prev_sibling)    ==  56, "Node::prev_sibling (prev) @ 56");
    assert!(offset_of!(Node<'static>, doc)             ==  64, "Node::doc @ 64");
    assert!(offset_of!(Node<'static>, namespace)       ==  72, "Node::namespace (ns) @ 72");
    assert!(offset_of!(Node<'static>, content)         ==  80, "Node::content @ 80");
    assert!(offset_of!(Node<'static>, first_attribute) ==  88, "Node::first_attribute (properties) @ 88");
    assert!(offset_of!(Node<'static>, ns_def)          ==  96, "Node::ns_def @ 96");
    assert!(offset_of!(Node<'static>, psvi)            == 104, "Node::psvi @ 104");
    assert!(offset_of!(Node<'static>, line)            == 112, "Node::line @ 112");
    assert!(offset_of!(Node<'static>, extra)           == 114, "Node::extra @ 114");
    // The libxml2 ABI window ends at offset 120.  Our sup-xml-only
    // tail (last_attribute) starts there or after — we don't pin its
    // offset since it's not part of the contract.
    assert!(offset_of!(Node<'static>, last_attribute)  >= 120, "Node tail starts after ABI window");

    // ── _xmlAttr (64-bit) — shares first 8 fields with _xmlNode ──
    assert!(offset_of!(Attribute<'static>, _private)   ==   0, "Attribute::_private @ 0");
    assert!(offset_of!(Attribute<'static>, kind)       ==   8, "Attribute::kind @ 8");
    assert!(offset_of!(Attribute<'static>, name)       ==  16, "Attribute::name @ 16");
    assert!(offset_of!(Attribute<'static>, children)   ==  24, "Attribute::children @ 24");
    assert!(offset_of!(Attribute<'static>, last)       ==  32, "Attribute::last @ 32");
    assert!(offset_of!(Attribute<'static>, parent)     ==  40, "Attribute::parent @ 40");
    assert!(offset_of!(Attribute<'static>, next)       ==  48, "Attribute::next @ 48");
    assert!(offset_of!(Attribute<'static>, prev)       ==  56, "Attribute::prev @ 56");
    assert!(offset_of!(Attribute<'static>, doc)        ==  64, "Attribute::doc @ 64");
    assert!(offset_of!(Attribute<'static>, namespace)  ==  72, "Attribute::namespace (ns) @ 72");
    assert!(offset_of!(Attribute<'static>, atype)      ==  80, "Attribute::atype @ 80");
    assert!(offset_of!(Attribute<'static>, psvi)       ==  88, "Attribute::psvi @ 88");
    // value lives in the sup-xml-only tail.
    assert!(offset_of!(Attribute<'static>, value)      >=  96, "Attribute value in tail");

    // ── Struct sizes ──
    // _xmlNode public window is 120 bytes.  Our struct is 120 + tail
    // (last_attribute = 8 bytes, no padding).
    assert!(std::mem::size_of::<Node<'static>>()      >= 120, "Node size >= 120 (ABI window)");
    // Attribute's ABI window is exposed up through `psvi` at offset 88
    // (so the window is 96 bytes including alignment padding); the
    // value field in the tail brings us to 96 + 8 = 104+ minimum.
    assert!(std::mem::size_of::<Attribute<'static>>() >=  96, "Attribute size >= ABI window");

    // ── ArenaCStr is 1 pointer wide (matches xmlChar*) ──
    assert!(std::mem::size_of::<ArenaCStr<'static>>() == std::mem::size_of::<*const u8>(),
        "ArenaCStr must be a single thin pointer to match xmlChar*");
    // ── Option<ArenaCStr> must niche-opt to a single pointer too,
    //    since libxml2 uses NULL `xmlChar*` to mean "absent" and our
    //    Option<ArenaCStr> sits in the xmlChar* slot of _xmlNs::prefix.
    assert!(std::mem::size_of::<Option<ArenaCStr<'static>>>() == std::mem::size_of::<*const u8>(),
        "Option<ArenaCStr> must niche-opt to a single pointer");

    // ── _xmlNs (64-bit) — the odd duck: `next` precedes `_private`
    //    whereas every other public libxml2 struct puts `_private` first.
    //    Layout verified against libxml2 2.9.13 and 2.15.3.
    assert!(offset_of!(Namespace<'static>, next)     ==  0, "Namespace::next @ 0");
    assert!(offset_of!(Namespace<'static>, kind)     ==  8, "Namespace::kind @ 8");
    assert!(offset_of!(Namespace<'static>, href)     == 16, "Namespace::href @ 16");
    assert!(offset_of!(Namespace<'static>, prefix)   == 24, "Namespace::prefix @ 24");
    assert!(offset_of!(Namespace<'static>, _private) == 32, "Namespace::_private @ 32");
    assert!(offset_of!(Namespace<'static>, context)  == 40, "Namespace::context @ 40");
    assert!(std::mem::size_of::<Namespace<'static>>() == 48, "sizeof(Namespace) == 48");
};

impl<'doc> Node<'doc> {
    pub fn is_element(&self)     -> bool { matches!(self.kind, NodeKind::Element) }
    pub fn is_text(&self)        -> bool { matches!(self.kind, NodeKind::Text) }
    pub fn is_entity_ref(&self)  -> bool { matches!(self.kind, NodeKind::EntityRef) }

    /// Element name (or PI target for PI nodes).  Empty `""` for
    /// Text/CData/Comment.
    ///
    /// **Prefer this method over direct `node.name` field access in
    /// internal / downstream code.**  The `pub name` field stays
    /// stable on the lean rlib build, but when sup-xml is built with
    /// the `c-abi` feature the storage type changes to a thin
    /// NUL-terminated `ArenaCStr`.  This method returns `&str` on
    /// both configs — direct field access does not.
    #[inline]
    pub fn name(&self) -> &'doc str {
        #[cfg(not(feature = "c-abi"))]
        { self.name }
        #[cfg(feature = "c-abi")]
        { self.name.as_str() }
    }

    /// Text payload for `Text`/`CData`/`Comment`/`Pi` nodes; `""` for
    /// elements.  See [`name`](Self::name) for why to prefer this
    /// method over direct field access.
    #[inline]
    pub fn content(&self) -> &'doc str {
        self.content_opt().unwrap_or("")
    }

    /// The payload as libxml2 stores it: `None` when the underlying
    /// `content` pointer is NULL, `Some` (possibly `""`) otherwise.
    /// Most callers want [`content`](Self::content); the serializer uses
    /// this to reproduce libxml2's PI rule (a NULL-data PI omits the
    /// space a non-NULL-but-empty-data PI emits).
    pub fn content_opt(&self) -> Option<&'doc str> {
        #[cfg(not(feature = "c-abi"))]
        { self.content.get() }
        #[cfg(feature = "c-abi")]
        { self.content.get().map(|c| c.as_str()) }
    }

    /// Replace the node's content (text/CData/comment/PI payload).
    /// Used by the HTML sink and other in-place tree mutators.  Allocates
    /// a NUL-terminated copy in the arena when the c-abi feature is on.
    ///
    /// `arena` must be the same `DocumentBuilder` that owns this node
    /// (lifetimes enforce it).
    pub fn set_content(&self, arena: &'doc DocumentBuilder, content: &'doc str) {
        #[cfg(not(feature = "c-abi"))]
        {
            let _ = arena; // unused in lean build — content fits straight as &str
            self.content.set(Some(content));
        }
        #[cfg(feature = "c-abi")]
        {
            self.content.set(Some(arena.alloc_arena_cstr(content)));
        }
    }

    /// Iterate child nodes in document order.
    pub fn children(&self) -> ChildIter<'doc> {
        ChildIter { cur: self.first_child.get() }
    }

    /// Iterate attributes in document order.  Returns an empty iterator for
    /// non-elements.
    pub fn attributes(&self) -> AttrIter<'doc> {
        AttrIter { cur: self.first_attribute.get() }
    }

    /// Local part of this element/attr-like node's name — `"foo"` for both
    /// `<foo/>` and `<ns:foo/>`.  On the lean build [`name`](Self::name)
    /// returns the full QName including any prefix; on the `c-abi` build
    /// the prefix is stripped at parse time and `name()` is already the
    /// local part.  `local_name()` is the layout-agnostic accessor:
    /// callers that want to match by local name should use this.
    #[inline]
    pub fn local_name(&self) -> &'doc str {
        let n = self.name();
        match n.rfind(':') {
            Some(i) => &n[i + 1..],
            None    => n,
        }
    }

    /// Iterate this element's xmlns declarations as `(prefix, href)` —
    /// `prefix` is `None` for the default namespace (`xmlns="..."`),
    /// `Some("dc")` for `xmlns:dc="..."`.  Empty for non-elements.
    ///
    /// Storage differs by build: the c-abi build keeps xmlns declarations
    /// on the element's `ns_def` chain (matching libxml2's `_xmlNode`);
    /// the lean build keeps them in [`attributes`](Self::attributes)
    /// instead.  A namespace-blind parse (`namespace_aware: false`) does
    /// no namespace processing and leaves xmlns declarations in the
    /// attribute list under *either* build, so the c-abi iterator walks
    /// `ns_def` first and then the attribute list — the two are mutually
    /// exclusive (a given parse populates one or the other), so nothing is
    /// reported twice.  Callers see every declaration regardless of build
    /// or parse mode.
    pub fn ns_declarations(&self) -> NsDeclIter<'doc> {
        #[cfg(feature = "c-abi")]
        {
            NsDeclIter::CAbi { cur: self.ns_def.get(), attrs: self.first_attribute.get() }
        }
        #[cfg(not(feature = "c-abi"))]
        {
            NsDeclIter::Lean { cur: self.first_attribute.get() }
        }
    }

    /// First child element matching `name`, or `None`.  O(n) walk; for repeated
    /// lookups, iterate `children()` yourself.
    pub fn find_child(&self, name: &str) -> Option<&'doc Node<'doc>> {
        self.children().find(|n| n.is_element() && n.name() == name)
    }

    /// For text-bearing kinds returns the content; for elements returns the
    /// first text-or-CDATA child's content; otherwise `None`.
    pub fn text_content(&self) -> Option<&'doc str> {
        match self.kind {
            NodeKind::Text | NodeKind::CData => Some(self.content()),
            NodeKind::Element => self.children().find_map(|c| match c.kind {
                NodeKind::Text | NodeKind::CData => Some(c.content()),
                _ => None,
            }),
            _ => None,
        }
    }
}

impl std::fmt::Debug for Node<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("Node");
        s.field("kind", &self.kind);
        let name = self.name();
        if !name.is_empty() { s.field("name", &name); }
        let content = self.content();
        if !content.is_empty() { s.field("content", &content); }
        s.finish()
    }
}

/// Iterator over a node's children.
pub struct ChildIter<'doc> { cur: Option<&'doc Node<'doc>> }
impl<'doc> ChildIter<'doc> {
    /// Construct from an explicit head pointer — used by
    /// [`Attribute::children`] which holds its child chain in a
    /// different field than [`Node::children`].
    #[inline]
    pub fn from_head(head: Option<&'doc Node<'doc>>) -> Self {
        Self { cur: head }
    }
}
impl<'doc> Iterator for ChildIter<'doc> {
    type Item = &'doc Node<'doc>;
    fn next(&mut self) -> Option<Self::Item> {
        let c = self.cur?;
        self.cur = c.next_sibling.get();
        Some(c)
    }
}

/// Iterator over an element's attributes.
pub struct AttrIter<'doc> { cur: Option<&'doc Attribute<'doc>> }
impl<'doc> Iterator for AttrIter<'doc> {
    type Item = &'doc Attribute<'doc>;
    fn next(&mut self) -> Option<Self::Item> {
        let a = self.cur?;
        self.cur = a.next.get();
        Some(a)
    }
}

/// Iterator yielded by [`Node::ns_declarations`].  Each item is
/// `(prefix, href)` where `prefix` is `None` for the default
/// namespace declaration.  The variant in use depends on the build
/// feature flags — callers should treat it as opaque.
pub enum NsDeclIter<'doc> {
    #[cfg(feature = "c-abi")]
    CAbi {
        cur:   Option<&'doc Namespace<'doc>>,
        attrs: Option<&'doc Attribute<'doc>>,
    },
    #[cfg(not(feature = "c-abi"))]
    Lean { cur: Option<&'doc Attribute<'doc>> },
}

/// Advance an attribute cursor to the next `xmlns`/`xmlns:*` declaration,
/// yielding `(prefix, href)` and skipping ordinary attributes.
#[inline]
fn next_xmlns_attr<'doc>(
    cur: &mut Option<&'doc Attribute<'doc>>,
) -> Option<(Option<&'doc str>, &'doc str)> {
    loop {
        let a = (*cur)?;
        *cur = a.next.get();
        let n = a.name();
        if n == "xmlns" {
            return Some((None, a.value()));
        } else if let Some(local) = n.strip_prefix("xmlns:") {
            return Some((Some(local), a.value()));
        }
    }
}

impl<'doc> Iterator for NsDeclIter<'doc> {
    type Item = (Option<&'doc str>, &'doc str);
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            #[cfg(feature = "c-abi")]
            NsDeclIter::CAbi { cur, attrs } => {
                if let Some(ns) = *cur {
                    *cur = ns.next.get();
                    return Some((ns.prefix(), ns.href()));
                }
                // `ns_def` exhausted — surface any xmlns declarations that a
                // namespace-blind parse left in the attribute list.
                next_xmlns_attr(attrs)
            }
            #[cfg(not(feature = "c-abi"))]
            NsDeclIter::Lean { cur } => next_xmlns_attr(cur),
        }
    }
}

// ── builder ─────────────────────────────────────────────────────────────────

/// Constructs a [`Document`] one node at a time.  The parser uses this
/// internally; consumers building trees by hand use the same.
///
/// The builder owns a [`Bump`].  Allocated nodes live for the lifetime of
/// the builder (and, after `build()`, of the resulting [`Document`]).
///
/// # Build flow
///
/// ```
/// # use sup_xml_tree::dom::DocumentBuilder;
/// let b = DocumentBuilder::new();
/// let root = b.new_element(b.alloc_str("catalog"));
/// // … allocate more nodes, link them via append_child / append_attribute …
/// b.set_root(root);
/// let doc = b.build();
/// ```
///
/// `set_root` returns immediately (no borrow on the builder is held past the
/// call), so the subsequent `build()` is free to consume `self` even though
/// `root` borrowed from the builder during construction.
pub struct DocumentBuilder {
    /// Refcounted heap allocation.  Stored as `Arc<Bump>` rather than
    /// `Box<Bump>` so a node grafted into another document can keep its
    /// origin arena alive past the origin document's drop — see
    /// [`DocumentBuilder::new_with_dict_and_arena`] and the
    /// `compat::dict::new_doc_arena` rationale.  Cloning is a pure
    /// refcount bump.
    ///
    /// `Bump` is `!Sync`, which makes `Arc<Bump>` `!Send` per Rust's
    /// auto-trait rules.  The containing `Document` re-asserts `Send`
    /// via `unsafe impl` — that's safe because the GIL contract on the
    /// consumer side (and our own internal single-threaded discipline)
    /// guarantees we never use two `&Bump` derived from the same Arc on
    /// two threads concurrently.
    bump: Arc<Bump>,
    /// Per-document name interner.  Element / attribute /
    /// namespace names route through this rather than the bumpalo
    /// arena so consumers see pointer-equality across identical
    /// names — e.g. every `<p>` tag in a 100k-element doc shares
    /// one canonical name pointer instead of 100k arena copies.
    ///
    /// Holds *one* refcounted reference; the underlying dict may be
    /// shared with the parser context that originally created it
    /// (when [`new_with_dict`](Self::new_with_dict) is used) or be
    /// owned solely by this builder otherwise.  Released on drop.
    #[cfg(feature = "c-abi")]
    dict: *mut crate::dict::Dict,
    /// Root pointer.  Stored with `'static` lifetime erasure; the borrow-check
    /// rigor lives in `set_root` (lifetime ties root to `&self`) and the
    /// `Document::root()` accessor (re-binds lifetime to `&'a self`).
    root: Cell<*const Node<'static>>,
    /// XML declaration version (default `"1.0"`).  Set via
    /// [`set_version`](Self::set_version); plumbed into [`Document::version`].
    version:    std::cell::RefCell<String>,
    /// XML declaration encoding (default `"UTF-8"`).  Set via
    /// [`set_encoding`](Self::set_encoding); plumbed into [`Document::encoding`].
    encoding:   std::cell::RefCell<String>,
    /// XML declaration `standalone="…"` value, or `None` when absent.
    /// Set via [`set_standalone`](Self::set_standalone).
    standalone: Cell<Option<bool>>,
    /// URI the document is being loaded from, if known.  Set via
    /// [`set_base_url`](Self::set_base_url); plumbed into
    /// [`Document::base_url`].
    base_url:   std::cell::RefCell<Option<String>>,
    /// HTML-specific metadata.  Set by the HTML parser sink via
    /// [`set_html_metadata`](Self::set_html_metadata); `None` for XML
    /// documents.  Plumbed into [`Document::html_metadata`] on `build`.
    html_metadata: std::cell::RefCell<Option<HtmlMeta>>,
    /// Owned source buffer (raw pointer + length) that arena strings may
    /// point into via [`alloc_str_borrow`](Self::alloc_str_borrow).
    ///
    /// **Raw pointer rather than `Box<[u8]>` is intentional.**  Storing
    /// as `Pin<Box<[u8]>>` triggers a Stacked Borrows violation: when
    /// [`build`](Self::build) consumes `self`, the Box field is moved
    /// into the [`Document`] and the move performs a Unique retag of
    /// the parent.  Any `SharedReadOnly` borrows we'd handed out into
    /// the source bytes (i.e. arena names and text content) get
    /// invalidated by that Unique retag — a real `cargo +nightly miri`
    /// failure.  Storing the bytes via a raw pointer that the parent
    /// only ever observes by-value avoids the retag chain entirely.
    /// See `parse_owned_bytes` in `sup-xml-core::parser` for the
    /// matching parser-side discipline.
    ///
    /// Ownership: the `Box<[u8]>` is leaked at [`set_source`](Self::set_source)
    /// time; the resulting raw pointer is then either reclaimed by
    /// [`build`](Self::build) (which transfers ownership to the
    /// [`Document`]'s `Drop`) or by the builder's own `Drop` if `build`
    /// was never called.
    source_ptr: Cell<*mut u8>,
    source_len: Cell<usize>,
    /// Orphan leaves (comments / PIs) encountered before the root
    /// element opened.  Each entry is a type-erased pointer to a
    /// node already allocated in this builder's arena.  On
    /// [`build`](Self::build) these are linked as `root.prev`
    /// siblings (in order) so consumers walking the document's
    /// children see comments-before-root + root + comments-after-
    /// root, just like libxml2.  Empty when the document has no
    /// prolog/epilogue content (the common case).
    prolog_orphans:   RefCell<Vec<*const Node<'static>>>,
    /// Same as [`prolog_orphans`] for nodes that follow the root.
    epilogue_orphans: RefCell<Vec<*const Node<'static>>>,
}

impl DocumentBuilder {
    pub fn new() -> Self {
        Self {
            bump:          Arc::new(Bump::new()),
            #[cfg(feature = "c-abi")]
            dict:          crate::dict::Dict::new_refcounted(),
            root:          Cell::new(std::ptr::null()),
            version:       std::cell::RefCell::new("1.0".to_string()),
            // Default to empty — libxml2 leaves doc->encoding NULL
            // when the source XML had no `<?xml encoding="…"?>`
            // declaration; serializers then omit the encoding
            // attribute on output.  The parser explicitly sets this
            // when it sees a declaration.
            encoding:      std::cell::RefCell::new(String::new()),
            standalone:    Cell::new(None),
            base_url:      std::cell::RefCell::new(None),
            html_metadata: std::cell::RefCell::new(None),
            source_ptr:    Cell::new(std::ptr::null_mut()),
            source_len:    Cell::new(0),
            prolog_orphans:   RefCell::new(Vec::new()),
            epilogue_orphans: RefCell::new(Vec::new()),
        }
    }

    /// Build with an externally-supplied name dict (refcount bumped
    /// for our reference).  Used when a parser context already owns
    /// a dict that should be shared with the new document — e.g.
    /// libxml2's `xmlCtxtReadMemory` flow, where `ctxt->dict` is
    /// the thread-shared interner the consumer wants names to live
    /// in.  Names interned through this builder reuse that dict's
    /// canonical pointers.
    ///
    /// # Safety
    ///
    /// `dict` must be a valid pointer returned by
    /// [`crate::dict::Dict::new_refcounted`] (or otherwise refcount-
    /// managed by the libxml2 ABI), with at least one outstanding
    /// reference.
    #[cfg(feature = "c-abi")]
    pub unsafe fn new_with_dict(dict: *mut crate::dict::Dict) -> Self {
        // SAFETY: caller asserts `dict` is live with a positive
        // refcount; bumping is sound.
        unsafe { (*dict).add_ref(); }
        Self {
            bump:          Arc::new(Bump::new()),
            dict,
            root:          Cell::new(std::ptr::null()),
            version:       std::cell::RefCell::new("1.0".to_string()),
            // Default to empty — libxml2 leaves doc->encoding NULL
            // when the source XML had no `<?xml encoding="…"?>`
            // declaration; serializers then omit the encoding
            // attribute on output.  The parser explicitly sets this
            // when it sees a declaration.
            encoding:      std::cell::RefCell::new(String::new()),
            standalone:    Cell::new(None),
            base_url:      std::cell::RefCell::new(None),
            html_metadata: std::cell::RefCell::new(None),
            source_ptr:    Cell::new(std::ptr::null_mut()),
            source_len:    Cell::new(0),
            prolog_orphans:   RefCell::new(Vec::new()),
            epilogue_orphans: RefCell::new(Vec::new()),
        }
    }

    /// Build with both an externally-supplied dict and arena.  The
    /// arena is an [`Arc`]-shared [`Bump`]; cloning into a Document
    /// is a pure refcount bump, no allocation.  Used by C-ABI
    /// consumers that route all per-thread doc creation through a
    /// single shared arena — node memory then survives any
    /// individual doc's drop, which makes cross-doc graft
    /// operations (libxml2 consumers' `_appendChild` /
    /// `moveNodeToDocument`) safe by construction.
    ///
    /// # Safety
    ///
    /// `dict` must be a valid pointer returned by
    /// [`crate::dict::Dict::new_refcounted`] with at least one
    /// outstanding reference.
    #[cfg(feature = "c-abi")]
    pub unsafe fn new_with_dict_and_arena(
        dict:  *mut crate::dict::Dict,
        arena: Arc<Bump>,
    ) -> Self {
        // SAFETY: caller asserts `dict` is live with a positive refcount.
        unsafe { (*dict).add_ref(); }
        Self {
            bump:          arena,
            dict,
            root:          Cell::new(std::ptr::null()),
            version:       std::cell::RefCell::new("1.0".to_string()),
            // Default to empty — libxml2 leaves doc->encoding NULL
            // when the source XML had no `<?xml encoding="…"?>`
            // declaration; serializers then omit the encoding
            // attribute on output.  The parser explicitly sets this
            // when it sees a declaration.
            encoding:      std::cell::RefCell::new(String::new()),
            standalone:    Cell::new(None),
            base_url:      std::cell::RefCell::new(None),
            html_metadata: std::cell::RefCell::new(None),
            source_ptr:    Cell::new(std::ptr::null_mut()),
            source_len:    Cell::new(0),
            prolog_orphans:   RefCell::new(Vec::new()),
            epilogue_orphans: RefCell::new(Vec::new()),
        }
    }

    /// Pre-allocate `capacity` bytes for the arena.  Useful when parsing large
    /// documents — avoids the initial small-chunk allocations.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            bump:          Arc::new(Bump::with_capacity(capacity)),
            #[cfg(feature = "c-abi")]
            dict:          crate::dict::Dict::new_refcounted(),
            root:          Cell::new(std::ptr::null()),
            version:       std::cell::RefCell::new("1.0".to_string()),
            // Default to empty — libxml2 leaves doc->encoding NULL
            // when the source XML had no `<?xml encoding="…"?>`
            // declaration; serializers then omit the encoding
            // attribute on output.  The parser explicitly sets this
            // when it sees a declaration.
            encoding:      std::cell::RefCell::new(String::new()),
            standalone:    Cell::new(None),
            base_url:      std::cell::RefCell::new(None),
            html_metadata: std::cell::RefCell::new(None),
            source_ptr:    Cell::new(std::ptr::null_mut()),
            source_len:    Cell::new(0),
            prolog_orphans:   RefCell::new(Vec::new()),
            epilogue_orphans: RefCell::new(Vec::new()),
        }
    }

    /// Stash an owned source buffer that arena strings may borrow from.
    /// The parser calls this with the (possibly transcoded) input bytes so
    /// that subsequent [`alloc_str_borrow`](Self::alloc_str_borrow) calls
    /// can hand back zero-copy `&str` slices into these bytes instead of
    /// memcpy'ing them into the arena.
    ///
    /// Ownership transfers to the [`Document`] on [`build`](Self::build),
    /// so the borrowed slices stay valid for the document's lifetime.
    /// Calling `set_source` twice on the same builder is allowed and
    /// frees the prior buffer.
    pub fn set_source(&self, bytes: Box<[u8]>) {
        // Free any previous source first.
        self.free_source();
        let leaked: &'static mut [u8] = Box::leak(bytes);
        self.source_ptr.set(leaked.as_mut_ptr());
        self.source_len.set(leaked.len());
    }

    /// Free the leaked source bytes (if any).  Used by `Drop` and by
    /// `set_source` for replace semantics.
    fn free_source(&self) {
        let ptr = self.source_ptr.get();
        let len = self.source_len.get();
        if !ptr.is_null() {
            // SAFETY: `ptr` came from `Box::leak`'d `Box<[u8]>` of length `len`.
            // Recover the Box and drop it.
            unsafe {
                let _: Box<[u8]> = Box::from_raw(
                    std::slice::from_raw_parts_mut(ptr, len) as *mut [u8]
                );
            }
            self.source_ptr.set(std::ptr::null_mut());
            self.source_len.set(0);
        }
    }

    /// Raw mutable pointer to the stashed source buffer.  Returns null
    /// if no source has been set.  Used by `parse_bytes_in_place` to
    /// construct a `&mut [u8]` view into the leaked buffer that the
    /// in-place reader will mutate.  Not part of the public stable API
    /// — the parser is the only intended caller.
    #[doc(hidden)]
    pub fn source_ptr_for_inplace(&self) -> *mut u8 {
        self.source_ptr.get()
    }
    /// Companion to [`source_ptr_for_inplace`].
    #[doc(hidden)]
    pub fn source_len_for_inplace(&self) -> usize {
        self.source_len.get()
    }

    /// Return the stashed source bytes, if any.  Returns a slice whose
    /// lifetime is bounded by `&self` — but in practice the bytes live
    /// at a stable heap address until [`build`](Self::build) moves
    /// ownership of them to the [`Document`].
    pub fn source(&self) -> Option<&[u8]> {
        let ptr = self.source_ptr.get();
        let len = self.source_len.get();
        if ptr.is_null() {
            None
        } else {
            // SAFETY: `ptr` came from `Box::leak`, points at `len` valid
            // bytes that we own.  The returned slice is bounded by
            // `&self`'s lifetime; it stays valid until `build()` is
            // called (which transfers the pointer to a `Document` whose
            // own `Drop` will eventually free the bytes).
            Some(unsafe { std::slice::from_raw_parts(ptr, len) })
        }
    }

    /// Set the XML declaration version (e.g. `"1.0"`, `"1.1"`).  Default is
    /// `"1.0"`.  Plumbed into [`Document::version`] on [`build`](Self::build).
    pub fn set_version(&self, v: impl Into<String>) {
        *self.version.borrow_mut() = v.into();
    }

    /// Set the XML declaration encoding (e.g. `"UTF-8"`).  Default is
    /// `"UTF-8"`.  Plumbed into [`Document::encoding`] on [`build`](Self::build).
    pub fn set_encoding(&self, e: impl Into<String>) {
        *self.encoding.borrow_mut() = e.into();
    }

    /// Set the URI the document is being loaded from.  Plumbed into
    /// [`Document::base_url`] on [`build`](Self::build).
    pub fn set_base_url(&self, uri: Option<String>) {
        *self.base_url.borrow_mut() = uri;
    }

    /// Set the XML declaration `standalone="…"` value.  `None` means the
    /// declaration did not include `standalone`; `Some(true)` / `Some(false)`
    /// correspond to `standalone="yes"` / `standalone="no"`.
    pub fn set_standalone(&self, s: Option<bool>) {
        self.standalone.set(s);
    }

    /// Attach HTML-specific document metadata.  Used by the HTML parser sink
    /// to record quirks-mode and DOCTYPE info; XML callers leave this as
    /// `None` (the default).  Plumbed into [`Document::html_metadata`] on
    /// [`build`](Self::build).
    pub fn set_html_metadata(&self, m: Option<HtmlMeta>) {
        *self.html_metadata.borrow_mut() = m;
    }

    /// Direct access to the underlying [`Bump`].  Use sparingly — most allocation
    /// should go through the typed [`new_element`](Self::new_element) etc. methods.
    pub fn bump(&self) -> &Bump { &self.bump }

    /// Copy `s` into the arena and return an arena-lifetime slice.  When the
    /// caller can prove `s` already lives at least as long as the eventual
    /// [`Document`] (e.g. it borrows from the input string slice that the
    /// caller will keep alive), [`alloc_str_borrow`](Self::alloc_str_borrow)
    /// is cheaper — no copy.
    pub fn alloc_str<'a>(&'a self, s: &str) -> &'a str {
        self.bump.alloc_str(s)
    }

    /// Pass through a `&str` that the caller has guaranteed lives at least
    /// until the [`Document`] drops.  Typically the input source slice.
    /// Zero-copy.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `s` outlives the returned reference.  In
    /// practice this means `s` borrows from the same input the parser is
    /// reading and the [`Document`] will be tied to that same input via the
    /// caller's outer lifetime.  For untrusted lifetimes use [`alloc_str`](Self::alloc_str).
    pub unsafe fn alloc_str_borrow<'a>(&'a self, s: &'a str) -> &'a str {
        // SAFETY: extending the lifetime from the caller's '_ to 'a (which is
        // 'self-bounded) is sound because the caller guarantees s outlives 'a.
        unsafe { &*(s as *const str) }
    }

    /// Construct a new node with the given `kind`, `name`, and `content`.
    /// Internal helper — public builder methods call this with the right
    /// shape for each kind.  Centralises the field-init dance so the
    /// cfg-gated layouts only diverge in one place.
    fn new_node_impl<'a>(
        &'a self,
        kind: NodeKind,
        name: &'a str,
        content: Option<&'a str>,
    ) -> &'a mut Node<'a> {
        #[cfg(not(feature = "c-abi"))]
        {
            self.bump.alloc(Node {
                kind,
                name,
                namespace:       Cell::new(None),
                first_attribute: Cell::new(None),
                last_attribute:  Cell::new(None),
                first_child:     Cell::new(None),
                last_child:      Cell::new(None),
                parent:          Cell::new(None),
                next_sibling:    Cell::new(None),
                prev_sibling:    Cell::new(None),
                content:         Cell::new(content),
                line:            0,
            })
        }
        #[cfg(feature = "c-abi")]
        {
            // Names dedup through the dict (pointer-equal across
            // duplicate tags); content stays in the arena (rarely
            // repeats).
            // Text / CData nodes pin `name` to the libxml2
            // `xmlStringText` / `xmlStringTextNoenc` statics — C
            // consumers (libxslt's xsltCopyText, lxml's smart-string
            // wrapping) compare against those by pointer to identify
            // text-kinds.  Any other `name` would silently break the
            // dispatch and surface as `Internal error in xsltCopyText`.
            let name_c = match kind {
                NodeKind::Text | NodeKind::CData => ArenaCStr::text_name(),
                _ if name.is_empty() => ArenaCStr::empty(),
                _ => self.intern_arena_cstr(name),
            };
            let content_c = content.map(|c| if c.is_empty() { ArenaCStr::empty() } else { self.alloc_arena_cstr(c) });
            self.bump.alloc(Node {
                _private:        Cell::new(std::ptr::null_mut()),
                kind,
                _pad_kind:       0,
                name:            name_c,
                first_child:     Cell::new(None),
                last_child:      Cell::new(None),
                parent:          Cell::new(None),
                next_sibling:    Cell::new(None),
                prev_sibling:    Cell::new(None),
                doc:             Cell::new(std::ptr::null_mut()),
                namespace:       Cell::new(None),
                content:         Cell::new(content_c),
                first_attribute: Cell::new(None),
                ns_def:          Cell::new(None),
                psvi:            Cell::new(std::ptr::null_mut()),
                line:            0,
                extra:           0,
                _pad_extra:      [0u8; 4],
                last_attribute:  Cell::new(None),
                full_line:       0,
            })
        }
    }

    pub fn new_element<'a>(&'a self, name: &'a str) -> &'a mut Node<'a> {
        self.new_node_impl(NodeKind::Element, name, None)
    }

    pub fn new_text<'a>(&'a self, content: &'a str) -> &'a mut Node<'a> {
        self.new_node_impl(NodeKind::Text, "", Some(content))
    }

    pub fn new_cdata<'a>(&'a self, content: &'a str) -> &'a mut Node<'a> {
        self.new_node_impl(NodeKind::CData, "", Some(content))
    }

    pub fn new_comment<'a>(&'a self, content: &'a str) -> &'a mut Node<'a> {
        self.new_node_impl(NodeKind::Comment, "", Some(content))
    }

    /// Build a processing-instruction node.  `content` is `None` for a
    /// PI with no data section (`<?foo?>`) and `Some` — possibly `""` —
    /// for one that has one (`<?foo?>` created via `xmlNewDocPI` with an
    /// empty string).  The distinction is libxml2's NULL-vs-empty
    /// `content` and governs the trailing space the serializer emits.
    pub fn new_pi<'a>(&'a self, target: &'a str, content: Option<&'a str>) -> &'a mut Node<'a> {
        self.new_node_impl(NodeKind::Pi, target, content)
    }

    /// Build a DTD internal-subset declaration node.  `content` is the
    /// raw markup declarations (each newline-terminated), emitted
    /// verbatim by the serializer inside the DOCTYPE's `[ … ]`.  Held
    /// as the single child of the internal-subset DTD node.
    pub fn new_dtd_decl<'a>(&'a self, content: &'a str) -> &'a mut Node<'a> {
        self.new_node_impl(NodeKind::DtdDecl, "", Some(content))
    }

    /// Allocate an empty document-fragment node.  Used by the libxml2
    /// compat shim's `xmlNewDocFragment`; the fragment is a transparent
    /// container that holds an ordered child list before being grafted
    /// into a real document subtree.
    pub fn new_fragment<'a>(&'a self) -> &'a mut Node<'a> {
        self.new_node_impl(NodeKind::DocumentFragment, "", None)
    }

    /// Build an unresolved entity-reference node.  `name` is the
    /// entity's NCName (e.g. `"foo"` for `&foo;`); `content` is the
    /// literal source form including the leading `&` and trailing
    /// `;`, which the serializer writes verbatim to round-trip the
    /// reference back to source.  Emitted by the parser only when
    /// `ParseOptions::resolve_entities` is `false`.
    pub fn new_entity_ref<'a>(&'a self, name: &'a str, content: &'a str) -> &'a mut Node<'a> {
        self.new_node_impl(NodeKind::EntityRef, name, Some(content))
    }

    pub fn new_attribute<'a>(&'a self, name: &'a str, value: &'a str) -> &'a mut Attribute<'a> {
        #[cfg(not(feature = "c-abi"))]
        {
            self.bump.alloc(Attribute {
                name,
                namespace: Cell::new(None),
                value,
                next:      Cell::new(None),
                prev:      Cell::new(None),
                parent:    Cell::new(None),
            })
        }
        #[cfg(feature = "c-abi")]
        {
            // Materialise a text-node child holding `value` so
            // libxml2 ABI consumers (libxslt, lxml's XPath etc.)
            // that read `attr->children->content` directly see the
            // attribute's value through the documented C path,
            // not just our sup-xml-only `value` tail field.
            //
            // Without this, libxslt walks attribute children
            // looking for the text node, finds NULL, and treats
            // every attribute value as empty — surfacing as
            // "could not compile select expression ''" errors when
            // libxslt parses a stylesheet through our shim.
            let text_child: &Node = self.new_text(value);
            self.bump.alloc(Attribute {
                _private:  Cell::new(std::ptr::null_mut()),
                kind:      NodeKind::Attribute,
                _pad_kind: 0,
                name:      self.intern_arena_cstr(name),
                children:  Cell::new(Some(text_child)),
                last:      Cell::new(Some(text_child)),
                parent:    Cell::new(None),
                next:      Cell::new(None),
                prev:      Cell::new(None),
                doc:       Cell::new(std::ptr::null_mut()),
                namespace: Cell::new(None),
                atype:     0,
                _pad_atype: 0,
                psvi:      Cell::new(std::ptr::null_mut()),
                value:     self.alloc_arena_cstr(value),
            })
        }
    }

    /// Chain `ns` onto `el`'s `ns_def` list in document order
    /// (last-in-list ordering — libxml2's `xmlNewNs` does the same).
    /// c-abi-only because the lean Namespace has no `next` field.
    #[cfg(feature = "c-abi")]
    pub fn append_ns_def<'a>(&'a self, el: &'a Node<'a>, ns: &'a Namespace<'a>) {
        match el.ns_def.get() {
            None => el.ns_def.set(Some(ns)),
            Some(head) => {
                // Walk to the tail of the chain and link there.
                let mut cur = head;
                while let Some(n) = cur.next.get() {
                    cur = n;
                }
                cur.next.set(Some(ns));
            }
        }
    }

    pub fn new_namespace<'a>(&'a self, prefix: Option<&'a str>, href: &'a str) -> &'a Namespace<'a> {
        #[cfg(not(feature = "c-abi"))]
        {
            self.bump.alloc(Namespace { prefix, href })
        }
        #[cfg(feature = "c-abi")]
        {
            // c-abi: stamp NUL-terminated copies into the arena so
            // `as_ptr()` is a valid `xmlChar*`.  The full libxml2-shape
            // _xmlNs needs `_private`, `next`, `kind` slots populated
            // too — `next` is None at creation and gets stitched up
            // by the parser when chaining onto `node->ns_def`.
            // Namespace URI + prefix go through the dict — XML
            // commonly has many elements sharing the same xmlns,
            // and pointer-equal namespaces let consumers do
            // O(1) "same namespace?" checks across the tree.
            let href_c = self.intern_arena_cstr(href);
            let prefix_c = prefix.map(|p| self.intern_arena_cstr(p));
            self.bump.alloc(Namespace {
                next:     Cell::new(None),
                kind:     18,  // XML_LOCAL_NAMESPACE
                _pad_kind: 0,
                href:     href_c,
                prefix:   prefix_c,
                _private: Cell::new(std::ptr::null_mut()),
                context:  Cell::new(std::ptr::null_mut()),
            })
        }
    }

    /// Allocate a NUL-terminated copy of `s` in the arena, returning
    /// an [`ArenaCStr`] pointing at the start byte.  C-ABI-only.
    ///
    /// Used for *values* — text content, attribute values, etc. —
    /// which are typically unique per occurrence and don't benefit
    /// from dedup.  For *names* (element / attribute / namespace
    /// names) prefer [`intern_arena_cstr`](Self::intern_arena_cstr)
    /// so consumers get stable pointer-equality across identical
    /// names without paying repeated arena copies.
    #[cfg(feature = "c-abi")]
    fn alloc_arena_cstr<'a>(&'a self, s: &str) -> ArenaCStr<'a> {
        // Allocate len + 1 bytes, copy in s, NUL the last byte.
        let bytes = s.as_bytes();
        let dst: &mut [u8] = self.bump.alloc_slice_fill_with(bytes.len() + 1, |i| {
            if i < bytes.len() { bytes[i] } else { 0 }
        });
        // SAFETY: dst is bytes.len()+1 long with trailing 0; valid UTF-8
        // by construction (input was &str).
        unsafe { ArenaCStr::from_raw(dst.as_ptr()) }
    }

    /// Intern `s` through the per-document name dict, returning a
    /// canonical [`ArenaCStr`] that is byte-equal for any identical
    /// input.  Use this for any string a downstream consumer might
    /// pointer-compare (element / attribute / namespace names) or
    /// might pass to a "free if not dict-owned" path.  C-ABI-only.
    ///
    /// Repeated calls with the same bytes return the same pointer
    /// in O(1) amortised; on a miss the dict pays one allocation.
    /// Strings live until the owning document drops.
    #[cfg(feature = "c-abi")]
    fn intern_arena_cstr<'a>(&'a self, s: &str) -> ArenaCStr<'a> {
        // SAFETY: self.dict is a refcount-managed pointer owned by
        // this struct for as long as &self is live; we hold a
        // reference (drop releases it).  Dict::intern_str returns a
        // pointer into one of the dict's own Box<[u8]>s, valid for
        // the dict's lifetime.
        let ptr = unsafe { (*self.dict).intern_str(s) };
        unsafe { ArenaCStr::from_raw(ptr) }
    }

    /// Append `child` as the last child of `parent`.  Sets `parent`/sibling
    /// pointers consistently.
    ///
    /// # Panics
    ///
    /// Panics if `child` is already attached somewhere (its `parent` is not
    /// `None`) — callers should detach first.  We treat double-attach as a
    /// builder bug, not a recoverable error.
    pub fn append_child<'a>(&'a self, parent: &'a Node<'a>, child: &'a Node<'a>) {
        debug_assert!(child.parent.get().is_none(), "append_child: child is already attached");
        child.parent.set(Some(parent));
        match parent.last_child.get() {
            None => {
                parent.first_child.set(Some(child));
                parent.last_child.set(Some(child));
            }
            Some(prev_last) => {
                prev_last.next_sibling.set(Some(child));
                child.prev_sibling.set(Some(prev_last));
                parent.last_child.set(Some(child));
            }
        }
    }

    /// Append `attr` to the end of `element`'s attribute list.  Sets the
    /// attribute's `parent` and links siblings.
    pub fn append_attribute<'a>(&'a self, element: &'a Node<'a>, attr: &'a Attribute<'a>) {
        debug_assert!(element.is_element(), "append_attribute: not an element");
        debug_assert!(attr.parent.get().is_none(), "append_attribute: attr already attached");
        attr.parent.set(Some(element));
        match element.last_attribute.get() {
            None => {
                element.first_attribute.set(Some(attr));
                element.last_attribute.set(Some(attr));
            }
            Some(prev_last) => {
                prev_last.next.set(Some(attr));
                attr.prev.set(Some(prev_last));
                element.last_attribute.set(Some(attr));
            }
        }
    }

    /// Detach `child` from its current parent (no-op if already detached).
    /// Repairs sibling links on the parent's child list.
    pub fn detach<'a>(&'a self, child: &'a Node<'a>) {
        let Some(parent) = child.parent.get() else { return; };
        let prev = child.prev_sibling.get();
        let next = child.next_sibling.get();
        match prev {
            Some(p) => p.next_sibling.set(next),
            None    => parent.first_child.set(next),
        }
        match next {
            Some(n) => n.prev_sibling.set(prev),
            None    => parent.last_child.set(prev),
        }
        child.parent.set(None);
        child.prev_sibling.set(None);
        child.next_sibling.set(None);
    }

    /// Record an orphan leaf (comment / PI) that appeared in the
    /// document's prolog (before the root element opened).  Linked
    /// as `root.prev_sibling` chain on [`build`](Self::build).
    pub fn attach_prolog_orphan<'a>(&'a self, node: &'a Node<'a>) {
        let p: *const Node<'static> = node as *const _ as *const Node<'static>;
        self.prolog_orphans.borrow_mut().push(p);
    }

    /// Record an orphan leaf (comment / PI) that appeared in the
    /// document's epilogue (after `</root>`).  Linked as
    /// `root.next_sibling` chain on [`build`](Self::build).
    pub fn attach_epilogue_orphan<'a>(&'a self, node: &'a Node<'a>) {
        let p: *const Node<'static> = node as *const _ as *const Node<'static>;
        self.epilogue_orphans.borrow_mut().push(p);
    }

    /// Mark `root` as the document's root element.  Call before [`build`](Self::build).
    /// The borrow on `&self` only lives for this call, so subsequent
    /// `build()` can freely consume the builder.
    pub fn set_root<'a>(&'a self, root: &'a Node<'a>) {
        // SAFETY: erasing 'a → 'static is sound because:
        //   - `root` borrows from `self.bump`, which we own.
        //   - On `build()` we move `self.bump` into the `Document`, where it
        //     remains pinned in heap memory.  The pointer stays valid.
        //   - The Cell type prevents anyone from reading the raw pointer with
        //     a longer lifetime than the eventual `Document::root()` call,
        //     which re-binds to `&self`.
        let p: *const Node<'static> = root as *const _ as *const Node<'static>;
        self.root.set(p);
    }

    /// Finalize the build.  Must be called after [`set_root`](Self::set_root).
    ///
    /// # Panics
    ///
    /// Panics if `set_root` was never called.
    pub fn build(self) -> Document {
        let root = self.root.get();
        assert!(!root.is_null(),
            "DocumentBuilder::build called without set_root — call set_root(node) first");
        // Wire any prolog/epilogue orphan leaves (comments/PIs from
        // outside the root) as siblings of `root` so the resulting
        // document mirrors libxml2's children list: prolog → root →
        // epilogue.  No-op for the common case (no out-of-root
        // content).
        //
        // SAFETY: each entry was registered via `attach_orphan` with a
        // pointer into this builder's arena, which moves into the
        // returned Document and stays valid for its lifetime.
        let first_sibling: *const Node<'static> = unsafe {
            link_doc_level_orphans(
                root,
                &self.prolog_orphans.borrow(),
                &self.epilogue_orphans.borrow(),
            )
        };
        // We have a `Drop` impl on `DocumentBuilder` (to free the leaked
        // source bytes when build() is *not* called).  That Drop blocks
        // by-value move-out of fields, so we use the standard
        // ManuallyDrop + ptr::read pattern: suppress Drop, then move each
        // owned field out by raw pointer.
        let s = std::mem::ManuallyDrop::new(self);
        let source_ptr = s.source_ptr.get();
        let source_len = s.source_len.get();
        let standalone = s.standalone.get();
        // SAFETY: ManuallyDrop suppresses the destructor; each owned field
        // is read exactly once and ownership is transferred either to a
        // local binding (then into the returned Document) or explicitly
        // dropped here.  Final destination is the returned Document, which
        // now owns these fields (including the source buffer, freed by
        // Document's Drop).
        let bump          = unsafe { std::ptr::read(&s.bump) };
        let version       = unsafe { std::ptr::read(&s.version) }.into_inner();
        let encoding      = unsafe { std::ptr::read(&s.encoding) }.into_inner();
        let html_metadata = unsafe { std::ptr::read(&s.html_metadata) }.into_inner();
        let base_url      = unsafe { std::ptr::read(&s.base_url) }.into_inner();
        // The orphan Vecs were consumed above by `link_doc_level_orphans`.
        // Their contents (the *const Node pointers) are now wired into the
        // document, but the Vec's heap-allocated buffer itself still needs
        // to be freed — ManuallyDrop suppresses the field-level drop, so
        // read them out into local bindings that drop at scope end.
        let _prolog_orphans:   std::cell::RefCell<Vec<*const Node<'static>>>
            = unsafe { std::ptr::read(&s.prolog_orphans) };
        let _epilogue_orphans: std::cell::RefCell<Vec<*const Node<'static>>>
            = unsafe { std::ptr::read(&s.epilogue_orphans) };
        // Transfer the refcounted dict pointer.  No add_ref needed
        // — the builder's reference becomes the document's.
        #[cfg(feature = "c-abi")]
        let dict          = s.dict;
        Document {
            bump,
            #[cfg(feature = "c-abi")]
            #[cfg(feature = "c-abi")]
            dict,
            root,
            first_sibling,
            version,
            encoding,
            standalone,
            html_metadata,
            source_ptr,
            source_len,
            unparsed_entities: std::sync::Arc::new(std::collections::HashMap::new()),
            id_attributes:     std::sync::Arc::new(std::collections::HashMap::new()),
            idref_attributes:  std::sync::Arc::new(std::collections::HashMap::new()),
            base_url,
        }
    }
}

/// Link prolog/epilogue orphan leaves as siblings of `root` and
/// return the first node in the resulting sibling chain (either the
/// first prolog node, or `root` if there is no prolog).
///
/// # Safety
///
/// `root` and every entry in `prolog` / `epilogue` must be valid
/// pointers into the same arena that the returned chain will live
/// in.  Lifetimes are erased to `'static` for storage; callers must
/// ensure the arena outlives all reads.
unsafe fn link_doc_level_orphans(
    root: *const Node<'static>,
    prolog: &[*const Node<'static>],
    epilogue: &[*const Node<'static>],
) -> *const Node<'static> {
    if prolog.is_empty() && epilogue.is_empty() {
        return root;
    }
    // SAFETY: `root` is a non-null pointer into the arena; the
    // returned reference borrows from a 'static-erased pointer but
    // is only used to thread sibling links here.
    let root_ref: &Node<'static> = unsafe { &*root };
    // Prolog: chain prev_sibling links so the first prolog entry
    // becomes the head, then ... → root.
    let mut prev_node: Option<&Node<'static>> = None;
    for &p in prolog {
        let n: &Node<'static> = unsafe { &*p };
        if let Some(pv) = prev_node {
            n.prev_sibling.set(Some(pv));
            pv.next_sibling.set(Some(n));
        }
        prev_node = Some(n);
    }
    if let Some(last_prolog) = prev_node {
        last_prolog.next_sibling.set(Some(root_ref));
        root_ref.prev_sibling.set(Some(last_prolog));
    }
    // Epilogue: chain next_sibling links after root.
    let mut prev_node = Some(root_ref);
    for &p in epilogue {
        let n: &Node<'static> = unsafe { &*p };
        if let Some(pv) = prev_node {
            pv.next_sibling.set(Some(n));
            n.prev_sibling.set(Some(pv));
        }
        prev_node = Some(n);
    }
    // First sibling is either the first prolog entry or root.
    if let Some(&first) = prolog.first() { first } else { root }
}

impl Default for DocumentBuilder {
    fn default() -> Self { Self::new() }
}

impl Drop for DocumentBuilder {
    fn drop(&mut self) {
        // If `build()` wasn't called, the leaked source bytes are still ours.
        // `free_source` is a no-op when the pointer is null (post-`build`).
        self.free_source();
        // Release our dict reference.  When `build()` is called the
        // dict pointer is transferred to the Document via `ptr::read`
        // and our drop only fires when `build` ISN'T called — so we
        // need to release here to balance the `new()` add-ref.
        //
        // build() uses ManuallyDrop, so our drop doesn't fire after
        // a successful build.
        #[cfg(feature = "c-abi")]
        unsafe {
            if !self.dict.is_null() {
                crate::dict::Dict::release(self.dict);
            }
        }
    }
}

// ── document (self-ref wrapper) ─────────────────────────────────────────────

/// An owned XML document with an arena-allocated tree.
///
/// An unparsed external general entity declared with an `NDATA`
/// annotation (XML 1.0 § 4.2.2): `<!ENTITY name SYSTEM "uri" NDATA n>`
/// or the `PUBLIC "fpi" "uri"` form.  Backs XSLT's
/// `unparsed-entity-uri()` and `unparsed-entity-public-id()`
/// (XSLT 1.0 § 12.4).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UnparsedEntity {
    /// The entity's SYSTEM identifier — the URI a non-XML processor
    /// would fetch.  Stored as declared; callers resolve it against
    /// the document's base URI.
    pub system_id: String,
    /// The entity's PUBLIC identifier (FPI), when declared with the
    /// `PUBLIC` form; `None` for `SYSTEM`-only declarations.
    pub public_id: Option<String>,
}

/// `Document` owns a [`Bump`] and a pointer to the root [`Node`] inside that
/// `Bump`.  The struct is self-referential and the unsafety is *contained* —
/// see the module-level docs for the safety argument.
///
/// Outside this type, all node references are bounded by `&self` so they
/// can never escape the `Document`.
pub struct Document {
    // SAFETY-RELEVANT FIELD ORDER: `root` is dropped before `bump` (Rust drops
    // fields in declaration order), but neither field needs Drop, so the order
    // is academic — bumpalo's Drop on `bump` walks its own chunks; `root` is
    // just a raw pointer.  Keeping the order anyway as a defensive measure.
    root: *const Node<'static>,
    /// First sibling in the document's top-level chain — equals
    /// `root` when the document has no prolog/epilogue content
    /// (the common case), or points to the first prolog comment /
    /// PI otherwise.  Walking `next_sibling` from here visits
    /// every document-level node in source order.
    ///
    /// Used by the C-ABI shim's `XmlDoc.children` field (which
    /// libxml2 consumers walk to find prolog + root + epilogue)
    /// and by the serializer's `Document` write path.
    first_sibling: *const Node<'static>,
    /// Refcounted heap allocation.  See [`DocumentBuilder::bump`] for the
    /// rationale.  Multiple documents created on the same thread
    /// share this same `Arc<Bump>` — node memory survives any
    /// individual doc's drop and the libxml2-style cross-doc graft
    /// (lxml's `_appendChild` / `moveNodeToDocument`) is safe by
    /// construction.
    bump: Arc<Bump>,
    /// Per-document name interner.  Element / attribute / namespace
    /// names point into the boxes this dict owns; pointer equality
    /// across identical names matters to C-ABI consumers that
    /// pointer-compare names or check "did this string come from
    /// the dict?" before freeing.
    ///
    /// Refcount-managed (see [`crate::dict::Dict`]) — the underlying
    /// dict may be shared with the parser context that produced this
    /// document.  Document owns one reference; drop releases it.
    #[cfg(feature = "c-abi")]
    dict: *mut crate::dict::Dict,
    /// XML declaration version (e.g. `"1.0"`).  Defaults to `"1.0"` when
    /// the document had no `<?xml ... ?>` declaration.
    pub version:    String,
    /// XML declaration encoding (e.g. `"UTF-8"`).  Defaults to `"UTF-8"`
    /// when the declaration omitted `encoding=…` or was absent entirely.
    pub encoding:   String,
    /// XML declaration `standalone="…"` value, or `None` when absent.
    pub standalone: Option<bool>,
    /// HTML-specific metadata when this document was produced by an HTML
    /// parser; `None` for XML documents.  Use [`Document::is_html`] to
    /// discriminate.
    pub html_metadata: Option<HtmlMeta>,
    /// Owned source bytes (raw pointer + length) that arena strings may
    /// point into via the parser's borrow-from-source optimization.  The
    /// buffer is heap-allocated via `Box::leak` at parse time; the
    /// `Document`'s `Drop` impl reclaims it.
    ///
    /// **Raw pointer storage is intentional**, not just paranoia.  Using
    /// `Pin<Box<[u8]>>` directly causes a Stacked Borrows violation when
    /// `DocumentBuilder::build` moves the `Box` field into the
    /// `Document` — the move performs a Unique retag of the parent,
    /// invalidating any `SharedReadOnly` borrows the parser handed out
    /// into the bytes.  Storing as a raw pointer (which is `Copy`) means
    /// the build transfer is a pointer copy, not a parent retag, so the
    /// borrows survive.  Verified clean under `cargo +nightly miri`.
    ///
    /// `source_ptr` is null when the parser ran in pure-copy mode
    /// (everything alloc_str'd into the bump and no source was stashed).
    source_ptr: *mut u8,
    source_len: usize,
    /// Unparsed external general entities declared in the DTD with
    /// an `NDATA` annotation (XML 1.0 § 4.2.2).  Keyed by entity
    /// name → SYSTEM identifier.  Populated by the parser when the
    /// document carries a `<!DOCTYPE>` with one or more
    /// `<!ENTITY name SYSTEM "uri" NDATA notation>` declarations;
    /// otherwise empty.  Surfaces through
    /// [`Document::unparsed_entity_uri`] for XSLT's
    /// `unparsed-entity-uri()` function (XSLT 1.0 § 12.4).  Wrapped
    /// in `Arc` so XSLT can share a cheap handle across the
    /// transform's lifetime without copying the map per-call.
    unparsed_entities: std::sync::Arc<std::collections::HashMap<String, UnparsedEntity>>,
    /// DTD-declared ID attribute typing.  Keyed by element name
    /// (the parent of the attribute), value is the list of attribute
    /// names typed as `ID` in that element's `<!ATTLIST>`.  Empty
    /// when the document had no DTD or no ID-typed declarations.
    /// Consulted by XPath 1.0 §4.1's `id()` function.
    id_attributes: std::sync::Arc<std::collections::HashMap<String, Vec<String>>>,
    /// DTD-derived IDREF/IDREFS-attribute map: element-name → attribute
    /// names declared `<!ATTLIST e a IDREF>` / `IDREFS`.  Empty when the
    /// document had no such declarations.  Consulted by XPath 2.0
    /// §14.5.5's `idref()` function.
    idref_attributes: std::sync::Arc<std::collections::HashMap<String, Vec<String>>>,
    /// URI the document was loaded from, when known — the value of
    /// [`ParseOptions::base_url`](crate) at parse time, or the URI a
    /// loader resolved for `doc()`/`document()`.  `None` for documents
    /// built in memory with no source URI.  Surfaces as the document
    /// node's base URI for XPath `fn:base-uri()` / `fn:document-uri()`
    /// (XPath 2.0 §2.5, §15.5.3): a source document's base URI is the
    /// URI it was retrieved from, independent of the stylesheet's own
    /// static base URI.
    base_url: Option<String>,
}

// SAFETY: `Node` and friends contain `Cell<&Node>` which is !Sync.
// Document is !Sync as a result.  `Arc<Bump>` is !Send because `Bump`
// is !Sync — auto-Send is blocked on that ground.  We re-assert Send
// here under the contract that callers serialize all access to the
// document (Python GIL on the lxml shim, single-threaded API on the
// Rust side).  Moving the Document moves the Arc<Bump>, whose
// underlying chunks stay at the same heap address; the root pointer
// remains valid.  The raw `source_ptr` is safe to send since the
// bytes it points at aren't shared.
unsafe impl Send for Document {}

impl Drop for Document {
    fn drop(&mut self) {
        // Drop order: Rust drops `bump` (an `Arc`) after this body
        // runs.  The arena only releases its bumpalo memory when
        // the last `Arc<Bump>` clone drops — in c-abi compat the
        // thread keeps a clone alive until thread exit, so per-doc
        // drops are just refcount decrements.
        //
        // Source bytes: reclaimed unconditionally here.  In c-abi
        // mode no node field references them post-parse (names
        // intern into the dict, content/values copy into the bump),
        // so freeing is safe regardless of who else holds the
        // arena.  In the lean build callers that graft nodes
        // across documents must keep the source alive themselves —
        // we don't try to track those references.
        if !self.source_ptr.is_null() {
            // SAFETY: `source_ptr` came from `Box::leak`'d
            // `Box<[u8]>` of length `source_len` (set in
            // `DocumentBuilder::set_source`), and ownership was
            // transferred to this `Document` by
            // `DocumentBuilder::build`.  No other code frees it.
            unsafe {
                let _: Box<[u8]> = Box::from_raw(
                    std::slice::from_raw_parts_mut(self.source_ptr, self.source_len)
                        as *mut [u8]
                );
            }
        }
        // Release our dict reference.  Other holders (the parser
        // context that originated it, the thread-local dict slot)
        // keep it alive; only the last release frees the interned
        // strings.
        #[cfg(feature = "c-abi")]
        unsafe {
            if !self.dict.is_null() {
                crate::dict::Dict::release(self.dict);
            }
        }
    }
}

impl Document {
    /// The document root.  Lifetime is bound to `&self`, so node references
    /// cannot outlive the `Document`.
    pub fn root<'a>(&'a self) -> &'a Node<'a> {
        // SAFETY: `self.root` points into `self.bump`, which we own.  The
        // returned reference is bounded by `&'a self`, so it cannot outlive
        // the `Document` — and therefore cannot outlive the `Bump`.  The
        // pointer was set via `DocumentBuilder::set_root` from a `&'b Node<'b>`
        // borrowed from `self.bump` while it lived in the builder; moving the
        // pinned `Bump` from the builder to `Document` did not relocate the
        // node (bumpalo allocations have stable addresses).
        unsafe { &*(self.root as *const Node<'a>) }
    }

    /// Approximate bytes of memory the document holds.  Includes every
    /// allocation made by the parser (nodes, attributes, interned-style
    /// strings, the source slice).  Useful for memory diagnostics.
    pub fn memory_bytes(&self) -> usize {
        self.bump.allocated_bytes()
    }

    /// SYSTEM identifier of the unparsed external general entity
    /// declared as `<!ENTITY name SYSTEM "uri" NDATA notation>` in
    /// the source document's DTD.  Returns `None` when no such
    /// entity is declared.  Backs XSLT 1.0 §12.4's
    /// `unparsed-entity-uri()` function.
    pub fn unparsed_entity_uri(&self, name: &str) -> Option<&str> {
        self.unparsed_entities.get(name).map(|e| e.system_id.as_str())
    }

    /// PUBLIC identifier (FPI) of the unparsed external general entity
    /// named `name`, when it was declared with the `PUBLIC` form.
    /// Backs XSLT 2.0 §16.6.3's `unparsed-entity-public-id()`.
    pub fn unparsed_entity_public_id(&self, name: &str) -> Option<&str> {
        self.unparsed_entities.get(name)
            .and_then(|e| e.public_id.as_deref())
    }

    /// String-value of a node previously allocated in *some* document's
    /// arena (typically this one) and reachable by raw pointer.  Used
    /// by foreign-pointer code paths (XPath's `ForeignNodeSet`, EXSLT
    /// `str:tokenize` result nodes) where the type system can't track
    /// the lifetime back to a `Document`.  Returns `""` for null.
    ///
    /// SAFETY-encapsulated: the unsafe deref is contained here.  The
    /// caller's contract is that `ptr` was minted by *some* live
    /// `Document::new_*` (or this crate's parser) and the underlying
    /// arena has not been freed.  Passing a stale or alien pointer
    /// is undefined behavior.
    pub fn node_string_value_by_ptr(ptr: *const Node<'static>) -> String {
        if ptr.is_null() { return String::new(); }
        // SAFETY: caller's contract — see method doc.
        let n: &Node<'_> = unsafe { &*(ptr as *const Node<'_>) };
        n.content().to_string()
    }

    /// Replace the text content of the node at `node_ptr` with
    /// `content` (allocated in this document's arena).
    /// SAFETY-encapsulated: the caller's contract is that
    /// `node_ptr` was minted by *this* document's `new_*` (so
    /// `Cell<…>::set` writes into our own arena's matching
    /// lifetime).  Mismatched docs are debug-asserted at the
    /// arena boundary.  Silently no-ops on null.
    pub fn set_node_text_content_by_ptr(&self, node_ptr: *const Node<'static>, content: &str) {
        if node_ptr.is_null() { return; }
        // SAFETY: caller's contract — `node_ptr` came from this doc's `new_*`.
        let node: &Node<'_> = unsafe { &*(node_ptr as *const Node<'_>) };
        self.set_node_text_content(node, content);
    }

    /// Replace the text content of `node` with `content` (allocated
    /// in this document's arena).  Safe wrapper around the
    /// `Cell<…>`-typed content field — handles both the lean
    /// (`Cell<&str>`) and the c-abi (`Cell<ArenaCStr>`) builds.
    /// `node` must be a `Text` / `CData` / `Comment` / `Pi` node
    /// that lives in *this* document's arena; mismatched docs
    /// will silently store a pointer that drops with the wrong
    /// arena (debug-asserted to catch misuse early).
    pub fn set_node_text_content<'a>(&'a self, node: &'a Node<'a>, content: &str) {
        #[cfg(not(feature = "c-abi"))]
        {
            let s: &'a str = self.bump.alloc_str(content);
            node.content.set(Some(s));
        }
        #[cfg(feature = "c-abi")]
        {
            let dst: &mut [u8] = self.bump.alloc_slice_fill_with(content.len() + 1, |i| {
                if i < content.len() { content.as_bytes()[i] } else { 0 }
            });
            // SAFETY: `dst` is a NUL-terminated UTF-8 slice owned by the
            // arena.  `ArenaCStr::from_raw` requires exactly that.
            let new_content = unsafe { ArenaCStr::from_raw(dst.as_ptr()) };
            node.content.set(Some(new_content));
        }
    }

    /// Borrow the full unparsed-entity map by reference.  Cheap to
    /// clone (it's an `Arc`); used by the XSLT engine to capture a
    /// snapshot for its function dispatcher.
    pub fn unparsed_entities(&self) -> &std::sync::Arc<std::collections::HashMap<String, UnparsedEntity>> {
        &self.unparsed_entities
    }

    /// Replace the unparsed-entity table.  Called by the parser
    /// after DTD ingestion; not part of the stable public surface.
    #[doc(hidden)]
    pub fn set_unparsed_entities(
        &mut self,
        map: std::collections::HashMap<String, UnparsedEntity>,
    ) {
        self.unparsed_entities = std::sync::Arc::new(map);
    }

    /// Borrow the DTD-derived ID-attribute map: element-name →
    /// list of attribute names declared with `<!ATTLIST e a ID>`.
    /// Empty when the document had no DTD-typed ID attributes.
    pub fn id_attributes(
        &self,
    ) -> &std::sync::Arc<std::collections::HashMap<String, Vec<String>>> {
        &self.id_attributes
    }

    /// Replace the ID-attribute map.  Called by the parser after DTD
    /// ingestion; not part of the stable public surface.
    #[doc(hidden)]
    pub fn set_id_attributes(
        &mut self,
        map: std::collections::HashMap<String, Vec<String>>,
    ) {
        self.id_attributes = std::sync::Arc::new(map);
    }

    /// Borrow the DTD-derived IDREF-attribute map: element-name → list
    /// of attribute names declared `<!ATTLIST e a IDREF>` / `IDREFS`.
    /// Empty when the document had no DTD-typed IDREF attributes.
    pub fn idref_attributes(
        &self,
    ) -> &std::sync::Arc<std::collections::HashMap<String, Vec<String>>> {
        &self.idref_attributes
    }

    /// Replace the IDREF-attribute map.  Called by the parser after DTD
    /// ingestion; not part of the stable public surface.
    #[doc(hidden)]
    pub fn set_idref_attributes(
        &mut self,
        map: std::collections::HashMap<String, Vec<String>>,
    ) {
        self.idref_attributes = std::sync::Arc::new(map);
    }

    /// URI the document was loaded from, if known.  See the
    /// [`base_url`](Self::base_url) field: this is the document node's
    /// base URI for `fn:base-uri()` / `fn:document-uri()`, distinct
    /// from any host stylesheet's static base URI.
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    /// Record the URI the document was loaded from.  Called by the
    /// parser / loader once the source URI is known; not part of the
    /// stable public surface.
    #[doc(hidden)]
    pub fn set_base_url(&mut self, uri: Option<String>) {
        self.base_url = uri;
    }

    /// First sibling in the document's top-level chain (prolog
    /// comment / PI, or root if none).  Walking `next_sibling`
    /// from here visits every document-level node.  Equivalent to
    /// libxml2's `xmlDoc.children`.
    pub fn first_sibling<'a>(&'a self) -> &'a Node<'a> {
        let p = if self.first_sibling.is_null() { self.root } else { self.first_sibling };
        // SAFETY: same invariants as [`root`](Self::root).
        unsafe { &*(p as *const Node<'a>) }
    }

    /// True when this document was produced by an HTML parser.
    pub fn is_html(&self) -> bool {
        self.html_metadata.is_some()
    }

    /// Re-point the document's root.  Used by mutation APIs that
    /// replace the root post-build (libxml2's `xmlDocSetRootElement`).
    ///
    /// # Safety
    ///
    /// `node` must be a valid arena-resident pointer inside `self.bump`,
    /// OR NULL.  The previous root pointer is dropped — its target
    /// remains allocated in the arena (leaked unless still reachable
    /// via the new tree).
    pub unsafe fn set_root_ptr(&mut self, node: *const Node<'static>) {
        self.root = node;
    }

    /// Set the document's first top-level node (the head of the
    /// document-level sibling chain — prolog comments/PIs precede the
    /// root element).  [`first_sibling`](Self::first_sibling) returns
    /// this when non-NULL, otherwise it falls back to
    /// [`root`](Self::root).
    ///
    /// # Safety
    ///
    /// `node` must be a valid arena-resident pointer, or NULL.
    pub unsafe fn set_first_sibling_ptr(&mut self, node: *const Node<'static>) {
        self.first_sibling = node;
    }

    /// Direct access to the document's bumpalo arena.
    ///
    /// Use to allocate new nodes/attributes/strings into the same
    /// arena that owns the existing tree.  The caller is responsible
    /// for tree-invariant maintenance — e.g. when linking a freshly
    /// allocated node into the tree via `append_child`, the new
    /// node's parent/sibling pointers must be wired consistently.
    pub fn bump(&self) -> &Bump {
        &self.bump
    }

    /// Clone this document's arena handle.  Used by the C-ABI shim to
    /// pin a foreign document's arena onto a destination when a node is
    /// grafted across documents (cross-thread), so the moved node's
    /// memory outlives a drop of its origin document.
    #[cfg(feature = "c-abi")]
    pub fn bump_arc(&self) -> Arc<Bump> {
        Arc::clone(&self.bump)
    }

    // ── post-parse mutation helpers ──────────────────────────────────
    //
    // These mirror the `DocumentBuilder::new_*` set so callers can
    // allocate additional nodes into an existing `Document`'s arena.
    // Same allocation semantics (bumpalo), same field-init dance.

    /// Allocate a fresh element node in the document's arena.  The
    /// returned node is detached — link it into the tree via
    /// [`append_child`](Self::append_child).
    pub fn new_element<'a>(&'a self, name: &'a str) -> &'a mut Node<'a> {
        self.alloc_node(NodeKind::Element, name, None)
    }

    /// Allocate a text node.
    pub fn new_text<'a>(&'a self, content: &'a str) -> &'a mut Node<'a> {
        self.alloc_node(NodeKind::Text, "", Some(content))
    }

    /// Allocate a CDATA section.
    pub fn new_cdata<'a>(&'a self, content: &'a str) -> &'a mut Node<'a> {
        self.alloc_node(NodeKind::CData, "", Some(content))
    }

    /// Allocate a comment node.
    pub fn new_comment<'a>(&'a self, content: &'a str) -> &'a mut Node<'a> {
        self.alloc_node(NodeKind::Comment, "", Some(content))
    }

    /// Allocate a processing-instruction node.  `content` is `None` for
    /// a PI with no data section, `Some` (possibly `""`) otherwise — see
    /// [`DocumentBuilder::new_pi`].
    pub fn new_pi<'a>(&'a self, target: &'a str, content: Option<&'a str>) -> &'a mut Node<'a> {
        self.alloc_node(NodeKind::Pi, target, content)
    }

    /// Allocate a DTD internal-subset declaration node (raw markup
    /// declarations, newline-terminated).  Mirror of
    /// [`DocumentBuilder::new_dtd_decl`] for post-parse construction.
    pub fn new_dtd_decl<'a>(&'a self, content: &'a str) -> &'a mut Node<'a> {
        self.alloc_node(NodeKind::DtdDecl, "", Some(content))
    }

    /// Allocate an empty document-fragment node.  Mirror of
    /// [`DocumentBuilder::new_fragment`] for post-parse construction.
    pub fn new_fragment<'a>(&'a self) -> &'a mut Node<'a> {
        self.alloc_node(NodeKind::DocumentFragment, "", None)
    }

    /// Allocate an unresolved-entity-reference node.  See
    /// [`DocumentBuilder::new_entity_ref`] for semantics.
    pub fn new_entity_ref<'a>(&'a self, name: &'a str, content: &'a str) -> &'a mut Node<'a> {
        self.alloc_node(NodeKind::EntityRef, name, Some(content))
    }

    /// Deep-copy a subtree from another arena into this document.
    ///
    /// Despite the name (chosen for familiarity with DOM's `adoptNode`),
    /// this is a **copy**, not a move — the source subtree stays in its
    /// original arena.  Arenas don't release individual allocations, so
    /// a true move isn't representable; in practice consumers either
    /// drop the source document afterward (handing ownership semantics)
    /// or keep both copies live (template-and-reuse semantics).
    ///
    /// Walks the source subtree depth-first.  Each foreign element,
    /// text, CDATA, comment, PI, entity-reference, or document-fragment
    /// node gets a fresh allocation in `self`'s arena.  Attributes are
    /// copied alongside their owning element.
    ///
    /// **Namespaces are not yet copied** — element `namespace` and
    /// `ns_def` pointers on the result are left `None`.  Callers that
    /// need namespace-aware adoption should set them up after the fact
    /// via `bump_new_namespace` + `attach_ns_def` (c-abi build).  A
    /// future revision will resolve namespaces by URI against the
    /// target document's existing declarations.
    ///
    /// Returns a fresh detached node — link it into the tree via
    /// [`append_child`](Self::append_child).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// # // ignored: requires building two docs end-to-end
    /// let scratch = Document::new();
    /// let template = scratch.new_element("metadata");
    /// // ... build out template subtree ...
    ///
    /// let real_doc = parse_str("<root/>", &ParseOptions::default()).unwrap();
    /// let adopted = real_doc.adopt_subtree(template);
    /// real_doc.append_child(real_doc.root(), adopted);
    /// ```
    pub fn adopt_subtree<'a>(&'a self, foreign: &Node<'_>) -> &'a Node<'a> {
        self.adopt_node_inner(foreign)
    }

    fn adopt_node_inner<'a>(&'a self, src: &Node<'_>) -> &'a Node<'a> {
        let copy: &Node<'a> = match src.kind {
            NodeKind::Element => {
                let name = self.bump().alloc_str(src.name());
                let new_el = self.new_element(name);
                for attr in src.attributes() {
                    let aname = self.bump().alloc_str(attr.name());
                    let aval  = self.bump().alloc_str(attr.value());
                    let new_attr = self.new_attribute(aname, aval);
                    self.append_attribute(new_el, new_attr);
                }
                new_el
            }
            NodeKind::Text => {
                let content = self.bump().alloc_str(src.content());
                self.new_text(content)
            }
            NodeKind::CData => {
                let content = self.bump().alloc_str(src.content());
                self.new_cdata(content)
            }
            NodeKind::Comment => {
                let content = self.bump().alloc_str(src.content());
                self.new_comment(content)
            }
            NodeKind::Pi => {
                let name    = self.bump().alloc_str(src.name());
                let content = src.content_opt().map(|c| &*self.bump().alloc_str(c));
                self.new_pi(name, content)
            }
            NodeKind::EntityRef => {
                let name    = self.bump().alloc_str(src.name());
                let content = self.bump().alloc_str(src.content());
                self.new_entity_ref(name, content)
            }
            NodeKind::DtdDecl => {
                let content = self.bump().alloc_str(src.content());
                self.new_dtd_decl(content)
            }
            NodeKind::DocumentFragment => self.new_fragment(),
            // c-abi-only discriminants — these never appear as a real
            // node-kind on a Node<'_> the caller could hold.
            NodeKind::Attribute => unreachable!(
                "adopt_subtree: NodeKind::Attribute is not a Node — use new_attribute directly"
            ),
            NodeKind::Document  => unreachable!(
                "adopt_subtree: NodeKind::Document marker never appears on a Node — pass the root element instead"
            ),
            NodeKind::Dtd => unreachable!(
                "adopt_subtree: NodeKind::Dtd is a compat-shim sibling node (xmlDtd), never an arena Node reached by a subtree copy"
            ),
        };
        // Recurse into children for container kinds.
        if matches!(src.kind, NodeKind::Element | NodeKind::DocumentFragment) {
            for child in src.children() {
                let child_copy = self.adopt_node_inner(child);
                self.append_child(copy, child_copy);
            }
        }
        copy
    }

    /// Allocate an attribute (detached — link via
    /// [`append_attribute`](Self::append_attribute)).
    pub fn new_attribute<'a>(&'a self, name: &'a str, value: &'a str) -> &'a mut Attribute<'a> {
        #[cfg(not(feature = "c-abi"))]
        {
            self.bump.alloc(Attribute {
                name, value,
                namespace: Cell::new(None),
                next: Cell::new(None),
                prev: Cell::new(None),
                parent: Cell::new(None),
            })
        }
        #[cfg(feature = "c-abi")]
        {
            // See DocumentBuilder::new_attribute for the
            // "attribute child text-node" rationale — same
            // requirement applies to attributes allocated
            // post-build via the Document mutation API.
            let text_child: &Node = self.new_text(value);
            // Intern the name through the dict (as DocumentBuilder and
            // element allocation do): consumers that match attributes by
            // dict-canonical pointer — lxml's _MultiTagMatcher behind
            // strip_attributes / objectify.deannotate, and objectify's
            // own child lookup — only match when post-build attributes
            // carry the same canonical name pointer as parsed ones.
            let name_c  = self.intern_arena_cstr(name);
            let value_c = self.alloc_arena_cstr(value);
            self.bump.alloc(Attribute {
                _private:  Cell::new(std::ptr::null_mut()),
                kind:      NodeKind::Attribute,
                _pad_kind: 0,
                name:      name_c,
                children:  Cell::new(Some(text_child)),
                last:      Cell::new(Some(text_child)),
                parent:    Cell::new(None),
                next:      Cell::new(None),
                prev:      Cell::new(None),
                doc:       Cell::new(std::ptr::null_mut()),
                namespace: Cell::new(None),
                atype:     0,
                _pad_atype: 0,
                psvi:      Cell::new(std::ptr::null_mut()),
                value:     value_c,
            })
        }
    }

    /// Link `child` as the last child of `parent`.  Updates
    /// parent/sibling pointers consistently.
    pub fn append_child<'a>(&'a self, parent: &'a Node<'a>, child: &'a Node<'a>) {
        debug_assert!(child.parent.get().is_none(),
            "Document::append_child: child is already attached");
        child.parent.set(Some(parent));
        match parent.last_child.get() {
            None => {
                parent.first_child.set(Some(child));
                parent.last_child.set(Some(child));
            }
            Some(last) => {
                last.next_sibling.set(Some(child));
                child.prev_sibling.set(Some(last));
                parent.last_child.set(Some(child));
            }
        }
    }

    /// Allocate a namespace in this document's arena.  c-abi-only:
    /// the lean build's `Namespace` doesn't carry the chain pointer
    /// we'd need for tree-attached usage.
    #[cfg(feature = "c-abi")]
    pub fn bump_new_namespace<'a>(
        &'a self,
        prefix: Option<&'a str>,
        href:   &'a str,
    ) -> &'a Namespace<'a> {
        let href_c = self.alloc_arena_cstr(href);
        let prefix_c = prefix.map(|p| self.alloc_arena_cstr(p));
        self.bump.alloc(Namespace {
            next:      Cell::new(None),
            kind:      18, // XML_LOCAL_NAMESPACE
            _pad_kind: 0,
            href:      href_c,
            prefix:    prefix_c,
            _private:  Cell::new(std::ptr::null_mut()),
            context:   Cell::new(std::ptr::null_mut()),
        })
    }

    /// Append `ns` to `element`'s `ns_def` chain.  Used by the
    /// mutation API after creating a fresh namespace via
    /// [`bump_new_namespace`].
    #[cfg(feature = "c-abi")]
    pub fn attach_ns_def<'a>(&'a self, element: &'a Node<'a>, ns: &'a Namespace<'a>) {
        match element.ns_def.get() {
            None => element.ns_def.set(Some(ns)),
            Some(head) => {
                // Walk to the tail and link.
                let mut cur = head;
                while let Some(n) = cur.next.get() {
                    cur = n;
                }
                cur.next.set(Some(ns));
            }
        }
    }

    /// Link `attr` as the last attribute on `element`.
    pub fn append_attribute<'a>(&'a self, element: &'a Node<'a>, attr: &'a Attribute<'a>) {
        debug_assert!(element.is_element(),
            "Document::append_attribute: not an element");
        debug_assert!(attr.parent.get().is_none(),
            "Document::append_attribute: attr already attached");
        attr.parent.set(Some(element));
        match element.last_attribute.get() {
            None => {
                element.first_attribute.set(Some(attr));
                element.last_attribute.set(Some(attr));
            }
            Some(last) => {
                last.next.set(Some(attr));
                attr.prev.set(Some(last));
                element.last_attribute.set(Some(attr));
            }
        }
    }

    fn alloc_node<'a>(
        &'a self,
        kind: NodeKind,
        name: &'a str,
        content: Option<&'a str>,
    ) -> &'a mut Node<'a> {
        #[cfg(not(feature = "c-abi"))]
        {
            self.bump.alloc(Node {
                kind,
                name,
                namespace:       Cell::new(None),
                first_attribute: Cell::new(None),
                last_attribute:  Cell::new(None),
                first_child:     Cell::new(None),
                last_child:      Cell::new(None),
                parent:          Cell::new(None),
                next_sibling:    Cell::new(None),
                prev_sibling:    Cell::new(None),
                content:         Cell::new(content),
                line:            0,
            })
        }
        #[cfg(feature = "c-abi")]
        {
            // Names dedup through the dict (pointer-equal across
            // duplicate tags); content stays in the arena (rarely
            // repeats).
            // Text / CData nodes pin `name` to the libxml2
            // `xmlStringText` / `xmlStringTextNoenc` statics — C
            // consumers (libxslt's xsltCopyText, lxml's smart-string
            // wrapping) compare against those by pointer to identify
            // text-kinds.  Any other `name` would silently break the
            // dispatch and surface as `Internal error in xsltCopyText`.
            let name_c = match kind {
                NodeKind::Text | NodeKind::CData => ArenaCStr::text_name(),
                _ if name.is_empty() => ArenaCStr::empty(),
                _ => self.intern_arena_cstr(name),
            };
            let content_c = content.map(|c| if c.is_empty() { ArenaCStr::empty() } else { self.alloc_arena_cstr(c) });
            self.bump.alloc(Node {
                _private:        Cell::new(std::ptr::null_mut()),
                kind,
                _pad_kind:       0,
                name:            name_c,
                first_child:     Cell::new(None),
                last_child:      Cell::new(None),
                parent:          Cell::new(None),
                next_sibling:    Cell::new(None),
                prev_sibling:    Cell::new(None),
                doc:             Cell::new(std::ptr::null_mut()),
                namespace:       Cell::new(None),
                content:         Cell::new(content_c),
                first_attribute: Cell::new(None),
                ns_def:          Cell::new(None),
                psvi:            Cell::new(std::ptr::null_mut()),
                line:            0,
                extra:           0,
                _pad_extra:      [0u8; 4],
                last_attribute:  Cell::new(None),
                full_line:       0,
            })
        }
    }

    /// Internal — allocate a NUL-terminated copy of `s` in this
    /// document's bump arena, returning a c-abi-shaped `ArenaCStr`.
    /// Used for values (content / attribute values); for names
    /// prefer [`intern_arena_cstr`](Self::intern_arena_cstr).
    #[cfg(feature = "c-abi")]
    fn alloc_arena_cstr<'a>(&'a self, s: &str) -> ArenaCStr<'a> {
        let bytes = s.as_bytes();
        let dst: &mut [u8] = self.bump.alloc_slice_fill_with(bytes.len() + 1, |i| {
            if i < bytes.len() { bytes[i] } else { 0 }
        });
        // SAFETY: dst is bytes.len()+1 long with trailing 0; valid UTF-8.
        unsafe { ArenaCStr::from_raw(dst.as_ptr()) }
    }

    /// Intern `s` through the document's name dict; same semantics
    /// as [`DocumentBuilder::intern_arena_cstr`].  Used by post-parse
    /// mutation paths that need name pointers compatible with the
    /// parser-built tree.
    #[cfg(feature = "c-abi")]
    fn intern_arena_cstr<'a>(&'a self, s: &str) -> ArenaCStr<'a> {
        // SAFETY: self.dict is refcount-managed by this Document for
        // as long as &self is live.
        let ptr = unsafe { (*self.dict).intern_str(s) };
        unsafe { ArenaCStr::from_raw(ptr) }
    }

    /// Raw pointer to the document's name dict.  The dict is
    /// refcount-managed; callers wanting to retain a separate
    /// reference must invoke
    /// [`crate::dict::Dict::add_ref`] explicitly.
    #[cfg(feature = "c-abi")]
    pub fn dict_ptr(&self) -> *mut crate::dict::Dict {
        self.dict
    }
}

impl std::fmt::Debug for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Document")
            .field("root", self.root())
            .field("memory_bytes", &self.memory_bytes())
            .finish()
    }
}

// ── libxml2-shape document wrapper (c-abi only) ─────────────────────────────
//
// `XmlDoc` is the byte-exact mirror of libxml2's `_xmlDoc`.  Allocated on
// the heap by [`Document::into_xml_doc`]; consumed by [`XmlDoc::free`].
// The pointer returned to C callers IS the address of the libxml2 ABI
// window (the struct's first field is at offset 0 of the heap allocation,
// so `*mut XmlDoc` == address of `_private`).
//
// `_doc` lives at the *tail* of the struct (past the 176-byte ABI window).
// It owns the arena and source bytes that every pointer in the header
// reaches into; dropping the Box drops `_doc` last, which is fine because
// no one reads the header fields after free.

/// libxml2-shape `xmlDoc`.  Byte-exact match with `_xmlDoc` (64-bit) for
/// the public ABI window (offsets 0..176).  The `_doc` tail field is
/// sup-xml-only — it owns the arena that backs every pointer in the
/// header — and never appears in the ABI contract.
///
/// Construction:
///   - [`Document::into_xml_doc`] consumes a `Document` and returns a
///     `*mut XmlDoc`.  Caller owns the allocation.
///   - [`XmlDoc::free`] reclaims a `*mut XmlDoc`, dropping the embedded
///     arena.  Idempotent on NULL.
///
/// All pointers in the header (`children`, `version`, `encoding`, etc.)
/// reach into `_doc`'s arena.  They become dangling at `free` time; no
/// one reads the header after that.
#[cfg(feature = "c-abi")]
#[repr(C)]
pub struct XmlDoc {
    pub _private:    Cell<*mut std::os::raw::c_void>,            //   0
    pub kind:        NodeKind,                                   //   8 (XML_DOCUMENT_NODE = 9)
    _pad_kind:       u32,                                        //  12
    /// libxml2 stores the doc's URL here as `char*`.  We populate it
    /// from the `url` argument to `xmlReadMemory` when provided; NULL
    /// otherwise.  Note: this is `char*` not `xmlChar*` (a libxml2
    /// quirk — every other string in xmlDoc is `xmlChar*`).
    pub name:        *const std::os::raw::c_char,                //  16
    /// First child (typically the root element; may also be a comment
    /// or PI that precedes/follows the root element).
    pub children:    Cell<*mut Node<'static>>,                   //  24
    pub last:        Cell<*mut Node<'static>>,                   //  32
    /// Always NULL for documents (docs have no parent).
    pub parent:      Cell<*mut Node<'static>>,                   //  40
    /// `next` / `prev` chain — used when a doc is linked into a
    /// container (libxslt does this).  We don't link our docs; NULL.
    pub next:        Cell<*mut XmlDoc>,                          //  48
    pub prev:        Cell<*mut XmlDoc>,                          //  56
    /// libxml2 sets `doc` to point at the doc itself.  We mirror that
    /// after heap-allocation (see [`Document::into_xml_doc`]).
    pub doc:         Cell<*mut XmlDoc>,                          //  64
    /// libxml2 compression level (gzip).  Always 0 for us — we don't
    /// compress output.
    pub compression: i32,                                        //  72
    /// `standalone` declaration: -2 (no decl), -1 (yes), 0 (no), 1 (yes).
    /// libxml2's quirky tri-state encoding; we mirror it.
    pub standalone:  i32,                                        //  76
    pub int_subset:  *mut std::os::raw::c_void,                  //  80
    pub ext_subset:  *mut std::os::raw::c_void,                  //  88
    /// Chain of "global" ns declarations not attached to any element —
    /// rare in modern XML, kept for ABI fidelity; we never populate it.
    pub old_ns:      *mut std::os::raw::c_void,                  //  96
    pub version:     ArenaCStr<'static>,                         // 104
    pub encoding:    ArenaCStr<'static>,                         // 112
    pub ids:         *mut std::os::raw::c_void,                  // 120
    pub refs:        *mut std::os::raw::c_void,                  // 128
    pub url:         *const std::os::raw::c_char,                // 136
    pub charset:     i32,                                        // 144
    _pad_charset:    u32,                                        // 148
    pub dict:        *mut std::os::raw::c_void,                  // 152
    pub psvi:        *mut std::os::raw::c_void,                  // 160
    pub parse_flags: i32,                                        // 168
    pub properties:  i32,                                        // 172
    // ── sup-xml-only tail (past the 176-byte ABI window) ──
    /// The Rust [`Document`] that owns every arena allocation reached
    /// by the header pointers above.  Stored as the last field so its
    /// offset doesn't pin the ABI.  Dropped last (declaration order),
    /// at which point the arena and source bytes are released.
    pub _doc: std::mem::ManuallyDrop<Document>,
}

#[cfg(feature = "c-abi")]
const _: () = {
    use std::mem::offset_of;
    assert!(offset_of!(XmlDoc, _private)    ==   0, "XmlDoc::_private @ 0");
    assert!(offset_of!(XmlDoc, kind)        ==   8, "XmlDoc::kind @ 8");
    assert!(offset_of!(XmlDoc, name)        ==  16, "XmlDoc::name @ 16");
    assert!(offset_of!(XmlDoc, children)    ==  24, "XmlDoc::children @ 24");
    assert!(offset_of!(XmlDoc, last)        ==  32, "XmlDoc::last @ 32");
    assert!(offset_of!(XmlDoc, parent)      ==  40, "XmlDoc::parent @ 40");
    assert!(offset_of!(XmlDoc, next)        ==  48, "XmlDoc::next @ 48");
    assert!(offset_of!(XmlDoc, prev)        ==  56, "XmlDoc::prev @ 56");
    assert!(offset_of!(XmlDoc, doc)         ==  64, "XmlDoc::doc @ 64");
    assert!(offset_of!(XmlDoc, compression) ==  72, "XmlDoc::compression @ 72");
    assert!(offset_of!(XmlDoc, standalone)  ==  76, "XmlDoc::standalone @ 76");
    assert!(offset_of!(XmlDoc, int_subset)  ==  80, "XmlDoc::int_subset @ 80");
    assert!(offset_of!(XmlDoc, ext_subset)  ==  88, "XmlDoc::ext_subset @ 88");
    assert!(offset_of!(XmlDoc, old_ns)      ==  96, "XmlDoc::old_ns @ 96");
    assert!(offset_of!(XmlDoc, version)     == 104, "XmlDoc::version @ 104");
    assert!(offset_of!(XmlDoc, encoding)    == 112, "XmlDoc::encoding @ 112");
    assert!(offset_of!(XmlDoc, ids)         == 120, "XmlDoc::ids @ 120");
    assert!(offset_of!(XmlDoc, refs)        == 128, "XmlDoc::refs @ 128");
    assert!(offset_of!(XmlDoc, url)         == 136, "XmlDoc::url @ 136");
    assert!(offset_of!(XmlDoc, charset)     == 144, "XmlDoc::charset @ 144");
    assert!(offset_of!(XmlDoc, dict)        == 152, "XmlDoc::dict @ 152");
    assert!(offset_of!(XmlDoc, psvi)        == 160, "XmlDoc::psvi @ 160");
    assert!(offset_of!(XmlDoc, parse_flags) == 168, "XmlDoc::parse_flags @ 168");
    assert!(offset_of!(XmlDoc, properties)  == 172, "XmlDoc::properties @ 172");
    // The libxml2 ABI window ends at offset 176.  Our `_doc` tail starts
    // there or after (Rust may insert padding before `ManuallyDrop<Document>`
    // depending on Document's alignment).
    assert!(offset_of!(XmlDoc, _doc)        >= 176, "XmlDoc tail starts after ABI window");
};

#[cfg(feature = "c-abi")]
impl Document {
    /// Consume `self` and produce a heap-allocated libxml2-shape
    /// [`XmlDoc`].  The returned pointer is what `xmlReadMemory`-style
    /// FFI entry points hand back to C callers.  Caller takes ownership;
    /// reclaim via [`XmlDoc::free`].
    ///
    /// All pointers in the resulting [`XmlDoc`] reach into the same arena
    /// that this `Document` owns — `_doc` keeps that arena alive for the
    /// lifetime of the heap allocation.
    pub fn into_xml_doc(self) -> *mut XmlDoc {
        // Allocate arena strings for version/encoding via the embedded
        // Document's bump.  These slots in libxml2's xmlDoc are
        // `xmlChar*` (NUL-terminated UTF-8); ArenaCStr matches that.
        let version_str = self.version.clone();
        let encoding_str = self.encoding.clone();
        // libxml2 uses -1 (not -2) when the XML declaration carried no
        // `standalone` attribute (or there was no declaration): xmlNewDoc
        // initialises the field to -1, and consumers treat -1 as
        // "unspecified" (lxml's `isstandalone` maps -1 -> None, 1 -> True,
        // and everything else, including 0, -> False).
        let standalone_i32 = match self.standalone {
            None        => -1,
            Some(true)  =>  1,
            Some(false) =>  0,
        };
        // First-child pointer: equals the first prolog sibling
        // (comment / PI before the root) when present, otherwise
        // the root element.  libxml2 consumers walk `xmlDoc.children`
        // expecting this prolog-first layout.
        let root_ptr: *mut Node<'static> =
            self.root as *const Node<'static> as *mut Node<'static>;
        let first_child_ptr: *mut Node<'static> = if self.first_sibling.is_null() {
            root_ptr
        } else {
            self.first_sibling as *const Node<'static> as *mut Node<'static>
        };
        // For `xmlDoc.last`, walk to the tail of the sibling chain
        // — root or the last epilogue node.
        let last_child_ptr: *mut Node<'static> = {
            // SAFETY: first_child_ptr is a valid Node pointer into
            // the arena owned by `self.bump`, which moves into the
            // returned XmlDoc and stays alive.
            let mut cur: &Node<'static> = unsafe { &*first_child_ptr };
            while let Some(next) = cur.next_sibling.get() {
                // Reborrow with 'static lifetime so the while loop
                // can keep updating `cur` without the previous
                // borrow blocking.
                let next_ptr = next as *const Node<'_> as *const Node<'static>;
                // SAFETY: next is a sibling pointer into the same arena.
                cur = unsafe { &*next_ptr };
            }
            cur as *const Node<'_> as *mut Node<'static>
        };

        // Plant the document's name dict at offset 152 so consumers
        // walking the libxml2-shape `xmlDoc.dict` field can use it.
        // The refcount is bumped once *here* so the planted pointer
        // owns its own reference, independent of the embedded
        // `Document`'s own reference.  Consequences:
        //
        //   * `XmlDoc::free` releases the dict slot (one decrement)
        //     and then drops the embedded Document (another
        //     decrement for `Document.dict`).  Net: both refs
        //     released; the dict survives as long as any other
        //     holder (a different doc parsed on the same thread,
        //     the thread's own stash) still has a reference.
        //
        //   * Consumers that "free" `c_doc.dict` (e.g. lxml's
        //     `initThreadDictRef` decides our dict differs from its
        //     thread dict and calls `xmlDictFree` on it) are
        //     decrementing the slot's own reference; they may then
        //     overwrite the slot with their own dict pointer (with
        //     a fresh `xmlDictReference` of their own).  In either
        //     case our refcount math stays balanced.
        let dict_for_slot: *mut std::os::raw::c_void = {
            let d = self.dict;
            if !d.is_null() {
                // SAFETY: `self.dict` is a live, refcount-managed
                // Dict (set at builder construction and not yet
                // released — the embedded Document hasn't been
                // dropped).  Bumping is sound.
                unsafe { (*d).add_ref(); }
            }
            d as *mut std::os::raw::c_void
        };

        // Allocate version + encoding inside the embedded bump via a
        // temporary borrow.  After this, no more allocations happen in
        // the bump.
        let (version_c, encoding_c) = {
            let bytes_v = version_str.as_bytes();
            let bytes_e = encoding_str.as_bytes();
            let dst_v: &mut [u8] = self.bump.alloc_slice_fill_with(bytes_v.len() + 1, |i| {
                if i < bytes_v.len() { bytes_v[i] } else { 0 }
            });
            let dst_e: &mut [u8] = self.bump.alloc_slice_fill_with(bytes_e.len() + 1, |i| {
                if i < bytes_e.len() { bytes_e[i] } else { 0 }
            });
            // SAFETY: NUL-terminated UTF-8, lifetime tied to the bump
            // which moves into `_doc` (heap-pinned via Pin<Box<Bump>>).
            unsafe {
                (
                    ArenaCStr::from_raw(dst_v.as_ptr()),
                    ArenaCStr::from_raw(dst_e.as_ptr()),
                )
            }
        };

        let boxed = Box::new(XmlDoc {
            _private:    Cell::new(std::ptr::null_mut()),
            kind:        NodeKind::Document,
            _pad_kind:   0,
            name:        std::ptr::null(),
            children:    Cell::new(first_child_ptr),
            last:        Cell::new(last_child_ptr),
            parent:      Cell::new(std::ptr::null_mut()),
            next:        Cell::new(std::ptr::null_mut()),
            prev:        Cell::new(std::ptr::null_mut()),
            doc:         Cell::new(std::ptr::null_mut()),
            compression: 0,
            standalone:  standalone_i32,
            int_subset:  std::ptr::null_mut(),
            ext_subset:  std::ptr::null_mut(),
            old_ns:      std::ptr::null_mut(),
            version:     version_c,
            encoding:    encoding_c,
            ids:         std::ptr::null_mut(),
            refs:        std::ptr::null_mut(),
            url:         std::ptr::null(),
            charset:     1,  // libxml2's XML_CHAR_ENCODING_UTF8 = 1
            _pad_charset: 0,
            dict:        dict_for_slot,
            psvi:        std::ptr::null_mut(),
            parse_flags: 0,
            properties:  0,
            _doc:        std::mem::ManuallyDrop::new(self),
        });
        let raw = Box::into_raw(boxed);
        // Set the self-pointer for libxml2 compatibility (`doc->doc == doc`).
        // SAFETY: `raw` is a freshly allocated, valid pointer.
        unsafe { (*raw).doc.set(raw); }
        // Walk the tree once to stamp `node->doc = raw` on every node,
        // matching libxml2's invariant.  Mutation API (`xmlNewProp`,
        // `xmlAddChild` cross-doc detection) reads this; without it
        // every post-parse mutation would have to walk up to find the
        // owning doc.
        // SAFETY: raw is valid; the tree was just built by the parser.
        unsafe {
            let owned = &*raw;
            let raw_void = raw as *mut std::os::raw::c_void;
            let mut stack: Vec<*const Node<'static>> = Vec::new();
            stack.push(owned.children.get() as *const _);
            // Seed with every top-level sibling (prolog → root →
            // epilogue), not just `children`, so we stamp the whole
            // top-level chain in document-order.  Without this,
            // sibling chains after the first node would be missed.
            //
            // Top-level nodes (prolog comments, the root, epilogue
            // comments) also get their `parent` field stamped to the
            // doc-cast-as-Node.  libxml2 uses this so consumers can
            // walk up from any document-level node, and so that
            // `xmlUnlinkNode` on a prolog/epilogue node correctly
            // adjusts the doc's `children` / `last` pointers (the
            // sibling chain-update reads them off the parent).  The
            // type pun is sound because `XmlDoc` and `Node` share
            // identical layout at offsets 0–64 (private, kind, name,
            // children, last, parent, next, prev, doc).
            let doc_as_node: &Node<'static> = &*(raw as *const Node<'static>);
            let mut cur_sib: *const Node<'static> = owned.children.get() as *const _;
            while !cur_sib.is_null() {
                stack.push(cur_sib);
                let s_ref = &*cur_sib;
                s_ref.parent.set(Some(doc_as_node));
                cur_sib = s_ref.next_sibling.get()
                    .map(|n| n as *const _)
                    .unwrap_or(std::ptr::null());
            }
            while let Some(np) = stack.pop() {
                if np.is_null() { continue; }
                let n = &*np;
                n.doc.set(raw_void);
                // Stamp attributes' doc field too.
                let mut a = n.first_attribute.get();
                while let Some(attr) = a {
                    attr.doc.set(raw_void);
                    a = attr.next.get();
                }
                // Recurse via stack into children only — siblings
                // of the top-level sequence are already enqueued.
                let mut c = n.first_child.get();
                while let Some(ch) = c {
                    stack.push(ch as *const _);
                    c = ch.next_sibling.get();
                }
            }
        }
        raw
    }
}

#[cfg(feature = "c-abi")]
impl XmlDoc {
    /// Reclaim a heap-allocated [`XmlDoc`].  No-op on NULL.  After this
    /// returns, every pointer that was read out of the doc (children,
    /// version, encoding, etc.) is dangling — caller is responsible for
    /// not retaining them.
    ///
    /// # Safety
    /// `ptr` must be NULL or a pointer returned by
    /// [`Document::into_xml_doc`] that has not yet been freed.
    pub unsafe fn free(ptr: *mut XmlDoc) {
        if ptr.is_null() { return; }
        // SAFETY: precondition — caller guarantees ptr was from
        // into_xml_doc and not yet freed.  Box::from_raw reconstructs
        // ownership; the Box drops at end-of-scope.  Inside the drop,
        // _doc (ManuallyDrop) needs explicit drop to release the
        // embedded Document's arena and source bytes.
        let mut boxed = unsafe { Box::from_raw(ptr) };
        // Reclaim the leaked URL CString (set by C-ABI consumers
        // that record a source URL post-build).  NULL when no URL
        // was recorded; safe to skip.
        if !boxed.url.is_null() {
            // SAFETY: url was `CString::into_raw`'d by the consumer
            // (e.g. xml_read_memory_with_dict).  Reclaim and drop.
            unsafe { let _ = std::ffi::CString::from_raw(boxed.url as *mut std::os::raw::c_char); }
        }
        // Release the dict reference planted in the XmlDoc.dict slot.
        // libxml2's `xmlFreeDoc` semantically calls `xmlDictFree` on
        // `doc->dict` — consumers (like lxml) may have swapped the
        // pointer out for their own thread-shared dict before
        // freeing, but the reference is owned by the field regardless
        // of which dict it currently points at.  Mirroring this
        // matches the libxml2 ABI contract.
        let dict_at_free = boxed.dict;
        if !dict_at_free.is_null() {
            // SAFETY: `dict_at_free` was either planted by
            // `into_xml_doc` (with a bumped refcount) or swapped in
            // by a consumer that bumped the refcount itself.  Either
            // way one outstanding reference belongs to this slot.
            unsafe { crate::dict::Dict::release(dict_at_free as *mut crate::dict::Dict); }
        }
        unsafe { std::mem::ManuallyDrop::drop(&mut boxed._doc); }
        drop(boxed);
    }
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Every [`NodeKind`] discriminant must match libxml2's
    /// `xmlElementType` value for the corresponding node type.
    ///
    /// C consumers do `if (node->type == XML_ELEMENT_NODE)` against
    /// the integer; drift here silently misclassifies nodes in any
    /// tree walker linked through the cdylib.  Numbers verified
    /// against `xmlElementType` in
    /// `/opt/homebrew/Cellar/libxml2/<version>/include/libxml2/libxml/tree.h`.
    ///
    /// We model 8 of libxml2's 20 element types today.  The missing
    /// 12 (XML_ENTITY_NODE=6, XML_DOCUMENT_TYPE_NODE=10,
    /// XML_DOCUMENT_FRAG_NODE=11, XML_NOTATION_NODE=12,
    /// XML_HTML_DOCUMENT_NODE=13, XML_DTD_NODE=14,
    /// XML_ELEMENT_DECL=15, XML_ATTRIBUTE_DECL=16,
    /// XML_ENTITY_DECL=17, XML_NAMESPACE_DECL=18,
    /// XML_XINCLUDE_START=19, XML_XINCLUDE_END=20) are intentionally
    /// absent — their slot numbers stay free for future variants
    /// rather than being shadowed.  Any addition lands here too.
    #[test]
    fn node_kind_libxml2_values_match() {
        assert_eq!(NodeKind::Element   as u32,  1);
        assert_eq!(NodeKind::Attribute as u32,  2);
        assert_eq!(NodeKind::Text      as u32,  3);
        assert_eq!(NodeKind::CData     as u32,  4);
        assert_eq!(NodeKind::EntityRef as u32,  5);
        assert_eq!(NodeKind::Pi        as u32,  7);
        assert_eq!(NodeKind::Comment   as u32,  8);
        assert_eq!(NodeKind::Document  as u32,  9);
    }

    fn build_simple_tree() -> Document {
        let b = DocumentBuilder::new();
        let root = b.new_element(b.alloc_str("catalog"));
        let book = b.new_element(b.alloc_str("book"));
        let id   = b.new_attribute(b.alloc_str("id"), b.alloc_str("1"));
        b.append_attribute(book, id);
        let title = b.new_element(b.alloc_str("title"));
        let title_text = b.new_text(b.alloc_str("Dune"));
        b.append_child(title, title_text);
        b.append_child(book, title);
        b.append_child(root, book);
        b.set_root(root);
        b.build()
    }

    #[test]
    fn basic_tree_navigation() {
        let doc = build_simple_tree();
        let root = doc.root();
        assert_eq!(root.name(), "catalog");
        assert!(root.is_element());

        let book = root.children().next().unwrap();
        assert_eq!(book.name(), "book");
        assert!(book.parent.get().is_some());

        let title = book.find_child("title").unwrap();
        assert_eq!(title.name(), "title");
        assert_eq!(title.text_content(), Some("Dune"));
    }

    #[test]
    fn attribute_iteration() {
        let b = DocumentBuilder::new();
        let el = b.new_element(b.alloc_str("el"));
        for (n, v) in [("a", "1"), ("b", "2"), ("c", "3")] {
            let attr = b.new_attribute(b.alloc_str(n), b.alloc_str(v));
            b.append_attribute(el, attr);
        }
        b.set_root(el); let doc = b.build();
        let pairs: Vec<(String, String)> = doc.root().attributes()
            .map(|a| (a.name().to_owned(), a.value().to_owned()))
            .collect();
        assert_eq!(pairs, vec![
            ("a".into(), "1".into()),
            ("b".into(), "2".into()),
            ("c".into(), "3".into()),
        ]);
    }

    #[test]
    fn sibling_links_are_doubly_threaded() {
        let b = DocumentBuilder::new();
        let root = b.new_element(b.alloc_str("r"));
        for n in &["a", "b", "c"] {
            let child = b.new_element(b.alloc_str(*n));
            b.append_child(root, child);
        }
        b.set_root(root); let doc = b.build();
        let r = doc.root();

        let names_fwd: Vec<&str> = r.children().map(|c| c.name()).collect();
        assert_eq!(names_fwd, vec!["a", "b", "c"]);

        // Walk backwards from last_child via prev_sibling
        let mut names_back: Vec<&str> = Vec::new();
        let mut cur = r.last_child.get();
        while let Some(n) = cur {
            names_back.push(n.name());
            cur = n.prev_sibling.get();
        }
        assert_eq!(names_back, vec!["c", "b", "a"]);

        // Parent pointers are set
        for c in r.children() {
            assert!(std::ptr::eq(c.parent.get().unwrap(), r));
        }
    }

    #[test]
    fn detach_middle_child_repairs_links() {
        let b = DocumentBuilder::new();
        let root = b.new_element(b.alloc_str("r"));
        let a = b.new_element(b.alloc_str("a"));
        let b_node = b.new_element(b.alloc_str("b"));
        let c = b.new_element(b.alloc_str("c"));
        b.append_child(root, a);
        b.append_child(root, b_node);
        b.append_child(root, c);

        b.detach(b_node);

        let names: Vec<&str> = root.children().map(|n| n.name()).collect();
        assert_eq!(names, vec!["a", "c"]);
        assert!(b_node.parent.get().is_none());
        assert!(b_node.prev_sibling.get().is_none());
        assert!(b_node.next_sibling.get().is_none());
        // a.next now points to c (skipping b_node)
        assert!(std::ptr::eq(a.next_sibling.get().unwrap(), c));
        assert!(std::ptr::eq(c.prev_sibling.get().unwrap(), a));
    }

    #[test]
    fn detach_first_and_last() {
        let b = DocumentBuilder::new();
        let root = b.new_element(b.alloc_str("r"));
        let a = b.new_element(b.alloc_str("a"));
        let c = b.new_element(b.alloc_str("c"));
        b.append_child(root, a);
        b.append_child(root, c);

        b.detach(a);  // first
        assert!(std::ptr::eq(root.first_child.get().unwrap(), c));
        assert!(root.last_child.get().is_some());
        assert!(c.prev_sibling.get().is_none());

        b.detach(c);  // last (now only child)
        assert!(root.first_child.get().is_none());
        assert!(root.last_child.get().is_none());
    }

    #[test]
    fn document_is_send() {
        // Compile-time check: Document: Send.
        fn assert_send<T: Send>() {}
        assert_send::<Document>();
    }

    #[test]
    fn root_lifetime_bounded_by_document() {
        // This test exists to document the property; the trybuild-style
        // "this should fail to compile" check is below in a doc comment.
        let doc = build_simple_tree();
        let root = doc.root();
        assert_eq!(root.name(), "catalog");
        // `root` cannot outlive `doc` — the lifetime is tied to `&doc`.
        drop(doc);
        // (We intentionally do NOT use `root` here — it would not compile.)
    }

    #[test]
    fn mixed_content_children() {
        let b = DocumentBuilder::new();
        let root = b.new_element(b.alloc_str("r"));
        b.append_child(root, b.new_text(b.alloc_str("before ")));
        let em = b.new_element(b.alloc_str("em"));
        b.append_child(em, b.new_text(b.alloc_str("middle")));
        b.append_child(root, em);
        b.append_child(root, b.new_text(b.alloc_str(" after")));
        b.append_child(root, b.new_comment(b.alloc_str(" note ")));
        b.append_child(root, b.new_cdata(b.alloc_str("<raw>")));

        b.set_root(root); let doc = b.build();
        let r = doc.root();
        let kinds: Vec<NodeKind> = r.children().map(|c| c.kind).collect();
        assert_eq!(kinds, vec![NodeKind::Text, NodeKind::Element, NodeKind::Text,
                               NodeKind::Comment, NodeKind::CData]);

        // text_content() on a mixed element returns first text/cdata
        assert_eq!(r.text_content(), Some("before "));
    }

    #[test]
    fn namespace_attached_to_element() {
        let b = DocumentBuilder::new();
        let ns = b.new_namespace(Some(b.alloc_str("dc")),
                                 b.alloc_str("http://purl.org/dc/elements/1.1/"));
        let el = b.new_element(b.alloc_str("dc:title"));
        el.namespace.set(Some(ns));
        b.set_root(el); let doc = b.build();
        let ns_got = doc.root().namespace.get().unwrap();
        assert_eq!(ns_got.prefix(), Some("dc"));
        assert_eq!(ns_got.href(),   "http://purl.org/dc/elements/1.1/");
    }

    #[test]
    fn memory_bytes_grows_with_alloc() {
        let b = DocumentBuilder::new();
        let root = b.new_element(b.alloc_str("r"));
        for i in 0..100 {
            let child = b.new_element(b.alloc_str(&format!("c{i}")));
            b.append_child(root, child);
        }
        b.set_root(root); let doc = b.build();
        assert!(doc.memory_bytes() > 0);
        assert_eq!(doc.root().children().count(), 100);
    }

    #[test]
    fn pi_node_holds_target_and_content() {
        let b = DocumentBuilder::new();
        let root = b.new_element(b.alloc_str("r"));
        let pi = b.new_pi(b.alloc_str("xml-stylesheet"),
                          Some(b.alloc_str(r#"type="text/xsl" href="s.xsl""#)));
        b.append_child(root, pi);
        b.set_root(root); let doc = b.build();
        let pi = doc.root().children().next().unwrap();
        assert_eq!(pi.kind, NodeKind::Pi);
        assert_eq!(pi.name(), "xml-stylesheet");
        assert_eq!(pi.content(), r#"type="text/xsl" href="s.xsl""#);
    }

    // ── builder default + with_capacity ──────────────────────────

    #[test]
    fn builder_default_is_same_as_new() {
        let b = DocumentBuilder::default();
        // Same shape — should let us build an empty-ish doc.
        let root = b.new_element(b.alloc_str("r"));
        b.set_root(root);
        let doc = b.build();
        assert_eq!(doc.root().name(), "r");
    }

    #[test]
    fn builder_with_capacity_reserves_arena() {
        let b = DocumentBuilder::with_capacity(8 * 1024);
        let root = b.new_element(b.alloc_str("r"));
        b.set_root(root);
        let doc = b.build();
        // Smoke check — capacity matters for performance, not behaviour.
        assert!(doc.memory_bytes() > 0);
        assert_eq!(doc.root().name(), "r");
    }

    #[test]
    fn builder_bump_accessor() {
        // Direct access to the underlying Bump.
        let b = DocumentBuilder::new();
        let _: &bumpalo::Bump = b.bump();
    }

    // ── source() returns None until set_source_inplace_buffer is called ──

    #[test]
    fn builder_source_returns_none_when_no_inplace_buffer() {
        let b = DocumentBuilder::new();
        assert!(b.source().is_none());
    }

    // ── is_entity_ref / entity-ref allocation ────────────────────

    #[test]
    fn entity_ref_node_via_builder() {
        let b = DocumentBuilder::new();
        let root = b.new_element(b.alloc_str("r"));
        let er = b.new_entity_ref(b.alloc_str("foo"), b.alloc_str("&foo;"));
        b.append_child(root, er);
        b.set_root(root); let doc = b.build();
        let e = doc.root().children().next().unwrap();
        assert!(e.is_entity_ref());
        assert!(!e.is_element());
        assert!(!e.is_text());
        assert_eq!(e.name(), "foo");
        assert_eq!(e.content(), "&foo;");
    }

    // ── text_content fallthroughs ────────────────────────────────

    #[test]
    fn text_content_on_element_without_text_returns_none() {
        // Element with only element children → text_content returns None
        // (text_content's find_map exhausts without finding Text/CData,
        // hitting the `_ => None` arm).
        let b = DocumentBuilder::new();
        let root = b.new_element(b.alloc_str("r"));
        let child = b.new_element(b.alloc_str("c"));
        b.append_child(root, child);
        b.set_root(root); let doc = b.build();
        assert_eq!(doc.root().text_content(), None);
    }

    #[test]
    fn text_content_on_comment_returns_none() {
        // Comment kind → outer `_ => None` arm.
        let b = DocumentBuilder::new();
        let root = b.new_element(b.alloc_str("r"));
        let c = b.new_comment(b.alloc_str(" hi "));
        b.append_child(root, c);
        b.set_root(root); let doc = b.build();
        let comment = doc.root().children().next().unwrap();
        assert_eq!(comment.text_content(), None);
    }

    // ── Debug impls ──────────────────────────────────────────────

    #[test]
    fn node_debug_shows_kind_and_relevant_fields() {
        let b = DocumentBuilder::new();
        let root = b.new_element(b.alloc_str("foo"));
        b.append_child(root, b.new_text(b.alloc_str("hello")));
        b.set_root(root); let doc = b.build();
        let s = format!("{:?}", doc.root());
        assert!(s.contains("Element"), "got {s}");
        assert!(s.contains("foo"), "got {s}");

        // Text node — name is empty, content carries.
        let t = doc.root().children().next().unwrap();
        let s = format!("{t:?}");
        assert!(s.contains("Text"), "got {s}");
        assert!(s.contains("hello"), "got {s}");
    }

    #[test]
    fn attribute_debug_shows_name_and_value() {
        let b = DocumentBuilder::new();
        let el = b.new_element(b.alloc_str("el"));
        let attr = b.new_attribute(b.alloc_str("id"), b.alloc_str("123"));
        b.append_attribute(el, attr);
        b.set_root(el); let doc = b.build();
        let attr = doc.root().attributes().next().unwrap();
        let s = format!("{attr:?}");
        assert!(s.contains("Attribute"), "got {s}");
        assert!(s.contains("id"),  "got {s}");
        assert!(s.contains("123"), "got {s}");
    }

    #[test]
    fn document_debug_shows_root_and_memory() {
        let b = DocumentBuilder::new();
        b.set_root(b.new_element(b.alloc_str("rootname")));
        let doc = b.build();
        let s = format!("{doc:?}");
        assert!(s.contains("Document"),     "got {s}");
        assert!(s.contains("memory_bytes"), "got {s}");
        assert!(s.contains("rootname"),     "got {s}");
    }

    // ── ChildIter::from_head ─────────────────────────────────────

    #[test]
    fn child_iter_from_head_walks_chain() {
        let b = DocumentBuilder::new();
        let root = b.new_element(b.alloc_str("r"));
        for n in &["a", "b", "c"] {
            b.append_child(root, b.new_element(b.alloc_str(n)));
        }
        b.set_root(root); let doc = b.build();
        // ChildIter::from_head used internally by Attribute::children.
        // Test it directly with the root's first_child.
        let head = doc.root().first_child.get();
        let names: Vec<&str> = ChildIter::from_head(head).map(|c| c.name()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);

        // None head yields no nodes.
        let empty: Vec<&str> = ChildIter::from_head(None).map(|c| c.name()).collect();
        assert!(empty.is_empty());
    }

    // ── Document::first_sibling and is_html ─────────────────────

    #[test]
    fn document_first_sibling_equals_root_by_default() {
        let b = DocumentBuilder::new();
        b.set_root(b.new_element(b.alloc_str("r")));
        let doc = b.build();
        // No prolog → first_sibling is the root itself.
        let first = doc.first_sibling();
        assert_eq!(first.name(), "r");
    }

    #[test]
    fn document_is_html_false_without_metadata() {
        let b = DocumentBuilder::new();
        b.set_root(b.new_element(b.alloc_str("r")));
        let doc = b.build();
        assert!(!doc.is_html());
    }

    // ── Document post-build mutation API ────────────────────────

    #[test]
    fn document_post_build_new_element_and_append_child() {
        let b = DocumentBuilder::new();
        b.set_root(b.new_element(b.alloc_str("r")));
        let doc = b.build();
        let root = doc.root();
        let added = doc.new_element(doc.bump().alloc_str("late"));
        doc.append_child(root, added);
        let names: Vec<&str> = root.children().map(|c| c.name()).collect();
        assert_eq!(names, vec!["late"]);
    }

    #[test]
    fn document_post_build_append_two_children_threads_siblings() {
        let b = DocumentBuilder::new();
        b.set_root(b.new_element(b.alloc_str("r")));
        let doc = b.build();
        let root = doc.root();
        let a = doc.new_element(doc.bump().alloc_str("a"));
        let bn = doc.new_element(doc.bump().alloc_str("b"));
        doc.append_child(root, a);
        doc.append_child(root, bn);
        // First+second-append both branches in append_child (None / Some(last)).
        let names: Vec<&str> = root.children().map(|c| c.name()).collect();
        assert_eq!(names, vec!["a", "b"]);
        // Sibling links between a and b set up.
        assert!(std::ptr::eq(a.next_sibling.get().unwrap(), bn));
        assert!(std::ptr::eq(bn.prev_sibling.get().unwrap(), a));
    }

    #[test]
    fn document_post_build_append_attribute_two_threads_links() {
        let b = DocumentBuilder::new();
        b.set_root(b.new_element(b.alloc_str("r")));
        let doc = b.build();
        let root = doc.root();
        let a1 = doc.new_attribute(doc.bump().alloc_str("id"),    doc.bump().alloc_str("1"));
        let a2 = doc.new_attribute(doc.bump().alloc_str("class"), doc.bump().alloc_str("c"));
        doc.append_attribute(root, a1);
        doc.append_attribute(root, a2);
        let pairs: Vec<(&str, &str)> = root.attributes()
            .map(|a| (a.name(), a.value())).collect();
        assert_eq!(pairs, vec![("id", "1"), ("class", "c")]);
    }

    #[test]
    fn document_post_build_allocator_variants() {
        // Touch every Document-level allocator to cover their bodies.
        let b = DocumentBuilder::new();
        b.set_root(b.new_element(b.alloc_str("r")));
        let doc = b.build();
        let root = doc.root();

        let t  = doc.new_text(doc.bump().alloc_str("hi"));
        let cd = doc.new_cdata(doc.bump().alloc_str("raw"));
        let cm = doc.new_comment(doc.bump().alloc_str(" c "));
        let pi = doc.new_pi(doc.bump().alloc_str("php"), Some(doc.bump().alloc_str("")));
        let er = doc.new_entity_ref(doc.bump().alloc_str("foo"), doc.bump().alloc_str("&foo;"));

        doc.append_child(root, t);
        doc.append_child(root, cd);
        doc.append_child(root, cm);
        doc.append_child(root, pi);
        doc.append_child(root, er);

        let kinds: Vec<NodeKind> = root.children().map(|c| c.kind).collect();
        assert_eq!(kinds, vec![
            NodeKind::Text, NodeKind::CData, NodeKind::Comment,
            NodeKind::Pi, NodeKind::EntityRef,
        ]);
    }

    #[test]
    fn document_bump_accessor() {
        let b = DocumentBuilder::new();
        b.set_root(b.new_element(b.alloc_str("r")));
        let doc = b.build();
        let _: &bumpalo::Bump = doc.bump();
    }
}
