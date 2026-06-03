//! C-ABI DTD declaration objects and DOCTYPE-subset serialization.
//!
//! libxml2 exposes the parsed DTD as a chain of typed declaration nodes
//! hung off `xmlDtd.children` in declaration order, and *reconstructs*
//! the `<!DOCTYPE name [ … ]>` block from them on output (it does not
//! preserve the source text — it normalises whitespace, dumps notations
//! first, etc.).  lxml's `DTD` wrapper walks that chain by node `type`
//! (`XML_ELEMENT_DECL`=15 / `XML_ATTRIBUTE_DECL`=16 / `XML_ENTITY_DECL`
//! =17) for its object model, and its serializer calls
//! `xmlNodeDumpOutput` once per child to emit the body.
//!
//! Our parser produces a Rust-native [`Dtd`].  This module materializes
//! it into the libxml2-shaped structs (so the object model works) and,
//! at materialize time, computes each declaration's reconstructed text —
//! matching libxml2's format — keyed by the node pointer, so
//! [`crate::outbuf::xmlNodeDumpOutput`] can emit it without re-deriving
//! anything from the C structs.  The structs, backing strings, and
//! reconstructed text are owned by a per-handle [`DeclStore`] in a
//! thread-local map; freeing the DTD drops the store and its allocations.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;

use sup_xml_core::dtd::{AttDecl, AttDefault, AttType, ContentModel, DeclRef, Dtd, EntityDecl,
                        Group, GroupKind, Item, Occurrence};

use crate::dtd::xmlDtd;

const XML_ELEMENT_DECL:   c_int = 15;
const XML_ATTRIBUTE_DECL: c_int = 16;
const XML_ENTITY_DECL:    c_int = 17;

const XML_ELEMENT_TYPE_EMPTY:   c_int = 1;
const XML_ELEMENT_TYPE_ANY:     c_int = 2;
const XML_ELEMENT_TYPE_MIXED:   c_int = 3;
const XML_ELEMENT_TYPE_ELEMENT: c_int = 4;

const XML_ELEMENT_CONTENT_PCDATA:  c_int = 1;
const XML_ELEMENT_CONTENT_ELEMENT: c_int = 2;
const XML_ELEMENT_CONTENT_SEQ:     c_int = 3;
const XML_ELEMENT_CONTENT_OR:      c_int = 4;

const XML_ELEMENT_CONTENT_ONCE: c_int = 1;
const XML_ELEMENT_CONTENT_OPT:  c_int = 2;
const XML_ELEMENT_CONTENT_MULT: c_int = 3;
const XML_ELEMENT_CONTENT_PLUS: c_int = 4;

const XML_ATTRIBUTE_CDATA:       c_int = 1;
const XML_ATTRIBUTE_ID:          c_int = 2;
const XML_ATTRIBUTE_IDREF:       c_int = 3;
const XML_ATTRIBUTE_IDREFS:      c_int = 4;
const XML_ATTRIBUTE_ENTITY:      c_int = 5;
const XML_ATTRIBUTE_ENTITIES:    c_int = 6;
const XML_ATTRIBUTE_NMTOKEN:     c_int = 7;
const XML_ATTRIBUTE_NMTOKENS:    c_int = 8;
const XML_ATTRIBUTE_ENUMERATION: c_int = 9;
const XML_ATTRIBUTE_NOTATION:    c_int = 10;

const XML_ATTRIBUTE_NONE:     c_int = 1;
const XML_ATTRIBUTE_REQUIRED: c_int = 2;
const XML_ATTRIBUTE_IMPLIED:  c_int = 3;
const XML_ATTRIBUTE_FIXED:    c_int = 4;

const XML_INTERNAL_GENERAL_ENTITY:          c_int = 1;
const XML_EXTERNAL_GENERAL_PARSED_ENTITY:   c_int = 2;
const XML_EXTERNAL_GENERAL_UNPARSED_ENTITY: c_int = 3;
const XML_INTERNAL_PARAMETER_ENTITY:        c_int = 4;
const XML_EXTERNAL_PARAMETER_ENTITY:        c_int = 5;

