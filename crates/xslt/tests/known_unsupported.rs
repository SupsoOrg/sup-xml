//! Reminder tests for important XSLT 2.0/3.0 features we deliberately
//! don't implement.  Each one documents a feature, gives a minimal
//! stylesheet that exercises it, and asserts our current behavior.
//!
//! These tests PASS today because they assert that we DON'T have the
//! feature — either by compile-erroring or by ignoring the instruction
//! and producing un-streamed / un-schema-aware output.  If somebody
//! eventually wires up streaming or schema-aware processing, these
//! tests will fail and force the implementer to:
//!
//!   1. Acknowledge that they're enabling a major feature.
//!   2. Update or replace these reminders with real conformance tests
//!      against the W3C suite (see `xslt30.rs` for the runner).
//!
//! The W3C XSLT 3.0 test suite has extensive coverage for both
//! feature families:
//!   * Streaming:
//!     - `tests/attr/streamable/`         (`streamable=` attribute)
//!     - `tests/fn/stream-available/`     (`fn:stream-available()`)
//!     - `tests/insn/source-document/`    (`<xsl:source-document>`)
//!     - `tests/insn/iterate/`            (`<xsl:iterate>`)
//!     - `tests/insn/merge/`              (`<xsl:merge>`)
//!     - `tests/misc/streaming-fallback/` (graceful degradation)
//!     - `tests/strm/`                    (entire dir is streaming-only)
//!   * Schema-awareness:
//!     - `tests/decl/import-schema/`      (`<xsl:import-schema>`)
//!     - `tests/attr/validation/`         (`validation=` on element/attribute/document)
//!     - `tests/attr/strip-type-annotations/`
//!
//! These features are the ones Saxon-EE charges money for; Saxon-HE
//! has neither.  Implementing them would be substantial work — see
//! design notes in the relevant module docs once landed.

use sup_xml_core::{parse_str, ParseOptions};
use sup_xml_xslt::Stylesheet;

// ── streaming ───────────────────────────────────────────────────────

/// `<xsl:source-document streamable="yes">` is the canonical way to
/// open a streamed source in XSLT 3.0.  Our engine doesn't recognise
/// the instruction at all — at best the unknown XSLT element gets
/// compiled into the `Unsupported` AST node and errors at run time.
///
/// What a real streaming impl would do: read `loans.xml` lazily, run
/// the body's apply-templates once per subtree, never materialise the
/// whole tree.  For a 50 GB document, that's the difference between
/// "works in 100 MB of RAM" and "crashes".
#[test]
fn streaming_source_document_is_unsupported() {
    // Use just <xsl:template match="/"> as the body so XSLT 1.0
    // compile accepts the surrounding stylesheet structure.  The
    // <xsl:source-document> instruction is the 3.0-only bit.
    let xsl = r#"<xsl:stylesheet version="3.0"
                                xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template name="main">
            <xsl:source-document href="huge.xml" streamable="yes">
                <xsl:apply-templates select="/items/item"/>
            </xsl:source-document>
        </xsl:template>
    </xsl:stylesheet>"#;

    let stylesheet = Stylesheet::compile_str(xsl)
        .expect("compile must succeed — unsupported instructions reach run time");
    let src = parse_str("<dummy/>", &ParseOptions::default()).unwrap();
    let result = stylesheet.apply(&src);
    // Either the compile path swallowed source-document and produced
    // an empty result, or running it errors.  Both are acceptable
    // "we don't support streaming" outcomes — what's NOT acceptable
    // is silently transforming the document as if streaming were
    // honoured.  The point of this test is to fail loudly the day
    // someone wires up real streaming so that test gets replaced
    // with W3C-suite coverage.
    match result {
        Ok(rt) => {
            let s = rt.to_string().unwrap_or_default();
            assert!(s.trim().is_empty() || !s.contains("item"),
                "streaming source-document apparently honoured — replace this test \
                 with real streaming conformance coverage from the W3C suite \
                 (tests/insn/source-document/ etc.)");
        }
        Err(_) => { /* expected — instruction unsupported */ }
    }
}

/// `fn:stream-available($uri)` per XSLT 3.0 — returns true iff the
/// processor can stream from the URI.  Our engine doesn't define it,
/// so it must surface as an unknown-function error.
#[test]
fn stream_available_function_is_unsupported() {
    let xsl = r#"<xsl:stylesheet version="3.0"
                                xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                                xmlns:fn="http://www.w3.org/2005/xpath-functions">
        <xsl:template match="/">
            <out><xsl:value-of select="fn:stream-available('foo.xml')"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;

    let stylesheet = Stylesheet::compile_str(xsl)
        .expect("compile-time XPath check happens at apply time, not compile");
    let src = parse_str("<r/>", &ParseOptions::default()).unwrap();
    let result = stylesheet.apply(&src);
    match result {
        Err(_) => { /* expected — fn:stream-available undefined */ }
        Ok(rt) => {
            let s = rt.to_string().unwrap_or_default();
            // If anyone implements stream-available(), it would emit
            // "true" or "false".  Either string signals we shipped it.
            assert!(!s.contains("true") && !s.contains("false"),
                "fn:stream-available appears to resolve — replace with \
                 real conformance test from tests/fn/stream-available/");
        }
    }
}

