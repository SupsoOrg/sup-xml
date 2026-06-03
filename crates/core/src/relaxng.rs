#![forbid(unsafe_code)]

//! RelaxNG validation against the arena DOM.
//!
//! Arena-backed counterpart to [`crate::relaxng`].  Same semantics — Brzozowski
//! derivative algorithm, same supported pattern subset, same datatype handling —
//! but operates on [`sup_xml_tree::dom::Document`] / [`Node`].
//!
//! Public surface:
//!
//! - [`parse_schema`] — compiles an XML-syntax RelaxNG schema into an
//!   [`RngSchema`].  Same return type as the legacy [`crate::relaxng::parse_schema`],
//!   so callers can mix-and-match the compiler with either validator.
//! - [`validate`] — validates an arena [`Document`] against a compiled
//!   [`RngSchema`].
//!
//! `RngSchema`, [`Pattern`], [`NameClass`], and [`RELAXNG_NS`] live in this
//! module — the pattern AST is DOM-agnostic.

use std::collections::HashMap;
use std::sync::Arc;

use sup_xml_tree::dom::{Document, Node, NodeKind};

use crate::parser::parse_str;
use crate::error::{ErrorDomain, ErrorLevel, Result, XmlError};
use crate::options::ParseOptions;

/// RelaxNG namespace URI.  Constant per spec § 1.
pub const RELAXNG_NS: &str = "http://relaxng.org/ns/structure/1.0";

// ── pattern AST ──────────────────────────────────────────────────────────────

/// A RelaxNG pattern.  See module docs for what's supported.
///
/// Patterns are wrapped in `Arc` so derivative-style transformations
/// can share subtrees cheaply across alternatives.
#[derive(Debug, Clone)]
pub enum Pattern {
    /// `<empty/>` — matches the empty content sequence.
    Empty,
    /// `<notAllowed/>` — matches nothing.
    NotAllowed,
    /// `<text/>` — matches any sequence of text.
    Text,
    /// `<value type=...>X</value>` — text content must equal `X`
    /// after whitespace normalisation per the datatype.
    Value {
        datatype: String,
        text: String,
    },
    /// `<data type="..."><param name=...>...</param>...</data>` —
    /// text content must validate against the named datatype.
    Data {
        datatype: String,
        params: Vec<(String, String)>,
    },
    /// `<list>` — whitespace-tokenize the text, match each token
    /// against the inner pattern.
    List(Arc<Pattern>),
    /// `<element name="X">child</element>`.
    Element {
        name: NameClass,
        child: Arc<Pattern>,
    },
    /// `<attribute name="X">child</attribute>`.
    Attribute {
        name: NameClass,
        child: Arc<Pattern>,
    },
    /// `<group>` — sequential composition.
    Group(Arc<Pattern>, Arc<Pattern>),
    /// `<interleave>` — both patterns must match, in any order.
    Interleave(Arc<Pattern>, Arc<Pattern>),
    /// `<choice>` — match either alternative.
    Choice(Arc<Pattern>, Arc<Pattern>),
    /// `<oneOrMore>` — match `inner` one or more times.
    OneOrMore(Arc<Pattern>),
    /// `<ref name="X"/>`.
    Ref(String),
    /// Internal: a pattern that matches a fixed remaining set of
    /// children, produced by derivatives.  Equivalent to a
    /// successfully-matched-so-far state.
    After(Arc<Pattern>, Arc<Pattern>),
}

/// A RelaxNG name class — a predicate over `(namespace_uri,
/// local_name)` pairs.
#[derive(Debug, Clone)]
pub enum NameClass {
    /// `<name ns="X">local</name>` — exactly this name.
    Name {
        namespace: String,
        local: String,
    },
    /// `<anyName/>` (optionally with `<except>...</except>`).
    AnyName(Option<Box<NameClass>>),
    /// `<nsName ns="X"/>` — any local name in the given namespace.
    NsName {
        namespace: String,
        except: Option<Box<NameClass>>,
    },
    /// `<choice>` of name classes.
    Choice(Box<NameClass>, Box<NameClass>),
    /// Internal: matches nothing.
    Nothing,
}

impl NameClass {
    #[allow(dead_code)]
    fn describe(&self) -> String {
        match self {
            NameClass::Name { namespace, local } => {
                if namespace.is_empty() {
                    local.clone()
                } else {
                    format!("{{{namespace}}}{local}")
                }
            }
            NameClass::AnyName(_) => "*".into(),
            NameClass::NsName { namespace, .. } => format!("{{{namespace}}}*"),
            NameClass::Choice(a, b) => format!("{} | {}", a.describe(), b.describe()),
            NameClass::Nothing => "<nothing>".into(),
        }
    }
}

/// A compiled RelaxNG schema.
#[derive(Debug, Clone)]
pub struct RngSchema {
    pub start: Arc<Pattern>,
    pub defines: HashMap<String, Arc<Pattern>>,
}

// ── smart constructors (keep derivatives compact) ────────────────────────────
//
// These mirror the private helpers in `crate::relaxng`.  Duplicated rather
// than re-exported because the legacy module keeps them private; behaviour is
// byte-identical.

fn p_empty() -> Arc<Pattern> { Arc::new(Pattern::Empty) }
fn p_not_allowed() -> Arc<Pattern> { Arc::new(Pattern::NotAllowed) }

fn choice(a: Arc<Pattern>, b: Arc<Pattern>) -> Arc<Pattern> {
    match (&*a, &*b) {
        (Pattern::NotAllowed, _) => b,
        (_, Pattern::NotAllowed) => a,
        _ => Arc::new(Pattern::Choice(a, b)),
    }
}

fn group(a: Arc<Pattern>, b: Arc<Pattern>) -> Arc<Pattern> {
    match (&*a, &*b) {
        (Pattern::NotAllowed, _) | (_, Pattern::NotAllowed) => p_not_allowed(),
        (Pattern::Empty, _) => b,
        (_, Pattern::Empty) => a,
        _ => Arc::new(Pattern::Group(a, b)),
    }
}

fn interleave(a: Arc<Pattern>, b: Arc<Pattern>) -> Arc<Pattern> {
    match (&*a, &*b) {
        (Pattern::NotAllowed, _) | (_, Pattern::NotAllowed) => p_not_allowed(),
        (Pattern::Empty, _) => b,
        (_, Pattern::Empty) => a,
        _ => Arc::new(Pattern::Interleave(a, b)),
    }
}

fn one_or_more(p: Arc<Pattern>) -> Arc<Pattern> {
    match &*p {
        Pattern::NotAllowed => p_not_allowed(),
        Pattern::Empty => p_empty(),
        _ => Arc::new(Pattern::OneOrMore(p)),
    }
}

fn after(a: Arc<Pattern>, b: Arc<Pattern>) -> Arc<Pattern> {
    match &*a {
        Pattern::NotAllowed => p_not_allowed(),
        _ => Arc::new(Pattern::After(a, b)),
    }
}

// ── nullability ──────────────────────────────────────────────────────────────

/// True iff `p` can match the empty content sequence.
fn nullable(p: &Pattern, defs: &HashMap<String, Arc<Pattern>>) -> bool {
    match p {
        Pattern::Empty | Pattern::Text => true,
        Pattern::NotAllowed
        | Pattern::Value { .. }
        | Pattern::Data { .. }
        | Pattern::List(_)
        | Pattern::Element { .. }
        | Pattern::Attribute { .. }
        | Pattern::After(_, _) => false,
        Pattern::Group(a, b) | Pattern::Interleave(a, b) => {
            nullable(a, defs) && nullable(b, defs)
        }
        Pattern::Choice(a, b) => nullable(a, defs) || nullable(b, defs),
        Pattern::OneOrMore(inner) => nullable(inner, defs),
        Pattern::Ref(name) => match defs.get(name) {
            Some(target) => nullable(target, defs),
            None => false,
        },
    }
}

/// Helper to discriminate name-class matches; `NameClass::matches` is private
/// in the legacy module, so we reimplement it here against the same enum.
fn name_class_matches(nc: &NameClass, ns: &str, local: &str) -> bool {
    match nc {
        NameClass::Name { namespace, local: l } => namespace == ns && l == local,
        NameClass::AnyName(except) => {
            except.as_ref().is_none_or(|e| !name_class_matches(e, ns, local))
        }
        NameClass::NsName { namespace, except } => {
            namespace == ns
                && except.as_ref().is_none_or(|e| !name_class_matches(e, ns, local))
        }
        NameClass::Choice(a, b) => name_class_matches(a, ns, local)
            || name_class_matches(b, ns, local),
        NameClass::Nothing => false,
    }
}

// ── derivatives ──────────────────────────────────────────────────────────────

