//! Adversarial schema and instance inputs for the XSD subsystem.
//!
//! Mirrors `attacks.rs` (which covers the XML parser) for the XSD
//! compiler + validator.  Each test runs under a wall-clock timeout on
//! a worker thread so a regression that introduces an unbounded loop
//! or stack blowup surfaces as a loud test failure rather than a
//! silent CI hang.
//!
//! Two failure modes are tested explicitly:
//!
//! 1. **Compile-time** — pathological schemas that should either be
//!    rejected, or compile in bounded time/memory.
//! 2. **Validate-time** — large or pathological instances against
//!    benign schemas; validation must complete within budget.
//!
//! Inputs are constructed in code (not files) — the schemas are
//! small but the generated strings can be large.

#![cfg(feature = "xsd")]

use std::panic::AssertUnwindSafe;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use sup_xml::xsd::Schema;

const TIMEOUT_SECS: u64 = 10;

#[derive(Debug)]
enum Outcome {
    Ok,
    Err(String),
    Panicked,
    TimedOut,
}

/// Compile a schema string under a timeout, catching panics.  Owns
/// `src` so the worker thread doesn't need to share a borrow.
fn compile_guarded(src: String) -> Outcome {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
            Schema::compile_str(&src).map(|_| ()).map_err(|e| e.to_string())
        }));
        let _ = tx.send(res);
    });
    match rx.recv_timeout(Duration::from_secs(TIMEOUT_SECS)) {
        Ok(Ok(Ok(())))  => Outcome::Ok,
        Ok(Ok(Err(m)))  => Outcome::Err(m),
        Ok(Err(_panic)) => Outcome::Panicked,
        Err(_)          => Outcome::TimedOut,
    }
}

/// Compile + validate under a timeout.  Returns the OUTCOME of
/// validation; schema-compile failures are reported as Err with a
/// "compile:" prefix so tests can distinguish.
fn validate_guarded(schema_src: String, instance_src: String) -> Outcome {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let s = Schema::compile_str(&schema_src)
                .map_err(|e| format!("compile: {e}"))?;
            s.validate_str(&instance_src).map_err(|e| e.to_string())
        }));
        let _ = tx.send(res);
    });
    match rx.recv_timeout(Duration::from_secs(TIMEOUT_SECS)) {
        Ok(Ok(Ok(())))  => Outcome::Ok,
        Ok(Ok(Err(m)))  => Outcome::Err(m),
        Ok(Err(_panic)) => Outcome::Panicked,
        Err(_)          => Outcome::TimedOut,
    }
}

fn xsd_wrap(body: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
           targetNamespace="urn:test"
           xmlns="urn:test"
           elementFormDefault="qualified">
{body}
</xs:schema>"#
    )
}

