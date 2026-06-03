//! XSLT-added XPath functions — XSLT 1.0 §12.
//!
//! These functions are available inside XPath expressions in
//! template bodies (`<xsl:value-of select="key('byid', @ref)"/>`,
//! `current()`, etc.).  They differ from EXSLT in that they live
//! in the default XPath namespace (no prefix required) and are
//! always wired by the XSLT runtime — there's no equivalent of
//! `exsltRegisterAll`.
//!
//! Coverage in this build:
//!
//! | Function              | Status            | Notes                                                 |
//! |-----------------------|-------------------|-------------------------------------------------------|
//! | `current()`           | implemented       | XSLT context node                                     |
//! | `generate-id()`       | implemented       | NodeId formatted as `idNNNN`                          |
//! | `system-property()`   | implemented       | `xsl:version`, `xsl:vendor`                           |
//! | `element-available()` | implemented       | introspects the instruction set                       |
//! | `function-available()`| implemented       | introspects the function set                          |
//! | `key()`               | implemented       | eagerly-built index; prefix resolved via static ns ctx |
//! | `format-number()`     | implemented       | named `<xsl:decimal-format>` lookup wired             |
//! | `document()`          | implemented       | string-literal URIs only — see [`crate::walk`]        |
//! | `unparsed-entity-uri` | implemented       | source doc's parsed DTD entries surfaced              |

use std::collections::HashMap;

use sup_xml_core::error::{ErrorDomain, ErrorLevel, XmlError};
use sup_xml_core::xpath::eval::{Value, value_to_number, value_to_string};
use sup_xml_core::xpath::{DocIndex, DocIndexLike, NodeId};

use crate::ast::{QName, StylesheetAst};
use crate::format_number::{format_number, DecimalFormat};

type Result<T> = std::result::Result<T, XmlError>;

fn err(msg: impl Into<String>) -> XmlError {
    XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg)
}

// ── key index ─────────────────────────────────────────────────────

/// Eagerly-built lookup table for `<xsl:key>`.  Built once per
/// transformation across every document the engine knows about
/// (the source plus everything `document()` pre-loaded).  Bucketed
/// by `(key-name, doc-root, value)` so `key()` returns only nodes
/// in the same document as the calling context, per XSLT 1.0 §12.2.
pub struct KeyIndex {
    entries: HashMap<(String, NodeId, String), Vec<NodeId>>,
    /// Clark-form expanded names of every declared `<xsl:key>`.
    /// Used by `key()` to raise XTDE1260 on lookups whose name
    /// doesn't match any declaration — even when the key name has
    /// no actual indexed entries.
    declared: std::collections::HashSet<String>,
    /// Per-key-name effective collation URI.  Same-name xsl:key
    /// declarations agree (XTSE1220 enforces this).  The default
    /// codepoint collation isn't recorded; only entries needing
    /// non-codepoint folding live here.
    collations: HashMap<String, String>,
}

impl KeyIndex {
    pub fn new() -> Self {
        KeyIndex {
            entries:    HashMap::new(),
            declared:   std::collections::HashSet::new(),
            collations: HashMap::new(),
        }
    }

    /// Apply the collation registered for `name_key` (if any) to
    /// the bucket-string `s`.  The default codepoint collation
    /// returns `s` unchanged; the HTML ASCII case-insensitive
    /// collation lower-cases A-Z.
    fn fold_for(&self, name_key: &str, s: &str) -> String {
        match self.collations.get(name_key) {
            Some(uri) if uri ==
                "http://www.w3.org/2005/xpath-functions/collation/html-ascii-case-insensitive"
                => {
                    let mut out = String::with_capacity(s.len());
                    for c in s.chars() {
                        if c.is_ascii_uppercase() { out.push(c.to_ascii_lowercase()); }
                        else                       { out.push(c); }
                    }
                    out
                }
            _ => s.to_string(),
        }
    }

    /// True iff `name_key` (Clark-form expanded name) names a
    /// declared `<xsl:key>`.  Distinct from `entries.contains_key`,
    /// which only sees keys with at least one indexed value.
    pub fn is_declared(&self, name_key: &str) -> bool {
        self.declared.contains(name_key)
    }

    /// Build the index by walking every node in `idx` against
    /// every `xsl:key` declaration in `style`.  Each entry records
    /// the matching node AND its document root, so `lookup` can
    /// constrain results to the caller's document.
    ///
    /// `eval_one` is the closure that evaluates `expr` at a given
    /// context node — wraps the engine's XPath eval with the right
    /// bindings.  We don't construct the bindings here to keep
    /// this module dependency-light; the caller owns binding setup.
    /// Bucket `value` under `name_key` for `node_id`, applying the
    /// per-key collation fold and document-root partitioning.  Used
    /// by the deferred body-form key pass (XSLT 2.0 §16.3), whose
    /// values are computed by the instruction evaluator rather than a
    /// pure `use=` XPath expression.
    pub fn add_value<I: DocIndexLike>(
        &mut self, name_key: &str, node_id: NodeId, value: &Value, idx: &I,
    ) {
        let doc_root = doc_root_of(idx, node_id);
        for s in stringify_use_value(value, idx) {
            let folded = self.fold_for(name_key, &s);
            self.entries.entry((name_key.to_string(), doc_root, folded))
                .or_default()
                .push(node_id);
        }
    }

    /// Build the index for every `use=`-attribute key.  Keys declared
    /// with a sequence-constructor body (XSLT 2.0 §16.3) are returned
    /// as `(key-index, node-id)` deferrals: their values need the full
    /// instruction evaluator, which the caller drives via
    /// [`KeyIndex::add_value`].
    pub fn build<F>(
        style:   &StylesheetAst,
        idx:     &DocIndex<'_>,
        mut eval_one: F,
    ) -> Result<(Self, Vec<(usize, NodeId)>)>
    where
        F: FnMut(&sup_xml_core::xpath::Expr, NodeId) -> Result<Value>,
    {
        let mut entries: HashMap<(String, NodeId, String), Vec<NodeId>> = HashMap::new();
        let declared: std::collections::HashSet<String> =
            style.keys.iter().map(|k| qname_key(&k.name)).collect();
        let mut collations: HashMap<String, String> = HashMap::new();
        for key in &style.keys {
            if let Some(uri) = &key.collation {
                if !uri.is_empty() && uri !=
                    "http://www.w3.org/2005/xpath-functions/collation/codepoint"
                {
                    collations.insert(qname_key(&key.name), uri.clone());
                }
            }
        }
        // Capture the collation map BEFORE indexing so the fold
        // helper has the right context.  Build a throwaway index so
        // we can reuse `fold_for` while populating `entries`.
        let folder = KeyIndex {
            entries:    HashMap::new(),
            declared:   declared.clone(),
            collations: collations.clone(),
        };
        let mut deferred: Vec<(usize, NodeId)> = Vec::new();
        for (ki, key) in style.keys.iter().enumerate() {
            let nk = qname_key(&key.name);
            for node_id in 0..idx.nodes.len() {
                if !pattern_matches(&key.matcher, node_id, idx, &mut eval_one)? {
                    continue;
                }
                if !key.body.is_empty() {
                    // Sequence-constructor key: value computed later by
                    // the caller via the instruction evaluator.
                    deferred.push((ki, node_id));
                    continue;
                }
                let value    = eval_one(&key.use_, node_id)?;
                let doc_root = doc_root_of(idx, node_id);
                let strings  = stringify_use_value(&value, idx);
                for s in strings {
                    let folded = folder.fold_for(&nk, &s);
                    entries.entry((nk.clone(), doc_root, folded))
                        .or_default()
                        .push(node_id);
                }
            }
        }
        Ok((KeyIndex { entries, declared, collations }, deferred))
    }

    pub fn lookup(&self, name_key: &str, doc_root: NodeId, value: &str) -> Vec<NodeId> {
        let folded = self.fold_for(name_key, value);
        self.entries.get(&(name_key.to_string(), doc_root, folded))
            .cloned().unwrap_or_default()
    }
}

/// Walk up parents from `node` until a node with no parent is reached
/// — its `NodeId` is the document root.  XSLT 1.0 §12.2 partitions
/// keys per document, so we bucket each indexed node by its root.
fn doc_root_of<I: DocIndexLike>(idx: &I, node: NodeId) -> NodeId {
    let mut cur = node;
    while let Some(p) = idx.parent(cur) { cur = p; }
    cur
}

/// XSLT 1.0 §12.2 — when `use=` evaluates to a nodeset, each
/// node's string-value is a separate key value (the same source
/// node can index under multiple values).  XSLT 2.0 generalises
/// this to any sequence: each item atomises to its string and
/// becomes a separate key.  Other types produce one string key.
///
/// F&O §15.1 — xs:double/float NaN never compares equal (to itself
/// or anything else), so a NaN key value is silently dropped from
/// both the index (so a NaN-bearing node is not bucketed) and the
/// lookup (so `key('k', NaN)` always returns the empty sequence).
fn stringify_use_value<I: DocIndexLike>(v: &Value, idx: &I) -> Vec<String> {
    let strings: Vec<String> = match v {
        Value::NodeSet(ns) => ns.iter().map(|&id| idx.string_value(id)).collect(),
        Value::Sequence(items) => items.iter()
            .map(|it| canonical_key_string(it, idx)).collect(),
        Value::IntRange { lo, hi } => (*lo..=*hi).map(|n| n.to_string()).collect(),
        other              => vec![canonical_key_string(other, idx)],
    };
    strings.into_iter().filter(|s| s != "NaN").collect()
}

