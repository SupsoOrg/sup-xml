//! Minimal identity-transform reproducer.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ss = r#"<?xml version="1.0"?>
<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
    <xsl:template match="/">
        <xsl:apply-templates select="." mode="go"/>
    </xsl:template>
    <xsl:template match="*" mode="go">
        <xsl:copy>
            <xsl:copy-of select="@*"/>
            <xsl:apply-templates mode="go"/>
        </xsl:copy>
    </xsl:template>
</xsl:stylesheet>"#;
    let xslt = sup_xml_xslt::Stylesheet::compile_str(ss)?;
    let opts = sup_xml_core::ParseOptions { namespace_aware: true, ..Default::default() };
    let doc = sup_xml_core::parse_str(r#"<r foo="bar"><c/></r>"#, &opts)?;
    let result = xslt.apply(&doc)?;
    println!("{}", result.to_string()?);
    Ok(())
}
