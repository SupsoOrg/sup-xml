//! XSLT 3.0 streaming (Chapter 19).
//!
//! Streaming evaluates a transform against a forward-only event stream
//! of the source document, holding only a bounded window in memory —
//! the current node, its attributes/namespaces, and its ancestors —
//! rather than materializing the whole tree.  A processor that claims
//! streaming support must, per the spec, *statically* reject any
//! construct it cannot stream rather than silently building the tree.
//!
//! This module is organized in two layers:
//!
//! * [`analysis`] is the posture-and-sweep classifier over the XPath
//!   AST (XSLT 3.0 §19.8): the pure, expression-level rules that decide
//!   whether a selection stays within the streaming window.
//!
//! * This file lifts that classifier to the XSLT instruction set — the
//!   sequence-constructor instructions (`xsl:apply-templates`,
//!   `xsl:for-each`, `xsl:copy-of`, …) and their nesting rules — and
//!   exposes [`validate_streamability`], the compile-time gate that
//!   walks every streamable context (a `streamable="yes"` mode's
//!   template rules, `xsl:source-document streamable="yes"`, and
//!   `xsl:accumulator streamable="yes"`) and rejects the stylesheet
//!   with `XTSE3430` when a construct is not guaranteed-streamable.
//!
//! The classifier is conservative: constructs it cannot prove
//! streamable are rejected, never mis-streamed.

pub mod analysis;
pub mod engine;
pub mod push;
pub mod push_eval;

pub use engine::RecordSelector;
pub use push::stream_copy;

use crate::ast::{
    AccumulatorDecl, Avt, AvtPart, Body, Instr, ModeDecl, QName, StylesheetAst,
    Template, WithParam,
};
use crate::error::XsltError;
use analysis::{
    analyze_expr, apply_axis, combine_max, combine_strict, Posture, Ps, Sweep,
};
use sup_xml_core::xpath::Expr;

/// The static error code for "the stylesheet is not streamable" — the
/// XSLT 3.0 §19 family covering a construct used in a streamable context
/// that the streamability rules forbid.
const XTSE3430: &str = "XTSE3430";

fn not_streamable(what: &str) -> XsltError {
    XsltError::InvalidStylesheet(format!(
        "{what} is not guaranteed-streamable ({XTSE3430})"
    ))
}

/// Two modes are the same for streamability purposes when their expanded
/// names (namespace URI + local part) agree.  [`QName`] carries a
/// lexical prefix too, which is irrelevant to mode identity.
fn same_mode(a: &QName, b: &QName) -> bool {
    a.uri == b.uri && a.local == b.local
}

fn is_default_mode_qname(q: &QName) -> bool {
    q.uri.is_empty() && q.local.is_empty()
}

/// Validate every streamable context in the stylesheet, rejecting the
/// whole stylesheet (XTSE3430) if any contains a construct that is not
/// guaranteed-streamable.  A no-op for stylesheets that declare nothing
/// streamable.
pub fn validate_streamability(ast: &StylesheetAst) -> Result<(), XsltError> {
    validate_streamable_modes(ast)?;
    validate_streamable_accumulators(ast)?;

    // `xsl:source-document streamable="yes"` can appear in any template,
    // global-variable, or function body regardless of mode, so walk the
    // whole instruction forest for it.
    for t in &ast.templates {
        scan_source_documents(&t.body)?;
    }
    for v in &ast.global_variables {
        scan_source_documents(&v.body)?;
    }
    for f in &ast.functions {
        scan_source_documents(&f.body)?;
    }
    Ok(())
}