/// XSLT 2.0 §16.4 — `xsl:key use=` and the `key()` lookup compare
/// by *value*, not by raw lexical form.  Typed values therefore
/// need a canonical bucket key: dates/times normalise to UTC
/// microseconds since epoch (so `2002-10-06T03:00:00Z` matches
/// `2002-10-05T23:00:00-04:00`); durations collapse to total
/// seconds / months; numerics fold through XSD's canonical
/// decimal form.  Everything else stringifies as today.
pub(crate) fn canonical_key_string<I: DocIndexLike>(v: &Value, idx: &I) -> String {
    use sup_xml_core::xpath::eval::{date_value_to_utc_micros, DateKind};
    match v {
        Value::Typed(t) => match t.kind {
            "date" | "gYear" | "gYearMonth" | "gMonth" | "gMonthDay" | "gDay" =>
                date_value_to_utc_micros(&t.lexical, DateKind::Date)
                    .map(|us| format!("D{us}"))
                    .unwrap_or_else(|| t.lexical.clone()),
            "dateTime" => date_value_to_utc_micros(&t.lexical, DateKind::DateTime)
                .map(|us| format!("T{us}"))
                .unwrap_or_else(|| t.lexical.clone()),
            "time"     => date_value_to_utc_micros(&t.lexical, DateKind::Time)
                .map(|us| format!("H{us}"))
                .unwrap_or_else(|| t.lexical.clone()),
            "boolean"  => t.lexical.clone(),
            _ => {
                // Numeric typed atomics expose a cached f64 in `numeric`;
                // route through it so xs:integer/decimal/double/float all
                // hash under the same canonical numeric form.
                if let Some(n) = t.numeric {
                    return canonical_numeric_key(n);
                }
                t.lexical.clone()
            }
        }
        Value::Number(n) => canonical_numeric_key(n.as_f64()),
        other            => value_to_string(other, idx),
    }
}

/// Canonical numeric bucket key — folds `10`, `10.0`, `10.00`,
/// `0010`, `1e1` all to the same string so xs:integer / xs:decimal
/// / xs:double keys interop correctly via `key()`.  NaN / ±INF
/// keep their distinct names so equality is preserved.
fn canonical_numeric_key(n: f64) -> String {
    if n.is_nan()       { return "NaN".into(); }
    if n.is_infinite()  { return if n > 0.0 { "INF".into() } else { "-INF".into() }; }
    if n == 0.0         { return "0".into(); }
    if n.fract() == 0.0 && n.abs() < 1e18 {
        return (n as i64).to_string();
    }
    // Non-integral finite — use Rust's default f64 formatting,
    // which already strips trailing zeros and emits a sane
    // canonical form ("10.5" not "10.500000").
    format!("{n}")
}

fn pattern_matches<F>(
    pattern: &sup_xml_core::xpath::Expr,
    node: NodeId,
    idx: &DocIndex<'_>,
    eval_one: &mut F,
) -> Result<bool>
where
    F: FnMut(&sup_xml_core::xpath::Expr, NodeId) -> Result<Value>,
{
    // Walk ancestor-or-self chain — see `pattern::matches` for the
    // rationale.  Reimplemented inline because eval_one is a
    // closure, not a Bindings trait object.
    let mut cur = Some(node);
    while let Some(ctx) = cur {
        let v = eval_one(pattern, ctx)?;
        if let Value::NodeSet(ns) = v {
            if ns.contains(&node) {
                return Ok(true);
            }
        }
        cur = idx.parent(ctx);
    }
    Ok(false)
}

fn qname_key(q: &QName) -> String {
    if q.uri.is_empty() { q.local.clone() }
    else { format!("{{{}}}{}", q.uri, q.local) }
}

// ── function dispatch ────────────────────────────────────────────

/// Try to dispatch `name(args)` to an XSLT-built-in function.
/// Returns `Some(result)` if the name is recognised, `None`
/// otherwise (caller falls through to EXSLT / built-ins).
///
/// `xslt_context_node` is the node `current()` should return — the
/// caller threads this through (it's the XSLT instruction-level
/// context, not the XPath inner-predicate context).
///
/// `keys` is the prebuilt key index — `None` if the stylesheet
/// has no `<xsl:key>` declarations (callers can save the work).
pub(crate) fn dispatch<I: DocIndexLike>(
    name:               &str,
    args:               Vec<Value>,
    idx:                &I,
    xslt_context_node:  NodeId,
    xpath_context_node: NodeId,
    keys:               Option<&KeyIndex>,
    instruction_names:  &[&str],
    documents:          Option<&HashMap<String, NodeId>>,
    dyn_doc_loader:     Option<&dyn Fn(&str) -> Option<std::result::Result<NodeId, sup_xml_core::error::XmlError>>>,
    decimal_formats:    &HashMap<String, DecimalFormat>,
    namespaces:         &crate::eval::NamespaceContext,
    unparsed_entities:  &HashMap<String, sup_xml_tree::UnparsedEntity>,
    current_group:      Option<&[NodeId]>,
    current_grouping_key: Option<&Value>,
    regex_groups:       Option<&[String]>,
    unparsed_texts:     Option<&HashMap<String, String>>,
    user_functions:     &[crate::ast::UserFunction],
    xslt_version:       &str,
    accumulators:       Option<&HashMap<String, crate::eval::AccumulatorData>>,
) -> Option<Result<Value>> {
    let r = match name {
        "current" => current_fn(&args, xslt_context_node),
        "accumulator-before" =>
            accumulator_fn(&args, idx, namespaces, accumulators, xpath_context_node, true),
        "accumulator-after" =>
            accumulator_fn(&args, idx, namespaces, accumulators, xpath_context_node, false),
        // generate-id() with no argument defaults to the *XPath*
        // context node (XSLT 1.0 §12.4), not the XSLT current node.
        // The two differ inside predicate sub-expressions and any
        // step that re-sets the context.
        "generate-id" => generate_id_fn(&args, xpath_context_node),
        "system-property" => system_property_fn(&args, idx, xslt_version, namespaces),
        "element-available" => element_available_fn(&args, idx, instruction_names, xslt_version),
        "function-available" => function_available_fn(&args, idx, namespaces, user_functions),
        "key" => key_fn(&args, idx, keys, namespaces, xpath_context_node, xslt_version),
        "format-number"     => format_number_fn(&args, idx, decimal_formats, namespaces),
        "document"          => document_fn(&args, idx, documents, dyn_doc_loader),
        "json-to-xml"       => json_to_xml_fn(&args, idx),
        "json-doc"          => json_doc_fn(&args, idx, unparsed_texts),
        "unparsed-entity-uri" => unparsed_entity_uri_fn(
            &args, idx, unparsed_entities, xpath_context_node),
        "unparsed-entity-public-id" =>
            unparsed_entity_public_id_fn(
                &args, idx, unparsed_entities, xpath_context_node),
        // XSLT 2.0 `xsl:for-each-group` accessors — both return the
        // empty sequence outside a group, matching Saxon's behaviour.
        "current-group" => {
            if !args.is_empty() {
                return Some(Err(err("current-group() takes no arguments")));
            }
            Ok(Value::NodeSet(current_group.map(|g| g.to_vec()).unwrap_or_default()))
        }
        "current-grouping-key" => {
            if !args.is_empty() {
                return Some(Err(err("current-grouping-key() takes no arguments")));
            }
            Ok(current_grouping_key.cloned().unwrap_or(Value::String(String::new())))
        }
        // XSLT 3.0 §15 `xsl:merge` accessors.  The current merge group
        // and key reuse the grouping-accessor state (a merge-action and
        // a for-each-group body are never active at the same time).  An
        // optional source-name argument to current-merge-group() is
        // accepted but the full group is returned (no per-source split).
        "current-merge-group" => {
            if args.len() > 1 {
                return Some(Err(err("current-merge-group() takes at most one argument")));
            }
            Ok(Value::NodeSet(current_group.map(|g| g.to_vec()).unwrap_or_default()))
        }
        "current-merge-key" => {
            if !args.is_empty() {
                return Some(Err(err("current-merge-key() takes no arguments")));
            }
            Ok(current_grouping_key.cloned().unwrap_or(Value::String(String::new())))
        }
        // XSLT 2.0 § 15.1 `regex-group(n)` — captured group `n`
        // inside an `xsl:matching-substring` body.  Group 0 is the
        // whole match.  Out-of-range / outside-matching returns "".
        "regex-group" => {
            if args.len() != 1 {
                return Some(Err(err("regex-group() requires one argument")));
            }
            let n = value_to_number(&args[0], idx).round() as i64;
            let s = if n < 0 { String::new() } else {
                regex_groups.and_then(|g| g.get(n as usize)).cloned().unwrap_or_default()
            };
            Ok(Value::String(s))
        }
        // XSLT 2.0 §16.6: unparsed-text and friends.  URI must have
        // been pre-loaded by the runtime (the engine scans the
        // stylesheet for static string-literal arguments and loads
        // those at apply time); dynamic URIs that didn't make it
        // into the pool surface as the empty result that
        // unparsed-text-available reports as `false`.
        "unparsed-text" => {
            if args.is_empty() || args.len() > 2 {
                return Some(Err(err("unparsed-text() takes 1 or 2 arguments")));
            }
            let uri = value_to_string(&args[0], idx);
            // Second argument (encoding) is accepted but ignored —
            // pre-loading happens at apply time before this call
            // and the loader chooses an encoding then.
            match unparsed_texts.and_then(|m| m.get(&uri)) {
                Some(text) => Ok(Value::String(text.clone())),
                None       => Err(err(format!(
                    "unparsed-text({uri:?}): resource not pre-loaded; \
                     only static string-literal URIs are supported"
                ))),
            }
        }
        "unparsed-text-available" => {
            if args.is_empty() || args.len() > 2 {
                return Some(Err(err("unparsed-text-available() takes 1 or 2 arguments")));
            }
            let uri = value_to_string(&args[0], idx);
            Ok(Value::Boolean(unparsed_texts.is_some_and(|m| m.contains_key(&uri))))
        }
        "unparsed-text-lines" => {
            if args.is_empty() || args.len() > 2 {
                return Some(Err(err("unparsed-text-lines() takes 1 or 2 arguments")));
            }
            let uri = value_to_string(&args[0], idx);
            let Some(text) = unparsed_texts.and_then(|m| m.get(&uri)) else {
                return Some(Err(err(format!(
                    "unparsed-text-lines({uri:?}): resource not pre-loaded"
                ))));
            };
            // XPath 2.0 §16.6.7 — split on `\r\n`, `\r`, or `\n`;
            // each segment becomes a string item in the returned
            // sequence.  An empty file gives the empty sequence.
            let mut lines: Vec<Value> = Vec::new();
            let mut last = 0usize;
            let bytes = text.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                let (skip, end) = match bytes[i] {
                    b'\r' if i + 1 < bytes.len() && bytes[i + 1] == b'\n' => (2, i),
                    b'\r' | b'\n' => (1, i),
                    _ => { i += 1; continue; }
                };
                lines.push(Value::String(text[last..end].to_string()));
                i += skip;
                last = i;
            }
            if last < text.len() {
                lines.push(Value::String(text[last..].to_string()));
            }
            // XPath 2.0 represents sequences as Value::NodeSet for
            // node sequences only; for atomic sequences we just
            // return the last item (the engine doesn't model atomic
            // sequences yet).  Returning a joined string preserves
            // backward compatibility for the common
            // `string-join(unparsed-text-lines(…), …)` idiom while
            // surfacing all line content.  TODO once atomic
            // sequences land.
            let joined = lines.into_iter()
                .map(|v| if let Value::String(s) = v { s } else { String::new() })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(Value::String(joined))
        }
        _ => return None,
    };
    Some(r)
}

