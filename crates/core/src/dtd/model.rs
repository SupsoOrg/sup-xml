//! Type-level DTD vocabulary — Element/Attribute decls and content models.

/// An `<!ELEMENT name model>` declaration.
#[derive(Debug, Clone)]
pub struct ElementDecl {
    pub name:    String,
    pub content: ContentModel,
}

/// XML 1.0 § 3.2 element content classifications.
#[derive(Debug, Clone)]
pub enum ContentModel {
    /// `EMPTY` — no children of any kind allowed.
    Empty,
    /// `ANY` — anything goes (no validation against this element).
    Any,
    /// Mixed content per § 3.2.2 [51].  Either `(#PCDATA)*` (no
    /// child elements named, `choices` empty) or
    /// `(#PCDATA | a | b ...)*`.  Always carries the trailing `*`
    /// implicitly when `choices` is non-empty.
    Mixed { choices: Vec<String> },
    /// Children content per § 3.2.1 [47].  A nested group of
    /// element-name particles with sequence/choice/occurrence
    /// structure.
    Children(Group),
}

/// One Sequence or Choice with its own occurrence indicator —
/// matches the grammar in § 3.2.1 [49] / [50].
#[derive(Debug, Clone)]
pub struct Group {
    pub kind:  GroupKind,
    pub items: Vec<Particle>,
    pub occur: Occurrence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind { Sequence, Choice }

/// One element-name reference or a nested Group, with its own
/// `?` / `*` / `+` indicator.
#[derive(Debug, Clone)]
pub struct Particle {
    pub item:  Item,
    pub occur: Occurrence,
}

#[derive(Debug, Clone)]
pub enum Item {
    /// `Name` per [48] — refers to another element by GI.
    Name(String),
    /// Nested parenthesised group.
    Group(Box<Group>),
}

/// `?` / `*` / `+` per § 3.2.1 [47].  `One` = no suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occurrence {
    One,
    ZeroOrOne,
    ZeroOrMore,
    OneOrMore,
}

/// A reference to one internal/external-subset declaration in source
/// order, so the DTD serializer can reproduce libxml2's declaration
/// ordering (it walks `xmlDtd.children`, which is source order).
#[derive(Debug, Clone)]
pub enum DeclRef {
    /// `<!ELEMENT name …>` — index by name into `Dtd::elements`.
    Element(String),
    /// `<!ATTLIST name …>` — index by name into `Dtd::attlists`.
    Attlist(String),
    /// `<!ENTITY …>` — index into `Dtd::entities`.
    Entity(usize),
}

/// An `<!ENTITY name ...>` general-entity declaration — internal (with a
/// literal replacement text) or external (SYSTEM/PUBLIC, optionally
/// `NDATA`).  Mirrors the fields lxml reads off libxml2's `xmlEntity`
/// and what the DTD serializer reconstructs.
#[derive(Debug, Clone)]
pub struct EntityDecl {
    pub name: String,
    /// `true` for a parameter entity (`<!ENTITY % name …>`).
    pub parameter: bool,
    /// Replacement text as written, before reference expansion
    /// (libxml2's `orig`).  `None` for external entities.
    pub orig: Option<String>,
    /// Replacement text after character/entity-reference expansion
    /// (libxml2's `content`).  `None` for external entities.
    pub content: Option<String>,
    /// `SYSTEM` identifier (or the second literal of `PUBLIC`).
    pub system_id: Option<String>,
    /// `PUBLIC` identifier.
    pub public_id: Option<String>,
    /// `NDATA` notation name — present only for unparsed entities.
    pub ndata: Option<String>,
}

/// One row of an `<!ATTLIST element name type default>` block.
#[derive(Debug, Clone)]
pub struct AttDecl {
    pub name:    String,
    pub att_type: AttType,
    pub default:  AttDefault,
}

/// XML 1.0 § 3.3.1 attribute types.
#[derive(Debug, Clone)]
pub enum AttType {
    CData,
    Id,
    IdRef,
    IdRefs,
    Entity,
    Entities,
    Nmtoken,
    Nmtokens,
    /// `NOTATION (n1 | n2 | ...)`.  Stored verbatim for completeness;
    /// the validator does not currently cross-check against `<!NOTATION>`
    /// declarations.
    Notation(Vec<String>),
    /// `(v1 | v2 | ...)` — enumerated.  The attribute value at the
    /// document body must match one of these literally.
    Enumeration(Vec<String>),
}

/// XML 1.0 § 3.3.2 default declarations.
#[derive(Debug, Clone)]
pub enum AttDefault {
    /// `#REQUIRED`
    Required,
    /// `#IMPLIED`
    Implied,
    /// `#FIXED "value"` — attribute must equal `value` if present.
    Fixed(String),
    /// Literal default value — supplied if the attribute is absent
    /// from the document body.  v0.1 doesn't perform defaulting at
    /// parse time, so the validator just ignores this and accepts
    /// either presence or absence.
    Default(String),
}
