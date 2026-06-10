//! XSLT pattern matching — XSLT 1.0 §5.2.
//!
//! XSLT patterns (`match=`, `xsl:key match=`, `xsl:number
//! count=/from=`) are a restricted XPath grammar.  Rather than
//! implement a separate matcher, we compile patterns as full XPath
//! expressions and use the trick from the spec:
//!
//! > A node matches a pattern if there is some possible context
//! > such that evaluating the pattern as an expression with that
//! > context yields a node-set containing the node being matched.
//!
//! Concretely: walk the ancestor-or-self chain of the candidate
//! node, evaluating the pattern as an XPath at each one.  If any
//! evaluation produces a node-set containing the candidate, the
//! pattern matches.
//!
//! For default priority we look at the pattern's shape — a name
//! test `prefix:local` defaults to 0, `*` to -0.5, etc., per
//! §5.5.  A `|` union behaves as several template rules, one per
//! alternative (XSLT 2.0 §6.4): template selection flattens the
//! union via [`pattern_branches`] and evaluates each branch as its
//! own logical rule, with its own default priority.  `xsl:next-match`
//! can therefore step from one branch to another within the same
//! template before falling through to a different template.
//!
//! Template selection picks the best template per XSLT's
//! priority + import-precedence rules.  Currently a linear scan
//! across all templates; an index by innermost-step-name is a
//! known optimisation tracked for later.

use sup_xml_core::error::XmlError;
use sup_xml_core::xpath::ast::{Expr, LocationPath, NodeTest, Step};
use sup_xml_core::xpath::eval::{
    eval_expr, EvalCtx, NoBindings, StaticContext, Value, XPathBindings,
};
use sup_xml_core::xpath::{DocIndexLike, NodeId};

use crate::ast::{QName, StylesheetAst, Template};

type Result<T> = std::result::Result<T, XmlError>;

// ── match-ness check ──────────────────────────────────────────────

/// Does `pattern` match `node`?  Walks the ancestor-or-self chain
/// and evaluates the pattern at each context until either a match
/// is found or the chain is exhausted.
pub fn matches<I: DocIndexLike>(
    pattern: &Expr,
    node:    NodeId,
    idx:     &I,
    bindings: &dyn XPathBindings,
) -> Result<bool> {
    // Fresh step budget per pattern match — XSLT runs this once per
    // (template, candidate-node) pair and the cap is meant to bound a
    // single evaluation, not the cross-cutting sum.
    sup_xml_core::xpath::eval::reset_eval_budget();
    // See through the synthetic backwards-compat wrapper added in a
    // `version="1.0"` scope so the structural pattern handling below
    // (union split, document-node, single-step) sees the real shape.
    if let Expr::BackwardsCompat(inner) = pattern {
        return matches(inner, node, idx, bindings);
    }
    // XSLT 2.0 §5.5.3 — patterns may anchor on `document-node()`.
    // The "find some context that places `node` in the pattern's
    // result set" trick fails for these shapes (no XPath
    // `child::document-node()` step can ever reach a document node).
    // Handle them structurally here.  Union patterns get split into
    // their branches and each is matched independently so other
    // branches keep their normal eval-walk semantics.
    if let Expr::Union(l, r) = pattern {
        if matches(l, node, idx, bindings)? { return Ok(true); }
        return matches(r, node, idx, bindings);
    }
    if pattern_is_document_node(pattern) {
        return Ok(matches!(idx.kind(node),
            sup_xml_core::xpath::XPathNodeKind::Document));
    }
    // `document-node(element(N))` / `document-node(element(*))` — like
    // the bare form, no `child::` step reaches a document node, so
    // match structurally: the node must be a document node whose
    // document element satisfies the inner test.
    if let Some(Some(inner)) = single_step_document_test(pattern) {
        if !matches!(idx.kind(node), sup_xml_core::xpath::XPathNodeKind::Document) {
            return Ok(false);
        }
        return Ok(idx.children(node).iter().any(|c|
            matches!(idx.kind(*c), sup_xml_core::xpath::XPathNodeKind::Element)
            && sup_xml_core::xpath::eval::node_matches_child(*c, inner, idx, bindings)));
    }
    // `document-node()/REST` reads as "REST anchored to the document
    // node".  Since every node lives under a document anyway, that
    // anchor is a tautology in our trees — evaluate REST as an
    // ordinary absolute path.  This rewrite handles patterns like
    // `document-node()/child::element()` that the ancestor walk
    // otherwise can't satisfy (because no XPath child:: step ever
    // produces a document node).
    if let Some(rest) = rewrite_document_node_prefix(pattern) {
        return matches(&rest, node, idx, bindings);
    }
    // Walk node, parent(node), parent(parent(node))…
    let sc = StaticContext {
        xpath_2_0: bindings.xpath_version_2_or_later(),
        xpath_3_0: false,
        libxml2_compatible: false, current_node: None,
    };
    let mut cur = Some(node);
    while let Some(ctx_node) = cur {
        let ctx = EvalCtx {
            context_node: ctx_node, pos: 1, size: 1, bindings, static_ctx: &sc };
        let v = eval_expr(pattern, &ctx, idx)?;
        if let Value::NodeSet(ns) = v {
            if ns.contains(&node) {
                return Ok(true);
            }
        }
        cur = idx.parent(ctx_node);
    }
    Ok(false)
}

