//! Compiled-schema data structures.
//!
//! A [`Schema`] is the output of the schema compiler — a graph of
//! reference-counted declarations and type definitions, ready for the
//! validator to consume.
//!
//! Names are [`QName`]s carrying the namespace URI alongside the local
//! name; the schema compiler resolves prefixes to URIs at compile time
//! so the runtime never has to deal with prefix bookkeeping.

use std::collections::HashMap;
use std::sync::Arc;

use super::types::{ComplexType, SimpleType};

// ── compile-time options ─────────────────────────────────────────────────────

/// Which XSD version the compiler should target.  XSD 1.1 is a strict
/// superset of 1.0 — every 1.0 schema is also a valid 1.1 schema — so
/// the choice only matters when a schema *uses* a 1.1-specific
/// construct (`xs:assert`, `xs:alternative`, `xs:override`, wildcard
/// `notQName` / `notNamespace`, the 1.1-added built-in datatypes, …).
///
/// Defaults to [`Xsd10`](SchemaVersion::Xsd10) — matches libxml2's
/// behaviour, matches Xerces and Saxon's defaults, and avoids the
/// "schema looked like it compiled but the 1.1 constraints aren't
/// actually being enforced" failure mode that auto-detection produces
/// when authors forget to set `vc:minVersion`.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchemaVersion {
    /// Strict XSD 1.0 — reject any 1.1 construct.  Equivalent to
    /// libxml2's behaviour.  Recommended for any pipeline that's
    /// migrating from libxml2 or whose schemas predate 2012.
    #[default]
    Xsd10,
    /// Strict XSD 1.1 — accept 1.1 constructs unconditionally,
    /// without requiring `vc:minVersion` on the schema document.
    Xsd11,
    /// Hybrid: start in 1.0 mode, auto-promote to 1.1 when the schema
    /// document carries `vc:minVersion="1.1"` on its root element.
    /// Convenient for mixed corpora; explicit opt-in via
    /// [`Xsd11`](SchemaVersion::Xsd11) is still preferred for
    /// production pipelines because most 1.1 schemas in the wild
    /// don't bother to set `vc:minVersion`.
    Auto,
}

/// Knobs for [`Schema::compile_str_with_options`](Schema::compile_str_with_options)
/// / [`Schema::compile_with_options`](Schema::compile_with_options).
///
/// Today the only knob is [`version`](Self::version); more options
/// (strictness toggles for libxml2-compat, optional warning callbacks,
/// etc.) will land on this struct so we don't break the API every
/// time we add a flag.
#[derive(Default, Clone, Debug)]
pub struct SchemaOptions {
    pub version: SchemaVersion,
}

// ── qualified names ──────────────────────────────────────────────────────────

/// A namespace-qualified XML name.  XSD operates entirely in terms of
/// these — local names alone are insufficient when a schema imports
/// other namespaces.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QName {
    /// `None` when the name is in no namespace.
    pub namespace: Option<Arc<str>>,
    pub local:     Arc<str>,
}

impl QName {
    pub fn new(ns: Option<&str>, local: &str) -> Self {
        Self {
            namespace: ns.map(Arc::from),
            local:     Arc::from(local),
        }
    }

    /// XSD-spec built-in namespace (`http://www.w3.org/2001/XMLSchema`).
    pub const XSD_NS: &'static str = "http://www.w3.org/2001/XMLSchema";

    /// Build a QName in the XSD spec namespace.
    pub fn xsd(local: &str) -> Self {
        Self::new(Some(Self::XSD_NS), local)
    }
}

impl std::fmt::Display for QName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.namespace {
            Some(ns) => write!(f, "{{{ns}}}{}", self.local),
            None     => f.write_str(&self.local),
        }
    }
}

// ── element declaration ──────────────────────────────────────────────────────