/// Derivative of `p` with respect to a child element with name `(ns, local)`,
/// attributes `atts`, and content `children`.  Returns the residual pattern
/// that the rest of the parent's content must match.
fn child_deriv<'a>(
    p: &Pattern,
    ns: &str,
    local: &str,
    atts: &[(String, String, String)],
    children: &[&'a Node<'a>],
    defs: &HashMap<String, Arc<Pattern>>,
) -> Arc<Pattern> {
    match p {
        Pattern::Element { name, child } => {
            if !name_class_matches(name, ns, local) {
                return p_not_allowed();
            }
            // First validate attributes against the inner pattern.
            let after_atts = consume_attributes(child.clone(), atts, defs);
            if matches!(&*after_atts, Pattern::NotAllowed) {
                return p_not_allowed();
            }
            // Then validate child content (text + elements) against the residual.
            let after_content = consume_content(after_atts, children, defs);
            if nullable(&after_content, defs) {
                p_empty()
            } else {
                p_not_allowed()
            }
        }
        Pattern::Choice(a, b) => choice(
            child_deriv(a, ns, local, atts, children, defs),
            child_deriv(b, ns, local, atts, children, defs),
        ),
        Pattern::Group(a, b) => {
            let d_a = child_deriv(a, ns, local, atts, children, defs);
            let in_a = group(d_a, b.clone());
            if nullable(a, defs) {
                let d_b = child_deriv(b, ns, local, atts, children, defs);
                choice(in_a, d_b)
            } else {
                in_a
            }
        }
        Pattern::Interleave(a, b) => {
            let d_a = child_deriv(a, ns, local, atts, children, defs);
            let d_b = child_deriv(b, ns, local, atts, children, defs);
            choice(interleave(d_a, b.clone()), interleave(a.clone(), d_b))
        }
        Pattern::OneOrMore(inner) => {
            let d = child_deriv(inner, ns, local, atts, children, defs);
            group(
                d,
                choice(p_empty(), Arc::new(Pattern::OneOrMore(inner.clone()))),
            )
        }
        Pattern::After(a, b) => after(
            child_deriv(a, ns, local, atts, children, defs),
            b.clone(),
        ),
        Pattern::Ref(name) => match defs.get(name) {
            Some(target) => child_deriv(target, ns, local, atts, children, defs),
            None => p_not_allowed(),
        },
        _ => p_not_allowed(),
    }
}

/// Derivative of `p` with respect to a text run.
fn text_deriv(p: &Pattern, text: &str, defs: &HashMap<String, Arc<Pattern>>) -> Arc<Pattern> {
    match p {
        Pattern::Text => p_empty(),
        Pattern::Value { datatype, text: expected } => {
            if value_matches(datatype, text, expected) {
                p_empty()
            } else {
                p_not_allowed()
            }
        }
        Pattern::Data { datatype, params } => {
            if data_matches(datatype, params, text) {
                #[cfg(feature = "xsd")]
                record_id_value(datatype, text);
                p_empty()
            } else {
                p_not_allowed()
            }
        }
        Pattern::List(inner) => {
            let tokens: Vec<&str> = text.split_whitespace().collect();
            let mut current = inner.clone();
            for t in &tokens {
                current = text_deriv(&current, t, defs);
                if matches!(&*current, Pattern::NotAllowed) {
                    return p_not_allowed();
                }
            }
            if nullable(&current, defs) {
                p_empty()
            } else {
                p_not_allowed()
            }
        }
        Pattern::Choice(a, b) => choice(
            text_deriv(a, text, defs),
            text_deriv(b, text, defs),
        ),
        Pattern::Group(a, b) => {
            let d_a = text_deriv(a, text, defs);
            let in_a = group(d_a, b.clone());
            if nullable(a, defs) {
                let d_b = text_deriv(b, text, defs);
                choice(in_a, d_b)
            } else {
                in_a
            }
        }
        Pattern::Interleave(a, b) => {
            let d_a = text_deriv(a, text, defs);
            let d_b = text_deriv(b, text, defs);
            choice(interleave(d_a, b.clone()), interleave(a.clone(), d_b))
        }
        Pattern::OneOrMore(inner) => {
            let d = text_deriv(inner, text, defs);
            group(
                d,
                choice(p_empty(), Arc::new(Pattern::OneOrMore(inner.clone()))),
            )
        }
        Pattern::After(a, b) => after(text_deriv(a, text, defs), b.clone()),
        Pattern::Ref(name) => match defs.get(name) {
            Some(target) => text_deriv(target, text, defs),
            None => p_not_allowed(),
        },
        Pattern::Empty => {
            if text.trim().is_empty() {
                p_empty()
            } else {
                p_not_allowed()
            }
        }
        _ => p_not_allowed(),
    }
}

/// Consume an attribute against `p`.  Returns the residual pattern.
fn att_deriv(
    p: &Pattern,
    ns: &str,
    local: &str,
    value: &str,
    defs: &HashMap<String, Arc<Pattern>>,
) -> Arc<Pattern> {
    match p {
        Pattern::Attribute { name, child } => {
            if !name_class_matches(name, ns, local) {
                return p_not_allowed();
            }
            let after_text = text_deriv(child, value, defs);
            if nullable(&after_text, defs) {
                p_empty()
            } else {
                p_not_allowed()
            }
        }
        Pattern::Choice(a, b) => choice(
            att_deriv(a, ns, local, value, defs),
            att_deriv(b, ns, local, value, defs),
        ),
        Pattern::Group(a, b) => {
            let d_a = att_deriv(a, ns, local, value, defs);
            let in_a = group(d_a, b.clone());
            let d_b = att_deriv(b, ns, local, value, defs);
            let in_b = group(a.clone(), d_b);
            choice(in_a, in_b)
        }
        Pattern::Interleave(a, b) => {
            let d_a = att_deriv(a, ns, local, value, defs);
            let d_b = att_deriv(b, ns, local, value, defs);
            choice(interleave(d_a, b.clone()), interleave(a.clone(), d_b))
        }
        Pattern::OneOrMore(inner) => {
            let d = att_deriv(inner, ns, local, value, defs);
            group(
                d,
                choice(p_empty(), Arc::new(Pattern::OneOrMore(inner.clone()))),
            )
        }
        Pattern::After(a, b) => after(att_deriv(a, ns, local, value, defs), b.clone()),
        Pattern::Ref(name) => match defs.get(name) {
            Some(target) => att_deriv(target, ns, local, value, defs),
            None => p_not_allowed(),
        },
        _ => p_not_allowed(),
    }
}

/// Validate the content (text + child elements) of an element against the
/// residual pattern returned after attribute matching.
fn consume_content<'a>(
    p: Arc<Pattern>,
    children: &[&'a Node<'a>],
    defs: &HashMap<String, Arc<Pattern>>,
) -> Arc<Pattern> {
    let mut current = p;
    for c in children {
        if matches!(&*current, Pattern::NotAllowed) {
            return p_not_allowed();
        }
        current = match c.kind {
            NodeKind::Element => {
                let (ns, local) = name_split(c);
                let atts = collect_attributes(c);
                let child_refs: Vec<&Node<'_>> = c
                    .children()
                    .filter(|n| {
                        !(n.kind == NodeKind::Text
                            && n.content().trim().is_empty())
                    })
                    .collect();
                child_deriv(&current, &ns, local, &atts, &child_refs, defs)
            }
            NodeKind::Text => {
                let txt = c.content();
                if txt.trim().is_empty() {
                    // Whitespace-only text between elements is tolerated
                    // whether the pattern accepts text or not.
                    current
                } else {
                    text_deriv(&current, txt, defs)
                }
            }
            NodeKind::CData => text_deriv(&current, c.content(), defs),
            // Comments and PIs are ignored by RelaxNG validation.
            _ => current,
        };
    }
    current
}

/// Validate an element's attributes against `p`.  Loops over the element's
/// attributes (skipping `xmlns*`) and applies `att_deriv` for each.
fn consume_attributes(
    p: Arc<Pattern>,
    atts: &[(String, String, String)],
    defs: &HashMap<String, Arc<Pattern>>,
) -> Arc<Pattern> {
    let mut current = p;
    for (ns, local, value) in atts {
        if matches!(&*current, Pattern::NotAllowed) {
            return p_not_allowed();
        }
        current = att_deriv(&current, ns, local, value, defs);
    }
    current
}

// ── helpers for navigating the arena tree ────────────────────────────────────

/// Split an element's qualified name into `(namespace_uri, local)`.  The
/// namespace URI comes from the element's resolved `namespace` binding (when
/// the parser was run with `namespace_aware: true`); otherwise it's the empty
/// string and we fall back to local-name-only matching.
fn name_split<'a>(elem: &'a Node<'a>) -> (String, &'a str) {
    debug_assert!(elem.kind == NodeKind::Element, "name_split on non-element");
    let name: &str = elem.name();
    let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name);
    let ns = elem
        .namespace
        .get()
        .map(|n| n.href().to_string())
        .unwrap_or_default();
    (ns, local)
}

/// RelaxNG simplification (§ 4.1, "Annotations"): an element from a
/// namespace other than the RelaxNG namespace is a foreign annotation
/// and is removed before grammar processing — a RelaxNG schema "can be
/// mixed freely with stuff from other namespaces" (e.g. embedded ISO
/// Schematron `<sch:*>` rules).  An element with no resolved namespace
/// is treated as RelaxNG so the non-namespace-aware fallback still
/// parses bare schemas.
fn is_foreign_element<'a>(elem: &'a Node<'a>) -> bool {
    let (ns, _) = name_split(elem);
    !ns.is_empty() && ns != RELAXNG_NS
}

