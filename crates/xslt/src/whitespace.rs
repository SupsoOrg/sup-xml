//! Source-tree whitespace handling — implements
//! `xsl:strip-space` / `xsl:preserve-space` (XSLT 1.0 §3.4).
//!
//! When the engine iterates source nodes (via `apply-templates`
//! without explicit `select=`, or via the built-in element-rule),
//! whitespace-only text nodes whose parent element matches a
//! strip-space rule are filtered out — they don't trigger the
//! built-in text-rule and don't emit text into the result tree.
//!
//! Precedence (XSLT 1.0 §3.4): when a node matches both
//! strip-space and preserve-space patterns, the more-specific
//! pattern wins.  Specificity uses pattern-default-priority +
//! declaration-order — same rules as `xsl:template` priorities.

use sup_xml_core::xpath::{DocIndexLike, NodeId, XPathNodeKind};

use crate::ast::{QName, StylesheetAst, WhitespaceRule};

/// XSLT 1.0 § 3.4: a "whitespace text node" is one whose content
/// consists entirely of the four XML whitespace characters — space
/// (`#x20`), tab (`#x9`), CR (`#xD`), and LF (`#xA`).  Notably this
/// does NOT include NEL (`#x85`), LSEP (`#x2028`), or any of the
/// other Unicode whitespace categories that Rust's `char::is_whitespace`
/// recognises.  Using the spec-correct set is what lets XML 1.1
/// stylesheets preserve NEL/LSEP content through the result tree.
pub fn is_xslt_whitespace_only(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
}