/// Validate the template rules of every `streamable="yes"` mode.  Each
/// rule's body is evaluated with the matched node as context (posture
/// striding), so the body's sequence constructor must be streamable
/// there.
fn validate_streamable_modes(ast: &StylesheetAst) -> Result<(), XsltError> {
    let streamable_named: Vec<&ModeDecl> =
        ast.modes.iter().filter(|m| m.streamable && m.name.is_some()).collect();
    let default_streamable =
        ast.modes.iter().any(|m| m.streamable && m.name.is_none());

    if streamable_named.is_empty() && !default_streamable {
        return Ok(());
    }

    for t in &ast.templates {
        // Named templates with no match pattern are call-targets, not
        // rules in a mode.
        if t.match_pattern.is_none() {
            continue;
        }
        if template_in_streamable_mode(t, &streamable_named, default_streamable) {
            require_body_streamable(
                &t.body,
                Posture::Striding,
                "template rule in a streamable mode",
            )?;
        }
    }
    Ok(())
}

/// Whether a template rule participates in any streamable mode.
fn template_in_streamable_mode(
    t: &Template,
    streamable_named: &[&ModeDecl],
    default_streamable: bool,
) -> bool {
    // `mode="#all"` puts the rule in every mode, including streamable
    // ones.
    if t.modes_match_all && (default_streamable || !streamable_named.is_empty()) {
        return true;
    }
    let in_default = t.modes.is_empty() || t.modes.iter().any(is_default_mode_qname);
    if default_streamable && in_default {
        return true;
    }
    t.modes.iter().any(|tm| {
        streamable_named
            .iter()
            .any(|m| m.name.as_ref().is_some_and(|n| same_mode(n, tm)))
    })
}

/// Validate the rules of every `streamable="yes"` accumulator.  An
/// accumulator rule's `select`/body is evaluated with the matched node
/// as context (striding); the once-only `initial-value` runs before the
/// stream and is unconstrained.
fn validate_streamable_accumulators(ast: &StylesheetAst) -> Result<(), XsltError> {
    for acc in ast.accumulators.iter().filter(|a| a.streamable) {
        validate_accumulator(acc)?;
    }
    Ok(())
}

fn validate_accumulator(acc: &AccumulatorDecl) -> Result<(), XsltError> {
    for rule in &acc.rules {
        let ps = match &rule.select {
            Some(sel) => analyze_expr(sel, Posture::Striding),
            None => classify_body(&rule.body, Posture::Striding)?,
        };
        if !ps.is_streamable() {
            return Err(not_streamable("a rule of a streamable xsl:accumulator"));
        }
    }
    Ok(())
}

/// Recursively find `xsl:source-document streamable="yes"` instructions
/// and validate each one's body as its own streamable context.
fn scan_source_documents(body: &Body) -> Result<(), XsltError> {
    for instr in body.instrs() {
        scan_source_documents_instr(instr)?;
    }
    Ok(())
}

