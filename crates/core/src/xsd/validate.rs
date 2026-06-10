//! Instance-document validation against a compiled [`Schema`].
//!
//! Drives [`XmlReader`] events and walks the schema's content models in
//! parallel.  Errors carry line/column locators inherited from the
//! reader plus an XPath-style instance path (`/root/items/item[3]`).
//!
//! v1 implements:
//! * Element / attribute structural validation.
//! * Simple-type content validation against built-in types + facets.
//! * Sequence / choice / all content models with occurrence bounds.
//! * Substitution groups.
//! * `xsi:type` overrides and `xsi:nil="true"` (when the schema's
//!   `honor_xsi_nil` opt is on — currently always honored).
//! * Wildcards (`xs:any` / `xs:anyAttribute`) with strict / lax / skip.
//! * Identity constraints (`xs:key`, `xs:keyref`, `xs:unique`) —
//!   uniqueness enforcement plus keyref referential integrity across
//!   the constraint-declaring element's subtree.  Selectors and
//!   fields use the XSD §3.11.6 XPath subset.

use std::sync::Arc;

use rustc_hash::FxHashMap as HashMap;

use crate::reader::{Attr, EventInto, XmlReader};

use super::error::{ValidationError, ValidationIssue, ValidationKind, ValidationOptions};

mod walker;
pub(crate) use walker::DocumentEventSource;

/// Source of XML events that drives the XSD validator's state machine.
///
/// Both the streaming `XmlReader` path (used by `validate_str`) and
/// the arena-DOM walker (used by `validate_doc`) implement this
/// trait so the validator's content-model / namespace / identity-
/// constraint logic doesn't need to know which source it's reading
/// from.  Each method mirrors a single `XmlReader` accessor.
pub(crate) trait XsdEventSource<'x> {
    fn next_into(&mut self, attr_buf: &mut Vec<Attr<'x>>) -> crate::error::Result<EventInto<'x>>;
    /// Byte offset of the `<` of the most recently emitted
    /// StartElement; `None` if no StartElement has been emitted yet
    /// or the source can't supply byte offsets (e.g. the DOM walker).
    fn last_start_offset(&self) -> Option<usize>;
    /// Current source offset.  DOM walkers return 0.
    fn src_offset(&self) -> usize;
    /// Translate a byte offset to a 1-based `(line, column)` pair.
    /// DOM walkers return `(0, 0)` since they have no byte addressing.
    fn line_col_at(&self, offset: usize) -> (u32, u32);
    /// Add a default/fixed-value attribute to the *current* element (the
    /// one whose `StartElement` was most recently emitted).  Only the
    /// live-document source implements this; string/byte sources can't
    /// mutate their input and leave it a no-op.
    fn fill_default_attr(&self, _name: &str, _value: &str) {}
    /// Stable identity key (the node's address) of the element whose
    /// `StartElement` was most recently emitted, when the source can
    /// supply one.  The arena-DOM walker returns the live node's
    /// address so callers can recover per-node schema types after
    /// validation (see [`PsviTypes`]); byte/stream sources have no
    /// persistent nodes and return `None`.
    fn current_node_key(&self) -> Option<usize> { None }
}

impl<'x> XsdEventSource<'x> for XmlReader<'x> {
    #[inline] fn next_into(&mut self, buf: &mut Vec<Attr<'x>>) -> crate::error::Result<EventInto<'x>> {
        XmlReader::next_into(self, buf)
    }
    #[inline] fn last_start_offset(&self) -> Option<usize> {
        XmlReader::last_start_offset(self)
    }
    #[inline] fn src_offset(&self) -> usize {
        XmlReader::src_offset(self)
    }
    #[inline] fn line_col_at(&self, offset: usize) -> (u32, u32) {
        XmlReader::line_col_at(self, offset)
    }
}
use super::identity::{
    ConstraintKind, FieldPath, NameTest, PathExpr, PathStep, SelectorPath,
};
use super::schema::{
    AttributeUseKind, BlockSet, ContentModel, ElementDecl, GroupKind,
    Particle, ProcessContents, QName, Schema, Term, TypeRef, Wildcard,
};
use super::types::{BuiltinType, ComplexType, DerivationMethod, SimpleType};

// ── public entry points ──────────────────────────────────────────────────────

impl Schema {
    pub fn validate_str(&self, xml: &str) -> Result<(), ValidationError> {
        self.validate_str_opts(xml, ValidationOptions::default())
    }

    pub fn validate_bytes(&self, xml: &[u8]) -> Result<(), ValidationError> {
        let s = std::str::from_utf8(xml).map_err(|e| ValidationError::single(
            ValidationIssue {
                message: format!("invalid UTF-8: {e}"),
                line: None, column: None, path: String::new(),
                kind: ValidationKind::Other,
                expected: Vec::new(), value: None, type_name: None,
            }
        ))?;
        self.validate_str(s)
    }

    pub fn validate_str_opts(&self, xml: &str, opts: ValidationOptions)
        -> Result<(), ValidationError>
    {
        let mut v = Validator::new(self, xml, opts);
        v.run();
        if v.issues.is_empty() {
            Ok(())
        } else {
            Err(ValidationError { issues: v.issues })
        }
    }

    /// Validate an already-parsed [`Document`](sup_xml_tree::dom::Document)
    /// directly, without going through XML re-serialisation.
    ///
    /// Equivalent to serialising the document and calling
    /// [`validate_str`](Self::validate_str), but skips that
    /// round-trip — useful when the document came from
    /// [`parse_str`](crate::parse_str) or
    /// [`process_xincludes`](crate::xinclude::process_xincludes)
    /// and you'd otherwise burn a second parse pass.
    ///
    /// Diagnostic positioning trade-off: source byte offsets aren't
    /// available from a `Document` (the original bytes are gone),
    /// so reported issues carry `line: None` / `column: None`
    /// instead of the precise positions you'd get from
    /// `validate_str`.  Element paths (`/catalog/book[3]`) are
    /// reported the same either way.
    pub fn validate_doc(&self, doc: &sup_xml_tree::dom::Document)
        -> std::result::Result<(), ValidationError>
    {
        self.validate_doc_opts(doc, ValidationOptions::default())
    }

    /// As [`validate_doc`](Self::validate_doc), but honours an
    /// explicit [`ValidationOptions`].
    pub fn validate_doc_opts(
        &self, doc: &sup_xml_tree::dom::Document, opts: ValidationOptions,
    ) -> std::result::Result<(), ValidationError> {
        let source = DocumentEventSource::new(doc);
        let mut v = Validator::new_with_source(self, source, opts);
        v.run();
        if v.issues.is_empty() {
            Ok(())
        } else {
            Err(ValidationError { issues: v.issues })
        }
    }

    /// As [`validate_doc`](Self::validate_doc), but also returns
    /// [`PsviTypes`] — a per-node map of the schema type that governed
    /// each element during validation.
    ///
    /// This is the post-schema-validation infoset entry point that
    /// schema-aware XPath/XSLT processing builds on (typed atomization,
    /// `instance of element(*, T)`, `data()`).  The returned table is
    /// keyed by the addresses of `doc`'s nodes, so it is only valid
    /// while `doc` is alive.  Validation errors are reported in the
    /// `Result` exactly as `validate_doc` reports them; the annotations
    /// are returned regardless, so a caller that wants best-effort
    /// typing of a partially-valid document can ignore the `Result`.
    pub fn validate_doc_typed(&self, doc: &sup_xml_tree::dom::Document)
        -> (std::result::Result<(), ValidationError>, PsviTypes)
    {
        let source = DocumentEventSource::new(doc);
        let mut v = Validator::new_with_source(self, source, ValidationOptions::default());
        v.type_sink = Some(HashMap::default());
        v.run();
        let psvi = PsviTypes { by_node: v.type_sink.take().unwrap_or_default() };
        let res = if v.issues.is_empty() {
            Ok(())
        } else {
            Err(ValidationError { issues: v.issues })
        };
        (res, psvi)
    }
}

/// Post-schema-validation type annotations produced by
/// [`Schema::validate_doc_typed`].
///
/// Maps each validated element to the schema type that governed it,
/// keyed by the source node's address.  Identity is by address: the
/// document the table was built from must outlive every lookup (the
/// borrow on `validate_doc_typed` ties the contents to that document at
/// the call site).  Nodes validation never reached — e.g. content under
/// a skip wildcard — simply have no entry and return `None`.
#[derive(Default)]
pub struct PsviTypes {
    by_node: HashMap<usize, TypeRef>,
}

impl PsviTypes {
    /// Governing type recorded for `node`, or `None` if validation
    /// didn't assign one.
    pub fn governing_type(&self, node: &sup_xml_tree::dom::Node) -> Option<&TypeRef> {
        self.by_node.get(&(node as *const sup_xml_tree::dom::Node as usize))
    }

    /// True when no node received a type annotation.
    pub fn is_empty(&self) -> bool { self.by_node.is_empty() }
}

// ── internal driver ──────────────────────────────────────────────────────────

const XSI_NS: &str = "http://www.w3.org/2001/XMLSchema-instance";

struct Validator<'s, 'x, E: XsdEventSource<'x>> {
    schema:    &'s Schema,
    reader:    E,
    opts:      ValidationOptions,
    /// Lifetime witness so the validator inherits 'x from the event
    /// source's borrowed slices (attribute names/values, text
    /// content) without an extra explicit parameter on every method.
    _src:      std::marker::PhantomData<&'x str>,
    issues:    Vec<ValidationIssue>,
    /// Stack of in-progress element contexts.
    stack:     Vec<ElementCtx<'s>>,
    attr_buf:  Vec<Attr<'x>>,
    /// Prefix → namespace bindings active at the current depth (for
    /// resolving `xsi:type` and substitutionGroup QName values).
    ns_stack:  Vec<HashMap<String, String>>,
    /// Active identity-constraint scopes — pushed on entering an
    /// element with `<xs:key>`/`<xs:keyref>`/`<xs:unique>` declarations,
    /// popped (and validated) on leaving.
    key_scopes: Vec<KeyScope>,
    /// When `Some`, records each element's governing type keyed by
    /// source-node address as validation proceeds — the post-schema-
    /// validation infoset (see [`PsviTypes`]).  `None` on the ordinary
    /// pass/fail path so it costs nothing.
    type_sink: Option<HashMap<usize, TypeRef>>,
}

struct ElementCtx<'s> {
    decl:      Arc<ElementDecl>,
    /// Type used for validating *this* element — equal to `decl.type_def`
    /// unless overridden by `xsi:type`.
    type_def:  TypeRef,
    /// Cached attribute values for identity-constraint field eval.
    /// Populated when this element matched a constraint *or* when the
    /// parent is collecting child snapshots for multi-step field paths.
    /// Empty otherwise — zero overhead on the common path.
    cached_attrs: Vec<(QName, String)>,
    /// Index into `Validator::key_scopes` of the scope this element
    /// declared (if any).  `usize::MAX` means "no scope declared here."
    declared_scope: usize,
    /// For each scope-and-constraint pair this element matched, the
    /// (scope_idx, constraint_idx) pair to record at EndElement.
    matched_constraints: Vec<(usize, usize)>,
    /// How many levels of descendants below this element must be
    /// captured into [`child_snapshots`].  Set to the max of:
    ///   * own matched constraints' deepest field child-descent
    ///     (see [`max_field_child_descent`]), and
    ///   * `parent.collect_depth - 1` (so an ancestor's deep field
    ///     path propagates the capture requirement down the stack).
    /// Zero means "no descendant capture needed."
    collect_depth: usize,
    /// Snapshots of direct child elements — populated only when
    /// `collect_depth > 0`.  Each snapshot itself carries the next
    /// level of `children` when `collect_depth >= 2`, forming a
    /// `collect_depth`-deep tree used by [`eval_field`].
    child_snapshots: Vec<ChildSnapshot>,
    /// Marker lifetime — held by `cached_attrs` references back to the
    /// schema's QNames in some paths.
    _phantom: std::marker::PhantomData<&'s ()>,
    /// Position within the parent's content model.
    cursor:    ContentCursor,
    /// Buffer for accumulated text; flushed at EndElement.
    text_buf:  String,
    /// `xsi:nil="true"` on this element?
    is_nil:    bool,
    /// Full namespace-qualified name of this element.  Carried so that
    /// identity-constraint selector matching can compare against
    /// ancestor frames using the full namespace, not local-name only.
    /// Cost is one extra `Option<Arc<str>>` per stack frame — typical
    /// depth of 5-10 elements is negligible.
    ///
    /// Path locators built from `name.local` only (no `[n]` indices —
    /// see `current_path`).
    name:        QName,
    /// Did this element push a new namespace scope?  Used by
    /// [`pop_ns_scope_if`] at EndElement so we only pop when push
    /// actually happened.
    pushed_ns:   bool,
    /// Track which non-prohibited attributes were seen, for required-
    /// attribute checks at EndElement.
    seen_attrs: Vec<QName>,
    /// 1-based position among preceding-or-self siblings under the
    /// parent that share this element's local-name.  Used by
    /// [`current_path`] to disambiguate which `book` of fifty
    /// triggered an error.  Set to 1 for the root.
    sibling_index: u32,
    /// Per-child-local-name counters, used to assign
    /// [`sibling_index`] to each new child as it's pushed.  Keyed
    /// by local-name only — XSD typing matches on expanded names,
    /// but diagnostic paths read more naturally without prefixes.
    ///
    /// Stored as a flat Vec rather than a HashMap so an element with
    /// few distinct child names (the common case — most parents have
    /// 1–10 unique children) pays no allocation overhead beyond the
    /// Vec itself.  The Arc<str> key clones from the child's
    /// already-interned `QName::local` with no heap allocation
    /// (just an atomic refcount bump).  HashMap profiled at ~10%
    /// of validate time on customer1.xml; Vec form drops that to
    /// near-zero.
    child_counters: Vec<(Arc<str>, u32)>,
    /// Byte offset of this element's start tag within the source
    /// buffer.  Snapshot from `XmlReader::src_offset()` just before
    /// the StartElement event was consumed (so it points at the
    /// element's `<`, not past its `>`).  Used by [`Validator::report`]
    /// and [`Validator::report_at`] to translate to (line, column) on
    /// demand via [`XmlReader::line_col_at`].
    start_offset: u32,
}

/// Cursor into a parent's content model.  Three flavours:
///
/// * `None` — element has Empty or Simple content.
/// * `Dfa` — sequence/choice/element/wildcard models compiled to a
///   deterministic finite automaton at schema-compile time.  O(1) per
///   child element via [`Dfa::step`].
/// * `Group` (kept for `xs:all` only) — the existing particle-walk
///   matcher with bitset tracking, since `all` doesn't compile to a
///   DFA without exponential blowup.
enum ContentCursor {
    None,
    /// DFA-driven matching (sequence/choice/element/wildcard).
    Dfa {
        dfa:   Arc<super::dfa::Dfa>,
        state: super::dfa::StateId,
    },
    /// All-group fallback — each particle matches at most maxOccurs
    /// times in any order.
    Group {
        kind:        GroupKind,
        particles:   Arc<[Particle]>,
        idx:         usize,
        cur_count:   u32,
        all_seen:    HashMap<usize, u32>,
        /// `minOccurs` of the outer particle wrapping this group.
        /// When the group accepted zero children and `outer_min == 0`,
        /// per-child `minOccurs` checks are skipped — the user said
        /// "this whole all-group is optional."
        outer_min:   u32,
    },
}

/// One identity-constraint scope — pushed when we enter an element
/// that declares any constraints, popped (and validated) on exit.
///
/// Holds the declaring [`ElementDecl`] via `Arc::clone` so we don't
/// fight the borrow checker over the constraints' lifetime.  The
/// constraints are reachable via `scope.decl.identity`.
struct KeyScope {
    /// `self.stack.len()` at the moment this scope was pushed.  Used
    /// to compute relative paths when matching selectors.
    declaring_depth: usize,
    /// Byte offset of the declaring element's start tag — used to
    /// attribute uniqueness / keyref errors to the schema-defining
    /// element rather than wherever the scanner happens to be when
    /// finalize fires.
    declaring_offset: u32,
    decl: Arc<ElementDecl>,
    /// `decl.identity[i]` → list of field tuples collected so far.
    /// `None` represents a missing field (legal for `xs:unique`,
    /// rejected for `xs:key`).
    collected: Vec<Vec<KeyTuple>>,
}

/// One per matched selector — a tuple of field values (one slot per
/// `<xs:field>`).
type KeyTuple = Vec<Option<String>>;

/// Snapshot of a descendant element's identity-relevant data —
/// captured at EndElement time when an ancestor has multi-step field
/// paths to evaluate.  Recursive: `children` holds snapshots of this
/// element's own children when an ancestor's field path needs to
/// descend further.  The recursion depth is bounded by the deepest
/// child-step count across all in-scope field paths, computed once at
/// element-push time via [`max_field_child_descent`].
#[derive(Debug)]
#[derive(Clone)]
pub(super) struct ChildSnapshot {
    pub(super) name:     QName,
    pub(super) attrs:    Vec<(QName, String)>,
    pub(super) text:     String,
    pub(super) children: Vec<ChildSnapshot>,
}

/// Deepest child-step count across all alternatives of a field path.
/// `@attr` → 0, `child` → 1, `child/@attr` → 1, `a/b` → 2, `a/b/@c` → 2.
/// Determines how many levels of child snapshots the matched element
/// must capture to evaluate this field.
fn max_field_child_descent(fp: &FieldPath) -> usize {
    fp.paths.iter()
        .map(|p| p.steps.iter().filter(|s| matches!(s, PathStep::Child(_))).count())
        .max()
        .unwrap_or(0)
}

/// Increment the child-name counter for `local` on a parent
/// element's frame and return the new value (the 1-based sibling
/// index used in diagnostic paths like `/catalog/book[3]`).  Linear
/// scan over the parent's small counter table: real-world XSDs
/// have <20 distinct child names per type, so linear is cheaper
/// than a HashMap (no hashing, no String allocation per lookup —
/// the key is a cheap Arc<str> clone of the already-interned
/// `QName::local`).
fn bump_child_counter(counters: &mut Vec<(Arc<str>, u32)>, local: &Arc<str>) -> u32 {
    for (name, n) in counters.iter_mut() {
        if name.as_ref() == local.as_ref() {
            *n += 1;
            return *n;
        }
    }
    counters.push((local.clone(), 1));
    1
}

/// Capture an element's identity-relevant attributes — non-namespace,
/// non-`xsi:*` attrs with their parsed QNames.  Used both for the
/// matched element itself (for `@attr` field eval) and for snapshots
/// pushed to a parent that's collecting children for multi-step paths.
fn snapshot_attrs<F>(attrs: &[Attr], mut to_qname: F) -> Vec<(QName, String)>
where
    F: FnMut(&str) -> QName,
{
    attrs.iter()
        .filter(|a| {
            let n = a.name;
            !n.starts_with("xmlns") && !n.starts_with("xsi:")
        })
        .map(|a| (to_qname(a.name), a.value.to_string()))
        .collect()
}

impl<'s, 'x> Validator<'s, 'x, XmlReader<'x>> {
    /// Streaming-validator constructor — pulls events directly off the
    /// `XmlReader` for the bytes-in path used by `validate_str`.
    fn new(schema: &'s Schema, xml: &'x str, opts: ValidationOptions) -> Self {
        Self::new_with_source(schema, XmlReader::from_str(xml), opts)
    }
}

