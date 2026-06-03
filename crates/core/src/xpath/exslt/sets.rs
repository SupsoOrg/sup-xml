//! EXSLT set family — https://exslt.org/set/
//!
//! All functions live in the `http://exslt.org/sets` namespace.
//!
//! Coverage:
//!
//! | Function          | Returns  | Semantics                                  |
//! |-------------------|----------|--------------------------------------------|
//! | `difference`      | nodeset  | nodes in A not in B (by node identity)     |
//! | `intersection`    | nodeset  | nodes in both A and B (by node identity)   |
//! | `distinct`        | nodeset  | one representative per distinct string-val |
//! | `has-same-node`   | boolean  | non-empty intersection?                    |
//! | `leading`         | nodeset  | nodes from A before first node also in B   |
//! | `trailing`        | nodeset  | nodes from A after last node also in B     |
//!
//! Identity comparisons use `NodeId` equality, which is canonical
//! in our index — two NodeIds refer to the same node iff they're
//! equal.  This lets `difference` / `intersection` / `has-same-node`
//! / `leading` / `trailing` be O(n+m) via `HashSet<NodeId>` instead
//! of O(n*m) string-value comparison.  `distinct` is the one
//! exception — per spec it dedups by string-value, not identity.

use std::collections::HashSet;

use crate::error::{ErrorDomain, ErrorLevel, XmlError};
use crate::xpath::eval::Value;
use crate::xpath::index::{DocIndexLike, NodeId};

use super::Result;

fn err(msg: impl Into<String>) -> XmlError {
    XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg)
}

pub fn dispatch<I: DocIndexLike>(
    name: &str, args: Vec<Value>, idx: &I,
) -> Option<Result<Value>> {
    let r: Result<Value> = match name {
        "difference"     => difference(&args),
        "intersection"   => intersection(&args),
        "distinct"       => distinct(&args, idx),
        "has-same-node"  => has_same_node(&args),
        "leading"        => leading(&args),
        "trailing"       => trailing(&args),
        _ => return None,
    };
    Some(r)
}

// ── helpers ───────────────────────────────────────────────────────

fn two_nodesets<'a>(args: &'a [Value], name: &str) -> Result<(&'a [NodeId], &'a [NodeId])> {
    if args.len() != 2 {
        return Err(err(format!("set:{name} requires 2 nodeset arguments")));
    }
    let a = match &args[0] {
        Value::NodeSet(ns) => ns.as_slice(),
        _ => return Err(err(format!("set:{name} first arg must be a nodeset"))),
    };
    let b = match &args[1] {
        Value::NodeSet(ns) => ns.as_slice(),
        _ => return Err(err(format!("set:{name} second arg must be a nodeset"))),
    };
    Ok((a, b))
}

// ── difference ────────────────────────────────────────────────────

fn difference(args: &[Value]) -> Result<Value> {
    let (a, b) = two_nodesets(args, "difference")?;
    let b_set: HashSet<NodeId> = b.iter().copied().collect();
    let out: Vec<NodeId> = a.iter().copied().filter(|id| !b_set.contains(id)).collect();
    Ok(Value::NodeSet(out))
}

// ── intersection ──────────────────────────────────────────────────

fn intersection(args: &[Value]) -> Result<Value> {
    let (a, b) = two_nodesets(args, "intersection")?;
    let b_set: HashSet<NodeId> = b.iter().copied().collect();
    let out: Vec<NodeId> = a.iter().copied().filter(|id| b_set.contains(id)).collect();
    Ok(Value::NodeSet(out))
}

// ── has-same-node ─────────────────────────────────────────────────

fn has_same_node(args: &[Value]) -> Result<Value> {
    let (a, b) = two_nodesets(args, "has-same-node")?;
    let b_set: HashSet<NodeId> = b.iter().copied().collect();
    Ok(Value::Boolean(a.iter().any(|id| b_set.contains(id))))
}

// ── distinct ──────────────────────────────────────────────────────