/// If `p` is `document-node()/REST` (a relative path whose first
/// step is `document-node()`), return REST as an absolute path
/// (rooted at the document).  Returns `None` for other shapes.
fn rewrite_document_node_prefix(p: &Expr) -> Option<Expr> {
    if let Expr::Path(LocationPath::Relative(steps)) = p {
        if steps.len() >= 2
            && matches!(&steps[0].node_test, NodeTest::Document(None))
            && steps[0].predicates.is_empty()
        {
            return Some(Expr::Path(LocationPath::Absolute(steps[1..].to_vec())));
        }
    }
    None
}

/// If `p` is a single-step `document-node(...)` test with no
/// predicates, return its inner element test — `&None` for the bare
/// `document-node()`, `&Some(t)` for `document-node(element(t))`.
/// Returns `None` for any other pattern shape.
fn single_step_document_test(p: &Expr) -> Option<&Option<Box<NodeTest>>> {
    let step = single_step_pattern(p)?;
    if !step.predicates.is_empty() {
        return None;
    }
    match &step.node_test {
        NodeTest::Document(inner) => Some(inner),
        _ => None,
    }
}

/// Detect a pattern whose only effect is `document-node()` — either
/// a bare `document-node()`, or a union one of whose branches is.
fn pattern_is_document_node(p: &Expr) -> bool {
    fn step_is_doc(s: &Step) -> bool {
        matches!(&s.node_test, NodeTest::Document(None)) && s.predicates.is_empty()
    }
    match p {
        Expr::Path(LocationPath::Relative(s)) if s.len() == 1
            => step_is_doc(&s[0]),
        Expr::Path(LocationPath::Absolute(s)) if s.is_empty() => true,
        Expr::Union(a, b) => pattern_is_document_node(a) || pattern_is_document_node(b),
        _ => false,
    }
}

// ── default priority (XSLT 1.0 §5.5) ──────────────────────────────

/// Compute the default priority for a pattern AST node.  The XSLT
/// spec defines four buckets:
///
/// | Pattern shape                                       | Priority |
/// |-----------------------------------------------------|----------|
/// | `prefix:local`, `@prefix:local`                     |   0      |
/// | `NCName:*`, `@NCName:*`                             |  -0.25   |
/// | `*`, `@*`, `node()`, `text()`, `comment()`, `pi()`  |  -0.5    |
/// | anything more specific (paths, predicates, etc.)    |   0.5    |
pub fn default_priority(pattern: &Expr) -> f64 {
    // See through the synthetic backwards-compat wrapper the compiler
    // adds in a `version="1.0"` scope — it carries no structural
    // information relevant to the pattern's default priority.
    if let Expr::BackwardsCompat(inner) = pattern {
        return default_priority(inner);
    }
    // Unions: each branch independently; take the max.
    if let Expr::Union(l, r) = pattern {
        return default_priority(l).max(default_priority(r));
    }
    // XSLT 2.0 §6.4 — `match="/"` (and `document-node()`) default
    // to priority -0.5, the same bucket as `*` and `node()`.
    if pattern_is_document_node(pattern) {
        return -0.5;
    }
    let single_step = single_step_pattern(pattern);
    match single_step {
        Some(step) if step.predicates.is_empty() => match &step.node_test {
            NodeTest::QName(_, _)            => 0.0,
            NodeTest::DefaultNamespaceName { .. } => 0.0,
            NodeTest::PrefixWildcard(_)      => -0.25,
            // XPath 2.0 `*:NCName` — half-bound: a specific local
            // name in any namespace.  Less specific than a full
            // QName, more specific than `prefix:*`.
            NodeTest::LocalNameOnly(_)       => -0.25,
            NodeTest::LocalName(_)           => 0.0,
            // node() / text() / comment() / pi() / *  — the
            // least-specific patterns.
            NodeTest::AnyNode | NodeTest::Wildcard
                | NodeTest::Text | NodeTest::Comment
                | NodeTest::PI(None) => -0.5,
            // `document-node()` and `document-node(element(*))` are
            // the least-specific (-0.5); `document-node(element(N))`
            // carries the name's specificity, so it gets the priority
            // of the inner element name test (XSLT 2.0 §6.4).
            NodeTest::Document(inner) => match inner.as_deref() {
                Some(NodeTest::QName(..))
                    | Some(NodeTest::DefaultNamespaceName { .. })
                    | Some(NodeTest::LocalName(_)) => 0.0,
                Some(NodeTest::PrefixWildcard(_))
                    | Some(NodeTest::LocalNameOnly(_)) => -0.25,
                _ => -0.5,
            },
            // pi('target') is more specific than pi() — gets 0.
            NodeTest::PI(Some(_)) => 0.0,
        },
        _ => 0.5,
    }
}

