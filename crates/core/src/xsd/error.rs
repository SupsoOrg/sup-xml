//! Errors emitted by the schema compiler and validator.

use std::fmt;

/// Error returned by [`Schema::compile_str`](crate::xsd::Schema::compile_str)
/// and friends.  Schema compilation produces at most one error — the
/// compiler bails on the first problem since later phases assume a
/// well-formed type graph.
#[derive(Debug, Clone)]
pub struct SchemaCompileError {
    pub message: String,
    pub line:    Option<u32>,
    pub column:  Option<u32>,
}

impl SchemaCompileError {
    pub fn msg(s: impl Into<String>) -> Self {
        Self { message: s.into(), line: None, column: None }
    }

    pub fn at(mut self, line: u32, column: u32) -> Self {
        self.line   = Some(line);
        self.column = Some(column);
        self
    }
}

impl fmt::Display for SchemaCompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let (Some(l), Some(c)) = (self.line, self.column) {
            write!(f, "{l}:{c}: ")?;
        }
        f.write_str(&self.message)
    }
}

impl std::error::Error for SchemaCompileError {}

impl From<crate::error::XmlError> for SchemaCompileError {
    fn from(e: crate::error::XmlError) -> Self {
        let mut out = Self::msg(e.message);
        out.line   = e.line;
        out.column = e.column;
        out
    }
}

// ── validation ───────────────────────────────────────────────────────────────

/// Returned by [`Schema::validate_str`](crate::xsd::Schema::validate_str).
/// May contain one or many issues depending on
/// [`ValidationOptions::fail_fast`].
#[derive(Debug, Clone)]
pub struct ValidationError {
    pub issues: Vec<ValidationIssue>,
}

impl ValidationError {
    pub fn single(issue: ValidationIssue) -> Self {
        Self { issues: vec![issue] }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.issues.len() {
            0 => f.write_str("(no issues)"),
            1 => self.issues[0].fmt(f),
            n => write!(f, "{n} validation issues; first: {}", self.issues[0]),
        }
    }
}

impl std::error::Error for ValidationError {}

/// One validation problem discovered while walking an instance document.
#[derive(Debug, Clone)]
pub struct ValidationIssue {
    pub message: String,
    pub line:    Option<u32>,
    pub column:  Option<u32>,
    /// XPath-ish locator into the instance document — e.g.
    /// `/invoice/items/item[3]/@price`.  Empty string at the document root.
    pub path:    String,
    pub kind:    ValidationKind,
    /// Element names the content model expected at an
    /// [`UnexpectedElement`](ValidationKind::UnexpectedElement) failure —
    /// used by the libxml2-compat shim to render "Expected is ( … )".
    /// Empty for other kinds.
    pub expected: Vec<String>,
    /// The offending lexical value for a
    /// [`TypeMismatch`](ValidationKind::TypeMismatch); lets the shim
    /// render libxml2's "'value' is not a valid value …".
    pub value: Option<String>,
    /// The simple type's name (e.g. `"xs:integer"`) for a datatype
    /// mismatch — the "atomic type 'xs:integer'" in libxml2's wording.
    pub type_name: Option<String>,
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let (Some(l), Some(c)) = (self.line, self.column) {
            write!(f, "{l}:{c}: ")?;
        }
        if !self.path.is_empty() {
            write!(f, "at {}: ", self.path)?;
        }
        f.write_str(&self.message)
    }
}

/// Categorical tag for a validation issue.  Use for programmatic dispatch
/// rather than string-matching the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationKind {
    UnexpectedElement,
    UnexpectedAttribute,
    MissingRequiredElement,
    MissingRequiredAttribute,
    /// Lexical value doesn't match its declared type.
    TypeMismatch,
    /// Value parses but a facet (pattern, enumeration, length, range, …)
    /// rejects it.
    FacetViolation,
    /// `xs:key`-declared value appears more than once in scope.
    KeyNotUnique,
    /// `xs:keyref` references a key value that doesn't exist.
    KeyRefDangling,
    /// An element is in a substitution group but isn't substitutable for
    /// the expected head (incompatible type).
    SubstitutionMismatch,
    /// `xsi:nil="true"` on a non-nillable element, or with content.
    NillableViolation,
    /// XSD 1.1 `xs:assert` / `xs:assertion` evaluated to `false`
    /// against the instance.  cvc-assertion.
    AssertionViolation,
    Other,
}

// ── options ──────────────────────────────────────────────────────────────────

/// Tunables for [`Schema::validate_str_opts`](crate::xsd::Schema::validate_str_opts).
#[derive(Debug, Clone)]
pub struct ValidationOptions {
    /// Stop on the first issue.  Default `true`.  When `false`, the
    /// validator collects up to [`max_issues`](Self::max_issues) issues
    /// before returning.
    pub fail_fast:  bool,
    /// Cap on collected issues when `fail_fast` is off.  Default 1000.
    pub max_issues: usize,
    /// Augment the instance with schema-defined attribute value
    /// constraints: when an attribute with a `default=` / `fixed=` value
    /// is absent on an element, add it.  Only effective when validating a
    /// live [`Document`](sup_xml_tree::dom::Document) (the source can then
    /// mutate it); a no-op for string/byte sources.  Default `false`
    /// (mirrors libxml2's `XML_SCHEMA_VAL_VC_I_CREATE` opt-in).
    pub apply_attribute_defaults: bool,
}

impl Default for ValidationOptions {
    fn default() -> Self {
        Self { fail_fast: true, max_issues: 1000, apply_attribute_defaults: false }
    }
}