/// XSLT 1.0 §12.4 — return the SYSTEM identifier for the unparsed
/// external entity named by the (string) argument, or the empty
/// string when no such entity is declared in the source document.
fn unparsed_entity_uri_fn<I: DocIndexLike>(
    args:    &[Value],
    idx:     &I,
    table:   &HashMap<String, sup_xml_tree::UnparsedEntity>,
    context_node: NodeId,
) -> Result<Value> {
    if args.len() != 1 {
        return Err(err("unparsed-entity-uri() requires exactly 1 argument"));
    }
    // XSLT 2.0 §16.6.3 / XTDE1370 — the function reads from the tree
    // rooted at the context node; calling it where the focus is
    // undefined (e.g. inside an xsl:function body), or where that
    // tree's root is not a document node, is a dynamic error.  A
    // contained path step that explicitly re-defines the focus
    // (`$nodes/unparsed-entity-uri(...)`) clears the XPath flag so
    // the call still works.
    if crate::eval::is_context_undefined()
        && sup_xml_core::xpath::eval::focus_is_undefined()
    {
        return Err(err(
            "unparsed-entity-uri() called with no context (XTDE1370)"));
    }
    if !root_is_document(idx, context_node) {
        return Err(err(
            "unparsed-entity-uri(): the tree containing the context node is \
             not rooted at a document node (XTDE1370)"));
    }
    let name = value_to_string(&args[0], idx);
    // The table already holds SYSTEM ids resolved against the source
    // document's base URI (done once at apply time).
    Ok(Value::String(
        table.get(&name).map(|e| e.system_id.clone()).unwrap_or_default()))
}

/// Walk the ancestor chain of `node` and report whether the root of
/// that tree is a document node.  Used by `unparsed-entity-uri()` and
/// `unparsed-entity-public-id()` to enforce XTDE1370 / XTDE1380, and
/// by `key()` for XTDE1270.  Synthetic RTF doc-wraps that hold the
/// items of a sequence-typed XSLT binding are NOT real documents per
/// XSLT 2.0 §5.7.2 — those items are parentless and reading their
/// tree's root must return the item itself, not the storage wrap.
fn root_is_document<I: DocIndexLike>(idx: &I, node: NodeId) -> bool {
    let mut cur = node;
    while let Some(p) = idx.parent(cur) { cur = p; }
    matches!(idx.kind(cur), sup_xml_core::xpath::XPathNodeKind::Document)
        && !idx.is_synthetic_wrap(cur)
}

/// XSLT 2.0 §16.6.3 `unparsed-entity-public-id(name)` — the PUBLIC
/// identifier of the named unparsed external entity, or the empty
/// string when there is no such entity or it had no PUBLIC id.
fn unparsed_entity_public_id_fn<I: DocIndexLike>(
    args:    &[Value],
    idx:     &I,
    table:   &HashMap<String, sup_xml_tree::UnparsedEntity>,
    context_node: NodeId,
) -> Result<Value> {
    if args.len() != 1 {
        return Err(err("unparsed-entity-public-id() requires exactly 1 argument"));
    }
    // XSLT 2.0 §16.6.3 / XTDE1380 — same context requirement as
    // unparsed-entity-uri(); raise here when the focus is undefined
    // or the tree's root isn't a document node.  A contained path
    // step that explicitly re-defines the focus clears the XPath
    // flag so the call still works.
    if crate::eval::is_context_undefined()
        && sup_xml_core::xpath::eval::focus_is_undefined()
    {
        return Err(err(
            "unparsed-entity-public-id() called with no context (XTDE1380)"));
    }
    if !root_is_document(idx, context_node) {
        return Err(err(
            "unparsed-entity-public-id(): the tree containing the context \
             node is not rooted at a document node (XTDE1380)"));
    }
    let name = value_to_string(&args[0], idx);
    Ok(Value::String(
        table.get(&name).and_then(|e| e.public_id.clone()).unwrap_or_default()))
}

/// Strip a fragment identifier (`#...`) from a document URI.  A
/// fragment plays no part in retrieving the resource or in document
/// identity, so `document('a.xml#x')` and `document('a.xml#y')` denote
/// the same node (XSLT 1.0 §12.1; XPath 2.0 `fn:doc` likewise resolves
/// the absolute URI without its fragment).
pub(crate) fn strip_uri_fragment(uri: &str) -> &str {
    uri.split_once('#').map(|(base, _)| base).unwrap_or(uri)
}

/// `document(uri-string)` and `document(uri-string, nodeset)` —
/// XSLT 1.0 §12.1.
///
/// **Scope (v1):** Only the string-literal-URI form is supported.
/// The engine pre-loads every URI that appears as a string literal
/// in `document(...)` calls during stylesheet compilation; this
/// function looks the URI up in that table.  Dynamic forms
/// (`document(@href)`, `document(concat(...))`, node-set first arg,
/// empty-string URI for "this stylesheet") raise a clear error
/// pointing the user at the limitation.
///
/// The second argument (base URI override) is accepted but ignored
/// — pre-loading happens with the stylesheet's base, so dynamic
/// rebasing isn't meaningful here.
fn document_fn<I: DocIndexLike>(
    args:      &[Value],
    idx:       &I,
    documents: Option<&HashMap<String, NodeId>>,
    dyn_loader: Option<&dyn Fn(&str) -> Option<std::result::Result<NodeId, sup_xml_core::error::XmlError>>>,
) -> Result<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(err("document() requires 1 or 2 arguments"));
    }
    // Empty map = no URIs were pre-loaded — distinct from "Loader was
    // never supplied" (the upstream caller always hands us at least a
    // `NullLoader`).  Surface the empty-map case as a not-pre-loaded
    // error per URI below, which carries the actual URI in the
    // diagnostic.
    let documents = documents.ok_or_else(|| err(
        "document(): the XSLT engine doesn't currently expose a \
         document-loading hook to this dispatcher"
    ))?;
    // XSLT 1.0 §12.1: each input node's string-value becomes a URI
    // to load; the result is the union of all loaded doc roots.
    // String / number / boolean first args collapse to a one-element
    // URI list.  Caller pre-loads all candidate URIs into `documents`.
    //
    // The `from_node_set` flag distinguishes the dynamic form (one
    // URI per node) from the static-string form.  Empty URIs read
    // from nodes silently produce nothing — matching Saxon's
    // behaviour and avoiding spurious "stylesheet self-reference"
    // for incidental empty node values.  An *explicit* empty-string
    // argument is the spec's "this stylesheet" form and is looked
    // up directly in the pre-loaded map.
    let (uris, from_node_set): (Vec<String>, bool) = match &args[0] {
        Value::String(s)   => (vec![s.clone()], false),
        Value::Number(n)   => (vec![format!("{}", n.as_f64())], false),
        Value::Boolean(b)  =>
            (vec![if *b { "true".to_string() } else { "false".to_string() }], false),
        Value::NodeSet(ns) =>
            (ns.iter().map(|&id| idx.string_value(id)).collect(), true),
        Value::ForeignNodeSet(_) => return Err(err(
            "document(): foreign-node-set first argument not yet supported"
        )),
        Value::Typed(t)    => (vec![t.lexical.clone()], false),
        Value::IntRange { lo, hi } =>
            ((*lo..=*hi).map(|i| i.to_string()).collect(), false),
        // Mixed / atomic sequence: stringify each item, treat as a
        // node-set-driven dynamic URI list.
        Value::Sequence(items) => {
            let strs: Vec<String> = items.iter().map(|v| match v {
                Value::String(s)  => s.clone(),
                Value::Number(n)  => format!("{}", n.as_f64()),
                Value::Boolean(b) => if *b { "true".into() } else { "false".into() },
                Value::Typed(t)   => t.lexical.clone(),
                Value::NodeSet(ns) => ns.first()
                    .map(|&id| idx.string_value(id)).unwrap_or_default(),
                _ => String::new(),
            }).collect();
            (strs, true)
        }
        // A map / array is not a valid document() argument.
        Value::Map(_) | Value::Array(_) | Value::Function(_) => (Vec::new(), false),
    };
    let mut out: Vec<NodeId> = Vec::new();
    let mut seen: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    for uri in uris {
        if uri.is_empty() && from_node_set { continue; }
        let key = strip_uri_fragment(&uri);
        match documents.get(key).copied() {
            Some(id) => {
                if seen.insert(id) { out.push(id); }
            }
            None => {
                // Static pre-load missed.  Try the runtime
                // dynamic-doc loader if the bindings layer supplied
                // one — that path resolves the URI via the active
                // Loader, parses the resource, and grafts it into
                // the DocIndex via `DocIndex::graft_dynamic_document`.
                if let Some(load) = dyn_loader {
                    match load(key) {
                        Some(Ok(id)) => {
                            if seen.insert(id) { out.push(id); }
                            continue;
                        }
                        Some(Err(e)) => return Err(e),
                        None => {}
                    }
                }
                return Err(err(format!(
                    "document('{uri}'): URI not pre-loaded and no \
                     runtime loader is available to fetch it.  \
                     Stylesheet::apply (no loader) never pre-loads \
                     anything; the LoaderImpl passed to \
                     apply_with_loader must implement enumeration \
                     or the URI must appear as a string literal in \
                     the stylesheet."
                )));
            }
        }
    }
    Ok(Value::NodeSet(out))
}

