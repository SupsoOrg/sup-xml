//! EXSLT extension function families — the math, date, str, and
//! set namespaces every real-world XSLT stylesheet (and many bare
//! XPath consumers) reach for.
//!
//! EXSLT lives *inside* the XPath crate, not inside XSLT, because
//! these are XPath function libraries — they plug into the XPath
//! function dispatcher and are useful even when no stylesheet is
//! involved (e.g. `etree.XPath("math:max(...)")`).
//!
//! Each family exposes a `dispatch(name, args, idx)` function that
//! returns `Some(result)` if the function name belongs to the
//! family, or `None` otherwise — letting the XPath evaluator chain
//! through user bindings → EXSLT families → built-ins.
//!
//! Spec references:
//! - https://exslt.org/math/
//! - https://exslt.org/date/
//! - https://exslt.org/str/
//! - https://exslt.org/set/

use crate::error::XmlError;
use crate::xpath::eval::{Numeric, Value};
use crate::xpath::index::DocIndexLike;

pub mod math;
pub mod date;
pub mod str;
pub mod sets;
pub mod regexp;
pub mod common;

type Result<T> = std::result::Result<T, XmlError>;

// ── namespace URIs ─────────────────────────────────────────────────

pub const MATH_NS:    &str = "http://exslt.org/math";
pub const DATE_NS:    &str = "http://exslt.org/dates-and-times";
pub const STR_NS:     &str = "http://exslt.org/strings";
pub const SET_NS:     &str = "http://exslt.org/sets";
pub const REGEXP_NS:  &str = "http://exslt.org/regular-expressions";
pub const COMMON_NS:  &str = "http://exslt.org/common";
pub const DYN_NS:     &str = "http://exslt.org/dynamic";
/// XPath 3.0 math namespace — distinct from EXSLT math.  Hosts
/// `math:pi()`, `math:sin()`, `math:pow()`, etc. per XPath 3.0
/// §4.8.  XSLT 3.0 §3.6 pre-binds this prefix.
pub const XPATH_MATH_NS: &str = "http://www.w3.org/2005/xpath-functions/math";

/// Dispatch an XPath function call to the matching EXSLT family.
/// Returns `Some(result)` when `ns_uri` matches one of the EXSLT
/// namespace URIs *and* the family recognises `name`; otherwise
/// `None`, so the caller can fall through to its own table or
/// surface "unregistered function."
///
/// Generic over the doc index because some EXSLT functions (e.g.
/// `set:distinct`, `str:tokenize` when given a nodeset) need to
/// read string-values of nodes.
pub fn dispatch<I: DocIndexLike>(
    ns_uri: &str,
    name: &str,
    args: Vec<Value>,
    idx: &I,
) -> Option<Result<Value>> {
    match ns_uri {
        MATH_NS        => math::dispatch(name, args, idx),
        DATE_NS        => date::dispatch(name, args, idx),
        STR_NS         => self::str::dispatch(name, args, idx),
        SET_NS         => sets::dispatch(name, args, idx),
        REGEXP_NS      => regexp::dispatch(name, args, idx),
        COMMON_NS      => common::dispatch(name, args, idx),
        XPATH_MATH_NS  => xpath_math_dispatch(name, args, idx),
        // DYN_NS (`dyn:evaluate`) is intercepted upstream in the XPath
        // evaluator because it needs the full `EvalCtx` to re-enter the
        // parser and evaluator at runtime — passing only `args` and
        // `idx` here is insufficient.
        _         => None,
    }
}

/// XPath 3.0 §4.8 math functions.  Names differ slightly from
/// EXSLT math (`power` → `pow`, plus `pi`, `exp10`, `log10`).
/// All operate on `xs:double` per the spec; we round-trip
/// through f64 since the value model already treats numbers as
/// double-precision.
fn xpath_math_dispatch<I: DocIndexLike>(
    name: &str, args: Vec<Value>, idx: &I,
) -> Option<Result<Value>> {
    use crate::error::{ErrorDomain, ErrorLevel};
    let err = |msg: &str| XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg.to_string());
    fn num<I: DocIndexLike>(v: &Value, idx: &I) -> f64 {
        crate::xpath::eval::value_to_number(v, idx)
    }
    let r = match name {
        "pi"   => {
            if !args.is_empty() { return Some(Err(err("math:pi() takes no arguments"))); }
            Ok(Value::Number(Numeric::Double(std::f64::consts::PI)))
        }
        "exp"   | "exp10" | "log"   | "log10"
        | "sqrt"  | "sin"   | "cos"   | "tan"
        | "asin"  | "acos"  | "atan" => {
            if args.len() != 1 {
                return Some(Err(err(
                    &format!("math:{name}() takes 1 argument"))));
            }
            let n = num(&args[0], idx);
            let r = match name {
                "exp"   => n.exp(),
                "exp10" => 10_f64.powf(n),
                "log"   => n.ln(),
                "log10" => n.log10(),
                "sqrt"  => n.sqrt(),
                "sin"   => n.sin(),
                "cos"   => n.cos(),
                "tan"   => n.tan(),
                "asin"  => n.asin(),
                "acos"  => n.acos(),
                "atan"  => n.atan(),
                _       => unreachable!(),
            };
            Ok(Value::Number(Numeric::Double(r)))
        }
        "pow" => {
            if args.len() != 2 {
                return Some(Err(err("math:pow() takes 2 arguments")));
            }
            let x = num(&args[0], idx);
            let y = num(&args[1], idx);
            Ok(Value::Number(Numeric::Double(x.powf(y))))
        }
        "atan2" => {
            if args.len() != 2 {
                return Some(Err(err("math:atan2() takes 2 arguments")));
            }
            let y = num(&args[0], idx);
            let x = num(&args[1], idx);
            Ok(Value::Number(Numeric::Double(y.atan2(x))))
        }
        _ => return None,
    };
    Some(r)
}
