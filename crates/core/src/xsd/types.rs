//! Core type-system data structures.
//!
//! `TypeDef` is the top-level: a type is either *simple* (lexical → value
//! with facets) or *complex* (an element-content type with attributes).

use std::sync::Arc;

use rust_decimal::Decimal;

use super::error::ValidationKind;
use super::facets::{Facet, FacetSet, FacetViolation};
use super::whitespace::WhitespaceMode;

// ── built-in type catalogue ──────────────────────────────────────────────────

/// One of the XSD built-in datatypes.  Simple types defined in user
/// schemas reference one of these as their ultimate base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinType {
    /// XSD §3.16.1 — the ur-simple-type: implicit base of every
    /// simple type.  Imposes no constraint beyond "is a string";
    /// any post-whitespace lexical value is accepted.
    AnySimpleType,
    // ── primitives ───────────────────────────────────────────────────
    String,
    Boolean,
    Decimal,
    Float,
    Double,
    Duration,
    DateTime,
    Time,
    Date,
    GYearMonth,
    GYear,
    GMonthDay,
    GDay,
    GMonth,
    HexBinary,
    Base64Binary,
    AnyUri,
    QName,
    Notation,

    // ── derived from string ──────────────────────────────────────────
    NormalizedString,
    Token,
    Language,
    NmToken,
    NmTokens,
    Name,
    NCName,
    Id,
    IdRef,
    IdRefs,
    Entity,
    Entities,

    // ── derived from decimal ─────────────────────────────────────────
    Integer,
    NonPositiveInteger,
    NegativeInteger,
    Long,
    Int,
    Short,
    Byte,
    NonNegativeInteger,
    UnsignedLong,
    UnsignedInt,
    UnsignedShort,
    UnsignedByte,
    PositiveInteger,

    // ── XSD 1.1 additions (§ 3.3) ────────────────────────────────────
    /// Restriction of `xs:dateTime` requiring a timezone offset
    /// (`explicitTimezone="required"`).  Lexical form is identical
    /// to `xs:dateTime`; the only difference is that values without
    /// a trailing `Z` / `±HH:MM` are rejected.
    DateTimeStamp,
    /// Restriction of `xs:duration` allowing only D / H / M / S
    /// components — `P1Y` and `P2M` are invalid.
    DayTimeDuration,
    /// Restriction of `xs:duration` allowing only Y / M components
    /// — `P1D`, `PT1H`, etc. are invalid.
    YearMonthDuration,
    /// Abstract supertype of every atomic primitive (the new
    /// intermediate node between `xs:anySimpleType` and the
    /// primitives).  Cannot be used to validate an instance
    /// directly; only as a base in a restriction chain.
    AnyAtomicType,
    /// The type whose value space is empty.  Every instance value
    /// is rejected.  Used together with `xs:alternative` to mark a
    /// conditional branch as "this case is an error."
    Error,
}