/// Collect `(namespace, local, value)` for each non-`xmlns*` attribute.
fn collect_attributes<'a>(elem: &'a Node<'a>) -> Vec<(String, String, String)> {
    debug_assert!(elem.kind == NodeKind::Element, "collect_attributes on non-element");
    elem.attributes()
        .filter_map(|a| {
            let aname: &str = a.name();
            if aname == "xmlns" || aname.starts_with("xmlns:") {
                return None;
            }
            let local = aname.rsplit_once(':').map(|(_, l)| l).unwrap_or(aname);
            let ns = a.namespace.get().map(|n| n.href().to_string()).unwrap_or_default();
            Some((ns, local.to_string(), a.value().to_string()))
        })
        .collect()
}

// ── value / data validation ──────────────────────────────────────────────────

fn value_matches(datatype: &str, actual: &str, expected: &str) -> bool {
    if datatype == "token" || datatype == "NMTOKEN" {
        let a_norm = actual.split_whitespace().collect::<Vec<_>>().join(" ");
        let e_norm = expected.split_whitespace().collect::<Vec<_>>().join(" ");
        a_norm == e_norm
    } else {
        actual == expected
    }
}

fn data_matches(datatype: &str, _params: &[(String, String)], text: &str) -> bool {
    #[cfg(feature = "xsd")]
    {
        if let Some(checker) = xsd_datatype_checker(datatype) {
            return checker(text);
        }
    }
    let _ = datatype;
    !text.is_empty() || datatype == "string"
}

// ── ID / IDREF semantic cross-reference checking ─────────────────────────────
//
// RELAX NG's structural (derivative) validation only checks that an
// `<data type="ID"/>` / `IDREF` value is a syntactically valid NCName; the
// *semantic* rule — every IDREF must reference a declared ID — is a
// separate pass.  The derivative engine is pure, so we accumulate the
// values into thread-local state during the walk and resolve them at the
// end, exactly as libxml2 does.  The collector is only armed when the
// schema actually uses IDREF/IDREFS, so every other schema is untouched.

#[cfg(feature = "xsd")]
#[derive(Default)]
struct IdRefState {
    ids:    std::collections::HashSet<String>,
    idrefs: Vec<String>,
}

#[cfg(feature = "xsd")]
thread_local! {
    static ID_REFS: std::cell::RefCell<Option<IdRefState>> =
        const { std::cell::RefCell::new(None) };
}

/// Record a value matched against an `ID`/`IDREF`/`IDREFS` datatype, when
/// the collector is armed.  No-op otherwise.  Over-collection from a failed
/// `<choice>` branch is possible (and matches libxml2) but rare.
#[cfg(feature = "xsd")]
fn record_id_value(datatype: &str, text: &str) {
    ID_REFS.with(|c| {
        if let Some(st) = c.borrow_mut().as_mut() {
            match datatype {
                "ID"     => { st.ids.insert(text.trim().to_string()); }
                "IDREF"  => st.idrefs.push(text.trim().to_string()),
                "IDREFS" => st.idrefs.extend(text.split_whitespace().map(str::to_string)),
                _ => {}
            }
        }
    });
}

/// Whether any pattern in the schema declares an `IDREF`/`IDREFS` datatype
/// (so resolution must be checked).  `ID`-only schemas need no cross-check.
#[cfg(feature = "xsd")]
fn schema_uses_idref(schema: &RngSchema) -> bool {
    fn walk(p: &Pattern) -> bool {
        match p {
            Pattern::Data { datatype, .. } => matches!(datatype.as_str(), "IDREF" | "IDREFS"),
            Pattern::List(a) | Pattern::OneOrMore(a) => walk(a),
            Pattern::Element { child, .. } | Pattern::Attribute { child, .. } => walk(child),
            Pattern::Group(a, b) | Pattern::Interleave(a, b)
            | Pattern::Choice(a, b) | Pattern::After(a, b) => walk(a) || walk(b),
            _ => false,
        }
    }
    walk(schema.start.as_ref()) || schema.defines.values().any(|p| walk(p.as_ref()))
}

/// Arm or disarm the collector for a validation run.
#[cfg(feature = "xsd")]
fn set_idref_tracking(on: bool) {
    ID_REFS.with(|c| *c.borrow_mut() = on.then(IdRefState::default));
}

/// Take the collector and return the first IDREF that resolves to no ID,
/// or `None` when every reference resolves (or tracking was off).
#[cfg(feature = "xsd")]
fn first_unresolved_idref() -> Option<String> {
    ID_REFS.with(|c| {
        c.borrow_mut().take().and_then(|st| {
            st.idrefs.iter().find(|r| !st.ids.contains(*r)).cloned()
        })
    })
}

#[cfg(feature = "xsd")]
fn xsd_datatype_checker(datatype: &str) -> Option<fn(&str) -> bool> {
    match datatype {
        "string" | "token" | "normalizedString" | "Name" | "NCName" | "NMTOKEN" | "QName"
        | "ID" | "IDREF" | "IDREFS" | "ENTITY" | "ENTITIES" | "language" => {
            Some(|s| !s.is_empty())
        }
        "boolean" => Some(|s| matches!(s.trim(), "true" | "false" | "1" | "0")),
        "decimal" | "double" | "float" => Some(|s| s.trim().parse::<f64>().is_ok()),
        "integer" | "int" | "long" | "short" | "byte" => {
            Some(|s| s.trim().parse::<i64>().is_ok())
        }
        "positiveInteger" => Some(|s| s.trim().parse::<i64>().is_ok_and(|n| n > 0)),
        "nonNegativeInteger" | "unsignedInt" | "unsignedLong" | "unsignedShort"
        | "unsignedByte" => Some(|s| s.trim().parse::<u64>().is_ok()),
        "negativeInteger" => Some(|s| s.trim().parse::<i64>().is_ok_and(|n| n < 0)),
        "nonPositiveInteger" => Some(|s| s.trim().parse::<i64>().is_ok_and(|n| n <= 0)),
        "anyURI" => Some(|s| !s.is_empty()),
        "date" => Some(|s| s.len() >= 10 && s[..10].chars().filter(|c| *c == '-').count() == 2),
        _ => None,
    }
}

// ── schema parser ────────────────────────────────────────────────────────────

/// Parse a RelaxNG XML schema into an [`RngSchema`].  Schemas are parsed in
/// namespace-aware mode so name-class `ns` attribute inheritance works
/// correctly.
pub fn parse_schema(source: &str) -> Result<RngSchema> {
    parse_schema_with_base(source, None)
}

/// Parse a RELAX NG schema, resolving `<include href="…">` against
/// `base` (the schema document's URL).  `parse_schema` is the
/// no-base form.
pub fn parse_schema_with_base(source: &str, base: Option<&str>) -> Result<RngSchema> {
    let opts = ParseOptions {
        namespace_aware: true,
        ..ParseOptions::default()
    };
    let doc = parse_str(source, &opts)?;
    parse_schema_doc(&doc, base)
}

fn parse_schema_doc(doc: &Document, base: Option<&str>) -> Result<RngSchema> {
    let root = doc.root();
    if root.kind != NodeKind::Element {
        return Err(schema_err("schema root must be an element"));
    }
    let mut ctx = SchemaCtx {
        defines: HashMap::new(),
        define_combines: HashMap::new(),
        start_combine: None,
        default_ns: String::new(),
        base: base.map(str::to_string),
    };
    if let Some(ns) = attr(root, "ns") {
        ctx.default_ns = ns.to_string();
    }

    // The schema's root must be in the RELAX NG namespace (§ 4.1); a
    // root from another namespace isn't a RELAX NG pattern at all.  An
    // unresolved (empty) namespace is tolerated for the
    // non-namespace-aware fallback path.
    let (root_ns, local) = name_split(root);
    if !root_ns.is_empty() && root_ns != RELAXNG_NS {
        return Err(schema_err(format!(
            "schema root <{}> is not in the RELAX NG namespace",
            root.name()
        )));
    }
    let start = match local {
        "grammar" => parse_grammar(root, &mut ctx)?,
        _ => parse_pattern_element(root, &ctx)?,
    };

    Ok(RngSchema { start, defines: ctx.defines })
}

struct SchemaCtx {
    defines: HashMap<String, Arc<Pattern>>,
    define_combines: HashMap<String, Option<CombineKind>>,
    start_combine: Option<CombineKind>,
    default_ns: String,
    /// Base URI for resolving `<include href="…">`, threaded from the
    /// schema document's URL (or the including file when nested).
    base: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CombineKind {
    Choice,
    Interleave,
}

fn parse_combine_attr<'a>(elem: &'a Node<'a>, ctx_label: &str) -> Result<Option<CombineKind>> {
    match attr(elem, "combine") {
        None => Ok(None),
        Some("choice") => Ok(Some(CombineKind::Choice)),
        Some("interleave") => Ok(Some(CombineKind::Interleave)),
        Some(other) => Err(schema_err(format!(
            "invalid combine={other:?} on {ctx_label} — must be \"choice\" or \"interleave\""
        ))),
    }
}

