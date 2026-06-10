/// W3C XML Conformance Test Suite (xmlts20130923).
///
/// Tests are read from tests/w3c/ (copied from https://www.w3.org/XML/Test/).
/// We run XML 1.0 tests only; XML 1.1 sub-catalogs are skipped.
///
/// Expected outcomes:
///   not-wf  → parse_bytes must return Err
///   valid   → parse_bytes must return Ok
///   invalid → parse_bytes must return Ok  (we are not a validating parser)
///   error   → skipped (implementation-defined behaviour)
use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::borrow::Cow;

use sup_xml::{encoding, parse_bytes, EntityResolver, ParseOptions, ResolveError};

// ── known-failing test allowlist ──────────────────────────────────────────────
//
// Each entry is a W3C catalog `ID=` value whose current outcome
// diverges from the catalog's expected outcome.  These are real
// gaps we want surfaced as tests (not silently skipped); the
// allowlist lets the test report them as `xfail` so the build
// stays green while still failing loudly on any new regression
// *outside* the allowlist.
//
// Unexpectedly-passing IDs (in this list but the test now passes)
// trigger a build failure — that forces the entry to be removed
// when the underlying bug is fixed.
//
// Groupings reflect the parser feature each test exercises.  Fix
// any group → delete its entries here.
const KNOWN_FAILING_IDS: &[&str] = &[];

/// Test-only entity resolver scoped to a single catalog's directory
/// tree.  Each catalog's tests reference external `.ent` files and
/// occasionally cross-directory neighbours (e.g.
/// `xmltest/valid/ext-sa/ext02.xml` pulls
/// `../invalid/utf16b.xml`), so the allowlist is the catalog root,
/// not the individual test file's parent.
///
/// The parser pre-resolves relative SYSTEM identifiers against the
/// document's / containing entity's base URI before calling the
/// resolver (XML 1.0 § 4.2.2 + errata E18), so this resolver only
/// needs to open already-absolute paths.  Falls back to `test_dir`
/// when given a bare relative path (e.g. when `base_url` was unset
/// on `ParseOptions`).  All resolved paths are canonicalised and
/// must land under `root_dir`.
#[derive(Debug)]
struct FixtureResolver {
    root_dir: PathBuf,
    test_dir: PathBuf,
}

impl EntityResolver for FixtureResolver {
    fn resolve(
        &self,
        _public_id: Option<&str>,
        system_id: &str,
        _base_uri: Option<&str>,
    ) -> Result<Vec<u8>, ResolveError> {
        let stripped = system_id.strip_prefix("file://").unwrap_or(system_id);
        if stripped.contains("://") {
            return Err(ResolveError::Refused(format!(
                "FixtureResolver only handles file:// URIs, got: {system_id}"
            )));
        }
        let raw = if Path::new(stripped).is_absolute() {
            PathBuf::from(stripped)
        } else {
            self.test_dir.join(stripped)
        };
        let canonical = raw.canonicalize().map_err(|_| ResolveError::Refused(
            format!("path {} not found", raw.display())
        ))?;
        let root_canonical = self.root_dir.canonicalize().map_err(|_| ResolveError::Refused(
            format!("root {} not canonicalisable", self.root_dir.display())
        ))?;
        if !canonical.starts_with(&root_canonical) {
            return Err(ResolveError::Refused(format!(
                "path {} escapes catalog root {}",
                canonical.display(), root_canonical.display()
            )));
        }
        std::fs::read(&canonical).map_err(|e| ResolveError::Io(
            format!("reading {}: {e}", canonical.display())
        ))
    }
}

// ── catalog location ──────────────────────────────────────────────────────────

fn w3c_root() -> PathBuf {
    // Canonicalise eagerly: this collapses the `../..` and normalises the
    // separators, so paths joined onto the root — and the resolver's
    // `starts_with` security check — compare cleanly on every OS.  Left in
    // its raw `..` / mixed-separator form, path canonicalisation of the
    // joined fixture paths fails on Windows.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/w3c")
        .canonicalize()
        .expect("tests/w3c assets directory must exist")
}

// ── XML 1.0 sub-catalogs to load ──────────────────────────────────────────────

