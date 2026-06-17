//! Evaluator integration tests — feed source XML through compiled
//! stylesheets and verify the produced result tree's structure
//! and content.
//!
//! Tests inspect the in-memory tree directly via
//! [`ResultTree::children`] so any divergence between the
//! evaluator and the output serialiser is caught at this layer first.

use sup_xml_core::{parse_str, ParseOptions};
use sup_xml_xslt::loader::InMemoryLoader;
use sup_xml_xslt::result_tree::ResultNode;
use sup_xml_xslt::Stylesheet;

const HEAD: &str = r#"<xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform">"#;
const TAIL: &str = "</xsl:stylesheet>";

fn run(stylesheet_body: &str, source: &str) -> Vec<ResultNode> {
    let full = format!("{HEAD}{stylesheet_body}{TAIL}");
    let xslt = Stylesheet::compile_str(&full).expect("stylesheet compile");
    let doc  = parse_str(source, &ParseOptions::default()).expect("source parse");
    let result = xslt.apply(&doc).expect("apply");
    result.children
}

/// Convenience — find the first Element in `nodes` (skipping
/// any whitespace text from indentation).
fn first_element(nodes: &[ResultNode]) -> &ResultNode {
    nodes.iter().find(|n| matches!(n, ResultNode::Element { .. }))
        .expect("expected at least one Element")
}

/// Recursive text extraction — collect all text-node content
/// within `node`'s subtree (or the top-level if a slice).
fn text_of(node: &ResultNode) -> String {
    let mut s = String::new();
    collect_text(node, &mut s);
    s
}

fn collect_text(node: &ResultNode, out: &mut String) {
    match node {
        ResultNode::Text { content, .. } => out.push_str(content),
        ResultNode::Element { children, .. } => {
            for c in children { collect_text(c, out); }
        }
        _ => {}
    }
}

// ── trivial literal output ───────────────────────────────────────