fn merge_with_combine(
    name: &str,
    existing: Arc<Pattern>,
    new_pat: Arc<Pattern>,
    prev_combine: Option<CombineKind>,
    new_combine: Option<CombineKind>,
    label: &str,
) -> Result<(Arc<Pattern>, CombineKind)> {
    let effective = match (prev_combine, new_combine) {
        (Some(a), Some(b)) if a == b => a,
        (Some(a), Some(b)) => {
            return Err(schema_err(format!(
                "inconsistent combine values for {label} {name:?}: previously {a:?}, now {b:?}"
            )));
        }
        (None, Some(b)) => b,
        (Some(a), None) => a,
        (None, None) => {
            return Err(schema_err(format!(
                "duplicate {label} {name:?} requires a combine attribute (\"choice\" or \"interleave\") \
                 on at least the second occurrence — RelaxNG spec § 4.17"
            )));
        }
    };
    let merged = match effective {
        CombineKind::Choice => choice(existing, new_pat),
        CombineKind::Interleave => interleave(existing, new_pat),
    };
    Ok((merged, effective))
}

fn parse_grammar<'a>(elem: &'a Node<'a>, ctx: &mut SchemaCtx) -> Result<Arc<Pattern>> {
    if let Some(ns) = attr(elem, "ns") {
        ctx.default_ns = ns.to_string();
    }
    let mut start: Option<Arc<Pattern>> = None;
    for child in elem.children() {
        if child.kind != NodeKind::Element || is_foreign_element(child) {
            continue;
        }
        match local_name(child.name()) {
            "start" => handle_start(child, ctx, &mut start)?,
            "define" => handle_define(child, ctx)?,
            "div" => parse_grammar_div(child, ctx, &mut start)?,
            "include" => handle_include(child, ctx, &mut start)?,
            other => {
                return Err(schema_err(format!(
                    "unsupported grammar child <{other}>"
                )));
            }
        }
    }
    start.ok_or_else(|| schema_err("<grammar> requires a <start>"))
}

/// Process `<include href="…">` (RELAX NG § 4.7): load the referenced
/// grammar and fold its `<start>`/`<define>`/`<div>` into the including
/// grammar.  A `<define>`/`<start>` *inside* the `<include>` overrides
/// (replaces) the same-named component from the included grammar.
fn handle_include<'a>(
    elem:  &'a Node<'a>,
    ctx:   &mut SchemaCtx,
    start: &mut Option<Arc<Pattern>>,
) -> Result<()> {
    let href = attr(elem, "href")
        .ok_or_else(|| schema_err("<include> requires an href attribute"))?;
    let resolved = crate::resolve_uri(href, ctx.base.as_deref());
    let path = resolved.strip_prefix("file://").unwrap_or(&resolved);
    let src = std::fs::read_to_string(path)
        .map_err(|e| schema_err(format!("<include href=\"{href}\">: {e}")))?;
    let opts = ParseOptions { namespace_aware: true, ..ParseOptions::default() };
    let inc_doc = parse_str(&src, &opts)?;
    let inc_root = inc_doc.root();
    if local_name(inc_root.name()) != "grammar" {
        return Err(schema_err("<include> href must point to a <grammar>"));
    }

    // Components redefined inside the <include> override the included
    // ones; apply them first and remember their names so the included
    // versions are skipped.
    let mut overridden: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut override_start = false;
    for ovc in elem.children() {
        if ovc.kind != NodeKind::Element || is_foreign_element(ovc) {
            continue;
        }
        match local_name(ovc.name()) {
            "define" => {
                if let Some(n) = attr(ovc, "name") {
                    overridden.insert(n.to_string());
                }
                handle_define(ovc, ctx)?;
            }
            "start" => {
                override_start = true;
                handle_start(ovc, ctx, start)?;
            }
            _ => {}
        }
    }

    // The included grammar resolves its own nested includes against its
    // own location.
    let saved_base = ctx.base.take();
    ctx.base = Some(resolved.clone());
    if let Some(ns) = attr(inc_root, "ns") {
        // A `ns` on the included grammar applies only within it; we don't
        // currently track per-grammar default namespaces, so leave the
        // ambient one in place (the common case has matching/no `ns`).
        let _ = ns;
    }
    for child in inc_root.children() {
        if child.kind != NodeKind::Element || is_foreign_element(child) {
            continue;
        }
        match local_name(child.name()) {
            "start" if !override_start => handle_start(child, ctx, start)?,
            "start" => {}
            "define" => {
                if attr(child, "name").map(|n| !overridden.contains(n)).unwrap_or(true) {
                    handle_define(child, ctx)?;
                }
            }
            "div" => parse_grammar_div(child, ctx, start)?,
            "include" => handle_include(child, ctx, start)?,
            _ => {}
        }
    }
    ctx.base = saved_base;
    Ok(())
}

fn parse_grammar_div<'a>(
    elem: &'a Node<'a>,
    ctx: &mut SchemaCtx,
    start: &mut Option<Arc<Pattern>>,
) -> Result<()> {
    for child in elem.children() {
        if child.kind != NodeKind::Element || is_foreign_element(child) {
            continue;
        }
        match local_name(child.name()) {
            "start" => handle_start(child, ctx, start)?,
            "define" => handle_define(child, ctx)?,
            "div" => parse_grammar_div(child, ctx, start)?,
            _ => {}
        }
    }
    Ok(())
}

fn handle_start<'a>(
    e: &'a Node<'a>,
    ctx: &mut SchemaCtx,
    start: &mut Option<Arc<Pattern>>,
) -> Result<()> {
    let new_combine = parse_combine_attr(e, "<start>")?;
    let new_pat = combine_seq(parse_pattern_children(e, ctx)?);
    match start.take() {
        None => {
            *start = Some(new_pat);
            ctx.start_combine = new_combine;
        }
        Some(existing) => {
            let prev = ctx.start_combine;
            let (merged, effective) =
                merge_with_combine("<start>", existing, new_pat, prev, new_combine, "<start>")?;
            *start = Some(merged);
            ctx.start_combine = Some(effective);
        }
    }
    Ok(())
}

fn handle_define<'a>(e: &'a Node<'a>, ctx: &mut SchemaCtx) -> Result<()> {
    let name = attr(e, "name")
        .ok_or_else(|| schema_err("<define> missing name attribute"))?
        .to_string();
    let new_combine = parse_combine_attr(e, &format!("<define name={name:?}/>"))?;
    let new_pat = combine_seq(parse_pattern_children(e, ctx)?);
    match ctx.defines.get(&name).cloned() {
        None => {
            ctx.defines.insert(name.clone(), new_pat);
            ctx.define_combines.insert(name, new_combine);
        }
        Some(existing) => {
            let prev_combine = ctx.define_combines.get(&name).copied().flatten();
            let (merged, effective) = merge_with_combine(
                &name,
                existing,
                new_pat,
                prev_combine,
                new_combine,
                "<define>",
            )?;
            ctx.defines.insert(name.clone(), merged);
            ctx.define_combines.insert(name, Some(effective));
        }
    }
    Ok(())
}

fn parse_pattern_element<'a>(elem: &'a Node<'a>, ctx: &SchemaCtx) -> Result<Arc<Pattern>> {
    debug_assert!(elem.kind == NodeKind::Element, "parse_pattern_element on non-element");
    match local_name(elem.name()) {
        "element" => {
            let nc = parse_name_class(elem, ctx, false)?;
            let skip_first_child = attr(elem, "name").is_none();
            let pats = parse_content_patterns(elem, ctx, skip_first_child)?;
            // An `<element>` pattern requires a content pattern (§ 4.13);
            // `<element name="b"/>` with nothing inside is a malformed
            // schema, not an empty-content element.
            if pats.is_empty() {
                return Err(schema_err("<element> pattern has no content pattern"));
            }
            let child = combine_seq(pats);
            Ok(Arc::new(Pattern::Element { name: nc, child }))
        }
        "attribute" => {
            let nc = parse_name_class(elem, ctx, true)?;
            let skip_first_child = attr(elem, "name").is_none();
            let pats = parse_content_patterns(elem, ctx, skip_first_child)?;
            let child = if pats.is_empty() {
                Arc::new(Pattern::Text)
            } else {
                combine_seq(pats)
            };
            Ok(Arc::new(Pattern::Attribute { name: nc, child }))
        }
        "text" => Ok(Arc::new(Pattern::Text)),
        "empty" => Ok(p_empty()),
        "notAllowed" => Ok(p_not_allowed()),
        "value" => {
            let datatype = attr(elem, "type").unwrap_or("token").to_string();
            let text = elem.text_content().unwrap_or("").to_string();
            Ok(Arc::new(Pattern::Value { datatype, text }))
        }
        "data" => {
            let datatype = attr(elem, "type").unwrap_or("string").to_string();
            let params: Vec<(String, String)> = elem
                .children()
                .filter_map(|c| {
                    if c.kind == NodeKind::Element && local_name(c.name()) == "param" {
                        let name = attr(c, "name")?.to_string();
                        let value = c.text_content().unwrap_or("").to_string();
                        return Some((name, value));
                    }
                    None
                })
                .collect();
            Ok(Arc::new(Pattern::Data { datatype, params }))
        }
        "list" => {
            let inner = combine_seq(parse_pattern_children(elem, ctx)?);
            Ok(Arc::new(Pattern::List(inner)))
        }
        "group" => Ok(combine_seq(parse_pattern_children(elem, ctx)?)),
        "choice" => {
            let pats = parse_pattern_children(elem, ctx)?;
            Ok(combine_choice(pats))
        }
        "interleave" => {
            let pats = parse_pattern_children(elem, ctx)?;
            Ok(combine_interleave(pats))
        }
        "mixed" => {
            let inner = combine_seq(parse_pattern_children(elem, ctx)?);
            Ok(interleave(Arc::new(Pattern::Text), inner))
        }
        "optional" => {
            let p = combine_seq(parse_pattern_children(elem, ctx)?);
            Ok(choice(p_empty(), p))
        }
        "zeroOrMore" => {
            let p = combine_seq(parse_pattern_children(elem, ctx)?);
            Ok(choice(p_empty(), one_or_more(p)))
        }
        "oneOrMore" => {
            let p = combine_seq(parse_pattern_children(elem, ctx)?);
            Ok(one_or_more(p))
        }
        "ref" => {
            let name = attr(elem, "name")
                .ok_or_else(|| schema_err("<ref> missing name attribute"))?
                .to_string();
            Ok(Arc::new(Pattern::Ref(name)))
        }
        other => Err(schema_err(format!(
            "unsupported pattern <{other}> — see module docs for the v1 subset"
        ))),
    }
}

