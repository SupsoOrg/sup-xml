//! W3C XML Schema Test Suite (XSTS 2007-06-20) conformance runner.
//!
//! Walks every `.testSet` manifest under
//! `tests/assets/xsts/xmlschema2006-11-06/{sun,boeing,nist}Meta/`,
//! dispatches each `<schemaTest>` to `Schema::compile_str` and each
//! `<instanceTest>` to `schema.validate_str`, and tallies pass/fail
//! against the manifest's expected validity.
//!
//! Marked `#[ignore]` because the suite has ~25k tests and shouldn't
//! be on the every-PR critical path.  Run explicitly with:
//!
//! ```text
//! cargo test --features xsd --test xsts -- --ignored --nocapture
//! ```
//!
//! Fetch the suite first via `tests/assets/xsts/fetch.sh`.  If the
//! suite isn't present, the test logs a hint and returns OK.
//!
//! ## What gets counted
//!
//! For each `<testGroup>`:
//! * `schemaTest` — compile the schema; expect Ok iff
//!   `expected validity="valid"`.
//! * `instanceTest` — compile the most recent same-group schema,
//!   then validate the instance; expect Ok iff
//!   `expected validity="valid"`.
//!
//! All four contributors in the XSTS distribution use the same
//! `.testSet` manifest format: `sunMeta/`, `boeingMeta/`, `nistMeta/`,
//! and `msMeta/` (Microsoft).

#![cfg(feature = "xsd")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sup_xml::xsd::{FsResolver, Schema};
use sup_xml::{Event, XmlReader};

const XSTS_ROOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/assets/xsts/xmlschema2006-11-06"
);

#[derive(Default, Debug)]
struct Stats {
    schema_pass:    usize,
    schema_fail:    usize,
    instance_pass:  usize,
    instance_fail:  usize,
    schema_errored_for_instance: usize,
}

impl Stats {
    fn add(&mut self, other: &Stats) {
        self.schema_pass    += other.schema_pass;
        self.schema_fail    += other.schema_fail;
        self.instance_pass  += other.instance_pass;
        self.instance_fail  += other.instance_fail;
        self.schema_errored_for_instance += other.schema_errored_for_instance;
    }

    fn schema_rate(&self) -> f64 {
        let total = self.schema_pass + self.schema_fail;
        if total == 0 { 0.0 } else { self.schema_pass as f64 * 100.0 / total as f64 }
    }

    fn instance_rate(&self) -> f64 {
        let total = self.instance_pass + self.instance_fail;
        if total == 0 { 0.0 } else { self.instance_pass as f64 * 100.0 / total as f64 }
    }
}