/// Decide whether a text node should be stripped (true) or
/// preserved (false) under the stylesheet's whitespace rules.
/// Non-whitespace-only text is always preserved; whitespace-only
/// text consults the rules.
pub fn should_strip<I: DocIndexLike>(
    style:  &StylesheetAst,
    node:   NodeId,
    idx:    &I,
) -> bool {
    // Only text/CData nodes are candidates.
    if !matches!(idx.kind(node), XPathNodeKind::Text | XPathNodeKind::CData) {
        return false;
    }
    let content = idx.string_value(node);
    if !is_xslt_whitespace_only(&content) {
        return false;
    }
    let Some(parent) = idx.parent(node) else { return false; };
    if !matches!(idx.kind(parent), XPathNodeKind::Element) {
        return false;
    }
    // XSLT 1.0 §3.4 — `xml:space` overrides the strip-space rules: a
    // whitespace-only text node is preserved when the nearest ancestor
    // element bearing an `xml:space` attribute has the value `preserve`.
    if nearest_xml_space(parent, idx) == Some(XmlSpace::Preserve) {
        return false;
    }
    let parent_local = idx.local_name(parent);
    let parent_uri   = idx.namespace_uri(parent);
    strips_whitespace_under(style, parent_local, parent_uri)
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum XmlSpace { Default, Preserve }

const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";

/// The effective `xml:space` value at `elem`: the value declared on the
/// nearest ancestor-or-self element that carries an `xml:space`
/// attribute (XML 1.0 §2.10), or `None` when none does.
fn nearest_xml_space<I: DocIndexLike>(elem: NodeId, idx: &I) -> Option<XmlSpace> {
    let mut cur = Some(elem);
    while let Some(e) = cur {
        if matches!(idx.kind(e), XPathNodeKind::Element) {
            for a in idx.attr_range(e) {
                // The `xml` prefix is bound to the XML namespace by
                // definition (XML Names §3); accept either the resolved
                // namespace URI or the lexical `xml:space` name so the
                // check holds whether or not the source was parsed
                // namespace-aware.
                let is_xml_space = idx.local_name(a) == "space"
                    && (idx.namespace_uri(a) == XML_NAMESPACE
                        || idx.node_name(a) == "xml:space");
                if is_xml_space {
                    return Some(if idx.string_value(a).trim() == "preserve" {
                        XmlSpace::Preserve
                    } else {
                        XmlSpace::Default
                    });
                }
            }
        }
        cur = idx.parent(e);
    }
    None
}

/// Rule-only variant of [`should_strip`] for callers that already hold
/// the parent element's expanded name and have confirmed the text is
/// whitespace-only.  Used when grafting an externally-loaded document
/// (`document()` / `doc()` / `xsl:source-document`): the spec applies
/// `xsl:strip-space` to every source document, not just the principal
/// one (XSLT 3.0 §4.4), but the grafted nodes don't have index ids when
/// the strip decision must be made.
pub fn strips_whitespace_under(
    style: &StylesheetAst,
    parent_local: &str,
    parent_uri:   &str,
) -> bool {
    // XSLT 1.0 §3.4 conflict resolution, in order:
    //   1. Higher import precedence wins.
    //   2. Higher pattern specificity wins (`*` < `prefix:*` <
    //      exact-name).
    //   3. Later declaration order wins.
    // Iterating rules in declaration order and replacing the running
    // best on `>=` of (precedence, specificity) gives that behaviour:
    // a later rule with the same key as the current best overwrites
    // it (rule 3), while strictly-higher precedence or specificity
    // forces a fresh win.
    let mut best: Option<(i32, i32, bool)> = None; // (precedence, specificity, strip?)
    for rule in &style.whitespace_rules {
        let (q, prec, strip) = match rule {
            WhitespaceRule::Strip(q, p)    => (q, *p, true),
            WhitespaceRule::Preserve(q, p) => (q, *p, false),
        };
        let Some(spec) = match_specificity(q, parent_local, parent_uri) else { continue };
        let key = (prec, spec);
        if best.is_none_or(|(bp, bs, _)| key >= (bp, bs)) {
            best = Some((prec, spec, strip));
        }
    }
    best.is_some_and(|(_, _, strip)| strip)
}

fn match_specificity(rule: &QName, name: &str, uri: &str) -> Option<i32> {
    // `*` — matches any element.  Local name "*" with empty URI
    // is the bare wildcard.
    if rule.local == "*" && rule.uri.is_empty() && rule.prefix.is_none() {
        return Some(0);
    }
    // `prefix:*` / `Q{uri}*` — namespaced wildcard.
    if rule.local == "*" {
        return if rule.uri == uri { Some(1) } else { None };
    }
    // `*:NCName` — any namespace, specific local name (XSLT 2.0 §5.5.3).
    // Encoded by the compiler as prefix `*` with a concrete local name.
    if rule.prefix.as_deref() == Some("*") {
        return if rule.local == name { Some(1) } else { None };
    }
    // Exact local name; URIs must also match (both empty if no
    // namespace).
    if rule.local == name && rule.uri == uri { return Some(2); }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stylesheet;
    use sup_xml_core::{parse_str, ParseOptions, XPathContext};
    use sup_xml_core::xpath::eval::Value;

    fn parse_doc(xml: &str) -> sup_xml_tree::dom::Document {
        parse_str(xml, &ParseOptions::default()).unwrap()
    }

    fn first_text_in(idx: &sup_xml_core::xpath::DocIndex, parent: NodeId) -> NodeId {
        // First text-or-cdata child of `parent`.
        for &c in idx.children(parent) {
            if matches!(idx.kind(c), XPathNodeKind::Text | XPathNodeKind::CData) {
                return c;
            }
        }
        panic!("no text child");
    }

    fn ws_text(_doc: &sup_xml_tree::dom::Document, ctx: &XPathContext, query: &str) -> NodeId {
        let ns = match ctx.eval(query).unwrap() { Value::NodeSet(ns) => ns, _ => panic!() };
        first_text_in(&ctx.index, ns[0])
    }

    fn make_style(body: &str) -> Stylesheet {
        let full = format!(
            r#"<xsl:stylesheet version="1.0"
                xmlns:xsl="http://www.w3.org/1999/XSL/Transform">{body}</xsl:stylesheet>"#);
        Stylesheet::compile_str(&full).unwrap()
    }

    #[test]
    fn unmarked_element_preserves_whitespace() {
        let xslt = make_style("");
        let doc = parse_doc("<r>  </r>");
        let ctx = XPathContext::new(&doc);
        let t = ws_text(&doc, &ctx, "/r");
        assert!(!should_strip(&xslt.ast, t, &ctx.index));
    }

    #[test]
    fn strip_space_strips_marked_element() {
        let xslt = make_style(r#"<xsl:strip-space elements="r"/>"#);
        let doc = parse_doc("<r>  </r>");
        let ctx = XPathContext::new(&doc);
        let t = ws_text(&doc, &ctx, "/r");
        assert!(should_strip(&xslt.ast, t, &ctx.index));
    }

    #[test]
    fn strip_space_with_wildcard_strips_all() {
        let xslt = make_style(r#"<xsl:strip-space elements="*"/>"#);
        let doc = parse_doc("<r><a>  </a></r>");
        let ctx = XPathContext::new(&doc);
        let t = ws_text(&doc, &ctx, "/r/a");
        assert!(should_strip(&xslt.ast, t, &ctx.index));
    }

    #[test]
    fn non_whitespace_text_always_preserved() {
        let xslt = make_style(r#"<xsl:strip-space elements="*"/>"#);
        let doc = parse_doc("<r>hello</r>");
        let ctx = XPathContext::new(&doc);
        let t = ws_text(&doc, &ctx, "/r");
        assert!(!should_strip(&xslt.ast, t, &ctx.index));
    }

    #[test]
    fn preserve_space_beats_wildcard_strip() {
        let xslt = make_style(r#"
            <xsl:strip-space elements="*"/>
            <xsl:preserve-space elements="keep"/>
        "#);
        let doc = parse_doc("<r><keep>  </keep></r>");
        let ctx = XPathContext::new(&doc);
        let t = ws_text(&doc, &ctx, "/r/keep");
        assert!(!should_strip(&xslt.ast, t, &ctx.index));
    }

    #[test]
    fn xml_space_preserve_beats_wildcard_strip() {
        let xslt = make_style(r#"<xsl:strip-space elements="*"/>"#);
        let doc = parse_doc(r#"<r><k xml:space="preserve">  </k></r>"#);
        let ctx = XPathContext::new(&doc);
        let t = ws_text(&doc, &ctx, "/r/k");
        assert!(!should_strip(&xslt.ast, t, &ctx.index));
    }

    #[test]
    fn xml_space_preserve_inherited_from_ancestor() {
        let xslt = make_style(r#"<xsl:strip-space elements="*"/>"#);
        let doc = parse_doc(r#"<r xml:space="preserve"><k>  </k></r>"#);
        let ctx = XPathContext::new(&doc);
        let t = ws_text(&doc, &ctx, "/r/k");
        assert!(!should_strip(&xslt.ast, t, &ctx.index));
    }

    #[test]
    fn xml_space_default_under_preserve_ancestor_strips() {
        // The nearest ancestor with xml:space wins: a `default` child of
        // a `preserve` parent falls back to the strip-space rules.
        let xslt = make_style(r#"<xsl:strip-space elements="*"/>"#);
        let doc = parse_doc(
            r#"<r xml:space="preserve"><k xml:space="default">  </k></r>"#);
        let ctx = XPathContext::new(&doc);
        let t = ws_text(&doc, &ctx, "/r/k");
        assert!(should_strip(&xslt.ast, t, &ctx.index));
    }

    #[test]
    fn any_namespace_wildcard_preserve_beats_strip() {
        let xslt = make_style(r#"
            <xsl:strip-space elements="*"/>
            <xsl:preserve-space elements="*:keep"/>
        "#);
        let doc = parse_doc(
            r#"<r xmlns:n="urn:x"><n:keep>  </n:keep></r>"#);
        let ctx = XPathContext::new(&doc);
        let t = ws_text(&doc, &ctx, "/r/*[local-name()='keep']");
        assert!(!should_strip(&xslt.ast, t, &ctx.index));
    }
}