#[repr(C)]
pub struct xmlEnumeration {
    pub next: *mut xmlEnumeration,
    pub name: *const c_char,
}

#[repr(C)]
pub struct xmlElementContent {
    pub typ:    c_int,
    pub ocur:   c_int,
    pub name:   *const c_char,
    pub c1:     *mut xmlElementContent,
    pub c2:     *mut xmlElementContent,
    pub parent: *mut xmlElementContent,
    pub prefix: *const c_char,
}

#[repr(C)]
pub struct xmlElement {
    pub _private:   *mut c_void,            //   0
    pub typ:        c_int,                  //   8
    _pad8:          u32,                    //  12
    pub name:       *const c_char,          //  16
    pub children:   *mut c_void,            //  24
    pub last:       *mut c_void,            //  32
    pub parent:     *mut xmlDtd,            //  40
    pub next:       *mut c_void,            //  48
    pub prev:       *mut c_void,            //  56
    pub doc:        *mut c_void,            //  64
    pub etype:      c_int,                  //  72
    _pad72:         u32,                    //  76
    pub content:    *mut xmlElementContent, //  80
    pub attributes: *mut xmlAttribute,      //  88
    pub prefix:     *const c_char,          //  96
    pub cont_model: *mut c_void,            // 104
}

#[repr(C)]
pub struct xmlAttribute {
    pub _private:      *mut c_void,         //   0
    pub typ:           c_int,               //   8
    _pad8:             u32,                 //  12
    pub name:          *const c_char,       //  16
    pub children:      *mut c_void,         //  24
    pub last:          *mut c_void,         //  32
    pub parent:        *mut xmlDtd,         //  40
    pub next:          *mut c_void,         //  48
    pub prev:          *mut c_void,         //  56
    pub doc:           *mut c_void,         //  64
    pub nexth:         *mut xmlAttribute,   //  72
    pub atype:         c_int,               //  80
    pub def:           c_int,               //  84
    pub default_value: *const c_char,       //  88
    pub tree:          *mut xmlEnumeration, //  96
    pub prefix:        *const c_char,       // 104
    pub elem:          *const c_char,       // 112
}

#[repr(C)]
pub struct xmlEntity {
    pub _private:    *mut c_void,    //   0
    pub typ:         c_int,          //   8
    _pad8:           u32,            //  12
    pub name:        *const c_char,  //  16
    pub children:    *mut c_void,    //  24
    pub last:        *mut c_void,    //  32
    pub parent:      *mut xmlDtd,    //  40
    pub next:        *mut c_void,    //  48
    pub prev:        *mut c_void,    //  56
    pub doc:         *mut c_void,    //  64
    pub orig:        *mut c_char,    //  72
    pub content:     *mut c_char,    //  80
    pub length:      c_int,          //  88
    pub etype:       c_int,          //  92
    pub external_id: *const c_char,  //  96
    pub system_id:   *const c_char,  // 104
    pub nexte:       *mut xmlEntity, // 112
    pub uri:         *const c_char,  // 120
    pub owner:       c_int,          // 128
    pub flags:       c_int,          // 132
}

const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<xmlEnumeration>() == 16);
    assert!(size_of::<xmlElementContent>() == 48);
    assert!(size_of::<xmlElement>() == 112);
    assert!(offset_of!(xmlElement, name) == 16);
    assert!(offset_of!(xmlElement, etype) == 72);
    assert!(offset_of!(xmlElement, content) == 80);
    assert!(offset_of!(xmlElement, attributes) == 88);
    assert!(size_of::<xmlAttribute>() == 120);
    assert!(offset_of!(xmlAttribute, nexth) == 72);
    assert!(offset_of!(xmlAttribute, atype) == 80);
    assert!(offset_of!(xmlAttribute, tree) == 96);
    assert!(offset_of!(xmlAttribute, elem) == 112);
    assert!(size_of::<xmlEntity>() == 136);
    assert!(offset_of!(xmlEntity, orig) == 72);
    assert!(offset_of!(xmlEntity, content) == 80);
    assert!(offset_of!(xmlEntity, etype) == 92);
    assert!(offset_of!(xmlEntity, system_id) == 104);
};

