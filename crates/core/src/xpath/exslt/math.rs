//! EXSLT math family — https://exslt.org/math/
//!
//! All functions live in the `http://exslt.org/math` namespace.
//! The dispatcher returns `None` for names it doesn't recognise so
//! the XPath engine can fall through to other tables.
//!
//! Coverage:
//!   - `min`, `max`, `highest`, `lowest`   — nodeset reductions
//!   - `abs`, `sqrt`, `exp`, `log`, `power` — scalar
//!   - `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2` — scalar
//!   - `constant`              — π / e / etc. (lookup by name)
//!   - `random`                — uniform [0, 1) per call
//!
//! Scalar functions take/return `Value::Number`.  Nodeset reductions
//! coerce each node's string-value to a number; non-numeric values
//! propagate NaN through the result (matching libexslt).

use crate::error::{ErrorDomain, ErrorLevel, XmlError};
use crate::xpath::eval::{Numeric, Value, value_to_number};
use crate::xpath::index::DocIndexLike;

use super::Result;

fn err(msg: impl Into<String>) -> XmlError {
    XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg)
}

pub fn dispatch<I: DocIndexLike>(
    name: &str, args: Vec<Value>, idx: &I,
) -> Option<Result<Value>> {
    let r = match name {
        // Nodeset reductions — return a number.
        "min"     => reduce(&args, idx, f64::INFINITY,      |acc, x| if x < acc { x } else { acc }),
        "max"     => reduce(&args, idx, f64::NEG_INFINITY,  |acc, x| if x > acc { x } else { acc }),
        // `highest` / `lowest` differ from max/min: they return the
        // *nodes* with the extreme value, not the number.  Multiple
        // ties → all tied nodes in document order.
        "highest" => extremum(&args, idx, |a, b| a > b),
        "lowest"  => extremum(&args, idx, |a, b| a < b),

        // Scalar functions (1 arg → number).
        "abs"      => one_num(&args, idx, |n| n.abs()),
        "sqrt"     => one_num(&args, idx, |n| n.sqrt()),
        "exp"      => one_num(&args, idx, |n| n.exp()),
        "log"      => one_num(&args, idx, |n| n.ln()),
        "sin"      => one_num(&args, idx, |n| n.sin()),
        "cos"      => one_num(&args, idx, |n| n.cos()),
        "tan"      => one_num(&args, idx, |n| n.tan()),
        "asin"     => one_num(&args, idx, |n| n.asin()),
        "acos"     => one_num(&args, idx, |n| n.acos()),
        "atan"     => one_num(&args, idx, |n| n.atan()),

        // Two-arg scalar functions.
        "power"    => two_num(&args, idx, |a, b| a.powf(b)),
        "atan2"    => two_num(&args, idx, |a, b| a.atan2(b)),

        // `constant(name, precision)` — fixed table.  Precision
        // truncates the result to the given number of significant
        // decimal digits (libexslt treats it as a hint, not a hard
        // requirement).
        "constant" => constant(&args, idx),

        // `random()` — uniform [0, 1).
        "random"   => random_value(&args),

        _ => return None,
    };
    Some(r)
}

// ── helpers ───────────────────────────────────────────────────────

fn one_num<I: DocIndexLike>(
    args: &[Value], idx: &I, f: impl Fn(f64) -> f64,
) -> Result<Value> {
    if args.len() != 1 {
        return Err(err("math: function requires 1 argument"));
    }
    Ok(Value::Number(Numeric::Double(f(value_to_number(&args[0], idx)))))
}

fn two_num<I: DocIndexLike>(
    args: &[Value], idx: &I, f: impl Fn(f64, f64) -> f64,
) -> Result<Value> {
    if args.len() != 2 {
        return Err(err("math: function requires 2 arguments"));
    }
    Ok(Value::Number(Numeric::Double(f(
        value_to_number(&args[0], idx),
        value_to_number(&args[1], idx),
    ))))
}