// ── JSON (XPath 3.1 §17.5) ────────────────────────────────────────

/// Read a string option from a `map(*)` options argument.
fn json_opt_str<I: DocIndexLike>(opts: Option<&Value>, key: &str, idx: &I) -> Option<String> {
    match opts {
        Some(Value::Map(m)) => m.iter()
            .find(|(k, _)| value_to_string(k, idx) == key)
            .map(|(_, v)| value_to_string(v, idx)),
        _ => None,
    }
}

/// `fn:json-doc($href [, $options])` — load the pre-fetched resource
/// text and parse it as JSON.  The text must have been registered via
/// the runtime's unparsed-text pool (static string-literal URIs).
fn json_doc_fn<I: DocIndexLike>(
    args: &[Value], idx: &I, unparsed_texts: Option<&HashMap<String, String>>,
) -> Result<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(err("json-doc() takes 1 or 2 arguments"));
    }
    if matches!(&args[0], Value::NodeSet(n) if n.is_empty())
        || matches!(&args[0], Value::Sequence(s) if s.is_empty())
    {
        return Ok(Value::Sequence(Vec::new()));
    }
    let href = value_to_string(&args[0], idx);
    let Some(text) = unparsed_texts.and_then(|m| m.get(&href)) else {
        return Err(err(format!(
            "json-doc({href:?}): resource not pre-loaded; only static \
             string-literal URIs are supported (FOUT1170)")));
    };
    let opts = args.get(1);
    use sup_xml_core::xpath::eval::json_option_bool;
    let dup = json_opt_str(opts, "duplicates", idx).unwrap_or_else(|| "use-first".into());
    let escape = json_option_bool(opts, "escape", idx)?.unwrap_or(false);
    let liberal = json_option_bool(opts, "liberal", idx)?.unwrap_or(false);
    sup_xml_core::xpath::eval::parse_json_value(text, &dup, escape, liberal)
}

/// `fn:xml-to-json` lives in core (reads a node, returns a string), but
/// `fn:json-to-xml($json [, $options])` constructs nodes, so it lives
/// here where the result-tree builder and RTF grafting are available.
/// Produces the F&O JSON element vocabulary in the functions namespace,
/// preserving the input's number lexicals.
fn json_to_xml_fn<I: DocIndexLike>(args: &[Value], idx: &I) -> Result<Value> {
    use crate::result_tree::ResultBuilder;
    use crate::ast::QName;
    use sup_xml_core::xpath::eval::{parse_json_events, JsonEvent};
    const FN_NS: &str = "http://www.w3.org/2005/xpath-functions";

    if args.is_empty() || args.len() > 2 {
        return Err(err("json-to-xml() takes 1 or 2 arguments"));
    }
    if matches!(&args[0], Value::NodeSet(n) if n.is_empty())
        || matches!(&args[0], Value::Sequence(s) if s.is_empty())
    {
        return Ok(Value::Sequence(Vec::new()));
    }
    let text = value_to_string(&args[0], idx);
    let opts = args.get(1);
    use sup_xml_core::xpath::eval::json_option_bool;
    let escape = match json_option_bool(opts, "escape", idx) { Ok(v) => v.unwrap_or(false), Err(e) => return Err(e) };
    let liberal = match json_option_bool(opts, "liberal", idx) { Ok(v) => v.unwrap_or(false), Err(e) => return Err(e) };
    // `validate` is accepted (we don't schema-validate) but its value
    // is still type-checked; `fallback` must be a function.
    if let Err(e) = json_option_bool(opts, "validate", idx) { return Err(e); }
    if let Some(Value::Map(m)) = opts {
        if let Some((_, v)) = m.iter().find(|(k, _)| value_to_string(k, idx) == "fallback") {
            if !matches!(v, Value::Function(_)) {
                return Err(err("JSON option 'fallback' must be a function (FOJS0005)"));
            }
        }
    }

    let fo = |local: &str| QName { prefix: None, local: local.into(), uri: FN_NS.into() };
    let plain = |local: &str| QName { prefix: None, local: local.into(), uri: String::new() };

    let mut b = ResultBuilder::new();
    let mut key: Option<String> = None;
    // Open an FO element, consuming a pending object key as `key`.
    let open = |b: &mut ResultBuilder, key: &mut Option<String>, local: &str| {
        b.open_element(fo(local));
        // Declare the JSON namespace as the default; push_namespace_decl
        // skips it on descendants where it's already in scope, so it
        // lands once on the root element of the result.
        b.push_namespace_decl(None, FN_NS.to_string());
        if let Some(k) = key.take() {
            if escape { b.push_attribute(plain("escaped-key"), "true".into()); }
            b.push_attribute(plain("key"), k);
        }
    };
    let parse = parse_json_events(&text, escape, liberal, &mut |ev| match ev {
        JsonEvent::Key(k) => key = Some(k),
        JsonEvent::StartObject => open(&mut b, &mut key, "map"),
        JsonEvent::EndObject => b.close_element(),
        JsonEvent::StartArray => open(&mut b, &mut key, "array"),
        JsonEvent::EndArray => b.close_element(),
        JsonEvent::Str(s) => {
            open(&mut b, &mut key, "string");
            if escape { b.push_attribute(plain("escaped"), "true".into()); }
            b.push_text(s, false);
            b.close_element();
        }
        JsonEvent::Number(n) => {
            open(&mut b, &mut key, "number");
            b.push_text(n, false);
            b.close_element();
        }
        JsonEvent::Bool(bl) => {
            open(&mut b, &mut key, "boolean");
            b.push_text(if bl { "true".into() } else { "false".into() }, false);
            b.close_element();
        }
        JsonEvent::Null => { open(&mut b, &mut key, "null"); b.close_element(); }
    });
    parse?;

    let ids = crate::eval::rtf_children_into_index_generic(idx, &b.top);
    Ok(Value::NodeSet(ids))
}

// ── current() ─────────────────────────────────────────────────────

/// `accumulator-before($name)` / `accumulator-after($name)` (XSLT 3.0
/// §18.4) — return the named accumulator's value before / after the
/// context node, from the precomputed maps.
fn accumulator_fn<I: DocIndexLike>(
    args: &[Value],
    idx: &I,
    namespaces: &crate::eval::NamespaceContext,
    accumulators: Option<&HashMap<String, crate::eval::AccumulatorData>>,
    context_node: NodeId,
    before: bool,
) -> Result<Value> {
    let fname = if before { "accumulator-before" } else { "accumulator-after" };
    if args.len() != 1 {
        return Err(err(format!("{fname}() takes exactly one argument")));
    }
    let raw = value_to_string(&args[0], idx);
    // Resolve the lexical accumulator name to its expanded-name key
    // (matching ast::QName's qname_key form used at declaration time).
    let key = match raw.split_once(':') {
        Some((p, l)) => match namespaces.resolve(p) {
            Some(uri) => format!("{{{uri}}}{l}"),
            None      => raw.clone(),
        },
        None => raw.clone(),
    };
    let Some(data) = accumulators.and_then(|m| m.get(&key)) else {
        return Err(err(format!(
            "accumulator '{raw}' is not declared (or not applicable here) (XTDE3340)")));
    };
    let map = if before { &data.before } else { &data.after };
    Ok(map.get(&context_node).cloned().unwrap_or_else(|| data.initial.clone()))
}

fn current_fn(args: &[Value], xslt_context: NodeId) -> Result<Value> {
    if !args.is_empty() {
        return Err(err("current() takes no arguments"));
    }
    Ok(Value::NodeSet(vec![xslt_context]))
}

// ── generate-id() ─────────────────────────────────────────────────

fn generate_id_fn(args: &[Value], xslt_context: NodeId) -> Result<Value> {
    let id = if args.is_empty() {
        xslt_context
    } else {
        match &args[0] {
            Value::NodeSet(ns) if !ns.is_empty() => ns[0],
            Value::NodeSet(_) => return Ok(Value::String(String::new())),
            _ => return Err(err("generate-id() requires a nodeset or no argument")),
        }
    };
    // XSLT spec: ID must start with an ASCII letter, contain only
    // ASCII alphanumerics, and be the same for the same node, and
    // different for different nodes.  NodeId as hex satisfies all
    // three.
    Ok(Value::String(format!("id{:x}", id)))
}

