//! EXSLT common family — http://exslt.org/common
//!
//! Two functions:
//!
//! | Function            | Status      | Notes                                       |
//! |---------------------|-------------|---------------------------------------------|
//! | `exsl:node-set`     | implemented | nodeset → identity; scalar → 1-text-node    |
//! | `exsl:object-type`  | implemented | XPath value-type discriminant string         |
//!
//! `exsl:node-set`'s motivating use-case is unwrapping XSLT
//! result-tree-fragment variables (`<xsl:variable name="x"><foo/>
//! </xsl:variable>`) so they can be traversed.  That XSLT-specific
//! materialisation is intercepted by the XSLT engine's bindings
//! before reaching this fallback dispatcher; the path here covers
//! the bare-XPath and "already a node-set" cases.

use crate::error::{ErrorDomain, ErrorLevel, XmlError};
use crate::xpath::eval::{Value, value_to_string};
use crate::xpath::index::DocIndexLike;

use super::Result;

fn err(msg: impl Into<String>) -> XmlError {
    XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg)
}

pub fn dispatch<I: DocIndexLike>(
    name: &str, args: Vec<Value>, idx: &I,
) -> Option<Result<Value>> {
    match name {
        "node-set"    => Some(node_set_fn(&args, idx)),
        "object-type" => Some(object_type_fn(&args, idx)),
        _ => None,
    }
}

/// `exsl:node-set(value) → node-set`.
///
/// - If `value` is already a node-set, return it unchanged.
/// - For a scalar (string / number / boolean), allocate a single
///   synthetic text node holding the string-value and return a
///   one-element node-set.  This matches libexslt's behaviour for
///   non-RTF inputs.
/// - Foreign node-sets pass through (they're already navigable).
///
/// The interesting XSLT case — `exsl:node-set($rtf-variable)` — is
/// handled inside the XSLT engine's bindings before reaching this
/// dispatcher.  By the time control lands here, the argument has
/// already been resolved to a `Value`; there's no way to recover
/// the RTF structure from a stringified value.
fn node_set_fn<I: DocIndexLike>(args: &[Value], idx: &I) -> Result<Value> {
    if args.len() != 1 {
        return Err(err("exsl:node-set takes 1 argument"));
    }
    match &args[0] {
        Value::NodeSet(_) | Value::ForeignNodeSet(_) => Ok(args[0].clone()),
        scalar => {
            let s = value_to_string(scalar, idx);
            let ids = idx.allocate_rtf_text_nodes(vec![s]).ok_or_else(|| err(
                "exsl:node-set: this XPath context does not support RTF allocation",
            ))?;
            Ok(Value::NodeSet(ids))
        }
    }
}

