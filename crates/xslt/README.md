# sup-xml-xslt

The XSLT 1.0 transformation engine for
[SupXML](https://supso.org/projects/sup-xml), built on its XPath core.

**Most users should enable the `xslt` feature on
[`sup-xml`](https://crates.io/crates/sup-xml) instead** of depending on this
crate directly — that re-exports the engine through a stable public surface.

```rust
use sup_xml_xslt::Stylesheet;
use sup_xml_core::{parse_str, ParseOptions};

let xslt = Stylesheet::compile_str(r#"<xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
  <xsl:output method="xml" omit-xml-declaration="yes"/>
  <xsl:template match="/"><out><xsl:value-of select="/r"/></out></xsl:template>
</xsl:stylesheet>"#)?;

let doc = parse_str("<r>hello</r>", &ParseOptions::default())?;
let out = xslt.transform(&doc)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## License

SupXML is **source-available** software released through
[Supported Source](https://supso.org/). A valid license certificate is
required to use it; document parsing returns a fatal error without one (a
grace period applies after an existing certificate expires). Get a
certificate — free for individuals and non-monetized projects — at
[supso.org/projects/sup-xml](https://supso.org/projects/sup-xml). Full terms
are in the repository `LICENSE`.

## Documentation

- [Project docs](https://supso.org/projects/sup-xml/docs)
- [API reference (docs.rs)](https://docs.rs/sup-xml-xslt)
- [Source on GitHub](https://github.com/SupsoOrg/sup-xml)
