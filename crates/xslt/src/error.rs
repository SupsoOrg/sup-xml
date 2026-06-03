//! `XsltError` — single error type covering compile + apply + serialise.
//!
//! Wraps the underlying `XmlError` from `sup-xml-core` for any
//! XPath evaluation failures hit during a transformation (an
//! `<xsl:value-of select="bad-xpath()"/>` surfaces as `Xpath`),
//! and exposes engine-specific variants for stylesheet structure
//! problems and runtime termination.

use sup_xml_core::error::XmlError;

#[derive(Debug)]
pub enum XsltError {
    /// Stylesheet structure is malformed — e.g. an `xsl:template`
    /// with neither `match=` nor `name=`, a top-level element in
    /// the XSLT namespace that isn't a declaration.
    InvalidStylesheet(String),

    /// A template body referenced something the static analysis
    /// couldn't resolve — e.g. `xsl:call-template name="X"` where
    /// no template `X` exists.
    UnresolvedReference(String),

    /// An XPath expression failed during transformation.
    Xpath(XmlError),

    /// `<xsl:message terminate="yes">` fired.  Carries the
    /// message-element's stringified content.
    Terminated(String),
}

impl std::fmt::Display for XsltError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            XsltError::InvalidStylesheet(msg)  => write!(f, "invalid stylesheet: {msg}"),
            XsltError::UnresolvedReference(msg) => write!(f, "unresolved reference: {msg}"),
            XsltError::Xpath(e)                => write!(f, "xpath error: {}", e.message),
            XsltError::Terminated(msg)         => write!(f, "xsl:message terminate=yes: {msg}"),
        }
    }
}

impl std::error::Error for XsltError {}

impl From<XmlError> for XsltError {
    fn from(e: XmlError) -> Self { XsltError::Xpath(e) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sup_xml_core::error::{ErrorDomain, ErrorLevel};

    #[test]
    fn display_invalid_stylesheet() {
        let e = XsltError::InvalidStylesheet("bad <foo>".into());
        assert_eq!(format!("{e}"), "invalid stylesheet: bad <foo>");
    }

    #[test]
    fn display_unresolved_reference() {
        let e = XsltError::UnresolvedReference("template X".into());
        assert_eq!(format!("{e}"), "unresolved reference: template X");
    }

    #[test]
    fn display_xpath_wraps_inner_message() {
        let inner = XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, "syntax");
        let e = XsltError::Xpath(inner);
        assert_eq!(format!("{e}"), "xpath error: syntax");
    }

    #[test]
    fn display_terminated() {
        let e = XsltError::Terminated("user-requested halt".into());
        assert_eq!(format!("{e}"), "xsl:message terminate=yes: user-requested halt");
    }

    #[test]
    fn from_xml_error_wraps_as_xpath() {
        let inner = XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, "div by zero");
        let e: XsltError = inner.into();
        assert!(matches!(e, XsltError::Xpath(_)));
        assert_eq!(format!("{e}"), "xpath error: div by zero");
    }

    #[test]
    fn implements_std_error_trait() {
        // Smoke test: XsltError must be usable as `&dyn std::error::Error`.
        let e = XsltError::InvalidStylesheet("x".into());
        let err: &dyn std::error::Error = &e;
        assert!(err.to_string().contains("invalid stylesheet"));
    }

    #[test]
    fn debug_format_works() {
        // The #[derive(Debug)] impl — exercise it so it isn't listed as
        // an uncovered function.
        let e = XsltError::Terminated("x".into());
        let s = format!("{e:?}");
        assert!(s.contains("Terminated"));
    }
}