impl BuiltinType {
    /// The XSD-spec local name (e.g. `"string"`, `"nonNegativeInteger"`).
    pub fn name(self) -> &'static str {
        use BuiltinType::*;
        match self {
            String              => "string",
            Boolean             => "boolean",
            Decimal             => "decimal",
            Float               => "float",
            Double              => "double",
            Duration            => "duration",
            DateTime            => "dateTime",
            Time                => "time",
            Date                => "date",
            GYearMonth          => "gYearMonth",
            GYear               => "gYear",
            GMonthDay           => "gMonthDay",
            GDay                => "gDay",
            GMonth              => "gMonth",
            HexBinary           => "hexBinary",
            Base64Binary        => "base64Binary",
            AnyUri              => "anyURI",
            QName               => "QName",
            Notation            => "NOTATION",
            NormalizedString    => "normalizedString",
            Token               => "token",
            Language            => "language",
            NmToken             => "NMTOKEN",
            NmTokens            => "NMTOKENS",
            Name                => "Name",
            NCName              => "NCName",
            Id                  => "ID",
            IdRef               => "IDREF",
            IdRefs              => "IDREFS",
            Entity              => "ENTITY",
            Entities            => "ENTITIES",
            Integer             => "integer",
            NonPositiveInteger  => "nonPositiveInteger",
            NegativeInteger     => "negativeInteger",
            Long                => "long",
            Int                 => "int",
            Short               => "short",
            Byte                => "byte",
            NonNegativeInteger  => "nonNegativeInteger",
            UnsignedLong        => "unsignedLong",
            UnsignedInt         => "unsignedInt",
            UnsignedShort       => "unsignedShort",
            UnsignedByte        => "unsignedByte",
            PositiveInteger     => "positiveInteger",
            DateTimeStamp       => "dateTimeStamp",
            DayTimeDuration     => "dayTimeDuration",
            YearMonthDuration   => "yearMonthDuration",
            AnyAtomicType       => "anyAtomicType",
            AnySimpleType       => "anySimpleType",
            Error               => "error",
        }
    }

    /// True for the five built-in types added in XSD 1.1.  The
    /// schema compiler rejects these as a `base=` reference when
    /// running in strict 1.0 mode.
    pub fn is_xsd11_only(self) -> bool {
        use BuiltinType::*;
        matches!(self,
            DateTimeStamp | DayTimeDuration | YearMonthDuration
            | AnyAtomicType | Error)
    }

    /// Default whitespace mode per XSD §3.4.6.  `string` preserves;
    /// `normalizedString` replaces; everything else collapses.
    pub fn default_whitespace(self) -> WhitespaceMode {
        use BuiltinType::*;
        match self {
            String                              => WhitespaceMode::Preserve,
            NormalizedString                    => WhitespaceMode::Replace,
            _                                   => WhitespaceMode::Collapse,
        }
    }

    /// True for `xs:integer` and the integer subtypes (Long, Int,
    /// Short, Byte, NonPositiveInteger, NegativeInteger,
    /// NonNegativeInteger, PositiveInteger, UnsignedLong,
    /// UnsignedInt, UnsignedShort, UnsignedByte).
    pub fn is_integer_family(self) -> bool {
        use BuiltinType::*;
        matches!(self,
            Integer | Long | Int | Short | Byte
            | NonPositiveInteger | NegativeInteger
            | NonNegativeInteger | PositiveInteger
            | UnsignedLong | UnsignedInt | UnsignedShort | UnsignedByte)
    }

    /// The XSD-spec parent built-in, or `None` for the 19
    /// primitives that derive directly from `xs:anySimpleType`.
    /// Used by `xsi:type` derivation checks so a substituted
    /// built-in is recognised as a restriction of its declared
    /// base (e.g. `xs:integer` `→` `xs:decimal`).
    pub fn parent(self) -> Option<Self> {
        use BuiltinType::*;
        Some(match self {
            // Primitives — no built-in parent.
            String              => return None,
            Boolean             => return None,
            Decimal             => return None,
            Float               => return None,
            Double              => return None,
            Duration            => return None,
            DateTime            => return None,
            Time                => return None,
            Date                => return None,
            GYearMonth          => return None,
            GYear               => return None,
            GMonthDay           => return None,
            GDay                => return None,
            GMonth              => return None,
            HexBinary           => return None,
            Base64Binary        => return None,
            AnyUri              => return None,
            QName               => return None,
            Notation            => return None,

            // string-derived.
            NormalizedString    => String,
            Token               => NormalizedString,
            Language            => Token,
            Name                => Token,
            NCName              => Name,
            NmToken             => Token,
            NmTokens            => NmToken,  // list-of-NMTOKEN per spec
            Id                  => NCName,
            IdRef               => NCName,
            IdRefs              => IdRef,
            Entity              => NCName,
            Entities            => Entity,

            // decimal-derived.
            Integer             => Decimal,
            NonPositiveInteger  => Integer,
            NegativeInteger     => NonPositiveInteger,
            Long                => Integer,
            Int                 => Long,
            Short               => Int,
            Byte                => Short,
            NonNegativeInteger  => Integer,
            UnsignedLong        => NonNegativeInteger,
            UnsignedInt         => UnsignedLong,
            UnsignedShort       => UnsignedInt,
            UnsignedByte        => UnsignedShort,
            PositiveInteger     => NonNegativeInteger,

            // XSD 1.1 additions.
            DateTimeStamp       => DateTime,
            DayTimeDuration     => Duration,
            YearMonthDuration   => Duration,
            AnyAtomicType       => return None,
            AnySimpleType       => return None,
            Error               => return None,
        })
    }

    /// Walk the parent chain to the spec's PRIMITIVE built-in (the
    /// 19 that derive directly from `xs:anySimpleType`).  Two values
    /// of types sharing a primitive ancestor compare equal in the
    /// value space — that's the identity-constraint equality basis
    /// per XSD 1.0 §3.11.4 cvc-identity-constraint.4.2.2 / F&O
    /// op:numeric-equal, op:string-equal, etc.  Self if already a
    /// primitive (or a non-derived special like `anySimpleType` /
    /// `anyAtomicType` / `error` whose chain stops here).
    pub fn primitive(self) -> Self {
        let mut cur = self;
        while let Some(p) = cur.parent() {
            cur = p;
        }
        cur
    }

    /// True when `self` derives from `other` in the built-in
    /// hierarchy (including identity).  Always a chain of
    /// restrictions per XSD §3.16.6.
    pub fn derives_from(self, other: Self) -> bool {
        let mut cur = self;
        loop {
            if cur == other { return true; }
            match cur.parent() {
                Some(p) => cur = p,
                None    => return false,
            }
        }
    }

    /// Look up a built-in by its XSD local name (case-sensitive).
    pub fn from_name(name: &str) -> Option<Self> {
        use BuiltinType::*;
        Some(match name {
            "string"             => String,
            "boolean"            => Boolean,
            "decimal"            => Decimal,
            "float"              => Float,
            "double"             => Double,
            "duration"           => Duration,
            "dateTime"           => DateTime,
            "time"               => Time,
            "date"               => Date,
            "gYearMonth"         => GYearMonth,
            "gYear"              => GYear,
            "gMonthDay"          => GMonthDay,
            "gDay"               => GDay,
            "gMonth"             => GMonth,
            "hexBinary"          => HexBinary,
            "base64Binary"       => Base64Binary,
            "anyURI"             => AnyUri,
            "QName"              => QName,
            "NOTATION"           => Notation,
            "normalizedString"   => NormalizedString,
            "token"              => Token,
            "language"           => Language,
            "NMTOKEN"            => NmToken,
            "NMTOKENS"           => NmTokens,
            "Name"               => Name,
            "NCName"             => NCName,
            "ID"                 => Id,
            "IDREF"              => IdRef,
            "IDREFS"             => IdRefs,
            "ENTITY"             => Entity,
            "ENTITIES"           => Entities,
            "integer"            => Integer,
            "nonPositiveInteger" => NonPositiveInteger,
            "negativeInteger"    => NegativeInteger,
            "long"               => Long,
            "int"                => Int,
            "short"              => Short,
            "byte"               => Byte,
            "nonNegativeInteger" => NonNegativeInteger,
            "unsignedLong"       => UnsignedLong,
            "unsignedInt"        => UnsignedInt,
            "unsignedShort"      => UnsignedShort,
            "unsignedByte"       => UnsignedByte,
            "positiveInteger"    => PositiveInteger,
            "dateTimeStamp"      => DateTimeStamp,
            "dayTimeDuration"    => DayTimeDuration,
            "yearMonthDuration"  => YearMonthDuration,
            "anyAtomicType"      => AnyAtomicType,
            "anySimpleType"      => AnySimpleType,
            "error"              => Error,
            _ => return None,
        })
    }
}

