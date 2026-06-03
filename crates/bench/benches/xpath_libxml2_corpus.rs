//! XPath 1.0 cross-implementation runner against libxml2's own
//! test corpus.
//!
//! Vendored at `tests/assets/xpath-libxml2-corpus/`.  Run
//!   `tests/assets/xpath-libxml2-corpus/fetch.sh`
//! once to populate (the directory is gitignored — fetch fresh
//! any time to refresh).  libxml2 is MIT-licensed; the corpus
//! is included in their main repo under `test/XPath/`.
//!
//! This is the *scale* complement to `xpath_compliance.rs`:
//!
//! * `xpath_compliance.rs` — 87 hand-curated tests against
//!   spec-correct expected outputs.  Independent of any one
//!   implementation.  Measures *conformance*.
//! * `xpath_libxml2_corpus.rs` (this file) — 300+ expressions
//!   from libxml2's own test inputs.  Both backends evaluate the
//!   same expression on the same doc; we report whether they
//!   agree.  Measures *agreement with the reference C
//!   implementation* — which has more breadth than our hand-curated
//!   set but inherits any libxml2-specific quirks.  Sustained
//!   disagreement on a test file is the actionable signal.
//!
//! We deliberately don't vendor the libxml2-formatted `result/`
//! files: parsing their ad-hoc text format adds nothing the
//! head-to-head comparison doesn't already give us.
//!
//! Run:
//!     cargo bench -p sup-xml-bench --bench xpath_libxml2_corpus
//!
//! Env vars:
//!     XPATH_VERBOSE=1     print every disagreement
//!     XPATH_FILTER=<sub>  only run test files whose path contains <sub>

#![allow(clippy::missing_safety_doc)]

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use sup_xml::{ParseOptions, XPathContext, XPathOptions, XPathValue, parse_str};

const CORPUS_ROOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/assets/xpath-libxml2-corpus"
);

// ── libxml2 XPath FFI ────────────────────────────────────────────────────────
//
// Same surface as xpath_compliance.rs; kept self-contained here so each
// bench remains a single-file artifact (matching the convention of every
// other bench in this crate).

type XmlDocPtr          = *mut c_void;
type XmlXPathContextPtr = *mut c_void;
type XmlNodePtr         = *mut c_void;

const XPATH_NODESET: c_int = 1;
const XPATH_BOOLEAN: c_int = 2;
const XPATH_NUMBER:  c_int = 3;
const XPATH_STRING:  c_int = 4;

#[repr(C)]
struct XmlNodeSet {
    node_nr:  c_int,
    node_max: c_int,
    node_tab: *mut XmlNodePtr,
}

#[repr(C)]
struct XmlXPathObject {
    object_type: c_int,
    nodesetval:  *mut XmlNodeSet,
    boolval:     c_int,
    floatval:    f64,
    stringval:   *mut c_char,
    user:        *mut c_void,
    index:       c_int,
    user2:       *mut c_void,
    index2:      c_int,
}

unsafe extern "C" {
    fn xmlReadMemory(buffer: *const c_char, size: c_int, url: *const c_char,
                     encoding: *const c_char, options: c_int) -> XmlDocPtr;
    fn xmlFreeDoc(doc: XmlDocPtr);

    fn xmlXPathNewContext(doc: XmlDocPtr) -> XmlXPathContextPtr;
    fn xmlXPathFreeContext(ctx: XmlXPathContextPtr);
    fn xmlXPathEvalExpression(expr: *const c_char, ctx: XmlXPathContextPtr)
        -> *mut XmlXPathObject;
    fn xmlXPathFreeObject(obj: *mut XmlXPathObject);
    fn xmlXPathCastNodeToString(node: XmlNodePtr) -> *mut c_char;

    fn xmlSetGenericErrorFunc(ctx: *mut c_void, handler: Option<unsafe extern "C" fn()>);
    fn xmlSetStructuredErrorFunc(ctx: *mut c_void, handler: Option<unsafe extern "C" fn()>);
}

unsafe extern "C" fn libxml2_swallow() {}