/// `<xs:element>` declaration — top-level or local.
#[derive(Debug)]
pub struct ElementDecl {
    pub name:        QName,
    /// Resolved type — may be a simple or complex type.  Anonymous
    /// inline types end up here too.
    pub type_def:    TypeRef,
    /// `xs:nillable` attribute.  `xsi:nil="true"` instances on this
    /// element are valid only when this is `true`.
    pub nillable:    bool,
    /// `default="…"` value applied when the element is empty.
    pub default:     Option<String>,
    /// `fixed="…"` value the element *must* match if present.
    pub fixed:       Option<String>,
    /// `abstract="true"` — element cannot appear in instances; only
    /// substitutes for it can.
    pub abstract_:   bool,
    /// Head of substitution group this element substitutes into,
    /// resolved by the schema compiler.
    pub substitution_group: Option<QName>,
    /// `block="restriction|extension|substitution"` — disallowed
    /// derivation modes for substitutes.  Stored as a flag set.
    pub block:       BlockSet,
    /// `final="restriction|extension"` — derivation forms that must not
    /// derive from this element.
    pub final_:      BlockSet,
    /// `<xs:key>` / `<xs:keyref>` / `<xs:unique>` constraints declared
    /// directly on this element.  Empty for the common case.  Each
    /// constraint scopes to the subtree rooted at this element.
    pub identity:    Vec<super::identity::IdentityConstraint>,
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct BlockSet: u8 {
        const RESTRICTION = 0b00001;
        const EXTENSION   = 0b00010;
        const SUBSTITUTION = 0b00100;
        /// `final="list"` — block derivation of a list type whose
        /// `itemType` is this type (XSD §3.16.6).
        const LIST        = 0b01000;
        /// `final="union"` — block derivation of a union type whose
        /// `memberTypes` includes this type (XSD §3.16.6).
        const UNION       = 0b10000;
    }
}

// ── attribute declaration ────────────────────────────────────────────────────

/// `<xs:attribute>` — top-level or local.
#[derive(Debug)]
pub struct AttributeDecl {
    pub name:     QName,
    /// Always a simple type per spec.
    pub type_def: Arc<SimpleType>,
    pub default:  Option<String>,
    pub fixed:    Option<String>,
    /// XSD 1.1 § 3.2.2 — when `true`, this attribute's value is
    /// inherited into descendant elements' assertion / conditional-
    /// type-assignment contexts.  Default is `false`.  Today's
    /// validator records the flag for forward compatibility; it has
    /// no observable effect until `xs:assert` / `xs:alternative`
    /// land (those consume inherited attributes via the dynamic
    /// XPath context).  Compiling a schema with `inheritable="true"`
    /// requires [`SchemaVersion::Xsd11`].
    pub inheritable: bool,
}

