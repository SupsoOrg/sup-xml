//! AST walkers — visit every XPath [`Expr`] inside a compiled
//! stylesheet.
//!
//! Used by the compiler to harvest information that's resolved
//! eagerly at apply time.  Currently the only consumer is
//! `collect_static_document_uris`, which finds every literal-URI
//! `document()` call so the runtime can pre-load those documents
//! before evaluation starts.

use sup_xml_core::xpath::ast::{Expr, LookupKey};

use crate::ast::{Avt, AvtPart, Instr, Sort, StylesheetAst, Template, WithParam, Variable, Param};

/// Walk the entire stylesheet and collect string-literal arguments
/// passed as the first argument of `document()`.  Returns the URIs
/// in document order with duplicates removed.
///
/// Dynamic `document(@href)` style calls are intentionally not
/// captured here — the engine resolves them at apply time via the
/// `Loader` only when the URI is known statically.  Non-static
/// `document()` arguments raise a runtime error pointing the user
/// at the limitation.
pub fn collect_static_document_uris(style: &StylesheetAst) -> Vec<String> {
    let mut out = Vec::new();

    for t in &style.templates {
        collect_template(t, &mut out);
    }
    for v in &style.global_variables {
        collect_variable(v, &mut out);
    }
    for p in &style.global_params {
        collect_param(p, &mut out);
    }
    for k in &style.keys {
        walk_expr(&k.matcher, &mut out);
        walk_expr(&k.use_, &mut out);
    }
    for set in &style.attribute_sets {
        for instr in &set.attributes {
            walk_instr(instr, &mut out);
        }
    }

    out.sort();
    out.dedup();
    out
}

