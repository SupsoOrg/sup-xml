//! Compile-and-run mirror of every Rust API snippet on the docs site
//! (`docs/src/content/docs/**`).  Each test corresponds to a code block
//! in the published documentation; keeping them here means a rename or
//! signature change that would break a documented example turns
//! `cargo test-all` red instead of shipping a broken copy-paste.
//!
//! When you change an example on the docs site, change it here too (and
//! vice versa).  I/O-bound snippets (`include_bytes!`, filesystem
//! resolvers, network) are adapted to self-contained inputs; the API
//! surface they exercise is identical.

type R = Result<(), Box<dyn std::error::Error>>;

// ── index.mdx / getting-started.md ───────────────────────────────────────────

#[test]
fn getting_started_parse_and_query() -> R {
    use sup_xml::{parse_str, ParseOptions, XPathContext};

    let opts = ParseOptions { namespace_aware: true, ..Default::default() };
    let doc = parse_str("<catalog><book id='b1'/><book id='b2'/></catalog>", &opts)?;

    let ctx = XPathContext::new(&doc);
    assert_eq!(ctx.eval_count("/catalog/book")?, 2);
    Ok(())
}

#[test]
fn getting_started_serialize() -> R {
    use sup_xml::{parse_str, serialize_to_string};

    let doc = parse_str("<r a='1' b='2'/>", &Default::default())?;
    let xml = serialize_to_string(&doc);
    assert!(xml.contains("<r"));
    Ok(())
}

// ── guides/parsing.md ────────────────────────────────────────────────────────

#[test]
fn parsing_from_str() -> R {
    use sup_xml::{parse_str, ParseOptions};
    let _doc = parse_str("<r/>", &ParseOptions::default())?;
    Ok(())
}

#[test]
fn parsing_from_bytes() -> R {
    use sup_xml::{parse_bytes, ParseOptions};

    let _doc = parse_bytes(b"<r/>", &ParseOptions::default())?;

    let opts = ParseOptions {
        recovery_mode: true,
        skip_inter_element_whitespace: true,
        ..Default::default()
    };
    let _doc = parse_bytes(b"<r><a/></r>", &opts)?;
    Ok(())
}

#[test]
fn parsing_serialization() -> R {
    use sup_xml::{parse_str, serialize_to_string, serialize_formatted, serialize_with, SerializeOptions};

    let doc = parse_str("<r><a/></r>", &Default::default())?;

    let _xml: String = serialize_to_string(&doc);
    let _pretty: String = serialize_formatted(&doc);

    let opts = SerializeOptions {
        write_xml_decl: true,
        format: true,
        indent: "    ".to_string(),
        ..Default::default()
    };
    let _xml: String = serialize_with(&doc, &opts);
    Ok(())
}