// ── value space ──────────────────────────────────────────────────────────────

/// A parsed instance value of any built-in type.
///
/// Each variant holds the *value-space* representation (not the lexical
/// string).  Equality respects XSD canonical-equality rules: `1.0 == 1.00`
/// for decimals, normalized timezones for date/time, etc.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Bool(bool),
    Decimal(Decimal),
    /// 128-bit signed integer covers every XSD integer type up to
    /// `xs:long` and `xs:unsignedLong` without overflow.  Values above
    /// `i128::MAX` use `BigInt` (boxed) to keep `Value` small in the
    /// common case.
    Int(i128),
    BigInt(Box<num_overflow::BigInt>),
    Float(f32),
    Double(f64),
    Bytes(Vec<u8>),
    /// `anyURI`, `QName`, `NOTATION`, and the various Name-like types
    /// all reduce to a string after lexical validation.  They retain
    /// their kind via the type-def, not via the Value enum.
    Token(String),

    // ── date/time family ─────────────────────────────────────────────
    DateTime(super::datetime::XsdDateTime),
    Date(super::datetime::XsdDate),
    Time(super::datetime::XsdTime),
    GYearMonth(super::datetime::XsdGYearMonth),
    GYear(super::datetime::XsdGYear),
    GMonthDay(super::datetime::XsdGMonthDay),
    GDay(super::datetime::XsdGDay),
    GMonth(super::datetime::XsdGMonth),
    Duration(super::datetime::XsdDuration),
}

/// Placeholder big-integer for the rare cases above i128/u128 range.
/// We avoid pulling in the `num` crate just for this — most schemas
/// never hit it.  A lightweight pair-of-(sign, decimal-string) suffices
/// for equality and ordering.
pub mod num_overflow {
    /// Big integer kept as a sign + canonicalized decimal-string body.
    /// Comparison is lexicographic on equal-length zero-padded bodies.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BigInt {
        pub negative: bool,
        /// No leading zeros (except the literal "0").
        pub digits:   String,
    }
}

// ── type definitions ─────────────────────────────────────────────────────────

/// A simple type — atomic value, optional facets, references a built-in
/// base.  User-defined simple types get their facets populated by the
/// schema compiler; built-ins come with empty facet sets and rely on the
/// per-builtin range/character-set rules.
#[derive(Debug, Clone)]
pub struct SimpleType {
    /// The ultimate built-in this type derives from.  For
    /// [`Variety::List`] and [`Variety::Union`] this is meaningless
    /// for the outer type (validation routes through the variety) —
    /// conventionally set to `String`.
    pub builtin: BuiltinType,
    /// Constraining facets layered on top of the built-in (for
    /// atomic) or on the list/union as a whole (for list/union).
    /// For lists, `length`/`minLength`/`maxLength` are interpreted
    /// in items, not characters.
    pub facets:  FacetSet,
    /// Whitespace mode — defaulted from the built-in unless the schema
    /// explicitly overrides it.  Lists force `Collapse` per spec.
    pub whitespace: WhitespaceMode,
    /// Optional name, for diagnostics.  None for anonymous local types.
    pub name:    Option<Arc<str>>,
    /// Atomic, list-of-T, or union-of-Ts.  Most simple types are
    /// [`Variety::Atomic`]; list/union widen the value space.
    pub variety: Variety,
    /// XSD §3.14.6 — derivation methods that may not be used to
    /// derive new simple types from this one.  Honoured by the
    /// schema compiler when validating restriction/list/union bases.
    pub final_:  super::schema::BlockSet,
    /// XSD 1.1 `<xs:assertion test="…">` facets declared on this
    /// type.  Empty in XSD 1.0 schemas.  Evaluated at value-
    /// validation time with `$value` bound to the parsed value.
    pub assertions: Vec<super::schema::Assertion>,
}

/// XSD §2.5.1 — simple types come in three varieties.  The variant
/// determines how [`SimpleType::validate`] interprets the lexical input.
#[derive(Debug, Clone)]
pub enum Variety {
    /// Single value of the declared built-in.  This is the common case.
    Atomic,
    /// Whitespace-separated list of items, each validated against
    /// `item_type`.  Length facets count items, not characters.
    List { item_type: Arc<SimpleType> },
    /// Disjunction of member types.  The lexical input is accepted
    /// iff it validates against at least one member, in declaration
    /// order (first match wins per XSD §3.16).
    Union { members: Vec<Arc<SimpleType>> },
}

impl SimpleType {
    /// Build a bare wrapper around a built-in with no extra facets.
    pub fn of_builtin(b: BuiltinType) -> Self {
        Self {
            builtin:    b,
            facets:     FacetSet::default(),
            whitespace: b.default_whitespace(),
            name:       None,
            variety:    Variety::Atomic,
            final_:     super::schema::BlockSet::default(),
            assertions: Vec::new(),
        }
    }