impl<'s, 'x, E: XsdEventSource<'x>> Validator<'s, 'x, E> {
    /// Source-generic constructor — used by both `new` (streaming
    /// reader) and `validate_doc` (DOM walker).
    fn new_with_source(schema: &'s Schema, source: E, opts: ValidationOptions) -> Self {
        Self {
            schema,
            reader: source,
            opts,
            issues: Vec::new(),
            stack: Vec::new(),
            attr_buf: Vec::new(),
            ns_stack: vec![HashMap::default()],
            key_scopes: Vec::new(),
            type_sink: None,
            _src: std::marker::PhantomData,
        }
    }

    /// Record the governing `type_def` for the element currently being
    /// validated, keyed by its source-node address.  No-op unless type
    /// collection is enabled (`type_sink` is `Some`).
    fn record_governing_type(&mut self, type_def: &TypeRef) {
        let Some(key) = self.reader.current_node_key() else { return };
        if let Some(sink) = self.type_sink.as_mut() {
            sink.insert(key, type_def.clone());
        }
    }

    // ── issue reporting ─────────────────────────────────────────────────

    fn report(&mut self, kind: ValidationKind, message: impl Into<String>) -> bool {
        // Default offset: the element currently being validated (top of
        // stack).  Falls back to the reader's current position when the
        // stack is empty (root-level / pre-element issues).
        let offset = self.stack.last()
            .map(|c| c.start_offset as usize)
            .unwrap_or_else(|| self.reader.src_offset());
        self.report_at(kind, message, offset)
    }

    /// Report an issue at a specific source byte offset.  Used when the
    /// issue's natural anchor isn't the top-of-stack element — e.g.
    /// "missing required element" detected at EndElement (use the
    /// just-ended element's offset, since by then it's already popped),
    /// or "unexpected element" detected before push (use the offset of
    /// the bad element itself, not its parent).
    fn report_at(&mut self, kind: ValidationKind, message: impl Into<String>, offset: usize) -> bool {
        let (line, col) = self.reader.line_col_at(offset);
        let path = self.current_path();
        let issue = ValidationIssue {
            message: message.into(),
            line: Some(line), column: Some(col),
            path,
            kind,
            expected: Vec::new(), value: None, type_name: None,
        };
        self.issues.push(issue);
        if self.opts.fail_fast || self.issues.len() >= self.opts.max_issues {
            return true; // signal caller to stop
        }
        false
    }

    fn current_path(&self) -> String {
        // Built only when an issue is reported — the success path skips
        // every per-element String allocation that previously happened
        // here (see `handle_start`).
        //
        // Each step is emitted as `/name[N]` when N > 1, plain `/name`
        // otherwise — readable for the common no-repeat case, but
        // points at the right one of N when sibling elements share
        // a name.  Saxon would always emit `[1]`; we drop it for the
        // single-occurrence case so simple paths don't read as
        // `/a[1]/b[1]/c[1]`.
        if self.stack.is_empty() { return String::new(); }
        let mut s = String::new();
        for ctx in &self.stack {
            s.push('/');
            s.push_str(&ctx.name.local);
            if ctx.sibling_index >= 2 {
                use std::fmt::Write;
                let _ = write!(s, "[{}]", ctx.sibling_index);
            }
        }
        s
    }

    // ── namespace bookkeeping ────────────────────────────────────────────

    /// Push a new namespace scope only when this element actually
    /// declares one (`xmlns=` or `xmlns:*`).  For the typical instance
    /// where namespaces are declared once at the root and inherited
    /// thereafter, every other element pays nothing here — we previously
    /// cloned the parent's HashMap per element.
    ///
    /// Returns `true` when a scope was pushed, `false` otherwise.  The
    /// caller stashes the bool on the [`ElementCtx`] so the matching
    /// `pop_ns_scope_if` knows whether to pop.
    fn push_ns_scope(&mut self, attrs: &[Attr<'x>]) -> bool {
        let has_ns = attrs.iter().any(|a| {
            let n = a.name;
            n == "xmlns" || n.starts_with("xmlns:")
        });
        if !has_ns { return false; }

        let mut new = self.ns_stack.last().cloned().unwrap_or_default();
        for a in attrs {
            let n = a.name;
            if n == "xmlns" {
                new.insert(String::new(), a.value.to_string());
            } else if let Some(prefix) = n.strip_prefix("xmlns:") {
                new.insert(prefix.to_string(), a.value.to_string());
            }
        }
        self.ns_stack.push(new);
        true
    }

    fn pop_ns_scope_if(&mut self, pushed: bool) {
        if pushed { self.ns_stack.pop(); }
    }

    fn resolve_prefix(&self, prefix: &str) -> Option<&str> {
        for scope in self.ns_stack.iter().rev() {
            if let Some(uri) = scope.get(prefix) {
                return Some(uri.as_str());
            }
        }
        None
    }

    /// Parse an element name from instance source.  Per XML
    /// Namespaces, an unprefixed element name takes the default
    /// namespace (xmlns="…") if one is bound, else lives in no
    /// namespace.  The schema's `targetNamespace` is NOT used as a
    /// fallback — that's a job for the schema's
    /// `elementFormDefault`, applied at schema-compile time.
    fn parse_element_qname(&self, raw: &str) -> QName {
        match raw.split_once(':') {
            Some((p, local)) => QName {
                namespace: self.resolve_prefix(p).map(Arc::from),
                local:     Arc::from(local),
            },
            None => QName {
                namespace: self.resolve_prefix("").map(Arc::from),
                local:     Arc::from(raw),
            },
        }
    }

    /// Parse an attribute name from instance source.  Per XML
    /// Namespaces, an unprefixed attribute is ALWAYS in no namespace
    /// (attributes do not pick up the default xmlns).
    fn parse_attribute_qname(&self, raw: &str) -> QName {
        match raw.split_once(':') {
            Some((p, local)) => QName {
                namespace: self.resolve_prefix(p).map(Arc::from),
                local:     Arc::from(local),
            },
            None => QName {
                namespace: None,
                local:     Arc::from(raw),
            },
        }
    }

    /// Parse a QName-valued attribute *value* (e.g. xsi:type,
    /// substitutionGroup).  Same namespace rules as element names:
    /// default xmlns or no namespace.
    fn parse_qname_value(&self, raw: &str) -> QName {
        self.parse_element_qname(raw)
    }

    // ── event loop ──────────────────────────────────────────────────────

    fn run(&mut self) {
        loop {
            let ev = match self.reader.next_into(&mut self.attr_buf) {
                Ok(ev) => ev,
                Err(e) => {
                    self.issues.push(ValidationIssue {
                        message: format!("XML parse error: {e}"),
                        line: e.line, column: e.column, path: self.current_path(),
                        kind: ValidationKind::Other,
                        expected: Vec::new(), value: None, type_name: None,
                    });
                    return;
                }
            };
            match ev {
                // EntityRef events only appear under
                // `resolve_entities=false`, which isn't a mode XSD
                // validation supports — schema-style assertions about
                // typed text/element content depend on the values
                // being post-expansion.  Skip them.
                EventInto::Comment(_)
                | EventInto::Pi { .. }
                | EventInto::EntityRef { .. } => continue,
                EventInto::StartElement { name } => {
                    // Snapshot the offset of the just-emitted start
                    // tag's `<` for downstream line/column attribution.
                    // `last_start_offset()` is updated by the reader
                    // inside its start-tag dispatch; falling back to
                    // `src_offset()` covers the entity-stream case
                    // where the source offset is meaningless (it'll
                    // point past the closing `>` — best we can do).
                    let event_offset = self.reader.last_start_offset()
                        .unwrap_or_else(|| self.reader.src_offset());
                    // Move `attr_buf` out of `self` so `handle_start`
                    // can call other `&mut self` methods while still
                    // reading the parsed attributes.  `mem::take` is
                    // zero-alloc — the buffer's heap allocation moves
                    // through the local var and back into `self`,
                    // preserving its capacity across iterations.
                    // The fresh `Vec::new()` left behind has zero
                    // capacity and zero allocation; the next
                    // `next_into` call will populate it (and the
                    // reader's eventual reuse of the now-emptied
                    // original buffer happens on the next start tag).
                    let attrs = std::mem::take(&mut self.attr_buf);
                    let bail = self.handle_start(name, &attrs, event_offset);
                    // Put the (capacity-preserving) buffer back so the
                    // reader can fill it on the next call without
                    // reallocating.
                    self.attr_buf = attrs;
                    if bail { return; }
                }
                EventInto::EndElement { .. } => {
                    if self.handle_end() { return; }
                }
                EventInto::Text(t) | EventInto::CData(t) => {
                    if let Some(ctx) = self.stack.last_mut() {
                        ctx.text_buf.push_str(&t);
                    }
                }
                EventInto::Eof => {
                    if !self.stack.is_empty() {
                        self.report(ValidationKind::Other,
                            "unexpected EOF (unclosed elements)");
                    }
                    return;
                }
            }
        }
    }

    fn handle_start(
        &mut self,
        name: std::borrow::Cow<'x, str>,
        attrs: &[Attr<'x>],
        event_offset: usize,
    ) -> bool {
        let pushed_ns = self.push_ns_scope(attrs);
        let qn = self.parse_element_qname(&name);

        // Locate the element decl: from the parent's content model on
        // the stack, or from the schema for the root.
        let decl = if let Some(parent) = self.stack.last_mut() {
            // Match against parent's cursor.
            let m = match_in_cursor(&qn, &mut parent.cursor, self.schema);
            match m {
                MatchOutcome::Element(decl) => decl,
                MatchOutcome::Wildcard(wc) => {
                    // Wildcards: validate per process_contents.
                    return self.handle_wildcard_match(&qn, attrs, wc, pushed_ns, event_offset);
                }
                MatchOutcome::None => {
                    // Attribute the "unexpected" error to the offending
                    // element itself — not its parent.  This is our
                    // native wording; the compat shim translates it to
                    // libxml2's `xmlschemas.c` phrasing for ABI consumers
                    // (see `libxml2_validation_message`), using the
                    // expected-element names the content model wanted.
                    let expected = expected_element_names(&parent.cursor);
                    let stop = self.report_at(ValidationKind::UnexpectedElement,
                        format!("unexpected element <{qn}>"), event_offset);
                    if let Some(last) = self.issues.last_mut() {
                        last.expected = expected;
                    }
                    if stop { return true; }
                    // Skip the body to keep parsing.
                    return self.skip_body_into_issues(pushed_ns);
                }
            }
        } else {
            // Root element.  XSD 1.0 §5.2 permits "type-only" validation
            // when a root element isn't globally declared but carries
            // `xsi:type=` — assess against that type with an
            // ad-hoc declaration.  Common in test suites where a small
            // schema only defines named types.
            match self.schema.element(&qn) {
                Some(d) => d.clone(),
                None => {
                    if let Some(xsi_type) = find_xsi_type(&attrs) {
                        let type_qn = self.parse_qname_value(xsi_type);
                        if let Some(tr) = self.lookup_type_by_qname(&type_qn) {
                            Arc::new(ElementDecl {
                                name:               qn.clone(),
                                type_def:           tr,
                                nillable:           true,
                                default:            None,
                                fixed:              None,
                                abstract_:          false,
                                substitution_group: None,
                                identity:           Vec::new(),
                                block:              super::schema::BlockSet::empty(),
                                final_:             super::schema::BlockSet::empty(),
                            })
                        } else {
                            self.report_at(ValidationKind::UnexpectedElement,
                                format!("root element <{qn}>: xsi:type {type_qn} not declared in schema"),
                                event_offset);
                            return self.skip_body_into_issues(pushed_ns);
                        }
                    } else {
                        self.report_at(ValidationKind::UnexpectedElement,
                            format!("root element <{qn}> not declared in schema"),
                            event_offset);
                        return self.skip_body_into_issues(pushed_ns);
                    }
                }
            }
        };

        // Resolve declared type → real type.
        let mut type_def = self.resolve_type(&decl.type_def);

        // xsi:type override — per XSD 1.0 §3.4.6, the override must
        // derive from the declared type (transitively, by restriction
        // or extension) AND that derivation must not be blocked by
        // the element's `block=` or the declared type's `final=`.
        //
        // Tracked here so the abstract-element check below can know
        // whether a valid concrete override was applied.  Per
        // cvc-elt-2, an abstract element declaration IS allowed in
        // an instance when accompanied by an `xsi:type` that
        // resolves to a non-abstract type derived from the declared
        // type — that's how abstract heads with xsi:type-based
        // polymorphism (SOAP, CloudEvents, audit logs) work.
        let mut xsi_type_override_applied = false;
        if let Some(xsi_type) = find_xsi_type(&attrs) {
            let override_qn = self.parse_qname_value(xsi_type);
            if let Some(t) = self.lookup_type_by_qname(&override_qn) {
                let declared = type_def.clone();
                match self.derivation_methods_from(&t, &declared) {
                    None => {
                        self.report_at(ValidationKind::TypeMismatch,
                            format!("xsi:type {override_qn} does not derive from declared type"),
                            event_offset);
                        // Keep declared type — best chance of producing
                        // useful downstream errors.
                    }
                    Some(methods) => {
                        // Block= on the element: any method in `methods`
                        // that's also in decl.block forbids the
                        // substitution.  Methods is empty for identity
                        // (T == D), so identity is never blocked.
                        // The declared type's own `block` also
                        // participates (cvc-type-2.x): a complexType
                        // declared with block="extension" forbids
                        // any xsi:type whose derivation chain includes
                        // extension.
                        let type_block = if let TypeRef::Complex(declared_ct) = &declared {
                            declared_ct.block
                        } else { BlockSet::empty() };
                        let element_blocked = (decl.block | type_block) & methods;
                        if !element_blocked.is_empty() {
                            self.report_at(ValidationKind::TypeMismatch,
                                format!(
                                    "xsi:type {override_qn} blocked by block={}",
                                    format_block_set(element_blocked),
                                ),
                                event_offset);
                            // Don't apply the override.
                        } else {
                            // Final= on the declared type's complex
                            // form: any method in `methods` that's in
                            // declared_ct.final_ forbids the
                            // derivation.  (Simple types: skip — we
                            // don't track final on built-ins.)
                            let final_blocked = if let TypeRef::Complex(declared_ct) = &declared {
                                declared_ct.final_ & methods
                            } else {
                                BlockSet::empty()
                            };
                            if !final_blocked.is_empty() {
                                self.report_at(ValidationKind::TypeMismatch,
                                    format!(
                                        "xsi:type {override_qn} blocked by base type final={}",
                                        format_block_set(final_blocked),
                                    ),
                                    event_offset);
                                // Don't apply the override.
                            } else {
                                // cvc-type: the type the element ends
                                // up assessed against must itself not
                                // be abstract.  An abstract xsi:type
                                // target doesn't rescue an abstract
                                // element decl from cvc-elt-2 below.
                                let target_is_abstract = matches!(&t,
                                    TypeRef::Complex(ct) if ct.abstract_);
                                if !target_is_abstract {
                                    type_def = t;
                                    xsi_type_override_applied = true;
                                } else {
                                    self.report_at(ValidationKind::TypeMismatch,
                                        format!("xsi:type {override_qn} is abstract"),
                                        event_offset);
                                }
                            }
                        }
                    }
                }
            } else {
                self.report_at(ValidationKind::TypeMismatch,
                    format!("xsi:type {override_qn:?} not declared in schema"),
                    event_offset);
            }
        }

        // XSD 1.0 §3.3.4 / cvc-elt-2: an element declared `abstract="true"`
        // must not appear in instance documents — only declarations that
        // substitute for it via `substitutionGroup=` can.  Match dispatch
        // resolves substitutes before we get here, so reaching this point
        // with `decl.abstract_` true means the literal abstract name was
        // used.  The exception (handled above) is when an `xsi:type`
        // attribute points to a non-abstract concrete subtype — then
        // the abstract head is acting as a typed slot, not as the
        // element being instantiated.
        if decl.abstract_ && !xsi_type_override_applied {
            self.report_at(ValidationKind::UnexpectedElement,
                format!("element <{qn}> is abstract and cannot appear in instances"),
                event_offset);
            return self.skip_body_into_issues(pushed_ns);
        }

        // Parse xsi:nil as xs:boolean (XSD 1.0 §3.2.2):
        //   "true"  | "1" → nil enabled
        //   "false" | "0" → nil disabled
        //   anything else → lexical error; treat the element as NOT
        //   nil so its content still gets validated and any further
        //   issues surface in the same pass.
        let is_nil = match find_xsi_nil(&attrs) {
            None => false,
            Some(raw) => match parse_xsi_nil(raw) {
                XsiNilParse::True  => true,
                XsiNilParse::False => false,
                XsiNilParse::Invalid(bad) => {
                    self.report_at(ValidationKind::TypeMismatch,
                        format!("xsi:nil value {bad:?} is not a valid xs:boolean \
                                 (expected \"true\", \"false\", \"1\", or \"0\")"),
                        event_offset);
                    false
                }
            },
        };
        if is_nil && !decl.nillable {
            // Element not yet pushed — anchor at its event_offset.
            self.report_at(ValidationKind::NillableViolation,
                format!("xsi:nil on non-nillable element <{qn}>"),
                event_offset);
        }

        // Validate attributes.  All issues raised here are anchored at
        // `event_offset` (the start of the current element) — the
        // element hasn't been pushed onto the stack yet, so the
        // default `report()` would attribute to the parent.
        let seen_attrs = self.validate_attrs_against_type(&type_def, &attrs, event_offset);

        // Build content cursor.  `xsi:nil` always emits None
        // (no children allowed); otherwise use the type's compiled
        // matcher (DFA when available, all-group fallback otherwise).
        let cursor = if is_nil { ContentCursor::None } else { build_cursor(&type_def) };

        // ── identity-constraint bookkeeping ────────────────────────
        //
        // Two things to do per element:
        //   1.  If THIS element's decl carries constraints, push a new
        //       scope.
        //   2.  For each ALREADY-active scope, check whether THIS
        //       element matches any of its constraints' selectors —
        //       if so, mark for field eval at EndElement.
        let declared_scope = if !decl.identity.is_empty() {
            let collected = vec![Vec::new(); decl.identity.len()];
            self.key_scopes.push(KeyScope {
                declaring_depth: self.stack.len(),
                declaring_offset: event_offset as u32,
                decl: decl.clone(),
                collected,
            });
            self.key_scopes.len() - 1
        } else {
            usize::MAX
        };

        // Parent (about to be replaced as `last`) may want a snapshot of
        // this child element for multi-step field eval.  Check before
        // we push so we know whether to cache attrs even if we
        // ourselves matched no constraint.
        let parent_collect_depth = self.stack.last()
            .map(|p| p.collect_depth).unwrap_or(0);
        let parent_collects = parent_collect_depth > 0;
        // XSD 1.1 `xs:assert` evaluation needs the full subtree of
        // every element whose complex type carries assertions, plus
        // that element's own attributes.  Detect the case so we can
        // force-snapshot.  Cheap: just a vec-empty check on the
        // resolved type definition.
        let has_assertions = matches!(
            &type_def, TypeRef::Complex(ct) if !ct.assertions.is_empty()
        );
        let need_capture = parent_collects || has_assertions;
        let (matched, mut cached_attrs) = self.check_active_scopes(&qn, &attrs);
        if cached_attrs.is_empty() && need_capture {
            cached_attrs = snapshot_attrs(&attrs, |s| self.parse_attribute_qname(s));
        }
        // Own descent need: the deepest child-step count across all
        // matched constraints' fields.  When this element has
        // assertions, treat the subtree as unbounded — XPath in a
        // test expression may walk arbitrarily deep.
        let own_collect_depth = if has_assertions {
            usize::MAX
        } else {
            matched.iter().map(|(si, ci)| {
                self.key_scopes[*si].decl.identity[*ci].fields.iter()
                    .map(max_field_child_descent)
                    .max().unwrap_or(0)
            }).max().unwrap_or(0)
        };
        // Ancestor propagation: if parent is collecting to depth N, we
        // (as one of its captured children) must collect N-1 below
        // ourselves so the snapshot tree reaches the required depth.
        let collect_depth = own_collect_depth
            .max(parent_collect_depth.saturating_sub(1));

        // Sibling index: per-name counter on the parent's frame.
        // The root has no parent and gets index 1.
        let sibling_index = match self.stack.last_mut() {
            Some(parent) => bump_child_counter(&mut parent.child_counters, &qn.local),
            None => 1,
        };

        self.record_governing_type(&type_def);
        self.stack.push(ElementCtx {
            decl,
            type_def,
            cursor,
            text_buf: String::new(),
            is_nil,
            name: qn,
            pushed_ns,
            seen_attrs,
            cached_attrs,
            declared_scope,
            matched_constraints: matched,
            collect_depth,
            child_snapshots: Vec::new(),
            start_offset: event_offset as u32,
            _phantom: std::marker::PhantomData,
            sibling_index,
            child_counters: Vec::new(),
        });
        false
    }

    /// For each active key scope, check whether `qn` matches any of the
    /// scope's constraints' selectors *at the current relative depth*.
    /// Returns the list of matches (scope_idx, constraint_idx) and the
    /// attribute snapshot (only populated when there's at least one
    /// match — otherwise we don't pay for it).
    fn check_active_scopes(
        &self,
        qn: &QName,
        attrs: &[Attr<'x>],
    ) -> (Vec<(usize, usize)>, Vec<(QName, String)>) {
        if self.key_scopes.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let depth = self.stack.len();
        let mut matched = Vec::new();
        for (si, scope) in self.key_scopes.iter().enumerate() {
            let rel = depth.saturating_sub(scope.declaring_depth);
            // Walk back through the path from this element up to the
            // declaring element to compare against each selector.
            for (ci, c) in scope.decl.identity.iter().enumerate() {
                if selector_matches(&c.selector, &self.stack, qn, rel) {
                    matched.push((si, ci));
                }
            }
        }
        let cached_attrs = if matched.is_empty() {
            Vec::new()
        } else {
            snapshot_attrs(attrs, |s| self.parse_attribute_qname(s))
        };
        (matched, cached_attrs)
    }

    /// Evaluate every XSD 1.1 `<xs:assertion>` facet on `st` against
    /// the lexical `value`, with `$value` bound to it.  Reports each
    /// failure as a separate validation issue at `ctx_off`.
    fn run_simple_type_assertions(
        &mut self,
        st: &super::types::SimpleType,
        value: &str,
        ctx_off: usize,
    ) {
        if st.assertions.is_empty() { return; }
        for a in &st.assertions {
            use super::assertion::{eval_simple_assertion, AssertOutcome};
            match eval_simple_assertion(a, value) {
                AssertOutcome::Pass => {}
                AssertOutcome::Fail => {
                    self.report_at(ValidationKind::AssertionViolation,
                        format!("assertion failed: {}", a.test), ctx_off);
                }
                AssertOutcome::Unevaluable(_) => {}
            }
        }
    }

    fn handle_end(&mut self) -> bool {
        let mut ctx = self.stack.pop().expect("EndElement with empty stack");
        self.pop_ns_scope_if(ctx.pushed_ns);

        // ── identity-constraint field eval ──────────────────────────
        // Evaluate fields for every constraint that matched this
        // element at start-time, recording into the matched scope.
        // Each Single value is canonicalised through its declared
        // (or xsi:type-overridden) simple type so XSD 1.0 §3.11.4
        // cvc-identity-constraint.4.2.2 value-space equality holds:
        // boolean("1") and decimal("1") don't collide just because
        // the lexical form happens to match, and decimal("1") and
        // decimal("1.0") DO match because they're the same value.
        let element_simple_type =
            self.field_dot_simple_type(&ctx.type_def);
        for (si, ci) in &ctx.matched_constraints {
            let constraint = self.key_scopes[*si].decl.identity[*ci].clone();
            let fields = constraint.fields.clone();
            // Canonicalising the field value through its simple type
            // closes XSD 1.0 §3.11.4.4.2.2's value-space equality
            // gap (boolean("1") ≠ decimal("1"), decimal("1") =
            // decimal("1.0")) — but only the `.` path resolves a type
            // today.  For canonicalisation to be sound across a
            // `<xs:key>` and its referring `<xs:keyref>`, BOTH sides
            // must canonicalise (otherwise canonical "decimal:5" would
            // never match raw "5.0"); fall back to raw lex when the
            // peers can't share the canonical form.
            let canonicalisable =
                fields.iter().all(field_path_is_dot)
                    && self.constraint_peers_all_dot(*si, *ci);
            let mut tuple: KeyTuple = Vec::with_capacity(fields.len());
            let mut ambiguous = false;
            for fp in fields {
                match eval_field(
                    &fp, &ctx.cached_attrs, &ctx.text_buf, &ctx.child_snapshots,
                ) {
                    FieldEval::Single(v) => {
                        let canon = if canonicalisable {
                            canonical_field_key(&v, element_simple_type.as_ref())
                        } else {
                            v
                        };
                        tuple.push(Some(canon));
                    }
                    FieldEval::Missing   => tuple.push(None),
                    FieldEval::Ambiguous => { ambiguous = true; tuple.push(None); }
                }
            }
            if ambiguous {
                // XSD §3.11.6: a field xpath must select at most one
                // node.  When multiple nodes match, the constraint
                // cannot produce a well-defined key value.
                self.report(ValidationKind::Other, format!(
                    "<xs:{} {:?}>: a field xpath selected more than one node",
                    match constraint.kind {
                        ConstraintKind::Key    => "key",
                        ConstraintKind::Unique => "unique",
                        ConstraintKind::KeyRef => "keyref",
                    },
                    constraint.name.local,
                ));
            }
            self.key_scopes[*si].collected[*ci].push(tuple);
        }
        // If we declared a scope here, it ends now: validate uniqueness
        // and resolve keyrefs.
        if ctx.declared_scope != usize::MAX {
            let scope = self.key_scopes.pop().expect("scope stack out of sync");
            self.finalize_key_scope(&scope);
        }
        // Push a snapshot to the parent for multi-step field eval, when
        // applicable.  We pass ownership of our own child_snapshots so
        // the parent has the full recursive tree for deep field paths.
        if let Some(parent) = self.stack.last_mut() {
            if parent.collect_depth > 0 {
                parent.child_snapshots.push(ChildSnapshot {
                    name:     ctx.name.clone(),
                    attrs:    ctx.cached_attrs.clone(),
                    text:     ctx.text_buf.clone(),
                    children: std::mem::take(&mut ctx.child_snapshots),
                });
            }
        }

        // Cursor / content checks below all describe ctx (the
        // just-ended element) — but ctx is already popped so the
        // default report() would attribute to the parent.  Anchor each
        // issue at ctx.start_offset instead.
        let ctx_off = ctx.start_offset as usize;

        // 1. Final occurrence checks for the cursor we just finished.
        if let ContentCursor::Dfa { dfa, state } = &ctx.cursor {
            if !dfa.is_accept(*state) {
                // Surface the element names this state could transition
                // on — those are exactly the "expected next" elements
                // the schema author meant to require.
                let expected: Vec<String> = dfa.states[*state as usize]
                    .on_element.iter()
                    .map(|t| t.name.local.to_string())
                    .collect();
                let msg = if expected.is_empty() {
                    "element content does not match the schema (no valid continuation)".to_string()
                } else if expected.len() == 1 {
                    format!("missing required element <{}>", expected[0])
                } else {
                    format!("missing required element (one of: {})", expected.join(", "))
                };
                self.report_at(ValidationKind::MissingRequiredElement, msg, ctx_off);
            }
        }
        if let ContentCursor::Group { kind, particles, idx, cur_count, all_seen, outer_min, .. } = &ctx.cursor {
            match kind {
                GroupKind::Sequence => {
                    // Verify any unfilled particles meet their min_occurs == 0.
                    if *idx < particles.len() {
                        // Check current first.
                        let p = &particles[*idx];
                        if *cur_count < p.min_occurs {
                            let _ = self.handle_min_occurs_violation(p, ctx_off);
                        }
                        // Then any tail particles.
                        for p in &particles[*idx + 1..] {
                            if p.min_occurs > 0 {
                                let _ = self.handle_min_occurs_violation(p, ctx_off);
                            }
                        }
                    }
                }
                GroupKind::Choice => {
                    // If no choice was ever taken, the choice itself has
                    // an occurrence requirement of 1 implicitly.
                    if *cur_count == 0 && !particles.is_empty() {
                        self.report_at(ValidationKind::MissingRequiredElement,
                            "no branch of <xs:choice> matched", ctx_off);
                    }
                }
                GroupKind::All => {
                    // An all-group with `minOccurs="0"` is allowed to
                    // not occur at all — when nothing matched, per-
                    // child min checks are skipped.
                    let saw_anything = all_seen.values().any(|&n| n > 0);
                    if *outer_min == 0 && !saw_anything {
                        // Whole all-group skipped — legal.
                    } else {
                        for (i, p) in particles.iter().enumerate() {
                            let seen = all_seen.get(&i).copied().unwrap_or(0);
                            if seen < p.min_occurs {
                                let _ = self.handle_min_occurs_violation(p, ctx_off);
                            }
                        }
                    }
                }
            }
        }

        // 2. Simple-content / nil / empty validation.
        match (&ctx.type_def, ctx.is_nil) {
            (_, true) => {
                // xsi:nil — element must be empty.
                if !ctx.text_buf.trim().is_empty() {
                    self.report_at(ValidationKind::NillableViolation,
                        "xsi:nil element must have empty content", ctx_off);
                }
            }
            (TypeRef::Simple(st), _) => {
                let real = self.resolve_simple_type(st);
                // XSD 1.0 §3.3.4 cvc-elt-5: an empty element with a
                // `default=` or `fixed=` value validates as that value
                // (the schema-supplied "value" stands in for empty
                // lexical content).
                let to_check = effective_text(&ctx.text_buf, ctx.decl.default.as_deref(),
                                              ctx.decl.fixed.as_deref());
                if let Err(e) = real.validate_only(to_check) {
                    let elem_local = ctx.decl.name.local.to_string();
                    self.report_at(e.kind, format!("element content: {}", e.message), ctx_off);
                    if let Some(last) = self.issues.last_mut() {
                        last.value = Some(to_check.to_string());
                        last.type_name = Some(format!("xs:{}", real.builtin.name()));
                        // Include the positional `[N]` (matching
                        // `current_path`) so the locator points at the right
                        // one of N same-named siblings — the element's frame
                        // is already popped from `self.stack` at content-check
                        // time, so its index isn't in `last.path`.
                        let idx = if ctx.sibling_index >= 2 {
                            format!("[{}]", ctx.sibling_index)
                        } else {
                            String::new()
                        };
                        last.path = format!("{}/{elem_local}{idx}", last.path);
                    }
                }
                // XSD 1.1 `<xs:assertion>` facets on the simple type —
                // evaluate `$value`-bound test expressions against
                // the lexical input.
                self.run_simple_type_assertions(&real, to_check, ctx_off);
            }
            (TypeRef::Complex(ct), _) => {
                match &ct.content {
                    ContentModel::Empty => {
                        if !ctx.text_buf.trim().is_empty() {
                            self.report_at(ValidationKind::TypeMismatch,
                                "complexType with empty content cannot have text", ctx_off);
                        }
                    }
                    ContentModel::Simple(st) => {
                        // Inline anonymous complex types on local element
                        // decls don't go through `merge_inline_extension_in_elements`
                        // (which only walks top-level entries), so a
                        // `<xs:simpleContent><xs:extension base="T">` body
                        // still carries the xs:string placeholder that
                        // [`parse_derivation_body`] seeded.  Resolve the
                        // extension's declared base lazily here so facet
                        // validation sees the real simple type.
                        let effective = self.simple_content_effective_type(ct, st);
                        let real = self.resolve_simple_type(&effective);
                        let to_check = effective_text(&ctx.text_buf, ctx.decl.default.as_deref(),
                                                      ctx.decl.fixed.as_deref());
                        if let Err(e) = real.validate_only(to_check) {
                            let elem_local = ctx.decl.name.local.to_string();
                            self.report_at(e.kind,
                                format!("element content: {}", e.message), ctx_off);
                            if let Some(last) = self.issues.last_mut() {
                                last.value = Some(to_check.to_string());
                                last.type_name = Some(format!("xs:{}", real.builtin.name()));
                                // Include the positional `[N]` (matching
                        // `current_path`) so the locator points at the right
                        // one of N same-named siblings — the element's frame
                        // is already popped from `self.stack` at content-check
                        // time, so its index isn't in `last.path`.
                        let idx = if ctx.sibling_index >= 2 {
                            format!("[{}]", ctx.sibling_index)
                        } else {
                            String::new()
                        };
                        last.path = format!("{}/{elem_local}{idx}", last.path);
                            }
                        }
                        self.run_simple_type_assertions(&real, to_check, ctx_off);
                    }
                    ContentModel::Complex { mixed, .. } => {
                        if !*mixed && !ctx.text_buf.trim().is_empty() {
                            self.report_at(ValidationKind::TypeMismatch,
                                "non-mixed complexType cannot have text content", ctx_off);
                        }
                    }
                }
            }
        }

        // `fixed=` on the declaration applies regardless of simple vs
        // complex content (per XSD §3.3.4, equality is *value-space* —
        // we approximate with whitespace-trimmed lexical equality, which
        // matches the spec for every built-in whose canonical lexical
        // form is the trimmed lexical input).
        //
        // Carve-out: when `xsi:nil="true"` is set, the element is
        // treated as having no value, so `fixed=` doesn't apply.
        // Otherwise every nillable+fixed element would always fail
        // under nil (text_buf "" never equals fixed value).
        if !ctx.is_nil
            && let Some(fixed) = &ctx.decl.fixed
            // Empty content takes the fixed value implicitly (cvc-elt-5.2.2.2);
            // a non-empty content must match it lexically (modulo whitespace).
            && !ctx.text_buf.is_empty()
            && ctx.text_buf.trim() != fixed.trim()
        {
            self.report_at(ValidationKind::TypeMismatch,
                format!("element content {:?} doesn't match fixed value {:?}",
                    ctx.text_buf, fixed), ctx_off);
        }

        // _seen_attrs is currently used only for the required-attr
        // pre-check at start time; keep it referenced to silence dead-code.
        let _ = ctx.seen_attrs;

        // 3. XSD 1.1 `xs:assert` evaluation.  Runs after content/
        // attribute/fixed checks so an instance that's already
        // structurally invalid doesn't get a redundant assertion
        // failure on top.  Each assertion sees the full subtree we
        // snapshotted under `collect_depth` — built lazily here.
        if let TypeRef::Complex(ct) = &ctx.type_def {
            if !ct.assertions.is_empty() {
                let snapshot = ChildSnapshot {
                    name:     ctx.name.clone(),
                    attrs:    ctx.cached_attrs.clone(),
                    text:     ctx.text_buf.clone(),
                    children: ctx.child_snapshots.clone(),
                };
                for a in &ct.assertions {
                    use super::assertion::{eval_complex_assert, AssertOutcome};
                    match eval_complex_assert(a, &snapshot) {
                        AssertOutcome::Pass => {}
                        AssertOutcome::Fail => {
                            self.report_at(ValidationKind::AssertionViolation,
                                format!("assertion failed: {}", a.test), ctx_off);
                        }
                        AssertOutcome::Unevaluable(_why) => {
                            // Treat unsupported XPath in the assertion
                            // as pass — better to false-negative than
                            // false-positive while the evaluator's
                            // feature surface is incomplete.
                        }
                    }
                }
            }
        }

        false
    }

    fn handle_wildcard_match(
        &mut self,
        qn: &QName,
        attrs: &[Attr<'x>],
        wc: Wildcard,
        pushed_ns: bool,
        event_offset: usize,
    ) -> bool {
        match wc.process_contents {
            ProcessContents::Skip => {
                self.skip_body_into_issues(pushed_ns)
            }
            ProcessContents::Lax => {
                if let Some(decl) = self.schema.element(qn) {
                    let decl = decl.clone();
                    self.push_decl_ctx(qn, decl, attrs, pushed_ns, event_offset);
                    false
                } else {
                    self.skip_body_into_issues(pushed_ns)
                }
            }
            ProcessContents::Strict => {
                if let Some(decl) = self.schema.element(qn) {
                    let decl = decl.clone();
                    self.push_decl_ctx(qn, decl, attrs, pushed_ns, event_offset);
                    false
                } else {
                    self.report_at(ValidationKind::UnexpectedElement,
                        format!("strict wildcard: <{qn}> not declared in schema"),
                        event_offset);
                    self.skip_body_into_issues(pushed_ns)
                }
            }
        }
    }

    /// Validate `attrs` against the complex type's attribute uses
    /// and anyAttribute wildcard, reporting unexpected, prohibited,
    /// or missing-required attribute issues. Returns the set of
    /// QNames that matched a use (for downstream identity-constraint
    /// processing). No-op when the type isn't complex.
    fn validate_attrs_against_type(
        &mut self,
        type_def:     &TypeRef,
        attrs:        &[Attr<'x>],
        event_offset: usize,
    ) -> Vec<QName> {
        let mut seen_attrs = Vec::with_capacity(attrs.len());
        let TypeRef::Complex(ct) = type_def else { return seen_attrs };
        for a in attrs {
            let aname = a.name;
            if aname == "xmlns" || aname.starts_with("xmlns:") { continue; }
            // Split into (prefix, local) without allocating.  Per
            // XML Namespaces, unprefixed attributes are in *no*
            // namespace (they never pick up the default xmlns).
            let (prefix_opt, local): (Option<&str>, &str) =
                match aname.split_once(':') {
                    Some((p, l)) => (Some(p), l),
                    None         => (None, aname),
                };
            let ns_uri: Option<&str> = prefix_opt.and_then(|p| self.resolve_prefix(p));
            if ns_uri == Some(XSI_NS) { continue; }
            // Lookup by raw (uri, local) — no QName allocation on
            // the matched path.  We only build the heap-allocated
            // QName for diagnostic / seen_attrs entries when an
            // attribute matched a decl (clone the decl's Arc) or
            // when an error needs reporting (lazy build).
            // XSD §3.4.2 — `use="prohibited"` is a derivation-restriction
            // device that removes a base type's attribute use from the
            // derived type's attribute uses (so the derived type has no
            // declaration for it).  An instance attribute that lands on a
            // prohibited entry has no explicit declaration in the
            // *effective* attribute uses set, so it falls through to the
            // attribute wildcard the same way any unknown attribute
            // would.  Treating prohibited as "instance must not present
            // this attribute" misreads the spec and rejects valid
            // instances that the wildcard would otherwise admit.
            let matched = ct.attributes.iter().find(|au| {
                au.use_kind != AttributeUseKind::Prohibited
                    && au.decl.name.local.as_ref() == local
                    && au.decl.name.namespace.as_deref() == ns_uri
            });
            match matched {
                Some(au) => {
                    // For ref-form attribute uses the parser stores a
                    // placeholder `type_def` of `xs:string` — the real
                    // type and any `fixed=` value live on the top-level
                    // attribute decl. Look up the ref'd attribute by
                    // name; if found, use its type and fixed value,
                    // otherwise fall back to the use's own values.
                    let referenced = self.schema.attribute(&au.decl.name);
                    let resolved_type = referenced
                        .map(|d| d.type_def.clone())
                        .unwrap_or_else(|| au.decl.type_def.clone());
                    let st = self.resolve_simple_type(&resolved_type);
                    let val = a.value.as_ref();
                    if let Err(e) = st.validate_only(val) {
                        self.report_at(e.kind,
                            format!("attribute {}: {}", au.decl.name, e.message),
                            event_offset);
                    }
                    let fixed = au.fixed.as_deref()
                        .or(au.decl.fixed.as_deref())
                        .or_else(|| referenced.and_then(|d| d.fixed.as_deref()));
                    if let Some(f) = fixed {
                        if val.trim() != f.trim() {
                            self.report_at(ValidationKind::TypeMismatch,
                                format!("attribute {} value {val:?} \
                                         doesn't match fixed value {f:?}",
                                        au.decl.name),
                                event_offset);
                        }
                    }
                    // Share the decl's already-interned QName: an
                    // Arc::clone is just an atomic refcount bump
                    // (no heap allocation), while a fresh
                    // `parse_attribute_qname` would alloc two
                    // Arc<str>s per element.
                    seen_attrs.push(au.decl.name.clone());
                }
                None => {
                    // Now we need a heap-allocated QName — either
                    // for the wildcard accept check or for an
                    // unexpected-attribute diagnostic.
                    let attr_qn = QName {
                        namespace: ns_uri.map(Arc::from),
                        local:     Arc::from(local),
                    };
                    if let Some(wc) = &ct.any_attribute {
                        if attr_wildcard_accepts_qname(wc, &attr_qn, self.schema, ct) {
                            // XSD §3.10.4 — `processContents="strict"`
                            // is supposed to require the wildcard
                            // attribute to be a declared top-level
                            // attribute somewhere in the schema, but
                            // real-world schemas frequently rely on
                            // xsi:schemaLocation hints in the instance
                            // to bring those declarations in. Our
                            // validator doesn't follow instance hints,
                            // so flagging unknown attributes here would
                            // produce many false positives. Treat
                            // strict like lax for attributes — the
                            // typical xs:anyAttribute use case.
                            seen_attrs.push(attr_qn);
                            continue;
                        }
                    }
                    self.report_at(ValidationKind::UnexpectedAttribute,
                        format!("unexpected attribute {attr_qn}"),
                        event_offset);
                }
            }
        }
        for au in &ct.attributes {
            // A value constraint to materialise (only when asked to fill
            // defaults, and only for no-namespace attributes).  Computed
            // first so the common path — not filling — does no extra work
            // for plain optional attributes: it skips straight past them,
            // exactly as before, only inspecting `seen_attrs` for the
            // required-attribute check.
            let fill_value: Option<&str> =
                if self.opts.apply_attribute_defaults && au.decl.name.namespace.is_none() {
                    au.default.as_deref()
                        .or(au.decl.default.as_deref())
                        .or(au.fixed.as_deref())
                        .or(au.decl.fixed.as_deref())
                } else {
                    None
                };
            if au.use_kind != AttributeUseKind::Required && fill_value.is_none() {
                continue;
            }
            let present = seen_attrs.iter().any(|n|
                n == &au.decl.name
                || (n.local == au.decl.name.local
                    && n.namespace.is_none()
                    && au.decl.name.namespace.is_none()));
            if present { continue; }
            // Absent.  Materialise a `default=` / `fixed=` value constraint
            // onto the live instance (the XSD post-schema-validation
            // infoset adds it; libxml2's XML_SCHEMA_VAL_VC_I_CREATE).  A
            // satisfied default isn't then reported as a missing required.
            if let Some(value) = fill_value {
                self.reader.fill_default_attr(au.decl.name.local.as_ref(), value);
                continue;
            }
            if au.use_kind == AttributeUseKind::Required {
                self.report_at(ValidationKind::MissingRequiredAttribute,
                    format!("missing required attribute {}", au.decl.name),
                    event_offset);
            }
        }
        seen_attrs
    }

    fn push_decl_ctx(&mut self, qn: &QName, decl: Arc<ElementDecl>, attrs: &[Attr<'x>],
        pushed_ns: bool, event_offset: usize,
    ) {
        let type_def = self.resolve_type(&decl.type_def);
        // Run attribute validation against the resolved type — lax/strict
        // wildcards land here when the matched element decl pins down a
        // concrete type, and the spec requires the per-element attribute
        // checks (required, fixed, prohibited, simple-type validation,
        // anyAttribute wildcard) to run regardless of how we got here.
        let _ = self.validate_attrs_against_type(&type_def, &attrs, event_offset);
        let cursor = build_cursor(&type_def);
        let declared_scope = if !decl.identity.is_empty() {
            let collected = vec![Vec::new(); decl.identity.len()];
            self.key_scopes.push(KeyScope {
                declaring_depth: self.stack.len(),
                declaring_offset: event_offset as u32,
                decl: decl.clone(),
                collected,
            });
            self.key_scopes.len() - 1
        } else { usize::MAX };
        let parent_collect_depth = self.stack.last()
            .map(|p| p.collect_depth).unwrap_or(0);
        let parent_collects = parent_collect_depth > 0;
        let (matched, mut cached_attrs) = self.check_active_scopes(qn, &attrs);
        if cached_attrs.is_empty() && parent_collects {
            cached_attrs = snapshot_attrs(&attrs, |s| self.parse_attribute_qname(s));
        }
        let own_collect_depth = matched.iter().map(|(si, ci)| {
            self.key_scopes[*si].decl.identity[*ci].fields.iter()
                .map(max_field_child_descent)
                .max().unwrap_or(0)
        }).max().unwrap_or(0);
        let collect_depth = own_collect_depth
            .max(parent_collect_depth.saturating_sub(1));
        let sibling_index = match self.stack.last_mut() {
            Some(parent) => bump_child_counter(&mut parent.child_counters, &qn.local),
            None => 1,
        };
        self.record_governing_type(&type_def);
        self.stack.push(ElementCtx {
            decl,
            type_def,
            cursor,
            text_buf: String::new(),
            is_nil: false,
            name: qn.clone(),
            pushed_ns,
            seen_attrs: Vec::new(),
            cached_attrs,
            declared_scope,
            matched_constraints: matched,
            collect_depth,
            child_snapshots: Vec::new(),
            start_offset: event_offset as u32,
            _phantom: std::marker::PhantomData,
            sibling_index,
            child_counters: Vec::new(),
        });
        // Note: attrs not validated for wildcard-skipped/lax-with-no-decl
        // children — matches typical XSD wildcard semantics.
        let _ = attrs;
    }

    /// Skip the body of an element whose start was just consumed (and
    /// rejected — caller already reported the issue).  `pushed_ns` is
    /// the value returned by [`push_ns_scope`] for the containing
    /// element; we pop the scope here when applicable so the caller
    /// doesn't have to.
    fn skip_body_into_issues(&mut self, pushed_ns: bool) -> bool {
        let mut depth = 1usize;
        while depth > 0 {
            match self.reader.next_into(&mut self.attr_buf) {
                Ok(EventInto::StartElement { .. }) => depth += 1,
                Ok(EventInto::EndElement { .. })   => depth -= 1,
                Ok(EventInto::Eof) => {
                    self.report(ValidationKind::Other, "unexpected EOF in skipped subtree");
                    return true;
                }
                Err(e) => {
                    self.issues.push(ValidationIssue {
                        message: format!("XML parse error: {e}"),
                        line: e.line, column: e.column, path: self.current_path(),
                        kind: ValidationKind::Other,
                        expected: Vec::new(), value: None, type_name: None,
                    });
                    return true;
                }
                _ => {}
            }
        }
        self.pop_ns_scope_if(pushed_ns);
        false
    }

    fn handle_min_occurs_violation(&mut self, p: &Particle, at_offset: usize) -> bool {
        let what = match &p.term {
            Term::Element(e) => format!("element <{}>", e.name),
            Term::Group { .. } => "group".to_string(),
            Term::Wildcard(_)  => "wildcard".to_string(),
            Term::GroupRef(name) => format!("group {name}"),
        };
        self.report_at(ValidationKind::MissingRequiredElement,
            format!("missing required {what} (minOccurs={})", p.min_occurs),
            at_offset)
    }

    // ── type resolution ─────────────────────────────────────────────────

    /// Resolve an `UNRESOLVED:` placeholder type to the real one if the
    /// schema (or built-in catalogue) declares it.  For non-placeholder
    /// types, return as-is.
    fn resolve_type(&self, t: &TypeRef) -> TypeRef {
        if let TypeRef::Simple(st) = t {
            if let Some(name) = &st.name {
                if let Some(rest) = name.strip_prefix("UNRESOLVED:") {
                    let qn = parse_unresolved_marker(rest);
                    if let Some(real) = self.lookup_type_by_qname(&qn) {
                        return real;
                    }
                }
            }
        }
        t.clone()
    }

    /// True when every constraint cross-referenced with the
    /// constraint at `(scope_idx, constraint_idx)` also uses
    /// all-`.` field paths.  This is the gate that lets a key /
    /// keyref pair share the same canonical key encoding — see
    /// the call site in `handle_end` for rationale.  For a Key /
    /// Unique target, the cross-references are every `KeyRef`
    /// pointing at it.  For a KeyRef, the cross-reference is the
    /// `refer=` target.  No referring constraints → vacuously
    /// `true` (canonicalisation is safe).
    fn constraint_peers_all_dot(&self, scope_idx: usize, constraint_idx: usize) -> bool {
        use super::identity::ConstraintKind;
        let me = &self.key_scopes[scope_idx].decl.identity[constraint_idx];
        match me.kind {
            ConstraintKind::Key | ConstraintKind::Unique => {
                for scope in &self.key_scopes {
                    for c in scope.decl.identity.iter() {
                        if c.kind != ConstraintKind::KeyRef { continue; }
                        if c.refer.as_ref() != Some(&me.name) { continue; }
                        if !c.fields.iter().all(field_path_is_dot) {
                            return false;
                        }
                    }
                }
                true
            }
            ConstraintKind::KeyRef => {
                let Some(target_name) = me.refer.as_ref() else { return true; };
                for scope in &self.key_scopes {
                    for c in scope.decl.identity.iter() {
                        if !matches!(c.kind,
                            ConstraintKind::Key | ConstraintKind::Unique) { continue; }
                        if &c.name != target_name { continue; }
                        if !c.fields.iter().all(field_path_is_dot) {
                            return false;
                        }
                    }
                }
                true
            }
        }
    }

    /// For an identity-constraint `<xs:field xpath="."/>` evaluated
    /// on an element with effective type `type_def`, the simple type
    /// that should be used to canonicalise the field value.  Returns
    /// `None` for complex element-only types (no text value to
    /// canonicalise) so the caller falls back to the raw lexical.
    fn field_dot_simple_type(
        &self, type_def: &TypeRef,
    ) -> Option<Arc<SimpleType>> {
        match type_def {
            TypeRef::Simple(st) => Some(self.resolve_simple_type(st)),
            TypeRef::Complex(ct) => match &ct.content {
                ContentModel::Simple(st) => {
                    let effective = self.simple_content_effective_type(ct, st);
                    Some(self.resolve_simple_type(&effective))
                }
                _ => None,
            },
        }
    }

    /// XSD 1.0 §3.4.2 — for a complex type with `<xs:simpleContent>
    /// <xs:extension base="T"/>`, the effective simple content type
    /// is `T`.  `merge_inline_extension_in_elements` patches this in
    /// for top-level element decls, but inline anonymous complex
    /// types on LOCAL element decls (nested inside another complex
    /// type's particles) aren't reached by that walk and still carry
    /// the `ContentModel::Simple(xs:string)` placeholder
    /// `parse_derivation_body` seeded.  Detect that shape here and
    /// substitute the extension's declared base, so element-text
    /// facet validation sees the right type.  Returns `current`
    /// unchanged when the type is already correctly resolved or
    /// when the derivation base isn't a simple type.
    fn simple_content_effective_type(
        &self, ct: &Arc<ComplexType>, current: &Arc<SimpleType>,
    ) -> Arc<SimpleType> {
        // Only patch the seeded placeholder — `SimpleType::of_builtin(
        // String)` leaves `name = None` with no facets, which is what
        // [`parse_derivation_body`] writes before the merge pass
        // resolves the extension base.  A deliberately-declared
        // `xs:string` content has `name = Some("string")` and is left
        // untouched.
        let is_placeholder = current.name.is_none()
            && matches!(current.builtin, super::types::BuiltinType::String);
        if !is_placeholder { return current.clone(); }
        let Some(d) = &ct.derivation else { return current.clone(); };
        if d.method != super::types::DerivationMethod::Extension {
            return current.clone();
        }
        let base = self.resolve_type(&d.base);
        match base {
            TypeRef::Simple(s) => s,
            TypeRef::Complex(_) => current.clone(),
        }
    }

    fn resolve_simple_type(&self, st: &Arc<SimpleType>) -> Arc<SimpleType> {
        // First, resolve THIS type if it's an UNRESOLVED placeholder.
        let top = if let Some(name) = &st.name {
            if let Some(rest) = name.strip_prefix("UNRESOLVED:") {
                let qn = parse_unresolved_marker(rest);
                if let Some(TypeRef::Simple(real)) = self.lookup_type_by_qname(&qn) {
                    real
                } else {
                    st.clone()
                }
            } else { st.clone() }
        } else { st.clone() };

        // Then recursively resolve any placeholders inside variety:
        // List.item_type and Union.members can themselves be
        // UNRESOLVED, and the existing parser doesn't run a topo-sort
        // post-pass.  This recursive resolve makes validation work
        // regardless of declaration order.
        match &top.variety {
            super::types::Variety::Atomic => top,
            super::types::Variety::List { item_type } => {
                let new_item = self.resolve_simple_type(item_type);
                if Arc::ptr_eq(&new_item, item_type) {
                    top
                } else {
                    let mut t: SimpleType = (*top).clone();
                    t.variety = super::types::Variety::List { item_type: new_item };
                    Arc::new(t)
                }
            }
            super::types::Variety::Union { members } => {
                let new_members: Vec<Arc<SimpleType>> = members.iter()
                    .map(|m| self.resolve_simple_type(m))
                    .collect();
                let changed = new_members.iter().zip(members.iter())
                    .any(|(a, b)| !Arc::ptr_eq(a, b));
                if !changed {
                    top
                } else {
                    let mut t: SimpleType = (*top).clone();
                    t.variety = super::types::Variety::Union { members: new_members };
                    Arc::new(t)
                }
            }
        }
    }

    fn lookup_type_by_qname(&self, qn: &QName) -> Option<TypeRef> {
        // Built-ins live in the XSD spec namespace.
        if qn.namespace.as_deref() == Some(QName::XSD_NS) {
            if let Some(b) = BuiltinType::from_name(&qn.local) {
                return Some(TypeRef::Simple(Arc::new(SimpleType {
                    name: Some(qn.local.clone()),
                    builtin: b,
                    facets: super::facets::FacetSet::default(),
                    whitespace: b.default_whitespace(),
                    variety: super::types::Variety::Atomic,
                    final_: super::schema::BlockSet::default(),
                    assertions: Vec::new(),
                })));
            }
        }
        // Schema-declared types.
        self.schema.type_def(qn).cloned()
    }
}

fn build_cursor(type_def: &TypeRef) -> ContentCursor {
    match type_def {
        TypeRef::Simple(_) => ContentCursor::None,
        TypeRef::Complex(ct) => match ct.matcher.get() {
            Some(super::dfa::ContentMatcher::Dfa(dfa)) => ContentCursor::Dfa {
                dfa: dfa.clone(),
                state: dfa.initial,
            },
            Some(super::dfa::ContentMatcher::All) => match &ct.content {
                ContentModel::Complex { root: Particle { term: Term::Group { kind, particles }, min_occurs, .. }, .. } => {
                    ContentCursor::Group {
                        kind: *kind,
                        particles: Arc::clone(particles),
                        idx: 0,
                        cur_count: 0,
                        all_seen: HashMap::default(),
                        outer_min: *min_occurs,
                    }
                }
                _ => ContentCursor::None,
            },
            Some(super::dfa::ContentMatcher::None) | None => ContentCursor::None,
        },
    }
}

/// Look up an attribute named `xsi:<local>` (where `xsi` is bound to the
/// XSD-instance namespace).  We match only on the literal `xsi:` prefix
/// since that's the universal convention; instances using a different
/// prefix bound to the same URI fall through to the regular attribute
/// validation path (and would currently miss the special handling — a
/// known v1 limitation, documented).
///
/// Two specialisations to keep the hot path allocation-free.
fn find_xsi_type<'a>(attrs: &'a [Attr]) -> Option<&'a str> {
    attrs.iter().find(|a| a.name() == "xsi:type").map(|a| a.value.as_ref())
}

/// XSD 1.0 §3.3.4 cvc-elt-5.1.2 / cvc-elt-5.2.2.2: an empty element
/// with a `default=` or `fixed=` value behaves as if its content
/// were the supplied value when validating against the simple
/// type.  `fixed` takes precedence over `default` (an element can
/// only declare one of the two per §3.3.3, but we honour the
/// precedence order anyway).
fn effective_text<'a>(text: &'a str, default: Option<&'a str>, fixed: Option<&'a str>) -> &'a str {
    if !text.is_empty() { return text; }
    fixed.or(default).unwrap_or(text)
}

fn find_xsi_nil<'a>(attrs: &'a [Attr]) -> Option<&'a str> {
    attrs.iter().find(|a| a.name() == "xsi:nil").map(|a| a.value.as_ref())
}