fn scan_source_documents_instr(i: &Instr) -> Result<(), XsltError> {
    use Instr::*;
    match i {
        SourceDocument { streamable: true, body, .. } => {
            require_body_streamable(
                body,
                Posture::Striding,
                "xsl:source-document streamable=\"yes\"",
            )?;
            scan_source_documents(body)?;
        }
        SourceDocument { body, .. }
        | LiteralElement { body, .. }
        | If { body, .. }
        | ForEach { body, .. }
        | Copy { body, .. }
        | Element { body, .. }
        | Attribute { body, .. }
        | Comment { body, .. }
        | ProcessingInstruction { body, .. }
        | Message { body, .. }
        | Assert { body, .. }
        | Fallback { body }
        | Map { body }
        | MapEntry { body, .. }
        | ForEachGroup { body, .. }
        | OnEmpty { body }
        | OnNonEmpty { body }
        | WherePopulated { body }
        | Fork { body }
        | PerformSort { body, .. }
        | Document { body }
        | ResultDocument { body, .. }
        | Namespace { body, .. }
        | ValueOfBody { body, .. }
        | Break { body, .. } => scan_source_documents(body)?,
        Variable(v) => scan_source_documents(&v.body)?,
        Choose { whens, otherwise } => {
            for (_, b) in whens {
                scan_source_documents(b)?;
            }
            if let Some(b) = otherwise {
                scan_source_documents(b)?;
            }
        }
        Iterate { params, on_completion, body, .. } => {
            for p in params {
                scan_source_documents(&p.body)?;
            }
            scan_source_documents(on_completion)?;
            scan_source_documents(body)?;
        }
        AnalyzeString { matching, non_matching, .. } => {
            scan_source_documents(matching)?;
            scan_source_documents(non_matching)?;
        }
        Merge { action, .. } => scan_source_documents(action)?,
        Try { body, catches } => {
            scan_source_documents(body)?;
            for c in catches {
                scan_source_documents(&c.body)?;
            }
        }
        ApplyTemplates { with_params, .. }
        | CallTemplate { with_params, .. }
        | ApplyImports { with_params }
        | NextMatch { with_params }
        | NextIteration { with_params }
        | Evaluate { with_params, .. } => {
            for w in with_params {
                scan_source_documents(&w.body)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Require a sequence-constructor body to be guaranteed-streamable in the
/// given context, returning an XTSE3430 error naming `what` otherwise.
fn require_body_streamable(body: &Body, ctx: Posture, what: &str) -> Result<(), XsltError> {
    if classify_body(body, ctx)?.is_streamable() {
        Ok(())
    } else {
        Err(not_streamable(what))
    }
}

/// Classify a sequence constructor (a template/instruction body).  Its
/// instructions share the context node, so they fold under the
/// at-most-one-consuming rule: more than one consuming instruction makes
/// the body free-ranging.  The body's result is grounded (a result-tree
/// fragment); the returned sweep is what it contributes to its parent.
fn classify_body(body: &Body, ctx: Posture) -> Result<Ps, XsltError> {
    let mut sweep = Sweep::Motionless;
    for instr in body.instrs() {
        let ps = analyze_instr(instr, ctx)?;
        if !ps.is_streamable() {
            return Err(not_streamable("an instruction in a streamable context"));
        }
        sweep = combine_strict(sweep, ps.sweep);
        if sweep == Sweep::FreeRanging {
            return Err(not_streamable(
                "a sequence constructor with more than one consuming instruction \
                 (use xsl:fork)",
            ));
        }
    }
    Ok(Ps::new(Posture::Grounded, sweep))
}

/// Like [`classify_body`] but for `xsl:fork`: the prongs each consume the
/// same input in a single shared pass, so they fold under the relaxed
/// rule (multiple consuming prongs are allowed).
fn classify_fork(body: &Body, ctx: Posture) -> Result<Ps, XsltError> {
    let mut sweep = Sweep::Motionless;
    for instr in body.instrs() {
        let ps = analyze_instr(instr, ctx)?;
        if !ps.is_streamable() {
            return Err(not_streamable("a prong of xsl:fork"));
        }
        sweep = combine_max(sweep, ps.sweep);
    }
    Ok(Ps::new(Posture::Grounded, sweep))
}

/// Classify the AVT-valued operands (attribute names/values, `href=`,
/// `terminate=`, …): the literal parts read nothing; each `{expr}` part
/// is analyzed and folded under the one-consuming rule.
fn classify_avt(avt: &Avt, ctx: Posture) -> Result<Ps, XsltError> {
    let mut sweep = Sweep::Motionless;
    for part in &avt.parts {
        if let AvtPart::Expr(e) = part {
            let p = analyze_expr(e, ctx);
            if p.posture == Posture::Roaming {
                return Err(not_streamable("an attribute value template expression"));
            }
            sweep = combine_strict(sweep, p.sweep);
        }
    }
    Ok(Ps::new(Posture::Grounded, sweep))
}

/// Fold the `select`/body operands of the `xsl:with-param` arguments to a
/// call, all evaluated in the caller's context (so they share it and
/// fold strictly).
fn classify_with_params(params: &[WithParam], ctx: Posture) -> Result<Sweep, XsltError> {
    let mut sweep = Sweep::Motionless;
    for w in params {
        let ps = match &w.select {
            Some(sel) => {
                let p = analyze_expr(sel, ctx);
                if p.posture == Posture::Roaming {
                    return Err(not_streamable("an xsl:with-param select expression"));
                }
                Ps::new(Posture::Grounded, p.sweep)
            }
            None => classify_body(&w.body, ctx)?,
        };
        sweep = combine_strict(sweep, ps.sweep);
        if sweep == Sweep::FreeRanging {
            return Err(not_streamable("the parameters of a single call"));
        }
    }
    Ok(sweep)
}

/// The streamability of a single `select`-style expression operand,
/// rejecting on a roaming posture.
fn expr_operand(expr: &Expr, ctx: Posture, what: &str) -> Result<Ps, XsltError> {
    let p = analyze_expr(expr, ctx);
    if !p.is_streamable() {
        return Err(not_streamable(what));
    }
    Ok(p)
}

/// Classify one XSLT instruction in a streamable context.  Returns the
/// posture/sweep it contributes to its enclosing sequence constructor;
/// errors (XTSE3430) when the instruction or one of its operands is not
/// guaranteed-streamable.
fn analyze_instr(instr: &Instr, ctx: Posture) -> Result<Ps, XsltError> {
    use Instr::*;
    match instr {
        // ── grounded, motionless emission ───────────────────────────
        LiteralText { .. } => Ok(Ps::grounded()),

        LiteralElement { attributes, body, .. } => {
            let mut sweep = classify_body(body, ctx)?.sweep;
            for (_, avt) in attributes {
                sweep = combine_strict(sweep, classify_avt(avt, ctx)?.sweep);
                if sweep == Sweep::FreeRanging {
                    return Err(not_streamable("a literal result element"));
                }
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }

        // ── value extraction: select consumes, result is text ───────
        ValueOf { select, separator, .. } => {
            let mut sweep = expr_operand(select, ctx, "xsl:value-of select")?.sweep;
            if let Some(sep) = separator {
                sweep = combine_strict(sweep, classify_avt(sep, ctx)?.sweep);
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        ValueOfBody { body, .. } => Ok(Ps::new(Posture::Grounded, classify_body(body, ctx)?.sweep)),
        Sequence { select } => Ok(expr_operand(select, ctx, "xsl:sequence select")?),
        CopyOf { select, .. } => {
            // copy-of grounds the selected subtree; the selection itself
            // may consume the stream.
            let p = expr_operand(select, ctx, "xsl:copy-of select")?;
            Ok(Ps::new(Posture::Grounded, p.sweep))
        }
        Copy { select, body, .. } => {
            // Shallow-copy the context (or `select=`) node, then run the
            // body in the same context (the identity-transform idiom).
            let mut sweep = classify_body(body, ctx)?.sweep;
            if let Some(sel) = select {
                sweep = combine_strict(sweep, expr_operand(sel, ctx, "xsl:copy select")?.sweep);
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }

        // ── template invocation: select is the consuming operand ────
        ApplyTemplates { select, with_params, sort, .. } => {
            reject_streaming_sort(sort, "xsl:apply-templates")?;
            let sel = match select {
                Some(e) => expr_operand(e, ctx, "xsl:apply-templates select")?,
                // Default select is `child::node()`.
                None => {
                    let p = apply_axis(ctx, sup_xml_core::xpath::ast::Axis::Child);
                    if !p.is_streamable() {
                        return Err(not_streamable("xsl:apply-templates over children"));
                    }
                    p
                }
            };
            let params_sweep = classify_with_params(with_params, ctx)?;
            let sweep = combine_strict(sel.sweep, params_sweep);
            if sweep == Sweep::FreeRanging {
                return Err(not_streamable("xsl:apply-templates and its parameters"));
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        ApplyImports { with_params } | NextMatch { with_params } => {
            Ok(Ps::new(Posture::Grounded, classify_with_params(with_params, ctx)?))
        }
        CallTemplate { with_params, .. } => {
            Ok(Ps::new(Posture::Grounded, classify_with_params(with_params, ctx)?))
        }

        // ── conditionals ────────────────────────────────────────────
        If { test, body } => {
            let t = expr_operand(test, ctx, "xsl:if test")?;
            let b = classify_body(body, ctx)?;
            Ok(Ps::new(Posture::Grounded, combine_strict(t.sweep, b.sweep)))
        }
        Choose { whens, otherwise } => {
            let mut test_sweep = Sweep::Motionless;
            let mut branch_sweep = Sweep::Motionless;
            for (test, body) in whens {
                test_sweep = combine_strict(test_sweep, expr_operand(test, ctx, "xsl:when test")?.sweep);
                branch_sweep = combine_max(branch_sweep, classify_body(body, ctx)?.sweep);
            }
            if let Some(body) = otherwise {
                branch_sweep = combine_max(branch_sweep, classify_body(body, ctx)?.sweep);
            }
            let sweep = combine_strict(test_sweep, branch_sweep);
            if sweep == Sweep::FreeRanging {
                return Err(not_streamable("xsl:choose"));
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }

        // ── iteration: select consumes; body runs per item ──────────
        ForEach { select, sort, body } => {
            reject_streaming_sort(sort, "xsl:for-each")?;
            let sel = expr_operand(select, ctx, "xsl:for-each select")?;
            let body_ps = classify_body(body, sel.posture)?;
            Ok(Ps::new(Posture::Grounded, combine_max(sel.sweep, body_ps.sweep)))
        }
        Iterate { select, on_completion, body, .. } => {
            let sel = expr_operand(select, ctx, "xsl:iterate select")?;
            let body_ps = classify_body(body, sel.posture)?;
            // `on-completion` runs once after the stream, against the
            // post-iteration state — grounded, no further consumption.
            let completion_ps = classify_body(on_completion, Posture::Grounded)?;
            let sweep = combine_max(combine_max(sel.sweep, body_ps.sweep), completion_ps.sweep);
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        Break { select, body } => {
            let mut sweep = classify_body(body, ctx)?.sweep;
            if let Some(sel) = select {
                sweep = combine_strict(sweep, expr_operand(sel, ctx, "xsl:break select")?.sweep);
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        NextIteration { with_params } => {
            Ok(Ps::new(Posture::Grounded, classify_with_params(with_params, ctx)?))
        }

        // ── constructed nodes ───────────────────────────────────────
        Element { name, namespace, body, .. } => {
            let mut sweep = classify_body(body, ctx)?.sweep;
            sweep = combine_strict(sweep, classify_avt(name, ctx)?.sweep);
            if let Some(ns) = namespace {
                sweep = combine_strict(sweep, classify_avt(ns, ctx)?.sweep);
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        Attribute { name, namespace, select, separator, body, .. } => {
            let mut sweep = classify_body(body, ctx)?.sweep;
            sweep = combine_strict(sweep, classify_avt(name, ctx)?.sweep);
            if let Some(ns) = namespace {
                sweep = combine_strict(sweep, classify_avt(ns, ctx)?.sweep);
            }
            if let Some(sel) = select {
                sweep = combine_strict(sweep, expr_operand(sel, ctx, "xsl:attribute select")?.sweep);
            }
            if let Some(sep) = separator {
                sweep = combine_strict(sweep, classify_avt(sep, ctx)?.sweep);
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        Comment { select, body } => {
            let mut sweep = classify_body(body, ctx)?.sweep;
            if let Some(sel) = select {
                sweep = combine_strict(sweep, expr_operand(sel, ctx, "xsl:comment select")?.sweep);
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        ProcessingInstruction { name, select, body } => {
            let mut sweep = classify_avt(name, ctx)?.sweep;
            sweep = combine_strict(sweep, classify_body(body, ctx)?.sweep);
            if let Some(sel) = select {
                sweep = combine_strict(sweep, expr_operand(sel, ctx, "xsl:processing-instruction select")?.sweep);
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        Namespace { name, select, body } => {
            let mut sweep = classify_avt(name, ctx)?.sweep;
            sweep = combine_strict(sweep, classify_body(body, ctx)?.sweep);
            if let Some(sel) = select {
                sweep = combine_strict(sweep, expr_operand(sel, ctx, "xsl:namespace select")?.sweep);
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        Document { body } => Ok(Ps::new(Posture::Grounded, classify_body(body, ctx)?.sweep)),

        // ── variable binding: emits nothing, evaluates its value ────
        Variable(v) => {
            let sweep = match &v.select {
                Some(sel) => expr_operand(sel, ctx, "xsl:variable select")?.sweep,
                None => classify_body(&v.body, ctx)?.sweep,
            };
            Ok(Ps::new(Posture::Grounded, sweep))
        }

        // ── diagnostics ─────────────────────────────────────────────
        Message { terminate, error_code, body } => {
            let mut sweep = classify_body(body, ctx)?.sweep;
            if let Some(avt) = terminate {
                sweep = combine_strict(sweep, classify_avt(avt, ctx)?.sweep);
            }
            if let Some(avt) = error_code {
                sweep = combine_strict(sweep, classify_avt(avt, ctx)?.sweep);
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        Assert { test, select, body, error_code } => {
            let mut sweep = expr_operand(test, ctx, "xsl:assert test")?.sweep;
            sweep = combine_strict(sweep, classify_body(body, ctx)?.sweep);
            if let Some(sel) = select {
                sweep = combine_strict(sweep, expr_operand(sel, ctx, "xsl:assert select")?.sweep);
            }
            if let Some(avt) = error_code {
                sweep = combine_strict(sweep, classify_avt(avt, ctx)?.sweep);
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        Fallback { body } => classify_body(body, ctx),

        // ── populated-content wrappers ──────────────────────────────
        OnEmpty { body } | OnNonEmpty { body } | WherePopulated { body } => {
            classify_body(body, ctx)
        }

        // ── the one-pass-multiple-consumers escape hatch ────────────
        Fork { body } => classify_fork(body, ctx),

        // ── error handling ──────────────────────────────────────────
        Try { body, catches } => {
            let mut sweep = classify_body(body, ctx)?.sweep;
            for c in catches {
                sweep = combine_max(sweep, classify_body(&c.body, ctx)?.sweep);
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }

        // ── string analysis: select consumes; pieces are grounded ───
        AnalyzeString { select, regex, flags, matching, non_matching } => {
            let mut sweep = expr_operand(select, ctx, "xsl:analyze-string select")?.sweep;
            sweep = combine_strict(sweep, classify_avt(regex, ctx)?.sweep);
            sweep = combine_strict(sweep, classify_avt(flags, ctx)?.sweep);
            // The matching/non-matching bodies operate on the captured
            // substrings (grounded strings), not the streamed tree.
            classify_body(matching, Posture::Grounded)?;
            classify_body(non_matching, Posture::Grounded)?;
            Ok(Ps::new(Posture::Grounded, sweep))
        }

        // ── nested streaming of a separate document ─────────────────
        SourceDocument { href, streamable, body } => {
            // A nested source-document opens its own input; relative to
            // the enclosing stream it is motionless.  When it is itself
            // streamable, its body is validated as a fresh streaming
            // context (also handled by `scan_source_documents`).
            let sweep = classify_avt(href, ctx)?.sweep;
            if *streamable {
                require_body_streamable(body, Posture::Striding, "xsl:source-document streamable=\"yes\"")?;
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        ResultDocument { href, body, .. } => {
            let mut sweep = classify_avt(href, ctx)?.sweep;
            sweep = combine_strict(sweep, classify_body(body, ctx)?.sweep);
            Ok(Ps::new(Posture::Grounded, sweep))
        }

        // ── grouping / sorting / merge / maps ───────────────────────
        //
        // These are analyzed by their operands rather than blanket-
        // rejected: when their selections are streamable they are
        // accepted, and the streamed executor falls back to burst (which
        // grounds one record and runs the full tree evaluator) for the
        // ones its incremental path doesn't cover.  A genuinely roaming
        // operand still rejects via `expr_operand` / `classify_body`.
        ForEachGroup { select, key, body, .. } => {
            let sel = expr_operand(select, ctx, "xsl:for-each-group select")?;
            // `key` and `body` are evaluated per group member.
            let key_ps = expr_operand(key, ctx, "xsl:for-each-group group key")?;
            let body_ps = classify_body(body, sel.posture)?;
            let sweep = combine_max(combine_max(sel.sweep, key_ps.sweep), body_ps.sweep);
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        PerformSort { select, body, .. } => {
            let mut sweep = match select {
                Some(s) => expr_operand(s, ctx, "xsl:perform-sort select")?.sweep,
                None => Sweep::Motionless,
            };
            sweep = combine_max(sweep, classify_body(body, ctx)?.sweep);
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        Merge { sources, action } => {
            let mut sweep = Sweep::Motionless;
            for src in sources {
                sweep = combine_max(sweep, expr_operand(&src.select, ctx, "xsl:merge-source select")?.sweep);
            }
            sweep = combine_max(sweep, classify_body(action, ctx)?.sweep);
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        Number { value, select, count, from, .. } => {
            let mut sweep = Sweep::Motionless;
            for e in [value.as_ref(), select.as_ref(), count.as_ref(), from.as_ref()]
                .into_iter().flatten()
            {
                sweep = combine_max(sweep, expr_operand(e, ctx, "xsl:number")?.sweep);
            }
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        Map { body } => Ok(Ps::new(Posture::Grounded, classify_body(body, ctx)?.sweep)),
        MapEntry { key, select, body } => {
            let mut sweep = expr_operand(key, ctx, "xsl:map-entry key")?.sweep;
            if let Some(s) = select {
                sweep = combine_strict(sweep, expr_operand(s, ctx, "xsl:map-entry select")?.sweep);
            }
            sweep = combine_strict(sweep, classify_body(body, ctx)?.sweep);
            Ok(Ps::new(Posture::Grounded, sweep))
        }
        // Dynamic XPath: not statically analyzable.  Accept — the
        // fallback executor runs it in-memory (we don't claim strict
        // streaming, so a hint we can't verify isn't a hard error).
        Evaluate { .. } => Ok(Ps::grounded()),

        // A genuinely unrecognized instruction can't be streamed.
        Unsupported { .. } => Err(not_streamable("an unsupported instruction")),
    }
}

/// `xsl:sort` requires the whole selected sequence in memory, so a
/// sort over a streamed (non-grounded) selection is not streamable.
fn reject_streaming_sort(sort: &[crate::ast::Sort], owner: &str) -> Result<(), XsltError> {
    if sort.is_empty() {
        Ok(())
    } else {
        Err(not_streamable(&format!("{owner} with xsl:sort")))
    }
}

#[cfg(test)]
mod tests {
    use crate::Stylesheet;

    fn compiles(xsl: &str) -> Result<(), String> {
        Stylesheet::compile_str(xsl).map(|_| ()).map_err(|e| e.to_string())
    }

    const HEAD: &str = r#"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">"#;

    #[test]
    fn streamable_mode_with_downward_apply_templates_compiles() {
        let xsl = format!(
            r#"{HEAD}
            <xsl:mode name="s" streamable="yes"/>
            <xsl:template match="item" mode="s">
                <out><xsl:value-of select="@id"/></out>
            </xsl:template>
            <xsl:template match="items" mode="s">
                <xsl:apply-templates select="item" mode="s"/>
            </xsl:template>
        </xsl:stylesheet>"#
        );
        assert!(compiles(&xsl).is_ok(), "{:?}", compiles(&xsl));
    }

    #[test]
    fn streamable_mode_accepts_absolute_downward_path() {
        // Rooted at the streamed document node, an absolute downward path
        // is a striding selection (matches the W3C source-document tests).
        let xsl = format!(
            r#"{HEAD}
            <xsl:mode name="s" streamable="yes"/>
            <xsl:template match="items" mode="s">
                <xsl:apply-templates select="/items/item" mode="s"/>
            </xsl:template>
        </xsl:stylesheet>"#
        );
        assert!(compiles(&xsl).is_ok(), "{:?}", compiles(&xsl));
    }

    #[test]
    fn streamable_mode_rejects_preceding_axis() {
        let xsl = format!(
            r#"{HEAD}
            <xsl:mode name="s" streamable="yes"/>
            <xsl:template match="item" mode="s">
                <out><xsl:value-of select="preceding-sibling::item"/></out>
            </xsl:template>
        </xsl:stylesheet>"#
        );
        assert!(compiles(&xsl).unwrap_err().contains("XTSE3430"));
    }

    #[test]
    fn streamable_mode_rejects_two_consuming_instructions() {
        let xsl = format!(
            r#"{HEAD}
            <xsl:mode name="s" streamable="yes"/>
            <xsl:template match="item" mode="s">
                <a><xsl:value-of select="string(child::x)"/></a>
                <b><xsl:value-of select="string(child::y)"/></b>
            </xsl:template>
        </xsl:stylesheet>"#
        );
        assert!(compiles(&xsl).unwrap_err().contains("XTSE3430"));
    }

    #[test]
    fn fork_allows_multiple_consumers() {
        let xsl = format!(
            r#"{HEAD}
            <xsl:mode name="s" streamable="yes"/>
            <xsl:template match="item" mode="s">
                <xsl:fork>
                    <xsl:sequence select="string(child::x)"/>
                    <xsl:sequence select="string(child::y)"/>
                </xsl:fork>
            </xsl:template>
        </xsl:stylesheet>"#
        );
        assert!(compiles(&xsl).is_ok(), "{:?}", compiles(&xsl));
    }

    #[test]
    fn non_streamable_mode_is_not_analyzed() {
        // The same body that fails under streamable="yes" must still
        // compile in an ordinary mode.
        let xsl = format!(
            r#"{HEAD}
            <xsl:template match="item">
                <out><xsl:value-of select="preceding-sibling::item"/></out>
                <two><xsl:value-of select="following::x"/></two>
            </xsl:template>
        </xsl:stylesheet>"#
        );
        assert!(compiles(&xsl).is_ok(), "{:?}", compiles(&xsl));
    }

    #[test]
    fn streamable_source_document_rejects_roaming_axis() {
        // A genuinely non-streamable selection (a sibling axis) inside a
        // streamable source-document body is rejected.
        let xsl = format!(
            r#"{HEAD}
            <xsl:template name="main">
                <xsl:source-document href="huge.xml" streamable="yes">
                    <out><xsl:value-of select="following-sibling::x"/></out>
                </xsl:source-document>
            </xsl:template>
        </xsl:stylesheet>"#
        );
        assert!(compiles(&xsl).unwrap_err().contains("XTSE3430"));
    }

    #[test]
    fn streamable_source_document_accepts_relative_path() {
        let xsl = format!(
            r#"{HEAD}
            <xsl:template name="main">
                <xsl:source-document href="huge.xml" streamable="yes">
                    <xsl:apply-templates select="items/item"/>
                </xsl:source-document>
            </xsl:template>
        </xsl:stylesheet>"#
        );
        assert!(compiles(&xsl).is_ok(), "{:?}", compiles(&xsl));
    }

    #[test]
    fn streamable_accumulator_accepts_motionless_rule() {
        let xsl = format!(
            r#"{HEAD}
            <xsl:accumulator name="depth" initial-value="0" streamable="yes">
                <xsl:accumulator-rule match="section" select="$value + 1"/>
            </xsl:accumulator>
            <xsl:template match="/"><out/></xsl:template>
        </xsl:stylesheet>"#
        );
        assert!(compiles(&xsl).is_ok(), "{:?}", compiles(&xsl));
    }
}