fn instance_wrap(body: &str) -> String {
    format!(r#"{body}"#)
}

fn assert_no_hang_no_panic(label: &str, o: Outcome) {
    match o {
        Outcome::Ok | Outcome::Err(_) => {}
        Outcome::Panicked => panic!("{label}: PANICKED — must return Err, not panic"),
        Outcome::TimedOut => panic!(
            "{label}: HUNG past {TIMEOUT_SECS}s — possible DoS regression"
        ),
    }
}

fn assert_rejected(label: &str, o: Outcome) {
    match o {
        Outcome::Err(_) => {}
        Outcome::Ok => panic!("{label}: expected error, got success"),
        Outcome::Panicked => panic!("{label}: PANICKED"),
        Outcome::TimedOut => panic!(
            "{label}: HUNG past {TIMEOUT_SECS}s — possible DoS regression"
        ),
    }
}

fn assert_accepted(label: &str, o: Outcome) {
    match o {
        Outcome::Ok => {}
        Outcome::Err(m)   => panic!("{label}: expected success, got error: {m}"),
        Outcome::Panicked => panic!("{label}: PANICKED"),
        Outcome::TimedOut => panic!(
            "{label}: HUNG past {TIMEOUT_SECS}s — possible DoS regression"
        ),
    }
}

// ═══ Cyclic type references ═══════════════════════════════════════════════

#[test]
fn cyclic_complex_extension_chain_does_not_hang() {
    // A extends B, B extends A.  Must be rejected (or compile bounded)
    // — the merge_extension_chains post-pass walks the chain.
    let schema = xsd_wrap(r#"
        <xs:complexType name="A">
            <xs:complexContent>
                <xs:extension base="B">
                    <xs:sequence><xs:element name="a" type="xs:string"/></xs:sequence>
                </xs:extension>
            </xs:complexContent>
        </xs:complexType>
        <xs:complexType name="B">
            <xs:complexContent>
                <xs:extension base="A">
                    <xs:sequence><xs:element name="b" type="xs:string"/></xs:sequence>
                </xs:extension>
            </xs:complexContent>
        </xs:complexType>
    "#);
    assert_rejected("cyclic A↔B extension", compile_guarded(schema));
}

#[test]
fn self_extension_does_not_hang() {
    let schema = xsd_wrap(r#"
        <xs:complexType name="Self">
            <xs:complexContent>
                <xs:extension base="Self">
                    <xs:sequence><xs:element name="x" type="xs:string"/></xs:sequence>
                </xs:extension>
            </xs:complexContent>
        </xs:complexType>
    "#);
    assert_rejected("self-extension", compile_guarded(schema));
}

// ═══ Recursive type references (legitimate, common in tree data) ══════════

#[test]
fn self_referencing_element_type_validates() {
    // Common tree pattern: <node> contains <node>* children.  Must
    // compile and validate without infinite recursion.
    let schema = xsd_wrap(r#"
        <xs:element name="node" type="Node"/>
        <xs:complexType name="Node">
            <xs:sequence>
                <xs:element name="node" type="Node" minOccurs="0" maxOccurs="unbounded"/>
            </xs:sequence>
        </xs:complexType>
    "#);
    let instance = instance_wrap(r#"
        <node xmlns="urn:test">
            <node>
                <node/>
                <node><node/></node>
            </node>
            <node/>
        </node>
    "#);
    assert_accepted("self-referencing tree", validate_guarded(schema, instance));
}

// ═══ Deeply nested types ══════════════════════════════════════════════════

#[test]
fn deeply_nested_anonymous_complex_types_compile() {
    // 50 levels of inline <xs:complexType><xs:sequence><xs:element>...
    // Recursive structure — each level wraps the previous.
    let depth = 50;
    let mut body = String::new();
    for i in 0..depth {
        body.push_str(&format!(
            r#"<xs:element name="lvl{i}"><xs:complexType><xs:sequence>"#
        ));
    }
    body.push_str(r#"<xs:element name="leaf" type="xs:string"/>"#);
    for _ in 0..depth {
        body.push_str(r#"</xs:sequence></xs:complexType></xs:element>"#);
    }
    let schema = xsd_wrap(&body);
    assert_no_hang_no_panic("50-deep anonymous types", compile_guarded(schema));
}

// ═══ maxOccurs="unbounded" with huge instances ════════════════════════════

#[test]
fn unbounded_repetition_huge_instance_validates() {
    // 50_000 repetitions of a simple element — must validate in
    // linear time without stack blowup.
    let schema = xsd_wrap(r#"
        <xs:element name="root">
            <xs:complexType>
                <xs:sequence>
                    <xs:element name="item" type="xs:string"
                                minOccurs="0" maxOccurs="unbounded"/>
                </xs:sequence>
            </xs:complexType>
        </xs:element>
    "#);
    let n = 50_000;
    let mut instance = String::with_capacity(n * 16);
    instance.push_str(r#"<root xmlns="urn:test">"#);
    for _ in 0..n {
        instance.push_str("<item>x</item>");
    }
    instance.push_str("</root>");
    assert_accepted("50k unbounded items", validate_guarded(schema, instance));
}

// ═══ Massive substitution groups ══════════════════════════════════════════

#[test]
fn massive_substitution_group_compiles() {
    // 500 elements substituting for one head.  DFA-build cost must
    // be reasonable (this used to be quadratic in some implementations).
    let n = 500;
    let mut body = String::from(r#"<xs:element name="head" type="xs:string" abstract="true"/>"#);
    for i in 0..n {
        body.push_str(&format!(
            r#"<xs:element name="sub{i}" type="xs:string" substitutionGroup="head"/>"#
        ));
    }
    body.push_str(r#"
        <xs:element name="doc">
            <xs:complexType>
                <xs:sequence>
                    <xs:element ref="head" maxOccurs="unbounded"/>
                </xs:sequence>
            </xs:complexType>
        </xs:element>
    "#);
    let schema = xsd_wrap(&body);
    assert_no_hang_no_panic("500-member substitution group", compile_guarded(schema));
}

// ═══ Identity-constraint pathology ════════════════════════════════════════

#[test]
fn identity_constraint_huge_selector_set_validates() {
    // 10k matched elements — uniqueness check must be O(n) in tuple
    // count (HashMap-based), not O(n²).
    let schema = xsd_wrap(r#"
        <xs:element name="root">
            <xs:complexType>
                <xs:sequence>
                    <xs:element name="item" maxOccurs="unbounded">
                        <xs:complexType>
                            <xs:attribute name="id" type="xs:string" use="required"/>
                        </xs:complexType>
                    </xs:element>
                </xs:sequence>
            </xs:complexType>
            <xs:unique name="u">
                <xs:selector xpath=".//item"/>
                <xs:field xpath="@id"/>
            </xs:unique>
        </xs:element>
    "#);
    let n = 10_000;
    let mut instance = String::with_capacity(n * 24);
    instance.push_str(r#"<root xmlns="urn:test">"#);
    for i in 0..n {
        instance.push_str(&format!(r#"<item id="i{i}"/>"#));
    }
    instance.push_str("</root>");
    assert_accepted("10k identity tuples", validate_guarded(schema, instance));
}

// ═══ Enormous enumeration ═════════════════════════════════════════════════

#[test]
fn enormous_enumeration_validates() {
    // 5k enumeration values — lookup must terminate in reasonable
    // time per element.  (Currently linear; this test mostly guards
    // against accidental O(n²) — e.g. recompiling the enum list per
    // validation.)
    let n = 5_000;
    let mut enums = String::new();
    for i in 0..n {
        enums.push_str(&format!(r#"<xs:enumeration value="v{i}"/>"#));
    }
    let schema = xsd_wrap(&format!(r#"
        <xs:simpleType name="Big">
            <xs:restriction base="xs:string">
                {enums}
            </xs:restriction>
        </xs:simpleType>
        <xs:element name="v" type="Big"/>
    "#));
    let mut instance = String::from(r#"<v xmlns="urn:test">v4999</v>"#);
    instance.push_str("");
    assert_accepted("5k enumeration accept", validate_guarded(schema, instance));
}

// ═══ Empty / degenerate ═══════════════════════════════════════════════════

#[test]
fn empty_schema_compiles() {
    let schema = xsd_wrap("");
    assert_no_hang_no_panic("empty schema", compile_guarded(schema));
}

#[test]
fn schema_with_only_annotation_compiles() {
    let schema = xsd_wrap(r#"<xs:annotation><xs:documentation>hi</xs:documentation></xs:annotation>"#);
    assert_no_hang_no_panic("annotation-only schema", compile_guarded(schema));
}

// ═══ Pathological pattern facet ═══════════════════════════════════════════

#[test]
fn catastrophic_backtracking_pattern_does_not_hang_validation() {
    // Classic catastrophic-backtracking regex `(a+)+$` against a long
    // input.  We expect EITHER the regex to be rejected at schema
    // compile time, OR validation to complete (the XSD regex engine
    // should be DFA-based or otherwise bounded).
    let schema = xsd_wrap(r#"
        <xs:simpleType name="Pat">
            <xs:restriction base="xs:string">
                <xs:pattern value="(a+)+$"/>
            </xs:restriction>
        </xs:simpleType>
        <xs:element name="v" type="Pat"/>
    "#);
    // 30 'a's followed by '!': bad case for backtracking implementations.
    let bad = "a".repeat(30) + "!";
    let instance = instance_wrap(&format!(r#"<v xmlns="urn:test">{bad}</v>"#));
    assert_no_hang_no_panic("catastrophic backtracking pattern",
        validate_guarded(schema, instance));
}

// ═══ xs:redefine cycle ═══════════════════════════════════════════════════

#[test]
fn redefine_self_cycle_does_not_hang() {
    // A schema that <xs:redefine>s itself.  The `loaded` cycle-detector
    // in handle_redefine/load_schema_via_resolver should catch it.
    use sup_xml::xsd::InMemoryResolver;
    let outer = r#"<?xml version="1.0"?>
        <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
                   targetNamespace="urn:test"
                   xmlns="urn:test">
            <xs:redefine schemaLocation="self.xsd"/>
            <xs:element name="x" type="xs:string"/>
        </xs:schema>"#;
    let resolver = InMemoryResolver::new()
        .with("self.xsd", outer.as_bytes().to_vec());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let res = std::panic::catch_unwind(AssertUnwindSafe(|| {
            Schema::compile_with(outer, resolver).map(|_| ()).map_err(|e| e.to_string())
        }));
        let _ = tx.send(res);
    });
    match rx.recv_timeout(Duration::from_secs(TIMEOUT_SECS)) {
        Ok(Ok(Ok(()))) | Ok(Ok(Err(_))) => {}
        Ok(Err(_)) => panic!("redefine self-cycle PANICKED"),
        Err(_)     => panic!(
            "redefine self-cycle HUNG past {TIMEOUT_SECS}s — cycle detection broken"
        ),
    }
}