#[derive(Default)]
struct DeclStore {
    elements:   Vec<Box<xmlElement>>,
    attributes: Vec<Box<xmlAttribute>>,
    entities:   Vec<Box<xmlEntity>>,
    contents:   Vec<Box<xmlElementContent>>,
    enums:      Vec<Box<xmlEnumeration>>,
    strings:    Vec<CString>,
    /// Node pointers this store registered in [`DECL_SOURCES`], so they
    /// can be evicted when the store is dropped.
    source_keys: Vec<usize>,
}

impl DeclStore {
    fn cstr(&mut self, s: &str) -> *const c_char {
        match CString::new(s) {
            Ok(c) => { let p = c.as_ptr(); self.strings.push(c); p }
            Err(_) => ptr::null(),
        }
    }
    fn cstr_opt(&mut self, s: Option<&str>) -> *const c_char {
        s.map(|s| self.cstr(s)).unwrap_or(ptr::null())
    }
}

thread_local! {
    static DECL_STORES: RefCell<HashMap<usize, DeclStore>> = RefCell::new(HashMap::new());
    /// Reconstructed declaration text keyed by decl-node pointer; read by
    /// [`decl_source`] from the serializer.
    static DECL_SOURCES: RefCell<HashMap<usize, String>> = RefCell::new(HashMap::new());
}

/// The reconstructed `<!… …>\n` text for a declaration node (types 15 /
/// 16 / 17), or `None` if the pointer isn't a materialized decl.  Used by
/// `xmlNodeDumpOutput` to emit the DOCTYPE-subset body.
pub(crate) fn decl_source(node: *const c_void) -> Option<String> {
    DECL_SOURCES.with(|m| m.borrow().get(&(node as usize)).cloned())
}

/// Build the declaration graph for `model`, link it on `dtd.children`,
/// and register each node's reconstructed serialization.  No-op for a
/// NULL handle.
///
/// # Safety
/// `dtd` is a live `xmlDtd` whose `children`/`last`/`pentities` slots
/// this overwrites; `doc` is its owning document (or NULL).
pub(crate) unsafe fn materialize(dtd: *mut xmlDtd, doc: *mut c_void, model: &Dtd) {
    if dtd.is_null() {
        return;
    }
    let mut store = DeclStore::default();
    let mut head: *mut c_void = ptr::null_mut();
    let mut tail: *mut c_void = ptr::null_mut();
    // Element nodes by name + the attribute nodes for each element, so an
    // `<!ATTLIST>` (which may precede or follow its `<!ELEMENT>`) can be
    // linked onto `xmlElement.attributes` for lxml's object model.
    let mut elem_nodes: HashMap<String, *mut xmlElement> = HashMap::new();
    let mut elem_attrs: HashMap<String, Vec<*mut xmlAttribute>> = HashMap::new();

    for decl in &model.decl_order {
        match decl {
            DeclRef::Element(name) => {
                let Some(d) = model.elements.get(name) else { continue };
                let el = build_element(&mut store, dtd, doc, name, &d.content);
                register_source(&mut store, el as *mut c_void, serialize_element(name, &d.content));
                link_sibling(&mut head, &mut tail, el as *mut c_void);
                elem_nodes.insert(name.clone(), el);
                store.elements.push(unsafe { Box::from_raw(el) });
            }
            DeclRef::Attlist(name) => {
                let Some(atts) = model.attlists.get(name) else { continue };
                for att in atts {
                    let a = build_attribute(&mut store, dtd, doc, name, att);
                    register_source(&mut store, a as *mut c_void, serialize_attribute(name, att));
                    link_sibling(&mut head, &mut tail, a as *mut c_void);
                    elem_attrs.entry(name.clone()).or_default().push(a);
                    store.attributes.push(unsafe { Box::from_raw(a) });
                }
            }
            DeclRef::Entity(idx) => {
                let Some(ent) = model.entities.get(*idx) else { continue };
                let e = build_entity(&mut store, dtd, doc, ent);
                register_source(&mut store, e as *mut c_void, serialize_entity(ent));
                link_sibling(&mut head, &mut tail, e as *mut c_void);
                store.entities.push(unsafe { Box::from_raw(e) });
            }
        }
    }

    // Hang each element's attribute list off `xmlElement.attributes`
    // (`nexth`-chained), so lxml's `element.attributes()` works without
    // `xmlGetDtdElementDesc` (a stub).
    for (name, attrs) in &elem_attrs {
        let Some(&el) = elem_nodes.get(name) else { continue };
        for w in attrs.windows(2) {
            unsafe { (*w[0]).nexth = w[1]; }
        }
        unsafe { (*el).attributes = attrs[0]; }
    }

    unsafe {
        (*dtd).children = head;
        (*dtd).last = tail;
        // lxml only emits the `[ … ]` block when one of the DTD's hash
        // pointers is non-NULL (serializer.pxi); give it a real empty
        // pentities hash to trip that gate, matching libxml2.
        if !head.is_null() && (*dtd).pentities.is_null() {
            (*dtd).pentities = crate::hash::xmlHashCreate(0) as *mut c_void;
        }
    }
    DECL_STORES.with(|m| { m.borrow_mut().insert(dtd as usize, store); });
}

