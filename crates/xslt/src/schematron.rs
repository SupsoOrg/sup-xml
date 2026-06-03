//! ISO Schematron validator — native implementation.
//!
//! Schematron is a rule-based validation language defined by
//! ISO/IEC 19757-3.  A schema is a collection of patterns; each
//! pattern groups rules; each rule fires for nodes matching its
//! `context` (an XSLT match pattern); inside a rule, `<assert>`
//! and `<report>` carry XPath tests against the matched node.
//!
//! The canonical libxslt implementation runs the schema through a
//! 3-step XSLT pipeline (`iso_dsdl_include` →
//! `iso_abstract_expand` → `iso_svrl_for_xslt1`) and applies the
//! resulting stylesheet to the instance, then parses SVRL output.
//! We bypass the meta-XSLT pipeline by interpreting the schema
//! directly: every Schematron construct corresponds to a Rust
//! struct and the same XPath / pattern-matching machinery the
//! XSLT engine uses.
//!
//! Coverage:
//!
//! | Construct          | Status      |
//! |--------------------|-------------|
//! | `schema`           | implemented |
//! | `ns` (prefix decl) | implemented |
//! | `pattern`          | implemented |
//! | `rule` (`context`) | implemented |
//! | `assert` (`test`)  | implemented |
//! | `report` (`test`)  | implemented |
//! | inline `<value-of select="…"/>` in assert/report text | implemented |
//! | inline `<name path="…"/>` in assert/report text       | implemented |
//! | `let` / variable bindings        | implemented (top-level + rule-level) |
//! | `include` (schema/pattern/rule)  | implemented via [`Schematron::compile_str_with_loader`] |
//! | `abstract` patterns + `is-a` / `param` | implemented (XPath-source substitution) |
//! | `abstract` rules + `extends`     | implemented (rule-content splicing)    |
//! | `phase` / `active` filtering     | implemented via [`Schematron::validate_with_phase`] |
//! | `function` (Schematron QuickFix) | out of scope (Schematron 1.5+      |
//! |                                              with sch:fix-only-extension) |
//!
//! Both Schematron namespace URIs are accepted: the ISO version
//! (`http://purl.oclc.org/dsdl/schematron`) and the older 1.5
//! flavour (`http://www.ascc.net/xml/schematron`).

use std::collections::HashMap;

use sup_xml_core::xpath::eval::{
    eval_expr, EvalCtx, StaticContext, Value, XPathBindings,
};
use sup_xml_core::xpath::{parse_xpath, DocIndex, DocIndexLike, Expr, NodeId, XPathNodeKind};
use sup_xml_tree::dom::{Document, Node, NodeKind};

use crate::error::XsltError;

type Result<T> = std::result::Result<T, XsltError>;

pub const SCH_NS_ISO: &str = "http://purl.oclc.org/dsdl/schematron";
pub const SCH_NS_1_5: &str = "http://www.ascc.net/xml/schematron";

fn is_schematron_element(node: &Node) -> bool {
    if !node.is_element() { return false; }
    let uri = node.namespace.get().map(|ns| ns.href()).unwrap_or("");
    uri == SCH_NS_ISO || uri == SCH_NS_1_5
}

// ── compiled schema AST ───────────────────────────────────────────

/// A compiled Schematron schema, ready to validate instances.
#[derive(Debug)]
pub struct Schematron {
    /// Schema-level `<ns prefix="…" uri="…"/>` declarations.
    namespaces: HashMap<String, String>,
    /// Schema-level `<let name="…" value="…"/>` bindings.
    lets:       Vec<Let>,
    patterns:   Vec<Pattern>,
    /// `<sch:phase>` declarations, keyed by phase id.  Each phase
    /// names the subset of patterns active during a `phase`-scoped
    /// validation run.  Empty when the schema declares no phases
    /// — every validation runs all patterns in that case.
    phases:     HashMap<String, Vec<String>>,
    /// Value of `<schema defaultPhase="…">`, if present.  Used by
    /// [`Schematron::validate_with_phase`] when the caller passes
    /// `"#DEFAULT"`.
    default_phase: Option<String>,
}

#[derive(Debug)]
struct Pattern {
    id:    Option<String>,
    /// `<pattern name="…">` — captured for forward-compat with
    /// older Schematron 1.5 schemas that used `name` instead of
    /// `id`.  Not currently surfaced in `Finding`; available for
    /// callers via introspection.
    #[allow(dead_code)]
    name:  Option<String>,
    rules: Vec<Rule>,
}

#[derive(Debug)]
struct Rule {
    context: Expr,
    lets:    Vec<Let>,
    asserts: Vec<Assertion>,
}

#[derive(Debug)]
struct Let {
    name:  String,
    value: Expr,
}

#[derive(Debug)]
struct Assertion {
    kind:    AssertKind,
    test:    Expr,
    /// Mixed-content message — literal text plus inline
    /// `<value-of>`/`<name>` substitutions that consult the
    /// matched node.
    message: Vec<MessagePart>,
    id:      Option<String>,
    role:    Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum AssertKind { Assert, Report }

#[derive(Debug)]
enum MessagePart {
    Text(String),
    /// `<value-of select="…"/>` — evaluate XPath and emit string.
    ValueOf(Expr),
    /// `<name path="…"/>` — XPath result's name (defaults to `.`).
    Name(Expr),
}

// ── compile ───────────────────────────────────────────────────────

impl Schematron {
    /// Compile a parsed Schematron schema document.  The document
    /// MUST have been parsed in namespace-aware mode — Schematron
    /// is namespace-driven (rule contexts and assertion tests use
    /// prefixes declared by `<sch:ns>` elements).
    ///
    /// Returns `XsltError::InvalidStylesheet` for structural
    /// problems (no `<schema>` root, missing `test=` on assert,
    /// etc.) — reusing `XsltError` keeps the public surface small.
    pub fn compile(schema_doc: &Document) -> Result<Self> {
        Self::compile_with_loader(schema_doc, &crate::loader::NullLoader, None)
    }

