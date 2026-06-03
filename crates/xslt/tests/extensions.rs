//! End-to-end coverage for caller-registered XPath extension
//! functions.  Covers the `Extensions` builder and the underlying
//! `ExtensionFunctions` trait, plus the interaction with built-in
//! XSLT/EXSLT dispatch.

use sup_xml_core::error::{ErrorDomain, ErrorLevel, XmlError};
use sup_xml_core::{parse_str, ParseOptions};
use sup_xml_core::xpath::eval::Numeric;
use sup_xml_xslt::{ExtensionFunctions, Extensions, Stylesheet, XPathValue};

fn ext_err(msg: &str) -> XmlError {
    XmlError::new(ErrorDomain::XPath, ErrorLevel::Error, msg)
}

fn render(xsl: &str, src: &str, exts: &dyn ExtensionFunctions) -> String {
    let style = Stylesheet::compile_str(xsl).expect("compile stylesheet");
    let doc = parse_str(src, &ParseOptions::default()).expect("parse source");
    let result = style.apply_with_extensions(&doc, exts).expect("apply");
    result.to_string().expect("serialize")
}

/// Basic case: a closure registered in a custom namespace runs and
/// its return value flows through `<xsl:value-of>`.
#[test]
fn closure_extension_called_from_value_of() {
    let mut exts = Extensions::new();
    exts.register("urn:test", "double", |args| {
        let n = match args.first() {
            Some(XPathValue::Number(n)) => n.as_f64(),
            Some(XPathValue::String(s)) => s.parse().unwrap_or(0.0),
            _ => 0.0,
        };
        Ok(XPathValue::Number(Numeric::Double(n * 2.0)))
    });

    let xsl = r#"<xsl:stylesheet version="1.0"
                                 xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                                 xmlns:t="urn:test"
                                 exclude-result-prefixes="t">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/r">
            <out><xsl:value-of select="t:double(21)"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = render(xsl, "<r/>", &exts);
    assert_eq!(out, "<out>42</out>");
}

