//! Full ISO Schematron pipeline using xsl:import resolution.

use sup_xml_xslt::{FilesystemLoader, Stylesheet};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = "/Users/jp/projects/sup-xml/target/lxml-redir-pkg/lxml/isoschematron/resources/xsl/iso-schematron-xslt1";
    let loader = FilesystemLoader::new(vec![std::path::PathBuf::from(base)]);

    eprintln!("[step] compiling iso_svrl_for_xslt1 with imports...");
    let svrl_path = format!("{base}/iso_svrl_for_xslt1.xsl");
    let svrl_text = std::fs::read_to_string(&svrl_path)?;
    let svrl = Stylesheet::compile_str_with_loader(&svrl_text, &loader, Some(&svrl_path))?;
    eprintln!("[ok]   compiled — {} templates total", svrl.ast.templates.len());

    eprintln!("[step] applying iso_svrl to a tiny schematron schema...");
    let schema = r#"<?xml version="1.0"?>
<schema xmlns="http://purl.oclc.org/dsdl/schematron">
    <pattern>
        <rule context="r">
            <assert test="@x">missing x</assert>
        </rule>
    </pattern>
</schema>"#;
    let opts = sup_xml_core::ParseOptions { namespace_aware: true, ..Default::default() };
    let schema_doc = sup_xml_core::parse_str(schema, &opts)?;
    let result = svrl.apply(&schema_doc)?;
    let out = result.to_string()?;
    eprintln!("[ok]   produced {} bytes of validator XSLT", out.len());
    println!("{out}");
    Ok(())
}