    /// Compile a Schematron schema, resolving any `<sch:include>`
    /// references via `loader`.  `base` is the URI of the containing
    /// schema (used by [`crate::loader::Loader::load`] for relative
    /// href resolution); pass `None` when relative resolution isn't
    /// needed.
    ///
    /// `<sch:include href="other.sch"/>` is supported at schema-,
    /// pattern-, and rule-level: the referenced document's content
    /// is spliced in as if it had been inlined at the include site.
    /// Fragment IDs (`href="other.sch#frag"`) are honoured — when
    /// present, the element in the loaded doc whose `id="frag"`
    /// supplies the spliced content; without a fragment, the loaded
    /// doc's root supplies it.
    pub fn compile_with_loader(
        schema_doc: &Document,
        loader:     &dyn crate::loader::Loader,
        base:       Option<&str>,
    ) -> Result<Self> {
        let root = schema_doc.root();
        if !is_schematron_element(root) || root.local_name() != "schema" {
            return Err(XsltError::InvalidStylesheet(
                "Schematron root element must be sch:schema".into(),
            ));
        }

        let mut s = Schematron {
            namespaces:    HashMap::new(),
            lets:          Vec::new(),
            patterns:      Vec::new(),
            phases:        HashMap::new(),
            default_phase: attr(root, "defaultPhase").map(|s| s.to_string()),
        };
        // First pass: index abstract patterns (referenced by
        // `<pattern is-a="…">`) and abstract rules (referenced by
        // `<sch:extends rule="…"/>`) by their `id=` attribute.  The
        // index borrows from `schema_doc`, which the caller owns
        // for the duration of this call.
        let mut abs_patterns: HashMap<String, &Node> = HashMap::new();
        let mut abs_rules:    HashMap<String, &Node> = HashMap::new();
        collect_abstract_patterns(root, &mut abs_patterns);
        collect_abstract_rules(root, &mut abs_rules);
        let abstracts = Abstracts { patterns: abs_patterns, rules: abs_rules };
        process_schema_children(root, loader, base, &abstracts, &mut s)?;
        // libxml2's `xmlSchematronParse` rejects a schema that defines no
        // rules at all — an empty `<schema/>` or one whose patterns carry
        // no `<rule>` — so a consumer (lxml) raises SchematronParseError
        // rather than silently accepting a no-op schema.  Abstract patterns
        // and rules (templates referenced via `is-a` / `extends`) count:
        // an abstract-only schema is well-formed even though its concrete
        // pattern/rule lists are empty.
        let has_concrete_rule = s.patterns.iter().any(|p| !p.rules.is_empty());
        let has_abstract = !abstracts.patterns.is_empty() || !abstracts.rules.is_empty();
        if !has_concrete_rule && !has_abstract {
            return Err(XsltError::InvalidStylesheet(
                "Schematron schema defines no rules".into(),
            ));
        }
        Ok(s)
    }

    /// Compile a Schematron schema directly from its text.
    /// Convenience wrapper.
    pub fn compile_str(text: &str) -> Result<Self> {
        let opts = sup_xml_core::ParseOptions {
            namespace_aware: true, ..Default::default()
        };
        let doc = sup_xml_core::parse_str(text, &opts).map_err(XsltError::from)?;
        Self::compile(&doc)
    }

    /// Compile a Schematron schema from its text, resolving any
    /// `<sch:include>` references via `loader`.  See
    /// [`Schematron::compile_with_loader`] for include semantics.
    pub fn compile_str_with_loader(
        text:   &str,
        loader: &dyn crate::loader::Loader,
        base:   Option<&str>,
    ) -> Result<Self> {
        let opts = sup_xml_core::ParseOptions {
            namespace_aware: true, ..Default::default()
        };
        let doc = sup_xml_core::parse_str(text, &opts).map_err(XsltError::from)?;
        Self::compile_with_loader(&doc, loader, base)
    }

    /// Compile via the official ISO Schematron XSLT pipeline:
    /// run the schema through `iso_dsdl_include.xsl` →
    /// `iso_abstract_expand.xsl` → `iso_svrl_for_xslt1.xsl`, then
    /// use the generated SVRL-emitting validator stylesheet as
    /// the actual validator.  Output: a [`Schematron`] whose
    /// `validate` method runs the validator stylesheet against
    /// the instance and parses the SVRL findings.
    ///
    /// `loader` resolves `xsl:import` references inside the
    /// pipeline stylesheets (`iso_svrl_for_xslt1.xsl` imports
    /// `iso_schematron_skeleton_for_xslt1.xsl`).  `base_dir` is
    /// where the pipeline xsl files live; use the host's local
    /// copy of lxml's `isoschematron/resources/xsl/iso-schematron-xslt1/`.
    ///
    /// Compared to [`Schematron::compile_str`] (our native
    /// implementation), the ISO pipeline understands `<abstract>`
    /// patterns, `<extends>`, and `<include>` references — at the
    /// cost of being slower and depending on those vendored XSLT
    /// files on disk.  Use compile_str for simple schemas; use
    /// `compile_iso` when you need full Schematron 1.6 features.
    pub fn compile_iso(
        schema_text: &str,
        base_dir:    &str,
        loader:      &dyn crate::loader::Loader,
    ) -> Result<IsoSchematronValidator> {
        let svrl_path = format!("{base_dir}/iso_svrl_for_xslt1.xsl");
        let svrl_text = loader.load(&svrl_path, None)?;
        let svrl = crate::Stylesheet::compile_str_with_loader(
            &svrl_text, loader, Some(&svrl_path))?;
        let opts = sup_xml_core::ParseOptions {
            namespace_aware: true, ..Default::default()
        };
        let schema_doc = sup_xml_core::parse_str(schema_text, &opts)
            .map_err(XsltError::from)?;
        let validator_xsl = svrl.apply(&schema_doc)?.to_string()?;
        let validator = crate::Stylesheet::compile_str_with_loader(
            &validator_xsl, loader, Some(&svrl_path))?;
        Ok(IsoSchematronValidator { validator })
    }
}

/// Validator produced by the ISO Schematron XSLT pipeline.
/// Applying it to an instance produces SVRL (Schematron
/// Validation Report Language) output — the structured
/// machine-readable validation report.
pub struct IsoSchematronValidator {
    validator: crate::Stylesheet,
}

impl IsoSchematronValidator {
    /// Validate `instance_doc` and return the raw SVRL output as
    /// a string.  Parsing SVRL into structured findings is the
    /// caller's job (or a future helper in this module).
    pub fn validate_to_svrl(
        &self,
        instance_doc: &sup_xml_tree::dom::Document,
    ) -> Result<String> {
        let result = self.validator.apply(instance_doc)?;
        Ok(result.to_string()?)
    }

    /// Validate an instance from source text.  Convenience wrapper.
    pub fn validate_str(&self, instance_text: &str) -> Result<String> {
        let opts = sup_xml_core::ParseOptions {
            namespace_aware: true, ..Default::default()
        };
        let doc = sup_xml_core::parse_str(instance_text, &opts)
            .map_err(XsltError::from)?;
        self.validate_to_svrl(&doc)
    }
}

/// Look-up table for `<pattern abstract="true">` and
/// `<rule abstract="true">` by id, populated in a pre-pass before
/// concrete patterns are compiled.  Lives as long as the source
/// schema Document.
struct Abstracts<'a> {
    patterns: HashMap<String, &'a Node<'a>>,
    rules:    HashMap<String, &'a Node<'a>>,
}

/// Param substitutions active during compilation of an abstract
/// pattern instantiation.  `None` outside abstract instantiation.
/// Maps `name` → raw XPath fragment to splice in at each `$name`
/// occurrence inside the abstract pattern's XPath strings.
type Params<'a> = Option<&'a HashMap<String, String>>;