/// How an attribute appears within a complex type.
#[derive(Debug, Clone)]
pub struct AttributeUse {
    pub use_kind: AttributeUseKind,
    pub decl:     Arc<AttributeDecl>,
    /// Local override of the declaration's default, if any.
    pub default:  Option<String>,
    /// Local override of the declaration's fixed value.
    pub fixed:    Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeUseKind { Required, Optional, Prohibited }

// ── XSD 1.1 assertions ───────────────────────────────────────────────────────

/// XSD 1.1 `<xs:assert test="…">` / `<xs:assertion test="…">`.
///
/// Captures an XPath 2.0 expression that's evaluated during
/// validation: against the element being validated (for `xs:assert`
/// on a complex type) or against the simple-type value bound to
/// `$value` (for `xs:assertion` inside a simple-type restriction).
/// The expression is stored as its source string and parsed lazily
/// by the validator.
#[derive(Debug, Clone)]
pub struct Assertion {
    /// The verbatim XPath expression from the `test=` attribute.
    pub test: String,
    /// Namespace bindings in scope at the assert/assertion element —
    /// used to resolve prefixed names in `test` at evaluation time.
    pub namespaces: Vec<(Option<String>, String)>,
    /// XSD 1.1 `xpathDefaultNamespace` — the URI an unprefixed
    /// element name in `test` defaults to.  `None` means the no-
    /// namespace default.
    pub xpath_default_namespace: Option<String>,
}

// ── content model ────────────────────────────────────────────────────────────

/// A type's body shape: nothing, a simple value, or a particle tree.
#[derive(Debug, Clone)]
pub enum ContentModel {
    /// `<xs:complexType><xs:complexContent>` with no children — empty
    /// element type.
    Empty,
    /// Simple-content type — body is a parsed value of a simple type
    /// (still allows attributes via the surrounding ComplexType).
    Simple(Arc<SimpleType>),
    /// Element-only or mixed content described by a particle tree.
    Complex { root: Particle, mixed: bool },
}

/// One particle in a content model — an element, a wildcard, or a
/// nested group — with occurrence bounds.
#[derive(Debug, Clone)]
pub struct Particle {
    pub min_occurs: u32,
    pub max_occurs: MaxOccurs,
    pub term:       Term,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxOccurs { Bounded(u32), Unbounded }

impl MaxOccurs {
    pub fn allows(self, n: u32) -> bool {
        match self {
            MaxOccurs::Bounded(m) => n <= m,
            MaxOccurs::Unbounded  => true,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Term {
    /// Reference to an element declaration (top-level by name, or a
    /// local declaration inlined into the particle).
    Element(Arc<ElementDecl>),
    /// Nested particle group: sequence / choice / all.  Particles live
    /// in an `Arc<[Particle]>` so the validator's cursor can clone it
    /// (one refcount bump) instead of allocating a fresh `Vec` per
    /// element entered.
    Group {
        kind:      GroupKind,
        particles: Arc<[Particle]>,
    },
    /// `<xs:any>` wildcard.
    Wildcard(Wildcard),
    /// `<xs:group ref="...">` reference to a named model group.
    /// Emitted by the parser; replaced with the referenced group's
    /// `Group` particle by a post-pass during schema compile.  The
    /// validator and DFA builder should never see this variant.
    GroupRef(QName),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind { Sequence, Choice, All }

/// `<xs:any>` / `<xs:anyAttribute>` wildcard configuration.
#[derive(Debug, Clone)]
pub struct Wildcard {
    pub namespaces:       NamespaceConstraint,
    pub process_contents: ProcessContents,
    /// XSD 1.1 § 3.10.4 `notQName` — element/attribute names excluded
    /// from the wildcard's match set.  Empty in 1.0 mode (the parser
    /// rejects the attribute there); only populated when the schema
    /// was compiled under [`SchemaVersion::Xsd11`].
    pub not_qnames:       Vec<QName>,
    /// XSD 1.1 § 3.10.4 `notNamespace` — namespaces excluded from
    /// the wildcard's match set.  Shape matches
    /// [`NamespaceConstraint::List`] — `None` entries are the
    /// no-namespace (`##local`); `##targetNamespace` is resolved
    /// at parse time.
    pub not_namespaces:   Vec<Option<Arc<str>>>,
    /// XSD 1.1 § 3.10.4 — the `##defined` token in a `notQName`
    /// list, meaning "any element/attribute with a top-level
    /// declaration in this schema."  Enforced at validation time:
    /// element wildcards consult the schema's element table, and
    /// `xs:anyAttribute` wildcards consult the attribute table.
    pub not_qname_defined:         bool,
    /// XSD 1.1 § 3.10.4 — the `##definedSibling` token, meaning
    /// "any element/attribute declared as a sibling in the
    /// enclosing complex type."  Enforced at validation time
    /// against the type's static element-decl set (for `xs:any`)
    /// or attribute-use set (for `xs:anyAttribute`).
    pub not_qname_defined_sibling: bool,
}

#[derive(Debug, Clone)]
pub enum NamespaceConstraint {
    /// `namespace="##any"`.
    Any,
    /// `namespace="##other"` — any namespace *except* the schema's
    /// target namespace and the no-namespace.
    Other,
    /// Explicit list.  `None` entries mean the no-namespace
    /// (`##local`).  `##targetNamespace` is resolved at compile time.
    List(Vec<Option<Arc<str>>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessContents { Strict, Lax, Skip }

// ── notation declaration ─────────────────────────────────────────────────────

#[derive(Debug)]
pub struct NotationDecl {
    pub name:      QName,
    pub public_id: Option<String>,
    pub system_id: Option<String>,
}

// ── type reference (simple/complex bridge) ───────────────────────────────────

/// A type reference — either a simple or complex type.  Used everywhere
/// an element's `type=...` is resolved.
#[derive(Debug, Clone)]
pub enum TypeRef {
    Simple(Arc<SimpleType>),
    Complex(Arc<ComplexType>),
}

// ── compiled schema ──────────────────────────────────────────────────────────

/// The top-level compiled schema.  Cheap to clone (one `Arc` bump).
///
/// All names are fully resolved at this point; the validator looks up
/// types and elements by [`QName`].
#[derive(Clone)]
pub struct Schema {
    inner: Arc<SchemaInner>,
}

impl std::fmt::Debug for Schema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Schema")
            .field("target_namespace", &self.inner.target_namespace)
            .field("elements",   &self.inner.elements.keys().collect::<Vec<_>>())
            .field("types",      &self.inner.types.keys().collect::<Vec<_>>())
            .field("attributes", &self.inner.attributes.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[allow(dead_code)] // attribute_groups / model_groups / notations consumed by PR3+
pub(crate) struct SchemaInner {
    pub target_namespace: Option<Arc<str>>,
    pub elements:         HashMap<QName, Arc<ElementDecl>>,
    pub attributes:       HashMap<QName, Arc<AttributeDecl>>,
    pub types:            HashMap<QName, TypeRef>,
    pub attribute_groups: HashMap<QName, Arc<AttributeGroup>>,
    pub model_groups:     HashMap<QName, Arc<ModelGroup>>,
    pub notations:        HashMap<QName, Arc<NotationDecl>>,
    /// For each substitution-group head, the list of element decls
    /// substitutable into it (computed at compile time from each
    /// element's `substitutionGroup` attribute).
    pub substitutions:    HashMap<QName, Vec<Arc<ElementDecl>>>,
}

impl Schema {
    pub(crate) fn from_inner(inner: SchemaInner) -> Self {
        Self { inner: Arc::new(inner) }
    }

    /// Look up a top-level element by qualified name.
    pub fn element(&self, name: &QName) -> Option<&Arc<ElementDecl>> {
        self.inner.elements.get(name)
    }

    /// Look up a top-level attribute by qualified name.
    pub fn attribute(&self, name: &QName) -> Option<&Arc<AttributeDecl>> {
        self.inner.attributes.get(name)
    }

    /// Look up a top-level type by qualified name.  Returns `None` for
    /// types not declared in this schema (callers handle built-ins via
    /// the [`BuiltinType`](super::types::BuiltinType) lookup).
    pub fn type_def(&self, name: &QName) -> Option<&TypeRef> {
        self.inner.types.get(name)
    }

    pub fn target_namespace(&self) -> Option<&str> {
        self.inner.target_namespace.as_deref()
    }

    /// Iterate every top-level element.  Useful for instance validation
    /// to find a candidate root type.
    pub fn elements(&self) -> impl Iterator<Item = (&QName, &Arc<ElementDecl>)> {
        self.inner.elements.iter()
    }

    /// Iterate every top-level type definition.
    pub fn types(&self) -> impl Iterator<Item = (&QName, &TypeRef)> {
        self.inner.types.iter()
    }

    /// Substitution-group members for a given head, if any.  Empty when
    /// no element substitutes for this head.
    pub fn substitutes_for(&self, head: &QName) -> &[Arc<ElementDecl>] {
        self.inner.substitutions.get(head).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

// ── grouping declarations ────────────────────────────────────────────────────

/// `<xs:attributeGroup>` — a named bundle of attribute uses, referenced
/// from complex types via `<xs:attributeGroup ref="…"/>`.
#[derive(Debug)]
pub struct AttributeGroup {
    pub name:       QName,
    pub attributes: Vec<AttributeUse>,
    pub any:        Option<Wildcard>,
}

/// `<xs:group>` — a named model-group particle, referenced from complex
/// types or other groups.
#[derive(Debug)]
pub struct ModelGroup {
    pub name:     QName,
    pub particle: Particle,
}