/// Result of parsing a raw `xsi:nil` value as xs:boolean (XSD 1.0 §3.2.2).
///
/// The lexical space is exactly `{"true", "false", "1", "0"}` —
/// case-sensitive.  Anything else is a lexical error; we surface that
/// distinctly so the validator can emit a clean type-mismatch
/// diagnostic instead of silently picking a default.
enum XsiNilParse<'a> {
    True,
    False,
    Invalid(&'a str),
}

fn parse_xsi_nil(raw: &str) -> XsiNilParse<'_> {
    match raw {
        "true"  | "1" => XsiNilParse::True,
        "false" | "0" => XsiNilParse::False,
        other         => XsiNilParse::Invalid(other),
    }
}

fn parse_unresolved_marker(s: &str) -> QName {
    // Format: `{ns}local`   or just `local`
    if let Some(rest) = s.strip_prefix('{') {
        if let Some(end) = rest.find('}') {
            let ns = &rest[..end];
            let local = &rest[end + 1..];
            return QName::new(if ns.is_empty() { None } else { Some(ns) }, local);
        }
    }
    QName::new(None, s)
}

// ── content matching ─────────────────────────────────────────────────────────

enum MatchOutcome {
    Element(Arc<ElementDecl>),
    Wildcard(Wildcard),
    None,
}

