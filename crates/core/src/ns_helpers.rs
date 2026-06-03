//! Shared XML Namespaces 1.0 utilities used by [`crate::parser`] and
//! [`crate::stream_parser`].
//!
//! Lives outside the legacy [`crate::namespace`] module because it's used by
//! the new arena tree parsers, which don't depend on the legacy `Document` /
//! `ElementNode` types.  The legacy resolver keeps its own copies of these
//! checks for symmetry with its `Vec`-based tree.

use crate::error::{ErrorDomain, ErrorLevel, Result, XmlError};

// ── built-in namespace URIs (XML Namespaces 1.0) ─────────────────────────────

pub const XML_NS_URI:   &str = "http://www.w3.org/XML/1998/namespace";
pub const XMLNS_NS_URI: &str = "http://www.w3.org/2000/xmlns/";

// ── validation ──────────────────────────────────────────────────────────────

/// Validate QName syntax: at most one colon, both halves non-empty when a
/// colon is present.  XML Namespaces 1.0 § 3.
pub fn validate_qname(name: &str, context: &str) -> Result<()> {
    if let Some(colon) = name.find(':') {
        let prefix = &name[..colon];
        let local  = &name[colon + 1..];
        if prefix.is_empty() {
            return Err(ns_err(format!("empty prefix in {context} QName '{name}'")));
        }
        if local.is_empty() {
            return Err(ns_err(format!("empty local part in {context} QName '{name}'")));
        }
        if local.contains(':') {
            return Err(ns_err(format!("multiple ':' in {context} QName '{name}'")));
        }
    }
    Ok(())
}

/// Validate the right-hand side of an `xmlns:local="value"` declaration.
/// XML Namespaces 1.0 §§ 3, 6.
pub fn validate_xmlns_decl(local: &str, value: &str) -> Result<()> {
    if local.is_empty() {
        return Err(ns_err("empty local part in 'xmlns:' namespace declaration".into()));
    }
    if local == "xmlns" {
        return Err(ns_err("prefix 'xmlns' must not be declared".into()));
    }
    if local == "xml" && value != XML_NS_URI {
        return Err(ns_err(format!(
            "prefix 'xml' must be bound to '{XML_NS_URI}', not '{value}'"
        )));
    }
    if local != "xml" && value == XML_NS_URI {
        return Err(ns_err(format!(
            "prefix '{local}' cannot be bound to the XML namespace URI '{XML_NS_URI}'"
        )));
    }
    if value == XMLNS_NS_URI {
        return Err(ns_err(format!(
            "prefix '{local}' cannot be bound to the xmlns namespace URI '{XMLNS_NS_URI}'"
        )));
    }
    if value.is_empty() {
        return Err(ns_err(format!(
            "prefix '{local}' cannot be unbound (empty URI) in XML Namespaces 1.0"
        )));
    }
    Ok(())
}

pub fn ns_err(msg: String) -> XmlError {
    XmlError::new(ErrorDomain::Namespace, ErrorLevel::Fatal, msg)
}
