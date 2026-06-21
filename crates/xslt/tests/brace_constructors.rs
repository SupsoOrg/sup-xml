//! XPath 3.1 brace-delimited constructors (`map { … }`, `array { … }`,
//! square arrays `[ … ]`, and inline functions `function(…) { … }`) must
//! compile when used in a `version="3.0"` stylesheet's expressions.
//!
//! These guard against the XSLT compiler accidentally parsing a 2.0+
//! stylesheet's XPath in 1.0 grammar mode, which would reject the `{` /
//! `[` with an "unexpected LBrace/LBracket" error (the W3C streaming
//! suite leans on these constructors heavily — `map { … }` in
//! `xsl:for-each-group` keys, `array { … }`, `fold-left(…, function…)`).

use sup_xml_xslt::Stylesheet;

/// Wrap an XPath expression in a minimal `version="3.0"` stylesheet that
/// uses it in a `select=`, and return the compile result.
fn compiles(select: &str) -> Result<(), String> {
    let xsl = format!(
        r#"<xsl:stylesheet version="3.0"
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
            xmlns:map="http://www.w3.org/2005/xpath-functions/map"
            xmlns:array="http://www.w3.org/2005/xpath-functions/array">
          <xsl:template match="/"><xsl:sequence select="{select}"/></xsl:template>
        </xsl:stylesheet>"#
    );
    Stylesheet::compile_str(&xsl).map(|_| ()).map_err(|e| e.to_string())
}

#[test]
fn map_constructor_compiles() {
    for select in [
        "map{}",
        "map { 1 : 'a', 2 : 'b' }",
        "map{'outcome':'success'}",
        "map { 'a' : map { 'b' : 1 } }",
    ] {
        compiles(select).unwrap_or_else(|e| panic!("select={select:?}: {e}"));
    }
}

#[test]
fn array_constructors_compile() {
    for select in ["array {}", "array { 1, 2 }", "[ 1, 2, 3 ]", "[ ]"] {
        compiles(select).unwrap_or_else(|e| panic!("select={select:?}: {e}"));
    }
}

#[test]
fn inline_function_compiles() {
    for select in [
        "fold-left((1, 2, 3), 0, function($a, $b) { $a + $b })",
        "let $f := function($x) { $x * 2 } return $f(21)",
    ] {
        compiles(select).unwrap_or_else(|e| panic!("select={select:?}: {e}"));
    }
}