fn parse_pattern_children<'a>(elem: &'a Node<'a>, ctx: &SchemaCtx) -> Result<Vec<Arc<Pattern>>> {
    parse_content_patterns(elem, ctx, false)
}

/// Walk an element's children, parsing each as a content pattern.  When
/// `skip_first_child` is true the first element child is assumed to be a name
/// class (already consumed by the caller) and is skipped.
fn parse_content_patterns<'a>(
    elem: &'a Node<'a>,
    ctx: &SchemaCtx,
    skip_first_child: bool,
) -> Result<Vec<Arc<Pattern>>> {
    let mut out = Vec::new();
    let mut skipped = !skip_first_child;
    for child in elem.children() {
        if child.kind != NodeKind::Element || is_foreign_element(child) {
            continue;
        }
        if !skipped {
            skipped = true;
            continue;
        }
        let local = local_name(child.name());
        if matches!(local, "name" | "anyName" | "nsName") {
            continue;
        }
        out.push(parse_pattern_element(child, ctx)?);
    }
    Ok(out)
}

/// Resolve a RELAX NG name-class QName to `(namespace, local)`.
///
/// Per the RELAX NG spec § 4.10 / § 7: a prefixed name resolves the
/// prefix against the in-scope namespace declarations of the schema
/// document; an *unprefixed* name uses the inherited `ns` value for an
/// **element**, but the **empty** namespace for an **attribute** (XML
/// attributes are not in the default namespace).  Getting the
/// attribute case wrong makes every named attribute in an `ns`-scoped
/// grammar (e.g. ISO Schematron's `test` / `context`) fail to match.
fn resolve_name_class_qname(
    elem: &Node<'_>,
    name: &str,
    default_ns: &str,
    is_attribute: bool,
) -> (String, String) {
    match name.split_once(':') {
        Some((prefix, local)) => {
            let ns = resolve_schema_prefix(elem, prefix)
                .unwrap_or_else(|| default_ns.to_string());
            (ns, local.to_string())
        }
        None => {
            let ns = if is_attribute { String::new() } else { default_ns.to_string() };
            (ns, name.to_string())
        }
    }
}

/// Resolve a namespace prefix used in a name-class QName, walking the
/// schema element and its ancestors for the matching `xmlns:` decl.
/// `xml` is predefined (XML 1.0 § 3.7).
fn resolve_schema_prefix(elem: &Node<'_>, prefix: &str) -> Option<String> {
    if prefix == "xml" {
        return Some("http://www.w3.org/XML/1998/namespace".to_string());
    }
    let mut cur = Some(elem);
    while let Some(e) = cur {
        for (p, href) in e.ns_declarations() {
            if p == Some(prefix) {
                return Some(href.to_string());
            }
        }
        cur = e.parent.get();
    }
    None
}

/// Parse the name class of an `<element>` or `<attribute>`.
fn parse_name_class<'a>(elem: &'a Node<'a>, ctx: &SchemaCtx, is_attribute: bool) -> Result<NameClass> {
    let elem_default_ns = attr(elem, "ns")
        .map(|s| s.to_string())
        .unwrap_or_else(|| ctx.default_ns.clone());

    if let Some(name) = attr(elem, "name") {
        let (namespace, local) =
            resolve_name_class_qname(elem, name, &elem_default_ns, is_attribute);
        return Ok(NameClass::Name { namespace, local });
    }

    for child in elem.children() {
        if child.kind != NodeKind::Element {
            continue;
        }
        if let Some(nc) = parse_name_class_element(child, &elem_default_ns, is_attribute)? {
            return Ok(nc);
        }
    }
    Err(schema_err(format!(
        "<{}> needs a name attribute or child <name>/<anyName>/<nsName>/<choice>",
        local_name(elem.name())
    )))
}

fn parse_name_class_element<'a>(
    elem: &'a Node<'a>,
    default_ns: &str,
    is_attribute: bool,
) -> Result<Option<NameClass>> {
    match local_name(elem.name()) {
        "name" => {
            let raw = elem.text_content().unwrap_or("").trim().to_string();
            // An explicit `ns` on the <name> wins; otherwise resolve the
            // QName the same way as the `name` attribute form.
            let (namespace, local) = match attr(elem, "ns") {
                Some(ns) => (ns.to_string(), raw),
                None => resolve_name_class_qname(elem, &raw, default_ns, is_attribute),
            };
            Ok(Some(NameClass::Name { namespace, local }))
        }
        "anyName" => {
            let except = parse_except_name_class(elem, default_ns, is_attribute)?;
            Ok(Some(NameClass::AnyName(except.map(Box::new))))
        }
        "nsName" => {
            let ns = attr(elem, "ns")
                .map(|s| s.to_string())
                .unwrap_or_else(|| default_ns.to_string());
            let except = parse_except_name_class(elem, default_ns, is_attribute)?;
            Ok(Some(NameClass::NsName {
                namespace: ns,
                except: except.map(Box::new),
            }))
        }
        "choice" => {
            let mut alts = Vec::new();
            for child in elem.children() {
                if child.kind != NodeKind::Element {
                    continue;
                }
                if let Some(nc) = parse_name_class_element(child, default_ns, is_attribute)? {
                    alts.push(nc);
                }
            }
            Ok(Some(combine_name_choice(alts)))
        }
        _ => Ok(None),
    }
}

fn parse_except_name_class<'a>(
    elem: &'a Node<'a>,
    default_ns: &str,
    is_attribute: bool,
) -> Result<Option<NameClass>> {
    for child in elem.children() {
        if child.kind != NodeKind::Element {
            continue;
        }
        if local_name(child.name()) != "except" {
            continue;
        }
        let mut alts = Vec::new();
        for ec in child.children() {
            if ec.kind != NodeKind::Element {
                continue;
            }
            if let Some(nc) = parse_name_class_element(ec, default_ns, is_attribute)? {
                alts.push(nc);
            }
        }
        if alts.is_empty() {
            return Err(schema_err("<except> needs at least one name class"));
        }
        return Ok(Some(combine_name_choice(alts)));
    }
    Ok(None)
}

fn combine_name_choice(alts: Vec<NameClass>) -> NameClass {
    if alts.is_empty() {
        return NameClass::Nothing;
    }
    let mut iter = alts.into_iter();
    let mut acc = iter.next().unwrap();
    for a in iter {
        acc = NameClass::Choice(Box::new(acc), Box::new(a));
    }
    acc
}

fn combine_seq(mut pats: Vec<Arc<Pattern>>) -> Arc<Pattern> {
    match pats.len() {
        0 => p_empty(),
        1 => pats.remove(0),
        _ => {
            let mut iter = pats.into_iter();
            let mut acc = iter.next().unwrap();
            for p in iter {
                acc = group(acc, p);
            }
            acc
        }
    }
}

fn combine_choice(mut pats: Vec<Arc<Pattern>>) -> Arc<Pattern> {
    match pats.len() {
        0 => p_not_allowed(),
        1 => pats.remove(0),
        _ => {
            let mut iter = pats.into_iter();
            let mut acc = iter.next().unwrap();
            for p in iter {
                acc = choice(acc, p);
            }
            acc
        }
    }
}

fn combine_interleave(mut pats: Vec<Arc<Pattern>>) -> Arc<Pattern> {
    match pats.len() {
        0 => p_empty(),
        1 => pats.remove(0),
        _ => {
            let mut iter = pats.into_iter();
            let mut acc = iter.next().unwrap();
            for p in iter {
                acc = interleave(acc, p);
            }
            acc
        }
    }
}