const CATALOGS: &[&str] = &[
    "xmltest/xmltest.xml",
    "sun/sun-valid.xml",
    "sun/sun-not-wf.xml",
    "sun/sun-invalid.xml",
    "oasis/oasis.xml",
    "ibm/ibm_oasis_valid.xml",
    "ibm/ibm_oasis_not-wf.xml",
    "ibm/ibm_oasis_invalid.xml",
    // eduni errata cover XML 1.0 errata; skip xml-1.1 sub-dirs
    "eduni/errata-2e/errata2e.xml",
    "eduni/errata-3e/errata3e.xml",
    "eduni/errata-4e/errata4e.xml",
    "eduni/namespaces/1.0/rmt-ns10.xml",
    "eduni/misc/ht-bh.xml",
];

// ── minimal catalog parser ────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum TestType {
    Valid,
    Invalid,
    NotWf,
    Error,
}

struct TestCase {
    id: String,
    ty: TestType,
    /// Absolute path to the XML file being tested.
    path: PathBuf,
    /// Whether external entities are required (we skip those we can't load).
    needs_external_entities: bool,
    /// Use XML 1.0 4th-edition character tables (BaseChar/CombiningChar/Digit/Extender).
    /// Set for IBM P85/P87/P88/P89 tests which test those specific productions.
    fourth_edition: bool,
    /// Apply XML Namespaces 1.0 processing: parser NCName checks + resolve_namespaces.
    /// Set for tests with RECOMMENDATION="NS1.0" in the catalog.
    namespace_aware: bool,
}

/// Naively extract attribute value from a `key="value"` or `key='value'` pair.
fn attr<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle_dq = format!("{key}=\"");
    let needle_sq = format!("{key}='");
    if let Some(start) = line.find(&needle_dq) {
        let rest = &line[start + needle_dq.len()..];
        rest.find('"').map(|end| &rest[..end])
    } else if let Some(start) = line.find(&needle_sq) {
        let rest = &line[start + needle_sq.len()..];
        rest.find('\'').map(|end| &rest[..end])
    } else {
        None
    }
}

fn load_catalog(catalog_path: &Path) -> Vec<TestCase> {
    let base_dir = catalog_path.parent().unwrap().to_path_buf();
    let text = match std::fs::read_to_string(catalog_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("WARN: could not read catalog {:?}: {e}", catalog_path);
            return Vec::new();
        }
    };

    let mut cases = Vec::new();
    // Accumulate multi-line TEST elements
    let mut current = String::new();
    let mut in_test = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<TEST ") || (in_test && !trimmed.is_empty()) {
            current.push(' ');
            current.push_str(trimmed);
            in_test = true;
        }
        if in_test && (trimmed.contains('>') || trimmed.ends_with("/>")) {
            // We have the full TEST element collected
            let elem = &current;

            if let (Some(id), Some(ty_str), Some(uri)) = (
                attr(elem, "ID"),
                attr(elem, "TYPE"),
                attr(elem, "URI"),
            ) {
                // Skip XML 1.1 tests — we implement XML 1.0 only.
                if attr(elem, "VERSION") == Some("1.1") {
                    current.clear();
                    in_test = false;
                    continue;
                }

                let ty = match ty_str {
                    "valid" => TestType::Valid,
                    "invalid" => TestType::Invalid,
                    "not-wf" => TestType::NotWf,
                    "error" => TestType::Error,
                    _ => { current.clear(); in_test = false; continue; }
                };

                let entities = attr(elem, "ENTITIES").unwrap_or("none");
                let needs_external = matches!(entities, "general" | "parameter" | "both");

                // Tests that specifically target 4th-edition character-class
                // productions (BaseChar, CombiningChar, Digit, Extender —
                // removed in the 5th edition).  Their expected-to-be-rejected
                // characters are accepted under our default 5e rules, so we
                // opt them into 4e for the suite run — matching the way the
                // test authors intended the inputs to be validated.
                //
                // Modern callers leave `xml10_fourth_edition: false`
                // (libxml2's default too); opting individual tests into 4e
                // here is a harness-only annotation, not a behaviour change
                // for normal users.  Predicate-level + parse-level
                // edition-flip coverage lives in `crates/core`:
                //   - `charsets::tests::u309a_namestartchar_is_5e_valid_4e_invalid`
                //   - `xml_bytes_reader::tests::name_start_combining_mark_accepted_5e_rejected_4e`
                let fourth_edition = id.starts_with("ibm-not-wf-P85-")
                    || id.starts_with("ibm-not-wf-P86-")
                    || id.starts_with("ibm-not-wf-P87-")
                    || id.starts_with("ibm-not-wf-P88-")
                    || id.starts_with("ibm-not-wf-P89-")
                    // xmltest not-wf-sa-140/141: same character-class pattern
                    // as the IBM P85-P89 cases, just authored under a
                    // different catalog.  U+309A and U+0E5C are NameStartChar
                    // under 5e but not 4e — the suite's "not well-formed"
                    // verdict only holds under 4e.
                    || id == "not-wf-sa-140"
                    || id == "not-wf-sa-141";

                // Tests with RECOMMENDATION="NS1.0" require XML Namespaces 1.0
                // processing.  Tests with NAMESPACE="no" explicitly opt out.
                let recommendation = attr(elem, "RECOMMENDATION").unwrap_or("");
                let namespace_attr = attr(elem, "NAMESPACE").unwrap_or("");
                let namespace_aware = recommendation == "NS1.0" && namespace_attr != "no";

                cases.push(TestCase {
                    id: id.to_string(),
                    ty,
                    path: base_dir.join(uri),
                    needs_external_entities: needs_external,
                    fourth_edition,
                    namespace_aware,
                });
            }

            current.clear();
            in_test = false;
        }
    }

    cases
}