/// The element names a content-model cursor would accept next — what
/// libxml2 renders as "Expected is ( … )" on an unexpected-element
/// error.  Best-effort: returns the DFA state's outgoing element
/// transitions, or the current all-group particle's element names.
fn expected_element_names(cursor: &ContentCursor) -> Vec<String> {
    match cursor {
        ContentCursor::Dfa { dfa, state } => dfa.states[*state as usize]
            .on_element
            .iter()
            .map(|t| t.name.local.to_string())
            .collect(),
        ContentCursor::Group { particles, idx, .. } => particles
            .get(*idx)
            .map(particle_element_names)
            .unwrap_or_default(),
        ContentCursor::None => Vec::new(),
    }
}

fn particle_element_names(p: &Particle) -> Vec<String> {
    match &p.term {
        super::schema::Term::Element(decl) => vec![decl.name.local.to_string()],
        super::schema::Term::Group { particles, .. } => {
            particles.iter().flat_map(particle_element_names).collect()
        }
        _ => Vec::new(),
    }
}

fn match_in_cursor(qn: &QName, cursor: &mut ContentCursor, schema: &Schema) -> MatchOutcome {
    match cursor {
        ContentCursor::None => MatchOutcome::None,
        ContentCursor::Dfa { dfa, state } => {
            let target_ns = schema.target_namespace();
            let siblings  = dfa.defined_siblings.clone();
            let step = dfa.step(*state, qn, |wc, qn| {
                super::dfa::wildcard_admits(
                    wc, qn, target_ns,
                    |q| schema.element(q).is_some(),
                    |q| siblings.iter().any(|s| s == q),
                )
            });
            match step {
                Some(super::dfa::DfaTransition::Element { next, decl }) => {
                    *state = next;
                    MatchOutcome::Element(decl)
                }
                Some(super::dfa::DfaTransition::Wildcard { next, process_contents }) => {
                    *state = next;
                    // Downstream handling only consults `process_contents`;
                    // the synthetic wildcard skips the now-redundant
                    // namespace/notQName state.
                    MatchOutcome::Wildcard(super::schema::Wildcard {
                        namespaces: super::schema::NamespaceConstraint::Any,
                        process_contents,
                        not_qnames:                Vec::new(),
                        not_namespaces:            Vec::new(),
                        not_qname_defined:         false,
                        not_qname_defined_sibling: false,
                    })
                }
                None => MatchOutcome::None,
            }
        }
        ContentCursor::Group { kind, particles, idx, cur_count, all_seen, .. } => {
            match kind {
                GroupKind::Sequence => match_sequence(qn, particles, idx, cur_count, schema),
                GroupKind::Choice   => match_choice(qn, particles, idx, cur_count, schema),
                GroupKind::All      => match_all(qn, particles, all_seen, schema),
            }
        }
    }
}

fn match_sequence(
    qn: &QName,
    particles: &Arc<[Particle]>,
    idx: &mut usize,
    cur_count: &mut u32,
    schema: &Schema,
) -> MatchOutcome {
    while *idx < particles.len() {
        let p = &particles[*idx];
        if particle_accepts(p, qn, schema) {
            // If we'd exceed maxOccurs, advance to next particle and retry.
            if !p.max_occurs.allows(*cur_count + 1) {
                *idx += 1;
                *cur_count = 0;
                continue;
            }
            *cur_count += 1;
            return outcome_for(p, qn, schema);
        }
        // Doesn't match — current particle must have met its min already.
        if *cur_count < p.min_occurs {
            return MatchOutcome::None;
        }
        *idx += 1;
        *cur_count = 0;
    }
    MatchOutcome::None
}

fn match_choice(
    qn: &QName,
    particles: &Arc<[Particle]>,
    idx: &mut usize,
    cur_count: &mut u32,
    schema: &Schema,
) -> MatchOutcome {
    // First match in the choice: pick that particle.
    if *cur_count == 0 {
        for (i, p) in particles.iter().enumerate() {
            if particle_accepts(p, qn, schema) {
                *idx = i;
                *cur_count = 1;
                return outcome_for(p, qn, schema);
            }
        }
        return MatchOutcome::None;
    }
    // Subsequent matches stay on the chosen particle.
    let p = &particles[*idx];
    if particle_accepts(p, qn, schema) && p.max_occurs.allows(*cur_count + 1) {
        *cur_count += 1;
        return outcome_for(p, qn, schema);
    }
    MatchOutcome::None
}

fn match_all(
    qn: &QName,
    particles: &Arc<[Particle]>,
    all_seen: &mut HashMap<usize, u32>,
    schema: &Schema,
) -> MatchOutcome {
    let siblings = collect_particle_siblings(particles);
    for (i, p) in particles.iter().enumerate() {
        if particle_accepts_with_siblings(p, qn, schema, &siblings) {
            let seen = all_seen.entry(i).or_insert(0);
            if !p.max_occurs.allows(*seen + 1) {
                continue;
            }
            *seen += 1;
            return outcome_for(p, qn, schema);
        }
    }
    MatchOutcome::None
}

/// Flatten the element names statically declared in `particles` for
/// the `##definedSibling` exclusion in any wildcards they contain.
/// Walks nested groups; substitution-group members aren't included
/// (they're independent top-level declarations, not siblings of this
/// type's content).
fn collect_particle_siblings(particles: &[Particle]) -> Vec<QName> {
    fn walk(p: &Particle, out: &mut Vec<QName>) {
        match &p.term {
            Term::Element(decl) => {
                if !out.iter().any(|n| n == &decl.name) {
                    out.push(decl.name.clone());
                }
            }
            Term::Group { particles, .. } => {
                for c in particles.iter() { walk(c, out); }
            }
            Term::Wildcard(_) | Term::GroupRef(_) => {}
        }
    }
    let mut out = Vec::new();
    for p in particles { walk(p, &mut out); }
    out
}

fn particle_accepts(p: &Particle, qn: &QName, schema: &Schema) -> bool {
    match &p.term {
        Term::Element(decl) => {
            if &decl.name == qn { return true; }
            // Substitution-group dispatch — disabled when the anchor
            // has `block="substitution"` (XSD 1.0 §3.3.4 cvc-elt-2.2).
            if decl.block.contains(super::schema::BlockSet::SUBSTITUTION) {
                return false;
            }
            for sub in schema.substitutes_for(&decl.name) {
                if &sub.name == qn { return true; }
            }
            false
        }
        Term::Wildcard(wc) => wildcard_accepts_qname(wc, qn, schema, &[]),
        Term::Group { particles, .. } => particles.iter().any(|p2| particle_accepts(p2, qn, schema)),
        // GroupRef should have been expanded at schema compile time.
        Term::GroupRef(_) => false,
    }
}

/// Variant of [`particle_accepts`] that knows the surrounding
/// sibling-element set — wildcards using `notQName="##definedSibling"`
/// (XSD 1.1 §3.10.4) need this to know which names to exclude.  The
/// `xs:all` path threads its top-level particles in as `siblings`;
/// `xs:sequence` / `xs:choice` flow through the DFA path instead and
/// don't reach this helper.
fn particle_accepts_with_siblings(
    p:        &Particle,
    qn:       &QName,
    schema:   &Schema,
    siblings: &[QName],
) -> bool {
    match &p.term {
        Term::Wildcard(wc) => wildcard_accepts_qname(wc, qn, schema, siblings),
        Term::Group { particles, .. } => particles
            .iter()
            .any(|p2| particle_accepts_with_siblings(p2, qn, schema, siblings)),
        // Element / GroupRef share the simpler path.
        _ => particle_accepts(p, qn, schema),
    }
}

/// Standalone version of [`Validator::derivation_methods_from`] —
/// returns the set of XSD derivation methods used to derive `child`
/// from `ancestor` (via the schema's type map), or `None` when
/// `child` doesn't derive from `ancestor`. Used by the
/// substitution-group dispatch which doesn't have `&Validator`.
fn derivation_methods_between(
    child:    &TypeRef,
    ancestor: &TypeRef,
    schema:   &Schema,
) -> Option<BlockSet> {
    let child = resolve_typeref_via_schema(child, schema);
    let ancestor = resolve_typeref_via_schema(ancestor, schema);
    if type_refs_equal(&child, &ancestor) {
        return Some(BlockSet::empty());
    }
    if is_any_type(&ancestor) {
        return Some(BlockSet::RESTRICTION);
    }
    if is_any_simple_type(&ancestor) {
        return match &child {
            TypeRef::Simple(_)  => Some(BlockSet::RESTRICTION),
            TypeRef::Complex(_) => None,
        };
    }
    match &child {
        TypeRef::Complex(ct) => {
            let mut methods = BlockSet::empty();
            let mut cur: Arc<ComplexType> = ct.clone();
            for _ in 0..64 {
                let d = cur.derivation.as_ref()?;
                methods |= match d.method {
                    DerivationMethod::Restriction => BlockSet::RESTRICTION,
                    DerivationMethod::Extension   => BlockSet::EXTENSION,
                };
                let resolved_base = resolve_typeref_via_schema(&d.base, schema);
                if type_refs_equal(&resolved_base, &ancestor) {
                    return Some(methods);
                }
                match resolved_base {
                    TypeRef::Complex(next) => { cur = next; }
                    TypeRef::Simple(_)     => return None,
                }
            }
            None
        }
        TypeRef::Simple(s_child) => {
            if let TypeRef::Simple(s_anc) = &ancestor {
                if s_child.builtin == s_anc.builtin {
                    Some(BlockSet::RESTRICTION)
                } else { None }
            } else { None }
        }
    }
}

fn resolve_typeref_via_schema(tr: &TypeRef, schema: &Schema) -> TypeRef {
    if let TypeRef::Simple(st) = tr {
        if let Some(name) = &st.name {
            if let Some(rest) = name.strip_prefix("UNRESOLVED:") {
                let qn = parse_unresolved_marker(rest);
                if let Some(real) = schema.type_def(&qn) {
                    return real.clone();
                }
            }
        }
    }
    tr.clone()
}

/// Element-wildcard match against a candidate qname.  Delegates to
/// the shared XSD 1.1 helper in [`super::dfa`].  `siblings` is the
/// enclosing complex type's element-declaration name set, consulted
/// only when the wildcard carries `notQName="##definedSibling"`.
fn wildcard_accepts_qname(
    wc:       &Wildcard,
    qn:       &QName,
    schema:   &Schema,
    siblings: &[QName],
) -> bool {
    super::dfa::wildcard_admits(
        wc, qn, schema.target_namespace(),
        |q| schema.element(q).is_some(),
        |q| siblings.iter().any(|s| s == q),
    )
}