/// If `expr` is a one-step location path (with or without an
/// absolute root), return that step.  XSLT patterns with predicates
/// or multiple steps don't qualify as "simple" so they get the
/// more-specific default priority.
fn single_step_pattern(expr: &Expr) -> Option<&Step> {
    match expr {
        Expr::Path(lp) => {
            let steps = match lp {
                LocationPath::Absolute(s) | LocationPath::Relative(s) => s,
            };
            if steps.len() == 1 { Some(&steps[0]) } else { None }
        }
        _ => None,
    }
}

// ── template selection ────────────────────────────────────────────

/// Result of looking up a template: a borrow into the stylesheet's
/// template list plus the effective priority used to break ties.
///
/// XSLT 2.0 §6.4: when a template has a union pattern `A|B`, the
/// rule behaves as if each operand were a separate template rule
/// with the same body and source position.  `branch_idx` records
/// which operand actually matched the node — `Some(i)` indexes the
/// flattened operand list (see [`pattern_branches`]); `None`
/// indicates a non-union pattern.  `xsl:next-match` uses this to
/// pick up other branches of the same template before falling
/// through to templates with lower precedence/priority.
pub struct Selected<'a> {
    pub template:   &'a Template,
    pub priority:   f64,
    pub branch_idx: Option<usize>,
}

/// Flatten a pattern's outermost union into its operands.  Returns
/// `[pat]` for non-union patterns, `[A, B, …]` (in source order) for
/// `A|B|…`.  Sees through the synthetic `BackwardsCompat` wrapper
/// the compiler adds in a `version="1.0"` scope, but does not look
/// inside nested expressions (only the union shape at the top
/// matters for §6.4 splitting).
pub fn pattern_branches(pat: &Expr) -> Vec<&Expr> {
    fn walk<'a>(p: &'a Expr, out: &mut Vec<&'a Expr>) {
        match p {
            Expr::BackwardsCompat(inner) => walk(inner, out),
            Expr::Union(l, r) => { walk(l, out); walk(r, out); }
            _ => out.push(p),
        }
    }
    let mut out = Vec::new();
    walk(pat, &mut out);
    out
}

/// Find the best-matching template for `node` under `mode`.  XSLT
/// 1.0 conflict resolution (§5.5):
///
/// 1. Filter to templates with `match=` that match the node and
///    whose mode matches.
/// 2. Within that set, pick the highest effective priority
///    (explicit `priority=` if present, else default).
/// 3. Ties break by document order — last wins.
///
/// (Import precedence — the additional dimension from
/// `xsl:import` — lands when the include/import resolver does;
/// for now every template is at the same precedence.)
pub fn select_template<'a, I: DocIndexLike>(
    style:    &'a StylesheetAst,
    node:     NodeId,
    mode:     Option<&QName>,
    idx:      &I,
    bindings: &dyn XPathBindings,
) -> Result<Option<Selected<'a>>> {
    select_template_inner(style, node, mode, idx, bindings, None)
}

/// Variant of [`select_template`] used by `xsl:apply-imports`:
/// limits candidates to templates whose `import_precedence` is
/// at most `max_precedence`.  Conflict resolution within that
/// pool follows the same rules.
pub fn select_template_max_precedence<'a, I: DocIndexLike>(
    style:           &'a StylesheetAst,
    node:            NodeId,
    mode:            Option<&QName>,
    idx:             &I,
    bindings:        &dyn XPathBindings,
    max_precedence:  i32,
) -> Result<Option<Selected<'a>>> {
    select_template_inner(style, node, mode, idx, bindings, Some(max_precedence))
}

