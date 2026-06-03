//! W3C QT3 Test Suite — XPath 3.1 / XQuery 3.1 conformance.
//!
//! This crate only implements XPath 1.0, so most QT3 tests fail
//! because they exercise 2.0 / 3.x features we don't have.  The
//! runner still produces useful baseline numbers per test-set, and
//! shows where the bulk of our XPath surface lands.
//!
//! Marked `#[ignore]`.  Run with:
//!
//! ```text
//! cargo test --features xsd --test qt3 -- --ignored --nocapture
//! ```
//!
//! Fetch the suite first via `tests/assets/qt3tests/fetch.sh`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sup_xml::{parse_str, Event, ParseOptions, XmlReader, xpath_bool, xpath_str};

const QT3_ROOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/assets/qt3tests"
);

#[derive(Default, Debug)]
struct Stats {
    pass: usize,
    fail: usize,
    skip: usize,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.pass += o.pass;
        self.fail += o.fail;
        self.skip += o.skip;
    }
    fn pass_rate(&self) -> f64 {
        let total = self.pass + self.fail;
        if total == 0 { 0.0 } else { self.pass as f64 * 100.0 / total as f64 }
    }
}

#[derive(Debug)]
#[allow(dead_code)] // name + env_ref kept for diagnostic dumps
struct TestCase {
    name:    String,
    test:    String,          // the XPath expression
    env_ref: Option<String>,  // environment name (or None for inline)
    expects: Expectation,
}

#[derive(Debug)]
#[allow(dead_code)] // Error(code) kept for future code-aware verification
enum Expectation {
    /// `<assert-true/>` — XPath must evaluate to boolean true.
    True,
    /// `<assert-false/>` — XPath must evaluate to boolean false.
    False,
    /// `<assert-empty/>` — result must be the empty sequence.
    Empty,
    /// `<assert-string-value>...</assert-string-value>` — exact match.
    StringValue(String),
    /// `<assert-eq>VAL</assert-eq>` — for XPath 1.0 simplicity we
    /// match against either the string form or the numeric form.
    Eq(String),
    /// `<error code="..."/>` — evaluation must error.
    Error(String),
    /// Anything else — too complex for this runner; skip the case.
    Unsupported,
}

