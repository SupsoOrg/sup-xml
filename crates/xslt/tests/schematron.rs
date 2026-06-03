//! Schematron integration tests — exercise realistic schemas
//! against realistic instance documents.  These test the public
//! API end-to-end: `Schematron::compile_str` →
//! `Schematron::validate_str` → `ValidationReport`.

use std::path::Path;

use sup_xml_xslt::schematron::{FindingKind, Schematron};

#[test]
fn order_total_validation() {
    // A common business-rule check: order total must equal the
    // sum of line items.
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="order">
                    <assert test="number(@total) = sum(line/@price)">
                        order total <value-of select="@total"/> doesn't match line sum
                    </assert>
                </rule>
            </pattern>
        </schema>
    "#).unwrap();

    // Valid case: 10 + 20 = 30.
    let r = sch.validate_str(r#"<order total="30">
        <line price="10"/>
        <line price="20"/>
    </order>"#).unwrap();
    assert!(r.valid(), "{:?}", r.findings);

    // Invalid case: 10 + 20 != 50.
    let r = sch.validate_str(r#"<order total="50">
        <line price="10"/>
        <line price="20"/>
    </order>"#).unwrap();
    assert!(!r.valid());
    assert_eq!(r.findings.len(), 1);
    assert!(r.findings[0].message.contains("50"),
        "expected the AVT message to include the total, got: {}",
        r.findings[0].message);
}

#[test]
fn multiple_patterns_run_independently() {
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern id="non-empty">
                <rule context="item">
                    <assert test="normalize-space(.) != ''">item is empty</assert>
                </rule>
            </pattern>
            <pattern id="has-id">
                <rule context="item">
                    <assert test="@id">item has no id</assert>
                </rule>
            </pattern>
        </schema>
    "#).unwrap();
    // Two failures: both patterns fire on the same node.
    let r = sch.validate_str(r#"<r><item></item></r>"#).unwrap();
    assert!(!r.valid());
    let pattern_ids: Vec<_> = r.findings.iter()
        .map(|f| f.pattern_id.as_deref().unwrap_or(""))
        .collect();
    assert!(pattern_ids.contains(&"non-empty"));
    assert!(pattern_ids.contains(&"has-id"));
}

#[test]
fn schema_let_provides_global_binding() {
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <let name="min-len" value="3"/>
            <pattern>
                <rule context="word">
                    <assert test="string-length(.) &gt;= $min-len">
                        word too short
                    </assert>
                </rule>
            </pattern>
        </schema>
    "#).unwrap();

    let r = sch.validate_str(r#"<r><word>hi</word></r>"#).unwrap();
    assert!(!r.valid());
    let r = sch.validate_str(r#"<r><word>hello</word></r>"#).unwrap();
    assert!(r.valid());
}

#[test]
fn report_is_diagnostic_not_invalidating() {
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="msg">
                    <report test="@old">use of legacy attr 'old' noted</report>
                </rule>
            </pattern>
        </schema>
    "#).unwrap();
    let r = sch.validate_str(r#"<msg old="x"/>"#).unwrap();
    // The report fired (it's a Finding) but the document is still valid.
    assert!(r.valid());
    assert_eq!(r.findings.len(), 1);
    assert_eq!(r.findings[0].kind, FindingKind::SuccessfulReport);
}

#[test]
fn role_attribute_carries_through_to_finding() {
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="r">
                    <assert test="@x" role="critical">missing x</assert>
                </rule>
            </pattern>
        </schema>
    "#).unwrap();
    let r = sch.validate_str("<r/>").unwrap();
    assert_eq!(r.findings[0].role.as_deref(), Some("critical"));
}

/// End-to-end ISO Schematron pipeline test.  Skipped when the
/// vendored XSLT files aren't available locally (the suite ran in
/// this repo bundles lxml's resources at a known path).
#[test]
fn iso_pipeline_produces_svrl() {
    let base = "/Users/jp/projects/sup-xml/target/lxml-redir-pkg/lxml/isoschematron/resources/xsl/iso-schematron-xslt1";
    if !Path::new(base).join("iso_svrl_for_xslt1.xsl").exists() {
        eprintln!("skipping: ISO pipeline resources not staged");
        return;
    }
    let loader = sup_xml_xslt::FilesystemLoader::new(vec![std::path::PathBuf::from(base)]);
    let schema = r#"<?xml version="1.0"?>
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="r">
                    <assert test="@x">root must have @x</assert>
                </rule>
            </pattern>
        </schema>"#;
    let validator = Schematron::compile_iso(schema, base, &loader).expect("iso pipeline");

    // Valid → SVRL should contain a fired-rule but no failed-assert.
    let valid_svrl = validator.validate_str(r#"<r x="1"/>"#).unwrap();
    assert!(valid_svrl.contains("<svrl:fired-rule"), "got: {valid_svrl}");
    assert!(!valid_svrl.contains("<svrl:failed-assert"), "got: {valid_svrl}");

    // Invalid → SVRL should contain failed-assert.
    let invalid_svrl = validator.validate_str(r#"<r/>"#).unwrap();
    assert!(invalid_svrl.contains("<svrl:failed-assert"), "got: {invalid_svrl}");
    assert!(invalid_svrl.contains("root must have @x"), "got: {invalid_svrl}");
}

#[test]
fn nested_xpath_works() {
    // Test that XPath expressions involving multiple steps work.
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="book">
                    <assert test="count(author) &gt;= 1">book must have at least one author</assert>
                    <assert test="count(chapter) &gt;= 1">book must have at least one chapter</assert>
                </rule>
            </pattern>
        </schema>
    "#).unwrap();
    let r = sch.validate_str(r#"<book>
        <author>A</author>
        <chapter>One</chapter>
    </book>"#).unwrap();
    assert!(r.valid());
    let r = sch.validate_str(r#"<book><author>A</author></book>"#).unwrap();
    assert!(!r.valid());
    assert_eq!(r.findings.len(), 1);
    assert!(r.findings[0].message.contains("chapter"));
}

// ── <sch:include> ─────────────────────────────────────────────

#[test]
fn include_splices_top_level_pattern() {
    // Main schema includes an external file that contributes a
    // whole <pattern>.  Validation should fire the included rule.
    let loader = sup_xml_xslt::InMemoryLoader::new()
        .with("rules.sch", r#"<?xml version="1.0"?>
<schema xmlns="http://purl.oclc.org/dsdl/schematron">
    <pattern>
        <rule context="r">
            <assert test="@x">missing x</assert>
        </rule>
    </pattern>
</schema>"#);
    let main = r#"<?xml version="1.0"?>
<schema xmlns="http://purl.oclc.org/dsdl/schematron">
    <include href="rules.sch"/>
</schema>"#;
    let sch = Schematron::compile_str_with_loader(main, &loader, None).unwrap();
    let r = sch.validate_str("<r/>").unwrap();
    assert!(!r.valid(), "expected failed-assert from included rule");
    assert!(r.findings[0].message.contains("missing x"));
}

#[test]
fn include_with_fragment_id_targets_named_element() {
    let loader = sup_xml_xslt::InMemoryLoader::new()
        .with("lib.sch", r#"<?xml version="1.0"?>
<library xmlns="http://purl.oclc.org/dsdl/schematron">
    <pattern id="check-foo">
        <rule context="foo"><assert test="@id">foo needs id</assert></rule>
    </pattern>
    <pattern id="check-bar">
        <rule context="bar"><assert test="@id">bar needs id</assert></rule>
    </pattern>
</library>"#);
    let main = r#"<?xml version="1.0"?>
<schema xmlns="http://purl.oclc.org/dsdl/schematron">
    <include href="lib.sch#check-bar"/>
</schema>"#;
    let sch = Schematron::compile_str_with_loader(main, &loader, None).unwrap();
    // Only the `check-bar` pattern was included — <foo/> should validate cleanly.
    let r = sch.validate_str("<foo/>").unwrap();
    assert!(r.valid());
    // But <bar/> should fail.
    let r = sch.validate_str("<bar/>").unwrap();
    assert!(!r.valid());
}

#[test]
fn include_at_pattern_level_splices_rules() {
    // The <include> lives inside a <pattern>, so the loaded
    // element's <rule>s become part of that pattern.
    let loader = sup_xml_xslt::InMemoryLoader::new()
        .with("more-rules.sch", r#"<?xml version="1.0"?>
<pattern xmlns="http://purl.oclc.org/dsdl/schematron">
    <rule context="extra">
        <assert test="@kind">extra needs kind</assert>
    </rule>
</pattern>"#);
    let main = r#"<?xml version="1.0"?>
<schema xmlns="http://purl.oclc.org/dsdl/schematron">
    <pattern>
        <rule context="r"><assert test="@x">r needs x</assert></rule>
        <include href="more-rules.sch"/>
    </pattern>
</schema>"#;
    let sch = Schematron::compile_str_with_loader(main, &loader, None).unwrap();
    // <r x="1"/> passes, but the merged <extra/> rule fires.
    let r = sch.validate_str(r#"<r x="1"><extra/></r>"#).unwrap();
    assert!(!r.valid());
    assert!(r.findings[0].message.contains("extra needs kind"));
}

#[test]
fn include_at_rule_level_splices_asserts() {
    // <include> inside a <rule> pulls in extra asserts/reports
    // for the same context.
    let loader = sup_xml_xslt::InMemoryLoader::new()
        .with("checks.sch", r#"<?xml version="1.0"?>
<rule xmlns="http://purl.oclc.org/dsdl/schematron" context="ignored">
    <assert test="@y">also needs y</assert>
</rule>"#);
    let main = r#"<?xml version="1.0"?>
<schema xmlns="http://purl.oclc.org/dsdl/schematron">
    <pattern>
        <rule context="r">
            <assert test="@x">needs x</assert>
            <include href="checks.sch"/>
        </rule>
    </pattern>
</schema>"#;
    let sch = Schematron::compile_str_with_loader(main, &loader, None).unwrap();
    // <r x="1"/> still misses @y → one failure.
    let r = sch.validate_str(r#"<r x="1"/>"#).unwrap();
    assert_eq!(r.findings.len(), 1);
    assert!(r.findings[0].message.contains("needs y"));
}

#[test]
fn include_missing_fragment_errors_clearly() {
    let loader = sup_xml_xslt::InMemoryLoader::new()
        .with("lib.sch", r#"<?xml version="1.0"?>
<library xmlns="http://purl.oclc.org/dsdl/schematron">
    <pattern id="known"><rule context="r"><assert test="@x">x</assert></rule></pattern>
</library>"#);
    let main = r#"<?xml version="1.0"?>
<schema xmlns="http://purl.oclc.org/dsdl/schematron">
    <include href="lib.sch#nope"/>
</schema>"#;
    let err = Schematron::compile_str_with_loader(main, &loader, None).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("#nope"), "got: {msg}");
}

#[test]
fn compile_without_loader_errors_on_include() {
    let main = r#"<?xml version="1.0"?>
<schema xmlns="http://purl.oclc.org/dsdl/schematron">
    <include href="other.sch"/>
</schema>"#;
    // Plain compile_str defaults to NullLoader — <include> fails.
    assert!(Schematron::compile_str(main).is_err());
}

// ── <sch:phase> / <sch:active> ────────────────────────────────

#[test]
fn phase_runs_only_active_patterns() {
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <phase id="check-x">
                <active pattern="p-x"/>
            </phase>
            <phase id="check-y">
                <active pattern="p-y"/>
            </phase>
            <pattern id="p-x">
                <rule context="r"><assert test="@x">missing x</assert></rule>
            </pattern>
            <pattern id="p-y">
                <rule context="r"><assert test="@y">missing y</assert></rule>
            </pattern>
        </schema>
    "#).unwrap();

    // <r/> would fail both patterns under #ALL.
    let r = sch.validate_str_with_phase("<r/>", "#ALL").unwrap();
    assert_eq!(r.findings.len(), 2);

    // check-x phase: only the x-pattern fires.
    let r = sch.validate_str_with_phase("<r/>", "check-x").unwrap();
    assert_eq!(r.findings.len(), 1);
    assert!(r.findings[0].message.contains("missing x"));

    // check-y phase: only the y-pattern fires.
    let r = sch.validate_str_with_phase("<r/>", "check-y").unwrap();
    assert_eq!(r.findings.len(), 1);
    assert!(r.findings[0].message.contains("missing y"));
}

#[test]
fn phase_default_uses_schema_defaultphase_attr() {
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron" defaultPhase="strict">
            <phase id="lenient">
                <active pattern="p-x"/>
            </phase>
            <phase id="strict">
                <active pattern="p-x"/>
                <active pattern="p-y"/>
            </phase>
            <pattern id="p-x"><rule context="r"><assert test="@x">x</assert></rule></pattern>
            <pattern id="p-y"><rule context="r"><assert test="@y">y</assert></rule></pattern>
        </schema>
    "#).unwrap();

    // #DEFAULT resolves to defaultPhase="strict" → both patterns.
    let r = sch.validate_str_with_phase("<r/>", "#DEFAULT").unwrap();
    assert_eq!(r.findings.len(), 2);
}

#[test]
fn phase_unknown_name_errors() {
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern id="p"><rule context="r"><assert test="@x">x</assert></rule></pattern>
        </schema>
    "#).unwrap();
    let err = sch.validate_str_with_phase("<r/>", "no-such-phase").unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("no-such-phase"), "got: {msg}");
}

// ── <pattern abstract="true"> / is-a / param ──────────────────

#[test]
fn abstract_pattern_instantiates_with_param() {
    // The abstract pattern defines a slot $what; the concrete
    // instances fill it with different attribute names.
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern abstract="true" id="required-attr">
                <rule context="*">
                    <assert test="$what">element missing required attribute</assert>
                </rule>
            </pattern>
            <pattern is-a="required-attr" id="needs-id">
                <param name="what" value="@id"/>
            </pattern>
        </schema>
    "#).unwrap();

    let r = sch.validate_str("<r/>").unwrap();
    assert_eq!(r.findings.len(), 1, "{:?}", r.findings);
    assert!(r.findings[0].message.contains("missing required attribute"));

    let r = sch.validate_str(r#"<r id="ok"/>"#).unwrap();
    assert!(r.valid());
}

#[test]
fn abstract_pattern_multiple_instances_with_different_params() {
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern abstract="true" id="elt-needs">
                <rule context="$ctx">
                    <assert test="$attr">missing</assert>
                </rule>
            </pattern>
            <pattern is-a="elt-needs" id="r-needs-x">
                <param name="ctx" value="r"/>
                <param name="attr" value="@x"/>
            </pattern>
            <pattern is-a="elt-needs" id="r-needs-y">
                <param name="ctx" value="r"/>
                <param name="attr" value="@y"/>
            </pattern>
        </schema>
    "#).unwrap();

    let r = sch.validate_str("<r/>").unwrap();
    assert_eq!(r.findings.len(), 2);

    let r = sch.validate_str(r#"<r x="1"/>"#).unwrap();
    assert_eq!(r.findings.len(), 1);  // still missing y

    let r = sch.validate_str(r#"<r x="1" y="2"/>"#).unwrap();
    assert!(r.valid());
}

#[test]
fn abstract_pattern_unknown_id_errors() {
    let err = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern is-a="nope">
                <param name="x" value="@a"/>
            </pattern>
        </schema>
    "#).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("nope"), "got: {msg}");
}

#[test]
fn abstract_patterns_themselves_dont_run() {
    // Abstract patterns are templates; they should NOT fire on their own.
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern abstract="true" id="abstract-only">
                <rule context="r"><assert test="$x">never fires directly</assert></rule>
            </pattern>
        </schema>
    "#).unwrap();
    let r = sch.validate_str("<r/>").unwrap();
    assert!(r.valid(), "abstract patterns must not produce findings");
}

// ── <sch:extends rule=…> ──────────────────────────────────────

#[test]
fn extends_splices_abstract_rule_contents() {
    // The abstract rule has shared assertions; concrete rules pull
    // them in via <extends rule="…"/>.
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule abstract="true" id="needs-version-and-namespace">
                    <assert test="@version">missing version</assert>
                    <assert test="@xmlns">missing default namespace</assert>
                </rule>
                <rule context="root">
                    <extends rule="needs-version-and-namespace"/>
                    <assert test="@purpose">missing purpose</assert>
                </rule>
            </pattern>
        </schema>
    "#).unwrap();
    let r = sch.validate_str("<root/>").unwrap();
    // 3 failures: version, xmlns, purpose.
    assert_eq!(r.findings.len(), 3, "{:#?}", r.findings);
}

#[test]
fn extends_unknown_rule_id_errors() {
    let err = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule context="r">
                    <extends rule="ghost"/>
                </rule>
            </pattern>
        </schema>
    "#).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("ghost"), "got: {msg}");
}

#[test]
fn abstract_rules_themselves_dont_fire() {
    // Abstract rules are templates; only their content fires when
    // pulled in via <extends>.  Standing alone, they should be no-op.
    let sch = Schematron::compile_str(r#"
        <schema xmlns="http://purl.oclc.org/dsdl/schematron">
            <pattern>
                <rule abstract="true" id="lonely">
                    <assert test="false()">should never fire</assert>
                </rule>
            </pattern>
        </schema>
    "#).unwrap();
    let r = sch.validate_str("<anything/>").unwrap();
    assert!(r.valid());
}