/// Attribute-wildcard match against a candidate attribute qname.
/// `##defined` consults the schema's *attribute* table (not elements);
/// `##definedSibling` consults the enclosing complex type's declared
/// attribute uses.
fn attr_wildcard_accepts_qname(
    wc:           &Wildcard,
    qn:           &QName,
    schema:       &Schema,
    ct:           &ComplexType,
) -> bool {
    super::dfa::wildcard_admits(
        wc, qn, schema.target_namespace(),
        |q| schema.attribute(q).is_some(),
        |q| ct.attributes.iter().any(|au| &au.decl.name == q),
    )
}

fn outcome_for(p: &Particle, qn: &QName, schema: &Schema) -> MatchOutcome {
    match &p.term {
        Term::Element(decl) => {
            // Substitution: if qn is a substitute, use that decl instead.
            if &decl.name == qn {
                return MatchOutcome::Element(decl.clone());
            }
            // Honor `block="substitution"` (cvc-elt-2.2) — fall back
            // to the anchor decl so dispatch reports the qname mismatch
            // against it rather than promoting a forbidden substitute.
            if !decl.block.contains(super::schema::BlockSet::SUBSTITUTION) {
                for sub in schema.substitutes_for(&decl.name) {
                    if &sub.name == qn {
                        // XSD §3.3.6 (cvc-elt-substitution) — if the
                        // head's `block` covers the derivation method
                        // used to derive the substitute's type from
                        // the head's type, the substitution is
                        // forbidden. Fall back to the anchor so the
                        // ensuing dispatch reports a qname mismatch.
                        let methods = derivation_methods_between(
                            &sub.type_def, &decl.type_def, schema,
                        );
                        if let Some(m) = methods {
                            if !(decl.block & m).is_empty() {
                                return MatchOutcome::Element(decl.clone());
                            }
                        }
                        return MatchOutcome::Element(sub.clone());
                    }
                }
            }
            MatchOutcome::Element(decl.clone())
        }
        Term::Wildcard(wc) => MatchOutcome::Wildcard(wc.clone()),
        Term::Group { particles, .. } => {
            // Nested group matched — find the actual particle that took it.
            for p in particles.iter() {
                if particle_accepts(p, qn, schema) {
                    return outcome_for(p, qn, schema);
                }
            }
            MatchOutcome::None
        }
        Term::GroupRef(_) => MatchOutcome::None,
    }
}

// ── identity-constraint helpers ──────────────────────────────────────────────

/// Match a [`SelectorPath`] against the *current* element, given the
/// element name `qn`, the current stack frames, and `rel_depth` (the
/// number of element levels between the constraint-declaring element
/// and the current element).
///
/// v1 supports the two most common shapes: `name` (direct children) and
/// `.//name` (any descendant), plus `*` and `name1/name2`.  Step-by-step
/// matching against the parents at the corresponding stack offsets.
fn selector_matches<'s>(
    sel: &SelectorPath,
    stack: &[ElementCtx<'s>],
    qn: &QName,
    rel_depth: usize,
) -> bool {
    sel.paths.iter().any(|p| path_matches(p, stack, qn, rel_depth))
}

fn path_matches<'s>(
    p: &PathExpr,
    stack: &[ElementCtx<'s>],
    qn: &QName,
    rel_depth: usize,
) -> bool {
    let n = p.steps.len();
    if n == 0 {
        // `.` selector — matches the constraint-declaring element itself.
        return rel_depth == 0;
    }
    if p.descendant {
        // `.//step1/.../stepN` — matches at any depth ≥ N.
        if rel_depth < n { return false; }
    } else {
        // Anchored — must match at exact depth N.
        if rel_depth != n { return false; }
    }
    // Compare each step against the corresponding ancestor.
    // `qn` is the element at depth `rel_depth`; ancestors live in the
    // stack frames.  Steps are matched in reverse: steps[n-1] vs qn,
    // steps[n-2] vs parent, etc.
    let stack_top = stack.len();
    for (k, step) in p.steps.iter().rev().enumerate() {
        let nm = match step {
            PathStep::Child(nm) | PathStep::Attribute(nm) => nm,
        };
        let target_qn: &QName = if k == 0 {
            qn
        } else {
            &stack[stack_top - k].name
        };
        if !name_test_matches_qname(nm, target_qn) {
            return false;
        }
    }
    true
}

fn name_test_matches_qname(nt: &NameTest, qn: &QName) -> bool {
    match nt {
        NameTest::Any        => true,
        // Forgiving namespace match: an unprefixed name in an
        // identity-constraint XPath matches elements in *any*
        // namespace (matches libxml2/Xerces behaviour for the typical
        // case where the selector author meant "the schema's
        // namespace" but didn't bind a prefix).
        NameTest::Name(want) => {
            want.local == qn.local
                && (want.namespace.is_none() || want.namespace == qn.namespace)
        }
        NameTest::AnyInNs(ns) => qn.namespace.as_deref() == Some(ns.as_ref()),
    }
}

/// Evaluate one [`FieldPath`] against the just-finished element.
///
/// Walks each alternative path (separated by `|` in XSD source) and
/// returns the first non-missing value.  Supports arbitrary-depth
/// `child/.../@attr` paths via recursive descent through the
/// pre-collected snapshot tree — see [`ChildSnapshot`].
///
/// Carve-out: the `.//` descendant-axis prefix on field paths is
/// parsed but not yet honored.  XSD §3.11.6 allows it on fields; in
/// practice it's rare — real-world fields are anchored.  Adding
/// support would mean searching the snapshot tree rather than
/// indexing by name; flagged in `thoughts/unfinished.txt`.
/// Outcome of evaluating an `<xs:field>` xpath at a matched
/// selector node.
enum FieldEval {
    /// xpath selected no node — the tuple slot is `None`
    /// (legal for `xs:unique`, fatal for `xs:key`).
    Missing,
    /// xpath selected exactly one node — the value of that node.
    Single(String),
    /// xpath selected more than one node — XSD §3.11.6 forbids this.
    Ambiguous,
}

fn eval_field(
    fp: &FieldPath,
    cached_attrs: &[(QName, String)],
    text_buf: &str,
    children: &[ChildSnapshot],
) -> FieldEval {
    // The schema-compile XPath subset stores alternatives in `paths`
    // (one per `|`-separated branch).  We keep the union semantics:
    // the first branch that yields a value wins.  Ambiguity reported
    // by any branch propagates.
    for path in &fp.paths {
        match eval_path_steps(&path.steps, cached_attrs, text_buf, children) {
            FieldEval::Single(v)  => return FieldEval::Single(v),
            FieldEval::Ambiguous  => return FieldEval::Ambiguous,
            FieldEval::Missing    => continue,
        }
    }
    FieldEval::Missing
}

/// Recursive evaluator for a single path's [`PathStep`] sequence.
/// Walks child steps through `children`, descending into each
/// snapshot's own sub-children for nested steps.  Terminates on:
///   * empty steps (`.`)           → current element's text
///   * `[Attribute]` (last step)   → attribute on current element
///   * `[Child, rest...]`          → descend into matching child
fn eval_path_steps(
    steps: &[PathStep],
    attrs: &[(QName, String)],
    text:  &str,
    children: &[ChildSnapshot],
) -> FieldEval {
    match steps {
        [] => FieldEval::Single(text.trim().to_string()),
        [PathStep::Attribute(nm)] => match lookup_attr(attrs, nm) {
            Some(v) => FieldEval::Single(v),
            None    => FieldEval::Missing,
        },
        [PathStep::Child(nm), rest @ ..] => {
            let mut matched: Option<&ChildSnapshot> = None;
            for c in children {
                if name_test_matches_qname(nm, &c.name) {
                    if matched.is_some() {
                        return FieldEval::Ambiguous;
                    }
                    matched = Some(c);
                }
            }
            match matched {
                Some(c) => eval_path_steps(rest, &c.attrs, &c.text, &c.children),
                None    => FieldEval::Missing,
            }
        }
        // Attribute in non-final position is rejected by the
        // identity-constraint XPath micro-parser, so we shouldn't
        // see it here.  If we do (corrupt schema), miss safely.
        _ => FieldEval::Missing,
    }
}

fn lookup_attr(attrs: &[(QName, String)], nt: &NameTest) -> Option<String> {
    match nt {
        NameTest::Any => attrs.first().map(|(_, v)| v.clone()),
        NameTest::Name(want) => attrs.iter()
            .find(|(qn, _)| {
                // Match on local name; tolerate the "no namespace" form
                // (XSD identity-constraint XPath unprefixed names).
                qn.local == want.local
                    && (want.namespace.is_none() || want.namespace == qn.namespace)
            })
            .map(|(_, v)| v.clone()),
        NameTest::AnyInNs(ns) => attrs.iter()
            .find(|(qn, _)| qn.namespace.as_deref() == Some(ns.as_ref()))
            .map(|(_, v)| v.clone()),
    }
}

impl<'s, 'x, E: XsdEventSource<'x>> Validator<'s, 'x, E> {
    /// Validate a closing key-scope: uniqueness for key/unique,
    /// referential integrity for keyref.
    fn finalize_key_scope(&mut self, scope: &KeyScope) {
        // All identity-constraint issues are attributed to the
        // declaring element — by the time finalize fires, that element
        // has already been popped from the stack and the reader has
        // advanced past it, so the default offset is meaningless.
        let off = scope.declaring_offset as usize;
        for (ci, c) in scope.decl.identity.iter().enumerate() {
            let tuples = &scope.collected[ci];
            match c.kind {
                ConstraintKind::Key | ConstraintKind::Unique => {
                    // Uniqueness: every non-null tuple must appear once.
                    // For xs:unique, tuples containing any absent field
                    // are skipped (no key value → nothing to compare).
                    // For xs:key, every field must be present — a None
                    // there is a validity error in its own right.
                    let mut seen: HashMap<&KeyTuple, ()> = HashMap::default();
                    for t in tuples {
                        let has_null = t.iter().any(|f| f.is_none());
                        if has_null {
                            if c.kind == ConstraintKind::Key {
                                self.report_at(
                                    ValidationKind::Other,
                                    format!(
                                        "<xs:key {:?}>: a selected element is missing one of the field values",
                                        c.name.local
                                    ),
                                    off,
                                );
                            }
                            // xs:unique skips null tuples entirely.
                            continue;
                        }
                        if seen.insert(t, ()).is_some() {
                            let pretty = format_tuple(t);
                            self.report_at(
                                ValidationKind::KeyNotUnique,
                                format!(
                                    "<xs:{} {:?}>: duplicate key value {pretty}",
                                    if c.kind == ConstraintKind::Key { "key" } else { "unique" },
                                    c.name.local
                                ),
                                off,
                            );
                        }
                    }
                }
                ConstraintKind::KeyRef => {
                    let refer = match &c.refer {
                        Some(r) => r,
                        None => continue, // schema-compile error, but be tolerant
                    };
                    // Find the referenced key/unique in this scope or
                    // any enclosing scope.
                    // Resolve the referenced key: same-scope first, then
                    // any enclosing scope.  Collect dangling tuples and
                    // report after the borrow ends — `report` mutably
                    // borrows `self`, but the lookup borrows scopes
                    // immutably.
                    let dangling: Vec<KeyTuple> = {
                        let referenced: Option<&Vec<KeyTuple>> = scope.decl.identity.iter()
                            .position(|other| &other.name == refer)
                            .map(|i| &scope.collected[i])
                            .or_else(|| {
                                for outer in self.key_scopes.iter().rev() {
                                    if let Some(i) = outer.decl.identity.iter()
                                        .position(|other| &other.name == refer)
                                    {
                                        return Some(&outer.collected[i]);
                                    }
                                }
                                None
                            });
                        match referenced {
                            None => {
                                self.report_at(
                                    ValidationKind::Other,
                                    format!(
                                        "<xs:keyref {:?}>: refer={refer} not found in scope",
                                        c.name.local
                                    ),
                                    off,
                                );
                                continue;
                            }
                            // XSD §3.11.4 / cvc-identity-constraint.4.3:
                            // a keyref tuple with any missing field
                            // produces no obligation — the constraint
                            // only applies when every field selects a
                            // value.  Skip incomplete tuples.
                            Some(target) => tuples.iter()
                                .filter(|t| t.iter().all(|f| f.is_some()))
                                .filter(|t| !target.contains(t))
                                .cloned()
                                .collect(),
                        }
                    };
                    for t in dangling {
                        let pretty = format_tuple(&t);
                        self.report_at(
                            ValidationKind::KeyRefDangling,
                            format!(
                                "<xs:keyref {:?}>: value {pretty} has no matching key",
                                c.name.local
                            ),
                            off,
                        );
                    }
                }
            }
        }
    }
}

// ── xsi:type derivation check ────────────────────────────────────────────────

impl<'s, 'x, E: XsdEventSource<'x>> Validator<'s, 'x, E> {
    /// Walk `child`'s derivation chain looking for `ancestor`.  Returns
    /// `Some(methods)` when `child` transitively derives from
    /// `ancestor`, where `methods` is the union of
    /// [`DerivationMethod`]s encountered along the chain (empty when
    /// `child == ancestor`, signalling identity).  Returns `None`
    /// when `child` does not derive from `ancestor`.
    ///
    /// Used by xsi:type processing to gate against the XSD 1.0 §3.4.6
    /// substitution rules.  Caller checks the returned method set
    /// against the element's `block=` and the declared type's
    /// `final=` to decide whether the substitution is permitted.
    ///
    /// ## UNRESOLVED placeholder bases
    ///
    /// The parser leaves `derivation.base` as an `UNRESOLVED:` Simple
    /// placeholder for user-defined base types (so it doesn't have to
    /// topo-sort the type table at compile time).  We resolve each
    /// step lazily here via [`Validator::resolve_type`] before
    /// comparing or descending.
    ///
    /// ## Simple types
    ///
    /// Simple types in v1 don't carry an explicit derivation chain —
    /// we only know the ultimate built-in and any facet layer.  Two
    /// simple types sharing the same built-in are treated as derived
    /// (by restriction) for the purposes of this check.  This is a
    /// sound over-approximation: the worst case is that we accept an
    /// xsi:type override that should have been rejected for a
    /// derivation reason we can't see.  The override's facets are
    /// still validated.
    fn derivation_methods_from(&self, child: &TypeRef, ancestor: &TypeRef) -> Option<BlockSet> {
        let child = self.resolve_type(child);
        let ancestor = self.resolve_type(ancestor);
        if type_refs_equal(&child, &ancestor) {
            return Some(BlockSet::empty());
        }
        // XSD §3.4.7 / §3.16.7 — every type derives (transitively) from
        // `xs:anyType`, and every simple type also derives from
        // `xs:anySimpleType`.  Neither is one of the 44 enumerated
        // built-ins, so the chain walk below can't see them; short-circuit
        // here.  The derivation is restriction in both directions:
        // anySimpleType is the restriction of anyType to simple content,
        // and every specific simple type is a restriction of anySimpleType.
        if is_any_type(&ancestor) {
            return Some(BlockSet::RESTRICTION);
        }
        if is_any_simple_type(&ancestor) {
            return match &child {
                TypeRef::Simple(_)  => Some(BlockSet::RESTRICTION),
                TypeRef::Complex(_) => None,
            };
        }
        match &child {
            TypeRef::Complex(ct) => {
                let mut methods = BlockSet::empty();
                let mut cur: Arc<ComplexType> = ct.clone();
                // Bound the walk in case of malformed schemas with
                // cycles — real chains in practice are 2-5 deep.
                for _ in 0..64 {
                    let d = cur.derivation.as_ref()?;
                    methods |= match d.method {
                        DerivationMethod::Restriction => BlockSet::RESTRICTION,
                        DerivationMethod::Extension   => BlockSet::EXTENSION,
                    };
                    let resolved_base = self.resolve_type(&d.base);
                    if type_refs_equal(&resolved_base, &ancestor) {
                        return Some(methods);
                    }
                    match resolved_base {
                        TypeRef::Complex(next) => { cur = next; }
                        TypeRef::Simple(_)     => return None,
                    }
                }
                None
            }
            TypeRef::Simple(s_child) => {
                if let TypeRef::Simple(s_anc) = &ancestor {
                    // Built-in derivation per XSD §3.16: every chain
                    // between built-ins is restriction.
                    if s_child.builtin.derives_from(s_anc.builtin) {
                        return Some(BlockSet::RESTRICTION);
                    }
                    // Named simple types: derives-from is determined
                    // by traversing the user simple type's name.
                    if s_child.name.as_deref() == s_anc.name.as_deref()
                        && s_child.name.is_some()
                    {
                        return Some(BlockSet::empty());
                    }
                    // Union membership: an xsi:type that's one of the
                    // ancestor's union members is a valid restriction
                    // (per cvc-elt-2 the xsi:type's value space ⊆ the
                    // declared union's value space).
                    use super::types::Variety;
                    if let Variety::Union { members } = &s_anc.variety {
                        for m in members {
                            if self.derivation_methods_from(
                                &TypeRef::Simple(s_child.clone()),
                                &TypeRef::Simple(m.clone()),
                            ).is_some() {
                                return Some(BlockSet::RESTRICTION);
                            }
                        }
                    }
                }
                None
            }
        }
    }
}

/// True if `t` represents `xs:anyType` — either the synthesised complex
/// type built by [`any_type_ref`] or an unresolved placeholder produced
/// for `type="xs:anyType"` on a declaration the parser couldn't fold in.
fn is_any_type(t: &TypeRef) -> bool {
    match t {
        TypeRef::Complex(ct) => ct.name.as_ref()
            .is_some_and(|n| n.namespace.as_deref() == Some(QName::XSD_NS)
                          && n.local.as_ref() == "anyType"),
        TypeRef::Simple(st) => is_xsd_placeholder(st, "anyType"),
    }
}

/// True if `t` represents `xs:anySimpleType`.  The schema parser
/// currently emits an UNRESOLVED simple placeholder for it (no top-level
/// declaration exists to patch over), so most callers see it through the
/// placeholder shape.
/// True if `t` represents `xs:anySimpleType`.  Three shapes can show
/// up at this site: the UNRESOLVED placeholder the parser emits for a
/// reference (`type="xs:anySimpleType"`), the resolved built-in with
/// `name="anySimpleType"` returned from [`lookup_type_by_qname`], or
/// an anonymous SimpleType whose `builtin` is `AnySimpleType` (the
/// shape the parser produces for an inline `<xs:element type=
/// "xs:anySimpleType"/>` declaration — `name` is unset).
fn is_any_simple_type(t: &TypeRef) -> bool {
    match t {
        TypeRef::Simple(st) => is_xsd_placeholder(st, "anySimpleType")
            || st.name.as_deref() == Some("anySimpleType")
            || matches!(st.builtin, super::types::BuiltinType::AnySimpleType),
        _ => false,
    }
}

fn is_xsd_placeholder(st: &Arc<SimpleType>, local: &str) -> bool {
    st.name.as_deref()
        .and_then(|n| n.strip_prefix("UNRESOLVED:"))
        .map(parse_unresolved_marker)
        .is_some_and(|qn| qn.namespace.as_deref() == Some(QName::XSD_NS)
                      && qn.local.as_ref() == local)
}

/// Identity for type references: same Arc, or — when independent
/// SimpleType wrappers are constructed for the same built-in (e.g.
/// `lookup_type_by_qname` produces fresh wrappers for `xs:int`
/// queries) — same name + same built-in.
fn type_refs_equal(a: &TypeRef, b: &TypeRef) -> bool {
    match (a, b) {
        (TypeRef::Complex(x), TypeRef::Complex(y)) => {
            Arc::ptr_eq(x, y) || (x.name.is_some() && x.name == y.name)
        }
        (TypeRef::Simple(x), TypeRef::Simple(y)) => {
            Arc::ptr_eq(x, y)
                || (x.builtin == y.builtin && x.name == y.name)
        }
        _ => false,
    }
}

/// Render a [`BlockSet`] as a human-friendly token list for
/// inclusion in error messages.
fn format_block_set(b: BlockSet) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if b.contains(BlockSet::RESTRICTION)  { parts.push("restriction"); }
    if b.contains(BlockSet::EXTENSION)    { parts.push("extension"); }
    if b.contains(BlockSet::SUBSTITUTION) { parts.push("substitution"); }
    parts.join(" ")
}

/// True if `fp` is the trivial `.` (current-element text) field
/// path — the only shape for which we currently know how to type-
/// canonicalise the collected value.  Attribute / nested-child
/// fields would need per-step type lookup against the schema's
/// attribute uses or descendant element decls (a future extension);
/// they fall through to raw string comparison meanwhile.
fn field_path_is_dot(fp: &super::identity::FieldPath) -> bool {
    fp.paths.iter().all(|p| p.steps.is_empty())
}