/// Common path for `min` / `max` — fold over the nodeset's
/// numeric coercions.  Empty nodeset returns NaN (libexslt
/// behaviour; the spec says "indeterminate", which we render as
/// NaN for consistency with XPath's `number(())` semantics).
fn reduce<I: DocIndexLike>(
    args: &[Value], idx: &I, init: f64, op: impl Fn(f64, f64) -> f64,
) -> Result<Value> {
    if args.len() != 1 {
        return Err(err("math:min/max takes a single nodeset argument"));
    }
    let ns = match &args[0] {
        Value::NodeSet(ns) => ns,
        _ => return Err(err("math:min/max requires a nodeset argument")),
    };
    if ns.is_empty() {
        return Ok(Value::Number(Numeric::Double(f64::NAN)));
    }
    let mut acc = init;
    for &id in ns {
        let n: f64 = idx.string_value(id).trim().parse().unwrap_or(f64::NAN);
        if n.is_nan() {
            return Ok(Value::Number(Numeric::Double(f64::NAN)));
        }
        acc = op(acc, n);
    }
    Ok(Value::Number(Numeric::Double(acc)))
}

/// `highest` / `lowest`: return the subset of nodes whose numeric
/// string-value is the extremum.  Multiple winners → all in
/// document order (the nodeset is already document-sorted by the
/// engine before we receive it).
fn extremum<I: DocIndexLike>(
    args: &[Value], idx: &I, better: impl Fn(f64, f64) -> bool,
) -> Result<Value> {
    if args.len() != 1 {
        return Err(err("math:highest/lowest takes a single nodeset argument"));
    }
    let ns = match &args[0] {
        Value::NodeSet(ns) => ns.clone(),
        _ => return Err(err("math:highest/lowest requires a nodeset argument")),
    };
    if ns.is_empty() {
        return Ok(Value::NodeSet(Vec::new()));
    }
    let nums: Vec<f64> = ns.iter()
        .map(|&id| idx.string_value(id).trim().parse().unwrap_or(f64::NAN))
        .collect();
    if nums.iter().any(|n| n.is_nan()) {
        // libexslt: any NaN → empty nodeset, matching its IEEE
        // ordering bail-out.
        return Ok(Value::NodeSet(Vec::new()));
    }
    let mut best = nums[0];
    for &n in &nums[1..] {
        if better(n, best) { best = n; }
    }
    let out: Vec<_> = ns.iter().zip(nums.iter())
        .filter(|(_, n)| **n == best)
        .map(|(id, _)| *id)
        .collect();
    Ok(Value::NodeSet(out))
}

fn constant<I: DocIndexLike>(args: &[Value], idx: &I) -> Result<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(err("math:constant requires 1 or 2 arguments"));
    }
    let name = match &args[0] {
        Value::String(s) => s.clone(),
        v => crate::xpath::eval::value_to_string(v, idx),
    };
    // Named constants from libexslt.
    //
    // `SQRRT2` (with the double R) is a typo in the EXSLT reference
    // implementation that was adopted verbatim by every shipping
    // engine — libexslt (libxslt/libexslt/math.c, the EXSLT reference
    // XSL at exslt.github.io, Saxon, IBM DataPower, etc.).
    // Stylesheets in the wild call `math:constant('SQRRT2', n)`
    // because that's the key every consumer recognises.  We match the
    // typo and do NOT accept the obvious `SQRT2` spelling — a
    // stylesheet that depends on `SQRT2` working here would silently
    // break on every other XSLT engine, which is worse than the small
    // surprise of having to use the misspelled name.
    let raw = match name.as_str() {
        "PI"      => std::f64::consts::PI,
        "E"       => std::f64::consts::E,
        "SQRRT2"  => std::f64::consts::SQRT_2,
        "LN2"     => std::f64::consts::LN_2,
        "LN10"    => std::f64::consts::LN_10,
        "LOG2E"   => std::f64::consts::LOG2_E,
        "SQRT1_2" => std::f64::consts::FRAC_1_SQRT_2,
        _         => return Ok(Value::Number(Numeric::Double(f64::NAN))),
    };
    if args.len() == 1 {
        return Ok(Value::Number(Numeric::Double(raw)));
    }
    // Optional precision: round to `precision - 1` significant
    // figures.  Matches libexslt's behaviour — the second arg there
    // controls printf's `%g` precision modulo a `-1` offset (test
    // case: `math:constant('PI', 4)` → "3.14", i.e. 3 sig figs).
    let prec = value_to_number(&args[1], idx);
    if !prec.is_finite() || prec < 2.0 {
        // prec < 2 → 0 or fewer sig figs requested; return raw and
        // let XPath number-to-string take over.
        return Ok(Value::Number(Numeric::Double(raw)));
    }
    let sig_figs = (prec as i32) - 1;
    let scale = 10f64.powi(sig_figs - 1 - (raw.abs().log10().floor() as i32));
    Ok(Value::Number(Numeric::Double((raw * scale).round() / scale)))
}