/// XSLT 2.0 §6.7 `xsl:next-match` — pick the next template in the
/// conflict-resolution order after `current`.  Conflict order is
/// (precedence descending, priority descending, source position
/// descending — last in source wins ties); "next" means strictly
/// less along that order than `current`.  Returns `None` when no
/// such template matches the node + mode.
///
/// Union patterns participate per §6.4: each operand acts as a
/// separate logical rule, so `xsl:next-match` may select another
/// branch of the *same* template (sharing its body) before falling
/// through to a different template.  Within a single template, the
/// branch index acts as a secondary source position — later branches
/// are treated as later in source order for tie-breaking.
pub fn select_template_next<'a, I: DocIndexLike>(
    style:    &'a StylesheetAst,
    node:     NodeId,
    mode:     Option<&QName>,
    idx:      &I,
    bindings: &dyn XPathBindings,
    current:  &Selected<'_>,
    current_index: usize,
) -> Result<Option<Selected<'a>>> {
    let cur_prec = current.template.import_precedence;
    let cur_prio = current.priority;
    let cur_path = current.template.source_path.as_slice();
    let cur_branch = current.branch_idx;
    let mut best: Option<Selected<'a>> = None;
    let mut best_path: &[u32] = &[];
    let mut best_branch: Option<usize> = None;
    for (i, t) in style.templates.iter().enumerate() {
        let Some(pat) = t.match_pattern.as_ref() else { continue; };
        if !template_mode_matches(t, mode) { continue; }
        let branches = pattern_branches(pat);
        let multi = branches.len() > 1;
        for (b, branch_pat) in branches.iter().enumerate() {
            // The current (template, branch) is itself excluded
            // from next-match — the body has already run once for it.
            if i == current_index && (!multi || Some(b) == cur_branch) {
                continue;
            }
            if !matches(branch_pat, node, idx, bindings)? { continue; }
            let priority = match t.priority {
                Some(p) => p,
                None => default_priority(branch_pat),
            };
            let branch_idx = if multi { Some(b) } else { None };
            // Strict "less than current" in conflict-resolution order:
            // either lower precedence, or same precedence + lower
            // priority, or same precedence + same priority + earlier
            // (template source path, branch index).
            let prec = t.import_precedence;
            let path = t.source_path.as_slice();
            let strictly_after_current = if prec != cur_prec {
                prec < cur_prec
            } else if (priority - cur_prio).abs() > f64::EPSILON {
                priority < cur_prio
            } else if path != cur_path {
                path < cur_path
            } else {
                // Same template (different branch).
                branch_idx < cur_branch
            };
            if !strictly_after_current { continue; }
            let take = match &best {
                None => true,
                Some(bs) => {
                    let bprec = bs.template.import_precedence;
                    if prec != bprec {
                        prec > bprec
                    } else if (priority - bs.priority).abs() > f64::EPSILON {
                        priority > bs.priority
                    } else if path != best_path {
                        path > best_path
                    } else {
                        branch_idx > best_branch
                    }
                }
            };
            if take {
                best = Some(Selected { template: t, priority, branch_idx });
                best_path = path;
                best_branch = branch_idx;
            }
        }
    }
    Ok(best)
}