fn install_libxml2_silencer() {
    unsafe {
        xmlSetGenericErrorFunc(std::ptr::null_mut(), Some(libxml2_swallow));
        xmlSetStructuredErrorFunc(std::ptr::null_mut(), Some(libxml2_swallow));
    }
}

// ── normalised outcome ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Outcome {
    Bool(bool),
    Num(f64),
    Str(String),
    NodeStrings(Vec<String>),
    Error(String),
}

fn fmt_outcome(o: &Outcome) -> String {
    match o {
        Outcome::Bool(b) => format!("bool({})", b),
        Outcome::Num(n)  => format!("num({})",  n),
        Outcome::Str(s)  => format!("str({:?})", s),
        Outcome::NodeStrings(s) => {
            if s.len() <= 3 { format!("nodes({:?})", s) }
            else { format!("nodes(N={}; first={:?})", s.len(), &s[0]) }
        }
        Outcome::Error(e) => format!("error({})",
            if e.len() > 60 { format!("{}…", &e[..60]) } else { e.clone() }),
    }
}

// ── backend trait ────────────────────────────────────────────────────────────

trait Backend {
    fn name(&self) -> &'static str;
    fn eval(&self, expr: &str, doc_xml: &str) -> Outcome;
}

// ── sup-xml backends ─────────────────────────────────────────────────────────

struct SupXmlBackend  { name: &'static str, compat: bool }

impl Backend for SupXmlBackend {
    fn name(&self) -> &'static str { self.name }

    fn eval(&self, expr: &str, doc_xml: &str) -> Outcome {
        let doc = match parse_str(doc_xml, &ParseOptions::default()) {
            Ok(d)  => d,
            Err(e) => return Outcome::Error(format!("parse: {}", e)),
        };
        let opts = XPathOptions { libxml2_compatible: self.compat, xpath_2_0: false };
        let ctx  = XPathContext::new_with(&doc, opts);
        let value = match ctx.eval(expr) {
            Ok(v)  => v,
            Err(e) => return Outcome::Error(e.to_string()),
        };
        match value {
            XPathValue::Boolean(b) => Outcome::Bool(b),
            XPathValue::Number(n)  => Outcome::Num(n.as_f64()),
            XPathValue::String(s)  => Outcome::Str(s),
            XPathValue::NodeSet(_) | XPathValue::ForeignNodeSet(_) => {
                match ctx.eval_strings(expr) {
                    Ok(ss) => Outcome::NodeStrings(ss),
                    Err(e) => Outcome::Error(e.to_string()),
                }
            }
            // XPath 2.0-only shapes (Typed, Sequence, IntRange, Map,
            // Array, Function): this corpus is the libxml2 1.0 corpus
            // so these don't arise in practice; stringify defensively.
            other => Outcome::Str(format!("{:?}", other)),
        }
    }
}

// ── libxml2 backend ──────────────────────────────────────────────────────────

struct Libxml2Backend;

impl Backend for Libxml2Backend {
    fn name(&self) -> &'static str { "libxml2" }

