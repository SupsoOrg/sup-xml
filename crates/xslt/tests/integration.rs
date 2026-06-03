//! Smoke tests at the crate boundary — compile a stylesheet, then
//! poke its public surface.  End-to-end transformation tests live
//! in `eval.rs`.

use sup_xml_core::{parse_str, ParseOptions};
use sup_xml_xslt::{Stylesheet, XSLT_NS};

const TRIVIAL_STYLESHEET: &str = r#"
<xsl:stylesheet version="1.0"
                xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
    <xsl:template match="/">
        <out/>
    </xsl:template>
</xsl:stylesheet>
"#;

#[test]
fn xslt_namespace_constant_is_canonical() {
    assert_eq!(XSLT_NS, "http://www.w3.org/1999/XSL/Transform");
}

#[test]
fn compile_trivial_stylesheet_succeeds() {
    let xslt = Stylesheet::compile_str(TRIVIAL_STYLESHEET)
        .expect("compile should succeed");
    assert_eq!(xslt.ast.templates.len(), 1);
    assert!(xslt.ast.templates[0].match_pattern.is_some(),
        "the template has match='/'");
    assert_eq!(xslt.ast.version, "1.0");
}

#[test]
fn compile_with_pre_parsed_doc() {
    // The non-convenience entry point requires the caller to set
    // namespace_aware themselves.
    let opts = ParseOptions { namespace_aware: true, ..ParseOptions::default() };
    let doc = parse_str(TRIVIAL_STYLESHEET, &opts).unwrap();
    let xslt = Stylesheet::compile(&doc).expect("compile should succeed");
    assert_eq!(xslt.ast.templates.len(), 1);
}

#[test]
fn apply_trivial_stylesheet_produces_root_output_element() {
    // The trivial stylesheet matches "/" and emits <out/>.  Eval
    // should produce a result tree with that single element at top
    // level.
    let xslt = Stylesheet::compile_str(TRIVIAL_STYLESHEET).unwrap();
    let source_doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    let result = xslt.apply(&source_doc).expect("apply should succeed");
    // The result tree's `children` are top-level result nodes.
    // We expect a single Element named "out", plus possibly some
    // surrounding whitespace text nodes from the stylesheet source.
    let elements: Vec<_> = result.children.iter().filter(|n|
        matches!(n, sup_xml_xslt::result_tree::ResultNode::Element { .. })
    ).collect();
    assert_eq!(elements.len(), 1);
    if let sup_xml_xslt::result_tree::ResultNode::Element { name, .. } = elements[0] {
        assert_eq!(name.local, "out");
    }
}
