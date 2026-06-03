//! Apply just iso_abstract_expand.xsl to a tiny schematron schema.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = "/Users/jp/projects/sup-xml/target/lxml-redir-pkg/lxml/isoschematron/resources/xsl/iso-schematron-xslt1";
    let iae = sup_xml_xslt::Stylesheet::compile_str(
        &std::fs::read_to_string(format!("{base}/iso_abstract_expand.xsl"))?
    )?;
    let opts = sup_xml_core::ParseOptions { namespace_aware: true, ..Default::default() };
    let schema = r#"<?xml version="1.0"?>
<schema xmlns="http://purl.oclc.org/dsdl/schematron">
    <pattern>
        <rule context="r">
            <assert test="@x">missing x</assert>
        </rule>
    </pattern>
</schema>"#;
    let doc = sup_xml_core::parse_str(schema, &opts)?;
    let result = iae.apply(&doc)?;
    println!("{}", result.to_string()?);
    Ok(())
}