/// Pre-pass that walks `parent` (typically `<schema>`) and any
/// `<pattern abstract="true">` / `<rule abstract="true">` it
/// contains, indexing them by `id`.  Schemas referenced via
/// `<include>` aren't visited here — top-level abstracts only.
fn collect_abstract_patterns<'a>(parent: &'a Node<'a>, out: &mut HashMap<String, &'a Node<'a>>) {
    walk_abstracts(parent, out, "pattern");
}

fn collect_abstract_rules<'a>(parent: &'a Node<'a>, out: &mut HashMap<String, &'a Node<'a>>) {
    walk_abstracts(parent, out, "rule");
}

fn walk_abstracts<'a>(
    parent: &'a Node<'a>,
    out:    &mut HashMap<String, &'a Node<'a>>,
    want:   &str,
) {
    for child in parent.children() {
        if !is_schematron_element(child) { continue; }
        let local = child.local_name();
        if local == want && attr(child, "abstract") == Some("true") {
            if let Some(id) = attr(child, "id") {
                out.insert(id.to_string(), child);
            }
        }
        // Recurse into <pattern> so abstract <rule>s inside concrete
        // patterns get picked up too.  Don't descend into abstract
        // patterns when looking for rules — those rules belong to
        // the abstract, not the schema's flat rule pool.
        if local == "pattern" && attr(child, "abstract") != Some("true") {
            walk_abstracts(child, out, want);
        }
    }
}

/// Replace `$NCName` references in an XPath source string with the
/// matching param's value.  Used for abstract-pattern instantiation
/// per ISO Schematron §6.4 — the `<param value="…">` text is
/// itself raw XPath, so this is plain textual substitution.
fn substitute_params(s: &str, params: Params) -> String {
    let Some(params) = params else { return s.to_string(); };
    if params.is_empty() { return s.to_string(); }
    let mut out = String::with_capacity(s.len());
    let mut iter = s.char_indices().peekable();
    while let Some((_, c)) = iter.next() {
        if c == '$' {
            if let Some(&(name_start, first)) = iter.peek() {
                if is_ncname_start(first) {
                    iter.next();
                    let mut name_end = name_start + first.len_utf8();
                    while let Some(&(j, c)) = iter.peek() {
                        if is_ncname_cont(c) {
                            iter.next();
                            name_end = j + c.len_utf8();
                        } else {
                            break;
                        }
                    }
                    let name = &s[name_start..name_end];
                    if let Some(v) = params.get(name) {
                        out.push_str(v);
                    } else {
                        out.push('$');
                        out.push_str(name);
                    }
                    continue;
                }
            }
            out.push('$');
        } else {
            out.push(c);
        }
    }
    out
}

fn is_ncname_start(c: char) -> bool {
    // ASCII-only NCNameStartChar — Schematron `<param>` names in
    // practice are ASCII; full XML 1.0 §4 production is overkill.
    matches!(c, 'a'..='z' | 'A'..='Z' | '_')
}

fn is_ncname_cont(c: char) -> bool {
    is_ncname_start(c) || c.is_ascii_digit() || c == '-' || c == '.'
}

/// Parse `s` as XPath after applying abstract-pattern param
/// substitution (if any).
fn parse_xpath_with_params(s: &str, params: Params) -> Result<Expr> {
    parse_xpath(&substitute_params(s, params)).map_err(XsltError::from)
}

/// Walk a `<schema>` element's children, collecting `<ns>`,
/// `<let>`, `<pattern>` declarations into `out`.  Recurses through
/// any `<include>` elements found at this level.
fn process_schema_children<'a, 'b>(
    parent:    &'b Node<'b>,
    loader:    &dyn crate::loader::Loader,
    base:      Option<&str>,
    abstracts: &Abstracts<'a>,
    out:       &mut Schematron,
) -> Result<()> {
    for child in parent.children() {
        if !is_schematron_element(child) { continue; }
        match child.local_name() {
            "ns" => {
                if let (Some(p), Some(u)) = (
                    attr(child, "prefix"), attr(child, "uri"),
                ) {
                    out.namespaces.insert(p.to_string(), u.to_string());
                }
            }
            "let"     => out.lets.push(compile_let(child, None)?),
            "pattern" => {
                // Skip `<pattern abstract="true">` — already
                // indexed via [`collect_abstract_patterns`]; it's
                // a template, not a runnable pattern.
                if attr(child, "abstract") == Some("true") { continue; }
                if let Some(abs_id) = attr(child, "is-a") {
                    out.patterns.push(instantiate_abstract_pattern(
                        child, abs_id, loader, base, abstracts,
                    )?);
                } else {
                    out.patterns.push(compile_pattern(child, loader, base, abstracts, None)?);
                }
            }
            "phase" => {
                if let Some(id) = attr(child, "id") {
                    let mut active = Vec::new();
                    for c in child.children() {
                        if is_schematron_element(c)
                            && c.local_name() == "active"
                        {
                            if let Some(p) = attr(c, "pattern") {
                                active.push(p.to_string());
                            }
                        }
                    }
                    out.phases.insert(id.to_string(), active);
                }
            }
            "include" => {
                let (doc, target_lookup, sub_base) = load_include(child, loader, base)?;
                let target = locate_include_target(doc.root(), target_lookup.as_deref())?;
                // The included element can be either another `<schema>`
                // (top-level include) or a `<pattern>`/`<let>`/`<ns>`
                // directly (fragment include).  Handle both shapes.
                if target.local_name() == "schema" && is_schematron_element(target) {
                    process_schema_children(target, loader, sub_base.as_deref(), abstracts, out)?;
                } else {
                    process_schema_one(target, loader, sub_base.as_deref(), abstracts, out)?;
                }
            }
            // <title>, <p>, <diagnostics>, <fix> — documentary.
            _ => {}
        }
    }
    Ok(())
}

/// Treat a single included element as a top-level schema child.
/// Mirrors the `match` inside [`process_schema_children`] but for
/// the case where the fragment IS the declaration (not its parent).
fn process_schema_one<'a, 'b>(
    node:      &'b Node<'b>,
    loader:    &dyn crate::loader::Loader,
    base:      Option<&str>,
    abstracts: &Abstracts<'a>,
    out:       &mut Schematron,
) -> Result<()> {
    if !is_schematron_element(node) { return Ok(()); }
    match node.local_name() {
        "ns" => {
            if let (Some(p), Some(u)) = (attr(node, "prefix"), attr(node, "uri")) {
                out.namespaces.insert(p.to_string(), u.to_string());
            }
        }
        "let"     => out.lets.push(compile_let(node, None)?),
        "pattern" => {
            if attr(node, "abstract") == Some("true") { return Ok(()); }
            if let Some(abs_id) = attr(node, "is-a") {
                out.patterns.push(instantiate_abstract_pattern(
                    node, abs_id, loader, base, abstracts,
                )?);
            } else {
                out.patterns.push(compile_pattern(node, loader, base, abstracts, None)?);
            }
        }
        _ => {} // Other fragment kinds aren't valid as schema-level content.
    }
    Ok(())
}