// ── schema-awareness ─────────────────────────────────────────────────

/// `<xsl:import-schema>` is the XSLT 2.0+ instruction that imports
/// an XSD into the stylesheet's static context.  After import, XPath
/// expressions can reference schema types (`element(*, MyType)`,
/// `cast as xs:date`, etc.) and source documents get validated +
/// PSVI-annotated as they're loaded.
///
/// Our engine doesn't process `<xsl:import-schema>` at all — it lands
/// in the `Unsupported` AST bin.  Stylesheets that depend on it
/// silently degrade to untyped behavior.
#[test]
fn import_schema_is_unsupported() {
    let xsl = r#"<xsl:stylesheet version="2.0"
                                xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                                xmlns:xs="http://www.w3.org/2001/XMLSchema">
        <xsl:import-schema namespace="urn:test"
                          schema-location="loans.xsd"/>
        <xsl:template match="/">
            <out>untyped</out>
        </xsl:template>
    </xsl:stylesheet>"#;

    // Should compile (we tolerate unknown top-level elements).  The
    // schema isn't actually fetched or honoured.
    let stylesheet = Stylesheet::compile_str(xsl)
        .expect("compile must succeed even if we don't honour import-schema");
    let src = parse_str("<r/>", &ParseOptions::default()).unwrap();
    let out = stylesheet.apply(&src).expect("apply must succeed for unrelated body");
    let s = out.to_string().unwrap_or_default();
    assert!(s.contains("untyped"),
        "stylesheet body produced wrong output — but the point of this test is \
         to confirm xsl:import-schema is ignored, not whether the body runs");
}

/// `validation="strict"` on `<xsl:document>` / `<xsl:element>` /
/// `<xsl:copy>` etc. asks for XSD validation of the produced result
/// fragment.  Schema-aware processors enforce; non-schema-aware ones
/// must either reject the stylesheet or strip the attribute.
#[test]
fn validation_attribute_is_unsupported() {
    let xsl = r#"<xsl:stylesheet version="2.0"
                                xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <xsl:element name="out" validation="strict">
                <bad-content-per-schema>but we don't check</bad-content-per-schema>
            </xsl:element>
        </xsl:template>
    </xsl:stylesheet>"#;

    let stylesheet = Stylesheet::compile_str(xsl).expect("compile must succeed");
    let src = parse_str("<r/>", &ParseOptions::default()).unwrap();
    let out = stylesheet.apply(&src).expect("apply must succeed — validation= is ignored");
    let s = out.to_string().unwrap_or_default();
    // Schema-aware engines would error on `validation="strict"` if
    // no schema applies to <out>.  We produce the un-validated output.
    assert!(s.contains("bad-content-per-schema"),
        "unexpected output shape — but the assertion that matters is that we \
         don't reject the stylesheet over the unhonoured validation= attribute");
}

/// XSLT-extension XPath type tests like `element(*, MyType)` carry a
/// schema-type qualifier that a non-schema-aware processor can't
/// honour.  XPath 2.0 §2.5.4 defines the fallback: when no schema is
/// imported, the type qualifier is treated as if absent — i.e.,
/// `element(*, T)` reduces to `element(*)` and matches any element.
/// This test documents that fallback so a future schema-aware
/// implementation knows to revisit it: when types ARE honoured,
/// the typed template here should NOT match `<amount>42</amount>`
/// (whose declared type isn't `my:Currency`).
#[test]
fn schema_aware_type_test_falls_back_to_element_wildcard() {
    let xsl = r#"<xsl:stylesheet version="2.0"
                                xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                                xmlns:my="urn:test">
        <xsl:template match="element(*, my:Currency)">
            <typed/>
        </xsl:template>
        <xsl:template match="/">
            <out><xsl:apply-templates/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let stylesheet = Stylesheet::compile_str(xsl)
        .expect("schema-aware element() pattern should parse (type qualifier ignored)");
    let src = parse_str("<r><amount>42</amount></r>", &ParseOptions::default()).unwrap();
    let out = stylesheet.apply(&src).unwrap().to_string().unwrap();
    // Fallback behaviour: typed template matches because the qualifier
    // is dropped.  When schema-awareness lands, flip this to assert
    // the opposite.
    assert!(out.contains("<typed/>"), "expected typed template to match: {out}");
}
