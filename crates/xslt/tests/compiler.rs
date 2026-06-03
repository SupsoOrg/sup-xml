//! Compiler-AST tests — verify the structure produced for each
//! XSLT 1.0 instruction shape we support.  The evaluator (Stage
//! 2.3) consumes this AST, so getting the structure right here is
//! the contract for that work.

use sup_xml_xslt::ast::{Avt, AvtPart, Instr};
use sup_xml_xslt::Stylesheet;

fn compile(text: &str) -> Stylesheet {
    Stylesheet::compile_str(text).expect("stylesheet should compile")
}

fn must_compile_err(text: &str) -> sup_xml_xslt::XsltError {
    Stylesheet::compile_str(text).expect_err("stylesheet should fail to compile")
}

const HEAD: &str = r#"<xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform">"#;
const TAIL: &str = "</xsl:stylesheet>";

fn wrap(body: &str) -> String { format!("{HEAD}{body}{TAIL}") }

// ── basic top-level shapes ─────────────────────────────────────

#[test]
fn version_is_captured() {
    let xslt = compile(&wrap(""));
    assert_eq!(xslt.ast.version, "1.0");
}

#[test]
fn empty_stylesheet_compiles() {
    let xslt = compile(&wrap(""));
    assert!(xslt.ast.templates.is_empty());
}

