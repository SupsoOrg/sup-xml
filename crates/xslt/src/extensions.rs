//! User-supplied extension functions for XSLT / XPath.
//!
//! Stylesheets can reference functions in non-XSLT namespaces (e.g.
//! `<xsl:value-of select="my:lookup($id)"/>` where `xmlns:my="urn:app"`).
//! The XPath engine consults the [`ExtensionFunctions`] trait when
//! it encounters such a call; if the trait's `call` returns `Some`,
//! the engine uses that result.  Returning `None` signals "I don't
//! handle this function" — the engine continues its fallback chain
//! (native EXSLT, then "unknown function" error).
//!
//! Two integration paths:
//!
//! 1. **`Extensions` builder** — register individual functions by
//!    `(namespace, name)`.  Best for the common case of a few
//!    short helpers:
//!
//!    ```ignore
//!    use sup_xml_xslt::{Extensions, XPathValue, Stylesheet};
//!
//!    let mut exts = Extensions::new();
//!    exts.register("urn:app", "double", |args| {
//!        let n = match args.first() {
//!            Some(XPathValue::Number(n)) => *n,
//!            Some(XPathValue::String(s)) => s.parse().unwrap_or(0.0),
//!            _ => 0.0,
//!        };
//!        Ok(XPathValue::Number(n * 2.0))
//!    });
//!    let style = Stylesheet::compile_str(/* ... */).unwrap();
//!    let result = style.apply_with_extensions(&source_doc, &exts)?;
//!    ```
//!
//! 2. **Custom [`ExtensionFunctions`] impl** — for cases that need
//!    state, dynamic dispatch over a wide function set, or
//!    integration with an existing function table.  Implement the
//!    trait directly on your own type.
//!
//! Both paths reach the same hook inside the XPath evaluator.

use std::collections::HashMap;

use sup_xml_core::error::XmlError;
use sup_xml_core::xpath::XPathValue;

/// Caller-supplied lookup for non-XSLT XPath functions.
///
/// Implement on your own type to integrate with an existing function
/// registry, or use the [`Extensions`] builder for per-call closure
/// registration.
pub trait ExtensionFunctions {
    /// Try to invoke `name` in `ns_uri` with `args`.  Return
    /// `Some(Ok(value))` on success, `Some(Err(_))` to surface a
    /// runtime error, or `None` if this implementation doesn't
    /// handle the call (the engine will continue its fallback
    /// chain — native EXSLT, then "unknown function" error).
    fn call(
        &self,
        ns_uri: &str,
        name:   &str,
        args:   Vec<XPathValue>,
    ) -> Option<Result<XPathValue, XmlError>>;
}

type ExtensionFn = Box<dyn Fn(Vec<XPathValue>) -> Result<XPathValue, XmlError>>;

/// Ergonomic builder for the common case of registering a handful of
/// closure-based extension functions keyed by `(namespace, name)`.
///
/// Lookups are O(1) and don't allocate per call (the &str argument is
/// matched directly against the stored String keys via `Borrow`).
#[derive(Default)]
pub struct Extensions {
    fns: HashMap<String, HashMap<String, ExtensionFn>>,
}

impl Extensions {
    pub fn new() -> Self { Self::default() }

    /// Register `f` to handle `ns_uri:name(...)` calls.  Re-registering
    /// the same (ns, name) pair replaces the previous closure.
    pub fn register<F>(
        &mut self,
        ns_uri: impl Into<String>,
        name:   impl Into<String>,
        f:      F,
    ) -> &mut Self
    where
        F: Fn(Vec<XPathValue>) -> Result<XPathValue, XmlError> + 'static,
    {
        self.fns.entry(ns_uri.into())
            .or_default()
            .insert(name.into(), Box::new(f));
        self
    }
}

impl ExtensionFunctions for Extensions {
    fn call(
        &self,
        ns_uri: &str,
        name:   &str,
        args:   Vec<XPathValue>,
    ) -> Option<Result<XPathValue, XmlError>> {
        let f = self.fns.get(ns_uri)?.get(name)?;
        Some(f(args))
    }
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<String> = Vec::new();
        for (ns, m) in &self.fns {
            for k in m.keys() {
                names.push(format!("{{{ns}}}{k}"));
            }
        }
        names.sort();
        f.debug_struct("Extensions").field("registered", &names).finish()
    }
}