#[test]
fn parsing_streaming_bytes_reader() -> R {
    use sup_xml::{XmlBytesReader, BytesEvent};

    let mut r = XmlBytesReader::from_bytes(b"<r><a/><b>hi</b></r>")?;
    loop {
        match r.next()? {
            BytesEvent::Eof => break,
            BytesEvent::StartElement(t) => { let _ = String::from_utf8_lossy(t.name()); }
            BytesEvent::EndElement(t) => { let _ = String::from_utf8_lossy(t.name()); }
            BytesEvent::Text(t) => {
                let bytes = t.as_bytes();
                if !bytes.iter().all(u8::is_ascii_whitespace) {
                    let _ = String::from_utf8_lossy(bytes);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[test]
fn parsing_tree_walk() -> R {
    use sup_xml::Document;

    let doc: Document = sup_xml::parse_str("<r a='1'><b/></r>", &Default::default())?;
    let root = doc.root();
    let _ = root.name();
    for attr in root.attributes() {
        let _ = (attr.name(), attr.value());
    }
    for child in root.children() {
        let _ = child.name();
    }
    Ok(())
}

#[test]
fn parsing_streaming_from_read() -> R {
    use std::io::Cursor;
    use sup_xml::{XmlByteStreamReader, BytesEvent, DEFAULT_BUFFER_SIZE};

    // Any `io::Read` — a Cursor stands in for a File/socket/stdin here.
    let src = Cursor::new(b"<catalog><book>A</book><book>B</book></catalog>".to_vec());
    let mut reader = XmlByteStreamReader::new(src, DEFAULT_BUFFER_SIZE)?;

    let mut titles = Vec::new();
    loop {
        match reader.next_event()? {
            BytesEvent::Eof => break,
            BytesEvent::Text(t) =>
                titles.push(String::from_utf8_lossy(t.as_bytes()).into_owned()),
            _ => {}
        }
    }
    assert_eq!(titles, vec!["A", "B"]);
    Ok(())
}

// ── guides/xpath.md ──────────────────────────────────────────────────────────

#[test]
fn xpath_2_0_opt_in() -> R {
    use sup_xml::{parse_str, XPathContext, XPathOptions};

    let doc = parse_str("<r/>", &Default::default())?;
    let ctx = XPathContext::new_with(&doc, XPathOptions { xpath_2_0: true, ..Default::default() });
    let n: f64 = ctx.eval_num("count(for $x in 1 to 10 return $x * $x)")?;
    assert_eq!(n, 10.0);
    Ok(())
}

#[test]
fn xpath_basic_queries() -> R {
    use sup_xml::{parse_str, XPathContext};

    let doc = parse_str("<catalog><book id='b1'/><book id='b2'/></catalog>", &Default::default())?;
    let ctx = XPathContext::new(&doc);

    let n: usize = ctx.eval_count("/catalog/book")?;
    let total: f64 = ctx.eval_num("count(/catalog/book)")?;
    let s: String = ctx.eval_str("string(/catalog/book[1]/@id)")?;
    let b: bool = ctx.eval_bool("/catalog/book")?;
    assert_eq!((n, total, s.as_str(), b), (2, 2.0, "b1", true));
    Ok(())
}

#[test]
fn xpath_namespaces() -> R {
    use sup_xml::{parse_str, ParseOptions, XPathContext, XPathValue, XPathBindingsBuilder};

    let opts = ParseOptions { namespace_aware: true, ..Default::default() };
    let doc = parse_str("<catalog xmlns='http://example.com/ns'><book id='b1'/></catalog>", &opts)?;
    let ctx = XPathContext::new(&doc);

    let mut bindings = XPathBindingsBuilder::new();
    bindings.namespace("ns", "http://example.com/ns");

    let id = match ctx.eval_with("string(/ns:catalog/ns:book/@id)", 0, &bindings)? {
        XPathValue::String(s) => s,
        _ => String::new(),
    };
    assert_eq!(id, "b1");
    Ok(())
}

#[test]
fn xpath_custom_context_node() -> R {
    use sup_xml::{parse_str, XPathContext, XPathValue};

    let doc = parse_str("<catalog><book id='b1'/></catalog>", &Default::default())?;
    let ctx = XPathContext::new(&doc);

    let book = match ctx.eval("/catalog/book[1]")? {
        XPathValue::NodeSet(nodes) => nodes[0],
        _ => return Ok(()),
    };
    let id = match ctx.eval_at("string(@id)", book)? {
        XPathValue::String(s) => s,
        _ => String::new(),
    };
    assert_eq!(id, "b1");
    Ok(())
}

#[test]
fn xpath_bounding_untrusted() -> R {
    use sup_xml::{parse_str, XPathContext, XPathOptions};

    let doc = parse_str("<r/>", &Default::default())?;
    let opts = XPathOptions { max_eval_steps: 1_000_000, ..Default::default() };
    let _ctx = XPathContext::new_with(&doc, opts);
    Ok(())
}

// ── guides/canonical.md ──────────────────────────────────────────────────────

#[test]
fn canonical_inclusive() -> R {
    use sup_xml::{parse_str, canonicalize_to_bytes, CanonicalizeOptions, C14nMode};

    let doc = parse_str("<r b='2' a='1'/>", &Default::default())?;
    let opts = CanonicalizeOptions { mode: C14nMode::C14n10, with_comments: false };
    let c14n: Vec<u8> = canonicalize_to_bytes(&doc, &opts);
    assert_eq!(c14n, b"<r a=\"1\" b=\"2\"></r>");
    Ok(())
}

#[test]
fn canonical_exclusive() -> R {
    use sup_xml::{parse_str, canonicalize_to_bytes, CanonicalizeOptions, C14nMode};

    let doc = parse_str("<r b='2' a='1'/>", &Default::default())?;
    let opts = CanonicalizeOptions {
        mode: C14nMode::ExcC14n10 { inclusive_prefixes: vec![] },
        with_comments: false,
    };
    let _c14n = canonicalize_to_bytes(&doc, &opts);

    let opts = CanonicalizeOptions {
        mode: C14nMode::ExcC14n10 { inclusive_prefixes: vec!["wsse".into(), "ds".into()] },
        with_comments: false,
    };
    let _c14n = canonicalize_to_bytes(&doc, &opts);
    Ok(())
}

#[test]
fn canonical_subset_and_node() -> R {
    use sup_xml::{parse_str, canonicalize_with, canonicalize_node_to_bytes,
                  CanonicalizeOptions, C14nMode, CanonicalizeVisitTarget};

    let doc = parse_str("<doc><Signature/><payload>x</payload></doc>", &Default::default())?;
    let opts = CanonicalizeOptions { mode: C14nMode::C14n10, with_comments: false };

    let mut buf = Vec::new();
    canonicalize_with(&doc, &opts, &mut buf, |target| {
        match target {
            CanonicalizeVisitTarget::Node(n) => n.name() != "Signature",
            CanonicalizeVisitTarget::Attribute(_) => true,
        }
    })?;

    let target_node = doc.root();
    let _c14n: Vec<u8> = canonicalize_node_to_bytes(&target_node, &opts);
    Ok(())
}

#[test]
fn canonical_streaming_sink() -> R {
    // The published snippet hashes into `sha2::Sha256` (any `Write`
    // sink works); here we use a `Vec<u8>` to avoid the extra dep.
    use sup_xml::{parse_str, canonicalize_with, CanonicalizeOptions, C14nMode};

    let doc = parse_str("<r/>", &Default::default())?;
    let opts = CanonicalizeOptions { mode: C14nMode::C14n10, with_comments: false };
    let mut sink = Vec::new();
    canonicalize_with(&doc, &opts, &mut sink, |_| true)?;
    Ok(())
}

// ── guides/recovery.md ───────────────────────────────────────────────────────

#[test]
fn recovery_basic() -> R {
    use sup_xml::{parse_str_with_recovered, ParseOptions};

    let xml = "<r>tom & jerry<unclosed>";
    let opts = ParseOptions { recovery_mode: true, ..Default::default() };
    let (doc, recovered) = parse_str_with_recovered(xml, &opts);
    let _doc = doc.unwrap();
    assert!(!recovered.is_empty());
    for err in &recovered {
        let _ = &err.message;
    }
    Ok(())
}

// ── guides/encodings.md ──────────────────────────────────────────────────────

#[test]
fn encodings_auto_detect() -> R {
    use sup_xml::{parse_bytes, ParseOptions};

    // ISO-8859-1 document with a non-ASCII byte (é = 0xE9).
    let mut latin1 = b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><r>caf".to_vec();
    latin1.push(0xE9);
    latin1.extend_from_slice(b"</r>");

    let doc = parse_bytes(&latin1, &ParseOptions::default())?;
    assert_eq!(doc.root().name(), "r");
    Ok(())
}

#[test]
fn encodings_strict_utf8() -> R {
    use sup_xml::{parse_bytes, ParseOptions};
    let opts = ParseOptions { auto_transcode: false, ..Default::default() };
    let _doc = parse_bytes("<r>café</r>".as_bytes(), &opts)?;
    Ok(())
}

// ── guides/xsd.md (feature = "xsd") ──────────────────────────────────────────

#[cfg(feature = "xsd")]
mod xsd {
    use super::R;

    const SCHEMA: &str = r#"
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                   targetNamespace="urn:demo" xmlns="urn:demo">
          <xs:element name="port" type="xs:int"/>
        </xs:schema>"#;

    #[test]
    fn compile_and_validate() -> R {
        use sup_xml::xsd::Schema;

        let schema = Schema::compile_str(SCHEMA)?;
        schema.validate_str(r#"<port xmlns="urn:demo">8080</port>"#)?;

        let doc = sup_xml::parse_str("<port xmlns='urn:demo'>8080</port>", &Default::default())?;
        schema.validate_doc(&doc)?;

        schema.validate_bytes(br#"<port xmlns="urn:demo">8080</port>"#)?;
        Ok(())
    }

    #[test]
    fn compile_with_resolver() -> R {
        use sup_xml::xsd::{Schema, FsResolver};

        // Self-contained schema (no xs:include), so the resolver root is
        // never consulted — exercises the `compile_with` signature.
        let resolver = FsResolver::new("/srv/schemas");
        let _schema = Schema::compile_with(SCHEMA, resolver)?;
        Ok(())
    }

    #[test]
    fn versions() -> R {
        use sup_xml::xsd::{Schema, SchemaOptions, SchemaVersion};

        let _ = Schema::compile_str_with_options(SCHEMA,
            SchemaOptions { version: SchemaVersion::Xsd11, ..Default::default() })?;
        let _ = Schema::compile_str_with_options(SCHEMA,
            SchemaOptions { version: SchemaVersion::Auto, ..Default::default() })?;
        let _ = Schema::compile_str(SCHEMA)?;
        Ok(())
    }

    #[test]
    fn error_reporting() -> R {
        use sup_xml::xsd::Schema;

        let schema = Schema::compile_str(SCHEMA)?;
        match schema.validate_str(r#"<port xmlns="urn:demo">not-an-int</port>"#) {
            Ok(()) => {}
            Err(report) => {
                for issue in &report.issues {
                    let _ = (issue.line, issue.column, &issue.kind, &issue.message);
                }
            }
        }
        Ok(())
    }
}

// ── guides/xslt.md + schematron.md (feature = "xslt") ────────────────────────

#[cfg(feature = "xslt")]
mod xslt {
    use super::R;

    #[test]
    fn compile_and_apply() -> R {
        use sup_xml::{parse_str, ParseOptions};
        use sup_xml::xslt::Stylesheet;

        let xsl = r#"<xsl:stylesheet version="1.0"
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
          <xsl:template match="/catalog">
            <ul><xsl:for-each select="book"><li id="{@id}"/></xsl:for-each></ul>
          </xsl:template>
        </xsl:stylesheet>"#;

        let style = Stylesheet::compile_str(xsl)?;
        let doc = parse_str("<catalog><book id='b1'/></catalog>",
            &ParseOptions { namespace_aware: true, ..Default::default() })?;
        let result = style.apply(&doc)?;
        let _ = result.to_string()?;
        Ok(())
    }

    #[test]
    fn schematron_compile_and_validate() -> R {
        use sup_xml::xslt::schematron::Schematron;

        let sch = Schematron::compile_str(r#"
            <sch:schema xmlns:sch="http://purl.oclc.org/dsdl/schematron">
              <sch:pattern id="book-rules">
                <sch:rule context="book">
                  <sch:assert test="@isbn">every book must have an ISBN</sch:assert>
                </sch:rule>
              </sch:pattern>
            </sch:schema>"#)?;

        let report = sch.validate_str("<book/>")?;
        assert!(!report.findings.is_empty());
        for finding in &report.findings {
            let _ = (&finding.kind, &finding.message, &finding.context_name);
        }
        Ok(())
    }
}

// ── guides/serde.md (feature = "serde") ──────────────────────────────────────

#[cfg(feature = "serde")]
mod serde_de {
    use super::R;
    use serde::Deserialize;

    #[derive(Deserialize, Debug, PartialEq)]
    struct Book {
        #[serde(rename = "@id")]
        id: String,
        title: String,
        price: f64,
    }

    #[test]
    fn basic() -> R {
        use sup_xml::de::from_str;

        let xml = r#"
            <book id="b1">
                <title>The Soul of a New Machine</title>
                <price>19.99</price>
            </book>"#;

        let book: Book = from_str(xml)?;
        assert_eq!(book.id, "b1");
        assert_eq!(book.title, "The Soul of a New Machine");
        Ok(())
    }

    #[test]
    fn vec_and_bytes() -> R {
        use sup_xml::de::{from_str, from_bytes};

        #[derive(Deserialize)]
        struct Catalog {
            #[serde(rename = "book")]
            books: Vec<Book>,
        }

        let xml = r#"<catalog><book id="a"><title>A</title><price>1.0</price></book><book id="b"><title>B</title><price>2.0</price></book></catalog>"#;
        let cat: Catalog = from_str(xml)?;
        assert_eq!(cat.books.len(), 2);

        let book: Book = from_bytes(br#"<book id="b1"><title>T</title><price>1.0</price></book>"#)?;
        assert_eq!(book.id, "b1");
        Ok(())
    }

    #[test]
    fn options() -> R {
        use sup_xml::{ParseOptions, de::{from_str_opts, DeOptions}};

        let opts = DeOptions {
            parse: ParseOptions {
                recovery_mode: true,
                skip_inter_element_whitespace: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let _book: Book = from_str_opts(
            r#"<book id="b1"><title>T</title><price>1.0</price></book>"#, opts)?;
        Ok(())
    }

    // Pins the `$text` vs `$value` conventions documented in serde.md.

    #[test]
    fn dollar_text_captures_free_text() -> R {
        use sup_xml::de::from_str;

        #[derive(Deserialize)]
        struct Note {
            #[serde(rename = "@lang")]
            lang: String,
            #[serde(rename = "$text")]
            text: String,
        }

        let n: Note = from_str(r#"<note lang="en">hello world</note>"#)?;
        assert_eq!(n.lang, "en");
        assert_eq!(n.text, "hello world");
        Ok(())
    }

    #[test]
    fn dollar_value_collects_child_elements() -> R {
        use sup_xml::de::from_str;

        #[derive(Deserialize)]
        struct Doc {
            #[serde(rename = "$value")]
            items: Vec<String>,
        }

        // `$value` gathers child elements as a sequence …
        let d: Doc = from_str("<doc><a>x</a><b>y</b></doc>")?;
        assert_eq!(d.items, vec!["x", "y"]);

        // … and drops stray text between them ($value is for elements).
        let d: Doc = from_str("<doc>ignored<a>x</a></doc>")?;
        assert_eq!(d.items, vec!["x"]);
        Ok(())
    }

    #[test]
    fn plain_field_matches_child_element_not_text() -> R {
        use sup_xml::de::from_str;

        // A plainly-named field looks for a <text> child element; the
        // parent's free text is NOT captured by it.
        #[derive(Deserialize)]
        struct Plain { text: Option<String> }

        let p: Plain = from_str("<note>hello</note>")?;
        assert_eq!(p.text, None);
        Ok(())
    }
}

// ── guides/html.md (feature = "html") ────────────────────────────────────────

#[cfg(feature = "html")]
mod html {
    use super::R;
    use sup_xml::{HtmlSaxParser, HtmlSaxHandler, HtmlAttrs};

    #[test]
    fn parse_document() -> R {
        use sup_xml::{parse_html_str, parse_html_str_opts, HtmlParseOptions};

        let _doc = parse_html_str(r#"<!doctype html><html><body><p>hi</p></body></html>"#)?;

        let strict = HtmlParseOptions { recovery_mode: false, ..Default::default() };
        let _doc = parse_html_str_opts(
            "<!doctype html><html><body><p>hi</p></body></html>", &strict)?;
        Ok(())
    }

    #[test]
    fn query_like_xml() -> R {
        use sup_xml::{parse_html_str, XPathContext};

        let doc = parse_html_str("<html><body><h2>one</h2><h2>two</h2></body></html>")?;
        let ctx = XPathContext::new(&doc);
        let titles = ctx.eval_strings("//h2/text()")?;
        assert_eq!(titles.len(), 2);
        Ok(())
    }

    struct Counter { p: usize }
    impl HtmlSaxHandler for Counter {
        fn start_element(&mut self, name: &str, _attrs: HtmlAttrs<'_>) {
            if name == "p" { self.p += 1; }
        }
    }

    #[test]
    fn sax() -> R {
        let mut parser = HtmlSaxParser::new(Counter { p: 0 });
        parser.feed("<html><body><p>one</p><p>two</p></body></html>")?;
        let counter = parser.finish()?;
        assert_eq!(counter.p, 2);
        Ok(())
    }
}

// ── guides/async.md (feature = "tokio") ──────────────────────────────────────

#[cfg(feature = "tokio")]
mod async_io {
    #[tokio::test]
    async fn parse_async() -> Result<(), Box<dyn std::error::Error>> {
        use sup_xml::async_io::parse_async;

        let bytes: &[u8] = b"<r><b/></r>";
        let _doc = parse_async(bytes).await?;
        Ok(())
    }

    #[tokio::test]
    async fn parse_async_with_opts() -> Result<(), Box<dyn std::error::Error>> {
        use sup_xml::{async_io::parse_async_with, ParseOptions};

        let opts = ParseOptions {
            recovery_mode: true,
            skip_inter_element_whitespace: true,
            ..Default::default()
        };
        let _doc = parse_async_with(b"<r><b/></r>".as_slice(), &opts).await?;
        Ok(())
    }

    #[tokio::test]
    async fn parse_async_capped() -> Result<(), Box<dyn std::error::Error>> {
        use sup_xml::async_io::parse_async;
        use tokio::io::AsyncReadExt;

        const MAX_BODY: u64 = 10 * 1024 * 1024;
        let body: &[u8] = b"<r/>";
        let capped = body.take(MAX_BODY);
        let _doc = parse_async(capped).await?;
        Ok(())
    }
}