#[test]
fn root_must_be_xsl_stylesheet_or_transform() {
    // Wrong root element entirely.
    let err = must_compile_err(r#"<not-xslt/>"#);
    assert!(format!("{err}").contains("XSLT namespace"),
        "got: {err}");
}

#[test]
fn xsl_transform_root_is_synonym() {
    let xslt = Stylesheet::compile_str(r#"<xsl:transform version="1.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"/>"#).unwrap();
    assert_eq!(xslt.ast.version, "1.0");
}

// ── templates ─────────────────────────────────────────────────

#[test]
fn template_with_match_compiles() {
    let xslt = compile(&wrap(r#"<xsl:template match="/"><out/></xsl:template>"#));
    assert_eq!(xslt.ast.templates.len(), 1);
    let t = &xslt.ast.templates[0];
    assert!(t.match_pattern.is_some());
    assert!(t.name.is_none());
    // Body has one literal element.
    assert_eq!(t.body.len(), 1);
    match &t.body[0] {
        Instr::LiteralElement { name, .. } => assert_eq!(name.local, "out"),
        other => panic!("expected LiteralElement, got {other:?}"),
    }
}

#[test]
fn template_with_name_compiles() {
    let xslt = compile(&wrap(r#"<xsl:template name="foo"/>"#));
    let t = &xslt.ast.templates[0];
    assert_eq!(t.name.as_ref().unwrap().local, "foo");
}

#[test]
fn template_without_match_or_name_errors() {
    let err = must_compile_err(&wrap("<xsl:template/>"));
    assert!(format!("{err}").contains("match= or name="), "got: {err}");
}

#[test]
fn template_priority_is_parsed_as_number() {
    let xslt = compile(&wrap(
        r#"<xsl:template match="*" priority="2.5"/>"#));
    assert_eq!(xslt.ast.templates[0].priority, Some(2.5));
}

// ── instructions ──────────────────────────────────────────────

#[test]
fn value_of_compiles_select() {
    let xslt = compile(&wrap(
        r#"<xsl:template match="/"><xsl:value-of select="."/></xsl:template>"#));
    match &xslt.ast.templates[0].body[0] {
        Instr::ValueOf { select: _, dose, .. } => assert!(!*dose),
        other => panic!("expected ValueOf, got {other:?}"),
    }
}

#[test]
fn apply_templates_with_select_and_mode() {
    let xslt = compile(&wrap(
        r#"<xsl:template match="/"><xsl:apply-templates select="*" mode="x"/></xsl:template>"#));
    match &xslt.ast.templates[0].body[0] {
        Instr::ApplyTemplates { select, mode, .. } => {
            assert!(select.is_some());
            assert_eq!(mode.as_ref().unwrap().local, "x");
        }
        other => panic!("expected ApplyTemplates, got {other:?}"),
    }
}

#[test]
fn if_compiles_test() {
    let xslt = compile(&wrap(
        r#"<xsl:template match="/"><xsl:if test="@x"><a/></xsl:if></xsl:template>"#));
    match &xslt.ast.templates[0].body[0] {
        Instr::If { test: _, body } => assert_eq!(body.len(), 1),
        other => panic!("expected If, got {other:?}"),
    }
}

#[test]
fn choose_requires_at_least_one_when() {
    let err = must_compile_err(&wrap(
        r#"<xsl:template match="/"><xsl:choose><xsl:otherwise/></xsl:choose></xsl:template>"#));
    assert!(format!("{err}").contains("xsl:when"), "got: {err}");
}

#[test]
fn choose_compiles_whens_and_otherwise() {
    let xslt = compile(&wrap(r#"<xsl:template match="/"><xsl:choose>
        <xsl:when test="@a"><a/></xsl:when>
        <xsl:when test="@b"><b/></xsl:when>
        <xsl:otherwise><c/></xsl:otherwise>
    </xsl:choose></xsl:template>"#));
    match &xslt.ast.templates[0].body[0] {
        Instr::Choose { whens, otherwise } => {
            assert_eq!(whens.len(), 2);
            assert!(otherwise.is_some());
        }
        other => panic!("expected Choose, got {other:?}"),
    }
}

#[test]
fn for_each_with_sort_separates_them() {
    let xslt = compile(&wrap(r#"<xsl:template match="/">
        <xsl:for-each select="*">
            <xsl:sort select="@n" order="descending"/>
            <out/>
        </xsl:for-each>
    </xsl:template>"#));
    match &xslt.ast.templates[0].body[0] {
        Instr::ForEach { sort, body, .. } => {
            assert_eq!(sort.len(), 1);
            // Body excludes the xsl:sort.
            assert!(body.iter().any(|i| matches!(i, Instr::LiteralElement{..})));
        }
        other => panic!("expected ForEach, got {other:?}"),
    }
}

#[test]
fn call_template_collects_with_params() {
    let xslt = compile(&wrap(r#"<xsl:template match="/">
        <xsl:call-template name="t">
            <xsl:with-param name="a" select="1"/>
            <xsl:with-param name="b" select="2"/>
        </xsl:call-template>
    </xsl:template>"#));
    match &xslt.ast.templates[0].body[0] {
        Instr::CallTemplate { name, with_params } => {
            assert_eq!(name.local, "t");
            assert_eq!(with_params.len(), 2);
        }
        other => panic!("expected CallTemplate, got {other:?}"),
    }
}

#[test]
fn variable_either_select_or_body() {
    let xslt = compile(&wrap(r#"<xsl:template match="/">
        <xsl:variable name="x" select="1"/>
        <xsl:variable name="y"><inner/></xsl:variable>
    </xsl:template>"#));
    let body = &xslt.ast.templates[0].body;
    assert!(matches!(&body[0], Instr::Variable(v)
        if v.name.local == "x" && v.select.is_some()));
    assert!(matches!(&body[1], Instr::Variable(v)
        if v.name.local == "y" && v.select.is_none() && !v.body.is_empty()));
}

#[test]
fn text_with_dose_flag() {
    let xslt = compile(&wrap(r#"<xsl:template match="/">
        <xsl:text disable-output-escaping="yes">&lt;raw&gt;</xsl:text>
    </xsl:template>"#));
    match &xslt.ast.templates[0].body[0] {
        Instr::LiteralText { dose, .. } => assert!(*dose),
        other => panic!("expected LiteralText, got {other:?}"),
    }
}

// ── AVT compilation ────────────────────────────────────────────

#[test]
fn literal_element_with_avt_attribute() {
    let xslt = compile(&wrap(r#"<xsl:template match="/">
        <out class="row-{position()} item"/>
    </xsl:template>"#));
    let elt = match &xslt.ast.templates[0].body[0] {
        Instr::LiteralElement { attributes, .. } => &attributes[0],
        other => panic!("expected LiteralElement, got {other:?}"),
    };
    let avt: &Avt = &elt.1;
    // 3 parts: "row-" literal, expr, " item" literal.
    assert_eq!(avt.parts.len(), 3);
    assert!(matches!(&avt.parts[0], AvtPart::Literal(s) if s == "row-"));
    assert!(matches!(&avt.parts[1], AvtPart::Expr(_)));
    assert!(matches!(&avt.parts[2], AvtPart::Literal(s) if s == " item"));
    assert!(!avt.is_literal());
}

#[test]
fn literal_only_avt_is_marked_literal() {
    let xslt = compile(&wrap(r#"<xsl:template match="/">
        <out class="static-class"/>
    </xsl:template>"#));
    let avt = match &xslt.ast.templates[0].body[0] {
        Instr::LiteralElement { attributes, .. } => &attributes[0].1,
        _ => panic!(),
    };
    assert!(avt.is_literal());
    assert_eq!(avt.parts.len(), 1);
}

#[test]
fn avt_doubled_braces_are_literal() {
    let xslt = compile(&wrap(r#"<xsl:template match="/">
        <out class="{{not-an-expr}}"/>
    </xsl:template>"#));
    let avt = match &xslt.ast.templates[0].body[0] {
        Instr::LiteralElement { attributes, .. } => &attributes[0].1,
        _ => panic!(),
    };
    // Doubled braces decode to single — the AVT is "{not-an-expr}"
    // as a literal, no expression parts.
    assert!(avt.is_literal());
    let s = match &avt.parts[0] {
        AvtPart::Literal(s) => s.as_str(),
        _ => panic!(),
    };
    assert_eq!(s, "{not-an-expr}");
}

#[test]
fn avt_handles_string_with_close_brace_inside_quotes() {
    // The `}` inside the string literal must NOT terminate the AVT
    // expression.
    let xslt = compile(&wrap(r#"<xsl:template match="/">
        <out tag="{concat('a','}','b')}"/>
    </xsl:template>"#));
    let avt = match &xslt.ast.templates[0].body[0] {
        Instr::LiteralElement { attributes, .. } => &attributes[0].1,
        _ => panic!(),
    };
    // Single expression part — concat('a','}','b').
    assert_eq!(avt.parts.len(), 1);
    assert!(matches!(&avt.parts[0], AvtPart::Expr(_)));
}

// ── top-level structural ──────────────────────────────────────

#[test]
fn global_variable_compiles() {
    let xslt = compile(&wrap(r#"<xsl:variable name="answer" select="42"/>"#));
    assert_eq!(xslt.ast.global_variables.len(), 1);
    assert_eq!(xslt.ast.global_variables[0].name.local, "answer");
}

#[test]
fn output_method_captured() {
    let xslt = compile(&wrap(r#"<xsl:output method="html" indent="yes"/>"#));
    assert_eq!(xslt.ast.outputs.len(), 1);
    assert_eq!(xslt.ast.outputs[0].method.as_deref(), Some("html"));
    assert_eq!(xslt.ast.outputs[0].indent, Some(true));
}

#[test]
fn key_captures_match_and_use() {
    let xslt = compile(&wrap(
        r#"<xsl:key name="byid" match="*" use="@id"/>"#));
    assert_eq!(xslt.ast.keys.len(), 1);
    assert_eq!(xslt.ast.keys[0].name.local, "byid");
}

#[test]
fn strip_space_and_preserve_space_record_in_order() {
    let xslt = compile(&wrap(r#"
        <xsl:strip-space elements="a b"/>
        <xsl:preserve-space elements="c"/>
    "#));
    // 2 from strip-space + 1 from preserve-space = 3.
    assert_eq!(xslt.ast.whitespace_rules.len(), 3);
}

// ── XPath errors surface from compilation ──────────────────────

#[test]
fn malformed_xpath_in_select_errors_at_compile_time() {
    let err = must_compile_err(&wrap(
        r#"<xsl:template match="/"><xsl:value-of select="@@"/></xsl:template>"#));
    let msg = format!("{err}");
    assert!(
        msg.contains("xpath") || msg.contains("XPath") || msg.contains("expected"),
        "expected xpath error message, got: {msg}",
    );
}

#[test]
fn malformed_xpath_in_match_errors_at_compile_time() {
    let err = must_compile_err(&wrap(
        r#"<xsl:template match="@@"/>"#));
    let msg = format!("{err}");
    assert!(msg.contains("xpath") || msg.contains("XPath") || msg.contains("expected"),
        "got: {msg}");
}
