//! Run a Schematron schema through the ISO XSLT pipeline:
//!   1. iso_dsdl_include — resolve <sch:include>
//!   2. iso_abstract_expand — expand abstract patterns
//!   3. iso_svrl_for_xslt1 — emit the final validator stylesheet
//!
//! Each stage is a transformation; the output of stage N is the
//! input to stage N+1.  Then apply the final validator stylesheet
//! to the instance document and you get an SVRL report.
//!
//! `iso_svrl_for_xslt1.xsl` `xsl:import`s the skeleton stylesheet,
//! so we compile it via `compile_str_with_loader` pointed at the
//! pipeline directory.

use std::fs;
use std::path::PathBuf;
use sup_xml_xslt::{FilesystemLoader, Stylesheet};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = "/Users/jp/projects/sup-xml/target/lxml-redir-pkg/lxml/isoschematron/resources/xsl/iso-schematron-xslt1";
    let loader = FilesystemLoader::new(vec![PathBuf::from(base)]);

    let dsdl_path = format!("{base}/iso_dsdl_include.xsl");
    let abst_path = format!("{base}/iso_abstract_expand.xsl");
    let svrl_path = format!("{base}/iso_svrl_for_xslt1.xsl");
    let dsdl = Stylesheet::compile_str_with_loader(&fs::read_to_string(&dsdl_path)?, &loader, Some(&dsdl_path))?;
    let abst = Stylesheet::compile_str_with_loader(&fs::read_to_string(&abst_path)?, &loader, Some(&abst_path))?;
    let svrl = Stylesheet::compile_str_with_loader(&fs::read_to_string(&svrl_path)?, &loader, Some(&svrl_path))?;
    eprintln!("[ok] compiled 3 ISO pipeline XSLTs");

    // A trivial schema with an abstract pattern that gets extended.
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

    // Stage 1
    let stage1 = dsdl.apply(&schema_doc)?;
    let s1 = stage1.to_string()?;
    eprintln!("[ok] stage 1 (dsdl-include) — {} bytes", s1.len());

    let stage1_doc = sup_xml_core::parse_str(&s1, &opts)?;
    let stage2 = abst.apply(&stage1_doc)?;
    let s2 = stage2.to_string()?;
    eprintln!("[ok] stage 2 (abstract-expand) — {} bytes", s2.len());
    eprintln!("--- stage 2 output ---\n{s2}\n--- end ---");

    let stage2_doc = sup_xml_core::parse_str(&s2, &opts)?;
    let stage3 = svrl.apply(&stage2_doc)?;
    let s3 = stage3.to_string()?;
    eprintln!("[ok] stage 3 (svrl-for-xslt1) — {} bytes", s3.len());
    eprintln!("--- stage 3 output ---\n{s3}\n--- end ---");

    // Stage 3's output is the validator stylesheet.  Compile + apply.
    let validator = Stylesheet::compile_str(&s3)?;
    eprintln!("[ok] validator stylesheet compiled");

    let bad_instance = sup_xml_core::parse_str("<r/>", &opts)?;
    let report_rt = validator.apply(&bad_instance)?;
    let report = report_rt.to_string()?;
    eprintln!("[ok] validation produced {} bytes of SVRL", report.len());
    println!("{report}");
    Ok(())
}