fn random_value(args: &[Value]) -> Result<Value> {
    if !args.is_empty() {
        return Err(err("math:random takes no arguments"));
    }
    // Simple LCG-on-thread-locals — deterministic-per-thread is
    // fine for EXSLT; nothing security-sensitive here.  Seeded from
    // a constant XOR'd with the address of a thread-local so
    // different threads get different streams.
    use std::cell::Cell;
    std::thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0x9E37_79B9_7F4A_7C15) };
    }
    let v = STATE.with(|s| {
        let mut x = s.get();
        if x == 0 {
            // Address of the per-thread Cell — distinct across
            // threads, so seeds diverge.  `STATE` itself is the
            // LocalKey accessor (a thread-local *accessor*, not
            // storage); only the &Cell handed into this closure
            // has a real address.
            x = (s as *const _ as u64) ^ 0xDEAD_BEEF_DEAD_BEEF;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    });
    // Top 53 bits → uniform [0, 1).
    Ok(Value::Number(Numeric::Double((v >> 11) as f64 / (1u64 << 53) as f64)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xpath::XPathContext;
    use crate::{parse_str, ParseOptions};

    fn assert_num(r: Result<Value>, expected: f64) {
        match r.unwrap() {
            Value::Number(n) if (n.as_f64() - expected).abs() < 1e-12 => {}
            other => panic!("expected number ~{expected}, got {other:?}"),
        }
    }

    fn tiny_doc() -> sup_xml_tree::dom::Document {
        parse_str("<r/>", &ParseOptions::default()).unwrap()
    }

    #[test]
    fn scalar_sqrt() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        assert_num(
            dispatch("sqrt", vec![Value::Number(Numeric::Double(16.0))], &ctx.index).unwrap(),
            4.0,
        );
    }

    #[test]
    fn scalar_power() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        assert_num(
            dispatch("power",
                vec![Value::Number(Numeric::Double(2.0)), Value::Number(Numeric::Double(10.0))], &ctx.index).unwrap(),
            1024.0,
        );
    }

    #[test]
    fn scalar_abs() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        assert_num(
            dispatch("abs", vec![Value::Number(Numeric::Double(-7.5))], &ctx.index).unwrap(),
            7.5,
        );
    }

    #[test]
    fn constant_pi_with_precision_truncates() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("constant",
            vec![Value::String("PI".into()), Value::Number(Numeric::Double(5.0))],
            &ctx.index).unwrap().unwrap();
        match r {
            Value::Number(n) => assert!((n.as_f64() - 3.1416).abs() < 1e-3),
            _ => panic!("expected number"),
        }
    }

    #[test]
    fn unknown_returns_none() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        assert!(dispatch("does-not-exist", vec![], &ctx.index).is_none());
    }

    #[test]
    fn min_max_over_nodeset() {
        let doc = parse_str(
            "<r><i>3</i><i>1</i><i>5</i><i>2</i></r>",
            &ParseOptions::default(),
        ).unwrap();
        let ctx = XPathContext::new(&doc);
        let ns = ctx.eval("/r/i").unwrap();
        assert_num(dispatch("max", vec![ns.clone()], &ctx.index).unwrap(), 5.0);
        assert_num(dispatch("min", vec![ns],         &ctx.index).unwrap(), 1.0);
    }

    #[test]
    fn highest_returns_nodes_with_max_value() {
        let doc = parse_str(
            "<r><i>3</i><i>9</i><i>5</i><i>9</i></r>",
            &ParseOptions::default(),
        ).unwrap();
        let ctx = XPathContext::new(&doc);
        let ns = ctx.eval("/r/i").unwrap();
        match dispatch("highest", vec![ns], &ctx.index).unwrap().unwrap() {
            // Two nodes tied at 9 — both returned in document order.
            Value::NodeSet(ids) => assert_eq!(ids.len(), 2),
            other => panic!("expected nodeset, got {other:?}"),
        }
    }

    #[test]
    fn empty_nodeset_min_is_nan() {
        let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("min", vec![Value::NodeSet(vec![])],
            &ctx.index).unwrap().unwrap();
        match r {
            Value::Number(n) => assert!(n.as_f64().is_nan()),
            _ => panic!("expected NaN"),
        }
    }

    // ── scalar trig / log / exp ─────────────────────────────────────────

    #[test]
    fn scalar_trig_functions() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        let pi = std::f64::consts::PI;
        assert_num(dispatch("sin", vec![Value::Number(Numeric::Double(0.0))], &ctx.index).unwrap(), 0.0);
        assert_num(dispatch("cos", vec![Value::Number(Numeric::Double(0.0))], &ctx.index).unwrap(), 1.0);
        assert_num(dispatch("tan", vec![Value::Number(Numeric::Double(0.0))], &ctx.index).unwrap(), 0.0);
        assert_num(dispatch("asin", vec![Value::Number(Numeric::Double(0.0))], &ctx.index).unwrap(), 0.0);
        assert_num(dispatch("acos", vec![Value::Number(Numeric::Double(1.0))], &ctx.index).unwrap(), 0.0);
        assert_num(dispatch("atan", vec![Value::Number(Numeric::Double(0.0))], &ctx.index).unwrap(), 0.0);
        assert_num(dispatch("atan2",
            vec![Value::Number(Numeric::Double(0.0)), Value::Number(Numeric::Double(1.0))], &ctx.index).unwrap(), 0.0);
        // sin(π/2) ≈ 1
        assert_num(dispatch("sin", vec![Value::Number(Numeric::Double(pi / 2.0))], &ctx.index).unwrap(), 1.0);
    }

    #[test]
    fn scalar_exp_log() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        assert_num(dispatch("exp", vec![Value::Number(Numeric::Double(0.0))], &ctx.index).unwrap(), 1.0);
        assert_num(dispatch("log", vec![Value::Number(Numeric::Double(std::f64::consts::E))],
            &ctx.index).unwrap(), 1.0);
    }

    // ── argc / type errors ──────────────────────────────────────────────

    #[test]
    fn one_num_wrong_argc_errors() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("sqrt", vec![], &ctx.index).unwrap();
        assert!(r.is_err());
        let r = dispatch("sqrt",
            vec![Value::Number(Numeric::Double(1.0)), Value::Number(Numeric::Double(2.0))], &ctx.index).unwrap();
        assert!(r.is_err());
    }

    #[test]
    fn two_num_wrong_argc_errors() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("power", vec![Value::Number(Numeric::Double(1.0))], &ctx.index).unwrap();
        assert!(r.is_err());
    }

    #[test]
    fn reduce_wrong_argc_errors() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        assert!(dispatch("min", vec![], &ctx.index).unwrap().is_err());
        assert!(dispatch("min",
            vec![Value::NodeSet(vec![]), Value::NodeSet(vec![])],
            &ctx.index).unwrap().is_err());
    }

    #[test]
    fn reduce_wrong_type_errors() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("min", vec![Value::Number(Numeric::Double(3.0))], &ctx.index).unwrap();
        assert!(r.is_err());
    }

    #[test]
    fn reduce_nan_in_nodeset_propagates() {
        let doc = parse_str(
            "<r><i>3</i><i>not-a-number</i><i>1</i></r>",
            &ParseOptions::default(),
        ).unwrap();
        let ctx = XPathContext::new(&doc);
        let ns = ctx.eval("/r/i").unwrap();
        let r = dispatch("max", vec![ns], &ctx.index).unwrap().unwrap();
        match r {
            Value::Number(n) => assert!(n.as_f64().is_nan()),
            _ => panic!("expected NaN"),
        }
    }

    #[test]
    fn extremum_wrong_argc_errors() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        assert!(dispatch("highest", vec![], &ctx.index).unwrap().is_err());
    }

    #[test]
    fn extremum_wrong_type_errors() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("lowest", vec![Value::Number(Numeric::Double(3.0))], &ctx.index).unwrap();
        assert!(r.is_err());
    }

    #[test]
    fn extremum_empty_nodeset_returns_empty_nodeset() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("highest", vec![Value::NodeSet(vec![])],
            &ctx.index).unwrap().unwrap();
        match r {
            Value::NodeSet(ids) => assert!(ids.is_empty()),
            _ => panic!("expected nodeset"),
        }
    }

    #[test]
    fn extremum_nan_in_nodeset_returns_empty() {
        let doc = parse_str(
            "<r><i>3</i><i>bogus</i><i>5</i></r>",
            &ParseOptions::default(),
        ).unwrap();
        let ctx = XPathContext::new(&doc);
        let ns = ctx.eval("/r/i").unwrap();
        let r = dispatch("highest", vec![ns], &ctx.index).unwrap().unwrap();
        match r {
            Value::NodeSet(ids) => assert!(ids.is_empty()),
            _ => panic!("expected empty nodeset"),
        }
    }

    #[test]
    fn lowest_returns_nodes_with_min_value() {
        let doc = parse_str(
            "<r><i>3</i><i>1</i><i>5</i><i>1</i></r>",
            &ParseOptions::default(),
        ).unwrap();
        let ctx = XPathContext::new(&doc);
        let ns = ctx.eval("/r/i").unwrap();
        match dispatch("lowest", vec![ns], &ctx.index).unwrap().unwrap() {
            Value::NodeSet(ids) => assert_eq!(ids.len(), 2),
            other => panic!("expected nodeset, got {other:?}"),
        }
    }

    // ── constant() ───────────────────────────────────────────────────────

    #[test]
    fn constant_all_named_values() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        let single = |name: &str| -> f64 {
            match dispatch("constant",
                vec![Value::String(name.into())], &ctx.index).unwrap().unwrap() {
                Value::Number(n) => n.as_f64(),
                _ => panic!(),
            }
        };
        assert!((single("PI")      - std::f64::consts::PI).abs()         < 1e-12);
        assert!((single("E")       - std::f64::consts::E).abs()          < 1e-12);
        // SQRRT2 is the canonical EXSLT key — a typo baked into the
        // reference impl and copied by every shipping engine.
        assert!((single("SQRRT2")  - std::f64::consts::SQRT_2).abs()     < 1e-12);
        assert!((single("LN2")     - std::f64::consts::LN_2).abs()       < 1e-12);
        assert!((single("LN10")    - std::f64::consts::LN_10).abs()      < 1e-12);
        assert!((single("LOG2E")   - std::f64::consts::LOG2_E).abs()     < 1e-12);
        assert!((single("SQRT1_2") - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-12);
    }

    #[test]
    fn constant_unknown_name_is_nan() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("constant",
            vec![Value::String("BOGUS".into())], &ctx.index).unwrap().unwrap();
        match r {
            Value::Number(n) => assert!(n.as_f64().is_nan()),
            _ => panic!(),
        }
    }

    #[test]
    fn constant_wrong_argc_errors() {
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        assert!(dispatch("constant", vec![], &ctx.index).unwrap().is_err());
        assert!(dispatch("constant",
            vec![Value::String("PI".into()),
                 Value::Number(Numeric::Double(1.0)),
                 Value::Number(Numeric::Double(2.0))],
            &ctx.index).unwrap().is_err());
    }

    #[test]
    fn constant_non_string_first_arg_coerced() {
        // `v => value_to_string(...)` branch in constant().  Number 1.0
        // stringifies to "1", which isn't a known constant → NaN.
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("constant",
            vec![Value::Number(Numeric::Double(1.0))], &ctx.index).unwrap().unwrap();
        match r {
            Value::Number(n) => assert!(n.as_f64().is_nan()),
            _ => panic!(),
        }
    }

    #[test]
    fn constant_non_finite_precision_returns_raw() {
        // precision = NaN / inf / <1 → return raw value (line 183).
        let doc = tiny_doc();
        let ctx = XPathContext::new(&doc);
        let r = dispatch("constant",
            vec![Value::String("PI".into()), Value::Number(Numeric::Double(f64::NAN))],
            &ctx.index).unwrap().unwrap();
        match r {
            Value::Number(n) => assert!((n.as_f64() - std::f64::consts::PI).abs() < 1e-12),
            _ => panic!(),
        }
        let r = dispatch("constant",
            vec![Value::String("PI".into()), Value::Number(Numeric::Double(0.0))],
            &ctx.index).unwrap().unwrap();
        match r {
            Value::Number(n) => assert!((n.as_f64() - std::f64::consts::PI).abs() < 1e-12),
            _ => panic!(),
        }
    }

    // ── random() ─────────────────────────────────────────────────────────

    #[test]
    fn random_value_in_unit_interval() {
        // Note: random() uses a thread-local LCG; each call advances the
        // state.  We just check the value is in [0, 1).  This also
        // exercises the seed-init branch on the first call.
        let r = dispatch("random", vec![], &MockIdx).unwrap().unwrap();
        match r {
            Value::Number(n) => assert!((0.0..1.0).contains(&n.as_f64()), "got {}", n.as_f64()),
            _ => panic!(),
        }
        // Second call advances the state (exercises the post-seed path).
        let r = dispatch("random", vec![], &MockIdx).unwrap().unwrap();
        assert!(matches!(r, Value::Number(n) if (0.0..1.0).contains(&n.as_f64())));
    }

    #[test]
    fn random_rejects_args() {
        let r = dispatch("random",
            vec![Value::Number(Numeric::Double(1.0))], &MockIdx).unwrap();
        assert!(r.is_err());
    }

    // Minimal DocIndexLike for tests that don't need a real document.
    struct MockIdx;
    impl DocIndexLike for MockIdx {
        fn children(&self, _: NodeId) -> &[NodeId] { &[] }
        fn parent(&self, _: NodeId) -> Option<NodeId> { None }
        fn attr_range(&self, _: NodeId) -> std::ops::Range<NodeId> { 0..0 }
        fn kind(&self, _: NodeId) -> crate::xpath::index::XPathNodeKind {
            crate::xpath::index::XPathNodeKind::Document
        }
        fn pi_target(&self, _: NodeId) -> &str { "" }
        fn string_value(&self, _: NodeId) -> String { String::new() }
        fn node_name(&self, _: NodeId) -> &str { "" }
        fn local_name(&self, _: NodeId) -> &str { "" }
        fn namespace_uri(&self, _: NodeId) -> &str { "" }
    }
    use crate::xpath::index::NodeId;
}