/// Drop a freed handle's declaration store and evict its serialization
/// entries.  Called from `xmlFreeDtd` and document teardown.
pub(crate) fn forget(dtd: *mut xmlDtd) {
    if let Some(store) = DECL_STORES.with(|m| m.borrow_mut().remove(&(dtd as usize))) {
        DECL_SOURCES.with(|m| {
            let mut map = m.borrow_mut();
            for k in &store.source_keys {
                map.remove(k);
            }
        });
    }
}

fn register_source(store: &mut DeclStore, node: *mut c_void, text: String) {
    store.source_keys.push(node as usize);
    DECL_SOURCES.with(|m| { m.borrow_mut().insert(node as usize, text); });
}

fn link_sibling(head: &mut *mut c_void, tail: &mut *mut c_void, node: *mut c_void) {
    if head.is_null() {
        *head = node;
    }
    if !tail.is_null() {
        unsafe {
            ptr::write((node as *mut u8).add(56) as *mut *mut c_void, *tail); // node.prev = tail
            ptr::write((*tail as *mut u8).add(48) as *mut *mut c_void, node); // tail.next = node
        }
    }
    *tail = node;
}

// ── node construction ──────────────────────────────────────────────────────

fn build_element(
    store: &mut DeclStore, dtd: *mut xmlDtd, doc: *mut c_void, name: &str, content: &ContentModel,
) -> *mut xmlElement {
    let name_p = store.cstr(name);
    let (etype, content_p) = match content {
        ContentModel::Empty => (XML_ELEMENT_TYPE_EMPTY, ptr::null_mut()),
        ContentModel::Any => (XML_ELEMENT_TYPE_ANY, ptr::null_mut()),
        ContentModel::Mixed { choices } => (XML_ELEMENT_TYPE_MIXED, build_mixed(store, choices)),
        ContentModel::Children(g) => (XML_ELEMENT_TYPE_ELEMENT, build_group(store, g)),
    };
    Box::into_raw(Box::new(xmlElement {
        _private: ptr::null_mut(), typ: XML_ELEMENT_DECL, _pad8: 0, name: name_p,
        children: ptr::null_mut(), last: ptr::null_mut(), parent: dtd,
        next: ptr::null_mut(), prev: ptr::null_mut(), doc,
        etype, _pad72: 0, content: content_p, attributes: ptr::null_mut(),
        prefix: ptr::null(), cont_model: ptr::null_mut(),
    }))
}