/// Canonicalise a raw lexical field value into a key fragment that
/// compares per the field type's value-space equality (XSD 1.0
/// §3.11.4 cvc-identity-constraint.4.2.2).  Two values are
/// equal iff their canonical key fragments are equal.
///
/// Strategy: tag with the type's PRIMITIVE built-in so values of
/// different primitive families never collide (boolean(1) ≠
/// decimal(1)), then format the parsed value canonically so two
/// lexically different but value-equal entries collide
/// (decimal("1") == decimal("1.0")).  Unparseable values fall back
/// to the raw lexical with the primitive tag — they're already
/// either invalid (and surfaced elsewhere) or simply unsupported
/// in the typed-value layer here, in which case lex equality is
/// the conservative answer.  Types we can't pin to a simple
/// builtin (raw Complex with non-simple content) pass through
/// untagged so the prior behaviour is preserved.
fn canonical_field_key(raw: &str, simple_type: Option<&Arc<SimpleType>>) -> String {
    use super::types::{BuiltinType, Value, parse_lexical};
    let Some(st) = simple_type else { return raw.to_string(); };
    let prim = st.builtin.primitive();
    let tag = prim.name();
    // Apply the type's whitespace facet to match the parser's view.
    let prepared: std::borrow::Cow<'_, str> = match st.whitespace {
        super::WhitespaceMode::Preserve => std::borrow::Cow::Borrowed(raw),
        super::WhitespaceMode::Replace  =>
            std::borrow::Cow::Owned(raw.chars().map(|c| match c {
                '\t' | '\n' | '\r' => ' ',
                _ => c,
            }).collect()),
        super::WhitespaceMode::Collapse =>
            std::borrow::Cow::Owned(raw.split_whitespace().collect::<Vec<_>>().join(" ")),
    };
    let parsed = parse_lexical(st.builtin, &prepared);
    let body: String = match parsed {
        Ok(Value::Bool(b))        => if b { "true".into() } else { "false".into() },
        Ok(Value::Decimal(d))     => d.normalize().to_string(),
        Ok(Value::Int(n))         => n.to_string(),
        Ok(Value::BigInt(b))      => {
            let sign = if b.negative { "-" } else { "" };
            format!("{sign}{}", b.digits)
        }
        Ok(Value::Float(f))       => format!("{f:?}"),
        Ok(Value::Double(d))      => format!("{d:?}"),
        Ok(Value::String(s))      => s,
        Ok(Value::Token(t))       => t,
        Ok(Value::Bytes(bs))      => bs.iter().map(|b| format!("{b:02X}")).collect(),
        Ok(Value::DateTime(dt))   => format!("{dt:?}"),
        Ok(Value::Date(d))        => format!("{d:?}"),
        Ok(Value::Time(t))        => format!("{t:?}"),
        Ok(Value::GYearMonth(g))  => format!("{g:?}"),
        Ok(Value::GYear(g))       => format!("{g:?}"),
        Ok(Value::GMonthDay(g))   => format!("{g:?}"),
        Ok(Value::GDay(g))        => format!("{g:?}"),
        Ok(Value::GMonth(g))      => format!("{g:?}"),
        Ok(Value::Duration(d))    => format!("{d:?}"),
        // Parse failure — fall through to raw under the same tag so
        // an invalid value still compares against itself coherently.
        Err(_)                    => prepared.to_string(),
    };
    let _ = BuiltinType::AnySimpleType; // silence unused warning if path skips Ok arms
    format!("{tag}:{body}")
}

