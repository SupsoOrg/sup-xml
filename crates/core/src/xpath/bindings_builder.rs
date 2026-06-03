//! Ergonomic builder for [`XPathBindings`].
//!
//! Implementing the trait by hand is cheap once you've seen it, but
//! the boilerplate (one `match` per slot, threading `Option`,
//! tracking namespaces in a `HashMap`) gets old fast.
//! [`XPathBindingsBuilder`] wraps the three knobs callers actually
//! reach for — functions, variables, namespace prefixes — behind a
//! fluent builder, and impls the trait itself so it drops straight
//! into [`super::XPathContext::eval_with`].
//!
//! # Quick example
//!
//! ```
//! use sup_xml_core::{parse_str, ParseOptions, XPathContext};
//! use sup_xml_core::xpath::{XPathBindingsBuilder, eval::Value};
//!
//! let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
//! let ctx = XPathContext::new(&doc);
//!
//! let mut bindings = XPathBindingsBuilder::new();
//! bindings
//!     .namespace("my", "urn:my:ns")
//!     .bind_variable("greeting", Value::String("hello".into()))
//!     .function("urn:my:ns", "exclaim", |args| match args.into_iter().next() {
//!         Some(Value::String(s)) => Ok(Value::String(format!("{s}!"))),
//!         _ => Ok(Value::String(String::new())),
//!     });
//!
//! let v = ctx.eval_with("my:exclaim($greeting)", 0, &bindings).unwrap();
//! // v is `Value::String("hello!")`
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::XmlError;
use super::eval::{Value, XPathBindings};
use super::index::NodeId;

type BoxedFn = Arc<dyn Fn(Vec<Value>) -> Result<Value, XmlError> + Send + Sync>;

/// Builder + [`XPathBindings`] impl for caller-supplied namespaces,
/// variables, and extension functions.  Cheap to clone (functions
/// are stored in `Arc`).  Pass `&builder` (or any `&dyn
/// XPathBindings`) to [`super::XPathContext::eval_with`].
#[derive(Clone, Default)]
pub struct XPathBindingsBuilder {
    namespaces: HashMap<String, String>,
    variables:  HashMap<String, Value>,
    /// `(uri, local) -> closure`.  Two-level for fast lookup by
    /// namespace.  Unprefixed functions go under `""`.
    functions:  HashMap<String, HashMap<String, BoxedFn>>,
}

impl XPathBindingsBuilder {
    /// Empty builder — start here and chain.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `prefix` to `uri` for QName resolution in the XPath
    /// expression's static context.  Mirrors libxml2's
    /// `xmlXPathRegisterNs` / lxml's `namespaces=` kwarg.
    pub fn namespace(
        &mut self,
        prefix: impl Into<String>,
        uri:    impl Into<String>,
    ) -> &mut Self {
        self.namespaces.insert(prefix.into(), uri.into());
        self
    }

    /// Bind `$name` to `value`.  XPath references like `$name` (no
    /// prefix) and `$prefix:name` (prefix resolved against the
    /// namespace map) both consult this table.
    ///
    /// Method name is `bind_variable` (not `variable`) so it doesn't
    /// shadow the `XPathBindings::variable` trait method on the
    /// same type.
    pub fn bind_variable(
        &mut self,
        name:  impl Into<String>,
        value: Value,
    ) -> &mut Self {
        self.variables.insert(name.into(), value);
        self
    }

    /// Register `f` to handle `ns_uri:name(...)` calls.  Pass `""`
    /// as `ns_uri` to register a function under the default
    /// (unprefixed) namespace — that lets `name(...)` work without
    /// a prefix, but is rarely what you want because it can shadow
    /// XPath 1.0 built-ins.
    pub fn function<F>(
        &mut self,
        ns_uri: impl Into<String>,
        name:   impl Into<String>,
        f:      F,
    ) -> &mut Self
    where
        F: Fn(Vec<Value>) -> Result<Value, XmlError> + Send + Sync + 'static,
    {
        self.functions
            .entry(ns_uri.into())
            .or_default()
            .insert(name.into(), Arc::new(f));
        self
    }
}