#[derive(Debug)]
struct TestCase {
    kind:     TestKind,
    /// For SchemaTest: the primary document is `hrefs[0]`; any
    /// additional entries are companion schemas (xs:import /
    /// xs:include targets) loaded into the resolver under their
    /// basename, so the primary's location-relative references
    /// resolve.  For InstanceTest: a single href.
    hrefs:    Vec<String>,
    expected: Validity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestKind { Schema, Instance }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Validity { Valid, Invalid }

/// Parse one `.testSet` manifest into a flat list of test cases,
/// preserving document order (so an instanceTest finds the most
/// recently parsed sibling schemaTest at run time).
fn parse_test_set(xml: &str) -> Result<Vec<TestCase>, String> {
    let mut reader = XmlReader::from_str(xml);
    let mut cases: Vec<TestCase> = Vec::new();

    let mut in_schema_test = false;
    let mut in_instance_test = false;
    let mut pending_hrefs: Vec<String> = Vec::new();

    loop {
        let ev = reader.next().map_err(|e| e.to_string())?;
        match ev {
            Event::StartElement(tag) => {
                let name = tag.name().to_string();
                let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(&name).to_string();
                match local.as_str() {
                    "schemaTest"   => { in_schema_test = true;   pending_hrefs.clear(); }
                    "instanceTest" => { in_instance_test = true; pending_hrefs.clear(); }
                    "schemaDocument" | "instanceDocument" => {
                        for a in tag.attrs() {
                            let a = a.map_err(|e| e.to_string())?;
                            if a.name() == "xlink:href" || a.name() == "href" {
                                pending_hrefs.push(a.value().to_string());
                            }
                        }
                    }
                    "expected" => {
                        let mut validity: Option<Validity> = None;
                        for a in tag.attrs() {
                            let a = a.map_err(|e| e.to_string())?;
                            if a.name() == "validity" {
                                validity = match a.value() {
                                    "valid"   => Some(Validity::Valid),
                                    "invalid" => Some(Validity::Invalid),
                                    _ => None,
                                };
                            }
                        }
                        if let Some(v) = validity {
                            if !pending_hrefs.is_empty() {
                                let kind = if in_schema_test { TestKind::Schema }
                                           else if in_instance_test { TestKind::Instance }
                                           else { continue };
                                cases.push(TestCase {
                                    kind,
                                    hrefs: std::mem::take(&mut pending_hrefs),
                                    expected: v,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::EndElement(tag) => {
                let name = tag.name().to_string();
                let local = name.rsplit_once(':').map(|(_, l)| l).unwrap_or(&name).to_string();
                match local.as_str() {
                    "schemaTest"   => in_schema_test = false,
                    "instanceTest" => in_instance_test = false,
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(cases)
}

/// Read a file, tolerating non-UTF-8 inputs by lossy-converting (the
/// XSTS contains a handful of latin-1 encoded XML files that we'd
/// otherwise have to skip entirely; the parser handles them via its
/// own auto-detect, but we feed strings here for simplicity).
fn read_relative(base: &Path, href: &str) -> Result<String, std::io::Error> {
    let path = base.join(href);
    let bytes = std::fs::read(&path)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Run every test in one .testSet.  Returns stats, surprise-log
/// strings for printing, and a list of fingerprinted error messages
/// for bucket aggregation by the caller.
fn run_test_set(manifest: &Path) -> (Stats, Vec<String>, Vec<String>) {
    let mut stats = Stats::default();
    let mut surprises: Vec<String> = Vec::new();
    let mut fingerprints: Vec<String> = Vec::new();

    let manifest_dir = manifest.parent().unwrap_or(Path::new("."));
    let manifest_src = match std::fs::read_to_string(manifest) {
        Ok(s) => s,
        Err(_) => return (stats, surprises, fingerprints),
    };
    let cases = match parse_test_set(&manifest_src) {
        Ok(c) => c,
        Err(_) => return (stats, surprises, fingerprints),
    };

    let libxml2_diff = std::env::var("XSTS_LIBXML2_DIFF").is_ok();
    let mut last_schema_path: Option<PathBuf> = None;
    let mut last_schema_xmllint_ok: Option<bool> = None;

    // Track the most recently parsed schema in the same manifest so
    // instance tests can find their schema by document order.
    let mut last_schema: Option<Schema> = None;

    for case in cases {
        let primary_href = case.hrefs.first().cloned().unwrap_or_default();
        let src = match read_relative(manifest_dir, &primary_href) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let abs_path = manifest_dir.join(&primary_href);
        match case.kind {
            TestKind::Schema => {
                // The primary schema may xs:include / xs:import other
                // schemas via relative paths.  Companions sometimes
                // appear as additional <schemaDocument> entries (sun
                // & boeing style) and sometimes aren't listed at all
                // but exist in the same directory (ms style).
                // FsResolver rooted at the primary schema's directory
                // covers both: it finds files on disk by their
                // relative href.
                let resolver = FsResolver::new(
                    abs_path.parent().unwrap_or(manifest_dir).to_path_buf(),
                );
                let result = Schema::compile_with(&src, resolver);
                let actual = if result.is_ok() { Validity::Valid } else { Validity::Invalid };
                let ours_ok = actual == case.expected;
                if ours_ok {
                    stats.schema_pass += 1;
                } else {
                    stats.schema_fail += 1;
                    let msg = match &result {
                        Ok(_)  => "expected-invalid but compile succeeded".to_string(),
                        Err(e) => e.to_string(),
                    };
                    fingerprints.push(format!("schema: {}", fingerprint_message(&msg)));
                    if !libxml2_diff && surprises.len() < 200 {
                        let detail = match &result {
                            Ok(_)  => String::new(),
                            Err(e) => format!("  → {}", e),
                        };
                        surprises.push(format!(
                            "    schema {:?}  expected={:?}  got={:?}{detail}",
                            primary_href, case.expected, actual
                        ));
                    }
                }
                if libxml2_diff {
                    let xmllint_validity = xmllint_schema(&abs_path);
                    let xmllint_ok = xmllint_validity == case.expected;
                    let tag = match (ours_ok, xmllint_ok) {
                        (false, true)  => Some("[us-wrong]"),
                        (true,  false) => Some("[libxml2-wrong]"),
                        _              => None,
                    };
                    if let Some(tag) = tag {
                        if surprises.len() < 500 {
                            surprises.push(format!(
                                "    {tag} schema {:?}  expected={:?}  us={:?}  libxml2={:?}",
                                primary_href, case.expected, actual, xmllint_validity,
                            ));
                        }
                    }
                    last_schema_xmllint_ok = Some(matches!(xmllint_validity, Validity::Valid));
                }
                last_schema_path = Some(abs_path.clone());
                last_schema = result.ok();
            }
            TestKind::Instance => {
                let Some(schema) = last_schema.as_ref() else {
                    stats.schema_errored_for_instance += 1;
                    continue;
                };
                let result = schema.validate_str(&src);
                let actual = if result.is_ok() { Validity::Valid } else { Validity::Invalid };
                let ours_ok = actual == case.expected;
                if ours_ok {
                    stats.instance_pass += 1;
                } else {
                    stats.instance_fail += 1;
                    let msg = match &result {
                        Ok(()) => "expected-invalid but validate succeeded".to_string(),
                        Err(e) => e.issues.first()
                            .map(|i| i.message.clone())
                            .unwrap_or_else(|| "(no issue message)".to_string()),
                    };
                    fingerprints.push(format!("instance: {}", fingerprint_message(&msg)));
                    if !libxml2_diff && surprises.len() < 200 {
                        let detail = match &result {
                            Ok(()) => String::new(),
                            Err(e) => e.issues.first()
                                .map(|i| format!("  → {}", i.message))
                                .unwrap_or_default(),
                        };
                        surprises.push(format!(
                            "    instance {:?}  expected={:?}  got={:?}{detail}",
                            primary_href, case.expected, actual
                        ));
                    }
                }
                if libxml2_diff {
                    // Only meaningful when libxml2 compiled the schema.
                    let schema_path = last_schema_path.as_deref();
                    let xmllint_validity = match (last_schema_xmllint_ok, schema_path) {
                        (Some(true), Some(sp)) => xmllint_instance(sp, &abs_path),
                        _ => Validity::Invalid, // schema unavailable to libxml2
                    };
                    let schema_ok_for_libxml2 = last_schema_xmllint_ok == Some(true);
                    let xmllint_ok = schema_ok_for_libxml2 && xmllint_validity == case.expected;
                    // Skip cases where libxml2 couldn't even compile
                    // the schema — those aren't a meaningful
                    // comparison (we'd report them as us-wrong even
                    // when libxml2 simply can't reach the instance).
                    if schema_ok_for_libxml2 {
                        let tag = match (ours_ok, xmllint_ok) {
                            (false, true)  => Some("[us-wrong]"),
                            (true,  false) => Some("[libxml2-wrong]"),
                            _              => None,
                        };
                        if let Some(tag) = tag {
                            if surprises.len() < 500 {
                                surprises.push(format!(
                                    "    {tag} instance {:?}  expected={:?}  us={:?}  libxml2={:?}",
                                    primary_href, case.expected, actual, xmllint_validity,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    (stats, surprises, fingerprints)
}

/// Invoke xmllint on a schema file in isolation.  Returns the
/// schema's validity verdict per libxml2.  Falls back to `Invalid`
/// when xmllint isn't on PATH (treats the comparison as "libxml2
/// disagrees with us"; the missing-binary error will surface in
/// stderr the first time the runner hits it).
/// Run `xmllint` with a wall-clock budget — a handful of XSTS
/// schemas (e.g. `particlesZ012`) push libxml2 into exponential
/// backtracking, so we bound each invocation and treat a timeout
/// as "libxml2 disagrees with us" rather than blocking the whole
/// run.  Returns `None` on timeout, `Some((status, stderr))`
/// otherwise.
fn run_xmllint_bounded(
    args: &[&Path],
    seconds: u64,
) -> Option<(std::process::ExitStatus, Vec<u8>)> {
    let mut cmd = std::process::Command::new("timeout");
    cmd.arg(format!("{seconds}"));
    cmd.arg("xmllint").arg("--noout");
    for a in args { cmd.arg(a); }
    let out = cmd.output().ok()?;
    // `timeout` returns 124 (and on some systems 137 with --kill-after)
    // when it had to fire.  Treat that as a non-result.
    let code = out.status.code();
    if code == Some(124) || code == Some(137) { return None; }
    Some((out.status, out.stderr))
}

fn xmllint_schema(schema: &Path) -> Validity {
    // `xmllint --noout --schema X.xsd /dev/null` reports schema
    // compile errors to stderr.  /dev/null always fails the
    // subsequent doc parse, so the exit code only tells us "schema
    // compiled" if there's no "Schemas parser error" line.
    let dev_null = Path::new("/dev/null");
    let Some((_status, stderr)) = run_xmllint_bounded(
        &[Path::new("--schema"), schema, dev_null], 10,
    ) else {
        return Validity::Invalid;
    };
    let stderr = String::from_utf8_lossy(&stderr);
    if stderr.contains("Schemas parser error")
        || stderr.contains("failed to compile")
    {
        Validity::Invalid
    } else {
        Validity::Valid
    }
}

fn xmllint_instance(schema: &Path, instance: &Path) -> Validity {
    let Some((status, _stderr)) = run_xmllint_bounded(
        &[Path::new("--schema"), schema, instance], 10,
    ) else {
        return Validity::Invalid;
    };
    if status.success() { Validity::Valid } else { Validity::Invalid }
}

/// Strip variable parts of an error message (quoted strings, QNames,
/// numbers) so we can bucket failures by underlying cause.
fn fingerprint_message(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push_str("\"…\"");
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n == '"' { break; }
                }
            }
            '{' => {
                out.push_str("{…}");
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n == '}' { break; }
                }
            }
            c if c.is_ascii_digit() => {
                out.push('N');
                while let Some(&n) = chars.peek() {
                    if n.is_ascii_digit() { chars.next(); } else { break; }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// Discover every .testSet under the suite's `{sun,boeing,nist}Meta/`
/// directories, group by contributor, run, and print a summary.
fn run_suite() {
    let root = PathBuf::from(XSTS_ROOT);
    if !root.exists() {
        eprintln!(
            "XSTS not present at {}\n\
             Run `tests/assets/xsts/fetch.sh` to download the W3C suite (~4MB).",
            root.display()
        );
        return;
    }

    let mut by_contributor: HashMap<String, Stats> = HashMap::new();
    let mut by_contributor_fingerprints: HashMap<String, HashMap<String, usize>> = HashMap::new();
    let mut total_manifests = 0usize;

    for contributor in &["sunMeta", "boeingMeta", "nistMeta", "msMeta"] {
        let dir = root.join(contributor);
        if !dir.exists() { continue; }
        let read_dir = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            // sun/boeing/nist use `.testSet`; ms uses `.xml`.  Skip
            // anything else (the suite ships a few helper XSDs and
            // README-type files we shouldn't try to parse).
            let ext = path.extension().and_then(|s| s.to_str());
            if ext != Some("testSet") && ext != Some("xml") { continue; }
            total_manifests += 1;
            let (stats, surprises, fingerprints) = run_test_set(&path);
            by_contributor.entry(contributor.to_string()).or_default().add(&stats);
            let bucket = by_contributor_fingerprints
                .entry(contributor.to_string()).or_default();
            for fp in fingerprints {
                *bucket.entry(fp).or_default() += 1;
            }
            let print_surprises = std::env::var("XSTS_VERBOSE").is_ok()
                || std::env::var("XSTS_LIBXML2_DIFF").is_ok();
            if !surprises.is_empty() && print_surprises {
                println!("  {}:", path.file_name().unwrap().to_string_lossy());
                for s in surprises { println!("{}", s); }
            }
        }
    }

    let mut totals = Stats::default();
    println!("\n  XSTS conformance ({} manifests)\n", total_manifests);
    println!("  {:<14}  {:>9}  {:>9}  {:>9}  {:>9}  {:>10}",
        "contributor", "schemaPF", "schema%", "inst PF", "inst %", "noSchema");
    let mut keys: Vec<&String> = by_contributor.keys().collect();
    keys.sort();
    for key in keys {
        let s = &by_contributor[key];
        println!(
            "  {:<14}  {:>4}/{:>4}  {:>8.1}%  {:>4}/{:>4}  {:>8.1}%  {:>10}",
            key,
            s.schema_pass, s.schema_pass + s.schema_fail, s.schema_rate(),
            s.instance_pass, s.instance_pass + s.instance_fail, s.instance_rate(),
            s.schema_errored_for_instance,
        );
        totals.add(s);
    }
    println!("  {:<14}  {:>4}/{:>4}  {:>8.1}%  {:>4}/{:>4}  {:>8.1}%  {:>10}",
        "TOTAL",
        totals.schema_pass, totals.schema_pass + totals.schema_fail, totals.schema_rate(),
        totals.instance_pass, totals.instance_pass + totals.instance_fail, totals.instance_rate(),
        totals.schema_errored_for_instance,
    );

    if std::env::var("XSTS_BUCKETS").is_ok() {
        let mut contributors: Vec<&String> = by_contributor_fingerprints.keys().collect();
        contributors.sort();
        for c in contributors {
            let bucket = &by_contributor_fingerprints[c];
            let mut entries: Vec<(&String, &usize)> = bucket.iter().collect();
            entries.sort_by(|a, b| b.1.cmp(a.1));
            let total: usize = bucket.values().sum();
            println!("\n  {} — {} unique failure fingerprints, {} total failures",
                c, bucket.len(), total);
            for (msg, count) in entries.iter().take(20) {
                println!("    {:>4}  {}", count,
                    if msg.len() > 140 { format!("{}…", &msg[..139]) } else { msg.to_string() });
            }
        }
    }
}

#[test]
#[ignore = "run with --ignored: walks the W3C XSTS suite (~25k tests). \
            Fetch the suite first via tests/assets/xsts/fetch.sh."]
fn w3c_xsts_conformance() {
    run_suite();
}