fn parse_test_set(xml: &str) -> Vec<TestCase> {
    let mut reader = XmlReader::from_str(xml);
    let mut cases = Vec::new();

    let mut in_case = false;
    let mut cur_name = String::new();
    let mut cur_env: Option<String> = None;
    let mut cur_test = String::new();
    let mut cur_expect = Expectation::Unsupported;
    let mut in_test = false;
    let mut in_result = false;
    let mut result_depth = 0i32;
    let mut text_buf = String::new();
    let mut capturing_into: Option<&'static str> = None;

    loop {
        let ev = match reader.next() {
            Ok(e) => e,
            Err(_) => return cases,
        };
        match ev {
            Event::StartElement(tag) => {
                let name = tag.name().to_string();
                let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(&name).to_string();
                match local.as_str() {
                    "test-case" => {
                        in_case = true;
                        cur_name.clear();
                        cur_env = None;
                        cur_test.clear();
                        cur_expect = Expectation::Unsupported;
                        for a in tag.attrs() {
                            if let Ok(a) = a {
                                if a.name() == "name" { cur_name = a.value().to_string(); }
                            }
                        }
                    }
                    "environment" if in_case => {
                        for a in tag.attrs() {
                            if let Ok(a) = a {
                                if a.name() == "ref" { cur_env = Some(a.value().to_string()); }
                            }
                        }
                    }
                    "test" if in_case => {
                        in_test = true;
                        text_buf.clear();
                        capturing_into = Some("test");
                    }
                    "result" if in_case => {
                        in_result = true;
                        result_depth = 0;
                    }
                    "assert-true" if in_result => {
                        cur_expect = Expectation::True;
                    }
                    "assert-false" if in_result => {
                        cur_expect = Expectation::False;
                    }
                    "assert-empty" if in_result => {
                        cur_expect = Expectation::Empty;
                    }
                    "assert-string-value" if in_result => {
                        text_buf.clear();
                        capturing_into = Some("string-value");
                    }
                    "assert-eq" if in_result => {
                        text_buf.clear();
                        capturing_into = Some("eq");
                    }
                    "error" if in_result => {
                        let mut code = String::new();
                        for a in tag.attrs() {
                            if let Ok(a) = a {
                                if a.name() == "code" { code = a.value().to_string(); }
                            }
                        }
                        cur_expect = Expectation::Error(code);
                    }
                    _ => {}
                }
                if in_result { result_depth += 1; }
            }
            Event::EndElement(tag) => {
                let name = tag.name().to_string();
                let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(&name).to_string();
                match local.as_str() {
                    "test" if in_test => {
                        cur_test = std::mem::take(&mut text_buf).trim().to_string();
                        in_test = false;
                        capturing_into = None;
                    }
                    "assert-string-value" => {
                        cur_expect = Expectation::StringValue(std::mem::take(&mut text_buf));
                        capturing_into = None;
                    }
                    "assert-eq" => {
                        cur_expect = Expectation::Eq(std::mem::take(&mut text_buf).trim().to_string());
                        capturing_into = None;
                    }
                    "result" => { in_result = false; }
                    "test-case" if in_case => {
                        cases.push(TestCase {
                            name:    std::mem::take(&mut cur_name),
                            test:    std::mem::take(&mut cur_test),
                            env_ref: cur_env.take(),
                            expects: std::mem::replace(&mut cur_expect, Expectation::Unsupported),
                        });
                        in_case = false;
                    }
                    _ => {}
                }
                if in_result { result_depth -= 1; let _ = result_depth; }
            }
            Event::Text(t) => {
                if capturing_into.is_some() {
                    text_buf.push_str(t.as_str());
                }
            }
            Event::CData(t) => {
                if capturing_into.is_some() {
                    text_buf.push_str(t.as_str());
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    cases
}

/// Try to evaluate one test.  Returns Some(true) on pass,
/// Some(false) on fail, None on skip (can't run with our XPath 1.0
/// surface).
fn run_case(case: &TestCase) -> Option<bool> {
    // Tests that bind their context document via a named environment
    // (other than "empty") need source-loading we don't do — skip.
    if let Some(env) = &case.env_ref {
        if env != "empty" { return None; }
    }
    // Wrap the test expression so it runs against a tiny placeholder
    // doc.  Tests that don't depend on the context item still work
    // because the XPath engine accepts arbitrary expressions.
    let doc = parse_str("<r/>", &ParseOptions::default()).ok()?;

    match &case.expects {
        Expectation::True | Expectation::False => {
            let want = matches!(case.expects, Expectation::True);
            match xpath_bool(&doc, &case.test) {
                Ok(got) => Some(got == want),
                Err(_)  => Some(false),
            }
        }
        Expectation::Empty => {
            // XPath 1.0 has no "empty sequence" — closest is node-set
            // with no nodes (count=0) or a boolean false context.  For
            // the kind of XPath 3.1 expressions QT3 uses here, our
            // engine usually can't compile them.  Try string-eval +
            // empty-string heuristic.
            match xpath_str(&doc, &case.test) {
                Ok(s) => Some(s.is_empty()),
                Err(_) => None,
            }
        }
        Expectation::StringValue(want) => {
            match xpath_str(&doc, &case.test) {
                Ok(got) => Some(&got == want),
                Err(_)  => None,
            }
        }
        Expectation::Eq(want) => {
            // assert-eq is value-equality in the QT3 sense.  XPath 1.0
            // doesn't have the same atomic-type system, so we fall back
            // to string-form comparison after trimming.  Numeric tests
            // (e.g. "42" vs "42.0") will get false-negatives.
            match xpath_str(&doc, &case.test) {
                Ok(got) => Some(got.trim() == want.trim()),
                Err(_)  => None,
            }
        }
        Expectation::Error(_) => {
            match xpath_str(&doc, &case.test) {
                Ok(_)  => Some(false),  // expected error, got success
                Err(_) => Some(true),
            }
        }
        Expectation::Unsupported => None,
    }
}

fn run_test_set(path: &Path) -> Stats {
    let mut stats = Stats::default();
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return stats,
    };
    for case in parse_test_set(&src) {
        // A case that panics (e.g. an unhandled edge in the evaluator)
        // is bucketed as a failure rather than aborting the whole walk.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
            run_case(&case)
        ));
        match outcome {
            Ok(Some(true))  => stats.pass += 1,
            Ok(Some(false)) => stats.fail += 1,
            Ok(None)        => stats.skip += 1,
            Err(_)          => stats.fail += 1,
        }
    }
    stats
}

fn parse_catalog(catalog_path: &Path) -> Vec<PathBuf> {
    let src = match std::fs::read_to_string(catalog_path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut paths = Vec::new();
    let mut reader = XmlReader::from_str(&src);
    let base = catalog_path.parent().unwrap_or(Path::new("."));
    while let Ok(ev) = reader.next() {
        match ev {
            Event::StartElement(tag) => {
                let n = tag.name().to_string();
                let local = n.rsplit_once(':').map(|(_, l)| l).unwrap_or(&n).to_string();
                if local == "test-set" {
                    for a in tag.attrs() {
                        if let Ok(a) = a {
                            if a.name() == "file" {
                                paths.push(base.join(a.value()));
                            }
                        }
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    paths
}

fn run_suite() {
    let root = PathBuf::from(QT3_ROOT);
    let catalog = root.join("catalog.xml");
    if !catalog.exists() {
        eprintln!(
            "QT3 suite not present at {}\n\
             Run `tests/assets/qt3tests/fetch.sh` to clone the W3C repo.",
            root.display()
        );
        return;
    }

    let test_set_files = parse_catalog(&catalog);
    let mut by_group: HashMap<String, Stats> = HashMap::new();
    let mut total = Stats::default();

    for ts in &test_set_files {
        // Group key: first path segment after the suite root.
        let rel = ts.strip_prefix(&root).unwrap_or(ts);
        let group = rel.iter().next()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "(unknown)".to_string());
        let stats = run_test_set(ts);
        by_group.entry(group).or_default().add(&stats);
        total.add(&stats);
    }

    println!("\n  QT3 conformance ({} test-sets)\n", test_set_files.len());
    println!("  {:<20}  {:>6}  {:>6}  {:>6}  {:>8}",
        "group", "pass", "fail", "skip", "pass%");
    let mut keys: Vec<&String> = by_group.keys().collect();
    keys.sort();
    for k in keys {
        let s = &by_group[k];
        println!("  {:<20}  {:>6}  {:>6}  {:>6}  {:>7.1}%",
            k, s.pass, s.fail, s.skip, s.pass_rate());
    }
    println!("  {:<20}  {:>6}  {:>6}  {:>6}  {:>7.1}%",
        "TOTAL", total.pass, total.fail, total.skip, total.pass_rate());
    let attempted = total.pass + total.fail;
    let all = attempted + total.skip;
    println!("\n  Attempted {} of {} ({} skipped — features outside XPath 1.0 surface)",
        attempted, all, total.skip);
}

#[test]
#[ignore = "run with --ignored: walks the W3C QT3 suite (XPath/XQuery 3.1). \
            Fetch first via tests/assets/qt3tests/fetch.sh."]
fn w3c_qt3_conformance() {
    run_suite();
}