fn alloc_content(
    store: &mut DeclStore, typ: c_int, ocur: c_int, name: *const c_char,
    c1: *mut xmlElementContent, c2: *mut xmlElementContent,
) -> *mut xmlElementContent {
    let node = Box::into_raw(Box::new(xmlElementContent {
        typ, ocur, name, c1, c2, parent: ptr::null_mut(), prefix: ptr::null(),
    }));
    if !c1.is_null() { unsafe { (*c1).parent = node; } }
    if !c2.is_null() { unsafe { (*c2).parent = node; } }
    store.contents.push(unsafe { Box::from_raw(node) });
    node
}

fn occur_val(o: Occurrence) -> c_int {
    match o {
        Occurrence::One => XML_ELEMENT_CONTENT_ONCE,
        Occurrence::ZeroOrOne => XML_ELEMENT_CONTENT_OPT,
        Occurrence::ZeroOrMore => XML_ELEMENT_CONTENT_MULT,
        Occurrence::OneOrMore => XML_ELEMENT_CONTENT_PLUS,
    }
}

fn build_mixed(store: &mut DeclStore, choices: &[String]) -> *mut xmlElementContent {
    let pcdata = alloc_content(store, XML_ELEMENT_CONTENT_PCDATA, XML_ELEMENT_CONTENT_MULT,
        ptr::null(), ptr::null_mut(), ptr::null_mut());
    if choices.is_empty() {
        return pcdata;
    }
    let mut cur = pcdata;
    for name in choices {
        let np = store.cstr(name);
        let leaf = alloc_content(store, XML_ELEMENT_CONTENT_ELEMENT, XML_ELEMENT_CONTENT_ONCE,
            np, ptr::null_mut(), ptr::null_mut());
        cur = alloc_content(store, XML_ELEMENT_CONTENT_OR, XML_ELEMENT_CONTENT_MULT,
            ptr::null(), cur, leaf);
    }
    cur
}

fn build_group(store: &mut DeclStore, group: &Group) -> *mut xmlElementContent {
    let inner = build_group_inner(store, group);
    if !inner.is_null() && group.occur != Occurrence::One {
        unsafe { (*inner).ocur = occur_val(group.occur); }
    }
    inner
}

fn build_particle(store: &mut DeclStore, p: &sup_xml_core::dtd::Particle) -> *mut xmlElementContent {
    match &p.item {
        Item::Name(n) => {
            let np = store.cstr(n);
            alloc_content(store, XML_ELEMENT_CONTENT_ELEMENT, occur_val(p.occur),
                np, ptr::null_mut(), ptr::null_mut())
        }
        Item::Group(g) => {
            let node = build_group_inner(store, g);
            if !node.is_null() {
                let eff = if p.occur != Occurrence::One { p.occur } else { g.occur };
                unsafe { (*node).ocur = occur_val(eff); }
            }
            node
        }
    }
}

fn build_group_inner(store: &mut DeclStore, group: &Group) -> *mut xmlElementContent {
    if group.items.is_empty() {
        return ptr::null_mut();
    }
    if group.items.len() == 1 {
        return build_particle(store, &group.items[0]);
    }
    let typ = match group.kind {
        GroupKind::Sequence => XML_ELEMENT_CONTENT_SEQ,
        GroupKind::Choice => XML_ELEMENT_CONTENT_OR,
    };
    let mut iter = group.items.iter().rev();
    let mut acc = build_particle(store, iter.next().unwrap());
    for p in iter {
        let left = build_particle(store, p);
        acc = alloc_content(store, typ, XML_ELEMENT_CONTENT_ONCE, ptr::null(), left, acc);
    }
    acc
}