/// `set:distinct(nodeset)` — dedup by *string-value*, returning the
/// first occurrence of each unique string in document order.  This
/// is the one set function that doesn't use node identity.
fn distinct<I: DocIndexLike>(args: &[Value], idx: &I) -> Result<Value> {
    if args.len() != 1 {
        return Err(err("set:distinct requires 1 nodeset argument"));
    }
    let ns = match &args[0] {
        Value::NodeSet(ns) => ns,
        _ => return Err(err("set:distinct requires a nodeset argument")),
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<NodeId> = Vec::new();
    for &id in ns {
        let s = idx.string_value(id);
        if seen.insert(s) {
            out.push(id);
        }
    }
    Ok(Value::NodeSet(out))
}

// ── leading / trailing ────────────────────────────────────────────

/// `set:leading(A, B)` — nodes from `A` that appear before the
/// first node in `A` that's also in `B`.  Per spec, "document
/// order" is the ordering used to determine "before" — which
/// matches `A`'s order (the engine returns nodesets in document
/// order).
fn leading(args: &[Value]) -> Result<Value> {
    let (a, b) = two_nodesets(args, "leading")?;
    let b_set: HashSet<NodeId> = b.iter().copied().collect();
    let mut out: Vec<NodeId> = Vec::new();
    for &id in a {
        if b_set.contains(&id) { break; }
        out.push(id);
    }
    Ok(Value::NodeSet(out))
}

/// `set:trailing(A, B)` — nodes from `A` that appear after the
/// last node in `A` that's also in `B`.
fn trailing(args: &[Value]) -> Result<Value> {
    let (a, b) = two_nodesets(args, "trailing")?;
    let b_set: HashSet<NodeId> = b.iter().copied().collect();
    // Find the index of the last A-element that's in B; "trailing"
    // is everything after that index.  If no overlap, "trailing"
    // is empty (per libexslt's interpretation; the EXSLT spec is
    // slightly ambiguous here but this matches what real
    // stylesheets expect).
    let mut cut = None;
    for (i, &id) in a.iter().enumerate() {
        if b_set.contains(&id) { cut = Some(i); }
    }
    let out: Vec<NodeId> = match cut {
        Some(i) => a[i + 1..].to_vec(),
        None    => Vec::new(),
    };
    Ok(Value::NodeSet(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xpath::XPathContext;
    use crate::{parse_str, ParseOptions};

    fn ns(v: &Value) -> &[NodeId] {
        if let Value::NodeSet(ns) = v { ns } else { panic!("expected nodeset, got {v:?}") }
    }
    fn b(v: &Value) -> bool {
        if let Value::Boolean(b) = v { *b } else { panic!("expected boolean, got {v:?}") }
    }

    #[test]
    fn difference_removes_overlap() {
        let doc = parse_str("<r><a/><b/><c/><d/></r>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let all = ctx.eval("/r/*").unwrap();          // a, b, c, d
        let bd  = ctx.eval("/r/b | /r/d").unwrap();   // b, d
        let r = dispatch("difference", vec![all, bd], &ctx.index).unwrap().unwrap();
        // {a,b,c,d} − {b,d} = {a,c}
        assert_eq!(ns(&r).len(), 2);
    }

    #[test]
    fn intersection_keeps_overlap() {
        let doc = parse_str("<r><a/><b/><c/><d/></r>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let ac = ctx.eval("/r/a | /r/c").unwrap();
        let bc = ctx.eval("/r/b | /r/c").unwrap();
        let r = dispatch("intersection", vec![ac, bc], &ctx.index).unwrap().unwrap();
        // {a,c} ∩ {b,c} = {c}
        assert_eq!(ns(&r).len(), 1);
    }

    #[test]
    fn has_same_node_detects_overlap() {
        let doc = parse_str("<r><a/><b/><c/></r>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let ab = ctx.eval("/r/a | /r/b").unwrap();
        let bc = ctx.eval("/r/b | /r/c").unwrap();
        assert!(b(&dispatch("has-same-node", vec![ab, bc], &ctx.index).unwrap().unwrap()));
        let a = ctx.eval("/r/a").unwrap();
        let c = ctx.eval("/r/c").unwrap();
        assert!(!b(&dispatch("has-same-node", vec![a, c], &ctx.index).unwrap().unwrap()));
    }

    #[test]
    fn distinct_dedups_by_string_value_not_identity() {
        // Two <i> nodes with identical string-value: distinct should
        // collapse them to one.
        let doc = parse_str(
            "<r><i>x</i><i>y</i><i>x</i><i>z</i></r>",
            &ParseOptions::default(),
        ).unwrap();
        let ctx = XPathContext::new(&doc);
        let all = ctx.eval("/r/i").unwrap();
        let r = dispatch("distinct", vec![all], &ctx.index).unwrap().unwrap();
        // 4 input nodes, 3 distinct string values → 3 output.
        assert_eq!(ns(&r).len(), 3);
    }

    #[test]
    fn leading_returns_prefix_before_first_overlap() {
        let doc = parse_str("<r><a/><b/><c/><d/></r>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let abcd = ctx.eval("/r/*").unwrap();
        let c    = ctx.eval("/r/c").unwrap();
        let r = dispatch("leading", vec![abcd, c], &ctx.index).unwrap().unwrap();
        // {a,b,c,d}.leading({c}) → {a,b}
        assert_eq!(ns(&r).len(), 2);
    }

    #[test]
    fn trailing_returns_suffix_after_last_overlap() {
        let doc = parse_str("<r><a/><b/><c/><d/></r>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let abcd = ctx.eval("/r/*").unwrap();
        let b_   = ctx.eval("/r/b").unwrap();
        let r = dispatch("trailing", vec![abcd, b_], &ctx.index).unwrap().unwrap();
        // {a,b,c,d}.trailing({b}) → {c,d}
        assert_eq!(ns(&r).len(), 2);
    }

    #[test]
    fn empty_inputs_are_handled() {
        let doc = parse_str("<r><a/></r>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let empty = Value::NodeSet(Vec::new());
        let any   = ctx.eval("/r/a").unwrap();
        // ∅ − A = ∅
        assert_eq!(ns(&dispatch("difference",
            vec![empty.clone(), any.clone()], &ctx.index).unwrap().unwrap()).len(), 0);
        // A − ∅ = A
        assert_eq!(ns(&dispatch("difference",
            vec![any.clone(), empty.clone()], &ctx.index).unwrap().unwrap()).len(), 1);
        // ∅ ∩ A = ∅
        assert_eq!(ns(&dispatch("intersection",
            vec![empty, any], &ctx.index).unwrap().unwrap()).len(), 0);
    }

    #[test]
    fn unknown_function_returns_none() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        assert!(dispatch("nonsense", vec![], &ctx.index).is_none());
    }
}
