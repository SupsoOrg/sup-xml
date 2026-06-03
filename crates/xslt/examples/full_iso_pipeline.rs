//! End-to-end ISO Schematron pipeline:
//!   schema → iso_svrl_for_xslt1 → validator stylesheet → SVRL report

use sup_xml_xslt::{FilesystemLoader, Stylesheet};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = "/Users/jp/projects/sup-xml/target/lxml-redir-pkg/lxml/isoschematron/resources/xsl/iso-schematron-xslt1";
    let loader = FilesystemLoader::new(vec![std::path::PathBuf::from(base)]);
    let opts = sup_xml_core::ParseOptions { namespace_aware: true, ..Default::default() };

    // Stage 1+2 (using full iso_svrl_for_xslt1 with imports)
    let svrl_path = format!("{base}/iso_svrl_for_xslt1.xsl");
    let svrl = Stylesheet::compile_str_with_loader(
        &std::fs::read_to_string(&svrl_path)?, &loader, Some(&svrl_path),
    )?;
    eprintln!("[stage 1] svrl meta compiled — {} templates", svrl.ast.templates.len());

    let schema = r#"<?xml version="1.0"?>
<schema xmlns="http://purl.oclc.org/dsdl/schematron">
    <pattern>
        <rule context="r">
            <assert test="@x">root element must have @x attribute</assert>
            <report test="@deprecated">root element uses deprecated marker</report>
        </rule>
    </pattern>
</schema>"#;
    let schema_doc = sup_xml_core::parse_str(schema, &opts)?;
    let validator_xsl = svrl.apply(&schema_doc)?.to_string()?;
    eprintln!("[stage 2] validator XSLT generated — {} bytes", validator_xsl.len());
    std::fs::write("/tmp/validator.xsl", &validator_xsl)?;
    eprintln!("[stage 2] wrote /tmp/validator.xsl");

    // Stage 3: compile and apply the generated validator to instances.
    let validator = Stylesheet::compile_str_with_loader(
        &validator_xsl, &loader, Some(&svrl_path))?;
    eprintln!("[stage 3] validator compiled — {} templates", validator.ast.templates.len());

    // Valid instance.
    let good = sup_xml_core::parse_str(r#"<r x="1"/>"#, &opts)?;
    let report = validator.apply(&good)?.to_string()?;
    eprintln!("--- VALID instance SVRL ---\n{report}\n");

    // Invalid instance.
    let bad = sup_xml_core::parse_str(r#"<r/>"#, &opts)?;
    let report = validator.apply(&bad)?.to_string()?;
    eprintln!("--- INVALID instance SVRL ---\n{report}\n");

    // Instance triggering the report.
    let dep = sup_xml_core::parse_str(r#"<r x="1" deprecated="yes"/>"#, &opts)?;
    let report = validator.apply(&dep)?.to_string()?;
    eprintln!("--- DEPRECATED instance SVRL ---\n{report}\n");

    Ok(())
}