fn select_template_inner<'a, I: DocIndexLike>(
    style:           &'a StylesheetAst,
    node:            NodeId,
    mode:            Option<&QName>,
    idx:             &I,
    bindings:        &dyn XPathBindings,
    max_precedence:  Option<i32>,
) -> Result<Option<Selected<'a>>> {
    let mut best: Option<Selected<'a>> = None;
    let mut best_path: &[u32] = &[];
    let mut best_branch: Option<usize> = None;
    // Whether ≥2 distinct rules share the winning (precedence, priority)
    // tier — an unresolved conflict (XSLT 1.0 §5.5).  Reset whenever the
    // winner moves to a strictly higher tier.
    let mut multiple = false;
    for t in style.templates.iter() {
        if let Some(cap) = max_precedence {
            if t.import_precedence > cap { continue; }
        }
        // Only templates with match= participate in pattern-based
        // selection (`name=`-only templates are call-targets).
        let Some(pat) = t.match_pattern.as_ref() else { continue; };
        if !template_mode_matches(t, mode) { continue; }
        // XSLT 2.0 §6.4 — each union operand is a logical template
        // rule with its own default priority.  For non-union patterns
        // `pattern_branches` yields a single entry.
        let branches = pattern_branches(pat);
        let multi = branches.len() > 1;
        for (b, branch_pat) in branches.iter().enumerate() {
            if !matches(branch_pat, node, idx, bindings)? { continue; }
            let priority = match t.priority {
                Some(p) => p,
                None => default_priority(branch_pat),
            };
            let branch_idx = if multi { Some(b) } else { None };
            // Conflict resolution per XSLT 1.0 §5.5 / 2.0 §6.4:
            // 1. Highest import precedence wins.
            // 2. Highest priority wins (within the same precedence).
            // 3. Last in (include-aware) source order wins — with
            //    branch index as a secondary source position so later
            //    union operands beat earlier ones on the same template.
            let (take, tie) = match &best {
                None => (true, false),
                Some(bs) => {
                    let prec  = t.import_precedence;
                    let bprec = bs.template.import_precedence;
                    let path = t.source_path.as_slice();
                    if prec != bprec {
                        (prec > bprec, false)
                    } else if (priority - bs.priority).abs() > f64::EPSILON {
                        (priority > bs.priority, false)
                    } else if path != best_path {
                        (path > best_path, true)
                    } else {
                        // Same template, different branch — strictly
                        // later branch wins; identical (i, b) can't
                        // occur because we iterate distinct positions.
                        (branch_idx > best_branch, true)
                    }
                }
            };
            // Moving to a strictly higher tier clears any earlier tie;
            // a same-tier match (whoever wins source-order) records one.
            if take && !tie { multiple = false; }
            if tie { multiple = true; }
            if take {
                best = Some(Selected { template: t, priority, branch_idx });
                best_path = t.source_path.as_slice();
                best_branch = branch_idx;
            }
        }
    }
    // XSLT 1.0 §5.5 / XTRE0540 — when configured to report (rather than
    // recover from) an unresolved conflict, a tie at the winning tier
    // is a dynamic error.
    if multiple && on_multiple_match_is_error() {
        return Err(sup_xml_core::xpath::eval::xpath_err(
            "more than one template rule matches the node with the same \
             import precedence and priority"
        ).with_xpath_code("XTRE0540"));
    }
    Ok(best)
}