// ── test runner ───────────────────────────────────────────────────────────────

#[test]
fn w3c_xml_conformance() {
    let root = w3c_root();
    if !root.exists() {
        eprintln!("SKIP: W3C test suite not found at {root:?}");
        return;
    }

    let mut total = 0usize;
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut xfail = 0usize;          // failed AND in KNOWN_FAILING_IDS
    let mut unexpected_pass = 0usize; // passed BUT in KNOWN_FAILING_IDS — must be removed
    let mut failures: Vec<String> = Vec::new();
    let mut unexpected_passes: Vec<String> = Vec::new();

    for catalog_rel in CATALOGS {
        let catalog_path = root.join(catalog_rel);
        if !catalog_path.exists() {
            eprintln!("WARN: catalog not found: {catalog_path:?}");
            continue;
        }

        for case in load_catalog(&catalog_path) {
            total += 1;

            // Skip error tests (implementation-defined)
            if case.ty == TestType::Error {
                skipped += 1;
                continue;
            }

            let bytes = match std::fs::read(&case.path) {
                Ok(b) => b,
                Err(e) => {
                    skipped += 1;
                    eprintln!("WARN: could not read {:?}: {e}", case.path);
                    continue;
                }
            };

            // For tests whose catalog entry advertises external general or
            // parameter entities (`ENTITIES="general"|"parameter"|"both"`),
            // wire a [`FixtureResolver`] that resolves relative `SYSTEM`
            // identifiers against the test file's directory and refuses
            // anything outside the W3C catalog root.  Without this
            // wiring, every such test was previously skipped: the
            // malformed construct lives inside the external entity
            // file, so without loading it the parser sees a trivially
            // well-formed wrapper and the catalog's "not-wf" verdict
            // cannot be exercised.
            let external_resolver = if case.needs_external_entities {
                case.path.parent().map(|test_dir| {
                    Arc::new(FixtureResolver {
                        root_dir: root.clone(),
                        test_dir: test_dir.to_path_buf(),
                    }) as Arc<dyn EntityResolver>
                })
            } else {
                None
            };

            // Set the parser's base URL to the test file's absolute
            // `file://` URI.  Required for XML 1.0 § 4.2.2 + errata
            // E18 base-URI resolution of nested entity declarations.
            let base_url = format!("file://{}", case.path.display());
            let opts = ParseOptions {
                xml10_fourth_edition: case.fourth_edition,
                namespace_aware: case.namespace_aware,
                // The harness does its own (strict) transcoding below via
                // `transcode_to_utf8_strict`.  Turn the parser's auto-transcode
                // off so it doesn't re-detect the embedded `encoding="..."`
                // declaration and decode the already-UTF-8 bytes a second time.
                auto_transcode: false,
                external_resolver,
                // Loading the external subset is gated on
                // `load_external_dtd` (libxml2's `XML_PARSE_DTDLOAD`); a
                // resolver is the mechanism, not the trigger.  Enable it
                // for the cases whose construct lives in external markup.
                load_external_dtd: case.needs_external_entities,
                base_url: Some(base_url),
                ..ParseOptions::default()
            };
            // Auto-detect the input encoding (Tier 1/2/3 via `encoding`),
            // transcode to UTF-8, AND verify the inner `<?xml encoding=...?>`
            // declaration matches the detected encoding.  Catches the
            // BOM-vs-declaration contradictions tested by hst-lhs-007/008.
            let utf8: Cow<[u8]> = match encoding::transcode_to_utf8_strict(&bytes) {
                Ok(c)  => c,
                Err(e) => {
                    // Encoding-level rejection IS the document rejection for
                    // not-wf tests like the hst-lhs pair, so propagate as Err
                    // through the rest of the pipeline.
                    match case.ty {
                        TestType::NotWf => {
                            passed += 1;
                            continue;
                        }
                        _ => {
                            failed += 1;
                            failures.push(format!(
                                "FAIL {} {}: encoding rejection unexpected, got: {}",
                                match case.ty { TestType::Valid => "valid", TestType::Invalid => "invalid", _ => "?" },
                                case.id, e.message,
                            ));
                            continue;
                        }
                    }
                }
            };
            // Namespace processing happens inline in the arena parser when
            // ParseOptions::namespace_aware is set above.
            let result = parse_bytes(&utf8, &opts);

            let known_failing = KNOWN_FAILING_IDS.contains(&case.id.as_str());
            let test_passed = match case.ty {
                TestType::NotWf => result.is_err(),
                TestType::Valid | TestType::Invalid => result.is_ok(),
                TestType::Error => unreachable!(),
            };
            match (test_passed, known_failing) {
                (true, false) => passed += 1,
                (false, true) => xfail += 1,
                (false, false) => {
                    failed += 1;
                    failures.push(match case.ty {
                        TestType::NotWf => format!(
                            "FAIL not-wf {}: should have been rejected but parsed OK",
                            case.id),
                        TestType::Valid | TestType::Invalid => format!(
                            "FAIL {} {}: should have parsed OK, got: {}",
                            if case.ty == TestType::Valid { "valid" } else { "invalid" },
                            case.id,
                            result.unwrap_err().message),
                        TestType::Error => unreachable!(),
                    });
                }
                (true, true) => {
                    unexpected_pass += 1;
                    unexpected_passes.push(format!(
                        "UNEXPECTED PASS {} {}: in KNOWN_FAILING_IDS but now passes — remove the allowlist entry",
                        match case.ty {
                            TestType::NotWf   => "not-wf",
                            TestType::Valid   => "valid",
                            TestType::Invalid => "invalid",
                            TestType::Error   => "?",
                        },
                        case.id,
                    ));
                }
            }
        }
    }

    println!(
        "\nW3C XML Conformance: {passed} passed, {failed} failed, \
         {xfail} xfail (allow-listed), {skipped} skipped (of {total} total)"
    );

    let limit = std::env::var("W3C_FAILURES").ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(40);
    for f in failures.iter().take(limit) {
        println!("  {f}");
    }
    if failures.len() > limit {
        println!("  ... and {} more failures", failures.len() - limit);
    }
    for u in &unexpected_passes {
        println!("  {u}");
    }

    // Strict policy: any failure outside the allowlist breaks the build,
    // and any test that *passes* despite being on the allowlist also
    // breaks the build (force the allowlist entry to be removed so we
    // never silently drift back into a worse state).
    assert_eq!(
        failed, 0,
        "{failed} W3C conformance tests failed (see the FAIL lines above)"
    );
    assert_eq!(
        unexpected_pass, 0,
        "{unexpected_pass} test(s) on the KNOWN_FAILING_IDS allowlist now pass — \
         remove them from the allowlist in crates/api/tests/w3c.rs"
    );
}