// ── system-property() ─────────────────────────────────────────────

fn system_property_fn<I: DocIndexLike>(
    args: &[Value], idx: &I, xslt_version: &str,
    namespaces: &crate::eval::NamespaceContext,
) -> Result<Value> {
    if args.len() != 1 {
        return Err(err("system-property() requires 1 argument"));
    }
    // XSLT 1.0 §15 — the argument is a QName whose prefix is
    // resolved against the namespaces in scope at the call
    // site.  Stylesheets commonly use either `xsl:` or `xslt:`;
    // we normalise to the XSLT namespace URI and dispatch on
    // the local name.
    let raw = value_to_string(&args[0], idx);
    // XSLT 2.0 §16.6.5 — the argument must be a valid lexical QName.
    if !is_lexical_qname(&raw) {
        return Err(err(format!(
            "system-property(): '{raw}' is not a valid QName (XTDE1390)"
        )));
    }
    let (prefix, local) = match raw.split_once(':') {
        Some((p, l)) => (Some(p), l),
        None         => (None, raw.as_str()),
    };
    let xslt_uri = "http://www.w3.org/1999/XSL/Transform";
    let uri: Option<String> = match prefix {
        Some(p) => Some(namespaces.resolve(p).or_else(|| match p {
            // `xsl` and `xslt` are the conventional XSLT-namespace
            // prefixes; honour them even when no in-scope binding
            // exists so callers without a fully-populated namespace
            // context (use-when static evaluation, unit tests) can
            // still query system-property without a workaround.
            "xsl" | "xslt" => Some(xslt_uri.to_string()),
            _ => None,
        }).ok_or_else(|| err(format!(
            // XSLT 2.0 §16.6.5 / XTDE1390 — a prefix on the argument
            // QName that's not in scope is a dynamic error.
            "system-property('{raw}'): prefix '{p}' is not declared \
             in the in-scope namespaces (XTDE1390)"
        )))?),
        None    => None,
    };
    let in_xslt = uri.as_deref() == Some(xslt_uri);
    if !in_xslt {
        return Ok(Value::String(String::new()));
    }
    Ok(Value::String(match local {
        // XSLT 1.0 §15 / XSLT 2.0 §16.6.5 — `xsl:version` is the
        // version of XSLT the *processor* implements, not the
        // stylesheet's declared version.  Report "2.0" when the
        // stylesheet asked for 2.0+ so 2.0 conformance tests
        // checking `system-property(…) >= 2` pass; "1.0"
        // otherwise so XSLT 1.0 stylesheets that gate on
        // `>= 1.0 < 2.0` still see what they expect.
        "version" => {
            let major = xslt_version.split('.').next()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(1);
            if major >= 2 { "2.0".into() } else { "1.0".into() }
        }
        "vendor"     => "sup-xml".to_string(),
        "vendor-url" => "https://github.com/super_source/sup_xml".to_string(),
        // XSLT 2.0 §16.6.5 / XSLT 3.0 §18.6 — the `xsl:…` flag
        // properties report whether the processor supports the
        // named feature.  We answer "no" for everything not in
        // the core 2.0 feature set we actually implement so
        // `system-property('xsl:is-schema-aware') = 'no'` and
        // similar boolean checks come out correctly typed.
        "product-name"                        => "sup-xml".to_string(),
        "product-version"                     => env!("CARGO_PKG_VERSION").to_string(),
        "is-schema-aware"                     => "no".to_string(),
        "supports-serialization"              => "yes".to_string(),
        "supports-backwards-compatibility"    => "yes".to_string(),
        "supports-namespace-axis"             => "yes".to_string(),
        "supports-streaming"                  => "no".to_string(),
        "supports-dynamic-evaluation"         => "no".to_string(),
        "supports-higher-order-functions"     => "no".to_string(),
        "xpath-version"                       => {
            let major = xslt_version.split('.').next()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(1);
            if major >= 2 { "2.0".into() } else { "1.0".into() }
        }
        "xsd-version"                         => "1.0".to_string(),
        _ => String::new(),
    }))
}

// ── element-available() / function-available() ───────────────────

/// XSLT 2.0 §15.1.3 — `element-available()` answers true only for
/// INSTRUCTIONS (elements that may appear in a sequence constructor),
/// not for top-level declarations like xsl:key, xsl:template, etc.
const ELEMENT_NAMES: &[&str] = &[
    // XSLT 1.0 instructions.
    "apply-imports", "apply-templates", "attribute", "call-template",
    "choose", "comment", "copy", "copy-of", "element", "fallback",
    "for-each", "if", "message", "number", "processing-instruction",
    "text", "value-of", "variable",
    // XSLT 2.0 additions (instruction-level).
    "analyze-string", "document", "for-each-group", "namespace",
    "next-match", "perform-sort", "result-document", "sequence",
    // XSLT 3.0 additions (instruction-level only — top-level
    // declarations such as xsl:package / xsl:mode are not reportable
    // by element-available).  A 2.0-versioned host still answers false
    // for these via the `suppress_3_0` gate in element_available_fn.
    "try", "catch", "fork", "iterate", "next-iteration", "break",
    "on-completion", "merge", "assert", "evaluate", "where-populated",
    "on-empty", "on-non-empty",
];

/// True iff `local` is the local name of a built-in XSLT instruction
/// (regardless of namespace).  Used by the static `use-when`
/// evaluator's `element-available()` so authors can guard
/// version-specific instructions.
pub(crate) fn is_builtin_xslt_instruction(local: &str) -> bool {
    ELEMENT_NAMES.iter().any(|e| *e == local)
}

/// True iff `local` is the unprefixed name of a built-in XPath /
/// XSLT 1.0+ function.  Mirrors the FN_NAMES table.  Used by the
/// static `use-when` evaluator's `function-available()`.
pub(crate) fn is_builtin_function(local: &str) -> bool {
    FN_NAMES.iter().any(|e| *e == local)
}

fn element_available_fn<I: DocIndexLike>(
    args: &[Value], idx: &I, extras: &[&str], xslt_version: &str,
) -> Result<Value> {
    if args.len() != 1 {
        return Err(err("element-available() requires 1 argument"));
    }
    let name = value_to_string(&args[0], idx);
    if !is_lexical_qname(&name) {
        return Err(err(format!(
            "element-available(): '{name}' is not a valid QName (XTDE1440)"
        )));
    }
    let (prefix, local) = match name.split_once(':') {
        Some((p, l)) => (Some(p), l),
        None         => (None, name.as_str()),
    };
    // XSLT 2.0 §15.1: only instructions introduced in 2.0 or earlier
    // are reportable from a 2.0 stylesheet.  xsl:try / xsl:catch /
    // xsl:fork / xsl:iterate landed in 3.0, so a 2.0-versioned
    // host must answer false even though the engine implements them.
    let is_3_0_only = matches!(local,
        "try" | "catch" | "iterate" | "fork" | "merge" | "merge-source"
        | "merge-key" | "merge-action" | "evaluate" | "where-populated"
        | "assert" | "break" | "next-iteration" | "on-completion"
        | "on-empty" | "on-non-empty" | "global-context-item"
        | "context-item" | "expose" | "use-package" | "override"
        | "package" | "accept" | "mode" | "source-document"
        | "result-document" /* in 2.0 too but xsl:result-document we keep */
    );
    // Only suppress when the host names the XSLT namespace and the
    // running stylesheet declared XSLT 2.0 (not 3.0+ and not 1.0).
    let xslt_uri = "http://www.w3.org/1999/XSL/Transform";
    let suppress_3_0 = prefix == Some("xsl")
        && !xslt_version_3_or_more(xslt_version)
        && is_3_0_only
        && local != "result-document"   // 2.0 instruction, allowed
        ;
    let _ = xslt_uri;
    let ok = !suppress_3_0
        && (ELEMENT_NAMES.iter().any(|e| *e == local)
            || extras.iter().any(|e| *e == local));
    Ok(Value::Boolean(ok))
}

pub(crate) fn xslt_version_3_or_more(v: &str) -> bool {
    v.trim().parse::<f64>().map(|n| n >= 3.0).unwrap_or(false)
}

/// Pure XPath 1.0 functions plus XSLT-added ones plus EXSLT.
const FN_NAMES: &[&str] = &[
    // XPath 1.0 §3 / §4
    "last", "position", "count", "id", "local-name", "namespace-uri",
    "name", "string", "concat", "starts-with", "contains",
    "substring-before", "substring-after", "substring",
    "string-length", "normalize-space", "translate",
    "boolean", "not", "true", "false", "lang",
    "number", "sum", "floor", "ceiling", "round",
    // XSLT 1.0 §12
    "current", "document", "key", "format-number", "generate-id",
    "system-property", "element-available", "function-available",
    "unparsed-entity-uri",
    // XPath 2.0 §3 / XSLT 2.0 §16 additions
    "abs", "avg", "base-uri", "codepoint-equal", "codepoint-to-string",
    "collection", "compare", "current-date", "current-dateTime",
    "current-time", "data", "dateTime", "day-from-date",
    "day-from-dateTime", "days-from-duration", "deep-equal",
    "default-collation", "distinct-values", "doc", "doc-available",
    "document-uri", "empty", "encode-for-uri", "ends-with",
    "error", "escape-html-uri", "exactly-one", "exists",
    "format-date", "format-dateTime", "format-time", "head",
    "hours-from-dateTime", "hours-from-duration", "hours-from-time",
    "id", "idref", "implicit-timezone", "in-scope-prefixes",
    "index-of", "insert-before", "iri-to-uri", "lower-case",
    "matches", "max", "min", "minutes-from-dateTime",
    "minutes-from-duration", "minutes-from-time", "month-from-date",
    "month-from-dateTime", "months-from-duration",
    "namespace-uri-for-prefix", "namespace-uri-from-QName",
    "nilled", "node-name", "normalize-unicode", "one-or-more",
    "prefix-from-QName", "QName", "regex-group",
    "remove", "replace", "resolve-QName", "resolve-uri",
    "reverse", "root", "seconds-from-dateTime",
    "seconds-from-duration", "seconds-from-time", "static-base-uri",
    "string-join", "string-to-codepoints", "subsequence",
    "tail", "timezone-from-date", "timezone-from-dateTime",
    "timezone-from-time", "tokenize", "trace", "type-available",
    "unordered", "unparsed-text", "unparsed-text-available",
    "upper-case", "year-from-date", "year-from-dateTime",
    "years-from-duration", "zero-or-one",
    "current-group", "current-grouping-key",
    // EXSLT families dispatch by namespace, not by unqualified
    // name — those don't show up here.
];