/// String-returning extension wired into an attribute value template.
/// Demonstrates the common pattern of taking an explicit `string()`
/// of the argument inside the stylesheet so the extension never has
/// to deal with node-set coercion itself.
#[test]
fn extension_in_avt() {
    let mut exts = Extensions::new();
    exts.register("urn:lookup", "label", |args| {
        let id = match args.first() {
            Some(XPathValue::String(s)) => s.as_str(),
            _ => "",
        };
        let label = match id {
            "a" => "Alpha",
            "b" => "Bravo",
            _   => "?",
        };
        Ok(XPathValue::String(label.into()))
    });

    // `string(@id)` coerces the attribute node-set to a string at the
    // XPath layer before dispatching the extension call.
    let xsl = r#"<xsl:stylesheet version="1.0"
                                 xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                                 xmlns:l="urn:lookup"
                                 exclude-result-prefixes="l">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/r">
            <out label="{l:label(string(@id))}"/>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = render(xsl, r#"<r id="a"/>"#, &exts);
    assert_eq!(out, r#"<out label="Alpha"/>"#);
}

/// Multiple namespaces / multiple registrations coexist.
#[test]
fn multiple_namespaces_registered() {
    let mut exts = Extensions::new();
    exts.register("urn:math", "square", |args| {
        let n = if let Some(XPathValue::Number(n)) = args.first() { n.as_f64() } else { 0.0 };
        Ok(XPathValue::Number(Numeric::Double(n * n)))
    });
    exts.register("urn:str", "shout", |args| {
        let s = if let Some(XPathValue::String(s)) = args.first() { s.clone() } else { String::new() };
        Ok(XPathValue::String(s.to_uppercase()))
    });

    let xsl = r#"<xsl:stylesheet version="1.0"
                                 xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                                 xmlns:m="urn:math"
                                 xmlns:s="urn:str"
                                 exclude-result-prefixes="m s">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/r">
            <out>
                <sq><xsl:value-of select="m:square(7)"/></sq>
                <up><xsl:value-of select="s:shout('hi')"/></up>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = render(xsl, "<r/>", &exts);
    // Strip whitespace for stable comparison.
    assert!(out.contains("<sq>49</sq>"), "got: {out}");
    assert!(out.contains("<up>HI</up>"), "got: {out}");
}

/// Returning `Some(Err(_))` from an extension surfaces as a runtime
/// `XsltError` rather than silently being ignored.
#[test]
fn extension_error_propagates() {
    let mut exts = Extensions::new();
    exts.register("urn:fail", "bang", |_args| {
        Err(ext_err("boom from extension"))
    });

    let xsl = r#"<xsl:stylesheet version="1.0"
                                 xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                                 xmlns:f="urn:fail">
        <xsl:template match="/r">
            <out><xsl:value-of select="f:bang()"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let style = Stylesheet::compile_str(xsl).expect("compile");
    let doc = parse_str("<r/>", &ParseOptions::default()).expect("parse");
    let err = style.apply_with_extensions(&doc, &exts)
        .expect_err("error from extension must surface");
    let msg = format!("{err}");
    assert!(msg.contains("boom"), "expected extension error to propagate, got: {msg}");
}

/// Unknown (ns, name) → engine continues its fallback chain.  Since
/// no native function matches `urn:nope:missing`, the call must
/// produce an error rather than silently returning empty.
#[test]
fn unknown_extension_falls_through_to_unknown_function_error() {
    let exts = Extensions::new();
    let xsl = r#"<xsl:stylesheet version="1.0"
                                 xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                                 xmlns:n="urn:nope">
        <xsl:template match="/r">
            <out><xsl:value-of select="n:missing()"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let style = Stylesheet::compile_str(xsl).expect("compile");
    let doc = parse_str("<r/>", &ParseOptions::default()).expect("parse");
    let err = style.apply_with_extensions(&doc, &exts)
        .expect_err("unknown extension must error");
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("function") || msg.contains("missing") || msg.contains("nope"),
        "expected unknown-function diagnostic, got: {msg}",
    );
}

/// Built-in EXSLT functions still resolve when `Extensions` is
/// supplied — the extension hook is consulted but its `None` return
/// must not block native dispatch.
#[test]
fn native_exslt_still_resolves_alongside_extensions() {
    let mut exts = Extensions::new();
    // Register something unrelated to prove the hook is wired and
    // *also* check native EXSLT works.
    exts.register("urn:noop", "x", |_| Ok(XPathValue::Number(Numeric::Double(0.0))));

    let xsl = r#"<xsl:stylesheet version="1.0"
                                 xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                                 xmlns:math="http://exslt.org/math"
                                 exclude-result-prefixes="math">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/r">
            <out><xsl:value-of select="math:max(item)"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let src = "<r><item>1</item><item>5</item><item>3</item></r>";
    let out = render(xsl, src, &exts);
    assert_eq!(out, "<out>5</out>");
}

/// Custom `ExtensionFunctions` impl (not the `Extensions` builder)
/// also works.  Models the "stateful registry" use case.
#[test]
fn custom_extension_functions_impl() {
    struct Counter(std::cell::Cell<u32>);
    impl ExtensionFunctions for Counter {
        fn call(
            &self,
            ns_uri: &str,
            name:   &str,
            _args:  Vec<XPathValue>,
        ) -> Option<Result<XPathValue, XmlError>> {
            if ns_uri == "urn:counter" && name == "next" {
                let n = self.0.get() + 1;
                self.0.set(n);
                return Some(Ok(XPathValue::Number(Numeric::Double(n as f64))));
            }
            None
        }
    }

    let counter = Counter(std::cell::Cell::new(0));
    let xsl = r#"<xsl:stylesheet version="1.0"
                                 xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
                                 xmlns:c="urn:counter"
                                 exclude-result-prefixes="c">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/r">
            <out>
                <xsl:for-each select="item">
                    <n><xsl:value-of select="c:next()"/></n>
                </xsl:for-each>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let src = "<r><item/><item/><item/></r>";
    let out = render(xsl, src, &counter);
    assert!(out.contains("<n>1</n>") && out.contains("<n>2</n>") && out.contains("<n>3</n>"),
        "expected counter to fire three times, got: {out}");
}