/// Instantiate an abstract pattern via `<pattern is-a="X">`.  Looks
/// up the abstract pattern Node by id, gathers `<param>` children
/// into a substitution map, and compiles each rule with those
/// substitutions applied to its XPath strings.
fn instantiate_abstract_pattern<'a, 'b>(
    instance:  &'b Node<'b>,
    abs_id:    &str,
    loader:    &dyn crate::loader::Loader,
    base:      Option<&str>,
    abstracts: &Abstracts<'a>,
) -> Result<Pattern> {
    let abstract_node = abstracts.patterns.get(abs_id).ok_or_else(|| {
        XsltError::UnresolvedReference(format!(
            "<pattern is-a='{abs_id}'> references an abstract pattern that wasn't declared"
        ))
    })?;
    // Collect <param name=… value=…> children from the instance.
    let mut params: HashMap<String, String> = HashMap::new();
    for c in instance.children() {
        if !is_schematron_element(c) || c.local_name() != "param" { continue; }
        let n = attr(c, "name").ok_or_else(|| XsltError::InvalidStylesheet(
            "sch:param requires a name= attribute".into(),
        ))?;
        let v = attr(c, "value").ok_or_else(|| XsltError::InvalidStylesheet(
            "sch:param requires a value= attribute".into(),
        ))?;
        params.insert(n.to_string(), v.to_string());
    }
    // Compile the abstract pattern's rules with the substitution map.
    let id   = attr(instance, "id").map(|s| s.to_string());
    let name = attr(instance, "name").map(|s| s.to_string());
    let mut rules = Vec::new();
    process_pattern_children(abstract_node, loader, base, abstracts, Some(&params), &mut rules)?;
    Ok(Pattern { id, name, rules })
}

fn compile_pattern<'a, 'b>(
    node:      &'b Node<'b>,
    loader:    &dyn crate::loader::Loader,
    base:      Option<&str>,
    abstracts: &Abstracts<'a>,
    params:    Params,
) -> Result<Pattern> {
    let id   = attr(node, "id").map(|s| s.to_string());
    let name = attr(node, "name").map(|s| s.to_string());
    let mut rules = Vec::new();
    process_pattern_children(node, loader, base, abstracts, params, &mut rules)?;
    Ok(Pattern { id, name, rules })
}