/// Allowed arities for the built-in XPath / XSLT 1.0 functions
/// covered by [`FN_NAMES`].  XPath 2.0 / 3.0 functions added by
/// the engine fall through to "any arity ≥ 0 OK" — `function-available`
/// can over-report for those, but no test in the suite relies on
/// the precise-arity answer for newer additions.
fn builtin_arity_ok(name: &str, arity: usize) -> bool {
    match name {
        // Zero-arg only.
        "last" | "position" | "true" | "false" | "current"
            => arity == 0,
        // One-arg only.
        "boolean" | "not" | "floor" | "ceiling" | "round"
        | "generate-id" | "element-available" | "unparsed-entity-uri"
            => arity == 1,
        // 0 or 1 arg.
        "string" | "name" | "local-name" | "namespace-uri"
        | "number" | "string-length" | "normalize-space"
            => arity <= 1,
        // 2 args.
        "starts-with" | "contains" | "substring-before" | "substring-after"
        | "key" | "lang"
            => arity == 2,
        // 2 or 3 args.
        "substring" | "format-number" | "id"
            => (2..=3).contains(&arity),
        // 2+ args (variadic).
        "concat" => arity >= 2,
        // Exactly 3.
        "translate" => arity == 3,
        // 1 or 2.
        "document" | "function-available" | "system-property"
        | "count" | "sum"
            => (1..=2).contains(&arity),
        // Default — allow whatever the call site asked for.
        _ => true,
    }
}

/// True iff `s` parses as a lexical QName — either a single NCName
/// or `prefix:local` where both sides are NCNames.  Used by
/// function-available / element-available / system-property to
/// surface XTDE1400 on malformed names.
pub(crate) fn is_lexical_qname(s: &str) -> bool {
    fn ncname(p: &str) -> bool {
        if p.is_empty() { return false; }
        let mut cs = p.chars();
        let first = cs.next().unwrap();
        if !(first.is_alphabetic() || first == '_') { return false; }
        cs.all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
    }
    match s.split_once(':') {
        Some((p, l)) => ncname(p) && ncname(l),
        None         => ncname(s),
    }
}

fn function_available_fn<I: DocIndexLike>(
    args: &[Value], idx: &I,
    namespaces:     &crate::eval::NamespaceContext,
    user_functions: &[crate::ast::UserFunction],
) -> Result<Value> {
    // XSLT 2.0 §16.5 — `function-available(name [, arity])`.  We
    // honour the optional arity argument when it picks out one of
    // the per-arity user functions; built-ins don't track arity and
    // always answer present-vs-absent.
    if args.is_empty() || args.len() > 2 {
        return Err(err("function-available() requires 1 or 2 arguments"));
    }
    let name = value_to_string(&args[0], idx);
    let arity = args.get(1).map(|v| sup_xml_core::xpath::eval::value_to_number(v, idx) as usize);
    // XSLT 2.0 §16.5 / XTDE1400 — the supplied name must be a valid
    // lexical QName.  Anything else (containing `?`, embedded
    // space, malformed prefix, etc.) is a dynamic error.
    if !is_lexical_qname(&name) {
        return Err(err(format!(
            "function-available(): '{name}' is not a valid QName (XTDE1400)"
        )));
    }
    // Resolve the QName.  An unprefixed name maps to the no-prefix
    // built-in slot; a prefixed name resolves against the
    // stylesheet's namespace context.  An undeclared prefix is
    // XTDE1400 too.
    let (uri, local) = match name.split_once(':') {
        Some((p, local)) => match namespaces.resolve(p) {
            Some(u) => (u, local.to_string()),
            None    => return Err(err(format!(
                "function-available(): prefix '{p}' is not bound in scope (XTDE1400)"
            ))),
        },
        None             => (String::new(), name.clone()),
    };
    // Built-in unprefixed: name is in FN_NAMES OR is in the XPath
    // functions namespace (uri is "" or "fn:").
    let is_builtin = uri.is_empty()
        || uri == "http://www.w3.org/2005/xpath-functions";
    if is_builtin && FN_NAMES.iter().any(|e| *e == local) {
        return Ok(Value::Boolean(
            arity.map_or(true, |n| builtin_arity_ok(&local, n))));
    }
    // XSD type constructor functions — `xs:T(value)` constructs an
    // atomic of type T from a single value, so any recognised XSD
    // built-in type counts as available with arity 1.
    let xsd_ns = "http://www.w3.org/2001/XMLSchema";
    if uri == xsd_ns
        && sup_xml_core::xpath::eval::atomic_kind_static(&local).is_some()
    {
        let n = arity.unwrap_or(1);
        return Ok(Value::Boolean(n == 1));
    }
    // User-declared functions: match by Clark form + arity.
    let found = user_functions.iter().any(|uf| {
        uf.name.local == local
            && uf.name.uri == uri
            && arity.map_or(true, |n| uf.params.len() == n)
    });
    Ok(Value::Boolean(found))
}

// ── format-number() ───────────────────────────────────────────────

fn format_number_fn<I: DocIndexLike>(
    args: &[Value], idx: &I,
    decimal_formats: &HashMap<String, DecimalFormat>,
    namespaces:      &crate::eval::NamespaceContext,
) -> Result<Value> {
    if args.len() < 2 || args.len() > 3 {
        return Err(err("format-number() requires 2 or 3 arguments"));
    }
    let n = value_to_number(&args[0], idx);
    let picture = value_to_string(&args[1], idx);
    // XSLT 1.0 §12.3: the third argument is the QName of an
    // `<xsl:decimal-format>` declaration.  Resolve any prefix
    // against the static-context namespaces and look up by the
    // Clark form `{uri}local` so the call matches the
    // declaration even when they use different prefixes for the
    // same URI.  Unprefixed names hit the unprefixed slot.
    let name = args.get(2).map(|v| value_to_string(v, idx));
    let df: std::borrow::Cow<DecimalFormat> = match &name {
        Some(raw) => {
            let key = match raw.split_once(':') {
                Some((p, local)) => match namespaces.resolve(p) {
                    Some(uri) => format!("{{{uri}}}{local}"),
                    None      => raw.clone(),
                },
                None => raw.clone(),
            };
            decimal_formats.get(key.as_str())
                .map(std::borrow::Cow::Borrowed)
                .ok_or_else(|| err(format!(
                    "format-number(): no <xsl:decimal-format name='{raw}'> declared"
                )))?
        }
        None => decimal_formats.get("")
            .map(std::borrow::Cow::Borrowed)
            .unwrap_or_else(|| std::borrow::Cow::Owned(DecimalFormat::default())),
    };
    let formatted = format_number(n, &picture, &df).map_err(err)?;
    Ok(Value::String(formatted))
}

// ── key() ─────────────────────────────────────────────────────────

/// Does the stylesheet's `version=` attribute select XSLT 2.0 or
/// higher?  Drives the handful of places where 2.0 tightened a rule
/// that 1.0 left lax (e.g. `key()`'s undeclared-name error XTDE1260).
fn xslt_version_2_or_more(version: &str) -> bool {
    version.trim().split('.').next()
        .and_then(|s| s.parse::<u32>().ok())
        .is_some_and(|major| major >= 2)
}