impl XPathBindings for XPathBindingsBuilder {
    fn resolve_prefix(&self, prefix: &str) -> Option<String> {
        self.namespaces.get(prefix).cloned()
    }
    fn variable(&self, name: &str) -> Option<Value> {
        // Try the raw lookup first.  If the name carries a prefix
        // and the raw lookup misses, try the Clark form
        // `{uri}local` so callers can register either way.
        if let Some(v) = self.variables.get(name) { return Some(v.clone()); }
        if let Some((prefix, local)) = name.split_once(':') {
            if let Some(uri) = self.namespaces.get(prefix) {
                let clark = format!("{{{uri}}}{local}");
                if let Some(v) = self.variables.get(&clark) { return Some(v.clone()); }
            }
        }
        None
    }
    fn call_function(
        &self, ns_uri: &str, name: &str, args: Vec<Value>,
    ) -> Option<Result<Value, XmlError>> {
        let f = self.functions.get(ns_uri)?.get(name)?;
        Some(f(args))
    }
    fn call_function_in(
        &self, ns_uri: &str, name: &str, args: Vec<Value>,
        _xpath_context_node: NodeId,
    ) -> Option<Result<Value, XmlError>> {
        self.call_function(ns_uri, name, args)
    }
}

impl std::fmt::Debug for XPathBindingsBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut fn_names: Vec<String> = Vec::new();
        for (ns, m) in &self.functions {
            for k in m.keys() {
                fn_names.push(format!("{{{ns}}}{k}"));
            }
        }
        fn_names.sort();
        f.debug_struct("XPathBindingsBuilder")
            .field("namespaces", &self.namespaces)
            .field("variables",  &self.variables.keys().collect::<Vec<_>>())
            .field("functions",  &fn_names)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::eval::Numeric;
    use crate::{parse_str, ParseOptions, XPathContext};

    fn doc() -> sup_xml_tree::dom::Document {
        parse_str("<r><n>1</n><n>2</n><n>3</n></r>", &ParseOptions::default()).unwrap()
    }

    #[test]
    fn function_registered_and_called() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let mut b = XPathBindingsBuilder::new();
        b.namespace("my", "urn:my:ns")
            .function("urn:my:ns", "square", |args| {
                let n = match args.into_iter().next() {
                    Some(Value::Number(n)) => n.as_f64(),
                    _ => return Ok(Value::Number(Numeric::Double(f64::NAN))),
                };
                Ok(Value::Number(Numeric::Double(n * n)))
            });
        let v = ctx.eval_with("my:square(7)", 0, &b).unwrap();
        match v { Value::Number(n) => assert_eq!(n.as_f64(), 49.0), _ => panic!() }
    }

    #[test]
    fn variable_resolved() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let mut b = XPathBindingsBuilder::new();
        b.bind_variable("answer", Value::Number(Numeric::Double(42.0)));
        let v = ctx.eval_with("$answer + 1", 0, &b).unwrap();
        match v { Value::Number(n) => assert_eq!(n.as_f64(), 43.0), _ => panic!() }
    }

    #[test]
    fn namespace_prefix_resolves() {
        let d = parse_str(
            r#"<r xmlns:x="urn:x"><x:item>found</x:item></r>"#,
            &ParseOptions { namespace_aware: true, ..Default::default() },
        ).unwrap();
        let ctx = XPathContext::new(&d);
        let mut b = XPathBindingsBuilder::new();
        b.namespace("x", "urn:x");
        let v = ctx.eval_with("string(/r/x:item)", 0, &b).unwrap();
        match v { Value::String(s) => assert_eq!(s, "found"), _ => panic!() }
    }

    #[test]
    fn function_propagates_error() {
        use crate::error::{ErrorDomain, ErrorLevel};
        let d = doc();
        let ctx = XPathContext::new(&d);
        let mut b = XPathBindingsBuilder::new();
        b.namespace("my", "urn:my:ns")
            .function("urn:my:ns", "fail", |_args| {
                Err(XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, "boom"))
            });
        let r = ctx.eval_with("my:fail()", 0, &b);
        assert!(r.is_err());
        assert_eq!(r.unwrap_err().message, "boom");
    }

    #[test]
    fn unregistered_function_falls_through_to_unknown_error() {
        let d = doc();
        let ctx = XPathContext::new(&d);
        let mut b = XPathBindingsBuilder::new();
        b.namespace("my", "urn:my:ns");
        let r = ctx.eval_with("my:missing()", 0, &b);
        assert!(r.is_err());
    }
}