    /// Apply this type's whitespace mode, parse the lexical value, and
    /// run every facet check.  Returns the parsed [`Value`] on success
    /// or a [`ValidationKind`] tag plus message on failure.
    ///
    /// Routing by variety:
    /// * Atomic — parse against `builtin`, then check facets.
    /// * List — split the (collapsed) input on whitespace; validate
    ///   each item against `item_type`; apply list-level facets where
    ///   `length`/`minLength`/`maxLength` count items.
    /// * Union — try each member in declaration order; first to
    ///   validate wins.
    pub fn validate(&self, raw: &str) -> Result<Value, TypeError> {
        match &self.variety {
            Variety::Atomic => self.validate_atomic(raw),
            Variety::List { item_type } => self.validate_list(raw, item_type),
            Variety::Union { members } => self.validate_union(raw, members),
        }
    }

    /// Like [`validate`](Self::validate) but discards the parsed
    /// [`Value`] — useful for callers that only want to know whether
    /// the lexical form is acceptable, not what the value was.
    ///
    /// Critically, for the (large) class of types whose only check
    /// is "is this a string post-whitespace?" with no facets to
    /// run, this short-circuits to `Ok(())` without parsing or
    /// allocating.  `xs:string` / `xs:normalizedString` /
    /// `xs:token` / `xs:anyType` / `xs:anySimpleType` declarations
    /// with no `<xs:restriction>` facets land here — common enough
    /// in real-world schemas (every "free-text field") that the
    /// fast path materially reduces per-element allocation pressure
    /// during validation.
    pub fn validate_only(&self, raw: &str) -> Result<(), TypeError> {
        if let Variety::Atomic = &self.variety {
            if self.facets.facets.is_empty() && accepts_any_string(self.builtin) {
                // No-op: the type imposes no lexical or value-space
                // constraint beyond "is a string" — and any
                // string is acceptable post-whitespace.
                let _ = raw;
                return Ok(());
            }
        }
        // Fall through to the full validator, then drop the Value.
        // The `validate` path still runs whitespace normalisation,
        // parses the lexical form, and walks the facet table —
        // none of which can be skipped for typed values (numeric,
        // date, enumerated, patterned, etc.).
        self.validate(raw).map(|_| ())
    }