fn key_fn<I: DocIndexLike>(
    args: &[Value], idx: &I, keys: Option<&KeyIndex>,
    namespaces: &crate::eval::NamespaceContext,
    context_node: NodeId,
    xslt_version: &str,
) -> Result<Value> {
    if args.len() < 2 || args.len() > 3 {
        return Err(err("key() requires 2 or 3 arguments"));
    }
    // XSLT 2.0 §16.3 / XTDE1270 — the 2-argument form needs a context
    // node (whose root is a document); the 3-argument form supplies
    // its own focus.  An xsl:function body sets the outer focus to
    // undefined, but a *contained* path step `$nodes/key(...)`
    // explicitly re-defines the context for the call — so consult
    // the XPath-level focus flag too: if a step has cleared it, the
    // call is fine even when the outer XSLT scope is context-less.
    if args.len() == 2
        && crate::eval::is_context_undefined()
        && sup_xml_core::xpath::eval::focus_is_undefined()
    {
        return Err(err(
            "key() called with no context (XTDE1270)"));
    }
    if args.len() == 2 && !root_is_document(idx, context_node) {
        return Err(err(
            "key(): the tree containing the context node is not rooted at \
             a document node (XTDE1270)"));
    }
    let raw_name = value_to_string(&args[0], idx);
    let expanded = expand_qname(&raw_name, namespaces);
    // The key name must match an xsl:key declaration in scope.  XSLT
    // 1.0 §12.2 left an unmatched name unspecified, and libxslt — the
    // engine this library mirrors — returns the empty node-set; XSLT
    // 2.0 §16.3 tightened this to the dynamic error XTDE1260.  Honour
    // the stricter rule only for 2.0+ stylesheets so 1.0 (and 2.0
    // backwards-compatible) transforms stay libxslt-compatible.
    let declared = keys.map_or(false, |k| k.is_declared(&expanded));
    if !declared {
        if xslt_version_2_or_more(xslt_version) {
            return Err(err(&format!(
                "key(): no xsl:key declaration named '{raw_name}' (XTDE1260)"
            )));
        }
        return Ok(Value::NodeSet(Vec::new()));
    }
    let keys = keys.unwrap();
    // Same shape as `stringify_use_value` (build side) — and the
    // same NaN drop, so a NaN lookup never matches even a NaN-keyed
    // node (F&O §15.1; xs:double NaN compares unequal to itself).
    let lookup_values: Vec<String> = match &args[1] {
        Value::NodeSet(ns) => ns.iter().map(|&id| idx.string_value(id)).collect(),
        Value::Sequence(items) => items.iter()
            .map(|v| canonical_key_string(v, idx)).collect(),
        Value::IntRange { lo, hi } => (*lo..=*hi).map(|n| n.to_string()).collect(),
        other              => vec![canonical_key_string(other, idx)],
    };
    let lookup_values: Vec<String> = lookup_values.into_iter()
        .filter(|s| s != "NaN").collect();
    // XSLT 2.0 §16.4 — 3-arg form `key(name, value, $tree)` scopes
    // the lookup to the tree containing $tree's first item (instead
    // of the calling context's document).  When omitted, the calling
    // context's document is used.  XTDE1270 fires when the
    // determined root is not a document node (key indexing is only
    // defined under a document, not an unattached RTF element).
    let doc_root = if let Some(scope) = args.get(2) {
        let scope_nodes: Vec<NodeId> = match scope {
            Value::NodeSet(ns) => ns.clone(),
            Value::Sequence(items) => items.iter().flat_map(|it| {
                if let Value::NodeSet(ns) = it { ns.clone() } else { Vec::new() }
            }).collect(),
            _ => Vec::new(),
        };
        if scope_nodes.is_empty() {
            return Ok(Value::NodeSet(Vec::new()));
        }
        doc_root_of(idx, scope_nodes[0])
    } else {
        doc_root_of(idx, context_node)
    };
    use sup_xml_core::xpath::XPathNodeKind;
    if !matches!(idx.kind(doc_root), XPathNodeKind::Document)
        || idx.is_synthetic_wrap(doc_root)
    {
        return Err(err(
            "key(): the root of the tree containing the context node is \
             not a document node (XTDE1270)"
        ));
    }
    let mut out: Vec<NodeId> = Vec::new();
    for v in lookup_values {
        out.extend(keys.lookup(&expanded, doc_root, &v));
    }
    out.sort_unstable();
    out.dedup();
    Ok(Value::NodeSet(out))
}