fn attr<'a>(elem: &'a Node<'a>, name: &str) -> Option<&'a str> {
    elem.attributes().find_map(|a| {
        if a.name() == name {
            Some(a.value())
        } else {
            None
        }
    })
}

fn local_name(name: &str) -> &str {
    name.rsplit_once(':').map(|(_, l)| l).unwrap_or(name)
}

// ── public validation ────────────────────────────────────────────────────────

/// Validate `doc` against `schema`.
pub fn validate(schema: &RngSchema, doc: &Document) -> Result<()> {
    let root = doc.root();
    if root.kind != NodeKind::Element {
        return Err(validation_err("document root is not an element"));
    }
    let (ns, local) = name_split(root);
    let atts = collect_attributes(root);
    let children: Vec<&Node<'_>> = root
        .children()
        .filter(|n| {
            !(n.kind == NodeKind::Text && n.content().trim().is_empty())
        })
        .collect();
    // Arm ID/IDREF collection for the walk when the schema uses references.
    #[cfg(feature = "xsd")]
    let track_idrefs = schema_uses_idref(schema);
    #[cfg(feature = "xsd")]
    set_idref_tracking(track_idrefs);
    let result = child_deriv(
        &schema.start,
        &ns,
        local,
        &atts,
        &children,
        &schema.defines,
    );
    if nullable(&result, &schema.defines) {
        // Structural validation passed — now resolve IDREFs against the
        // IDs collected during the walk (RELAX NG's semantic constraint).
        #[cfg(feature = "xsd")]
        if track_idrefs {
            if let Some(missing) = first_unresolved_idref() {
                return Err(validation_err(format!(
                    "IDREF value \"{missing}\" does not reference a declared ID"
                )));
            }
        }
        Ok(())
    } else {
        #[cfg(feature = "xsd")]
        set_idref_tracking(false);
        // Produce libxml2's "Did not expect element X there"
        // (`RELAXNG_ERR_ELEMWRONG`, with the element's line) when the
        // failure is an unexpected child element; fall back to a generic
        // message otherwise.
        let located = element_content_for(&schema.start, &ns, local, &schema.defines)
            .map(|content| consume_attributes(content, &atts, &schema.defines))
            .and_then(|after_atts| locate_unexpected_child(after_atts, &children, &schema.defines));
        match located {
            Some((name, line)) => {
                let mut e = validation_err(format!("Did not expect element {name} there"));
                e.code = crate::error::ErrorCode::RelaxngErrElemwrong;
                e.line = Some(line);
                Err(e)
            }
            None => Err(validation_err(format!(
                "document root <{}> did not match the schema",
                root.name()
            ))),
        }
    }
}

/// Resolve the content pattern of the `<element>` pattern that matches
/// `(ns, local)`, looking through `ref`/`choice`.  Used only to build a
/// located validation error message.
fn element_content_for(
    p:     &Pattern,
    ns:    &str,
    local: &str,
    defs:  &HashMap<String, Arc<Pattern>>,
) -> Option<Arc<Pattern>> {
    match p {
        Pattern::Element { name, child } if name_class_matches(name, ns, local) => {
            Some(child.clone())
        }
        Pattern::Ref(n) => defs.get(n).and_then(|d| element_content_for(d, ns, local, defs)),
        Pattern::Choice(a, b) => element_content_for(a, ns, local, defs)
            .or_else(|| element_content_for(b, ns, local, defs)),
        _ => None,
    }
}

/// Walk an element's content pattern against its children (as the
/// validator does) and return the local name of the first child element
/// the pattern rejects — the one libxml2 reports as "not expected".
fn locate_unexpected_child<'a>(
    content:  Arc<Pattern>,
    children: &[&'a Node<'a>],
    defs:     &HashMap<String, Arc<Pattern>>,
) -> Option<(String, u32)> {
    let mut current = content;
    for c in children {
        match c.kind {
            NodeKind::Element => {
                let (cns, cl) = name_split(c);
                let catts = collect_attributes(c);
                let cchildren: Vec<&Node<'_>> = c
                    .children()
                    .filter(|n| !(n.kind == NodeKind::Text && n.content().trim().is_empty()))
                    .collect();
                let next = child_deriv(&current, &cns, cl, &catts, &cchildren, defs);
                if matches!(&*next, Pattern::NotAllowed) {
                    return Some((cl.to_string(), c.line as u32));
                }
                current = next;
            }
            NodeKind::Text if !c.content().trim().is_empty() => {
                current = text_deriv(&current, c.content(), defs);
            }
            NodeKind::CData => current = text_deriv(&current, c.content(), defs),
            _ => {}
        }
    }
    None
}

// ── error helpers ────────────────────────────────────────────────────────────

fn schema_err(msg: impl Into<String>) -> XmlError {
    XmlError::new(ErrorDomain::Validation, ErrorLevel::Fatal, msg)
}