fn build_attribute(
    store: &mut DeclStore, dtd: *mut xmlDtd, doc: *mut c_void, elem: &str, att: &AttDecl,
) -> *mut xmlAttribute {
    let name_p = store.cstr(&att.name);
    let elem_p = store.cstr(elem);
    let (atype, tree) = match &att.att_type {
        AttType::CData => (XML_ATTRIBUTE_CDATA, ptr::null_mut()),
        AttType::Id => (XML_ATTRIBUTE_ID, ptr::null_mut()),
        AttType::IdRef => (XML_ATTRIBUTE_IDREF, ptr::null_mut()),
        AttType::IdRefs => (XML_ATTRIBUTE_IDREFS, ptr::null_mut()),
        AttType::Entity => (XML_ATTRIBUTE_ENTITY, ptr::null_mut()),
        AttType::Entities => (XML_ATTRIBUTE_ENTITIES, ptr::null_mut()),
        AttType::Nmtoken => (XML_ATTRIBUTE_NMTOKEN, ptr::null_mut()),
        AttType::Nmtokens => (XML_ATTRIBUTE_NMTOKENS, ptr::null_mut()),
        AttType::Enumeration(v) => (XML_ATTRIBUTE_ENUMERATION, build_enum(store, v)),
        AttType::Notation(v) => (XML_ATTRIBUTE_NOTATION, build_enum(store, v)),
    };
    let (def, default_value) = match &att.default {
        AttDefault::Required => (XML_ATTRIBUTE_REQUIRED, ptr::null()),
        AttDefault::Implied => (XML_ATTRIBUTE_IMPLIED, ptr::null()),
        AttDefault::Fixed(v) => (XML_ATTRIBUTE_FIXED, store.cstr(v)),
        AttDefault::Default(v) => (XML_ATTRIBUTE_NONE, store.cstr(v)),
    };
    Box::into_raw(Box::new(xmlAttribute {
        _private: ptr::null_mut(), typ: XML_ATTRIBUTE_DECL, _pad8: 0, name: name_p,
        children: ptr::null_mut(), last: ptr::null_mut(), parent: dtd,
        next: ptr::null_mut(), prev: ptr::null_mut(), doc,
        nexth: ptr::null_mut(), atype, def, default_value, tree,
        prefix: ptr::null(), elem: elem_p,
    }))
}

fn build_enum(store: &mut DeclStore, vals: &[String]) -> *mut xmlEnumeration {
    let mut head: *mut xmlEnumeration = ptr::null_mut();
    let mut tail: *mut xmlEnumeration = ptr::null_mut();
    for v in vals {
        let np = store.cstr(v);
        let node = Box::into_raw(Box::new(xmlEnumeration { next: ptr::null_mut(), name: np }));
        if head.is_null() { head = node; }
        if !tail.is_null() { unsafe { (*tail).next = node; } }
        tail = node;
        store.enums.push(unsafe { Box::from_raw(node) });
    }
    head
}

fn build_entity(
    store: &mut DeclStore, dtd: *mut xmlDtd, doc: *mut c_void, ent: &EntityDecl,
) -> *mut xmlEntity {
    let name_p = store.cstr(&ent.name);
    let orig = ent.orig.as_deref().map(|s| store.cstr(s) as *mut c_char).unwrap_or(ptr::null_mut());
    let content = ent.content.as_deref().map(|s| store.cstr(s) as *mut c_char).unwrap_or(ptr::null_mut());
    let length = ent.content.as_deref().map(|s| s.chars().count() as c_int).unwrap_or(0);
    let external_id = store.cstr_opt(ent.public_id.as_deref());
    let system_id = store.cstr_opt(ent.system_id.as_deref());
    let external = ent.system_id.is_some() || ent.public_id.is_some();
    let etype = match (ent.parameter, external, ent.ndata.is_some()) {
        (true, true, _)   => XML_EXTERNAL_PARAMETER_ENTITY,
        (true, false, _)  => XML_INTERNAL_PARAMETER_ENTITY,
        (false, _, true)  => XML_EXTERNAL_GENERAL_UNPARSED_ENTITY,
        (false, true, _)  => XML_EXTERNAL_GENERAL_PARSED_ENTITY,
        (false, false, _) => XML_INTERNAL_GENERAL_ENTITY,
    };
    Box::into_raw(Box::new(xmlEntity {
        _private: ptr::null_mut(), typ: XML_ENTITY_DECL, _pad8: 0, name: name_p,
        children: ptr::null_mut(), last: ptr::null_mut(), parent: dtd,
        next: ptr::null_mut(), prev: ptr::null_mut(), doc,
        orig, content, length, etype, external_id, system_id,
        nexte: ptr::null_mut(), uri: ptr::null(), owner: 0, flags: 0,
    }))
}