    fn validate_atomic(&self, raw: &str) -> Result<Value, TypeError> {
        let normalized = self.whitespace.apply(raw);
        let value = parse_lexical(self.builtin, &normalized)?;
        // XSD §3.3.1 — NMTOKENS, IDREFS, ENTITIES are built-in
        // *list* types whose length/minLength/maxLength facets
        // count tokens, not characters. The Variety on the
        // SimpleType wrapper is Atomic for derivations rooted on
        // these built-ins, so we route length checks separately.
        let is_list_like = matches!(
            self.builtin,
            BuiltinType::NmTokens | BuiltinType::IdRefs | BuiltinType::Entities,
        );
        let item_count = if is_list_like {
            Some(normalized.split_whitespace().count())
        } else { None };
        // XSD §F.1 — length / minLength / maxLength have an
        // ·undefined· unit on QName and NOTATION; the facets are
        // accepted on the type but constrain nothing at validate
        // time. Skip the per-facet check for these built-ins.
        let length_is_undefined = matches!(
            self.builtin,
            BuiltinType::QName | BuiltinType::Notation,
        );
        for facet in &self.facets.facets {
            if length_is_undefined
                && matches!(facet, Facet::Length(_) | Facet::MinLength(_) | Facet::MaxLength(_))
            {
                continue;
            }
            if let Some(n) = item_count {
                match facet {
                    Facet::Length(k) => {
                        if n != *k {
                            return Err(TypeError {
                                kind:    ValidationKind::FacetViolation,
                                message: format!(
                                    "facet length violated: expected length {k}, got {n}"
                                ),
                            });
                        }
                        continue;
                    }
                    Facet::MinLength(k) => {
                        if n < *k {
                            return Err(TypeError {
                                kind:    ValidationKind::FacetViolation,
                                message: format!(
                                    "facet minLength violated: expected at least {k}, got {n}"
                                ),
                            });
                        }
                        continue;
                    }
                    Facet::MaxLength(k) => {
                        if n > *k {
                            return Err(TypeError {
                                kind:    ValidationKind::FacetViolation,
                                message: format!(
                                    "facet maxLength violated: expected at most {k}, got {n}"
                                ),
                            });
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            if let Err(v) = facet.check(&value, &normalized) {
                return Err(TypeError {
                    kind:    ValidationKind::FacetViolation,
                    message: format!("facet {} violated: {}", v.facet_name, v.detail),
                });
            }
        }
        Ok(value)
    }

    fn validate_list(&self, raw: &str, item_type: &Arc<SimpleType>) -> Result<Value, TypeError> {
        // List whitespace handling is implicitly `collapse` per spec.
        let items: Vec<&str> = raw.split_whitespace().collect();
        for (i, item) in items.iter().enumerate() {
            if let Err(e) = item_type.validate(item) {
                return Err(TypeError {
                    kind: e.kind,
                    message: format!("list item #{} ({:?}): {}", i + 1, item, e.message),
                });
            }
        }
        // List-level facets per XSD §2.5.1.3:
        //   * length / minLength / maxLength count items
        //   * pattern / enumeration apply to the whole lexical form
        //   * whiteSpace is always collapse (handled by tokenization)
        for facet in &self.facets.facets {
            match facet {
                Facet::Length(n) if items.len() != *n => {
                    return Err(TypeError {
                        kind:    ValidationKind::FacetViolation,
                        message: format!("facet length violated: list has {} item(s), expected {}",
                            items.len(), n),
                    });
                }
                Facet::MinLength(n) if items.len() < *n => {
                    return Err(TypeError {
                        kind:    ValidationKind::FacetViolation,
                        message: format!("facet minLength violated: list has {} item(s), needs ≥{}",
                            items.len(), n),
                    });
                }
                Facet::MaxLength(n) if items.len() > *n => {
                    return Err(TypeError {
                        kind:    ValidationKind::FacetViolation,
                        message: format!("facet maxLength violated: list has {} item(s), max {}",
                            items.len(), n),
                    });
                }
                Facet::Pattern(p) => {
                    // Pattern compares against the whole post-
                    // whitespace-collapse lexical form (the spec's
                    // "value space" string for the list).
                    let canon = items.join(" ");
                    if !p.is_match(&canon) {
                        return Err(TypeError {
                            kind:    ValidationKind::FacetViolation,
                            message: format!("facet pattern violated: list value {canon:?} does not match"),
                        });
                    }
                }
                Facet::Enumeration(opts) => {
                    // List enumeration compares value-sequences, not
                    // raw lexical strings.  Two lexically distinct
                    // inputs that tokenize to the same sequence
                    // (extra whitespace, leading/trailing) match
                    // a single enum entry.
                    let inst_items = items.clone();
                    let matched = opts.iter().any(|o|
                        o.split_whitespace().collect::<Vec<_>>() == inst_items
                    );
                    if !matched {
                        return Err(TypeError {
                            kind:    ValidationKind::FacetViolation,
                            message: format!("facet enumeration violated: list value {raw:?} not in {opts:?}"),
                        });
                    }
                }
                _ => {}
            }
        }
        // Return the raw string as a String value — the caller treats
        // list values as opaque (we already validated structure +
        // items).  Choosing String makes the existing facet machinery
        // happy if a later restriction overlays pattern/enumeration.
        Ok(Value::String(raw.to_string()))
    }

    fn validate_union(&self, raw: &str, members: &[Arc<SimpleType>]) -> Result<Value, TypeError> {
        let mut last_err: Option<TypeError> = None;
        let mut value: Option<Value> = None;
        for member in members {
            match member.validate(raw) {
                Ok(v) => { value = Some(v); break; }
                Err(e) => last_err = Some(e),
            }
        }
        let value = match value {
            Some(v) => v,
            None => return Err(TypeError {
                kind:    ValidationKind::TypeMismatch,
                message: match last_err {
                    Some(e) => format!("union: no member type accepts {:?} (last attempt: {})", raw, e.message),
                    None    => format!("union has no member types: cannot validate {:?}", raw),
                },
            }),
        };
        // XSD §2.5.1.3 — pattern and enumeration facets layered on
        // a union restriction apply to the lexical form of the
        // *union*, not the chosen member's value space.
        for facet in &self.facets.facets {
            match facet {
                Facet::Pattern(p) => {
                    if !p.is_match(raw) {
                        return Err(TypeError {
                            kind:    ValidationKind::FacetViolation,
                            message: format!("facet pattern violated: union value {raw:?} does not match"),
                        });
                    }
                }
                Facet::Enumeration(opts) => {
                    if !opts.iter().any(|o| o == raw) {
                        return Err(TypeError {
                            kind:    ValidationKind::FacetViolation,
                            message: format!("facet enumeration violated: union value {raw:?} not in {opts:?}"),
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(value)
    }
}

/// A complex type — content model + attribute uses.  Filled in by the
/// schema compiler.  See [`super::schema::ContentModel`].
#[derive(Debug)]
pub struct ComplexType {
    pub name: Option<super::schema::QName>,
    /// What this type derives from, if anything.  `None` for the
    /// implicit `xs:anyType` root.
    pub derivation:  Option<Derivation>,
    pub content:     super::schema::ContentModel,
    /// DFA built from `content` at schema-compile time.  When
    /// `ContentMatcher::Dfa(_)`, the validator walks the DFA in O(1)
    /// per child.  `ContentMatcher::All` falls back to the
    /// particle-walk matcher (xs:all has no clean DFA representation
    /// without exponential blowup).  `ContentMatcher::None` for
    /// empty/simple-content types.  Wrapped in `OnceLock` so the
    /// build can happen post-construction (after substitution-group
    /// resolution) without rebuilding `ComplexType`.
    pub matcher:     std::sync::OnceLock<super::dfa::ContentMatcher>,
    pub attributes:  Vec<super::schema::AttributeUse>,
    pub any_attribute: Option<super::schema::Wildcard>,
    pub abstract_:   bool,
    pub block:       super::schema::BlockSet,
    pub final_:      super::schema::BlockSet,
    /// Forward references to `<xs:attributeGroup ref="…"/>` particles
    /// inside this type that couldn't be expanded at parse time
    /// (declared later in source order).  Resolved + cleared during
    /// schema-compile post-pass; always empty on the returned
    /// [`Schema`](super::schema::Schema).
    #[doc(hidden)]
    pub pending_attribute_group_refs: Vec<super::schema::QName>,
    /// XSD 1.1 `<xs:assert test="…">` constraints declared on this
    /// type.  Empty in XSD 1.0 schemas (or when the assertions
    /// haven't been parsed yet).  Evaluated by the validator after
    /// child-content and attribute-use validation succeed.
    pub assertions: Vec<super::schema::Assertion>,
}

/// One derivation step in a complex-type chain.
#[derive(Debug, Clone)]
pub struct Derivation {
    pub method: DerivationMethod,
    /// What we derive from — either a previously declared complex type
    /// or a simple type (when this complex type has simple content).
    pub base:   super::schema::TypeRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivationMethod { Restriction, Extension }

/// A schema type — either simple or complex.  Schemas are graphs of
/// these reachable through the [`Schema`](super::schema::Schema) maps.
#[derive(Debug)]
pub enum TypeDef {
    Simple(SimpleType),
    Complex(ComplexType),
}

// ── error type ───────────────────────────────────────────────────────────────

/// Lower-level error from a single value-validation call.  Lifted to
/// [`ValidationIssue`](crate::xsd::ValidationIssue) by the runtime
/// validator with locator info.
#[derive(Debug, Clone)]
pub struct TypeError {
    pub kind:    ValidationKind,
    pub message: String,
}

impl TypeError {
    pub fn type_mismatch(s: impl Into<String>) -> Self {
        Self { kind: ValidationKind::TypeMismatch, message: s.into() }
    }

    pub fn facet(v: FacetViolation) -> Self {
        Self {
            kind:    ValidationKind::FacetViolation,
            message: format!("facet {} violated: {}", v.facet_name, v.detail),
        }
    }
}

/// True iff a `BuiltinType` admits every string post-whitespace —
/// i.e. its lexical-space check is a no-op.  These are the types
/// where a no-facet declaration imposes no constraint at all, so
/// validation can short-circuit before even running the parser.
///
/// `xs:string` / `xs:normalizedString` / `xs:token` differ from each
/// other in *whitespace handling* (applied earlier), not in lexical
/// acceptance — every post-whitespace string is a valid value.
/// `xs:anyAtomicType` is the abstract base of every atomic type; a
/// declaration that uses it (without further restriction) imposes
/// nothing beyond "is a string".
#[inline]
fn accepts_any_string(b: BuiltinType) -> bool {
    matches!(b, BuiltinType::String
              | BuiltinType::NormalizedString
              | BuiltinType::Token
              | BuiltinType::AnyAtomicType
              | BuiltinType::AnySimpleType)
}

// ── lexical-space parsers ────────────────────────────────────────────────────
//
// Filled in family-by-family.  The string family lives in this PR; numeric,
// date/time, and binary land in subsequent commits.  Each family adds its
// own arm to this match.

pub(crate) fn parse_lexical(b: BuiltinType, s: &str) -> Result<Value, TypeError> {
    use BuiltinType::*;
    match b {
        // String family.
        String              => parse_string_family(b, s),
        AnySimpleType       => parse_string_family(b, s),
        NormalizedString    => parse_string_family(b, s),
        Token               => parse_string_family(b, s),
        Language            => parse_language(s),
        NmToken             => parse_nmtoken(s),
        NmTokens            => parse_nmtokens(s),
        Name                => parse_name(s),
        NCName              => parse_ncname(s),
        Id                  => parse_ncname(s),
        IdRef               => parse_ncname(s),
        IdRefs              => parse_ncnames(s),
        Entity              => parse_ncname(s),
        Entities            => parse_ncnames(s),

        // Boolean.
        Boolean             => parse_boolean(s),

        // Numeric.
        Decimal             => super::lexical::parse_decimal(s),
        Float               => super::lexical::parse_float(s),
        Double              => super::lexical::parse_double(s),
        Integer             => super::lexical::parse_integer(s),
        NonPositiveInteger  => super::lexical::parse_non_positive(s),
        NegativeInteger     => super::lexical::parse_negative(s),
        Long                => super::lexical::parse_long(s),
        Int                 => super::lexical::parse_int(s),
        Short               => super::lexical::parse_short(s),
        Byte                => super::lexical::parse_byte(s),
        NonNegativeInteger  => super::lexical::parse_non_negative(s),
        UnsignedLong        => super::lexical::parse_unsigned_long(s),
        UnsignedInt         => super::lexical::parse_unsigned_int(s),
        UnsignedShort       => super::lexical::parse_unsigned_short(s),
        UnsignedByte        => super::lexical::parse_unsigned_byte(s),
        PositiveInteger     => super::lexical::parse_positive(s),

        // Date/time family.
        DateTime    => super::datetime::parse_date_time(s),
        Date        => super::datetime::parse_date(s),
        Time        => super::datetime::parse_time(s),
        GYearMonth  => super::datetime::parse_g_year_month(s),
        GYear       => super::datetime::parse_g_year(s),
        GMonthDay   => super::datetime::parse_g_month_day(s),
        GDay        => super::datetime::parse_g_day(s),
        GMonth      => super::datetime::parse_g_month(s),
        Duration    => super::datetime::parse_duration(s),

        // Binary / URI / QName / NOTATION.
        HexBinary    => super::lexical::parse_hex_binary(s),
        Base64Binary => super::lexical::parse_base64_binary(s),
        AnyUri       => super::lexical::parse_any_uri(s),
        QName        => super::lexical::parse_qname(s),
        Notation     => super::lexical::parse_notation(s),

        // ── XSD 1.1 additions ────────────────────────────────────────
        // `dateTimeStamp` is `dateTime` plus a required timezone.
        // Reuse the dateTime parser, then enforce the tz constraint.
        DateTimeStamp => {
            let v = super::datetime::parse_date_time(s)?;
            if let Value::DateTime(ref dt) = v {
                if dt.tz_min.is_none() {
                    return Err(TypeError::type_mismatch(
                        "xs:dateTimeStamp requires an explicit timezone offset"
                    ));
                }
            }
            Ok(v)
        }
        // `dayTimeDuration` rejects year / month components.
        DayTimeDuration => {
            let v = super::datetime::parse_duration(s)?;
            if let Value::Duration(ref d) = v {
                if d.months != 0 {
                    return Err(TypeError::type_mismatch(
                        "xs:dayTimeDuration may not contain year (Y) or month (M) components"
                    ));
                }
            }
            Ok(v)
        }
        // `yearMonthDuration` rejects day / hour / minute / second
        // components.
        YearMonthDuration => {
            let v = super::datetime::parse_duration(s)?;
            if let Value::Duration(ref d) = v {
                if d.seconds != 0 || d.nanos != 0 {
                    return Err(TypeError::type_mismatch(
                        "xs:yearMonthDuration may not contain day (D), hour (H), \
                         minute (M-after-T), or second (S) components"
                    ));
                }
            }
            Ok(v)
        }
        // `anyAtomicType` is abstract — it cannot be the type of an
        // instance value.  In practice schemas only ever name it via
        // `<xs:restriction base="xs:anyAtomicType">`, after which the
        // restriction yields a concrete derived type; if a value ends
        // up being parsed against the bare abstract base, that's a
        // schema-construction error and we surface it cleanly.
        AnyAtomicType => Err(TypeError::type_mismatch(
            "xs:anyAtomicType is abstract — instance values cannot have it as their type",
        )),
        // The empty-value-space type.  Every instance fails.
        Error => Err(TypeError::type_mismatch(
            "xs:error: no value is ever valid against this type",
        )),
    }
}

// ── string-family parsers ────────────────────────────────────────────────────
//
// Per-type validation rules from XSD §3.3 / §3.4 (Datatypes).  All operate
// on the *post-whitespace* value.

fn parse_string_family(b: BuiltinType, s: &str) -> Result<Value, TypeError> {
    // `string`/`normalizedString`/`token` accept any well-formed XML
    // chars.  Whitespace handling is the differentiator and runs before
    // we get here.  `normalizedString` additionally rejects literal
    // tab/CR/LF in the *normalized* value — but Replace already swapped
    // those, so the post-whitespace value cannot contain them.
    debug_assert!(matches!(b,
        BuiltinType::String
        | BuiltinType::NormalizedString
        | BuiltinType::Token
        | BuiltinType::AnySimpleType));
    Ok(Value::String(s.to_owned()))
}

fn parse_language(s: &str) -> Result<Value, TypeError> {
    // RFC 5646 / BCP 47, conservative: 1-8 alpha primary tag, optional
    // hyphen-separated subtags of 1-8 alphanumerics.  XSD spec uses an
    // older RFC but the regex `[a-zA-Z]{1,8}(-[a-zA-Z0-9]{1,8})*` covers
    // both.
    let mut parts = s.split('-');
    let primary = parts.next().ok_or_else(||
        TypeError::type_mismatch("empty language tag")
    )?;
    if primary.is_empty() || primary.len() > 8
        || !primary.bytes().all(|b| b.is_ascii_alphabetic())
    {
        return Err(TypeError::type_mismatch(
            format!("invalid primary language subtag: {primary:?}")
        ));
    }
    for sub in parts {
        if sub.is_empty() || sub.len() > 8
            || !sub.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(TypeError::type_mismatch(
                format!("invalid language subtag: {sub:?}")
            ));
        }
    }
    Ok(Value::Token(s.to_owned()))
}

fn parse_nmtoken(s: &str) -> Result<Value, TypeError> {
    if s.is_empty() {
        return Err(TypeError::type_mismatch("NMTOKEN cannot be empty"));
    }
    if !s.chars().all(is_xml_name_char) {
        return Err(TypeError::type_mismatch(
            format!("NMTOKEN contains non-NameChar: {s:?}")
        ));
    }
    Ok(Value::Token(s.to_owned()))
}

fn parse_nmtokens(s: &str) -> Result<Value, TypeError> {
    if s.is_empty() {
        return Err(TypeError::type_mismatch("NMTOKENS cannot be empty"));
    }
    for tok in s.split(' ') {
        let _ = parse_nmtoken(tok)?;
    }
    Ok(Value::Token(s.to_owned()))
}

fn parse_name(s: &str) -> Result<Value, TypeError> {
    let mut chars = s.chars();
    let first = chars.next().ok_or_else(||
        TypeError::type_mismatch("Name cannot be empty")
    )?;
    if !is_xml_name_start_char(first) {
        return Err(TypeError::type_mismatch(
            format!("Name starts with non-NameStartChar: {first:?}")
        ));
    }
    if !chars.all(is_xml_name_char) {
        return Err(TypeError::type_mismatch(
            format!("Name contains non-NameChar: {s:?}")
        ));
    }
    Ok(Value::Token(s.to_owned()))
}

fn parse_ncname(s: &str) -> Result<Value, TypeError> {
    // NCName is Name minus colons.
    let _ = parse_name(s)?;
    if s.contains(':') {
        return Err(TypeError::type_mismatch(
            format!("NCName cannot contain ':' (got {s:?})")
        ));
    }
    Ok(Value::Token(s.to_owned()))
}

fn parse_ncnames(s: &str) -> Result<Value, TypeError> {
    if s.is_empty() {
        return Err(TypeError::type_mismatch("IDREFS/ENTITIES cannot be empty"));
    }
    for tok in s.split(' ') {
        let _ = parse_ncname(tok)?;
    }
    Ok(Value::Token(s.to_owned()))
}

// ── boolean ──────────────────────────────────────────────────────────────────

fn parse_boolean(s: &str) -> Result<Value, TypeError> {
    match s {
        "true"  | "1" => Ok(Value::Bool(true)),
        "false" | "0" => Ok(Value::Bool(false)),
        _ => Err(TypeError::type_mismatch(format!("invalid boolean: {s:?}"))),
    }
}

// ── XML Name character classes (XML 1.0 §2.3 + Errata) ──────────────────────
//
// We need these here independently of the parser — XSD validates Name-like
// values without going through the parser's tokenizer.  Same character
// ranges, expressed as a function for clarity.

fn is_xml_name_start_char(c: char) -> bool {
    matches!(c,
        ':' | 'A'..='Z' | '_' | 'a'..='z'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}'
        | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}'
        | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}'
        | '\u{10000}'..='\u{EFFFF}'
    )
}

fn is_xml_name_char(c: char) -> bool {
    is_xml_name_start_char(c)
        || matches!(c,
            '-' | '.' | '0'..='9' | '\u{B7}'
            | '\u{0300}'..='\u{036F}' | '\u{203F}'..='\u{2040}'
        )
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn t(b: BuiltinType) -> SimpleType { SimpleType::of_builtin(b) }

    // ── string family ────────────────────────────────────────────────────

    #[test]
    fn string_preserves_whitespace() {
        let v = t(BuiltinType::String).validate("  hi  there  ").unwrap();
        assert_eq!(v, Value::String("  hi  there  ".into()));
    }

    #[test]
    fn normalized_string_replaces_ws() {
        let v = t(BuiltinType::NormalizedString).validate("a\tb\nc").unwrap();
        assert_eq!(v, Value::String("a b c".into()));
    }

    #[test]
    fn token_collapses_ws() {
        let v = t(BuiltinType::Token).validate("  a   b  c\t").unwrap();
        assert_eq!(v, Value::String("a b c".into()));
    }

    #[test]
    fn language_simple_tag() {
        let v = t(BuiltinType::Language).validate("en").unwrap();
        assert_eq!(v, Value::Token("en".into()));
    }

    #[test]
    fn language_with_region() {
        let v = t(BuiltinType::Language).validate("en-US").unwrap();
        assert_eq!(v, Value::Token("en-US".into()));
    }

    #[test]
    fn language_rejects_digit_primary() {
        assert!(t(BuiltinType::Language).validate("123").is_err());
    }

    #[test]
    fn language_rejects_too_long() {
        assert!(t(BuiltinType::Language).validate("toolongprimary").is_err());
    }

    #[test]
    fn nmtoken_accepts_digit_start() {
        let v = t(BuiltinType::NmToken).validate("123abc").unwrap();
        assert_eq!(v, Value::Token("123abc".into()));
    }

    #[test]
    fn nmtoken_rejects_space() {
        assert!(t(BuiltinType::NmToken).validate("a b").is_err());
    }

    #[test]
    fn nmtoken_rejects_empty() {
        assert!(t(BuiltinType::NmToken).validate("").is_err());
    }

    #[test]
    fn nmtokens_splits_on_single_space() {
        let v = t(BuiltinType::NmTokens).validate("a b c").unwrap();
        assert_eq!(v, Value::Token("a b c".into()));
    }

    #[test]
    fn name_rejects_digit_start() {
        assert!(t(BuiltinType::Name).validate("1a").is_err());
    }

    #[test]
    fn name_accepts_colon() {
        // Name allows colons (only NCName excludes them).
        let v = t(BuiltinType::Name).validate("ns:foo").unwrap();
        assert_eq!(v, Value::Token("ns:foo".into()));
    }

    #[test]
    fn ncname_rejects_colon() {
        assert!(t(BuiltinType::NCName).validate("ns:foo").is_err());
    }

    #[test]
    fn ncname_rejects_empty() {
        assert!(t(BuiltinType::NCName).validate("").is_err());
    }

    #[test]
    fn id_idref_idrefs_basic() {
        assert!(t(BuiltinType::Id).validate("section1").is_ok());
        assert!(t(BuiltinType::IdRef).validate("section1").is_ok());
        assert!(t(BuiltinType::IdRefs).validate("a b c").is_ok());
        assert!(t(BuiltinType::IdRefs).validate("a:bad b").is_err());
    }

    #[test]
    fn entity_entities_basic() {
        assert!(t(BuiltinType::Entity).validate("ent").is_ok());
        assert!(t(BuiltinType::Entities).validate("e1 e2").is_ok());
    }

    // ── boolean ──────────────────────────────────────────────────────────

    #[test]
    fn boolean_accepts_canonical_and_aliases() {
        for s in ["true", "false", "1", "0"] {
            assert!(t(BuiltinType::Boolean).validate(s).is_ok(), "{s}");
        }
    }

    #[test]
    fn boolean_rejects_other_strings() {
        for s in ["TRUE", "yes", "no", "2", ""] {
            assert!(t(BuiltinType::Boolean).validate(s).is_err(), "{s}");
        }
    }

    #[test]
    fn boolean_collapses_whitespace_first() {
        // collapse mode trims surrounding whitespace before parsing.
        assert!(t(BuiltinType::Boolean).validate("  true  ").is_ok());
    }

    // ── builtin name lookup ──────────────────────────────────────────────

    #[test]
    fn builtin_round_trip_through_name() {
        for b in [
            BuiltinType::String, BuiltinType::Boolean, BuiltinType::Decimal,
            BuiltinType::DateTime, BuiltinType::AnyUri, BuiltinType::QName,
            BuiltinType::NCName, BuiltinType::PositiveInteger,
        ] {
            assert_eq!(BuiltinType::from_name(b.name()), Some(b));
        }
    }

    #[test]
    fn builtin_rejects_unknown_name() {
        assert_eq!(BuiltinType::from_name("notAType"), None);
        assert_eq!(BuiltinType::from_name("STRING"),   None); // case-sensitive
    }
}