/// `exsl:object-type(value) → string` — discriminate XPath value
/// types.  Used by defensive library stylesheets that branch on
/// argument type.  Returns one of:
///
/// * `"node-set"` — `Value::NodeSet` / `Value::ForeignNodeSet`
/// * `"string"`   — `Value::String`
/// * `"number"`   — `Value::Number`
/// * `"boolean"`  — `Value::Boolean`
///
/// (No `"RTF"` value because by the time the function sees its
/// argument, an RTF has already been coerced — the XSLT engine
/// hands stringified RTFs through as `Value::String`.)
fn object_type_fn<I: DocIndexLike>(args: &[Value], _idx: &I) -> Result<Value> {
    if args.len() != 1 {
        return Err(err("exsl:object-type takes 1 argument"));
    }
    let label = match &args[0] {
        Value::NodeSet(_) | Value::ForeignNodeSet(_) => "node-set",
        Value::String(_)  => "string",
        Value::Number(_)  => "number",
        Value::Boolean(_) => "boolean",
        Value::Typed(t)   => {
            // Map the typed atomic back to one of EXSL's four
            // coarse labels.  Numeric kinds → "number"; boolean
            // → "boolean"; everything else (date / duration /
            // string / etc.) → "string".
            if t.numeric.is_some() { "number" }
            else if t.boolean.is_some() { "boolean" }
            else { "string" }
        }
        // EXSLT predates atomic sequences; label them as the
        // generic "node-set" since that's what callers expect to
        // iterate.
        Value::Sequence(_) | Value::IntRange { .. } => "node-set",
        Value::Map(_)     => "map",
        Value::Array(_)   => "array",
        Value::Function(_) => "function",
    };
    Ok(Value::String(label.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xpath::eval::Numeric;
    use crate::xpath::XPathContext;
    use crate::{parse_str, ParseOptions};

    fn doc() -> sup_xml_tree::dom::Document {
        parse_str("<r><a/><b/></r>", &ParseOptions::default()).unwrap()
    }

    #[test]
    fn node_set_passes_nodeset_through() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let ns = ctx.eval("/r/*").unwrap();
        let original_len = match &ns { Value::NodeSet(n) => n.len(), _ => panic!() };
        let r = dispatch("node-set", vec![ns], &ctx.index).unwrap().unwrap();
        match r {
            Value::NodeSet(n) => assert_eq!(n.len(), original_len),
            _ => panic!("expected node-set"),
        }
    }

    #[test]
    fn node_set_wraps_string_in_single_text_node() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let r = dispatch("node-set",
            vec![Value::String("hello".into())], &ctx.index).unwrap().unwrap();
        let ns = match r { Value::NodeSet(ns) => ns, _ => panic!() };
        assert_eq!(ns.len(), 1);
        assert_eq!(ctx.index.string_value(ns[0]), "hello");
    }

    #[test]
    fn node_set_wraps_number_in_text_node_with_xpath_string_form() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let r = dispatch("node-set",
            vec![Value::Number(Numeric::Double(42.0))], &ctx.index).unwrap().unwrap();
        let ns = match r { Value::NodeSet(ns) => ns, _ => panic!() };
        assert_eq!(ctx.index.string_value(ns[0]), "42");
    }

    #[test]
    fn object_type_returns_correct_label_per_variant() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let cases: &[(Value, &str)] = &[
            (Value::String("x".into()),  "string"),
            (Value::Number(Numeric::Double(1.0)),         "number"),
            (Value::Boolean(true),       "boolean"),
            (Value::NodeSet(vec![]),     "node-set"),
        ];
        for (arg, expected) in cases {
            let r = dispatch("object-type", vec![arg.clone()], &ctx.index)
                .unwrap().unwrap();
            assert!(matches!(r, Value::String(ref s) if s == expected),
                "got {r:?} for arg {arg:?}");
        }
    }

    #[test]
    fn unknown_function_returns_none() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        assert!(dispatch("nonsense", vec![], &ctx.index).is_none());
    }

    // ── dyn:evaluate end-to-end (lives in eval, not dispatch) ──

    #[test]
    fn dyn_evaluate_runs_string_as_xpath_against_context() {
        use crate::xpath::XPathBindingsBuilder;
        let d = parse_str(
            "<r><a>alpha</a><a>beta</a><a>gamma</a></r>",
            &ParseOptions::default(),
        ).unwrap();
        let ctx = crate::xpath::XPathContext::new(&d);
        let mut bind = XPathBindingsBuilder::new();
        bind.namespace("dyn", "http://exslt.org/dynamic");
        let v = ctx.eval_with("dyn:evaluate('count(/r/a)')", 0, &bind).unwrap();
        assert_eq!(crate::xpath::eval::value_to_number(&v, &ctx.index), 3.0);
    }

    #[test]
    fn dyn_evaluate_invalid_expression_yields_empty_nodeset() {
        use crate::xpath::XPathBindingsBuilder;
        let d = doc();
        let ctx = crate::xpath::XPathContext::new(&d);
        let mut bind = XPathBindingsBuilder::new();
        bind.namespace("dyn", "http://exslt.org/dynamic");
        let v = ctx.eval_with("dyn:evaluate('not a valid xpath /// ')", 0, &bind).unwrap();
        assert!(matches!(v, Value::NodeSet(ref ns) if ns.is_empty()));
    }
}