/// Expand a `prefix:local` (or bare `local`) QName-string into
/// `{uri}local` (or bare `local`) form for hash-table lookup.
/// Unresolved prefixes pass through verbatim — keeps backward
/// compatibility with stylesheets that already string-matched
/// against the raw `prefix:local` form.
fn expand_qname(raw: &str, namespaces: &crate::eval::NamespaceContext) -> String {
    match raw.split_once(':') {
        Some((prefix, local)) => match namespaces.resolve(prefix) {
            Some(uri) => format!("{{{uri}}}{local}"),
            None      => raw.to_string(),
        },
        None => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sup_xml_core::xpath::eval::Numeric;
    use sup_xml_core::{parse_str, ParseOptions, XPathContext};

    /// Test-only adapter that supplies the same node id for both the
    /// XSLT-current and XPath-context-node parameters of
    /// [`super::dispatch`].  The split between the two only matters
    /// inside predicate sub-expressions during a real eval — flat
    /// unit tests can treat them as one and the same.
    fn dispatch<I: DocIndexLike>(
        name: &str, args: Vec<Value>, idx: &I, ctx: NodeId,
        keys: Option<&KeyIndex>, instr: &[&str],
        docs: Option<&HashMap<String, NodeId>>,
        df: &HashMap<String, DecimalFormat>,
        ns: &crate::eval::NamespaceContext,
        ue: &HashMap<String, sup_xml_tree::UnparsedEntity>,
    ) -> Option<Result<Value>> {
        super::dispatch(name, args, idx, ctx, ctx, keys, instr, docs, None, df, ns, ue,
            None, None, None, None, &[], "1.0", None)
    }

    #[test]
    fn current_returns_xslt_context_node() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("current", vec![], &ctx.index, 5, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new());
        assert!(r.is_some());
        let v = r.unwrap().unwrap();
        assert!(matches!(v, Value::NodeSet(ns) if ns == vec![5]));
    }

    #[test]
    fn generate_id_produces_stable_id_per_node() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let v1 = dispatch("generate-id", vec![], &ctx.index, 7, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap().unwrap();
        let v2 = dispatch("generate-id", vec![], &ctx.index, 7, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap().unwrap();
        assert_eq!(format!("{v1:?}"), format!("{v2:?}"));
        // Different node gets a different id.
        let v3 = dispatch("generate-id", vec![], &ctx.index, 8, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap().unwrap();
        assert!(format!("{v1:?}") != format!("{v3:?}"));
    }

    #[test]
    fn system_property_xsl_version() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("system-property",
            vec![Value::String("xsl:version".into())], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v { Value::String(s) => assert_eq!(s, "1.0"), _ => panic!() }
    }

    #[test]
    fn element_available_for_known_instruction() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("element-available",
            vec![Value::String("xsl:for-each".into())], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v { Value::Boolean(b) => assert!(b), _ => panic!() }
        // Unknown → false.
        let v = dispatch("element-available",
            vec![Value::String("xsl:made-up".into())], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v { Value::Boolean(b) => assert!(!b), _ => panic!() }
    }

    #[test]
    fn function_available_recognises_xpath_and_xslt() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("function-available",
            vec![Value::String("count".into())], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v { Value::Boolean(b) => assert!(b), _ => panic!() }
        let v = dispatch("function-available",
            vec![Value::String("generate-id".into())], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v { Value::Boolean(b) => assert!(b), _ => panic!() }
    }

    #[test]
    fn unknown_function_returns_none() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        assert!(dispatch("nonsense", vec![], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).is_none());
    }

    // ── current() error path ────────────────────────────────────

    #[test]
    fn current_rejects_args() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("current",
            vec![Value::Number(Numeric::Double(0.0))], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap();
        assert!(r.is_err());
    }

    // ── generate-id() with explicit nodeset arg ─────────────────

    #[test]
    fn generate_id_with_nodeset_argument_returns_first_node_id() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        // Node ids 1, 2 — pass both, expect first to be used.
        let v = dispatch("generate-id",
            vec![Value::NodeSet(vec![5, 9])], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v {
            Value::String(s) => assert_eq!(s, "id5"),
            _ => panic!(),
        }
    }

    #[test]
    fn generate_id_with_empty_nodeset_returns_empty_string() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("generate-id",
            vec![Value::NodeSet(vec![])], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v {
            Value::String(s) => assert!(s.is_empty()),
            _ => panic!(),
        }
    }

    #[test]
    fn generate_id_rejects_non_nodeset_argument() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("generate-id",
            vec![Value::Number(Numeric::Double(3.0))], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap();
        assert!(r.is_err());
    }

    // ── system-property() additional paths ──────────────────────

    #[test]
    fn system_property_vendor_and_vendor_url() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("system-property",
            vec![Value::String("xsl:vendor".into())], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v { Value::String(s) => assert!(s.contains("sup-xml")), _ => panic!() }
        let v = dispatch("system-property",
            vec![Value::String("xsl:vendor-url".into())], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v { Value::String(s) => assert!(s.starts_with("http")), _ => panic!() }
    }

    #[test]
    fn system_property_unknown_returns_empty_string() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("system-property",
            vec![Value::String("xsl:made-up".into())], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v { Value::String(s) => assert!(s.is_empty()), _ => panic!() }
    }

    #[test]
    fn system_property_wrong_argc_errors() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("system-property", vec![], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap();
        assert!(r.is_err());
    }

    // ── element-available() unprefixed + extras + bad argc ──────

    #[test]
    fn element_available_unprefixed_name() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        // No prefix → checked as local name directly.
        let v = dispatch("element-available",
            vec![Value::String("for-each".into())], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v { Value::Boolean(b) => assert!(b), _ => panic!() }
    }

    #[test]
    fn element_available_extension_via_extras() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("element-available",
            vec![Value::String("ext:my-instr".into())], &ctx.index, 0, None,
            &["my-instr"], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v { Value::Boolean(b) => assert!(b), _ => panic!() }
    }

    #[test]
    fn element_available_wrong_argc_errors() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("element-available", vec![], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap();
        assert!(r.is_err());
    }

    // ── function-available() unprefixed + bad argc ──────────────

    #[test]
    fn function_available_unprefixed_unknown_returns_false() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("function-available",
            vec![Value::String("totally-fake".into())], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v { Value::Boolean(b) => assert!(!b), _ => panic!() }
    }

    #[test]
    fn function_available_wrong_argc_errors() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("function-available", vec![], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap();
        assert!(r.is_err());
    }

    // ── document() / unparsed-entity-uri ────────────────────────

    #[test]
    fn document_without_loader_errors() {
        // No Loader was wired (documents = None) — calling document()
        // surfaces a "no hook to this dispatcher" diagnostic.  This
        // path is unreachable from the public `Stylesheet::apply*`
        // entry points (they always pass `Some(&map)`); it guards
        // direct callers of `dispatch`.
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("document",
            vec![Value::String("foo.xml".into())], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap();
        match r {
            Err(e) => assert!(e.message.contains("document-loading hook"),
                              "got: {}", e.message),
            _ => panic!("expected an error when no Loader is configured"),
        }
    }

    #[test]
    fn document_uses_preloaded_uri_map() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let mut docs = HashMap::new();
        docs.insert("foo.xml".to_string(), 42usize);
        let r = dispatch("document",
            vec![Value::String("foo.xml".into())], &ctx.index, 0, None, &[],
            Some(&docs), &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap().unwrap();
        match r {
            Value::NodeSet(ns) => assert_eq!(ns, vec![42]),
            _ => panic!("expected nodeset"),
        }
    }

    #[test]
    fn document_rejects_unloaded_uri() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let docs: HashMap<String, usize> = HashMap::new();
        let r = dispatch("document",
            vec![Value::String("never-loaded.xml".into())], &ctx.index, 0, None, &[],
            Some(&docs), &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap();
        match r {
            Err(e) => assert!(e.message.contains("not pre-loaded")),
            _ => panic!("expected error for unloaded URI"),
        }
    }

    #[test]
    fn document_empty_uri_resolved_via_preloaded_map() {
        // `document('')` is now supported via apply-time preloading
        // against the stylesheet's base URI.  When the URI isn't in
        // the preload map (e.g. the caller didn't supply a Loader or
        // the stylesheet wasn't loaded from disk), we surface the
        // standard not-preloaded diagnostic — same as any other
        // unresolved URI.
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let docs: HashMap<String, usize> = HashMap::new();
        let r = dispatch("document",
            vec![Value::String("".into())], &ctx.index, 0, None, &[],
            Some(&docs), &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap();
        match r {
            Err(e) => assert!(e.message.contains("not pre-loaded")),
            _ => panic!("expected error when map is empty"),
        }
    }

    #[test]
    fn unparsed_entity_uri_returns_empty_string() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("unparsed-entity-uri",
            vec![Value::String("foo".into())], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new())
            .unwrap().unwrap();
        match v { Value::String(s) => assert!(s.is_empty()), _ => panic!() }
    }

    // ── format-number() ─────────────────────────────────────────

    #[test]
    fn format_number_basic() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("format-number",
            vec![Value::Number(Numeric::Double(1234.5)), Value::String("#,##0.00".into())],
            &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap().unwrap();
        match v { Value::String(s) => assert_eq!(s, "1,234.50"), _ => panic!() }
    }

    #[test]
    fn format_number_with_named_format() {
        // Third arg is the decimal-format name; we accept-and-ignore it.
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        // With no named decimal-formats registered, looking one up
        // errors — match libxslt behaviour.
        let r = dispatch("format-number",
            vec![Value::Number(Numeric::Double(42.0)), Value::String("0".into()),
                 Value::String("custom-fmt".into())],
            &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap();
        assert!(r.is_err(), "unknown decimal-format name must error");

        // With the named format registered, the lookup succeeds.
        let mut formats = HashMap::new();
        formats.insert("custom-fmt".to_string(), DecimalFormat::default());
        let v = dispatch("format-number",
            vec![Value::Number(Numeric::Double(42.0)), Value::String("0".into()),
                 Value::String("custom-fmt".into())],
            &ctx.index, 0, None, &[], None, &formats, &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap().unwrap();
        match v { Value::String(s) => assert_eq!(s, "42"), _ => panic!() }
    }

    #[test]
    fn format_number_wrong_argc_errors() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("format-number",
            vec![Value::Number(Numeric::Double(1.0))], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap();
        assert!(r.is_err());
        let r = dispatch("format-number",
            vec![Value::Number(Numeric::Double(1.0)), Value::String("0".into()),
                 Value::String("a".into()), Value::String("b".into())],
            &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap();
        assert!(r.is_err());
    }

    // ── key() ────────────────────────────────────────────────────

    /// XSLT 1.0 §12.2 / libxslt compatibility: an undeclared key name
    /// yields the empty node-set rather than an error.  The local
    /// `dispatch` adapter pins `version="1.0"`, so this exercises the
    /// lax path.
    #[test]
    fn key_without_any_index_returns_empty_nodeset() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let v = dispatch("key",
            vec![Value::String("idx".into()), Value::String("k".into())],
            &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap().unwrap();
        match v { Value::NodeSet(ns) => assert!(ns.is_empty()), _ => panic!() }
    }

    /// XSLT 2.0 §16.3 tightened the same case to the dynamic error
    /// XTDE1260.  Routed through `super::dispatch` so we can pin
    /// `version="2.0"`.
    #[test]
    fn key_with_undeclared_name_errors_in_2_0() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let r = super::dispatch("key",
            vec![Value::String("idx".into()), Value::String("k".into())],
            &ctx.index, 0, 0, None, &[], None, None, &HashMap::new(),
            &crate::eval::NamespaceContext::default(), &HashMap::new(),
            None, None, None, None, &[], "2.0", None).unwrap();
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("XTDE1260"), "got: {msg}");
    }

    #[test]
    fn key_wrong_argc_errors() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("key",
            vec![Value::String("idx".into())], &ctx.index, 0, None, &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap();
        assert!(r.is_err());
    }

    #[test]
    fn key_lookup_with_populated_index() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let mut keys = KeyIndex::new();
        // Manually populate the index.
        // KeyIndex buckets by (key-name, doc-root, value); doc-root
        // for nodes 5 and 7 is whatever node 0 reports.
        let doc_root = 0usize;
        keys.declared.insert("idx".into());
        keys.entries.insert(("idx".into(), doc_root, "k".into()), vec![5, 7]);
        let v = dispatch("key",
            vec![Value::String("idx".into()), Value::String("k".into())],
            &ctx.index, 0, Some(&keys), &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap().unwrap();
        match v {
            Value::NodeSet(ns) => assert_eq!(ns, vec![5, 7]),
            _ => panic!(),
        }
    }

    #[test]
    fn key_lookup_with_nodeset_second_arg_unions() {
        let doc = parse_str("<r><a>k1</a><b>k2</b></r>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let mut keys = KeyIndex::new();
        let doc_root = 0usize;
        keys.declared.insert("idx".into());
        keys.entries.insert(("idx".into(), doc_root, "k1".into()), vec![3]);
        keys.entries.insert(("idx".into(), doc_root, "k2".into()), vec![5]);
        // The string-value of each node in the input nodeset becomes a key.
        let lookup_nodes = match ctx.eval("/r/*").unwrap() {
            Value::NodeSet(ns) => ns,
            _ => panic!(),
        };
        let v = dispatch("key",
            vec![Value::String("idx".into()), Value::NodeSet(lookup_nodes)],
            &ctx.index, 0, Some(&keys), &[], None, &HashMap::new(), &crate::eval::NamespaceContext::default(), &HashMap::new()).unwrap().unwrap();
        match v {
            // Both 3 and 5, deduped and sorted.
            Value::NodeSet(ns) => assert_eq!(ns, vec![3, 5]),
            _ => panic!(),
        }
    }

    // ── qname_key helper ────────────────────────────────────────

    #[test]
    fn qname_key_with_and_without_uri() {
        let q1 = QName { prefix: None, local: "foo".into(), uri: String::new() };
        let q2 = QName { prefix: Some("ns".into()), local: "foo".into(), uri: "urn:n".into() };
        assert_eq!(qname_key(&q1), "foo");
        assert_eq!(qname_key(&q2), "{urn:n}foo");
    }

    #[test]
    fn key_index_new_is_empty() {
        let k = KeyIndex::new();
        assert!(k.lookup("nope", 0usize, "nope").is_empty());
    }

    #[test]
    fn stringify_use_value_for_non_nodeset() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let s = stringify_use_value(&Value::Number(Numeric::Double(42.0)), &ctx.index);
        assert_eq!(s, vec!["42".to_string()]);
    }

    // ── builtin_arity_ok ─────────────────────────────────────────

    #[test]
    fn arity_zero_arg_functions() {
        for name in ["last", "position", "true", "false", "current"] {
            assert!( super::builtin_arity_ok(name, 0), "{name}/0 should be ok");
            assert!(!super::builtin_arity_ok(name, 1), "{name}/1 should be rejected");
        }
    }

    #[test]
    fn arity_optional_arg_functions() {
        for name in ["string", "name", "local-name", "namespace-uri",
                     "number", "string-length", "normalize-space"] {
            assert!(super::builtin_arity_ok(name, 0), "{name}/0");
            assert!(super::builtin_arity_ok(name, 1), "{name}/1");
            assert!(!super::builtin_arity_ok(name, 2), "{name}/2 should be rejected");
        }
    }

    #[test]
    fn arity_substring_2_or_3() {
        assert!(!super::builtin_arity_ok("substring", 1));
        assert!( super::builtin_arity_ok("substring", 2));
        assert!( super::builtin_arity_ok("substring", 3));
        assert!(!super::builtin_arity_ok("substring", 4));
    }

    #[test]
    fn arity_concat_is_variadic() {
        assert!(!super::builtin_arity_ok("concat", 0));
        assert!(!super::builtin_arity_ok("concat", 1));
        assert!( super::builtin_arity_ok("concat", 2));
        assert!( super::builtin_arity_ok("concat", 7));
    }

    #[test]
    fn arity_translate_requires_three() {
        assert!(!super::builtin_arity_ok("translate", 2));
        assert!( super::builtin_arity_ok("translate", 3));
        assert!(!super::builtin_arity_ok("translate", 4));
    }
}