#[test]
fn literal_element_in_template_emits_into_result() {
    let nodes = run(r#"<xsl:template match="/"><out/></xsl:template>"#, "<r/>");
    match first_element(&nodes) {
        ResultNode::Element { name, .. } => assert_eq!(name.local, "out"),
        _ => panic!(),
    }
}

#[test]
fn nested_literals_preserve_structure() {
    let nodes = run(
        r#"<xsl:template match="/"><a><b><c/></b></a></xsl:template>"#,
        "<r/>",
    );
    let a = first_element(&nodes);
    match a {
        ResultNode::Element { name, children, .. } => {
            assert_eq!(name.local, "a");
            let b = children.iter().find(|n| matches!(n, ResultNode::Element { .. })).unwrap();
            if let ResultNode::Element { name, children, .. } = b {
                assert_eq!(name.local, "b");
                let c = children.iter().find(|n| matches!(n, ResultNode::Element { .. })).unwrap();
                if let ResultNode::Element { name, .. } = c {
                    assert_eq!(name.local, "c");
                }
            }
        }
        _ => panic!(),
    }
}

// ── value-of pulls text from source ─────────────────────────────

#[test]
fn value_of_selects_text_from_source() {
    let nodes = run(
        r#"<xsl:template match="/"><out><xsl:value-of select="/r/title"/></out></xsl:template>"#,
        "<r><title>Hello</title></r>",
    );
    let out = first_element(&nodes);
    assert_eq!(text_of(out), "Hello");
}

#[test]
fn value_of_handles_attribute_source() {
    let nodes = run(
        r#"<xsl:template match="/"><out><xsl:value-of select="/r/@id"/></out></xsl:template>"#,
        r#"<r id="42"/>"#,
    );
    let out = first_element(&nodes);
    assert_eq!(text_of(out), "42");
}

// ── apply-templates + built-in rules ────────────────────────────

#[test]
fn apply_templates_without_select_recurses_via_builtin_text_rule() {
    // No user template for <r> or its <title> child → built-in
    // template for element applies-templates to children → built-in
    // template for text copies the value.
    let nodes = run(r#"<xsl:template match="/"><out><xsl:apply-templates/></out></xsl:template>"#,
        "<r><title>Hello</title></r>");
    let out = first_element(&nodes);
    assert_eq!(text_of(out), "Hello");
}

#[test]
fn matched_template_overrides_builtin() {
    let nodes = run(r#"
        <xsl:template match="/"><out><xsl:apply-templates/></out></xsl:template>
        <xsl:template match="title">TITLE</xsl:template>
    "#, "<r><title>ignored</title></r>");
    let out = first_element(&nodes);
    assert_eq!(text_of(out), "TITLE");
}

// ── if / choose ────────────────────────────────────────────────

#[test]
fn if_true_emits_body() {
    let nodes = run(r#"<xsl:template match="/">
        <xsl:if test="/r/@active='yes'"><active/></xsl:if>
    </xsl:template>"#, r#"<r active="yes"/>"#);
    let active = first_element(&nodes);
    if let ResultNode::Element { name, .. } = active {
        assert_eq!(name.local, "active");
    }
}

#[test]
fn if_false_emits_nothing() {
    let nodes = run(r#"<xsl:template match="/">
        <out><xsl:if test="/r/@active='yes'"><inner/></xsl:if></out>
    </xsl:template>"#, r#"<r active="no"/>"#);
    let out = first_element(&nodes);
    if let ResultNode::Element { children, .. } = out {
        // No <inner/> element, only possible whitespace text.
        assert!(!children.iter().any(|n|
            matches!(n, ResultNode::Element { name, .. } if name.local == "inner")));
    }
}

#[test]
fn choose_picks_first_matching_when() {
    let nodes = run(r#"<xsl:template match="/">
        <xsl:choose>
            <xsl:when test="/r/@n=1"><one/></xsl:when>
            <xsl:when test="/r/@n=2"><two/></xsl:when>
            <xsl:otherwise><other/></xsl:otherwise>
        </xsl:choose>
    </xsl:template>"#, r#"<r n="2"/>"#);
    let e = first_element(&nodes);
    if let ResultNode::Element { name, .. } = e {
        assert_eq!(name.local, "two");
    }
}

#[test]
fn choose_otherwise_when_no_when_matches() {
    let nodes = run(r#"<xsl:template match="/">
        <xsl:choose>
            <xsl:when test="false()"><wrong/></xsl:when>
            <xsl:otherwise><right/></xsl:otherwise>
        </xsl:choose>
    </xsl:template>"#, "<r/>");
    let e = first_element(&nodes);
    if let ResultNode::Element { name, .. } = e {
        assert_eq!(name.local, "right");
    }
}

// ── for-each ──────────────────────────────────────────────────

#[test]
fn for_each_iterates_in_document_order() {
    let nodes = run(r#"<xsl:template match="/">
        <list>
            <xsl:for-each select="/r/i">
                <item><xsl:value-of select="."/></item>
            </xsl:for-each>
        </list>
    </xsl:template>"#, "<r><i>a</i><i>b</i><i>c</i></r>");
    let list = first_element(&nodes);
    if let ResultNode::Element { children, .. } = list {
        let items: Vec<&ResultNode> = children.iter()
            .filter(|n| matches!(n, ResultNode::Element { .. }))
            .collect();
        assert_eq!(items.len(), 3);
        assert_eq!(text_of(items[0]), "a");
        assert_eq!(text_of(items[1]), "b");
        assert_eq!(text_of(items[2]), "c");
    }
}

#[test]
fn for_each_exposes_position_function() {
    let nodes = run(r#"<xsl:template match="/">
        <out>
            <xsl:for-each select="/r/i">
                <item pos="{position()}"><xsl:value-of select="."/></item>
            </xsl:for-each>
        </out>
    </xsl:template>"#, "<r><i>a</i><i>b</i></r>");
    let out = first_element(&nodes);
    if let ResultNode::Element { children, .. } = out {
        let items: Vec<&ResultNode> = children.iter()
            .filter(|n| matches!(n, ResultNode::Element { .. }))
            .collect();
        if let ResultNode::Element { attributes, .. } = items[0] {
            assert_eq!(attributes[0].1, "1");
        }
        if let ResultNode::Element { attributes, .. } = items[1] {
            assert_eq!(attributes[0].1, "2");
        }
    }
}

// ── variables / call-template ──────────────────────────────────

#[test]
fn variable_referenced_by_xpath() {
    let nodes = run(r#"<xsl:template match="/">
        <out>
            <xsl:variable name="greeting" select="'Hello'"/>
            <xsl:value-of select="$greeting"/>
        </out>
    </xsl:template>"#, "<r/>");
    let out = first_element(&nodes);
    assert_eq!(text_of(out), "Hello");
}

#[test]
fn call_template_passes_parameters() {
    let nodes = run(r#"
        <xsl:template name="add">
            <xsl:param name="a"/>
            <xsl:param name="b"/>
            <xsl:value-of select="$a + $b"/>
        </xsl:template>
        <xsl:template match="/">
            <out>
                <xsl:call-template name="add">
                    <xsl:with-param name="a" select="3"/>
                    <xsl:with-param name="b" select="4"/>
                </xsl:call-template>
            </out>
        </xsl:template>
    "#, "<r/>");
    let out = first_element(&nodes);
    assert_eq!(text_of(out), "7");
}

#[test]
fn unknown_call_template_errors() {
    let xslt = Stylesheet::compile_str(&format!(
        "{HEAD}<xsl:template match=\"/\"><xsl:call-template name=\"missing\"/></xsl:template>{TAIL}",
    )).unwrap();
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    let err = xslt.apply(&doc).unwrap_err();
    assert!(format!("{err}").contains("missing"),
        "expected unresolved-reference error, got: {err}");
}

// ── copy / copy-of ─────────────────────────────────────────────

#[test]
fn copy_of_deep_copies_nodeset() {
    let nodes = run(r#"<xsl:template match="/">
        <wrapper><xsl:copy-of select="/r/inner"/></wrapper>
    </xsl:template>"#, "<r><inner><deep>text</deep></inner></r>");
    let wrap = first_element(&nodes);
    if let ResultNode::Element { children, .. } = wrap {
        let inner = children.iter()
            .find(|n| matches!(n, ResultNode::Element { name, .. } if name.local == "inner"));
        assert!(inner.is_some());
        assert_eq!(text_of(inner.unwrap()), "text");
    }
}

#[test]
fn copy_emits_shallow_for_element() {
    // xsl:copy makes a shallow copy of the current node (no
    // attributes, no descendants — those need explicit
    // apply-templates inside).
    let nodes = run(r#"<xsl:template match="/r"><xsl:copy>inside</xsl:copy></xsl:template>"#,
        r#"<r foo="bar"><child/></r>"#);
    let r = first_element(&nodes);
    if let ResultNode::Element { name, attributes, .. } = r {
        assert_eq!(name.local, "r");
        // attributes NOT copied by xsl:copy alone.
        assert!(attributes.is_empty(),
            "xsl:copy is shallow; attributes shouldn't appear automatically");
        assert_eq!(text_of(r), "inside");
    }
}

// ── EXSLT through XSLT eval ─────────────────────────────────────

#[test]
fn exslt_math_max_inside_value_of() {
    // Confirms the EXSLT-engine + XSLT-evaluator wiring: math:max
    // on a select= nodeset should compute fine without any extra
    // setup (EXSLT URIs are auto-bound in the namespace context).
    let nodes = run(r#"<xsl:template match="/">
        <out><xsl:value-of select="math:max(/r/i)"/></out>
    </xsl:template>"#, "<r><i>3</i><i>7</i><i>1</i></r>");
    let out = first_element(&nodes);
    assert_eq!(text_of(out), "7");
}

#[test]
fn exslt_str_padding_via_xslt() {
    let nodes = run(r#"<xsl:template match="/">
        <out><xsl:value-of select="str:padding(5, '*')"/></out>
    </xsl:template>"#, "<r/>");
    let out = first_element(&nodes);
    assert_eq!(text_of(out), "*****");
}

// ── error surfacing ─────────────────────────────────────────────

#[test]
fn xsl_message_terminate_errors() {
    let xslt = Stylesheet::compile_str(&format!(
        r#"{HEAD}<xsl:template match="/">
            <xsl:message terminate="yes">bail</xsl:message>
        </xsl:template>{TAIL}"#,
    )).unwrap();
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    let err = xslt.apply(&doc).unwrap_err();
    match err {
        sup_xml_xslt::XsltError::Terminated(msg) => assert!(msg.contains("bail")),
        other => panic!("expected Terminated, got {other:?}"),
    }
}

// ── document() function ─────────────────────────────────────────

#[test]
fn document_loads_external_doc_via_loader() {
    let loader = InMemoryLoader::new()
        .with("ext.xml", "<config><value>42</value></config>");
    let xslt = Stylesheet::compile_str(&format!(
        r#"{HEAD}<xsl:template match="/">
            <out><xsl:value-of select="document('ext.xml')/config/value"/></out>
        </xsl:template>{TAIL}"#,
    )).unwrap();
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    let result = xslt.apply_with_loader(&doc, &loader, None).expect("apply_with_loader");
    let out = first_element(&result.children);
    assert_eq!(text_of(out), "42");
}

#[test]
fn document_apply_without_loader_errors_when_used() {
    // The stylesheet references document('foo.xml'); without a Loader,
    // pre-loading fails at apply time.
    let xslt = Stylesheet::compile_str(&format!(
        r#"{HEAD}<xsl:template match="/">
            <out><xsl:value-of select="document('foo.xml')/a"/></out>
        </xsl:template>{TAIL}"#,
    )).unwrap();
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    assert!(xslt.apply(&doc).is_err());
}

#[test]
fn document_apply_without_document_calls_works_with_null_loader() {
    // No document() in stylesheet → apply() still works fine.
    let xslt = Stylesheet::compile_str(&format!(
        r#"{HEAD}<xsl:template match="/r"><out>ok</out></xsl:template>{TAIL}"#,
    )).unwrap();
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    let result = xslt.apply(&doc).expect("apply without document()");
    let out = first_element(&result.children);
    assert_eq!(text_of(out), "ok");
}

#[test]
fn document_iterates_loaded_doc() {
    let loader = InMemoryLoader::new()
        .with("items.xml", "<items><i>a</i><i>b</i><i>c</i></items>");
    let xslt = Stylesheet::compile_str(&format!(
        r#"{HEAD}<xsl:template match="/">
            <out><xsl:for-each select="document('items.xml')/items/i"><xsl:value-of select="."/>;</xsl:for-each></out>
        </xsl:template>{TAIL}"#,
    )).unwrap();
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    let result = xslt.apply_with_loader(&doc, &loader, None).expect("apply_with_loader");
    let out = first_element(&result.children);
    assert_eq!(text_of(out), "a;b;c;");
}

#[test]
fn document_caches_repeated_loads_at_compile_time() {
    // Two `document('shared.xml')` calls; pre-load should happen once
    // (URIs are de-duplicated in documents_to_load).  We verify by
    // using an InMemoryLoader whose .load() would error if asked
    // twice — but InMemoryLoader handles repeats fine, so instead we
    // just verify the two calls produce identical node-sets.
    let loader = InMemoryLoader::new()
        .with("shared.xml", "<r><v>x</v></r>");
    let xslt = Stylesheet::compile_str(&format!(
        r#"{HEAD}<xsl:template match="/">
            <out>
                <a><xsl:value-of select="document('shared.xml')/r/v"/></a>
                <b><xsl:value-of select="document('shared.xml')/r/v"/></b>
            </out>
        </xsl:template>{TAIL}"#,
    )).unwrap();
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    let result = xslt.apply_with_loader(&doc, &loader, None).expect("apply_with_loader");
    // Spot-check that documents_to_load deduped to a single URI.
    assert_eq!(xslt.ast.documents_to_load, vec!["shared.xml".to_string()]);
    let out = first_element(&result.children);
    let txt = text_of(out);
    assert!(txt.contains("x"));
}

#[test]
fn document_dynamic_uri_errors_clearly() {
    // document(@href) is dynamic — the URI doesn't appear as a
    // string literal in the stylesheet, so the compile-time scanner
    // never pre-loads it.  At apply time the dispatcher rejects the
    // URI with a clear "not pre-loaded" diagnostic that carries the
    // actual URI value and points at the dynamic-URI limitation.
    let loader = InMemoryLoader::new();
    let xslt = Stylesheet::compile_str(&format!(
        r#"{HEAD}<xsl:template match="/r">
            <xsl:value-of select="document(@href)/x"/>
        </xsl:template>{TAIL}"#,
    )).unwrap();
    let doc = parse_str(r#"<r href="something.xml"/>"#, &ParseOptions::default()).unwrap();
    let err = xslt.apply_with_loader(&doc, &loader, None).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("document('something.xml')") && msg.contains("not pre-loaded"),
            "expected not-pre-loaded diagnostic carrying the URI, got: {msg}");
    assert!(msg.contains("runtime loader") || msg.contains("string literal"),
            "diagnostic should point at the dynamic-URI limitation, got: {msg}");
}

// ── xsl:number focus inside xsl:function (XSLT 2.0 §10.3 / §13.1) ──

/// Inside an `xsl:function` the focus is undefined, but an
/// `xsl:for-each` over a node sequence establishes a new node focus —
/// so a contained `xsl:number` counts the iterated node rather than
/// raising XTTE0990.  Regression test for the SchXslt `redux:location`
/// pattern.
#[test]
fn xsl_number_in_function_for_each_over_nodes() {
    let sheet = r#"<xsl:transform version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:xs="http://www.w3.org/2001/XMLSchema"
        xmlns:my="urn:test">
      <xsl:function name="my:pos" as="xs:integer">
        <xsl:param name="context" as="node()"/>
        <xsl:variable name="n" as="xs:integer">
          <xsl:for-each select="$context"><xsl:number/></xsl:for-each>
        </xsl:variable>
        <xsl:sequence select="$n"/>
      </xsl:function>
      <xsl:template match="/">
        <out><xsl:value-of select="my:pos(/root/foo[2])"/></out>
      </xsl:template>
    </xsl:transform>"#;
    let xslt = Stylesheet::compile_str(sheet).expect("compile");
    let doc = parse_str("<root><foo/><foo/><foo/></root>", &ParseOptions::default()).unwrap();
    let result = xslt.apply(&doc).expect("apply");
    // <xsl:number/> on the second <foo> counts among same-name
    // siblings → 2.
    assert_eq!(text_of(first_element(&result.children)), "2");
}

/// An `xsl:for-each` over an *atomic* sequence inside a function does
/// not establish a node focus, so `xsl:number` with no `select=` must
/// still raise XTTE0990.
#[test]
fn xsl_number_in_function_for_each_over_atomics_errors() {
    let sheet = r#"<xsl:transform version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:xs="http://www.w3.org/2001/XMLSchema"
        xmlns:my="urn:test">
      <xsl:function name="my:f" as="xs:string">
        <xsl:value-of>
          <xsl:for-each select="1 to 3"><xsl:number/></xsl:for-each>
        </xsl:value-of>
      </xsl:function>
      <xsl:template match="/"><out><xsl:value-of select="my:f()"/></out></xsl:template>
    </xsl:transform>"#;
    let xslt = Stylesheet::compile_str(sheet).expect("compile");
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    let err = xslt.apply(&doc).unwrap_err();
    assert!(format!("{err}").contains("XTTE0990"),
        "expected XTTE0990 for atomic for-each, got: {err}");
}

/// A runtime XSLT error carries the failing instruction's source
/// position — file (when known), 1-based line/column, and the
/// ground-truth byte offset it was derived from.
#[test]
fn xsl_number_error_carries_source_position() {
    // `<xsl:number/>` sits on line 6; its name starts at column 50.
    let sheet = "\
<xsl:transform version=\"2.0\"\n\
\x20 xmlns:xsl=\"http://www.w3.org/1999/XSL/Transform\"\n\
\x20 xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"\n\
\x20 xmlns:my=\"urn:test\">\n\
\x20 <xsl:function name=\"my:f\" as=\"xs:string\">\n\
\x20   <xsl:value-of><xsl:for-each select=\"1 to 3\"><xsl:number/></xsl:for-each></xsl:value-of>\n\
\x20 </xsl:function>\n\
\x20 <xsl:template match=\"/\"><out><xsl:value-of select=\"my:f()\"/></out></xsl:template>\n\
</xsl:transform>";
    let xslt = Stylesheet::compile_str(sheet).expect("compile");
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    match xslt.apply(&doc).unwrap_err() {
        sup_xml_xslt::XsltError::Xpath(e) => {
            assert!(e.message.contains("XTTE0990"), "msg: {}", e.message);
            assert_eq!(e.line, Some(6), "line");
            assert_eq!(e.column, Some(50), "column");
            assert!(e.byte_offset.is_some(), "byte offset set");
        }
        other => panic!("expected positioned Xpath error, got: {other:?}"),
    }
}

/// General coverage: an XPath error raised inside a template
/// instruction's `select=` is attributed to that instruction's source
/// line/column — not just xsl:number.  Here an invalid xs:integer cast
/// fails at runtime.
#[test]
fn xpath_error_in_select_carries_source_position() {
    // `<xsl:value-of>` is on line 5; its name starts at column 7.
    let sheet = "\
<xsl:stylesheet version=\"2.0\"\n\
\x20 xmlns:xsl=\"http://www.w3.org/1999/XSL/Transform\"\n\
\x20 xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">\n\
\x20 <xsl:template match=\"/\">\n\
\x20     <xsl:value-of select=\"xs:integer('nope')\"/>\n\
\x20 </xsl:template>\n\
</xsl:stylesheet>";
    let xslt = Stylesheet::compile_str(sheet).expect("compile");
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    match xslt.apply(&doc).unwrap_err() {
        sup_xml_xslt::XsltError::Xpath(e) => {
            assert_eq!(e.line, Some(5), "line");
            assert_eq!(e.column, Some(8), "column");
            assert!(e.byte_offset.is_some(), "byte offset set");
        }
        other => panic!("expected positioned Xpath error, got: {other:?}"),
    }
}

/// General coverage: an `as=` type-coercion failure on `xsl:variable`
/// is attributed to the variable's source position.
#[test]
fn type_coercion_error_carries_source_position() {
    // `<xsl:variable>` is on line 5; its name starts at column 7.
    let sheet = "\
<xsl:stylesheet version=\"2.0\"\n\
\x20 xmlns:xsl=\"http://www.w3.org/1999/XSL/Transform\"\n\
\x20 xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">\n\
\x20 <xsl:template match=\"/\">\n\
\x20     <xsl:variable name=\"n\" as=\"xs:integer\" select=\"'banana'\"/>\n\
\x20     <out><xsl:value-of select=\"$n\"/></out>\n\
\x20 </xsl:template>\n\
</xsl:stylesheet>";
    let xslt = Stylesheet::compile_str(sheet).expect("compile");
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    match xslt.apply(&doc).unwrap_err() {
        sup_xml_xslt::XsltError::Xpath(e) => {
            assert_eq!(e.line, Some(5), "line: {e:?}");
            assert!(e.column.is_some() && e.byte_offset.is_some(), "col+offset set: {e:?}");
        }
        other => panic!("expected positioned Xpath error, got: {other:?}"),
    }
}

/// A runtime error raised inside an `xsl:function` whose body is lowered
/// to a single `xsl:sequence` (the desugaring path) is still attributed
/// to source — the lowered body carries the originating line/column
/// rather than dropping to an unpositioned error.
#[test]
fn desugared_function_body_error_carries_source_position() {
    // The `<xsl:function>` and its `<xsl:sequence>` both sit on line 5,
    // so the assertion holds whether the position is attributed to the
    // function element (desugar fires) or the sequence (it doesn't).
    let sheet = "\
<xsl:stylesheet version=\"2.0\"\n\
\x20 xmlns:xsl=\"http://www.w3.org/1999/XSL/Transform\"\n\
\x20 xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"\n\
\x20 xmlns:my=\"urn:test\">\n\
\x20 <xsl:function name=\"my:f\" as=\"xs:integer\"><xsl:sequence select=\"xs:integer('nope')\"/></xsl:function>\n\
\x20 <xsl:template match=\"/\"><out><xsl:value-of select=\"my:f()\"/></out></xsl:template>\n\
</xsl:stylesheet>";
    let xslt = Stylesheet::compile_str(sheet).expect("compile");
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    match xslt.apply(&doc).unwrap_err() {
        sup_xml_xslt::XsltError::Xpath(e) => {
            assert_eq!(e.line, Some(5), "line: {e:?}");
            assert!(e.column.is_some() && e.byte_offset.is_some(), "col+offset set: {e:?}");
        }
        other => panic!("expected positioned Xpath error, got: {other:?}"),
    }
}

/// Each instruction in a sequence constructor keeps its *own* source
/// position: an error in the second of two sibling `xsl:value-of`s
/// reports the second one's line, not the body's first line.  Guards
/// the side-array `positions` indexing after the direct-push refactor.
#[test]
fn per_instruction_source_position_is_distinct() {
    // Two value-ofs on lines 5 and 6; only the second fails.
    let sheet = "\
<xsl:stylesheet version=\"2.0\"\n\
\x20 xmlns:xsl=\"http://www.w3.org/1999/XSL/Transform\"\n\
\x20 xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">\n\
\x20 <xsl:template match=\"/\"><out>\n\
\x20   <xsl:value-of select=\"1 + 1\"/>\n\
\x20   <xsl:value-of select=\"xs:integer('nope')\"/>\n\
\x20 </out></xsl:template>\n\
</xsl:stylesheet>";
    let xslt = Stylesheet::compile_str(sheet).expect("compile");
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    match xslt.apply(&doc).unwrap_err() {
        sup_xml_xslt::XsltError::Xpath(e) => {
            assert_eq!(e.line, Some(6), "should point at the failing instruction: {e:?}");
        }
        other => panic!("expected positioned Xpath error, got: {other:?}"),
    }
}