fn format_tuple(t: &KeyTuple) -> String {
    let mut s = String::from("(");
    for (i, v) in t.iter().enumerate() {
        if i > 0 { s.push_str(", "); }
        match v {
            Some(v) => { s.push('"'); s.push_str(v); s.push('"'); }
            None    => s.push_str("<null>"),
        }
    }
    s.push(')');
    s
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn xsd_str(extra_decls: &str) -> String {
        format!(
            r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:test"
           xmlns="urn:test"
           elementFormDefault="qualified">
{extra_decls}
</xs:schema>"#
        )
    }

    fn instance_ns() -> &'static str { r#"xmlns="urn:test""# }

    #[test]
    fn validate_simple_string() {
        let s = Schema::compile_str(&xsd_str(
            r#"<xs:element name="msg" type="xs:string"/>"#
        )).unwrap();
        s.validate_str(&format!(r#"<msg {ns}>hello</msg>"#, ns = instance_ns())).unwrap();
    }

    #[test]
    fn validate_typed_int_value() {
        let s = Schema::compile_str(&xsd_str(
            r#"<xs:element name="age" type="xs:int"/>"#
        )).unwrap();
        assert!(s.validate_str(&format!(r#"<age {}>42</age>"#, instance_ns())).is_ok());
        assert!(s.validate_str(&format!(r#"<age {}>not-an-int</age>"#, instance_ns())).is_err());
    }

    #[test]
    fn validate_facet_pattern() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="zip" type="ZipCode"/>
            <xs:simpleType name="ZipCode">
                <xs:restriction base="xs:string">
                    <xs:pattern value="\d{5}(-\d{4})?"/>
                </xs:restriction>
            </xs:simpleType>
        "#)).unwrap();
        let ns = instance_ns();
        assert!(s.validate_str(&format!(r#"<zip {ns}>12345</zip>"#)).is_ok());
        assert!(s.validate_str(&format!(r#"<zip {ns}>12345-6789</zip>"#)).is_ok());
        assert!(s.validate_str(&format!(r#"<zip {ns}>not-a-zip</zip>"#)).is_err());
    }

    #[test]
    fn validate_complex_sequence() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="person" type="Person"/>
            <xs:complexType name="Person">
                <xs:sequence>
                    <xs:element name="name" type="xs:string"/>
                    <xs:element name="age"  type="xs:int"/>
                </xs:sequence>
            </xs:complexType>
        "#)).unwrap();
        let ns = instance_ns();
        assert!(s.validate_str(&format!(
            r#"<person {ns}><name>Ada</name><age>30</age></person>"#
        )).is_ok());
        // Wrong order.
        assert!(s.validate_str(&format!(
            r#"<person {ns}><age>30</age><name>Ada</name></person>"#
        )).is_err());
    }

    #[test]
    fn validate_required_attribute() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="thing" type="Thing"/>
            <xs:complexType name="Thing">
                <xs:attribute name="id" type="xs:int" use="required"/>
            </xs:complexType>
        "#)).unwrap();
        let ns = instance_ns();
        assert!(s.validate_str(&format!(r#"<thing {ns} id="1"/>"#)).is_ok());
        assert!(s.validate_str(&format!(r#"<thing {ns}/>"#)).is_err());
    }

    #[test]
    fn validate_attribute_type() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="thing" type="Thing"/>
            <xs:complexType name="Thing">
                <xs:attribute name="age" type="xs:int" use="required"/>
            </xs:complexType>
        "#)).unwrap();
        let ns = instance_ns();
        assert!(s.validate_str(&format!(r#"<thing {ns} age="not-int"/>"#)).is_err());
    }

    #[test]
    fn validate_max_occurs() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="items" type="Items"/>
            <xs:complexType name="Items">
                <xs:sequence>
                    <xs:element name="item" type="xs:string" maxOccurs="3"/>
                </xs:sequence>
            </xs:complexType>
        "#)).unwrap();
        let ns = instance_ns();
        assert!(s.validate_str(&format!(
            r#"<items {ns}><item>a</item><item>b</item><item>c</item></items>"#
        )).is_ok());
        assert!(s.validate_str(&format!(
            r#"<items {ns}><item>a</item><item>b</item><item>c</item><item>d</item></items>"#
        )).is_err());
    }

    #[test]
    fn validate_min_occurs() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="items" type="Items"/>
            <xs:complexType name="Items">
                <xs:sequence>
                    <xs:element name="item" type="xs:string"
                                minOccurs="2" maxOccurs="unbounded"/>
                </xs:sequence>
            </xs:complexType>
        "#)).unwrap();
        let ns = instance_ns();
        assert!(s.validate_str(&format!(
            r#"<items {ns}><item>a</item><item>b</item></items>"#
        )).is_ok());
        assert!(s.validate_str(&format!(
            r#"<items {ns}><item>a</item></items>"#
        )).is_err());
    }

    #[test]
    fn validate_unbounded() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="items" type="Items"/>
            <xs:complexType name="Items">
                <xs:sequence>
                    <xs:element name="item" type="xs:string" maxOccurs="unbounded"/>
                </xs:sequence>
            </xs:complexType>
        "#)).unwrap();
        let ns = instance_ns();
        let mut xml = format!(r#"<items {ns}>"#);
        for _ in 0..50 { xml.push_str("<item>x</item>"); }
        xml.push_str("</items>");
        assert!(s.validate_str(&xml).is_ok());
    }

    #[test]
    fn validate_choice() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="either" type="Either"/>
            <xs:complexType name="Either">
                <xs:choice>
                    <xs:element name="left"  type="xs:int"/>
                    <xs:element name="right" type="xs:string"/>
                </xs:choice>
            </xs:complexType>
        "#)).unwrap();
        let ns = instance_ns();
        assert!(s.validate_str(&format!(r#"<either {ns}><left>1</left></either>"#)).is_ok());
        assert!(s.validate_str(&format!(r#"<either {ns}><right>hi</right></either>"#)).is_ok());
        // No branch chosen.
        assert!(s.validate_str(&format!(r#"<either {ns}/>"#)).is_err());
    }

    #[test]
    fn validate_all_any_order() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="both" type="Both"/>
            <xs:complexType name="Both">
                <xs:all>
                    <xs:element name="a" type="xs:int"/>
                    <xs:element name="b" type="xs:int"/>
                </xs:all>
            </xs:complexType>
        "#)).unwrap();
        let ns = instance_ns();
        assert!(s.validate_str(&format!(r#"<both {ns}><a>1</a><b>2</b></both>"#)).is_ok());
        assert!(s.validate_str(&format!(r#"<both {ns}><b>2</b><a>1</a></both>"#)).is_ok());
        // Missing one of the two.
        assert!(s.validate_str(&format!(r#"<both {ns}><a>1</a></both>"#)).is_err());
    }

    #[test]
    fn validate_xsi_nil() {
        let s = Schema::compile_str(&xsd_str(
            r#"<xs:element name="opt" type="xs:int" nillable="true"/>"#
        )).unwrap();
        let inst = format!(
            r#"<opt {ns} xmlns:xsi="{xsi}" xsi:nil="true"/>"#,
            ns = instance_ns(), xsi = XSI_NS,
        );
        assert!(s.validate_str(&inst).is_ok());
    }

    #[test]
    fn validate_xsi_nil_on_non_nillable_fails() {
        let s = Schema::compile_str(&xsd_str(
            r#"<xs:element name="opt" type="xs:int"/>"#
        )).unwrap();
        let inst = format!(
            r#"<opt {ns} xmlns:xsi="{xsi}" xsi:nil="true"/>"#,
            ns = instance_ns(), xsi = XSI_NS,
        );
        assert!(s.validate_str(&inst).is_err());
    }

    #[test]
    fn validate_unknown_root_fails() {
        let s = Schema::compile_str(&xsd_str(
            r#"<xs:element name="known" type="xs:string"/>"#
        )).unwrap();
        let ns = instance_ns();
        assert!(s.validate_str(&format!(r#"<unknown {ns}>x</unknown>"#)).is_err());
    }

    #[test]
    fn validate_collects_multiple_issues_when_not_fail_fast() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="thing" type="Thing"/>
            <xs:complexType name="Thing">
                <xs:attribute name="id"   type="xs:int"    use="required"/>
                <xs:attribute name="name" type="xs:string" use="required"/>
            </xs:complexType>
        "#)).unwrap();
        let inst = format!(r#"<thing {}/>"#, instance_ns());
        let opts = ValidationOptions { fail_fast: false, max_issues: 100, ..Default::default() };
        let err = s.validate_str_opts(&inst, opts).unwrap_err();
        assert!(err.issues.len() >= 2);
    }

    // ── identity constraints ─────────────────────────────────────────

    fn parts_xsd() -> String {
        // Catalog with `xs:key` on part numbers and `xs:keyref` on
        // line items referring to those parts — the canonical XSD
        // identity-constraint example.
        xsd_str(r#"
            <xs:element name="catalog">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="parts">
                            <xs:complexType>
                                <xs:sequence>
                                    <xs:element name="part" maxOccurs="unbounded">
                                        <xs:complexType>
                                            <xs:attribute name="num" type="xs:string" use="required"/>
                                        </xs:complexType>
                                    </xs:element>
                                </xs:sequence>
                            </xs:complexType>
                        </xs:element>
                        <xs:element name="orders">
                            <xs:complexType>
                                <xs:sequence>
                                    <xs:element name="line" maxOccurs="unbounded">
                                        <xs:complexType>
                                            <xs:attribute name="part" type="xs:string" use="required"/>
                                        </xs:complexType>
                                    </xs:element>
                                </xs:sequence>
                            </xs:complexType>
                        </xs:element>
                    </xs:sequence>
                </xs:complexType>
                <xs:key name="partKey">
                    <xs:selector xpath=".//part"/>
                    <xs:field xpath="@num"/>
                </xs:key>
                <xs:keyref name="lineRef" refer="partKey">
                    <xs:selector xpath=".//line"/>
                    <xs:field xpath="@part"/>
                </xs:keyref>
            </xs:element>
        "#)
    }

    #[test]
    fn xs_key_accepts_unique_values() {
        let s = Schema::compile_str(&parts_xsd()).unwrap();
        let xml = format!(
            r#"<catalog {ns}>
                <parts>
                    <part num="A1"/><part num="A2"/><part num="A3"/>
                </parts>
                <orders>
                    <line part="A1"/><line part="A3"/>
                </orders>
            </catalog>"#,
            ns = instance_ns(),
        );
        s.validate_str(&xml).unwrap();
    }

    #[test]
    fn xs_key_rejects_duplicates() {
        let s = Schema::compile_str(&parts_xsd()).unwrap();
        let xml = format!(
            r#"<catalog {ns}>
                <parts>
                    <part num="A1"/><part num="A1"/>
                </parts>
                <orders>
                    <line part="A1"/>
                </orders>
            </catalog>"#,
            ns = instance_ns(),
        );
        let err = s.validate_str(&xml).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::KeyNotUnique)
        ), "expected KeyNotUnique, got {:?}", err.issues);
    }

    #[test]
    fn xs_keyref_rejects_dangling() {
        let s = Schema::compile_str(&parts_xsd()).unwrap();
        let xml = format!(
            r#"<catalog {ns}>
                <parts>
                    <part num="A1"/>
                </parts>
                <orders>
                    <line part="UNKNOWN"/>
                </orders>
            </catalog>"#,
            ns = instance_ns(),
        );
        let err = s.validate_str(&xml).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::KeyRefDangling)
        ), "expected KeyRefDangling, got {:?}", err.issues);
    }

    #[test]
    fn xs_unique_allows_missing_field() {
        // xs:unique tolerates missing fields (where xs:key would error).
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="root">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="x" maxOccurs="unbounded">
                            <xs:complexType>
                                <xs:attribute name="id" type="xs:string"/>
                            </xs:complexType>
                        </xs:element>
                    </xs:sequence>
                </xs:complexType>
                <xs:unique name="xUnique">
                    <xs:selector xpath=".//x"/>
                    <xs:field xpath="@id"/>
                </xs:unique>
            </xs:element>
        "#)).unwrap();
        let xml = format!(
            r#"<root {ns}><x id="A"/><x/><x id="B"/></root>"#,
            ns = instance_ns(),
        );
        // Missing field tolerated; A and B are unique.
        s.validate_str(&xml).unwrap();
        // But duplicate present values still fail.
        let bad = format!(
            r#"<root {ns}><x id="A"/><x id="A"/></root>"#,
            ns = instance_ns(),
        );
        assert!(s.validate_str(&bad).is_err());
    }

    // ── DFA-driven content matching ──────────────────────────────────

    #[test]
    fn dfa_rejects_ambiguous_schema_at_compile_time() {
        // `(x?, x)` — when an `<x>` arrives, the parser can't tell
        // whether to consume the optional first one or skip to the
        // required second one without lookahead.  UPA violation.
        let xsd = xsd_str(r#"
            <xs:complexType name="Bad">
                <xs:sequence>
                    <xs:element name="x" type="xs:string" minOccurs="0"/>
                    <xs:element name="x" type="xs:string"/>
                </xs:sequence>
            </xs:complexType>
            <xs:element name="root" type="Bad"/>
        "#);
        let err = Schema::compile_str(&xsd).unwrap_err();
        assert!(err.message.contains("Unique Particle Attribution"),
            "expected UPA error, got {:?}", err.message);
    }

    #[test]
    fn dfa_rejects_ambiguous_choice_at_compile_time() {
        // Two branches of a choice naming the same element.  Also UPA.
        let xsd = xsd_str(r#"
            <xs:element name="root">
                <xs:complexType>
                    <xs:choice>
                        <xs:element name="x" type="xs:string"/>
                        <xs:element name="x" type="xs:int"/>
                    </xs:choice>
                </xs:complexType>
            </xs:element>
        "#);
        assert!(Schema::compile_str(&xsd).is_err());
    }

    #[test]
    fn dfa_error_message_lists_expected_elements() {
        // Missing required element should name the expected one.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="root">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="alpha" type="xs:string"/>
                        <xs:element name="beta"  type="xs:string"/>
                    </xs:sequence>
                </xs:complexType>
            </xs:element>
        "#)).unwrap();
        let xml = format!(r#"<root {ns}><alpha>x</alpha></root>"#, ns = instance_ns());
        let err = s.validate_str(&xml).unwrap_err();
        // Should mention `beta` as the missing element.
        assert!(err.issues.iter().any(|i| i.message.contains("beta")),
            "expected `beta` in error, got {:?}", err.issues);
    }

    #[test]
    fn dfa_handles_substitution_groups() {
        // Substitution groups: any element substituting for `figure`
        // can appear where `figure` is expected.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="figure" type="xs:string" abstract="true"/>
            <xs:element name="image"  type="xs:string" substitutionGroup="figure"/>
            <xs:element name="chart"  type="xs:string" substitutionGroup="figure"/>
            <xs:element name="report">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element ref="figure" maxOccurs="unbounded"/>
                    </xs:sequence>
                </xs:complexType>
            </xs:element>
        "#)).unwrap();
        let xml = format!(
            r#"<report {ns}><image>i</image><chart>c</chart><image>j</image></report>"#,
            ns = instance_ns(),
        );
        s.validate_str(&xml).unwrap();
    }

    #[test]
    fn dfa_unbounded_works_correctly() {
        // The DFA self-loop at maxOccurs=unbounded should accept any
        // count from min upward.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="items">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="item" type="xs:int" maxOccurs="unbounded"/>
                    </xs:sequence>
                </xs:complexType>
            </xs:element>
        "#)).unwrap();
        let ns = instance_ns();
        for n in [1, 5, 100, 1000] {
            let mut xml = format!(r#"<items {ns}>"#);
            for i in 0..n { xml.push_str(&format!("<item>{i}</item>")); }
            xml.push_str("</items>");
            s.validate_str(&xml)
                .unwrap_or_else(|e| panic!("n={n} should validate: {e:?}"));
        }
    }

    #[test]
    fn dfa_accepts_optional_followed_by_required() {
        // Optional first (minOccurs=0) then required — the tricky
        // case where the DFA's initial state must accept either the
        // optional element or skip directly to the required one.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="msg">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="header" type="xs:string" minOccurs="0"/>
                        <xs:element name="body"   type="xs:string"/>
                    </xs:sequence>
                </xs:complexType>
            </xs:element>
        "#)).unwrap();
        let ns = instance_ns();
        s.validate_str(&format!(
            r#"<msg {ns}><header>h</header><body>b</body></msg>"#)).unwrap();
        s.validate_str(&format!(
            r#"<msg {ns}><body>b</body></msg>"#)).unwrap();
        // Just header with no body fails.
        assert!(s.validate_str(&format!(
            r#"<msg {ns}><header>h</header></msg>"#)).is_err());
    }

    #[test]
    fn xs_key_with_child_attribute_field() {
        // Field path is `id/@value` — descend into the `id` child of
        // the matched element, then read its `@value` attribute.
        // This is a common shape (e.g. SAML assertions key on
        // <NameID> attributes inside subjects).
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="users">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="user" maxOccurs="unbounded">
                            <xs:complexType>
                                <xs:sequence>
                                    <xs:element name="id">
                                        <xs:complexType>
                                            <xs:attribute name="value" type="xs:string" use="required"/>
                                        </xs:complexType>
                                    </xs:element>
                                </xs:sequence>
                            </xs:complexType>
                        </xs:element>
                    </xs:sequence>
                </xs:complexType>
                <xs:key name="userKey">
                    <xs:selector xpath=".//user"/>
                    <xs:field xpath="id/@value"/>
                </xs:key>
            </xs:element>
        "#)).unwrap();
        let ok = format!(
            r#"<users {ns}>
                 <user><id value="alice"/></user>
                 <user><id value="bob"/></user>
               </users>"#,
            ns = instance_ns(),
        );
        s.validate_str(&ok).unwrap();
        let dup = format!(
            r#"<users {ns}>
                 <user><id value="alice"/></user>
                 <user><id value="alice"/></user>
               </users>"#,
            ns = instance_ns(),
        );
        let err = s.validate_str(&dup).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::KeyNotUnique)
        ), "expected KeyNotUnique, got {:?}", err.issues);
    }

    #[test]
    fn xs_key_with_child_text_field() {
        // Field path is `name` — read the text content of the `name`
        // child of the matched element.  (Equivalent to `name/.` but
        // omitting the trailing dot is the common form.)
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="users">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="user" maxOccurs="unbounded">
                            <xs:complexType>
                                <xs:sequence>
                                    <xs:element name="name" type="xs:string"/>
                                </xs:sequence>
                            </xs:complexType>
                        </xs:element>
                    </xs:sequence>
                </xs:complexType>
                <xs:key name="userKey">
                    <xs:selector xpath=".//user"/>
                    <xs:field xpath="name"/>
                </xs:key>
            </xs:element>
        "#)).unwrap();
        let ok = format!(
            r#"<users {ns}>
                 <user><name>alice</name></user>
                 <user><name>bob</name></user>
               </users>"#,
            ns = instance_ns(),
        );
        s.validate_str(&ok).unwrap();
        let dup = format!(
            r#"<users {ns}>
                 <user><name>alice</name></user>
                 <user><name>alice</name></user>
               </users>"#,
            ns = instance_ns(),
        );
        let err = s.validate_str(&dup).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::KeyNotUnique)
        ), "expected KeyNotUnique, got {:?}", err.issues);
    }

    #[test]
    fn xs_key_text_field() {
        // Field uses `.` (text content) instead of @attr.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="cities">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="city" type="xs:string" maxOccurs="unbounded"/>
                    </xs:sequence>
                </xs:complexType>
                <xs:key name="cityKey">
                    <xs:selector xpath=".//city"/>
                    <xs:field xpath="."/>
                </xs:key>
            </xs:element>
        "#)).unwrap();
        let ok = format!(
            r#"<cities {ns}><city>Boston</city><city>NYC</city></cities>"#,
            ns = instance_ns(),
        );
        s.validate_str(&ok).unwrap();
        let bad = format!(
            r#"<cities {ns}><city>Boston</city><city>Boston</city></cities>"#,
            ns = instance_ns(),
        );
        assert!(s.validate_str(&bad).is_err());
    }

    // ── deeper-than-two-step field paths ───────────────────────────

    #[test]
    fn xs_key_with_grandchild_text_field() {
        // Field path is `name/first` — two child steps.  The matched
        // element is `user`; the value is the text content of
        // `user/name/first`.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="users">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="user" maxOccurs="unbounded">
                            <xs:complexType>
                                <xs:sequence>
                                    <xs:element name="name">
                                        <xs:complexType>
                                            <xs:sequence>
                                                <xs:element name="first" type="xs:string"/>
                                                <xs:element name="last"  type="xs:string"/>
                                            </xs:sequence>
                                        </xs:complexType>
                                    </xs:element>
                                </xs:sequence>
                            </xs:complexType>
                        </xs:element>
                    </xs:sequence>
                </xs:complexType>
                <xs:key name="userKey">
                    <xs:selector xpath=".//user"/>
                    <xs:field xpath="name/first"/>
                </xs:key>
            </xs:element>
        "#)).unwrap();
        let ok = format!(
            r#"<users {ns}>
                 <user><name><first>Alice</first><last>A</last></name></user>
                 <user><name><first>Bob</first><last>B</last></name></user>
               </users>"#,
            ns = instance_ns(),
        );
        s.validate_str(&ok).unwrap();
        let dup = format!(
            r#"<users {ns}>
                 <user><name><first>Alice</first><last>A</last></name></user>
                 <user><name><first>Alice</first><last>Z</last></name></user>
               </users>"#,
            ns = instance_ns(),
        );
        let err = s.validate_str(&dup).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::KeyNotUnique)
        ), "expected KeyNotUnique on duplicate name/first, got {:?}", err.issues);
    }

    #[test]
    fn xs_key_with_grandchild_attribute_field() {
        // Three-step path ending in attribute: `name/first/@lang`.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="users">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="user" maxOccurs="unbounded">
                            <xs:complexType>
                                <xs:sequence>
                                    <xs:element name="name">
                                        <xs:complexType>
                                            <xs:sequence>
                                                <xs:element name="first">
                                                    <xs:complexType>
                                                        <xs:simpleContent>
                                                            <xs:extension base="xs:string">
                                                                <xs:attribute name="lang" type="xs:string"/>
                                                            </xs:extension>
                                                        </xs:simpleContent>
                                                    </xs:complexType>
                                                </xs:element>
                                            </xs:sequence>
                                        </xs:complexType>
                                    </xs:element>
                                </xs:sequence>
                            </xs:complexType>
                        </xs:element>
                    </xs:sequence>
                </xs:complexType>
                <xs:key name="userKey">
                    <xs:selector xpath=".//user"/>
                    <xs:field xpath="name/first/@lang"/>
                </xs:key>
            </xs:element>
        "#)).unwrap();
        let ok = format!(
            r#"<users {ns}>
                 <user><name><first lang="en">Alice</first></name></user>
                 <user><name><first lang="fr">Alphonse</first></name></user>
               </users>"#,
            ns = instance_ns(),
        );
        s.validate_str(&ok).unwrap();
        let dup = format!(
            r#"<users {ns}>
                 <user><name><first lang="en">Alice</first></name></user>
                 <user><name><first lang="en">Bob</first></name></user>
               </users>"#,
            ns = instance_ns(),
        );
        let err = s.validate_str(&dup).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::KeyNotUnique)
        ), "expected KeyNotUnique on duplicate @lang, got {:?}", err.issues);
    }

    #[test]
    fn xs_keyref_with_deep_field() {
        // Both key and keyref use a 3-step child path ending in an
        // attribute: `header/meta/@sku`.  The keyref should resolve
        // against the deeply-extracted key values.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="catalog">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="parts">
                            <xs:complexType>
                                <xs:sequence>
                                    <xs:element name="part" maxOccurs="unbounded">
                                        <xs:complexType>
                                            <xs:sequence>
                                                <xs:element name="header">
                                                    <xs:complexType>
                                                        <xs:sequence>
                                                            <xs:element name="meta">
                                                                <xs:complexType>
                                                                    <xs:attribute name="sku" type="xs:string"/>
                                                                </xs:complexType>
                                                            </xs:element>
                                                        </xs:sequence>
                                                    </xs:complexType>
                                                </xs:element>
                                            </xs:sequence>
                                        </xs:complexType>
                                    </xs:element>
                                </xs:sequence>
                            </xs:complexType>
                        </xs:element>
                        <xs:element name="orders">
                            <xs:complexType>
                                <xs:sequence>
                                    <xs:element name="line" maxOccurs="unbounded">
                                        <xs:complexType>
                                            <xs:sequence>
                                                <xs:element name="header">
                                                    <xs:complexType>
                                                        <xs:sequence>
                                                            <xs:element name="ref">
                                                                <xs:complexType>
                                                                    <xs:attribute name="sku" type="xs:string"/>
                                                                </xs:complexType>
                                                            </xs:element>
                                                        </xs:sequence>
                                                    </xs:complexType>
                                                </xs:element>
                                            </xs:sequence>
                                        </xs:complexType>
                                    </xs:element>
                                </xs:sequence>
                            </xs:complexType>
                        </xs:element>
                    </xs:sequence>
                </xs:complexType>
                <xs:key name="partKey">
                    <xs:selector xpath=".//part"/>
                    <xs:field xpath="header/meta/@sku"/>
                </xs:key>
                <xs:keyref name="lineRef" refer="partKey">
                    <xs:selector xpath=".//line"/>
                    <xs:field xpath="header/ref/@sku"/>
                </xs:keyref>
            </xs:element>
        "#)).unwrap();
        let ok = format!(
            r#"<catalog {ns}>
                 <parts>
                   <part><header><meta sku="A1"/></header></part>
                   <part><header><meta sku="A2"/></header></part>
                 </parts>
                 <orders>
                   <line><header><ref sku="A1"/></header></line>
                   <line><header><ref sku="A2"/></header></line>
                 </orders>
               </catalog>"#,
            ns = instance_ns(),
        );
        s.validate_str(&ok).unwrap();
        let bad = format!(
            r#"<catalog {ns}>
                 <parts>
                   <part><header><meta sku="A1"/></header></part>
                 </parts>
                 <orders>
                   <line><header><ref sku="A1"/></header></line>
                   <line><header><ref sku="UNKNOWN"/></header></line>
                 </orders>
               </catalog>"#,
            ns = instance_ns(),
        );
        let err = s.validate_str(&bad).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::KeyRefDangling)
        ), "expected KeyRefDangling on unknown sku, got {:?}", err.issues);
    }

    #[test]
    fn xs_unique_three_level_child_path() {
        // Three child steps (no trailing attribute): `a/b/c`.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="root">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="item" maxOccurs="unbounded">
                            <xs:complexType>
                                <xs:sequence>
                                    <xs:element name="a">
                                        <xs:complexType>
                                            <xs:sequence>
                                                <xs:element name="b">
                                                    <xs:complexType>
                                                        <xs:sequence>
                                                            <xs:element name="c" type="xs:string"/>
                                                        </xs:sequence>
                                                    </xs:complexType>
                                                </xs:element>
                                            </xs:sequence>
                                        </xs:complexType>
                                    </xs:element>
                                </xs:sequence>
                            </xs:complexType>
                        </xs:element>
                    </xs:sequence>
                </xs:complexType>
                <xs:unique name="itemUnique">
                    <xs:selector xpath=".//item"/>
                    <xs:field xpath="a/b/c"/>
                </xs:unique>
            </xs:element>
        "#)).unwrap();
        let ok = format!(
            r#"<root {ns}>
                 <item><a><b><c>x</c></b></a></item>
                 <item><a><b><c>y</c></b></a></item>
               </root>"#,
            ns = instance_ns(),
        );
        s.validate_str(&ok).unwrap();
        let dup = format!(
            r#"<root {ns}>
                 <item><a><b><c>x</c></b></a></item>
                 <item><a><b><c>x</c></b></a></item>
               </root>"#,
            ns = instance_ns(),
        );
        let err = s.validate_str(&dup).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::KeyNotUnique)
        ), "expected KeyNotUnique on duplicate a/b/c, got {:?}", err.issues);
    }

    #[test]
    fn xs_key_deep_path_missing_intermediate_is_missing_field() {
        // <xs:key> with a 3-step path; if any intermediate child is
        // absent, the field is "missing" and (for xs:key) that's an
        // error.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="users">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="user" maxOccurs="unbounded">
                            <xs:complexType>
                                <xs:sequence>
                                    <xs:element name="name" minOccurs="0">
                                        <xs:complexType>
                                            <xs:sequence>
                                                <xs:element name="first" type="xs:string" minOccurs="0"/>
                                            </xs:sequence>
                                        </xs:complexType>
                                    </xs:element>
                                </xs:sequence>
                            </xs:complexType>
                        </xs:element>
                    </xs:sequence>
                </xs:complexType>
                <xs:key name="userKey">
                    <xs:selector xpath=".//user"/>
                    <xs:field xpath="name/first"/>
                </xs:key>
            </xs:element>
        "#)).unwrap();
        // Second <user> has no <name>, so its field is missing.
        // First <user> has the full path — its value must be
        // extracted, NOT also reported as missing.
        let bad = format!(
            r#"<users {ns}>
                 <user><name><first>Alice</first></name></user>
                 <user/>
               </users>"#,
            ns = instance_ns(),
        );
        let err = s.validate_str(&bad).unwrap_err();
        let missing: Vec<&ValidationIssue> = err.issues.iter()
            .filter(|i| i.message.contains("missing one of the field values"))
            .collect();
        // Exactly one missing-field error — proves the first user's
        // value WAS extracted (otherwise both would miss).
        assert_eq!(missing.len(), 1,
            "expected exactly one missing-field error, got {:?}", err.issues);
    }

    // ── line/column locators on validation issues ──────────────────

    #[test]
    fn issue_carries_line_and_column_for_simple_content() {
        let s = Schema::compile_str(&xsd_str(
            r#"<xs:element name="age" type="xs:int"/>"#
        )).unwrap();
        let bad = format!(r#"<age {}>not-an-int</age>"#, instance_ns());
        let err = s.validate_str(&bad).unwrap_err();
        let issue = &err.issues[0];
        assert_eq!(issue.line, Some(1), "single-line input, expected line 1, got {issue:?}");
        assert!(issue.column.is_some(), "expected column to be filled, got {issue:?}");
    }

    #[test]
    fn issue_line_points_at_offending_element_in_multiline_input() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="r">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="bad" type="xs:int"/>
                    </xs:sequence>
                </xs:complexType>
            </xs:element>
        "#)).unwrap();
        // <bad> sits on line 4 (after newlines on 1/2/3).
        let bad = format!("<r {}>\n  \n  \n  <bad>not-an-int</bad>\n</r>", instance_ns());
        let err = s.validate_str(&bad).unwrap_err();
        let issue = err.issues.iter()
            .find(|i| i.message.contains("element content"))
            .expect("expected element-content error");
        assert_eq!(issue.line, Some(4),
            "expected <bad> on line 4, got {issue:?}");
    }

    #[test]
    fn issue_line_for_missing_required_element_points_at_parent() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="r">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="x" type="xs:string"/>
                    </xs:sequence>
                </xs:complexType>
            </xs:element>
        "#)).unwrap();
        // <r> on line 2; missing required <x> should anchor at line 2.
        let bad = format!("\n<r {}>\n</r>", instance_ns());
        let err = s.validate_str(&bad).unwrap_err();
        let issue = err.issues.iter()
            .find(|i| i.message.contains("missing required element"))
            .expect("expected missing-required-element error");
        assert_eq!(issue.line, Some(2),
            "expected <r> on line 2, got {issue:?}");
    }

    #[test]
    fn issue_line_for_unexpected_element_points_at_the_element_not_parent() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="r">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="ok" type="xs:string"/>
                    </xs:sequence>
                </xs:complexType>
            </xs:element>
        "#)).unwrap();
        // <r> line 1; <bad> line 2.  The unexpected-element issue
        // must point at line 2, not line 1.
        let bad = format!("<r {}>\n  <bad/>\n</r>", instance_ns());
        let err = s.validate_str(&bad).unwrap_err();
        let issue = err.issues.iter()
            .find(|i| i.message.contains("unexpected element"))
            .expect("expected unexpected-element error");
        assert_eq!(issue.line, Some(2),
            "expected <bad> on line 2, got {issue:?}");
    }

    #[test]
    fn issue_line_for_missing_required_attribute_points_at_element() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="r">
                <xs:complexType>
                    <xs:attribute name="must" type="xs:string" use="required"/>
                </xs:complexType>
            </xs:element>
        "#)).unwrap();
        let bad = format!("\n\n<r {}/>", instance_ns());
        let err = s.validate_str(&bad).unwrap_err();
        let issue = err.issues.iter()
            .find(|i| i.message.contains("missing required attribute"))
            .expect("expected missing-required-attribute error");
        assert_eq!(issue.line, Some(3),
            "expected <r> on line 3, got {issue:?}");
    }

    // ── xsi:nil edge cases ─────────────────────────────────────────

    #[test]
    fn xsi_nil_true_with_empty_content_validates() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="opt" type="xs:int" nillable="true"/>
        "#)).unwrap();
        let xsi = "http://www.w3.org/2001/XMLSchema-instance";
        s.validate_str(&format!(r#"<opt {ns} xmlns:xsi="{xsi}" xsi:nil="true"/>"#,
            ns = instance_ns())).unwrap();
        // whitespace-only content is still "empty"
        s.validate_str(&format!(r#"<opt {ns} xmlns:xsi="{xsi}" xsi:nil="true">   </opt>"#,
            ns = instance_ns())).unwrap();
    }

    #[test]
    fn xsi_nil_true_with_non_empty_content_fails() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="opt" type="xs:int" nillable="true"/>
        "#)).unwrap();
        let xsi = "http://www.w3.org/2001/XMLSchema-instance";
        let err = s.validate_str(&format!(
            r#"<opt {ns} xmlns:xsi="{xsi}" xsi:nil="true">42</opt>"#,
            ns = instance_ns())).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::NillableViolation)
        ), "expected NillableViolation, got {:?}", err.issues);
    }

    #[test]
    fn xsi_nil_on_non_nillable_element_fails() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="opt" type="xs:int"/>
        "#)).unwrap();
        let xsi = "http://www.w3.org/2001/XMLSchema-instance";
        let err = s.validate_str(&format!(
            r#"<opt {ns} xmlns:xsi="{xsi}" xsi:nil="true"/>"#,
            ns = instance_ns())).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::NillableViolation)
                && i.message.contains("non-nillable")
        ), "expected non-nillable rejection, got {:?}", err.issues);
    }

    #[test]
    fn xsi_nil_with_required_attribute_still_validates_attribute() {
        // An element with a required attribute and xsi:nil="true":
        // the attribute is still required; content must still be empty.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="opt" nillable="true">
                <xs:complexType>
                    <xs:attribute name="id" type="xs:string" use="required"/>
                </xs:complexType>
            </xs:element>
        "#)).unwrap();
        let xsi = "http://www.w3.org/2001/XMLSchema-instance";
        // Present + nil → valid.
        s.validate_str(&format!(
            r#"<opt id="x" {ns} xmlns:xsi="{xsi}" xsi:nil="true"/>"#,
            ns = instance_ns())).unwrap();
        // Missing required attr + nil → still a missing-attribute error.
        let err = s.validate_str(&format!(
            r#"<opt {ns} xmlns:xsi="{xsi}" xsi:nil="true"/>"#,
            ns = instance_ns())).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::MissingRequiredAttribute)
        ), "expected MissingRequiredAttribute even under xsi:nil, got {:?}", err.issues);
    }

    #[test]
    fn xsi_nil_skips_required_children_check() {
        // With xsi:nil="true", a complex type's required child elements
        // are NOT required (the element is treated as absent for
        // content-model purposes).
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="opt" nillable="true">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="req" type="xs:string"/>
                    </xs:sequence>
                </xs:complexType>
            </xs:element>
        "#)).unwrap();
        let xsi = "http://www.w3.org/2001/XMLSchema-instance";
        // Without xsi:nil, missing <req> fails.
        let err = s.validate_str(&format!(r#"<opt {ns}/>"#, ns = instance_ns())).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::MissingRequiredElement)
        ));
        // With xsi:nil, the required child is not enforced.
        s.validate_str(&format!(
            r#"<opt {ns} xmlns:xsi="{xsi}" xsi:nil="true"/>"#,
            ns = instance_ns())).unwrap();
    }

    #[test]
    fn xsi_nil_overrides_fixed_value_check() {
        // Per XSD §3.3.4 / §2.6.2: an element with xsi:nil="true" is
        // treated as having no value, so the `fixed=` constraint does
        // not apply.  (Without this carve-out, fixed="ABC" + nil would
        // always fail because text_buf is "" ≠ "ABC".)
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="opt" type="xs:string" nillable="true" fixed="ABC"/>
        "#)).unwrap();
        let xsi = "http://www.w3.org/2001/XMLSchema-instance";
        // xsi:nil → fixed= waived.
        s.validate_str(&format!(
            r#"<opt {ns} xmlns:xsi="{xsi}" xsi:nil="true"/>"#,
            ns = instance_ns())).unwrap();
        // Without xsi:nil, fixed= enforces.
        s.validate_str(&format!(r#"<opt {ns}>ABC</opt>"#, ns = instance_ns())).unwrap();
        let err = s.validate_str(&format!(
            r#"<opt {ns}>XYZ</opt>"#, ns = instance_ns())).unwrap_err();
        assert!(err.issues.iter().any(|i| i.message.contains("fixed")),
            "expected fixed mismatch, got {:?}", err.issues);
    }

    #[test]
    fn xsi_nil_false_validates_normally() {
        // xsi:nil="false" is equivalent to not having it — content
        // must validate normally.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="opt" type="xs:int" nillable="true"/>
        "#)).unwrap();
        let xsi = "http://www.w3.org/2001/XMLSchema-instance";
        s.validate_str(&format!(
            r#"<opt {ns} xmlns:xsi="{xsi}" xsi:nil="false">42</opt>"#,
            ns = instance_ns())).unwrap();
        let err = s.validate_str(&format!(
            r#"<opt {ns} xmlns:xsi="{xsi}" xsi:nil="false">foo</opt>"#,
            ns = instance_ns())).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::TypeMismatch)
        ), "expected TypeMismatch for non-int content, got {:?}", err.issues);
    }

    #[test]
    fn xsi_nil_with_xsi_type_uses_substituted_type() {
        // xsi:nil and xsi:type may both be set; nil applies to the
        // substituted type.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="Base">
                <xs:sequence>
                    <xs:element name="child" type="xs:string"/>
                </xs:sequence>
            </xs:complexType>
            <xs:complexType name="Derived">
                <xs:complexContent>
                    <xs:extension base="Base">
                        <xs:sequence>
                            <xs:element name="extra" type="xs:string"/>
                        </xs:sequence>
                    </xs:extension>
                </xs:complexContent>
            </xs:complexType>
            <xs:element name="x" type="Base" nillable="true"/>
        "#)).unwrap();
        let xsi = "http://www.w3.org/2001/XMLSchema-instance";
        s.validate_str(&format!(
            r#"<x {ns} xmlns:xsi="{xsi}" xsi:type="Derived" xsi:nil="true"/>"#,
            ns = instance_ns())).unwrap();
    }

    // ── xs:redefine ────────────────────────────────────────────────

    #[test]
    fn redefine_replaces_simple_type_in_included_schema() {
        // included.xsd defines `Code` as xs:string with length=3 — any
        // 3-char string accepted.  outer.xsd redefines `Code` to
        // additionally restrict to the enumeration {"ABC", "XYZ"}.
        // After compilation, references to `Code` in outer.xsd must
        // resolve to the tighter type.
        let included = r#"<?xml version="1.0"?>
            <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                       targetNamespace="urn:test"
                       xmlns="urn:test">
                <xs:simpleType name="Code">
                    <xs:restriction base="xs:string">
                        <xs:length value="3"/>
                    </xs:restriction>
                </xs:simpleType>
            </xs:schema>
        "#;
        let outer = r#"<?xml version="1.0"?>
            <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                       targetNamespace="urn:test"
                       xmlns="urn:test">
                <xs:redefine schemaLocation="included.xsd">
                    <xs:simpleType name="Code">
                        <xs:restriction base="Code">
                            <xs:enumeration value="ABC"/>
                            <xs:enumeration value="XYZ"/>
                        </xs:restriction>
                    </xs:simpleType>
                </xs:redefine>
                <xs:element name="c" type="Code"/>
            </xs:schema>
        "#;
        let resolver = super::super::resolver::InMemoryResolver::new()
            .with("included.xsd", included.as_bytes().to_vec());
        let schema = Schema::compile_with(outer, resolver).unwrap();
        // Allowed values pass.
        schema.validate_str(&format!(r#"<c {}>ABC</c>"#, instance_ns())).unwrap();
        schema.validate_str(&format!(r#"<c {}>XYZ</c>"#, instance_ns())).unwrap();
        // "DEF" passes the included length=3 check but should fail the
        // redefining enumeration.
        let err = schema.validate_str(&format!(r#"<c {}>DEF</c>"#, instance_ns())).unwrap_err();
        assert!(err.issues.iter().any(|i| i.message.contains("enumeration")),
            "expected enumeration failure, got {:?}", err.issues);
        // Too-long input still fails (length=3 from base is inherited).
        let err = schema.validate_str(&format!(r#"<c {}>TOOLONG</c>"#, instance_ns())).unwrap_err();
        assert!(!err.issues.is_empty(),
            "expected validation failure for too-long input, got ok");
    }

    #[test]
    fn redefine_complex_type_extension_adds_fields() {
        // Canonical xs:redefine pattern: the redefining body extends
        // the same-named original.  The composed type validates
        // instances containing the original's fields PLUS the new ones.
        let included = r#"<?xml version="1.0"?>
            <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                       targetNamespace="urn:test"
                       xmlns="urn:test"
                       elementFormDefault="qualified">
                <xs:complexType name="Address">
                    <xs:sequence>
                        <xs:element name="city" type="xs:string"/>
                    </xs:sequence>
                </xs:complexType>
                <xs:element name="addr" type="Address"/>
            </xs:schema>
        "#;
        let outer = r#"<?xml version="1.0"?>
            <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                       xmlns:t="urn:test"
                       targetNamespace="urn:test"
                       xmlns="urn:test"
                       elementFormDefault="qualified">
                <xs:redefine schemaLocation="included.xsd">
                    <xs:complexType name="Address">
                        <xs:complexContent>
                            <xs:extension base="t:Address">
                                <xs:sequence>
                                    <xs:element name="country" type="xs:string"/>
                                </xs:sequence>
                            </xs:extension>
                        </xs:complexContent>
                    </xs:complexType>
                </xs:redefine>
            </xs:schema>
        "#;
        let resolver = super::super::resolver::InMemoryResolver::new()
            .with("included.xsd", included.as_bytes().to_vec());
        let schema = Schema::compile_with(outer, resolver).unwrap();
        // Both original and new fields required.
        schema.validate_str(&format!(
            r#"<addr {ns}><city>SF</city><country>US</country></addr>"#,
            ns = instance_ns(),
        )).unwrap();
        // Missing the new field — fails (the extension made country required).
        let err = schema.validate_str(&format!(
            r#"<addr {ns}><city>SF</city></addr>"#,
            ns = instance_ns(),
        )).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::MissingRequiredElement)
                && i.message.contains("country")
        ), "expected missing-country error, got {:?}", err.issues);
    }

    #[test]
    fn redefine_with_no_body_acts_like_include() {
        // <xs:redefine schemaLocation="..."/> with an empty body must
        // still load the referenced schema.
        let included = r#"<?xml version="1.0"?>
            <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                       targetNamespace="urn:test"
                       xmlns="urn:test">
                <xs:element name="msg" type="xs:string"/>
            </xs:schema>
        "#;
        let outer = r#"<?xml version="1.0"?>
            <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                       targetNamespace="urn:test"
                       xmlns="urn:test">
                <xs:redefine schemaLocation="included.xsd"/>
            </xs:schema>
        "#;
        let resolver = super::super::resolver::InMemoryResolver::new()
            .with("included.xsd", included.as_bytes().to_vec());
        let schema = Schema::compile_with(outer, resolver).unwrap();
        schema.validate_str(&format!(r#"<msg {}>hi</msg>"#, instance_ns())).unwrap();
    }

    // ── restriction with user-defined base ─────────────────────────

    #[test]
    fn restriction_chain_composes_facets_top_down() {
        // A: xs:string with maxLength=20
        // B: A with pattern="[A-Z]+"
        // C: B with enumeration={"FOO","BAR"}
        // Only "FOO" and "BAR" should validate; "ABC" fails enumeration,
        // "FOOOO" fails enumeration AND maxLength wouldn't fire here, and
        // "foo" fails pattern.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:simpleType name="A">
                <xs:restriction base="xs:string">
                    <xs:maxLength value="20"/>
                </xs:restriction>
            </xs:simpleType>
            <xs:simpleType name="B">
                <xs:restriction base="A">
                    <xs:pattern value="[A-Z]+"/>
                </xs:restriction>
            </xs:simpleType>
            <xs:simpleType name="C">
                <xs:restriction base="B">
                    <xs:enumeration value="FOO"/>
                    <xs:enumeration value="BAR"/>
                </xs:restriction>
            </xs:simpleType>
            <xs:element name="v" type="C"/>
        "#)).unwrap();
        // Accept the allowed values.
        s.validate_str(&format!(r#"<v {}>FOO</v>"#, instance_ns())).unwrap();
        s.validate_str(&format!(r#"<v {}>BAR</v>"#, instance_ns())).unwrap();
        // "ABC" fails enumeration (B's pattern accepts it).
        let err = s.validate_str(&format!(r#"<v {}>ABC</v>"#, instance_ns())).unwrap_err();
        assert!(err.issues.iter().any(|i| i.message.contains("enumeration")),
            "expected enumeration failure, got {:?}", err.issues);
        // "foo" fails B's pattern.
        let err = s.validate_str(&format!(r#"<v {}>foo</v>"#, instance_ns())).unwrap_err();
        assert!(err.issues.iter().any(|i| i.message.contains("pattern")),
            "expected pattern failure, got {:?}", err.issues);
    }

    #[test]
    fn restriction_preserves_list_variety_from_base() {
        // After fix: restricting a list type yields a list type.
        // The length facet then counts items, not characters.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:simpleType name="IntList">
                <xs:list itemType="xs:int"/>
            </xs:simpleType>
            <xs:simpleType name="ThreeInts">
                <xs:restriction base="IntList">
                    <xs:length value="3"/>
                </xs:restriction>
            </xs:simpleType>
            <xs:element name="nums" type="ThreeInts"/>
        "#)).unwrap();
        s.validate_str(&format!(r#"<nums {}>1 2 3</nums>"#, instance_ns())).unwrap();
        let err = s.validate_str(&format!(r#"<nums {}>1 2</nums>"#, instance_ns())).unwrap_err();
        assert!(err.issues.iter().any(|i| i.message.contains("2 item(s)")),
            "expected list-length error counting items, got {:?}", err.issues);
        // Items must still be valid ints — restriction inherits item type.
        let err = s.validate_str(&format!(r#"<v {}>1 foo 3</v>"#, instance_ns())).unwrap_err();
        assert!(!err.issues.is_empty(),
            "expected validation failure for non-int item, got ok");
    }

    // ── xsi:type derivation check ──────────────────────────────────

    #[test]
    fn xsi_type_accepts_derived_complex_type() {
        // Address is a base; USAddress extends it.  An element
        // declared as Address may use xsi:type to substitute USAddress,
        // and its content (city from Address + state from extension)
        // must validate under the merged content model.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="Address">
                <xs:sequence>
                    <xs:element name="city" type="xs:string"/>
                </xs:sequence>
            </xs:complexType>
            <xs:complexType name="USAddress">
                <xs:complexContent>
                    <xs:extension base="Address">
                        <xs:sequence>
                            <xs:element name="state" type="xs:string"/>
                        </xs:sequence>
                    </xs:extension>
                </xs:complexContent>
            </xs:complexType>
            <xs:element name="addr" type="Address"/>
        "#)).unwrap();
        let ok = format!(
            r#"<addr xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                   xsi:type="USAddress" {ns}>
                 <city>SF</city>
                 <state>CA</state>
               </addr>"#,
            ns = instance_ns(),
        );
        s.validate_str(&ok).unwrap();
    }

    #[test]
    fn extension_three_level_chain_merges_all_levels() {
        // A -> B (extension) -> C (extension), instance under C must
        // see fields from all three levels.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="A">
                <xs:sequence>
                    <xs:element name="a" type="xs:string"/>
                </xs:sequence>
            </xs:complexType>
            <xs:complexType name="B">
                <xs:complexContent>
                    <xs:extension base="A">
                        <xs:sequence>
                            <xs:element name="b" type="xs:string"/>
                        </xs:sequence>
                    </xs:extension>
                </xs:complexContent>
            </xs:complexType>
            <xs:complexType name="C">
                <xs:complexContent>
                    <xs:extension base="B">
                        <xs:sequence>
                            <xs:element name="c" type="xs:string"/>
                        </xs:sequence>
                    </xs:extension>
                </xs:complexContent>
            </xs:complexType>
            <xs:element name="root" type="C"/>
        "#)).unwrap();
        let ok = format!(
            r#"<root {}>
                 <a>x</a><b>y</b><c>z</c>
               </root>"#,
            instance_ns(),
        );
        s.validate_str(&ok).unwrap();
    }

    #[test]
    fn extension_merges_attributes() {
        // Base contributes a required attribute; extension adds another.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="Tagged">
                <xs:attribute name="id" type="xs:string" use="required"/>
            </xs:complexType>
            <xs:complexType name="TaggedNamed">
                <xs:complexContent>
                    <xs:extension base="Tagged">
                        <xs:attribute name="name" type="xs:string" use="required"/>
                    </xs:extension>
                </xs:complexContent>
            </xs:complexType>
            <xs:element name="item" type="TaggedNamed"/>
        "#)).unwrap();
        s.validate_str(&format!(r#"<item id="x" name="y" {}/>"#, instance_ns())).unwrap();
        // Missing inherited id → error.
        let err = s.validate_str(&format!(r#"<item name="y" {}/>"#, instance_ns())).unwrap_err();
        assert!(err.issues.iter().any(|i| i.message.contains("missing required attribute")
            && i.message.contains("id")
        ), "expected missing-id error, got {:?}", err.issues);
    }

    #[test]
    fn xsi_type_rejects_unrelated_complex_type() {
        // Two unrelated complex types — xsi:type pointing at the
        // other one must fail.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="Address">
                <xs:sequence>
                    <xs:element name="city" type="xs:string"/>
                </xs:sequence>
            </xs:complexType>
            <xs:complexType name="Person">
                <xs:sequence>
                    <xs:element name="name" type="xs:string"/>
                </xs:sequence>
            </xs:complexType>
            <xs:element name="addr" type="Address"/>
        "#)).unwrap();
        let bad = format!(
            r#"<addr xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                   xsi:type="Person" {ns}>
                 <name>alice</name>
               </addr>"#,
            ns = instance_ns(),
        );
        let err = s.validate_str(&bad).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::TypeMismatch)
                && i.message.contains("does not derive from")
        ), "expected derivation-failure error, got {:?}", err.issues);
    }

    #[test]
    fn xsi_type_accepts_identity_no_op() {
        // xsi:type set to the declared type itself is always allowed.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="Address">
                <xs:sequence>
                    <xs:element name="city" type="xs:string"/>
                </xs:sequence>
            </xs:complexType>
            <xs:element name="addr" type="Address"/>
        "#)).unwrap();
        let ok = format!(
            r#"<addr xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                   xsi:type="Address" {ns}>
                 <city>SF</city>
               </addr>"#,
            ns = instance_ns(),
        );
        s.validate_str(&ok).unwrap();
    }

    #[test]
    fn xsi_type_blocked_by_element_block_extension() {
        // `block="extension"` on the element forbids substituting any
        // type that derives by extension.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="Address">
                <xs:sequence>
                    <xs:element name="city" type="xs:string"/>
                </xs:sequence>
            </xs:complexType>
            <xs:complexType name="USAddress">
                <xs:complexContent>
                    <xs:extension base="Address">
                        <xs:sequence>
                            <xs:element name="state" type="xs:string"/>
                        </xs:sequence>
                    </xs:extension>
                </xs:complexContent>
            </xs:complexType>
            <xs:element name="addr" type="Address" block="extension"/>
        "#)).unwrap();
        let bad = format!(
            r#"<addr xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                   xsi:type="USAddress" {ns}>
                 <city>SF</city>
                 <state>CA</state>
               </addr>"#,
            ns = instance_ns(),
        );
        let err = s.validate_str(&bad).unwrap_err();
        assert!(err.issues.iter().any(|i|
            matches!(i.kind, ValidationKind::TypeMismatch)
                && i.message.contains("blocked")
        ), "expected block= rejection, got {:?}", err.issues);
    }

    #[test]
    fn xsi_type_blocked_by_base_type_final_extension() {
        // XSD §3.4.6 (Derivation Valid (Extension), clause 1.1):
        // if the base type's `final` contains `extension`, deriving
        // by extension is a schema component constraint violation
        // — the schema itself is invalid and must not compile.
        // We surface this at compile time (matches libxml2's
        // `cos-st-derived-ok` rejection for the same shape) rather
        // than deferring to instance-time `xsi:type` validation.
        let result = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="Address" final="extension">
                <xs:sequence>
                    <xs:element name="city" type="xs:string"/>
                </xs:sequence>
            </xs:complexType>
            <xs:complexType name="USAddress">
                <xs:complexContent>
                    <xs:extension base="Address">
                        <xs:sequence>
                            <xs:element name="state" type="xs:string"/>
                        </xs:sequence>
                    </xs:extension>
                </xs:complexContent>
            </xs:complexType>
            <xs:element name="addr" type="Address"/>
        "#));
        let err = result.expect_err("schema must not compile");
        assert!(
            err.message.contains("final") && err.message.contains("extension"),
            "expected diagnostic mentioning final/extension, got: {}",
            err.message,
        );
    }

    #[test]
    fn xsi_type_two_level_chain_derivation_check_accepts() {
        // A -> B (extension) -> C (extension).  Declared element is A;
        // xsi:type=C should pass the derivation check (transitively
        // derives by extension).  Instance uses xsi:nil so content
        // validation is skipped — the focus is the derivation walk.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:complexType name="A">
                <xs:sequence>
                    <xs:element name="a" type="xs:string" minOccurs="0"/>
                </xs:sequence>
            </xs:complexType>
            <xs:complexType name="B">
                <xs:complexContent>
                    <xs:extension base="A">
                        <xs:sequence>
                            <xs:element name="b" type="xs:string" minOccurs="0"/>
                        </xs:sequence>
                    </xs:extension>
                </xs:complexContent>
            </xs:complexType>
            <xs:complexType name="C">
                <xs:complexContent>
                    <xs:extension base="B">
                        <xs:sequence>
                            <xs:element name="c" type="xs:string" minOccurs="0"/>
                        </xs:sequence>
                    </xs:extension>
                </xs:complexContent>
            </xs:complexType>
            <xs:element name="root" type="A" nillable="true"/>
        "#)).unwrap();
        // No content — derivation check passes; content validation has
        // nothing to chew on.
        let ok = format!(
            r#"<root xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
                   xsi:type="C" xsi:nil="true" {ns}/>"#,
            ns = instance_ns(),
        );
        s.validate_str(&ok).unwrap();
    }

    // ── xs:list and xs:union ───────────────────────────────────────

    #[test]
    fn xs_list_of_int_validates_each_item() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:simpleType name="IntList">
                <xs:list itemType="xs:int"/>
            </xs:simpleType>
            <xs:element name="nums" type="IntList"/>
        "#)).unwrap();
        s.validate_str(&format!(r#"<nums {}>1 2 3</nums>"#, instance_ns())).unwrap();
        s.validate_str(&format!(r#"<nums {}></nums>"#, instance_ns())).unwrap();
        let err = s.validate_str(&format!(r#"<nums {}>1 foo 3</nums>"#, instance_ns())).unwrap_err();
        assert!(!err.issues.is_empty(),
            "expected at least one issue for 'foo' as int, got {:?}", err.issues);
    }

    #[test]
    fn xs_list_length_facet_counts_items_not_chars() {
        // length=3 on a list means exactly 3 items, not 3 characters.
        // (Schema written with an explicit named base so we don't depend
        // on the implicit-base nested-simpleType form.)
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:simpleType name="IntList">
                <xs:list itemType="xs:int"/>
            </xs:simpleType>
            <xs:simpleType name="ThreeInts">
                <xs:restriction base="IntList">
                    <xs:length value="3"/>
                </xs:restriction>
            </xs:simpleType>
            <xs:element name="nums" type="ThreeInts"/>
        "#)).unwrap();
        s.validate_str(&format!(r#"<nums {}>1 2 3</nums>"#, instance_ns())).unwrap();
        let too_few = s.validate_str(&format!(r#"<nums {}>1 2</nums>"#, instance_ns())).unwrap_err();
        assert!(too_few.issues.iter().any(|i| i.message.contains("length")),
            "expected length-facet error for 2 items, got {:?}", too_few.issues);
        let too_many = s.validate_str(&format!(r#"<nums {}>1 2 3 4</nums>"#, instance_ns())).unwrap_err();
        assert!(too_many.issues.iter().any(|i| i.message.contains("length")),
            "expected length-facet error for 4 items, got {:?}", too_many.issues);
    }

    #[test]
    fn xs_union_accepts_any_member_type() {
        // Union of int + date: both shapes accepted, nonsense rejected.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:simpleType name="IntOrDate">
                <xs:union memberTypes="xs:int xs:date"/>
            </xs:simpleType>
            <xs:element name="val" type="IntOrDate"/>
        "#)).unwrap();
        s.validate_str(&format!(r#"<val {}>42</val>"#, instance_ns())).unwrap();
        s.validate_str(&format!(r#"<val {}>2026-05-16</val>"#, instance_ns())).unwrap();
        let err = s.validate_str(&format!(r#"<val {}>nonsense</val>"#, instance_ns())).unwrap_err();
        assert!(!err.issues.is_empty(),
            "expected union-failure, got {:?}", err.issues);
    }

    #[test]
    fn xs_list_facet_length_unit_test() {
        // Direct unit test on SimpleType with Variety::List, avoiding
        // the schema-restriction path so the length-facet logic in
        // SimpleType::validate is exercised in isolation.
        use super::super::types::{SimpleType, Variety};
        use super::super::facets::{Facet, FacetSet};
        let mut list_facets = FacetSet::default();
        list_facets.push(Facet::Length(3));
        let three_ints = SimpleType {
            name: Some("ThreeInts".into()),
            builtin: BuiltinType::String,
            facets: list_facets,
            whitespace: super::super::whitespace::WhitespaceMode::Collapse,
            variety: Variety::List {
                item_type: Arc::new(SimpleType::of_builtin(BuiltinType::Int)),
            },
            final_: super::super::schema::BlockSet::default(),
            assertions: Vec::new(),
        };
        three_ints.validate("1 2 3").unwrap();
        let err = three_ints.validate("1 2").unwrap_err();
        assert!(err.message.contains("length") && err.message.contains("2 item(s)"),
            "expected list length error mentioning item count, got {}", err.message);
        let err = three_ints.validate("1 2 3 4").unwrap_err();
        assert!(err.message.contains("length") && err.message.contains("4 item(s)"),
            "expected list length error mentioning item count, got {}", err.message);
        // Item-type failure surfaces as a list-item error.
        let err = three_ints.validate("1 foo 3").unwrap_err();
        assert!(err.message.contains("list item #2"),
            "expected list-item error citing position, got {}", err.message);
    }

    #[test]
    fn xs_union_with_nested_simpletypes() {
        // Anonymous members via nested xs:simpleType.
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:simpleType name="SmallOrBig">
                <xs:union>
                    <xs:simpleType>
                        <xs:restriction base="xs:int">
                            <xs:maxInclusive value="10"/>
                        </xs:restriction>
                    </xs:simpleType>
                    <xs:simpleType>
                        <xs:restriction base="xs:int">
                            <xs:minInclusive value="1000"/>
                        </xs:restriction>
                    </xs:simpleType>
                </xs:union>
            </xs:simpleType>
            <xs:element name="n" type="SmallOrBig"/>
        "#)).unwrap();
        s.validate_str(&format!(r#"<n {}>5</n>"#, instance_ns())).unwrap();
        s.validate_str(&format!(r#"<n {}>2000</n>"#, instance_ns())).unwrap();
        let err = s.validate_str(&format!(r#"<n {}>500</n>"#, instance_ns())).unwrap_err();
        assert!(!err.issues.is_empty(),
            "expected union-failure for 500 (between 10 and 1000), got {:?}", err.issues);
    }

    #[test]
    fn issue_line_for_duplicate_key_points_at_constraint_declaring_element() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="users">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="user" maxOccurs="unbounded">
                            <xs:complexType>
                                <xs:attribute name="id" type="xs:string" use="required"/>
                            </xs:complexType>
                        </xs:element>
                    </xs:sequence>
                </xs:complexType>
                <xs:key name="userKey">
                    <xs:selector xpath=".//user"/>
                    <xs:field xpath="@id"/>
                </xs:key>
            </xs:element>
        "#)).unwrap();
        // <users> on line 3 (after 2 leading newlines).
        let bad = format!(
            "\n\n<users {}>\n  <user id=\"A\"/>\n  <user id=\"A\"/>\n</users>",
            instance_ns()
        );
        let err = s.validate_str(&bad).unwrap_err();
        let issue = err.issues.iter()
            .find(|i| matches!(i.kind, ValidationKind::KeyNotUnique))
            .expect("expected KeyNotUnique error");
        assert_eq!(issue.line, Some(3),
            "expected <users> (declaring element) on line 3, got {issue:?}");
    }

    #[test]
    fn validate_doc_typed_records_governing_types() {
        let s = Schema::compile_str(&xsd_str(r#"
            <xs:element name="root">
                <xs:complexType>
                    <xs:sequence>
                        <xs:element name="count" type="xs:integer"/>
                        <xs:element name="label" type="xs:string"/>
                    </xs:sequence>
                </xs:complexType>
            </xs:element>
        "#)).unwrap();
        let mut opts = crate::ParseOptions::default();
        opts.namespace_aware = true;
        let doc = crate::parse_str(
            &format!(r#"<root {}><count>3</count><label>hi</label></root>"#, instance_ns()),
            &opts,
        ).unwrap();
        let (res, psvi) = s.validate_doc_typed(&doc);
        assert!(res.is_ok(), "expected valid doc, got {res:?}");
        assert!(!psvi.is_empty(), "expected recorded type annotations");

        // Walk to the two leaf elements and check their recorded
        // primitive types.
        let root = doc.root();
        assert!(psvi.governing_type(root).is_some(), "root should be typed");
        for child in root.children().filter(|n|
            matches!(n.kind, sup_xml_tree::dom::NodeKind::Element))
        {
            let ty = psvi.governing_type(child)
                .unwrap_or_else(|| panic!("{} should be typed", child.name()));
            let TypeRef::Simple(st) = ty else { panic!("expected simple type") };
            match child.name() {
                "count" => assert_eq!(st.builtin, BuiltinType::Integer),
                "label" => assert_eq!(st.builtin, BuiltinType::String),
                other   => panic!("unexpected child {other}"),
            }
        }
    }
}