/// Collect every string-literal expression anywhere in the
/// stylesheet's XPath expressions.  Used (in conjunction with the
/// dynamic-document-call check) to seed the URI pre-load pool with
/// candidates the static document() collector misses — e.g. URIs
/// bound to a variable and then passed through to `document($v)`.
pub fn collect_all_string_literals(style: &StylesheetAst) -> Vec<String> {
    let mut out = Vec::new();
    let mut scan = |e: &Expr| {
        walk_expr_for_literals(e, &mut out);
    };
    for t in &style.templates {
        scan_template(t, &mut scan);
    }
    for v in &style.global_variables {
        if let Some(e) = &v.select { scan(e); }
        for instr in &v.body { scan_instr(instr, &mut scan); }
    }
    for p in &style.global_params {
        if let Some(e) = &p.select { scan(e); }
        for instr in &p.body { scan_instr(instr, &mut scan); }
    }
    for k in &style.keys {
        scan(&k.matcher);
        scan(&k.use_);
    }
    for set in &style.attribute_sets {
        for instr in &set.attributes {
            scan_instr(instr, &mut scan);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn walk_expr_for_literals(e: &Expr, out: &mut Vec<String>) {
    use Expr::*;
    match e {
        Or(l, r) | And(l, r) | Eq(l, r) | Ne(l, r)
        | Lt(l, r) | Gt(l, r) | Le(l, r) | Ge(l, r)
        | ValueEq(l, r) | ValueNe(l, r)
        | ValueLt(l, r) | ValueGt(l, r)
        | ValueLe(l, r) | ValueGe(l, r)
        | Add(l, r) | Sub(l, r) | Mul(l, r) | Div(l, r)
        | Mod(l, r) | Union(l, r) => {
            walk_expr_for_literals(l, out);
            walk_expr_for_literals(r, out);
        }
        Neg(e) => walk_expr_for_literals(e, out),
        Path(_) | Variable(_) | Integer(_) | Decimal(_) | Double(_) => {}
        Literal(s) => out.push(s.clone()),
        FilterPath { primary, predicates, steps } => {
            walk_expr_for_literals(primary, out);
            for p in predicates { walk_expr_for_literals(p, out); }
            for s in steps {
                for pred in &s.predicates { walk_expr_for_literals(pred, out); }
            }
        }
        FunctionCall(_, args) => {
            for a in args { walk_expr_for_literals(a, out); }
        }
        IfThenElse { cond, then_branch, else_branch } => {
            walk_expr_for_literals(cond, out);
            walk_expr_for_literals(then_branch, out);
            walk_expr_for_literals(else_branch, out);
        }
        For { bindings, body } | Let { bindings, body } => {
            for (_, e) in bindings { walk_expr_for_literals(e, out); }
            walk_expr_for_literals(body, out);
        }
        Range(a, b) | SimpleMap(a, b) | NodeBefore(a, b) | NodeAfter(a, b) | NodeIs(a, b) => {
            walk_expr_for_literals(a, out);
            walk_expr_for_literals(b, out);
        }
        Sequence(items) => for e in items { walk_expr_for_literals(e, out); }
        Quantified { bindings, test, .. } => {
            for (_, e) in bindings { walk_expr_for_literals(e, out); }
            walk_expr_for_literals(test, out);
        }
        IDiv(a, b) | Intersect(a, b) | Except(a, b) => {
            walk_expr_for_literals(a, out);
            walk_expr_for_literals(b, out);
        }
        InstanceOf(a, _) | CastAs(a, _) | CastableAs(a, _) | TreatAs(a, _) =>
            walk_expr_for_literals(a, out),
        TryCatch { body, catches } => {
            walk_expr_for_literals(body, out);
            for c in catches { walk_expr_for_literals(&c.body, out); }
        }
        WithDefaultCollation(_, inner) => walk_expr_for_literals(inner, out),
        BackwardsCompat(inner) => walk_expr_for_literals(inner, out),
        MapConstructor(es) => for (k, v) in es {
            walk_expr_for_literals(k, out); walk_expr_for_literals(v, out);
        },
        ArrayConstructor { members, .. } =>
            for m in members { walk_expr_for_literals(m, out); },
        Lookup(b, key) => {
            walk_expr_for_literals(b, out);
            if let LookupKey::Expr(e) = key { walk_expr_for_literals(e, out); }
        }
        UnaryLookup(key) =>
            if let LookupKey::Expr(e) = key { walk_expr_for_literals(e, out); },
        InlineFunction { body, .. } => walk_expr_for_literals(body, out),
        DynamicCall { func, args } => {
            walk_expr_for_literals(func, out);
            for a in args { walk_expr_for_literals(a, out); }
        }
        NamedFunctionRef { .. } | Placeholder | ContextItem => {}
    }
}

/// Walk the stylesheet and collect URIs passed as string-literal
/// first arguments to `unparsed-text(...)`, `unparsed-text-available(...)`,
/// or `unparsed-text-lines(...)`.  Mirrors
/// [`collect_static_document_uris`] for the XSLT 2.0 text-resource
/// family.  Returns URIs in document order, deduped.
pub fn collect_static_unparsed_text_uris(style: &StylesheetAst) -> Vec<String> {
    let mut out = Vec::new();
    let mut scan = |e: &Expr| {
        walk_expr_for_unparsed_text(e, &mut out);
    };
    for t in &style.templates {
        scan_template(t, &mut scan);
    }
    for v in &style.global_variables {
        if let Some(e) = &v.select { scan(e); }
        for instr in &v.body { scan_instr(instr, &mut scan); }
    }
    for p in &style.global_params {
        if let Some(e) = &p.select { scan(e); }
        for instr in &p.body { scan_instr(instr, &mut scan); }
    }
    for k in &style.keys {
        scan(&k.matcher);
        scan(&k.use_);
    }
    for set in &style.attribute_sets {
        for instr in &set.attributes {
            scan_instr(instr, &mut scan);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn walk_expr_for_unparsed_text(e: &Expr, out: &mut Vec<String>) {
    use Expr::*;
    match e {
        Or(l, r) | And(l, r) | Eq(l, r) | Ne(l, r)
        | Lt(l, r) | Gt(l, r) | Le(l, r) | Ge(l, r)
        | ValueEq(l, r) | ValueNe(l, r)
        | ValueLt(l, r) | ValueGt(l, r)
        | ValueLe(l, r) | ValueGe(l, r)
        | Add(l, r) | Sub(l, r) | Mul(l, r) | Div(l, r)
        | Mod(l, r) | Union(l, r) => {
            walk_expr_for_unparsed_text(l, out);
            walk_expr_for_unparsed_text(r, out);
        }
        Neg(e) => walk_expr_for_unparsed_text(e, out),
        Path(_) | Variable(_) | Literal(_) | Integer(_) | Decimal(_) | Double(_) => {}
        FilterPath { primary, predicates, steps } => {
            walk_expr_for_unparsed_text(primary, out);
            for p in predicates { walk_expr_for_unparsed_text(p, out); }
            for s in steps {
                for pred in &s.predicates { walk_expr_for_unparsed_text(pred, out); }
            }
        }
        FunctionCall(name, args) => {
            let is_unparsed_text = matches!(name.as_str(),
                "unparsed-text" | "unparsed-text-available" | "unparsed-text-lines");
            if is_unparsed_text && !args.is_empty() {
                if let Literal(s) = &args[0] {
                    out.push(s.clone());
                }
            }
            for a in args { walk_expr_for_unparsed_text(a, out); }
        }
        IfThenElse { cond, then_branch, else_branch } => {
            walk_expr_for_unparsed_text(cond, out);
            walk_expr_for_unparsed_text(then_branch, out);
            walk_expr_for_unparsed_text(else_branch, out);
        }
        For { bindings, body } | Let { bindings, body } => {
            for (_, e) in bindings { walk_expr_for_unparsed_text(e, out); }
            walk_expr_for_unparsed_text(body, out);
        }
        Range(a, b) | SimpleMap(a, b) | NodeBefore(a, b) | NodeAfter(a, b) | NodeIs(a, b) => {
            walk_expr_for_unparsed_text(a, out);
            walk_expr_for_unparsed_text(b, out);
        }
        Sequence(items) => for e in items { walk_expr_for_unparsed_text(e, out); }
        Quantified { bindings, test, .. } => {
            for (_, e) in bindings { walk_expr_for_unparsed_text(e, out); }
            walk_expr_for_unparsed_text(test, out);
        }
        IDiv(a, b) | Intersect(a, b) | Except(a, b) => {
            walk_expr_for_unparsed_text(a, out);
            walk_expr_for_unparsed_text(b, out);
        }
        InstanceOf(a, _) | CastAs(a, _) | CastableAs(a, _) | TreatAs(a, _) =>
            walk_expr_for_unparsed_text(a, out),
        TryCatch { body, catches } => {
            walk_expr_for_unparsed_text(body, out);
            for c in catches { walk_expr_for_unparsed_text(&c.body, out); }
        }
        WithDefaultCollation(_, inner) => walk_expr_for_unparsed_text(inner, out),
        BackwardsCompat(inner) => walk_expr_for_unparsed_text(inner, out),
        MapConstructor(es) => for (k, v) in es {
            walk_expr_for_unparsed_text(k, out); walk_expr_for_unparsed_text(v, out);
        },
        ArrayConstructor { members, .. } =>
            for m in members { walk_expr_for_unparsed_text(m, out); },
        Lookup(b, key) => {
            walk_expr_for_unparsed_text(b, out);
            if let LookupKey::Expr(e) = key { walk_expr_for_unparsed_text(e, out); }
        }
        UnaryLookup(key) =>
            if let LookupKey::Expr(e) = key { walk_expr_for_unparsed_text(e, out); },
        InlineFunction { body, .. } => walk_expr_for_unparsed_text(body, out),
        DynamicCall { func, args } => {
            walk_expr_for_unparsed_text(func, out);
            for a in args { walk_expr_for_unparsed_text(a, out); }
        }
        NamedFunctionRef { .. } | Placeholder | ContextItem => {}
    }
}

/// True iff the stylesheet contains a `document()` call whose first
/// argument isn't a string literal.  Dynamic forms (`document(@href)`,
/// `document(*)`, `document(concat(...))`) need apply-time URI
/// discovery; static-literal-only stylesheets can skip that work.
/// More-specific variant of [`has_dynamic_document_call`] that
/// returns `true` only for the node-set-driven `document()` form
/// — i.e. calls whose first argument is a Path or other non-
/// literal-non-variable expression that yields a node-set whose
/// per-item string-values supply many candidate URIs.  Used by
/// the speculative pre-load to decide whether the source-doc
/// walk is worth running.  `document($var)` / `doc(concat(...))`
/// don't trigger this — runtime loading handles them without
/// needing a source-doc scan.
pub fn has_document_node_set_call(style: &StylesheetAst) -> bool {
    let mut found = false;
    let mut scan = |e: &Expr| {
        walk_expr_for_doc_node_set(e, &mut found);
    };
    for t in &style.templates {
        scan_template(t, &mut scan);
    }
    for v in &style.global_variables {
        if let Some(e) = &v.select { scan(e); }
        for instr in &v.body { scan_instr(instr, &mut scan); }
    }
    for p in &style.global_params {
        if let Some(e) = &p.select { scan(e); }
        for instr in &p.body { scan_instr(instr, &mut scan); }
    }
    for k in &style.keys {
        scan(&k.matcher);
        scan(&k.use_);
    }
    for set in &style.attribute_sets {
        for instr in &set.attributes {
            scan_instr(instr, &mut scan);
        }
    }
    found
}

fn walk_expr_for_doc_node_set(e: &Expr, found: &mut bool) {
    if *found { return; }
    use Expr::*;
    match e {
        FunctionCall(name, args) => {
            let is_doc = matches!(name.as_str(),
                "document" | "{}document" | "fn:document"
              | "doc"      | "{}doc"      | "fn:doc");
            if is_doc && !args.is_empty() {
                // node-set-driven: first arg is a Path (axis
                // traversal) or anything else that's neither a
                // literal nor a variable.  doc($var) and
                // doc(concat(...)) don't qualify — the runtime
                // path handles them.
                match &args[0] {
                    Literal(_) | Variable(_) => {}
                    Path(_) | FilterPath { .. } => { *found = true; return; }
                    _ => {}
                }
            }
            for a in args { walk_expr_for_doc_node_set(a, found); }
        }
        Or(l, r) | And(l, r) | Eq(l, r) | Ne(l, r)
        | Lt(l, r) | Gt(l, r) | Le(l, r) | Ge(l, r)
        | ValueEq(l, r) | ValueNe(l, r)
        | ValueLt(l, r) | ValueGt(l, r)
        | ValueLe(l, r) | ValueGe(l, r)
        | Add(l, r) | Sub(l, r) | Mul(l, r) | Div(l, r)
        | Mod(l, r) | Union(l, r)
        | IDiv(l, r) | Intersect(l, r) | Except(l, r)
        | Range(l, r) | SimpleMap(l, r)
        | NodeBefore(l, r) | NodeAfter(l, r) | NodeIs(l, r) => {
            walk_expr_for_doc_node_set(l, found);
            walk_expr_for_doc_node_set(r, found);
        }
        Neg(e) | InstanceOf(e, _) | CastAs(e, _)
        | CastableAs(e, _) | TreatAs(e, _) => walk_expr_for_doc_node_set(e, found),
        Path(_) | Variable(_) | Literal(_) | Integer(_) | Decimal(_) | Double(_) => {}
        FilterPath { primary, predicates, steps } => {
            walk_expr_for_doc_node_set(primary, found);
            for p in predicates { walk_expr_for_doc_node_set(p, found); }
            for s in steps {
                for pred in &s.predicates { walk_expr_for_doc_node_set(pred, found); }
            }
        }
        IfThenElse { cond, then_branch, else_branch } => {
            walk_expr_for_doc_node_set(cond, found);
            walk_expr_for_doc_node_set(then_branch, found);
            walk_expr_for_doc_node_set(else_branch, found);
        }
        For { bindings, body } | Let { bindings, body } => {
            for (_, e) in bindings { walk_expr_for_doc_node_set(e, found); }
            walk_expr_for_doc_node_set(body, found);
        }
        Sequence(items) => for e in items { walk_expr_for_doc_node_set(e, found); }
        Quantified { bindings, test, .. } => {
            for (_, e) in bindings { walk_expr_for_doc_node_set(e, found); }
            walk_expr_for_doc_node_set(test, found);
        }
        TryCatch { body, catches } => {
            walk_expr_for_doc_node_set(body, found);
            for c in catches { walk_expr_for_doc_node_set(&c.body, found); }
        }
        WithDefaultCollation(_, inner) => walk_expr_for_doc_node_set(inner, found),
        BackwardsCompat(inner) => walk_expr_for_doc_node_set(inner, found),
        MapConstructor(es) => for (k, v) in es {
            walk_expr_for_doc_node_set(k, found); walk_expr_for_doc_node_set(v, found);
        },
        ArrayConstructor { members, .. } =>
            for m in members { walk_expr_for_doc_node_set(m, found); },
        Lookup(b, key) => {
            walk_expr_for_doc_node_set(b, found);
            if let LookupKey::Expr(e) = key { walk_expr_for_doc_node_set(e, found); }
        }
        UnaryLookup(key) =>
            if let LookupKey::Expr(e) = key { walk_expr_for_doc_node_set(e, found); },
        InlineFunction { body, .. } => walk_expr_for_doc_node_set(body, found),
        DynamicCall { func, args } => {
            walk_expr_for_doc_node_set(func, found);
            for a in args { walk_expr_for_doc_node_set(a, found); }
        }
        NamedFunctionRef { .. } | Placeholder | ContextItem => {}
    }
}

pub fn has_dynamic_document_call(style: &StylesheetAst) -> bool {
    let mut found = false;
    let mut scan = |e: &Expr| {
        walk_expr_for_dynamic_doc(e, &mut found);
    };
    for t in &style.templates {
        scan_template(t, &mut scan);
    }
    for v in &style.global_variables {
        if let Some(e) = &v.select { scan(e); }
        for instr in &v.body { scan_instr(instr, &mut scan); }
    }
    for p in &style.global_params {
        if let Some(e) = &p.select { scan(e); }
        for instr in &p.body { scan_instr(instr, &mut scan); }
    }
    for k in &style.keys {
        scan(&k.matcher);
        scan(&k.use_);
    }
    for set in &style.attribute_sets {
        for instr in &set.attributes {
            scan_instr(instr, &mut scan);
        }
    }
    found
}

fn walk_expr_for_dynamic_doc(e: &Expr, found: &mut bool) {
    if *found { return; }
    use Expr::*;
    match e {
        Or(l, r) | And(l, r) | Eq(l, r) | Ne(l, r)
        | Lt(l, r) | Gt(l, r) | Le(l, r) | Ge(l, r)
        | ValueEq(l, r) | ValueNe(l, r)
        | ValueLt(l, r) | ValueGt(l, r)
        | ValueLe(l, r) | ValueGe(l, r)
        | Add(l, r) | Sub(l, r) | Mul(l, r) | Div(l, r)
        | Mod(l, r) | Union(l, r) => {
            walk_expr_for_dynamic_doc(l, found);
            walk_expr_for_dynamic_doc(r, found);
        }
        Neg(e) => walk_expr_for_dynamic_doc(e, found),
        Path(_) | Variable(_) | Literal(_) | Integer(_) | Decimal(_) | Double(_) => {}
        FilterPath { primary, predicates, steps } => {
            walk_expr_for_dynamic_doc(primary, found);
            for p in predicates { walk_expr_for_dynamic_doc(p, found); }
            for s in steps {
                for pred in &s.predicates { walk_expr_for_dynamic_doc(pred, found); }
            }
        }
        FunctionCall(name, args) => {
            // XPath 2.0 `doc()` and XSLT 1.0 `document()` are
            // both dynamic-URI-capable.  Treat any call whose
            // first argument isn't a string-literal as dynamic so
            // the loader's `enumerate` callback fires and pre-loads
            // the candidates it can find.
            let is_doc_load = matches!(name.as_str(),
                "document" | "{}document" | "fn:document"
              | "doc"      | "{}doc"      | "fn:doc"
              | "doc-available" | "{}doc-available" | "fn:doc-available"
            );
            if is_doc_load && !args.is_empty()
                && !matches!(&args[0], Literal(_))
            {
                *found = true;
                return;
            }
            for a in args { walk_expr_for_dynamic_doc(a, found); }
        }
        IfThenElse { cond, then_branch, else_branch } => {
            walk_expr_for_dynamic_doc(cond, found);
            walk_expr_for_dynamic_doc(then_branch, found);
            walk_expr_for_dynamic_doc(else_branch, found);
        }
        For { bindings, body } | Let { bindings, body } => {
            for (_, e) in bindings { walk_expr_for_dynamic_doc(e, found); }
            walk_expr_for_dynamic_doc(body, found);
        }
        Range(a, b) | SimpleMap(a, b) | NodeBefore(a, b) | NodeAfter(a, b) | NodeIs(a, b) => {
            walk_expr_for_dynamic_doc(a, found);
            walk_expr_for_dynamic_doc(b, found);
        }
        Sequence(items) => for e in items { walk_expr_for_dynamic_doc(e, found); }
        Quantified { bindings, test, .. } => {
            for (_, e) in bindings { walk_expr_for_dynamic_doc(e, found); }
            walk_expr_for_dynamic_doc(test, found);
        }
        IDiv(a, b) | Intersect(a, b) | Except(a, b) => {
            walk_expr_for_dynamic_doc(a, found);
            walk_expr_for_dynamic_doc(b, found);
        }
        InstanceOf(a, _) | CastAs(a, _) | CastableAs(a, _) | TreatAs(a, _) =>
            walk_expr_for_dynamic_doc(a, found),
        TryCatch { body, catches } => {
            walk_expr_for_dynamic_doc(body, found);
            for c in catches { walk_expr_for_dynamic_doc(&c.body, found); }
        }
        WithDefaultCollation(_, inner) => walk_expr_for_dynamic_doc(inner, found),
        BackwardsCompat(inner) => walk_expr_for_dynamic_doc(inner, found),
        MapConstructor(es) => for (k, v) in es {
            walk_expr_for_dynamic_doc(k, found); walk_expr_for_dynamic_doc(v, found);
        },
        ArrayConstructor { members, .. } =>
            for m in members { walk_expr_for_dynamic_doc(m, found); },
        Lookup(b, key) => {
            walk_expr_for_dynamic_doc(b, found);
            if let LookupKey::Expr(e) = key { walk_expr_for_dynamic_doc(e, found); }
        }
        UnaryLookup(key) =>
            if let LookupKey::Expr(e) = key { walk_expr_for_dynamic_doc(e, found); },
        InlineFunction { body, .. } => walk_expr_for_dynamic_doc(body, found),
        DynamicCall { func, args } => {
            walk_expr_for_dynamic_doc(func, found);
            for a in args { walk_expr_for_dynamic_doc(a, found); }
        }
        NamedFunctionRef { .. } | Placeholder | ContextItem => {}
    }
}

fn scan_template<F: FnMut(&Expr)>(t: &Template, scan: &mut F) {
    if let Some(m) = &t.match_pattern { scan(m); }
    for p in &t.params {
        if let Some(e) = &p.select { scan(e); }
        for instr in &p.body { scan_instr(instr, scan); }
    }
    for instr in &t.body { scan_instr(instr, scan); }
}

/// Visit every `Expr` reachable from `body` — `select=` clauses
/// on the instructions themselves and every `{…}` substitution
/// inside AVTs (attribute values, `name=` / `regex=` / etc.).
/// Used by analyses that need to know "does this body anywhere
/// reference function X / variable Y / etc."
pub fn walk_body<F: FnMut(&Expr)>(body: &[Instr], scan: &mut F) {
    for instr in body { scan_instr(instr, scan); }
}

fn scan_instr<F: FnMut(&Expr)>(instr: &Instr, scan: &mut F) {
    use Instr::*;
    match instr {
        LiteralText { .. } | Unsupported { .. } | ApplyImports { .. } => {}
        LiteralElement { attributes, body, .. } => {
            for (_, a) in attributes { scan_avt(a, scan); }
            for child in body { scan_instr(child, scan); }
        }
        ValueOf { select, .. } => scan(select),
        ValueOfBody { body, .. } => {
            for child in body { scan_instr(child, scan); }
        }
        Variable(v) => {
            if let Some(e) = &v.select { scan(e); }
            for child in &v.body { scan_instr(child, scan); }
        }
        Map { body } => {
            for child in body { scan_instr(child, scan); }
        }
        MapEntry { key, select, body } => {
            scan(key);
            if let Some(e) = select { scan(e); }
            for child in body { scan_instr(child, scan); }
        }
        ApplyTemplates { select, sort, with_params, .. } => {
            if let Some(e) = select { scan(e); }
            for s in sort {
                if let Some(a) = &s.lang { scan_avt(a, scan); }
                if let Some(a) = &s.data_type { scan_avt(a, scan); }
                if let Some(a) = &s.order     { scan_avt(a, scan); }
                if let Some(a) = &s.case_order{ scan_avt(a, scan); }
                if let Some(e) = &s.select    { scan(e); }
            }
            for w in with_params {
                if let Some(e) = &w.select { scan(e); }
                for child in &w.body { scan_instr(child, scan); }
            }
        }
        CallTemplate { with_params, .. } => {
            for w in with_params {
                if let Some(e) = &w.select { scan(e); }
                for child in &w.body { scan_instr(child, scan); }
            }
        }
        Evaluate { xpath, context_item, with_params } => {
            scan(xpath);
            if let Some(e) = context_item { scan(e); }
            for w in with_params {
                if let Some(e) = &w.select { scan(e); }
                for child in &w.body { scan_instr(child, scan); }
            }
        }
        SourceDocument { href, body } => {
            scan_avt(href, scan);
            for child in body { scan_instr(child, scan); }
        }
        ResultDocument { href, format, body, .. } => {
            scan_avt(href, scan);
            if let Some(f) = format { scan_avt(f, scan); }
            for child in body { scan_instr(child, scan); }
        }
        Fork { body } => { for child in body { scan_instr(child, scan); } }
        WherePopulated { body } => { for child in body { scan_instr(child, scan); } }
        OnEmpty { body } | OnNonEmpty { body } => { for child in body { scan_instr(child, scan); } }
        ForEach { select, sort, body } => {
            scan(select);
            for s in sort {
                if let Some(a) = &s.lang { scan_avt(a, scan); }
                if let Some(a) = &s.data_type { scan_avt(a, scan); }
                if let Some(a) = &s.order     { scan_avt(a, scan); }
                if let Some(a) = &s.case_order{ scan_avt(a, scan); }
                if let Some(e) = &s.select    { scan(e); }
            }
            for child in body { scan_instr(child, scan); }
        }
        If { test, body } => {
            scan(test);
            for child in body { scan_instr(child, scan); }
        }
        Choose { whens, otherwise } => {
            for (test, body) in whens {
                scan(test);
                for child in body { scan_instr(child, scan); }
            }
            if let Some(body) = otherwise {
                for child in body { scan_instr(child, scan); }
            }
        }
        Copy { body, .. } => for child in body { scan_instr(child, scan); }
        CopyOf { select, .. } => scan(select),
        Element { name, namespace, body, .. } => {
            scan_avt(name, scan);
            if let Some(n) = namespace { scan_avt(n, scan); }
            for child in body { scan_instr(child, scan); }
        }
        Attribute { name, namespace, body, .. } => {
            scan_avt(name, scan);
            if let Some(n) = namespace { scan_avt(n, scan); }
            for child in body { scan_instr(child, scan); }
        }
        Comment { select, body } => {
            if let Some(e) = select { scan(e); }
            for child in body { scan_instr(child, scan); }
        }
        ProcessingInstruction { name, select, body } => {
            scan_avt(name, scan);
            if let Some(e) = select { scan(e); }
            for child in body { scan_instr(child, scan); }
        }
        Number { value, count, from, format, grouping_separator, grouping_size,
                  ordinal, lang, letter_value, .. } => {
            if let Some(e) = value { scan(e); }
            if let Some(e) = count { scan(e); }
            if let Some(e) = from  { scan(e); }
            scan_avt(format, scan);
            if let Some(a) = grouping_separator { scan_avt(a, scan); }
            if let Some(a) = grouping_size      { scan_avt(a, scan); }
            if let Some(a) = ordinal      { scan_avt(a, scan); }
            if let Some(a) = lang         { scan_avt(a, scan); }
            if let Some(a) = letter_value { scan_avt(a, scan); }
        }
        Message { terminate, body } => {
            if let Some(a) = terminate { scan_avt(a, scan); }
            for child in body { scan_instr(child, scan); }
        }
        Fallback { body } => for child in body { scan_instr(child, scan); }
        Sequence { select } => scan(select),
        NextMatch { with_params } => {
            for w in with_params {
                if let Some(e) = &w.select { scan(e); }
                for child in &w.body { scan_instr(child, scan); }
            }
        }
        ForEachGroup { select, key, sort, body, .. } => {
            scan(select);
            scan(key);
            for s in sort {
                if let Some(e) = &s.select { scan(e); }
            }
            for child in body { scan_instr(child, scan); }
        }
        Merge { sources, action } => {
            for src in sources {
                scan(&src.select);
                if let Some(e) = &src.for_each_source { scan(e); }
                for k in &src.keys {
                    if let Some(e) = &k.select { scan(e); }
                }
            }
            for child in action { scan_instr(child, scan); }
        }
        AnalyzeString { select, regex, flags, matching, non_matching } => {
            scan(select);
            scan_avt(regex, scan);
            scan_avt(flags, scan);
            for child in matching     { scan_instr(child, scan); }
            for child in non_matching { scan_instr(child, scan); }
        }
        PerformSort { select, sort, body } => {
            if let Some(e) = select { scan(e); }
            for s in sort {
                if let Some(e) = &s.select { scan(e); }
                if let Some(a) = &s.lang { scan_avt(a, scan); }
                if let Some(a) = &s.data_type { scan_avt(a, scan); }
                if let Some(a) = &s.order     { scan_avt(a, scan); }
                if let Some(a) = &s.case_order{ scan_avt(a, scan); }
            }
            for child in body { scan_instr(child, scan); }
        }
        Document { body } => for child in body { scan_instr(child, scan); }
        Namespace { name, select, body } => {
            scan_avt(name, scan);
            if let Some(e) = select { scan(e); }
            for child in body { scan_instr(child, scan); }
        }
        Try { body, catches } => {
            for child in body { scan_instr(child, scan); }
            for c in catches {
                for child in &c.body { scan_instr(child, scan); }
            }
        }
        Iterate { select, params, on_completion, body } => {
            scan(select);
            for p in params {
                if let Some(e) = &p.select { scan(e); }
                for child in &p.body { scan_instr(child, scan); }
            }
            for child in on_completion { scan_instr(child, scan); }
            for child in body { scan_instr(child, scan); }
        }
        NextIteration { with_params } => {
            for w in with_params {
                if let Some(e) = &w.select { scan(e); }
                for child in &w.body { scan_instr(child, scan); }
            }
        }
        Break { select, body } => {
            if let Some(e) = select { scan(e); }
            for child in body { scan_instr(child, scan); }
        }
    }
}

fn scan_avt<F: FnMut(&Expr)>(avt: &Avt, scan: &mut F) {
    for part in &avt.parts {
        if let AvtPart::Expr(e) = part { scan(e); }
    }
}

fn collect_template(t: &Template, out: &mut Vec<String>) {
    if let Some(m) = &t.match_pattern {
        walk_expr(m, out);
    }
    for p in &t.params {
        collect_param(p, out);
    }
    walk_instrs(&t.body, out);
}

fn collect_param(p: &Param, out: &mut Vec<String>) {
    if let Some(e) = &p.select {
        walk_expr(e, out);
    }
    walk_instrs(&p.body, out);
}

fn collect_variable(v: &Variable, out: &mut Vec<String>) {
    if let Some(e) = &v.select {
        walk_expr(e, out);
    }
    walk_instrs(&v.body, out);
}

fn collect_with_param(w: &WithParam, out: &mut Vec<String>) {
    if let Some(e) = &w.select {
        walk_expr(e, out);
    }
    walk_instrs(&w.body, out);
}

fn collect_sort(s: &Sort, out: &mut Vec<String>) {
    if let Some(e) = &s.select {
        walk_expr(e, out);
    }
    if let Some(a) = &s.lang        { walk_avt(a, out); }
    if let Some(a) = &s.data_type   { walk_avt(a, out); }
    if let Some(a) = &s.order       { walk_avt(a, out); }
    if let Some(a) = &s.case_order  { walk_avt(a, out); }
}

fn walk_instrs(body: &[Instr], out: &mut Vec<String>) {
    for i in body {
        walk_instr(i, out);
    }
}

fn walk_instr(i: &Instr, out: &mut Vec<String>) {
    match i {
        Instr::LiteralElement { attributes, body, .. } => {
            for (_, avt) in attributes { walk_avt(avt, out); }
            walk_instrs(body, out);
        }
        Instr::LiteralText { .. } => {}
        Instr::Map { body } => walk_instrs(body, out),
        Instr::MapEntry { key, select, body } => {
            walk_expr(key, out);
            if let Some(e) = select { walk_expr(e, out); }
            walk_instrs(body, out);
        }
        Instr::ApplyTemplates { select, sort, with_params, .. } => {
            if let Some(e) = select { walk_expr(e, out); }
            for s in sort { collect_sort(s, out); }
            for w in with_params { collect_with_param(w, out); }
        }
        Instr::ApplyImports { with_params } => {
            for w in with_params { collect_with_param(w, out); }
        }
        Instr::CallTemplate { with_params, .. } => {
            for w in with_params { collect_with_param(w, out); }
        }
        Instr::Evaluate { xpath, context_item, with_params } => {
            walk_expr(xpath, out);
            if let Some(e) = context_item { walk_expr(e, out); }
            for w in with_params { collect_with_param(w, out); }
        }
        Instr::SourceDocument { href, body } => {
            walk_avt(href, out);
            walk_instrs(body, out);
        }
        Instr::ResultDocument { href, format, body, .. } => {
            walk_avt(href, out);
            if let Some(f) = format { walk_avt(f, out); }
            walk_instrs(body, out);
        }
        Instr::Fork { body } => walk_instrs(body, out),
        Instr::WherePopulated { body } => walk_instrs(body, out),
        Instr::OnEmpty { body } | Instr::OnNonEmpty { body } => walk_instrs(body, out),
        Instr::Choose { whens, otherwise } => {
            for (test, body) in whens {
                walk_expr(test, out);
                walk_instrs(body, out);
            }
            if let Some(body) = otherwise { walk_instrs(body, out); }
        }
        Instr::If { test, body } => {
            walk_expr(test, out);
            walk_instrs(body, out);
        }
        Instr::ForEach { select, sort, body } => {
            walk_expr(select, out);
            for s in sort { collect_sort(s, out); }
            walk_instrs(body, out);
        }
        Instr::ValueOf { select, .. } => walk_expr(select, out),
        Instr::ValueOfBody { body, .. } => walk_instrs(body, out),
        Instr::Copy { body, .. } => walk_instrs(body, out),
        Instr::CopyOf { select, .. } => walk_expr(select, out),
        Instr::Element { name, namespace, body, .. } => {
            walk_avt(name, out);
            if let Some(ns) = namespace { walk_avt(ns, out); }
            walk_instrs(body, out);
        }
        Instr::Attribute { name, namespace, body, .. } => {
            walk_avt(name, out);
            if let Some(ns) = namespace { walk_avt(ns, out); }
            walk_instrs(body, out);
        }
        Instr::Comment { select, body } => {
            if let Some(e) = select { walk_expr(e, out); }
            walk_instrs(body, out);
        }
        Instr::ProcessingInstruction { name, select, body } => {
            walk_avt(name, out);
            if let Some(e) = select { walk_expr(e, out); }
            walk_instrs(body, out);
        }
        Instr::Number { value, select, level: _, count, from, format,
                        grouping_separator, grouping_size,
                        ordinal, lang, letter_value, start_at } => {
            if let Some(e) = value  { walk_expr(e, out); }
            if let Some(e) = select { walk_expr(e, out); }
            if let Some(e) = count  { walk_expr(e, out); }
            if let Some(e) = from   { walk_expr(e, out); }
            walk_avt(format, out);
            if let Some(a) = grouping_separator { walk_avt(a, out); }
            if let Some(a) = grouping_size      { walk_avt(a, out); }
            if let Some(a) = ordinal      { walk_avt(a, out); }
            if let Some(a) = lang         { walk_avt(a, out); }
            if let Some(a) = letter_value { walk_avt(a, out); }
            if let Some(a) = start_at     { walk_avt(a, out); }
        }
        Instr::Variable(v) => collect_variable(v, out),
        Instr::Message { terminate, body } => {
            if let Some(a) = terminate { walk_avt(a, out); }
            walk_instrs(body, out);
        }
        Instr::Fallback { body } => walk_instrs(body, out),
        Instr::Unsupported { .. } => {}
        Instr::Sequence { select } => walk_expr(select, out),
        Instr::NextMatch { with_params } => {
            for w in with_params { collect_with_param(w, out); }
        }
        Instr::ForEachGroup { select, key, sort, body, .. } => {
            walk_expr(select, out);
            walk_expr(key, out);
            for s in sort { collect_sort(s, out); }
            walk_instrs(body, out);
        }
        Instr::Merge { sources, action } => {
            for src in sources {
                walk_expr(&src.select, out);
                if let Some(e) = &src.for_each_source { walk_expr(e, out); }
                for k in &src.keys { collect_sort(k, out); }
            }
            walk_instrs(action, out);
        }
        Instr::AnalyzeString { select, regex, flags, matching, non_matching } => {
            walk_expr(select, out);
            walk_avt(regex, out);
            walk_avt(flags, out);
            walk_instrs(matching, out);
            walk_instrs(non_matching, out);
        }
        Instr::PerformSort { select, sort, body } => {
            if let Some(e) = select { walk_expr(e, out); }
            for s in sort { collect_sort(s, out); }
            walk_instrs(body, out);
        }
        Instr::Document { body } => walk_instrs(body, out),
        Instr::Namespace { name, select, body } => {
            walk_avt(name, out);
            if let Some(e) = select { walk_expr(e, out); }
            walk_instrs(body, out);
        }
        Instr::Try { body, catches } => {
            walk_instrs(body, out);
            for c in catches { walk_instrs(&c.body, out); }
        }
        Instr::Iterate { select, params, on_completion, body } => {
            walk_expr(select, out);
            for p in params { collect_param(p, out); }
            walk_instrs(on_completion, out);
            walk_instrs(body, out);
        }
        Instr::NextIteration { with_params } => {
            for w in with_params { collect_with_param(w, out); }
        }
        Instr::Break { select, body } => {
            if let Some(e) = select { walk_expr(e, out); }
            walk_instrs(body, out);
        }
    }
}

/// True iff `body` contains, at any depth, an instruction that
/// dispatches to a template rule or named template
/// (`xsl:apply-templates`, `xsl:call-template`, `xsl:apply-imports`,
/// `xsl:next-match`).  The captured-substring context of an enclosing
/// `xsl:analyze-string` propagates into the called template (XSLT 2.0
/// §15.3), so a matching-substring body that delegates this way may
/// need the captured groups even when it doesn't name `regex-group`
/// directly.
pub fn body_invokes_templates(body: &[Instr]) -> bool {
    body.iter().any(instr_invokes_templates)
}

fn instr_invokes_templates(i: &Instr) -> bool {
    match i {
        Instr::ApplyTemplates { .. } | Instr::CallTemplate { .. }
        | Instr::ApplyImports { .. } | Instr::NextMatch { .. } => true,
        Instr::LiteralElement { body, .. }
        | Instr::Element { body, .. }
        | Instr::Attribute { body, .. }
        | Instr::Namespace { body, .. }
        | Instr::Copy { body, .. }
        | Instr::ValueOfBody { body, .. }
        | Instr::Comment { body, .. }
        | Instr::ProcessingInstruction { body, .. }
        | Instr::Message { body, .. }
        | Instr::Fallback { body }
        | Instr::Document { body }
        | Instr::Fork { body }
        | Instr::WherePopulated { body }
        | Instr::OnEmpty { body }
        | Instr::OnNonEmpty { body }
        | Instr::ForEach { body, .. }
        | Instr::ForEachGroup { body, .. }
        | Instr::PerformSort { body, .. }
        | Instr::SourceDocument { body, .. }
        | Instr::ResultDocument { body, .. }
        | Instr::Break { body, .. } => body_invokes_templates(body),
        Instr::If { body, .. } => body_invokes_templates(body),
        Instr::Choose { whens, otherwise } =>
            whens.iter().any(|(_, b)| body_invokes_templates(b))
            || otherwise.as_deref().map(body_invokes_templates).unwrap_or(false),
        Instr::AnalyzeString { matching, non_matching, .. } =>
            body_invokes_templates(matching) || body_invokes_templates(non_matching),
        Instr::Merge { action, .. } => body_invokes_templates(action),
        Instr::Try { body, catches } =>
            body_invokes_templates(body)
            || catches.iter().any(|c| body_invokes_templates(&c.body)),
        Instr::Iterate { on_completion, body, .. } =>
            body_invokes_templates(on_completion) || body_invokes_templates(body),
        _ => false,
    }
}

fn walk_avt(a: &Avt, out: &mut Vec<String>) {
    for part in &a.parts {
        if let AvtPart::Expr(e) = part {
            walk_expr(e, out);
        }
    }
}

fn walk_expr(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Or(l, r) | Expr::And(l, r)
        | Expr::Eq(l, r) | Expr::Ne(l, r)
        | Expr::Lt(l, r) | Expr::Gt(l, r) | Expr::Le(l, r) | Expr::Ge(l, r)
        | Expr::ValueEq(l, r) | Expr::ValueNe(l, r)
        | Expr::ValueLt(l, r) | Expr::ValueGt(l, r)
        | Expr::ValueLe(l, r) | Expr::ValueGe(l, r)
        | Expr::Add(l, r) | Expr::Sub(l, r) | Expr::Mul(l, r)
        | Expr::Div(l, r) | Expr::Mod(l, r) | Expr::Union(l, r) => {
            walk_expr(l, out);
            walk_expr(r, out);
        }
        Expr::Neg(e) => walk_expr(e, out),
        Expr::Path(p) => {
            use sup_xml_core::xpath::ast::LocationPath;
            match p {
                LocationPath::Absolute(steps) | LocationPath::Relative(steps) => {
                    for s in steps {
                        for pred in &s.predicates {
                            walk_expr(pred, out);
                        }
                    }
                }
            }
        }
        Expr::FilterPath { primary, predicates, steps } => {
            walk_expr(primary, out);
            for p in predicates { walk_expr(p, out); }
            for s in steps {
                for pred in &s.predicates {
                    walk_expr(pred, out);
                }
            }
        }
        Expr::FunctionCall(name, args) => {
            // Match unqualified `document(...)`, the explicit
            // empty-namespace form, the conventional XPath 2.0
            // prefixed form `fn:document(...)`, and the XPath 2.0
            // `doc(...)` accessor (whose single-argument literal form
            // pre-loads identically — `doc('')` is the stylesheet's
            // own document just as `document('')` is).
            // Only the 1-argument form is pre-loaded: a 2-argument
            // `document($rel, $base)` resolves $rel against $base's
            // (runtime) base URI, so its first-arg literal can't be
            // resolved statically — it loads dynamically instead.
            if matches!(name.as_str(),
                    "document" | "{}document" | "fn:document"
                  | "doc"      | "{}doc"      | "fn:doc")
                && args.len() == 1 {
                if let Expr::Literal(uri) = &args[0] {
                    // The empty URI is the spec's "this stylesheet"
                    // form and is resolved at apply time against the
                    // stylesheet's own base.  Include it in the
                    // preload list so the dispatcher has a real node
                    // to return.
                    out.push(uri.clone());
                }
            }
            for a in args { walk_expr(a, out); }
        }
        Expr::Variable(_) | Expr::Literal(_) | Expr::Integer(_) | Expr::Decimal(_) | Expr::Double(_) => {}
        Expr::IfThenElse { cond, then_branch, else_branch } => {
            walk_expr(cond, out);
            walk_expr(then_branch, out);
            walk_expr(else_branch, out);
        }
        Expr::For { bindings, body } | Expr::Let { bindings, body } => {
            for (_, e) in bindings { walk_expr(e, out); }
            walk_expr(body, out);
        }
        Expr::Range(a, b) | Expr::SimpleMap(a, b) | Expr::NodeBefore(a, b) | Expr::NodeAfter(a, b) | Expr::NodeIs(a, b) => {
            walk_expr(a, out);
            walk_expr(b, out);
        }
        Expr::Sequence(items) => for e in items { walk_expr(e, out); }
        Expr::Quantified { bindings, test, .. } => {
            for (_, e) in bindings { walk_expr(e, out); }
            walk_expr(test, out);
        }
        Expr::IDiv(a, b) | Expr::Intersect(a, b) | Expr::Except(a, b) => {
            walk_expr(a, out);
            walk_expr(b, out);
        }
        Expr::InstanceOf(a, _) | Expr::CastAs(a, _)
        | Expr::CastableAs(a, _) | Expr::TreatAs(a, _) => walk_expr(a, out),
        Expr::TryCatch { body, catches } => {
            walk_expr(body, out);
            for c in catches { walk_expr(&c.body, out); }
        }
        Expr::WithDefaultCollation(_, inner) => walk_expr(inner, out),
        Expr::BackwardsCompat(inner) => walk_expr(inner, out),
        Expr::MapConstructor(es) => for (k, v) in es {
            walk_expr(k, out); walk_expr(v, out);
        },
        Expr::ArrayConstructor { members, .. } =>
            for m in members { walk_expr(m, out); },
        Expr::Lookup(b, key) => {
            walk_expr(b, out);
            if let LookupKey::Expr(e) = key { walk_expr(e, out); }
        }
        Expr::UnaryLookup(key) =>
            if let LookupKey::Expr(e) = key { walk_expr(e, out); },
        Expr::InlineFunction { body, .. } => walk_expr(body, out),
        Expr::DynamicCall { func, args } => {
            walk_expr(func, out);
            for a in args { walk_expr(a, out); }
        }
        Expr::NamedFunctionRef { .. } | Expr::Placeholder | Expr::ContextItem => {}
    }
}