thread_local! {
    /// Per-thread switch: when set, an unresolved template conflict is
    /// REPORTED as XTRE0540 rather than recovered from (use-last).  The
    /// host sets it around an apply (e.g. the W3C harness honouring an
    /// `on-multiple-match="error"` dependency); default is to recover.
    static ON_MULTIPLE_MATCH_ERROR: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

fn on_multiple_match_is_error() -> bool {
    ON_MULTIPLE_MATCH_ERROR.with(|c| c.get())
}

/// Set whether unresolved template conflicts are reported (XTRE0540)
/// instead of recovered.  Returns the previous value so the caller can
/// restore it after the apply.
pub fn set_on_multiple_match_error(v: bool) -> bool {
    ON_MULTIPLE_MATCH_ERROR.with(|c| c.replace(v))
}

fn mode_matches(template_mode: Option<&QName>, requested: Option<&QName>) -> bool {
    match (template_mode, requested) {
        (None, None) => true,
        (Some(a), Some(b)) => a.uri == b.uri && a.local == b.local,
        _ => false,
    }
}

/// XSLT 2.0 §6 — a template participates in mode `requested` when:
/// * it declared `mode="#all"`, OR
/// * the requested mode is in its `modes` list (an empty-name
///   QName represents `#default`).
/// XSLT 1.0 templates with no `mode=` attribute keep matching only
/// the default mode (the `modes` vec is empty and `mode` is None).
fn template_mode_matches(t: &crate::ast::Template, requested: Option<&QName>) -> bool {
    if t.modes_match_all { return true; }
    if t.modes.is_empty() {
        // Legacy / single-mode shape: empty list + None mode means
        // "default mode only".
        return mode_matches(t.mode.as_ref(), requested);
    }
    let is_default = |q: &QName| q.local.is_empty() && q.uri.is_empty();
    match requested {
        None    => t.modes.iter().any(is_default),
        Some(r) => t.modes.iter().any(|m|
            !is_default(m) && m.uri == r.uri && m.local == r.local),
    }
}

// ── public helpers ────────────────────────────────────────────────

/// Convenience entry point for callers that don't need to thread
/// custom XPath bindings (e.g. simple stylesheets with no `xsl:key`
/// references in the matched patterns).  Uses `NoBindings`.
pub fn select_template_no_bindings<'a, I: DocIndexLike>(
    style: &'a StylesheetAst,
    node:  NodeId,
    mode:  Option<&QName>,
    idx:   &I,
) -> Result<Option<Selected<'a>>> {
    select_template(style, node, mode, idx, &NoBindings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stylesheet;
    use sup_xml_core::{parse_str, ParseOptions, XPathContext};

    fn build_stylesheet(body: &str) -> Stylesheet {
        let text = format!(
            r#"<xsl:stylesheet version="1.0"
                  xmlns:xsl="http://www.w3.org/1999/XSL/Transform">{body}</xsl:stylesheet>"#,
        );
        Stylesheet::compile_str(&text).unwrap()
    }

    fn doc_ns(xml: &str) -> sup_xml_tree::dom::Document {
        let opts = ParseOptions { namespace_aware: true, ..ParseOptions::default() };
        parse_str(xml, &opts).unwrap()
    }

    // ── default-priority table (XSLT 1.0 §5.5) ───────────────

    #[test]
    fn priority_star_is_negative_half() {
        let xslt = build_stylesheet(r#"<xsl:template match="*"/>"#);
        let p = default_priority(xslt.ast.templates[0].match_pattern.as_ref().unwrap());
        assert_eq!(p, -0.5);
    }

    #[test]
    fn priority_named_element_is_zero() {
        let xslt = build_stylesheet(r#"<xsl:template match="book"/>"#);
        let p = default_priority(xslt.ast.templates[0].match_pattern.as_ref().unwrap());
        assert_eq!(p, 0.0);
    }

    #[test]
    fn priority_node_test_is_negative_half() {
        let xslt = build_stylesheet(r#"<xsl:template match="text()"/>"#);
        let p = default_priority(xslt.ast.templates[0].match_pattern.as_ref().unwrap());
        assert_eq!(p, -0.5);
    }

    #[test]
    fn priority_path_is_half() {
        let xslt = build_stylesheet(r#"<xsl:template match="book/chapter"/>"#);
        let p = default_priority(xslt.ast.templates[0].match_pattern.as_ref().unwrap());
        assert_eq!(p, 0.5);
    }

    #[test]
    fn priority_predicate_is_half() {
        let xslt = build_stylesheet(r#"<xsl:template match="book[@x]"/>"#);
        let p = default_priority(xslt.ast.templates[0].match_pattern.as_ref().unwrap());
        assert_eq!(p, 0.5);
    }

    // ── match-ness ──────────────────────────────────────────

    #[test]
    fn root_pattern_matches_document_node() {
        let xslt = build_stylesheet(r#"<xsl:template match="/"/>"#);
        let pat = xslt.ast.templates[0].match_pattern.as_ref().unwrap();
        let doc = doc_ns("<r/>");
        let ctx = XPathContext::new(&doc);
        // Node 0 in the index is the synthetic document node.
        assert!(matches(pat, 0, &ctx.index, &NoBindings).unwrap());
    }

    #[test]
    fn star_pattern_matches_elements() {
        let xslt = build_stylesheet(r#"<xsl:template match="*"/>"#);
        let pat = xslt.ast.templates[0].match_pattern.as_ref().unwrap();
        let doc = doc_ns("<r><a/></r>");
        let ctx = XPathContext::new(&doc);
        // Element <r> exists somewhere in the index; query via XPath
        // to find its NodeId.
        let v = ctx.eval("/r").unwrap();
        let r_id = match v {
            Value::NodeSet(ns) => ns[0],
            _ => panic!(),
        };
        assert!(matches(pat, r_id, &ctx.index, &NoBindings).unwrap());
    }

    #[test]
    fn named_pattern_only_matches_that_name() {
        let xslt = build_stylesheet(r#"<xsl:template match="book"/>"#);
        let pat = xslt.ast.templates[0].match_pattern.as_ref().unwrap();
        let doc = doc_ns("<r><book/><article/></r>");
        let ctx = XPathContext::new(&doc);
        let book_id = match ctx.eval("/r/book").unwrap() {
            Value::NodeSet(ns) => ns[0], _ => panic!(),
        };
        let art_id = match ctx.eval("/r/article").unwrap() {
            Value::NodeSet(ns) => ns[0], _ => panic!(),
        };
        assert!(matches(pat,  book_id, &ctx.index, &NoBindings).unwrap());
        assert!(!matches(pat, art_id,  &ctx.index, &NoBindings).unwrap());
    }

    #[test]
    fn path_pattern_walks_ancestors() {
        // match="book/chapter" must match a <chapter> whose parent is
        // <book>.  Evaluating "book/chapter" against the chapter node
        // directly returns nothing; the matcher's ancestor walk reaches
        // the book element, evaluates from there, and finds the
        // chapter in the result set.
        let xslt = build_stylesheet(r#"<xsl:template match="book/chapter"/>"#);
        let pat = xslt.ast.templates[0].match_pattern.as_ref().unwrap();
        let doc = doc_ns("<root><book><chapter/></book></root>");
        let ctx = XPathContext::new(&doc);
        let chap_id = match ctx.eval("/root/book/chapter").unwrap() {
            Value::NodeSet(ns) => ns[0], _ => panic!(),
        };
        assert!(matches(pat, chap_id, &ctx.index, &NoBindings).unwrap());
    }

    #[test]
    fn path_pattern_does_not_match_outside_path() {
        let xslt = build_stylesheet(r#"<xsl:template match="book/chapter"/>"#);
        let pat = xslt.ast.templates[0].match_pattern.as_ref().unwrap();
        // chapter directly inside root, not inside book — shouldn't match.
        let doc = doc_ns("<root><chapter/></root>");
        let ctx = XPathContext::new(&doc);
        let chap_id = match ctx.eval("/root/chapter").unwrap() {
            Value::NodeSet(ns) => ns[0], _ => panic!(),
        };
        assert!(!matches(pat, chap_id, &ctx.index, &NoBindings).unwrap());
    }

    // ── template selection ──────────────────────────────────

    #[test]
    fn selects_higher_default_priority() {
        // Two templates, both match an <a> element.  match="a" has
        // priority 0, match="*" has priority -0.5.  The named one
        // wins.
        let xslt = build_stylesheet(r#"
            <xsl:template match="*"><star/></xsl:template>
            <xsl:template match="a"><named/></xsl:template>
        "#);
        let doc = doc_ns("<r><a/></r>");
        let ctx = XPathContext::new(&doc);
        let a_id = match ctx.eval("/r/a").unwrap() {
            Value::NodeSet(ns) => ns[0], _ => panic!(),
        };
        let sel = select_template_no_bindings(&xslt.ast, a_id, None, &ctx.index).unwrap().unwrap();
        // Named template body emits <named/>.
        match &sel.template.body[0] {
            crate::ast::Instr::LiteralElement { name, .. } => assert_eq!(name.local, "named"),
            other => panic!("expected LiteralElement, got {other:?}"),
        }
    }

    #[test]
    fn explicit_priority_overrides_default() {
        let xslt = build_stylesheet(r#"
            <xsl:template match="*" priority="10"><high/></xsl:template>
            <xsl:template match="a"><low/></xsl:template>
        "#);
        let doc = doc_ns("<r><a/></r>");
        let ctx = XPathContext::new(&doc);
        let a_id = match ctx.eval("/r/a").unwrap() {
            Value::NodeSet(ns) => ns[0], _ => panic!(),
        };
        let sel = select_template_no_bindings(&xslt.ast, a_id, None, &ctx.index).unwrap().unwrap();
        match &sel.template.body[0] {
            crate::ast::Instr::LiteralElement { name, .. } => assert_eq!(name.local, "high"),
            other => panic!("expected LiteralElement, got {other:?}"),
        }
    }

    #[test]
    fn ties_break_by_document_order_last_wins() {
        // Two identical-priority templates both matching `*`.
        // Document order: first one, then second one.  Spec says
        // last-in-document-order wins.
        let xslt = build_stylesheet(r#"
            <xsl:template match="*"><first/></xsl:template>
            <xsl:template match="*"><second/></xsl:template>
        "#);
        let doc = doc_ns("<r/>");
        let ctx = XPathContext::new(&doc);
        let r_id = match ctx.eval("/r").unwrap() {
            Value::NodeSet(ns) => ns[0], _ => panic!(),
        };
        let sel = select_template_no_bindings(&xslt.ast, r_id, None, &ctx.index).unwrap().unwrap();
        match &sel.template.body[0] {
            crate::ast::Instr::LiteralElement { name, .. } => assert_eq!(name.local, "second"),
            other => panic!("expected LiteralElement, got {other:?}"),
        }
    }

    #[test]
    fn no_match_returns_none() {
        let xslt = build_stylesheet(r#"<xsl:template match="book"/>"#);
        let doc = doc_ns("<r><article/></r>");
        let ctx = XPathContext::new(&doc);
        let art_id = match ctx.eval("/r/article").unwrap() {
            Value::NodeSet(ns) => ns[0], _ => panic!(),
        };
        let sel = select_template_no_bindings(&xslt.ast, art_id, None, &ctx.index).unwrap();
        assert!(sel.is_none());
    }

    #[test]
    fn mode_filters_templates() {
        let xslt = build_stylesheet(r#"
            <xsl:template match="a"><default/></xsl:template>
            <xsl:template match="a" mode="big"><big/></xsl:template>
        "#);
        let doc = doc_ns("<r><a/></r>");
        let ctx = XPathContext::new(&doc);
        let a_id = match ctx.eval("/r/a").unwrap() {
            Value::NodeSet(ns) => ns[0], _ => panic!(),
        };
        // No mode → matches the unmoded template.
        let sel = select_template_no_bindings(&xslt.ast, a_id, None, &ctx.index).unwrap().unwrap();
        match &sel.template.body[0] {
            crate::ast::Instr::LiteralElement { name, .. } => assert_eq!(name.local, "default"),
            _ => panic!(),
        }
        // Mode "big" → matches the moded template.
        let big = QName { prefix: None, local: "big".into(), uri: String::new() };
        let sel = select_template_no_bindings(&xslt.ast, a_id, Some(&big), &ctx.index).unwrap().unwrap();
        match &sel.template.body[0] {
            crate::ast::Instr::LiteralElement { name, .. } => assert_eq!(name.local, "big"),
            _ => panic!(),
        }
    }

    // ── pattern_is_document_node / rewrite_document_node_prefix ──

    fn parse_pat(src: &str) -> Expr {
        sup_xml_core::xpath::parse_xpath_with(src,
            &sup_xml_core::xpath::XPathOptions {
                xpath_2_0: true, libxml2_compatible: false,
                ..sup_xml_core::xpath::XPathOptions::default()
            }).unwrap()
    }

    #[test]
    fn detects_bare_document_node_pattern() {
        assert!(super::pattern_is_document_node(&parse_pat("document-node()")));
        // Absolute `/` parses to an empty-step absolute path — also
        // a document-node anchor in XSLT pattern semantics.
        assert!(super::pattern_is_document_node(&parse_pat("/")));
    }

    #[test]
    fn detects_document_node_branch_inside_union() {
        // Either branch matching is enough — used by `* | /`.
        assert!(super::pattern_is_document_node(&parse_pat("* | /")));
        assert!(super::pattern_is_document_node(&parse_pat("/ | foo")));
    }

    #[test]
    fn rejects_non_document_node_patterns() {
        assert!(!super::pattern_is_document_node(&parse_pat("foo")));
        assert!(!super::pattern_is_document_node(&parse_pat("element()")));
        assert!(!super::pattern_is_document_node(&parse_pat("foo/bar")));
    }

    #[test]
    fn rewrites_document_node_slash_rest() {
        // `document-node()/element()` should rewrite to an absolute
        // path containing only the element() step.
        let p = parse_pat("document-node()/element()");
        let rest = super::rewrite_document_node_prefix(&p).expect("should rewrite");
        match rest {
            Expr::Path(LocationPath::Absolute(steps)) => {
                assert_eq!(steps.len(), 1);
                assert!(matches!(steps[0].node_test, NodeTest::Wildcard | NodeTest::AnyNode));
            }
            other => panic!("expected absolute path, got {other:?}"),
        }
    }

    #[test]
    fn does_not_rewrite_bare_or_unrelated() {
        // Bare `document-node()` — no rest to rewrite.
        assert!(super::rewrite_document_node_prefix(&parse_pat("document-node()")).is_none());
        // Path that doesn't start with document-node().
        assert!(super::rewrite_document_node_prefix(&parse_pat("element()/foo")).is_none());
    }

    // ── default_priority for `/` and `document-node()` ───────────

    #[test]
    fn default_priority_for_document_node_patterns() {
        // XSLT 2.0 §6.4 — `/` and `document-node()` are the lowest
        // bucket alongside `*` / `node()`.
        assert_eq!(super::default_priority(&parse_pat("/")),               -0.5);
        assert_eq!(super::default_priority(&parse_pat("document-node()")), -0.5);
    }
}