fn process_pattern_children<'a, 'b>(
    parent:    &'b Node<'b>,
    loader:    &dyn crate::loader::Loader,
    base:      Option<&str>,
    abstracts: &Abstracts<'a>,
    params:    Params,
    rules:     &mut Vec<Rule>,
) -> Result<()> {
    for child in parent.children() {
        if !is_schematron_element(child) { continue; }
        match child.local_name() {
            "rule" => {
                // Skip abstract rules — they're templates that
                // contribute via <sch:extends>, not directly.
                if attr(child, "abstract") == Some("true") { continue; }
                rules.push(compile_rule(child, loader, base, abstracts, params)?);
            }
            "include" => {
                let (doc, target_lookup, sub_base) = load_include(child, loader, base)?;
                let target = locate_include_target(doc.root(), target_lookup.as_deref())?;
                // Two shapes: included is a <pattern> (use its rules)
                // or a <rule> directly (single-fragment include).
                match (is_schematron_element(target), target.local_name()) {
                    (true, "pattern") => process_pattern_children(target, loader, sub_base.as_deref(), abstracts, params, rules)?,
                    (true, "rule")    => rules.push(compile_rule(target, loader, sub_base.as_deref(), abstracts, params)?),
                    _ => return Err(XsltError::InvalidStylesheet(format!(
                        "sch:include inside <pattern> must point to a <pattern> or <rule>; got <{}>",
                        target.name(),
                    ))),
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn compile_rule<'a, 'b>(
    node:      &'b Node<'b>,
    loader:    &dyn crate::loader::Loader,
    base:      Option<&str>,
    abstracts: &Abstracts<'a>,
    params:    Params,
) -> Result<Rule> {
    let ctx = attr(node, "context").ok_or_else(|| XsltError::InvalidStylesheet(
        "sch:rule requires a context= attribute".into(),
    ))?;
    let context = parse_xpath_with_params(ctx, params)?;
    let mut lets    = Vec::new();
    let mut asserts = Vec::new();
    process_rule_children(node, loader, base, abstracts, params, &mut lets, &mut asserts)?;
    Ok(Rule { context, lets, asserts })
}

fn process_rule_children<'a, 'b>(
    parent:    &'b Node<'b>,
    loader:    &dyn crate::loader::Loader,
    base:      Option<&str>,
    abstracts: &Abstracts<'a>,
    params:    Params,
    lets:      &mut Vec<Let>,
    asserts:   &mut Vec<Assertion>,
) -> Result<()> {
    for child in parent.children() {
        if !is_schematron_element(child) { continue; }
        match child.local_name() {
            "let"    => lets.push(compile_let(child, params)?),
            "assert" => asserts.push(compile_assertion(child, AssertKind::Assert, params)?),
            "report" => asserts.push(compile_assertion(child, AssertKind::Report, params)?),
            "extends" => {
                // ISO §6.3: `<sch:extends rule="abstract-rule-id"/>`
                // splices the abstract rule's lets/asserts/reports
                // in here.  The abstract rule was indexed in the
                // pre-pass via [`collect_abstract_rules`].
                let rule_id = attr(child, "rule").ok_or_else(|| XsltError::InvalidStylesheet(
                    "sch:extends requires a rule= attribute".into(),
                ))?;
                let abstract_rule = abstracts.rules.get(rule_id).ok_or_else(||
                    XsltError::UnresolvedReference(format!(
                        "<extends rule='{rule_id}'> references an abstract rule that wasn't declared"
                    ))
                )?;
                process_rule_children(abstract_rule, loader, base, abstracts, params, lets, asserts)?;
            }
            "include" => {
                let (doc, target_lookup, sub_base) = load_include(child, loader, base)?;
                let target = locate_include_target(doc.root(), target_lookup.as_deref())?;
                // Two shapes: included is a <rule> (splice its
                // contents) or a single <assert>/<report>/<let>
                // (treat as the lone declaration at this site).
                match (is_schematron_element(target), target.local_name()) {
                    (true, "rule") => process_rule_children(target, loader, sub_base.as_deref(), abstracts, params, lets, asserts)?,
                    (true, "assert") => asserts.push(compile_assertion(target, AssertKind::Assert, params)?),
                    (true, "report") => asserts.push(compile_assertion(target, AssertKind::Report, params)?),
                    (true, "let")    => lets.push(compile_let(target, params)?),
                    _ => return Err(XsltError::InvalidStylesheet(format!(
                        "sch:include inside <rule> must point to <rule>/<assert>/<report>/<let>; got <{}>",
                        target.name(),
                    ))),
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Resolve a `<sch:include href=…>` element: load the referenced
/// document via `loader`, parse it, and return `(doc, fragment_id,
/// resolved_base)`.  Caller uses [`locate_include_target`] to find
/// the matching element inside `doc`.
fn load_include(
    node:   &Node,
    loader: &dyn crate::loader::Loader,
    base:   Option<&str>,
) -> Result<(Document, Option<String>, Option<String>)> {
    let href = attr(node, "href").ok_or_else(|| XsltError::InvalidStylesheet(
        "sch:include requires an href= attribute".into(),
    ))?;
    // Split off `#fragment` if present — Schematron uses the host
    // doc + element id to locate the specific declaration.
    let (path, fragment) = match href.find('#') {
        Some(i) => (&href[..i], Some(href[i+1..].to_string())),
        None    => (href, None),
    };
    let text = loader.load(path, base)?;
    let opts = sup_xml_core::ParseOptions {
        namespace_aware: true, ..Default::default()
    };
    let doc = sup_xml_core::parse_str(&text, &opts).map_err(XsltError::from)?;
    let resolved_base = loader.resolve(path, base).ok();
    Ok((doc, fragment, resolved_base))
}

/// Given the root of an included document and an optional fragment
/// id, find the element to splice in.  Without a fragment, returns
/// the root itself; with a fragment, searches the tree for an
/// element whose `id=` or `xml:id=` matches.
fn locate_include_target<'a>(
    root:     &'a Node<'a>,
    fragment: Option<&str>,
) -> Result<&'a Node<'a>> {
    let Some(id) = fragment else { return Ok(root); };
    find_by_id(root, id).ok_or_else(|| XsltError::InvalidStylesheet(format!(
        "sch:include fragment '#{id}' not found in loaded document"
    )))
}

fn find_by_id<'a>(node: &'a Node<'a>, id: &str) -> Option<&'a Node<'a>> {
    if node.is_element() {
        if attr(node, "id") == Some(id) {
            return Some(node);
        }
        // `xml:id` per https://www.w3.org/TR/xml-id/ — also honoured.
        for a in node.attributes() {
            if a.local_name() == "id"
               && a.namespace.get().and_then(|n| n.prefix()) == Some("xml") && a.value() == id {
                return Some(node);
            }
        }
        for c in node.children() {
            if let Some(hit) = find_by_id(c, id) { return Some(hit); }
        }
    }
    None
}

fn compile_let(node: &Node, params: Params) -> Result<Let> {
    let name  = attr(node, "name").ok_or_else(|| XsltError::InvalidStylesheet(
        "sch:let requires a name= attribute".into(),
    ))?.to_string();
    let value = attr(node, "value").ok_or_else(|| XsltError::InvalidStylesheet(
        "sch:let requires a value= attribute".into(),
    ))?;
    Ok(Let { name, value: parse_xpath_with_params(value, params)? })
}

fn compile_assertion(node: &Node, kind: AssertKind, params: Params) -> Result<Assertion> {
    let test = attr(node, "test").ok_or_else(|| XsltError::InvalidStylesheet(
        format!("sch:{} requires a test= attribute",
            if matches!(kind, AssertKind::Assert) { "assert" } else { "report" }),
    ))?;
    let test = parse_xpath_with_params(test, params)?;
    Ok(Assertion {
        kind,
        test,
        message: compile_message(node, params)?,
        id:   attr(node, "id").map(str::to_string),
        role: attr(node, "role").map(str::to_string),
    })
}

fn compile_message(node: &Node, params: Params) -> Result<Vec<MessagePart>> {
    let mut parts = Vec::new();
    for child in node.children() {
        match child.kind {
            NodeKind::Text | NodeKind::CData => {
                parts.push(MessagePart::Text(child.content().to_string()));
            }
            NodeKind::Element if is_schematron_element(child) => {
                match child.local_name() {
                    "value-of" => {
                        let sel = attr(child, "select").ok_or_else(||
                            XsltError::InvalidStylesheet(
                                "sch:value-of requires select=".into()))?;
                        parts.push(MessagePart::ValueOf(
                            parse_xpath_with_params(sel, params)?));
                    }
                    "name" => {
                        let path = attr(child, "path").unwrap_or(".");
                        parts.push(MessagePart::Name(
                            parse_xpath_with_params(path, params)?));
                    }
                    _ => {
                        // Other inline elements (sch:emph, sch:span):
                        // stringify their text content.
                        parts.push(MessagePart::Text(child_text(child)));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(parts)
}

fn child_text(node: &Node) -> String {
    let mut s = String::new();
    for c in node.children() {
        if matches!(c.kind, NodeKind::Text | NodeKind::CData) {
            s.push_str(c.content());
        }
    }
    s
}

fn attr<'a>(node: &'a Node, name: &str) -> Option<&'a str> {
    for a in node.attributes() {
        if a.name() == name && !a.name().contains(':') {
            return Some(a.value());
        }
    }
    None
}

// ── validation ────────────────────────────────────────────────────

/// One finding from a Schematron validation run.
#[derive(Debug, Clone)]
pub struct Finding {
    pub kind:        FindingKind,
    pub message:     String,
    /// `pattern.id` of the pattern that fired.  `None` for
    /// patterns with no id.
    pub pattern_id:  Option<String>,
    pub assertion_id: Option<String>,
    pub role:        Option<String>,
    /// `generate-id()`-style stable id for the node where the
    /// rule fired.
    pub location_id: String,
    /// Local-name of the offending node — handy for diagnostic
    /// messages.
    pub context_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    /// An `<assert>` test evaluated to false.
    FailedAssert,
    /// A `<report>` test evaluated to true.
    SuccessfulReport,
}

/// Result of a validation run.  `valid()` is the common gate
/// callers reach for; the full `findings` vec carries every
/// failed-assert and successful-report for richer reporting.
#[derive(Debug, Clone, Default)]
pub struct ValidationReport {
    pub findings: Vec<Finding>,
}

impl ValidationReport {
    /// A document is *valid* if no assertions failed.  Successful
    /// reports are diagnostic (they fire when something noteworthy
    /// happened, not when something's wrong), so they don't make
    /// a document invalid.
    pub fn valid(&self) -> bool {
        !self.findings.iter().any(|f| matches!(f.kind, FindingKind::FailedAssert))
    }
}

/// Bridge Schematron's namespace + let bindings into the XPath
/// engine.  Each rule-evaluation builds one of these against the
/// current variable scope.
struct SchBindings<'a> {
    namespaces: &'a HashMap<String, String>,
    vars:       &'a HashMap<String, Value>,
}

/// Static XPath context for a Schematron expression — seeded from the
/// bindings' host config (XPath version + regex dialect) so eval
/// observes the same distinctions Schematron declares.
fn sch_static_ctx(bindings: &SchBindings<'_>) -> StaticContext {
    StaticContext {
        xpath_2_0: bindings.xpath_version_2_or_later(),
        libxml2_compatible: false, current_node: None,
    }
}

impl XPathBindings for SchBindings<'_> {
    fn resolve_prefix(&self, prefix: &str) -> Option<String> {
        self.namespaces.get(prefix).cloned()
    }
    fn variable(&self, name: &str) -> Option<Value> {
        self.vars.get(name).cloned()
    }
}

impl Schematron {
    /// Validate an instance from source text.  Convenience wrapper
    /// that parses with `namespace_aware: true` — required for
    /// Schematron's prefix-based rule contexts to resolve.
    pub fn validate_str(&self, instance_text: &str) -> Result<ValidationReport> {
        self.validate_str_with_phase(instance_text, "#ALL")
    }

    /// Validate an instance from source text, running only the
    /// patterns active under `phase`.  See [`Schematron::validate_with_phase`]
    /// for phase semantics.
    pub fn validate_str_with_phase(
        &self, instance_text: &str, phase: &str,
    ) -> Result<ValidationReport> {
        let opts = sup_xml_core::ParseOptions {
            namespace_aware: true, ..Default::default()
        };
        let doc = sup_xml_core::parse_str(instance_text, &opts).map_err(XsltError::from)?;
        self.validate_with_phase(&doc, phase)
    }

    /// Validate `instance_doc` against this schema.  Returns a
    /// [`ValidationReport`] with every failed assert and
    /// successful report.  The instance document MUST have been
    /// parsed in namespace-aware mode if any rule contexts or
    /// assertion tests use prefixed names; use
    /// [`Schematron::validate_str`] for the common case.
    pub fn validate(&self, instance_doc: &Document) -> Result<ValidationReport> {
        self.validate_with_phase(instance_doc, "#ALL")
    }

    /// Validate `instance_doc`, running only the patterns active
    /// under `phase`.  ISO Schematron §6.5 — phases let a schema
    /// expose multiple validation profiles from one declaration set.
    ///
    /// Special phase names:
    /// * `"#ALL"` — run every pattern (the no-phase default).
    /// * `"#DEFAULT"` — run the phase named by `<schema defaultPhase=…>`;
    ///   if the schema has no `defaultPhase`, falls back to `#ALL`.
    ///
    /// Any other name is looked up in the schema's `<sch:phase>`
    /// declarations.  An unknown name (one that isn't `#ALL`, isn't
    /// `#DEFAULT`, and doesn't match a declared phase id) returns
    /// `Err(InvalidStylesheet)` so callers see the typo instead of
    /// silently validating against zero patterns.
    pub fn validate_with_phase(
        &self, instance_doc: &Document, phase: &str,
    ) -> Result<ValidationReport> {
        let active: Option<&[String]> = match phase {
            "#ALL" => None,
            "#DEFAULT" => match self.default_phase.as_deref() {
                None | Some("#ALL") => None,
                Some(name) => Some(self.phases.get(name)
                    .ok_or_else(|| XsltError::InvalidStylesheet(format!(
                        "schema defaultPhase='{name}' but no <sch:phase id='{name}'> declared"
                    )))?
                    .as_slice()),
            },
            other => Some(self.phases.get(other)
                .ok_or_else(|| XsltError::InvalidStylesheet(format!(
                    "unknown phase '{other}': no matching <sch:phase id='{other}'> in schema"
                )))?
                .as_slice()),
        };
        self.validate_inner(instance_doc, active)
    }

    fn validate_inner(
        &self, instance_doc: &Document, active: Option<&[String]>,
    ) -> Result<ValidationReport> {
        let idx = DocIndex::build(instance_doc);
        let mut report = ValidationReport::default();

        // Pre-evaluate schema-level `<let>` bindings.  XPath
        // context is the document root.  Each let sees the
        // previously-bound lets — we construct fresh bindings per
        // iteration so the borrow on `schema_vars` is released
        // before insert.
        let mut schema_vars: HashMap<String, Value> = HashMap::new();
        for l in &self.lets {
            let v = {
                let bindings = SchBindings {
                    namespaces: &self.namespaces,
                    vars:       &schema_vars,
                };
                let sc = sch_static_ctx(&bindings);
                let ctx = EvalCtx { context_node: 0, pos: 1, size: 1, bindings: &bindings, static_ctx: &sc };
                eval_expr(&l.value, &ctx, &idx).map_err(XsltError::from)?
            };
            schema_vars.insert(l.name.clone(), v);
        }

        // Per pattern → per node → first matching rule fires.
        for pattern in &self.patterns {
            // Phase filter: if `active` names a subset, skip
            // patterns whose id isn't in the list.  Patterns
            // without an id can never be selected by phase.
            if let Some(list) = active {
                let Some(pid) = pattern.id.as_deref() else { continue; };
                if !list.iter().any(|n| n == pid) { continue; }
            }
            for node_id in 0..idx.nodes.len() {
                // Skip the synthetic document and namespace nodes
                // by default — rule contexts almost always target
                // elements / attributes.  (Document-rooted rules
                // are still reachable via context="/".)
                let kind = idx.kind(node_id);
                if !matches!(kind,
                    XPathNodeKind::Element | XPathNodeKind::Document
                        | XPathNodeKind::Attribute)
                {
                    continue;
                }
                for rule in &pattern.rules {
                    if !rule_matches(&rule.context, node_id, &idx,
                        &self.namespaces, &schema_vars)?
                    {
                        continue;
                    }
                    // This rule fires.  Evaluate its lets, then
                    // each assertion.  Lower-cased "first rule
                    // wins" semantics: break after this rule.
                    let mut local_vars = schema_vars.clone();
                    for l in &rule.lets {
                        let v = {
                            let bindings = SchBindings {
                                namespaces: &self.namespaces, vars: &local_vars,
                            };
                            let sc = sch_static_ctx(&bindings);
                            let ctx = EvalCtx { context_node: node_id, pos: 1, size: 1, bindings: &bindings, static_ctx: &sc };
                            eval_expr(&l.value, &ctx, &idx).map_err(XsltError::from)?
                        };
                        local_vars.insert(l.name.clone(), v);
                    }
                    for assertion in &rule.asserts {
                        evaluate_assertion(
                            assertion, node_id, &idx,
                            &self.namespaces, &local_vars,
                            pattern, &mut report,
                        )?;
                    }
                    break;
                }
            }
        }
        Ok(report)
    }
}

fn rule_matches(
    context: &Expr, node: NodeId, idx: &DocIndex<'_>,
    namespaces: &HashMap<String, String>,
    vars:       &HashMap<String, Value>,
) -> Result<bool> {
    let bindings = SchBindings { namespaces, vars };
    let sc = sch_static_ctx(&bindings);
    let mut cur = Some(node);
    while let Some(ctx_node) = cur {
        let ctx = EvalCtx { context_node: ctx_node, pos: 1, size: 1, bindings: &bindings, static_ctx: &sc };
        let v = eval_expr(context, &ctx, idx).map_err(XsltError::from)?;
        if let Value::NodeSet(ns) = v {
            if ns.contains(&node) {
                return Ok(true);
            }
        }
        cur = idx.parent(ctx_node);
    }
    Ok(false)
}

fn evaluate_assertion(
    a:          &Assertion,
    node:       NodeId,
    idx:        &DocIndex<'_>,
    namespaces: &HashMap<String, String>,
    vars:       &HashMap<String, Value>,
    pattern:    &Pattern,
    report:     &mut ValidationReport,
) -> Result<()> {
    let bindings = SchBindings { namespaces, vars };
    let sc = sch_static_ctx(&bindings);
    let ctx = EvalCtx { context_node: node, pos: 1, size: 1, bindings: &bindings, static_ctx: &sc };
    let test_v = eval_expr(&a.test, &ctx, idx).map_err(XsltError::from)?;
    let truth = value_to_bool(&test_v);
    let fired = match a.kind {
        AssertKind::Assert => !truth,  // assert fires when test fails
        AssertKind::Report =>  truth,  // report fires when test succeeds
    };
    if !fired { return Ok(()); }
    let message = render_message(&a.message, node, idx, namespaces, vars)?;
    report.findings.push(Finding {
        kind: match a.kind {
            AssertKind::Assert => FindingKind::FailedAssert,
            AssertKind::Report => FindingKind::SuccessfulReport,
        },
        message,
        pattern_id:   pattern.id.clone(),
        assertion_id: a.id.clone(),
        role:         a.role.clone(),
        location_id:  format!("id{:x}", node),
        context_name: idx.local_name(node).to_string(),
    });
    Ok(())
}

fn render_message(
    parts:      &[MessagePart],
    node:       NodeId,
    idx:        &DocIndex<'_>,
    namespaces: &HashMap<String, String>,
    vars:       &HashMap<String, Value>,
) -> Result<String> {
    let bindings = SchBindings { namespaces, vars };
    let sc = sch_static_ctx(&bindings);
    let ctx = EvalCtx { context_node: node, pos: 1, size: 1, bindings: &bindings, static_ctx: &sc };
    let mut s = String::new();
    for p in parts {
        match p {
            MessagePart::Text(t) => s.push_str(t),
            MessagePart::ValueOf(e) => {
                let v = eval_expr(e, &ctx, idx).map_err(XsltError::from)?;
                s.push_str(&value_to_string(&v, idx));
            }
            MessagePart::Name(e) => {
                let v = eval_expr(e, &ctx, idx).map_err(XsltError::from)?;
                let target_node = match v {
                    Value::NodeSet(ns) if !ns.is_empty() => ns[0],
                    _ => node,
                };
                s.push_str(idx.node_name(target_node));
            }
        }
    }
    // Normalise inner whitespace per common Schematron output
    // conventions — collapse runs of whitespace to a single space
    // and trim leading/trailing whitespace.
    let normalised: Vec<&str> = s.split_whitespace().collect();
    Ok(normalised.join(" "))
}

fn value_to_bool(v: &Value) -> bool {
    match v {
        Value::Boolean(b) => *b,
        Value::Number(n)  => n.as_f64() != 0.0 && !n.as_f64().is_nan(),
        Value::String(s)  => !s.is_empty(),
        Value::NodeSet(n) => !n.is_empty(),
        // ForeignNodeSet only originates from document() in compat-
        // driven XPath; Schematron runs against single-doc XPath.
        Value::ForeignNodeSet(n) => !n.is_empty(),
        Value::Typed(t) => {
            if let Some(b) = t.boolean { return b; }
            if let Some(n) = t.numeric { return n != 0.0 && !n.is_nan(); }
            !t.lexical.is_empty()
        }
        Value::Sequence(items) => match items.first() {
            None    => false,
            Some(v) => value_to_bool(v),
        }
        Value::IntRange { lo, hi } if lo == hi => *lo != 0,
        Value::IntRange { .. } => true,
        Value::Map(_) | Value::Array(_) | Value::Function(_) => true,
    }
}

fn value_to_string<I: DocIndexLike>(v: &Value, idx: &I) -> String {
    use sup_xml_core::xpath::eval::value_to_string;
    value_to_string(v, idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(text: &str) -> Schematron {
        Schematron::compile_str(text).expect("schematron compile")
    }

    fn validate(sch: &Schematron, xml: &str) -> ValidationReport {
        sch.validate_str(xml).expect("validate")
    }

    // ── compile ─────────────────────────────────────────────

    #[test]
    fn compile_minimal_schema() {
        let s = compile(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="r">
                    <assert test="@x">missing x</assert>
                </rule>
            </pattern>
        </schema>"#);
        assert_eq!(s.patterns.len(), 1);
        assert_eq!(s.patterns[0].rules.len(), 1);
        assert_eq!(s.patterns[0].rules[0].asserts.len(), 1);
    }

    #[test]
    fn compile_rejects_non_schematron_root() {
        let err = Schematron::compile_str("<foo/>").unwrap_err();
        assert!(format!("{err}").contains("sch:schema"), "got: {err}");
    }

    #[test]
    fn accepts_old_namespace() {
        let s = compile(r#"<schema xmlns="http://www.ascc.net/xml/schematron">
            <pattern><rule context="*"><assert test="true()">ok</assert></rule></pattern>
        </schema>"#);
        assert_eq!(s.patterns.len(), 1);
    }

    // ── validate ───────────────────────────────────────────

    #[test]
    fn assert_fails_for_missing_attribute() {
        let s = compile(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="r">
                    <assert test="@x">missing x</assert>
                </rule>
            </pattern>
        </schema>"#);
        let r = validate(&s, "<r/>");
        assert!(!r.valid());
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].kind, FindingKind::FailedAssert);
        assert_eq!(r.findings[0].message, "missing x");
    }

    #[test]
    fn assert_passes_when_test_true() {
        let s = compile(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="r">
                    <assert test="@x">missing x</assert>
                </rule>
            </pattern>
        </schema>"#);
        let r = validate(&s, r#"<r x="42"/>"#);
        assert!(r.valid());
        assert!(r.findings.is_empty());
    }

    #[test]
    fn report_fires_when_test_true() {
        let s = compile(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="r">
                    <report test="@deprecated">element is deprecated</report>
                </rule>
            </pattern>
        </schema>"#);
        let r = validate(&s, r#"<r deprecated="yes"/>"#);
        // Reports don't make the doc invalid — only asserts do.
        assert!(r.valid());
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].kind, FindingKind::SuccessfulReport);
    }

    #[test]
    fn first_matching_rule_wins_per_pattern() {
        let s = compile(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="*[@kind='a']">
                    <assert test="false()">first rule</assert>
                </rule>
                <rule context="*">
                    <assert test="false()">second rule</assert>
                </rule>
            </pattern>
        </schema>"#);
        let r = validate(&s, r#"<r kind="a"/>"#);
        // First rule matches; second rule should NOT fire.
        let messages: Vec<_> = r.findings.iter().map(|f| f.message.clone()).collect();
        assert!(messages.iter().any(|m| m == "first rule"));
        assert!(!messages.iter().any(|m| m == "second rule"));
    }

    #[test]
    fn namespaces_resolve_via_sch_ns_declaration() {
        let s = compile(r##"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <ns prefix="dc" uri="http://purl.org/dc/elements/1.1/"/>
            <pattern>
                <rule context="dc:title">
                    <assert test="normalize-space(.) != ''">title is empty</assert>
                </rule>
            </pattern>
        </schema>"##);
        let r = validate(&s, r#"<r xmlns:dc="http://purl.org/dc/elements/1.1/">
            <dc:title></dc:title>
        </r>"#);
        assert!(!r.valid(), "empty title should fail assertion");
    }

    #[test]
    fn message_inlines_value_of() {
        let s = compile(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="r">
                    <assert test="@x">value: <value-of select="@y"/></assert>
                </rule>
            </pattern>
        </schema>"#);
        let r = validate(&s, r#"<r y="hello"/>"#);
        // x is missing → assert fails; message renders value-of @y.
        assert_eq!(r.findings[0].message, "value: hello");
    }

    #[test]
    fn message_inlines_name() {
        let s = compile(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="*">
                    <report test="@bad">element <name/> has bad attr</report>
                </rule>
            </pattern>
        </schema>"#);
        let r = validate(&s, r#"<foo bad="yes"/>"#);
        assert!(r.findings.iter().any(|f| f.message.contains("foo")));
    }

    #[test]
    fn pattern_id_propagates_to_findings() {
        let s = compile(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern id="check-001">
                <rule context="r">
                    <assert test="@x">missing x</assert>
                </rule>
            </pattern>
        </schema>"#);
        let r = validate(&s, "<r/>");
        assert_eq!(r.findings[0].pattern_id.as_deref(), Some("check-001"));
    }

    #[test]
    fn rule_let_provides_local_binding() {
        let s = compile(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="r">
                    <let name="count" value="count(item)"/>
                    <assert test="$count = 3">expected 3 items</assert>
                </rule>
            </pattern>
        </schema>"#);
        // 2 items → assert fails.
        let r = validate(&s, "<r><item/><item/></r>");
        assert!(!r.valid());
        // 3 items → passes.
        let r = validate(&s, "<r><item/><item/><item/></r>");
        assert!(r.valid());
    }

    // ── error paths ────────────────────────────────────────────

    #[test]
    fn rule_without_context_errors() {
        let r = Schematron::compile_str(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern><rule><assert test="true()">ok</assert></rule></pattern>
        </schema>"#);
        assert!(r.is_err());
        if let Err(XsltError::InvalidStylesheet(msg)) = r {
            assert!(msg.contains("context"), "got {msg}");
        }
    }

    #[test]
    fn let_without_name_errors() {
        let r = Schematron::compile_str(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern><rule context="r">
                <let value="@x"/>
                <assert test="true()">ok</assert>
            </rule></pattern>
        </schema>"#);
        assert!(r.is_err());
        if let Err(XsltError::InvalidStylesheet(msg)) = r {
            assert!(msg.contains("name"), "got {msg}");
        }
    }

    #[test]
    fn let_without_value_errors() {
        let r = Schematron::compile_str(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern><rule context="r">
                <let name="count"/>
                <assert test="true()">ok</assert>
            </rule></pattern>
        </schema>"#);
        assert!(r.is_err());
        if let Err(XsltError::InvalidStylesheet(msg)) = r {
            assert!(msg.contains("value"), "got {msg}");
        }
    }

    #[test]
    fn assert_without_test_errors() {
        let r = Schematron::compile_str(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern><rule context="r"><assert>missing</assert></rule></pattern>
        </schema>"#);
        assert!(r.is_err());
        if let Err(XsltError::InvalidStylesheet(msg)) = r {
            assert!(msg.contains("test"), "got {msg}");
            assert!(msg.contains("assert"), "got {msg}");
        }
    }

    #[test]
    fn report_without_test_errors() {
        let r = Schematron::compile_str(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern><rule context="r"><report>missing</report></rule></pattern>
        </schema>"#);
        assert!(r.is_err());
        if let Err(XsltError::InvalidStylesheet(msg)) = r {
            assert!(msg.contains("report"), "got {msg}");
        }
    }

    #[test]
    fn value_of_without_select_errors() {
        let r = Schematron::compile_str(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern><rule context="r">
                <assert test="true()">val=<value-of/></assert>
            </rule></pattern>
        </schema>"#);
        assert!(r.is_err());
    }

    // ── message inline element handling ────────────────────────

    #[test]
    fn message_inlines_emph_via_text_content() {
        // Non-value-of / non-name child element → fallback path
        // (`_ => {...}` arm in compile_message) that just stringifies
        // the child's text content.
        let s = compile(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="r">
                    <assert test="@x">missing <emph>x</emph> attribute</assert>
                </rule>
            </pattern>
        </schema>"#);
        let r = validate(&s, "<r/>");
        assert!(r.findings[0].message.contains("missing"));
        assert!(r.findings[0].message.contains("x"));
        assert!(r.findings[0].message.contains("attribute"));
    }

    #[test]
    fn name_with_path_attribute() {
        // <name path="@x"/> — use the optional path attribute (default ".").
        let s = compile(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="r">
                    <assert test="false()">node was <name path="."/></assert>
                </rule>
            </pattern>
        </schema>"#);
        let r = validate(&s, "<r/>");
        assert!(r.findings[0].message.contains("node was"));
        assert!(r.findings[0].message.contains("r"));
    }

    // ── rule unknown children are silently ignored ─────────────

    #[test]
    fn rule_ignores_unknown_children() {
        // <extension> isn't let/assert/report → silently skipped
        // (`_ => {}` arm in compile_rule).
        let s = compile(r#"<schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="r">
                    <extension/>
                    <assert test="@x">missing x</assert>
                </rule>
            </pattern>
        </schema>"#);
        let r = validate(&s, "<r/>");
        assert!(!r.valid());
        assert_eq!(r.findings[0].message, "missing x");
    }
}