fn validation_err(msg: impl Into<String>) -> XmlError {
    // libxml2 reports RELAX NG *validation* failures in the `RELAXNGV`
    // domain at ERROR level (schema *parse* failures are fatal); lxml
    // filters `error_log` by `level_name == "ERROR"` and `domain_name`.
    XmlError::new(ErrorDomain::RelaxNGValidate, ErrorLevel::Error, msg)
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(s: &str) -> RngSchema {
        parse_schema(s).expect("schema parses")
    }

    fn doc(s: &str) -> Document {
        // Validators use namespace-aware parsing so that `<nsName>` filtering
        // can compare resolved namespace URIs, not raw qualified names.
        let opts = ParseOptions {
            namespace_aware: true,
            ..ParseOptions::default()
        };
        parse_str(s, &opts).expect("doc parses")
    }

    #[test]
    fn simple_element_with_text_validates() {
        let s = schema(
            r#"<element name="greeting" xmlns="http://relaxng.org/ns/structure/1.0">
                 <text/>
               </element>"#,
        );
        validate(&s, &doc("<greeting>hello</greeting>")).unwrap();
    }

    #[test]
    fn wrong_element_name_rejected() {
        let s = schema(
            r#"<element name="greeting" xmlns="http://relaxng.org/ns/structure/1.0"><text/></element>"#,
        );
        let err = validate(&s, &doc("<farewell>bye</farewell>")).expect_err("wrong name");
        assert!(err.message.contains("did not match"), "got: {}", err.message);
    }

    #[test]
    fn attribute_with_value_validates() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <attribute name="kind"><value>small</value></attribute>
               </element>"#,
        );
        validate(&s, &doc(r#"<r kind="small"/>"#)).unwrap();
        assert!(validate(&s, &doc(r#"<r kind="huge"/>"#)).is_err());
    }

    #[test]
    fn missing_required_attribute_rejected() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <attribute name="id"><text/></attribute>
               </element>"#,
        );
        assert!(validate(&s, &doc("<r/>")).is_err());
    }

    #[test]
    fn group_of_elements_validates_in_order() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <group>
                   <element name="a"><text/></element>
                   <element name="b"><text/></element>
                 </group>
               </element>"#,
        );
        validate(&s, &doc("<r><a>1</a><b>2</b></r>")).unwrap();
        assert!(
            validate(&s, &doc("<r><b>2</b><a>1</a></r>")).is_err(),
            "wrong order should fail"
        );
    }

    #[test]
    fn choice_picks_matching_alternative() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <choice>
                   <element name="a"><text/></element>
                   <element name="b"><text/></element>
                 </choice>
               </element>"#,
        );
        validate(&s, &doc("<r><a>1</a></r>")).unwrap();
        validate(&s, &doc("<r><b>2</b></r>")).unwrap();
        assert!(validate(&s, &doc("<r><c>3</c></r>")).is_err());
    }

    #[test]
    fn zero_or_more_matches_any_count() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <zeroOrMore><element name="item"><text/></element></zeroOrMore>
               </element>"#,
        );
        validate(&s, &doc("<r/>")).unwrap();
        validate(&s, &doc("<r><item>1</item></r>")).unwrap();
        validate(
            &s,
            &doc("<r><item>1</item><item>2</item><item>3</item></r>"),
        )
        .unwrap();
    }

    #[test]
    fn one_or_more_requires_at_least_one() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <oneOrMore><element name="item"><text/></element></oneOrMore>
               </element>"#,
        );
        validate(&s, &doc("<r><item>1</item></r>")).unwrap();
        assert!(validate(&s, &doc("<r/>")).is_err());
    }

    #[test]
    fn optional_is_zero_or_one() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <group>
                   <element name="req"><text/></element>
                   <optional><element name="opt"><text/></element></optional>
                 </group>
               </element>"#,
        );
        validate(&s, &doc("<r><req>x</req></r>")).unwrap();
        validate(&s, &doc("<r><req>x</req><opt>y</opt></r>")).unwrap();
    }

    #[test]
    fn grammar_with_define_and_ref() {
        let s = schema(
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start>
                   <element name="root"><ref name="payload"/></element>
                 </start>
                 <define name="payload">
                   <oneOrMore><element name="item"><text/></element></oneOrMore>
                 </define>
               </grammar>"#,
        );
        validate(&s, &doc("<root><item>a</item><item>b</item></root>")).unwrap();
    }

    #[test]
    fn empty_pattern_requires_no_content() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0"><empty/></element>"#,
        );
        validate(&s, &doc("<r/>")).unwrap();
        assert!(validate(&s, &doc("<r><x/></r>")).is_err());
    }

    #[test]
    fn interleave_accepts_any_order() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <interleave>
                   <element name="a"><text/></element>
                   <element name="b"><text/></element>
                 </interleave>
               </element>"#,
        );
        validate(&s, &doc("<r><a>1</a><b>2</b></r>")).unwrap();
        validate(&s, &doc("<r><b>2</b><a>1</a></r>")).unwrap();
        assert!(validate(&s, &doc("<r><a>1</a></r>")).is_err());
        assert!(validate(&s, &doc("<r><a>1</a><a>2</a></r>")).is_err());
    }

    #[test]
    fn interleave_with_zero_or_more() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <interleave>
                   <element name="head"><text/></element>
                   <zeroOrMore><element name="item"><text/></element></zeroOrMore>
                 </interleave>
               </element>"#,
        );
        validate(&s, &doc("<r><head>h</head></r>")).unwrap();
        validate(
            &s,
            &doc("<r><item>1</item><head>h</head><item>2</item></r>"),
        )
        .unwrap();
        validate(
            &s,
            &doc("<r><head>h</head><item>1</item><item>2</item></r>"),
        )
        .unwrap();
    }

    #[test]
    fn mixed_allows_text_interleaved_with_elements() {
        let s = schema(
            r#"<element name="p" xmlns="http://relaxng.org/ns/structure/1.0">
                 <mixed>
                   <zeroOrMore><element name="b"><text/></element></zeroOrMore>
                 </mixed>
               </element>"#,
        );
        validate(&s, &doc("<p>hello <b>world</b></p>")).unwrap();
        validate(&s, &doc("<p>plain text</p>")).unwrap();
        validate(&s, &doc("<p><b>x</b></p>")).unwrap();
    }

    #[test]
    fn data_string_accepts_any_text() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <data type="string"/>
               </element>"#,
        );
        validate(&s, &doc("<r>any text</r>")).unwrap();
    }

    #[cfg(feature = "xsd")]
    #[test]
    fn data_positive_integer_validates() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <data type="positiveInteger"/>
               </element>"#,
        );
        validate(&s, &doc("<r>42</r>")).unwrap();
        validate(&s, &doc("<r>1</r>")).unwrap();
        assert!(validate(&s, &doc("<r>0</r>")).is_err(), "0 is not positive");
        assert!(validate(&s, &doc("<r>-5</r>")).is_err(), "negative");
        assert!(validate(&s, &doc("<r>abc</r>")).is_err(), "not integer");
    }

    #[cfg(feature = "xsd")]
    #[test]
    fn data_boolean_in_attribute() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <attribute name="enabled"><data type="boolean"/></attribute>
               </element>"#,
        );
        validate(&s, &doc(r#"<r enabled="true"/>"#)).unwrap();
        validate(&s, &doc(r#"<r enabled="false"/>"#)).unwrap();
        assert!(validate(&s, &doc(r#"<r enabled="maybe"/>"#)).is_err());
    }

    #[test]
    fn list_of_tokens() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <list>
                   <oneOrMore><value>x</value></oneOrMore>
                 </list>
               </element>"#,
        );
        validate(&s, &doc("<r>x x x</r>")).unwrap();
        validate(&s, &doc("<r>x</r>")).unwrap();
        assert!(validate(&s, &doc("<r>x y</r>")).is_err(), "y is not x");
    }

    #[test]
    fn any_name_matches_any_element() {
        let s = schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="root">
                 <oneOrMore>
                   <element><anyName/><text/></element>
                 </oneOrMore>
               </element>"#,
        );
        validate(&s, &doc("<root><a>1</a><b>2</b><z>3</z></root>")).unwrap();
    }

    #[test]
    fn ns_name_filters_by_namespace() {
        let s = schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="root">
                 <oneOrMore>
                   <element><nsName ns="urn:custom"/><text/></element>
                 </oneOrMore>
               </element>"#,
        );
        // The arena parser resolves namespaces automatically when invoked with
        // `namespace_aware: true` — no separate resolve pass needed.
        validate(&s, &doc(r#"<root><x xmlns="urn:custom">1</x></root>"#)).unwrap();
        assert!(validate(&s, &doc(r#"<root><x xmlns="urn:other">1</x></root>"#)).is_err());
    }

    #[test]
    fn name_class_choice() {
        let s = schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="root">
                 <oneOrMore>
                   <element>
                     <choice><name>a</name><name>b</name></choice>
                     <text/>
                   </element>
                 </oneOrMore>
               </element>"#,
        );
        validate(&s, &doc("<root><a>1</a><b>2</b></root>")).unwrap();
        assert!(validate(&s, &doc("<root><c>1</c></root>")).is_err());
    }

    #[test]
    fn nested_choice_resolves_via_derivative() {
        let s = schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="r">
                 <choice>
                   <group>
                     <element name="a"><text/></element>
                     <element name="b"><text/></element>
                   </group>
                   <group>
                     <element name="a"><text/></element>
                     <element name="c"><text/></element>
                   </group>
                 </choice>
               </element>"#,
        );
        validate(&s, &doc("<r><a>1</a><b>2</b></r>")).unwrap();
        validate(&s, &doc("<r><a>1</a><c>3</c></r>")).unwrap();
        assert!(validate(&s, &doc("<r><a>1</a><d>4</d></r>")).is_err());
    }

    #[test]
    fn deeply_recursive_grammar() {
        let s = schema(
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start>
                   <element name="tree"><ref name="branches"/></element>
                 </start>
                 <define name="branches">
                   <zeroOrMore>
                     <element name="branch"><ref name="branches"/></element>
                   </zeroOrMore>
                 </define>
               </grammar>"#,
        );
        validate(&s, &doc("<tree/>")).unwrap();
        validate(&s, &doc("<tree><branch/></tree>")).unwrap();
        validate(
            &s,
            &doc("<tree><branch><branch><branch/></branch></branch></tree>"),
        )
        .unwrap();
    }

    #[test]
    fn unsupported_pattern_errors_at_schema_parse() {
        let result = parse_schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="r">
                 <unknownPattern/>
               </element>"#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn empty_attribute_value_via_data() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <attribute name="x"><data type="string"/></attribute>
               </element>"#,
        );
        validate(&s, &doc(r#"<r x="value"/>"#)).unwrap();
    }

    // ── combine attribute (modular schema composition) ───────────────────────

    #[test]
    fn combine_choice_merges_two_defines() {
        let s = schema(
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start>
                   <element name="root"><ref name="content"/></element>
                 </start>
                 <define name="content" combine="choice">
                   <element name="a"><text/></element>
                 </define>
                 <define name="content" combine="choice">
                   <element name="b"><text/></element>
                 </define>
               </grammar>"#,
        );
        validate(&s, &doc("<root><a>1</a></root>")).unwrap();
        validate(&s, &doc("<root><b>2</b></root>")).unwrap();
        assert!(
            validate(&s, &doc("<root><c>3</c></root>")).is_err(),
            "<c> not in either define"
        );
    }

    #[test]
    fn combine_choice_merges_three_defines() {
        let s = schema(
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start>
                   <element name="root"><ref name="content"/></element>
                 </start>
                 <define name="content" combine="choice">
                   <element name="a"><text/></element>
                 </define>
                 <define name="content" combine="choice">
                   <element name="b"><text/></element>
                 </define>
                 <define name="content" combine="choice">
                   <element name="c"><text/></element>
                 </define>
               </grammar>"#,
        );
        validate(&s, &doc("<root><a>1</a></root>")).unwrap();
        validate(&s, &doc("<root><b>2</b></root>")).unwrap();
        validate(&s, &doc("<root><c>3</c></root>")).unwrap();
        assert!(validate(&s, &doc("<root><d>4</d></root>")).is_err());
    }

    #[test]
    fn combine_interleave_merges_two_defines() {
        let s = schema(
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start>
                   <element name="root"><ref name="content"/></element>
                 </start>
                 <define name="content" combine="interleave">
                   <element name="head"><text/></element>
                 </define>
                 <define name="content" combine="interleave">
                   <element name="body"><text/></element>
                 </define>
               </grammar>"#,
        );
        validate(&s, &doc("<root><head>h</head><body>b</body></root>")).unwrap();
        validate(&s, &doc("<root><body>b</body><head>h</head></root>")).unwrap();
        assert!(validate(&s, &doc("<root><head>h</head></root>")).is_err());
        assert!(validate(&s, &doc("<root><body>b</body></root>")).is_err());
    }

    #[test]
    fn combine_one_bare_define_plus_one_with_combine() {
        let s = schema(
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start>
                   <element name="root"><ref name="content"/></element>
                 </start>
                 <define name="content">
                   <element name="a"><text/></element>
                 </define>
                 <define name="content" combine="choice">
                   <element name="b"><text/></element>
                 </define>
               </grammar>"#,
        );
        validate(&s, &doc("<root><a>1</a></root>")).unwrap();
        validate(&s, &doc("<root><b>2</b></root>")).unwrap();
    }

    #[test]
    fn combine_on_start_element() {
        let s = schema(
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start combine="choice">
                   <element name="a"><text/></element>
                 </start>
                 <start combine="choice">
                   <element name="b"><text/></element>
                 </start>
               </grammar>"#,
        );
        validate(&s, &doc("<a>1</a>")).unwrap();
        validate(&s, &doc("<b>2</b>")).unwrap();
    }

    #[test]
    fn combine_duplicate_define_without_combine_errors() {
        let result = parse_schema(
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start><element name="r"><ref name="x"/></element></start>
                 <define name="x"><element name="a"><text/></element></define>
                 <define name="x"><element name="b"><text/></element></define>
               </grammar>"#,
        );
        assert!(
            result.is_err(),
            "duplicate define with no combine should be rejected"
        );
        let err = result.unwrap_err();
        assert!(
            err.message.contains("combine"),
            "error should mention combine; got: {}",
            err.message
        );
    }

    #[test]
    fn combine_inconsistent_values_error() {
        let result = parse_schema(
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start><element name="r"><ref name="x"/></element></start>
                 <define name="x" combine="choice">
                   <element name="a"><text/></element>
                 </define>
                 <define name="x" combine="interleave">
                   <element name="b"><text/></element>
                 </define>
               </grammar>"#,
        );
        assert!(
            result.is_err(),
            "inconsistent combine values must be rejected"
        );
        let err = result.unwrap_err();
        assert!(
            err.message.contains("combine") || err.message.contains("inconsistent"),
            "error should mention combine; got: {}",
            err.message
        );
    }

    #[test]
    fn combine_invalid_value_errors() {
        let result = parse_schema(
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start><element name="r"><ref name="x"/></element></start>
                 <define name="x" combine="merge">
                   <element name="a"><text/></element>
                 </define>
                 <define name="x" combine="merge">
                   <element name="b"><text/></element>
                 </define>
               </grammar>"#,
        );
        assert!(result.is_err(), "invalid combine value must be rejected");
    }

    #[test]
    fn combine_single_define_with_combine_attribute() {
        let s = schema(
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start>
                   <element name="root"><ref name="content"/></element>
                 </start>
                 <define name="content" combine="choice">
                   <element name="a"><text/></element>
                 </define>
               </grammar>"#,
        );
        validate(&s, &doc("<root><a>1</a></root>")).unwrap();
    }

    // ── NameClass::describe ─────────────────────────────────────

    #[test]
    fn name_class_describe_all_variants() {
        let n1 = NameClass::Name {
            namespace: String::new(),
            local: "elt".into(),
        };
        assert_eq!(n1.describe(), "elt");

        let n2 = NameClass::Name {
            namespace: "urn:x".into(),
            local: "elt".into(),
        };
        assert_eq!(n2.describe(), "{urn:x}elt");

        assert_eq!(NameClass::AnyName(None).describe(), "*");

        let n3 = NameClass::NsName {
            namespace: "urn:y".into(),
            except: None,
        };
        assert_eq!(n3.describe(), "{urn:y}*");

        let n4 = NameClass::Choice(
            Box::new(NameClass::Name { namespace: String::new(), local: "a".into() }),
            Box::new(NameClass::Name { namespace: String::new(), local: "b".into() }),
        );
        assert_eq!(n4.describe(), "a | b");

        assert_eq!(NameClass::Nothing.describe(), "<nothing>");
    }

    // ── attribute derivative paths: Choice / Group / Interleave / OneOrMore ──

    #[test]
    fn attribute_choice_of_two_attrs() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <choice>
                   <attribute name="x"><text/></attribute>
                   <attribute name="y"><text/></attribute>
                 </choice>
               </element>"#,
        );
        validate(&s, &doc(r#"<r x="1"/>"#)).unwrap();
        validate(&s, &doc(r#"<r y="2"/>"#)).unwrap();
        assert!(validate(&s, &doc(r#"<r z="3"/>"#)).is_err());
    }

    #[test]
    fn attribute_group_requires_both() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <group>
                   <attribute name="x"><text/></attribute>
                   <attribute name="y"><text/></attribute>
                 </group>
               </element>"#,
        );
        validate(&s, &doc(r#"<r x="1" y="2"/>"#)).unwrap();
        validate(&s, &doc(r#"<r y="2" x="1"/>"#)).unwrap();
        assert!(validate(&s, &doc(r#"<r x="1"/>"#)).is_err());
    }

    #[test]
    fn attribute_interleave_accepts_any_order() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <interleave>
                   <attribute name="x"><text/></attribute>
                   <attribute name="y"><text/></attribute>
                 </interleave>
               </element>"#,
        );
        validate(&s, &doc(r#"<r x="1" y="2"/>"#)).unwrap();
        validate(&s, &doc(r#"<r y="2" x="1"/>"#)).unwrap();
    }

    #[test]
    fn attribute_one_or_more_with_named_attrs() {
        // <oneOrMore><attribute><anyName/><text/></attribute></oneOrMore>
        // matches any number of attributes.
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <oneOrMore>
                   <attribute><anyName/><text/></attribute>
                 </oneOrMore>
               </element>"#,
        );
        validate(&s, &doc(r#"<r a="1"/>"#)).unwrap();
        validate(&s, &doc(r#"<r a="1" b="2" c="3"/>"#)).unwrap();
        assert!(validate(&s, &doc("<r/>")).is_err(), "oneOrMore requires ≥1");
    }

    // ── value{} datatype on an attribute ─────────────────────────

    #[test]
    fn attribute_value_constraint() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <attribute name="kind">
                   <choice>
                     <value>a</value>
                     <value>b</value>
                   </choice>
                 </attribute>
               </element>"#,
        );
        validate(&s, &doc(r#"<r kind="a"/>"#)).unwrap();
        validate(&s, &doc(r#"<r kind="b"/>"#)).unwrap();
        assert!(validate(&s, &doc(r#"<r kind="c"/>"#)).is_err());
    }

    // ── nsName / anyName with <except> ──────────────────────────

    #[test]
    fn ns_name_with_except() {
        // RelaxNG spec § 7: <nsName> with <except> excludes specific
        // names within the namespace.
        let s = schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="root">
                 <oneOrMore>
                   <element>
                     <nsName ns="urn:n">
                       <except><name ns="urn:n">forbidden</name></except>
                     </nsName>
                     <text/>
                   </element>
                 </oneOrMore>
               </element>"#,
        );
        validate(&s, &doc(r#"<root><a xmlns="urn:n">1</a></root>"#)).unwrap();
        assert!(validate(&s,
            &doc(r#"<root><forbidden xmlns="urn:n">1</forbidden></root>"#))
            .is_err());
    }

    #[test]
    fn any_name_with_except() {
        // <anyName> with <except> excludes specific names from the
        // "match anything" set.
        let s = schema(
            r#"<element xmlns="http://relaxng.org/ns/structure/1.0" name="root">
                 <oneOrMore>
                   <element>
                     <anyName>
                       <except><name>banned</name></except>
                     </anyName>
                     <text/>
                   </element>
                 </oneOrMore>
               </element>"#,
        );
        validate(&s, &doc("<root><a>1</a><b>2</b></root>")).unwrap();
        assert!(validate(&s, &doc("<root><banned>1</banned></root>")).is_err());
    }

    // ── choice with text+element (mixed-like) ────────────────────

    #[test]
    fn nested_group_with_optional_middle() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <group>
                   <element name="head"><text/></element>
                   <optional><element name="body"><text/></element></optional>
                   <element name="foot"><text/></element>
                 </group>
               </element>"#,
        );
        validate(&s, &doc("<r><head>h</head><foot>f</foot></r>")).unwrap();
        validate(&s,
            &doc("<r><head>h</head><body>b</body><foot>f</foot></r>")).unwrap();
        assert!(validate(&s, &doc("<r><head>h</head></r>")).is_err());
    }

    // ── grammar with broken ref → error at parse or validate ────

    #[test]
    fn ref_to_undefined_name_errors_at_validate() {
        // Schema parses (ref captured) but validating against it must fail
        // when the ref is looked up.
        let result = parse_schema(
            r#"<grammar xmlns="http://relaxng.org/ns/structure/1.0">
                 <start><element name="r"><ref name="undefined"/></element></start>
               </grammar>"#,
        );
        // Implementation may catch this at parse OR at validate; we
        // accept either (the line we want to cover is the
        // `None => p_not_allowed()` arm in deriv).
        if let Ok(s) = result {
            assert!(validate(&s, &doc("<r>x</r>")).is_err());
        }
    }

    // ── list_of_tokens variations ───────────────────────────────

    #[test]
    fn list_of_data_typed_tokens() {
        let s = schema(
            r#"<element name="r" xmlns="http://relaxng.org/ns/structure/1.0">
                 <list>
                   <oneOrMore><data type="string"/></oneOrMore>
                 </list>
               </element>"#,
        );
        validate(&s, &doc("<r>a b c</r>")).unwrap();
        validate(&s, &doc("<r>single</r>")).unwrap();
    }
}