// ── reconstruction (matches libxml2's xmlDump* output) ──────────────────────

/// `<!ELEMENT name CONTENT>\n`.
fn serialize_element(name: &str, content: &ContentModel) -> String {
    format!("<!ELEMENT {name} {}>\n", serialize_content(content))
}

fn serialize_content(content: &ContentModel) -> String {
    match content {
        ContentModel::Empty => "EMPTY".to_string(),
        ContentModel::Any => "ANY".to_string(),
        ContentModel::Mixed { choices } if choices.is_empty() => "(#PCDATA)".to_string(),
        ContentModel::Mixed { choices } => {
            let mut s = String::from("(#PCDATA");
            for c in choices {
                s.push_str(" | ");
                s.push_str(c);
            }
            s.push_str(")*");
            s
        }
        ContentModel::Children(g) => serialize_group(g),
    }
}

fn occur_suffix(o: Occurrence) -> &'static str {
    match o {
        Occurrence::One => "",
        Occurrence::ZeroOrOne => "?",
        Occurrence::ZeroOrMore => "*",
        Occurrence::OneOrMore => "+",
    }
}

fn serialize_group(group: &Group) -> String {
    let sep = match group.kind {
        GroupKind::Sequence => " , ",
        GroupKind::Choice => " | ",
    };
    let inner: Vec<String> = group.items.iter().map(serialize_particle).collect();
    format!("({}){}", inner.join(sep), occur_suffix(group.occur))
}

fn serialize_particle(p: &sup_xml_core::dtd::Particle) -> String {
    match &p.item {
        Item::Name(n) => format!("{n}{}", occur_suffix(p.occur)),
        Item::Group(g) => {
            // The particle occurrence overrides the inner group's when set.
            let mut inner = g.clone();
            if p.occur != Occurrence::One {
                inner.occur = p.occur;
            }
            serialize_group(&inner)
        }
    }
}

/// `<!ATTLIST elem name TYPE DEFAULT>\n` — one declaration per attribute,
/// as libxml2 dumps them.
fn serialize_attribute(elem: &str, att: &AttDecl) -> String {
    let ty = match &att.att_type {
        AttType::CData => "CDATA".to_string(),
        AttType::Id => "ID".to_string(),
        AttType::IdRef => "IDREF".to_string(),
        AttType::IdRefs => "IDREFS".to_string(),
        AttType::Entity => "ENTITY".to_string(),
        AttType::Entities => "ENTITIES".to_string(),
        AttType::Nmtoken => "NMTOKEN".to_string(),
        AttType::Nmtokens => "NMTOKENS".to_string(),
        AttType::Enumeration(v) => format!("({})", v.join(" | ")),
        AttType::Notation(v) => format!("NOTATION ({})", v.join(" | ")),
    };
    let def = match &att.default {
        AttDefault::Required => " #REQUIRED".to_string(),
        AttDefault::Implied => " #IMPLIED".to_string(),
        AttDefault::Fixed(v) => format!(" #FIXED \"{v}\""),
        AttDefault::Default(v) => format!(" \"{v}\""),
    };
    format!("<!ATTLIST {elem} {} {ty}{def}>\n", att.name)
}

/// `<!ENTITY [% ]name DEF>\n`.
fn serialize_entity(ent: &EntityDecl) -> String {
    let pct = if ent.parameter { "% " } else { "" };
    let def = if let Some(orig) = &ent.orig {
        format!("\"{orig}\"")
    } else {
        let mut s = match (&ent.public_id, &ent.system_id) {
            (Some(p), Some(sys)) => format!("PUBLIC \"{p}\" \"{sys}\""),
            (Some(p), None) => format!("PUBLIC \"{p}\""),
            (None, Some(sys)) => format!("SYSTEM \"{sys}\""),
            (None, None) => String::new(),
        };
        if let Some(nd) = &ent.ndata {
            s.push_str(&format!(" NDATA {nd}"));
        }
        s
    };
    format!("<!ENTITY {pct}{} {def}>\n", ent.name)
}