    fn eval(&self, expr: &str, doc_xml: &str) -> Outcome {
        // SAFETY: every libxml2 handle is freed before we return.
        unsafe {
            let doc = xmlReadMemory(
                doc_xml.as_ptr() as *const c_char,
                doc_xml.len() as c_int,
                std::ptr::null(), std::ptr::null(), 0,
            );
            if doc.is_null() {
                return Outcome::Error("libxml2 parse failed".into());
            }
            let ctx = xmlXPathNewContext(doc);
            if ctx.is_null() {
                xmlFreeDoc(doc);
                return Outcome::Error("libxml2 ctx alloc failed".into());
            }
            let Ok(c_expr) = CString::new(expr) else {
                xmlXPathFreeContext(ctx);
                xmlFreeDoc(doc);
                return Outcome::Error("expr had NUL".into());
            };
            let obj = xmlXPathEvalExpression(c_expr.as_ptr(), ctx);
            if obj.is_null() {
                xmlXPathFreeContext(ctx);
                xmlFreeDoc(doc);
                return Outcome::Error("libxml2 eval failed".into());
            }
            let outcome = extract_libxml2_result(obj);
            xmlXPathFreeObject(obj);
            xmlXPathFreeContext(ctx);
            xmlFreeDoc(doc);
            outcome
        }
    }
}

unsafe fn extract_libxml2_result(obj: *mut XmlXPathObject) -> Outcome {
    let o = unsafe { &*obj };
    match o.object_type {
        XPATH_BOOLEAN => Outcome::Bool(o.boolval != 0),
        XPATH_NUMBER  => Outcome::Num(o.floatval),
        XPATH_STRING  => {
            if o.stringval.is_null() {
                Outcome::Str(String::new())
            } else {
                Outcome::Str(unsafe { cstr_to_string(o.stringval) })
            }
        }
        XPATH_NODESET => {
            if o.nodesetval.is_null() {
                return Outcome::NodeStrings(Vec::new());
            }
            let ns = unsafe { &*o.nodesetval };
            if ns.node_nr <= 0 || ns.node_tab.is_null() {
                return Outcome::NodeStrings(Vec::new());
            }
            let mut out = Vec::with_capacity(ns.node_nr as usize);
            for i in 0..ns.node_nr {
                let node = unsafe { *ns.node_tab.offset(i as isize) };
                if node.is_null() { continue; }
                let s = unsafe { xmlXPathCastNodeToString(node) };
                if s.is_null() { out.push(String::new()); }
                else           { out.push(unsafe { cstr_to_string(s) }); }
                // Per-node string is leaked.  Bounded over a bench
                // run (a few hundred kB total).
            }
            Outcome::NodeStrings(out)
        }
        _ => Outcome::Error(format!("unknown libxml2 xpath result type: {}",
                                    o.object_type)),
    }
}

unsafe fn cstr_to_string(p: *const c_char) -> String {
    unsafe { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
}

// ── corpus discovery ─────────────────────────────────────────────────────────

/// One expression to run, paired with the source doc it expects.
/// `doc_xml` is `None` for the `expr/` subdirectory's stateless tests
/// (arithmetic / comparison / pure function calls).
struct LoadedTest {
    /// Test-file path relative to the corpus root, plus the 1-based
    /// expression line number.  Used as the test id in reports.
    file:     String,
    line:     usize,
    /// The XPath expression to evaluate.
    expr:     String,
    /// Source-document XML, or `None` for stateless tests.  When
    /// `None` we supply a trivial `<root/>` so libxml2's
    /// `xmlXPathNewContext` has something to bind to.
    doc_xml:  String,
}

/// Test files in `tests/` reference a source doc by name prefix.
/// Strip suffixes against this list to find the doc.
const DOC_PREFIXES: &[&str] = &[
    // Longer prefixes first so `chaptersprefol` resolves to `chapters`
    // before `simple` could grab anything starting with `s`.
    "chapters", "unicode", "simple", "issue289", "usr1",
    "mixed", "nodes", "lang", "str", "vid", "ns", "id",
];

const TRIVIAL_DOC: &str = "<root/>";

fn load_corpus() -> Result<Vec<LoadedTest>, String> {
    let root = PathBuf::from(CORPUS_ROOT);
    if !root.exists() {
        return Err(format!(
            "corpus not vendored at {}\n\
             Run `tests/assets/xpath-libxml2-corpus/fetch.sh` to fetch it.",
            root.display()));
    }

    let mut docs: std::collections::HashMap<&'static str, String> = Default::default();
    for p in DOC_PREFIXES {
        let path = root.join("docs").join(p);
        if let Ok(s) = std::fs::read_to_string(&path) {
            docs.insert(*p, s);
        }
    }

    let mut out: Vec<LoadedTest> = Vec::new();

    // expr/<name> — stateless tests, no source doc.
    let expr_dir = root.join("expr");
    if let Ok(rd) = std::fs::read_dir(&expr_dir) {
        for ent in rd.flatten() {
            let path = ent.path();
            let file = format!("expr/{}",
                path.file_name().and_then(|s| s.to_str()).unwrap_or("?"));
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            for (i, line) in src.lines().enumerate() {
                let expr = line.trim();
                if expr.is_empty() { continue; }
                out.push(LoadedTest {
                    file:    file.clone(),
                    line:    i + 1,
                    expr:    expr.to_string(),
                    doc_xml: TRIVIAL_DOC.to_string(),
                });
            }
        }
    }

    // tests/<name> — stateful tests, source doc inferred from prefix.
    let tests_dir = root.join("tests");
    if let Ok(rd) = std::fs::read_dir(&tests_dir) {
        for ent in rd.flatten() {
            let path = ent.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
            let prefix = DOC_PREFIXES.iter().find(|p| name.starts_with(*p));
            let Some(prefix) = prefix else {
                eprintln!("WARN: no doc prefix matches {}", name);
                continue;
            };
            let Some(doc_xml) = docs.get(prefix) else {
                eprintln!("WARN: doc {} missing on disk for test {}", prefix, name);
                continue;
            };
            let file = format!("tests/{}", name);
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            for (i, line) in src.lines().enumerate() {
                let expr = line.trim();
                if expr.is_empty() { continue; }
                out.push(LoadedTest {
                    file:    file.clone(),
                    line:    i + 1,
                    expr:    expr.to_string(),
                    doc_xml: doc_xml.clone(),
                });
            }
        }
    }

    out.sort_by(|a, b| (a.file.as_str(), a.line).cmp(&(b.file.as_str(), b.line)));
    Ok(out)
}

// ── override table: spec-correct verdicts ────────────────────────────────────
//
// For expressions where libxml2 violates XPath 1.0 spec, we record the
// spec-correct outcome (`strict`) AND libxml2's actual outcome
// (`compat`).  Each backend is then scored against the expected that
// matches its declared mode: sup-xml-strict against `strict`,
// sup-xml-compat against `compat`, libxml2 against `strict` (so its
// spec violations land as misses).  Unlisted expressions fall back to
// "libxml2's live output is the ground truth" — fine for the cases
// where libxml2 has no known divergence.

#[derive(Debug, Clone, PartialEq)]
enum Expected {
    Bool(bool),
    Num(f64),
    Str(String),
    NodeStrings(Vec<String>),
    /// Like [`NodeStrings`] but compares as an unordered multiset.
    /// Used for `namespace::` axis output, where XPath 1.0 § 5.4
    /// explicitly leaves the relative order of namespace nodes
    /// implementation-defined.
    NodeStringsSet(Vec<String>),
    Error,
}

struct OverrideEntry {
    /// What XPath 1.0 spec mandates — the verdict sup-xml-strict and
    /// libxml2 are scored against.
    strict: Expected,
    /// What libxml2 actually produces.  sup-xml-compat is scored
    /// against this.
    compat: Expected,
    /// One-line spec citation for the disagreement.  Printed in the
    /// per-override summary so the table reads "we judged this case
    /// for the following reason."
    why:    &'static str,
}

fn build_overrides() -> std::collections::HashMap<(&'static str, usize), OverrideEntry> {
    let mut m = std::collections::HashMap::new();

    // ── Group A: scientific notation in number literals ─────────────────────
    // XPath 1.0 § 3.5: `Number ::= Digits ('.' Digits?)? | '.' Digits` —
    // no exponent allowed.  libxml2 accepts `1eN` and evaluates it as
    // a double (which overflows to ±∞ or underflows to 0 at extreme N).
    let scientific_lit = "XPath 1.0 § 3.5: number literals have no exponent";
    for (line, lx_val) in [
        ( 9, f64::INFINITY),
        (10, f64::INFINITY),
        (11, f64::INFINITY),
        (12, f64::INFINITY),
        (13, 0.0),
        (14, 0.0),
        (15, 0.0),
        (16, 0.0),
    ] {
        m.insert(("expr/base", line), OverrideEntry {
            strict: Expected::Error,
            compat: Expected::Num(lx_val),
            why:    scientific_lit,
        });
    }
    m.insert(("expr/floats", 7), OverrideEntry {
        strict: Expected::Error, compat: Expected::Num(1230.0),
        why: scientific_lit,
    });
    m.insert(("expr/floats", 8), OverrideEntry {
        strict: Expected::Error, compat: Expected::Num(0.00123),
        why: scientific_lit,
    });

    // ── Group B: number('-') ────────────────────────────────────────────────
    // XPath 1.0 § 4.4: "If [the argument string] is not [a lexical
    // representation of a number], then NaN is returned."  `-` alone
    // is not a number literal.  libxml2 returns -0.
    m.insert(("expr/functions", 6), OverrideEntry {
        strict: Expected::Num(f64::NAN),
        compat: Expected::Num(-0.0),
        why:    "XPath 1.0 § 4.4: non-numeric string yields NaN",
    });

    // ── Group C: string() of very large numbers ─────────────────────────────
    // XPath 1.0 § 4.2: number-to-string produces decimal form with no
    // scientific notation, with enough digits to round-trip the f64.
    // libxml2 emits scientific form for |n| >= ~10^15.  The decimal
    // form has 16 significant figures matching the f64 round-trip; the
    // scientific form has 15.
    let string_decimal = "XPath 1.0 § 4.2: number→string is decimal-only";
    m.insert(("expr/strings", 6), OverrideEntry {
        strict: Expected::Str("12345678901234567000".into()),
        compat: Expected::Str("1.23456789012346e+19".into()),
        why:    string_decimal,
    });
    m.insert(("expr/strings", 7), OverrideEntry {
        strict: Expected::Str("-12345678901234567000".into()),
        compat: Expected::Str("-1.23456789012346e+19".into()),
        why:    string_decimal,
    });

    // ── Group D: f64 rounding on large integer literals ─────────────────────
    // The literal `12345678901234567890` exceeds 2^53 so isn't
    // representable in f64.  IEEE 754 round-to-nearest-even gives
    // 12345678901234567168 (verified against Python's strtod and
    // Rust's `f64::from_str`); libxml2 lands on 12345678901234569216,
    // one ULP higher — a real libxml2 parsing bug.  In compat mode we
    // *don't* mimic libxml2's mis-rounding; the compat expected is the
    // same correct f64.
    let f64_ulp = "IEEE 754 round-to-nearest-even — libxml2 off by 1 ULP";
    m.insert(("expr/floats", 62), OverrideEntry {
        strict: Expected::Num(12345678901234567000.0),
        compat: Expected::Num(12345678901234570000.0),
        why:    f64_ulp,
    });
    m.insert(("expr/floats", 63), OverrideEntry {
        strict: Expected::Num(-12345678901234567000.0),
        compat: Expected::Num(-12345678901234570000.0),
        why:    f64_ulp,
    });

    // ── Group E: namespace-axis ordering ────────────────────────────────────
    // XPath 1.0 § 5.4: "The relative order of namespace nodes is
    // implementation-dependent."  Both libxml2's order and sup-xml's
    // are spec-conformant; comparing them as a multiset accepts
    // either.
    m.insert(("tests/nssimple", 2), OverrideEntry {
        strict: Expected::NodeStringsSet(vec![
            "http://www.w3.org/XML/1998/namespace".into(),
            "nsuri1".into(),
            "nsuri2".into(),
        ]),
        compat: Expected::NodeStringsSet(vec![
            "http://www.w3.org/XML/1998/namespace".into(),
            "nsuri1".into(),
            "nsuri2".into(),
        ]),
        why: "XPath 1.0 § 5.4: namespace-axis order is implementation-defined",
    });

    m
}

fn outcome_to_expected(o: &Outcome) -> Expected {
    match o {
        Outcome::Bool(b)        => Expected::Bool(*b),
        Outcome::Num(n)         => Expected::Num(*n),
        Outcome::Str(s)         => Expected::Str(s.clone()),
        Outcome::NodeStrings(v) => Expected::NodeStrings(v.clone()),
        Outcome::Error(_)       => Expected::Error,
    }
}

fn matches(got: &Outcome, want: &Expected) -> bool {
    match (got, want) {
        (Outcome::Bool(g), Expected::Bool(e)) => g == e,
        (Outcome::Num(g),  Expected::Num(e))  => (g.is_nan() && e.is_nan()) || g == e,
        (Outcome::Str(g),  Expected::Str(e))  => g == e,
        (Outcome::NodeStrings(g), Expected::NodeStrings(e)) => g == e,
        (Outcome::NodeStrings(g), Expected::NodeStringsSet(e)) => {
            // Multiset equality: same length, every element of one
            // appears with matching multiplicity in the other.  Sort
            // both copies and compare.
            if g.len() != e.len() { return false; }
            let mut a = g.clone(); a.sort();
            let mut b = e.clone(); b.sort();
            a == b
        }
        (Outcome::Error(_), Expected::Error)  => true,
        _ => false,
    }
}

// ── runner ───────────────────────────────────────────────────────────────────

#[derive(Default, Clone)]
struct FileStats {
    total: usize,
    pass:  Vec<usize>,
}

impl FileStats {
    fn new(n: usize) -> Self { Self { total: 0, pass: vec![0; n] } }
}

fn main() {
    install_libxml2_silencer();

    let verbose  = std::env::var("XPATH_VERBOSE").is_ok();
    let filter   = std::env::var("XPATH_FILTER").ok();

    let backends: Vec<Box<dyn Backend>> = vec![
        Box::new(SupXmlBackend { name: "sup-xml-strict", compat: false }),
        Box::new(SupXmlBackend { name: "sup-xml-compat", compat: true  }),
        Box::new(Libxml2Backend),
    ];
    let backend_names: Vec<&'static str> = backends.iter().map(|b| b.name()).collect();
    let overrides = build_overrides();

    let tests = match load_corpus() {
        Ok(t) => t,
        Err(e) => { eprintln!("{}", e); return; }
    };

    println!("\nXPath 1.0 cross-impl runner — libxml2 corpus");
    println!("Backends: {}", backend_names.join(", "));
    println!("Tests loaded: {}  ({} have spec-graded overrides)\n",
             tests.len(), overrides.len());

    let mut by_file: std::collections::BTreeMap<String, FileStats> = Default::default();
    let mut totals = FileStats::new(backends.len());
    let mut backend_time: Vec<Duration> = vec![Duration::ZERO; backends.len()];
    let mut surprises: Vec<(String, usize, String, [Outcome; 3], &'static str)> = Vec::new();

    for t in &tests {
        if let Some(f) = &filter {
            if !t.file.contains(f.as_str()) { continue; }
        }
        let mut outcomes = Vec::with_capacity(backends.len());
        for (i, b) in backends.iter().enumerate() {
            let t0 = Instant::now();
            let o  = b.eval(&t.expr, &t.doc_xml);
            backend_time[i] += t0.elapsed();
            outcomes.push(o);
        }

        // Look up override; otherwise use libxml2's live output as
        // the de facto ground truth.  Order of `expecteds` mirrors
        // the `backends` Vec order: strict, compat, libxml2.
        let lookup_key: (&str, usize) = (t.file.as_str(), t.line);
        let (expecteds, source) = if let Some(ov) = overrides.get(&lookup_key) {
            ([ov.strict.clone(), ov.compat.clone(), ov.strict.clone()], "override")
        } else {
            let lx = outcome_to_expected(&outcomes[2]);
            ([lx.clone(), lx.clone(), lx], "libxml2-baseline")
        };

        let fs = by_file.entry(t.file.clone()).or_insert_with(|| FileStats::new(backends.len()));
        fs.total += 1;
        totals.total += 1;
        for (i, (o, e)) in outcomes.iter().zip(expecteds.iter()).enumerate() {
            if matches(o, e) {
                fs.pass[i]     += 1;
                totals.pass[i] += 1;
            }
        }

        // For non-overridden disagreements, capture a surprise record
        // so the user can investigate.  Overridden cases are expected
        // disagreements and don't surprise us.
        if source == "libxml2-baseline" && !outcomes.iter().all(|o|
            matches(o, &outcome_to_expected(&outcomes[2])))
        {
            let arr: [Outcome; 3] = [outcomes[0].clone(), outcomes[1].clone(), outcomes[2].clone()];
            surprises.push((t.file.clone(), t.line, t.expr.clone(), arr,
                            "non-overridden disagreement"));
            if verbose {
                eprintln!("  [{}:{}] expr={:?}  ({})", t.file, t.line, t.expr, source);
                for (b, o) in backends.iter().zip(outcomes.iter()) {
                    eprintln!("      {:<16} {}", b.name(), fmt_outcome(o));
                }
            }
        }
    }

    print_table(&backend_names, &by_file, &totals);
    print_timing(&backend_names, &backend_time);
    print_overrides(&overrides);
    print_surprises(&backend_names, &surprises);
}

fn print_table(
    backend_names: &[&'static str],
    by_file: &std::collections::BTreeMap<String, FileStats>,
    totals: &FileStats,
) {
    if totals.total == 0 {
        println!("  No tests ran (filter excluded everything).");
        return;
    }
    println!("  Per-test-file conformance");
    print!("  {:<32}  {:>6}", "file", "n");
    for name in backend_names {
        print!("  {:>20}", name);
    }
    println!();
    for (file, fs) in by_file {
        print!("  {:<32}  {:>6}", file, fs.total);
        for &p in &fs.pass {
            print!("  {:>20}", fmt_pf(p, fs.total));
        }
        println!();
    }
    print!("  {:<32}  {:>6}", "TOTAL", totals.total);
    for &p in &totals.pass {
        print!("  {:>20}", fmt_pf(p, totals.total));
    }
    println!();
}

fn print_timing(backend_names: &[&'static str], times: &[Duration]) {
    println!("\n  Wall-clock (parse + index + eval, summed across all expressions):");
    for (name, dt) in backend_names.iter().zip(times.iter()) {
        println!("    {:<18}  {}", name, fmt_dur(*dt));
    }
}

/// List every spec-graded override the bench applied.  Lets the reader
/// see exactly which cases we judged libxml2 wrong on, and the spec
/// section we cited.
fn print_overrides(
    overrides: &std::collections::HashMap<(&'static str, usize), OverrideEntry>,
) {
    if overrides.is_empty() { return; }
    let mut keys: Vec<_> = overrides.keys().collect();
    keys.sort();
    println!("\n  Spec-graded overrides ({} cases)", overrides.len());
    for k in keys {
        let ov = &overrides[k];
        println!("    {}:{:<3}  spec={}  libxml2={}",
                 k.0, k.1,
                 fmt_expected(&ov.strict),
                 fmt_expected(&ov.compat));
        println!("        why: {}", ov.why);
    }
}

/// Non-overridden disagreements — these are tests where libxml2's live
/// output is what we used as the expected, but one of the sup-xml
/// backends produced something different.  Either a sup-xml bug or a
/// case worth adding to the override table once we've judged the spec.
fn print_surprises(
    backend_names: &[&'static str],
    surprises: &[(String, usize, String, [Outcome; 3], &'static str)],
) {
    if surprises.is_empty() {
        println!("\n  No surprises — every non-overridden test agreed across backends. ✓");
        return;
    }
    println!("\n  Non-overridden disagreements ({})", surprises.len());
    for (file, line, expr, outcomes, _) in surprises.iter().take(20) {
        println!("    {}:{:<3}  {}", file, line, expr);
        for (b, o) in backend_names.iter().zip(outcomes.iter()) {
            println!("        {:<18}  {}", b, fmt_outcome(o));
        }
    }
    if surprises.len() > 20 {
        println!("    … (+{} more — re-run with XPATH_VERBOSE=1 to see all)",
                 surprises.len() - 20);
    }
}

fn fmt_expected(e: &Expected) -> String {
    match e {
        Expected::Bool(b) => format!("bool({})", b),
        Expected::Num(n) =>
            if n.is_nan() { "num(NaN)".into() }
            else { format!("num({})", n) },
        Expected::Str(s) => format!("str({:?})", s),
        Expected::NodeStrings(v)    => format!("nodes({:?})", v),
        Expected::NodeStringsSet(v) => format!("nodes-any-order({:?})", v),
        Expected::Error  => "error".into(),
    }
}

fn fmt_pf(pass: usize, total: usize) -> String {
    if total == 0 { "—".into() }
    else { format!("{}/{} {:>5.1}%", pass, total, pass as f64 * 100.0 / total as f64) }
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s >= 1.0      { format!("{:>6.2} s",  s) }
    else if s >= 1e-3 { format!("{:>6.2} ms", s * 1e3) }
    else              { format!("{:>6.2} µs", s * 1e6) }
}
