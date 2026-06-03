//! HTML parsing throughput: sup-xml vs html5ever-skip-sup-xml vs libxml2.
//!
//! Run with:
//!   cargo bench -p sup-xml-bench --bench html_parse
//!
//! Fixtures live in tests/assets/html/ and are gitignored — fetch
//! them once with:
//!   tests/assets/html/fetch.sh
//!
//! See SOURCES.md in the same dir for the URL list and licenses.
//!
//! Missing fixtures are skipped silently so partial corpora still
//! produce useful numbers.
//!
//! # Apples-to-apples notes
//!
//! - **sup_xml::parse_html_str** drives html5ever into our DOM types
//!   via our `BatchSink::TreeSink`.  Has a per-element overhead
//!   (arena allocation + final tree walk to produce a `tree::Document`)
//!   on top of html5ever's tokenizer/tree-builder cost.
//! - **html5ever-skip-sup-xml** runs html5ever with a no-op TreeSink
//!   that discards every node — measures html5ever's
//!   tokenizer/tree-builder ceiling with no DOM-building work.  This
//!   isn't a real-world configuration anyone ships (you always need
//!   a real sink); it's a calibration baseline.  The gap between
//!   `sup-xml` and this number tells us how much of our throughput
//!   is sink overhead vs html5ever cost.
//! - **libxml2** calls `htmlReadMemory` and frees the resulting
//!   `htmlDocPtr` — same shape as sup-xml (full DOM).  This is the
//!   migration story for users coming off libxml2.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use html5ever::driver::parse_document;
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode as H5QuirksMode, TreeSink};
use html5ever::{Attribute as H5Attribute, ExpandedName, QualName};

// ── libxml2 HTML FFI ──────────────────────────────────────────────────────────
// htmlReadMemory(buffer, size, url, encoding, options) -> htmlDocPtr
// xmlFreeDoc(doc)

unsafe extern "C" {
    fn htmlReadMemory(
        buffer: *const c_char,
        size: c_int,
        url: *const c_char,
        encoding: *const c_char,
        options: c_int,
    ) -> *mut c_void;
    fn xmlFreeDoc(cur: *mut c_void);
}

// libxml2 HTML parser options.  HTML_PARSE_RECOVER (1) | HTML_PARSE_NOERROR
// (32) | HTML_PARSE_NOWARNING (64) — produce a tree even on malformed input,
// don't print errors to stderr.  Matches what most lxml.html callers use.
const HTML_PARSE_OPTIONS: c_int = 1 | 32 | 64;

// ── benchmark targets ─────────────────────────────────────────────────────────

fn bench_sup_xml_html(input: &str) {
    let doc = sup_xml::parse_html_str(input).expect("sup-xml html parse failed");
    criterion::black_box(doc);
}

/// Drive html5ever directly with a discard sink — bypasses
/// sup-xml's `BatchSink` entirely.  Used as a calibration
/// baseline, not as a real competitor (no production code uses
/// html5ever with a no-op sink — every real consumer has a sink
/// that builds *some* DOM).
fn bench_html5ever_skip_sup_xml(input: &str) {
    let sink = NoopSink::new();
    let parser = parse_document(sink, Default::default());
    let result = parser.one(input);
    criterion::black_box(result);
}

fn bench_libxml2_html(bytes: &[u8]) {
    let url = CString::new("benchmark").unwrap();
    unsafe {
        let doc = htmlReadMemory(
            bytes.as_ptr() as *const c_char,
            bytes.len() as c_int,
            url.as_ptr(),
            std::ptr::null(),
            HTML_PARSE_OPTIONS,
        );
        if doc.is_null() {
            // libxml2 returns NULL only for completely degenerate input —
            // shouldn't happen for our fixtures.
            panic!("libxml2 htmlReadMemory returned NULL");
        }
        xmlFreeDoc(doc);
    }
}

// ── no-op TreeSink for the html5ever-skip-sup-xml baseline ──────────────────

/// Minimal TreeSink that discards every node, for measuring the
/// html5ever tokenize+tree-build cost without any DOM bookkeeping.
/// Mirrors the structure of html5ever/examples/noop-tree-builder.rs
/// but with the trait shape from markup5ever 0.39.
struct NoopSink {
    next_id: Cell<usize>,
    names: RefCell<HashMap<usize, &'static QualName>>,
}

impl NoopSink {
    fn new() -> Self {
        Self {
            next_id: Cell::new(1),
            names: RefCell::new(HashMap::new()),
        }
    }

    fn alloc(&self) -> usize {
        let id = self.next_id.get();
        self.next_id.set(id + 2);
        id
    }
}

impl TreeSink for NoopSink {
    type Handle = usize;
    type Output = Self;
    type ElemName<'a>
        = ExpandedName<'a>
    where
        Self: 'a;

    fn finish(self) -> Self {
        self
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}
    fn get_document(&self) -> usize {
        0
    }
    fn elem_name(&self, target: &usize) -> ExpandedName<'_> {
        self.names
            .borrow()
            .get(target)
            .expect("not an element")
            .expanded()
    }
    fn create_element(
        &self,
        name: QualName,
        _attrs: Vec<H5Attribute>,
        _flags: ElementFlags,
    ) -> usize {
        let id = self.alloc();
        // Same memory-leak strategy as html5ever's noop-tree-builder
        // example — fine for a bench that runs once and exits.
        self.names
            .borrow_mut()
            .insert(id, Box::leak(Box::new(name)));
        id
    }
    fn create_comment(&self, _text: StrTendril) -> usize {
        self.alloc()
    }
    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> usize {
        self.alloc()
    }
    fn append(&self, _parent: &usize, _child: NodeOrText<usize>) {}
    fn append_based_on_parent_node(
        &self,
        _element: &usize,
        _prev_element: &usize,
        _new_node: NodeOrText<usize>,
    ) {
    }
    fn append_doctype_to_document(&self, _: StrTendril, _: StrTendril, _: StrTendril) {}
    fn get_template_contents(&self, target: &usize) -> usize {
        target + 1
    }
    fn same_node(&self, x: &usize, y: &usize) -> bool {
        x == y
    }
    fn set_quirks_mode(&self, _mode: H5QuirksMode) {}
    fn append_before_sibling(&self, _sibling: &usize, _new_node: NodeOrText<usize>) {}
    fn add_attrs_if_missing(&self, target: &usize, _attrs: Vec<H5Attribute>) {
        debug_assert!(self.names.borrow().contains_key(target));
    }
    fn remove_from_parent(&self, _target: &usize) {}
    fn reparent_children(&self, _node: &usize, _new_parent: &usize) {}
}

// ── fixture loading ───────────────────────────────────────────────────────────

const FIXTURES: &[&str] = &[
    "hn.html",
    "mdn_table.html",
    "stackoverflow_rust.html",
    "bbc_news.html",
    "github_rust.html",
    "wikipedia_rust.html",
    "wikipedia_languages.html",
    "wikipedia_ww2.html",
    "guardian.html",
];

fn fixture_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/assets/html")
        .join(name)
}

fn load_fixture(name: &str) -> Option<Vec<u8>> {
    std::fs::read(fixture_path(name)).ok()
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_secs(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

// ── bench entry point ─────────────────────────────────────────────────────────

fn bench_html(c: &mut Criterion) {
    // Same env-var conventions as the XML benches:
    //   SUPXML_BENCH_SAMPLES=20 cargo bench -p sup-xml-bench --bench html_parse
    //   SUPXML_BENCH_TIME=3
    //   SUPXML_BENCH_WARMUP=1
    let sample_size = env_usize("SUPXML_BENCH_SAMPLES", 10).max(10);
    let meas_secs = env_secs("SUPXML_BENCH_TIME", 1.5);
    let warm_secs = env_secs("SUPXML_BENCH_WARMUP", 1.0);

    let mut group = c.benchmark_group("html-parse");
    group.sample_size(sample_size);
    group.measurement_time(std::time::Duration::from_secs_f64(meas_secs));
    group.warm_up_time(std::time::Duration::from_secs_f64(warm_secs));

    let mut found = 0;
    let mut skipped: Vec<&str> = Vec::new();
    // (label, byte_len) for every fixture we actually benched.  Drives
    // the post-run summary below — see `print_summary`.
    let mut processed: Vec<(&'static str, usize)> = Vec::new();

    for &fixture in FIXTURES {
        let bytes = match load_fixture(fixture) {
            Some(b) => b,
            None => {
                skipped.push(fixture);
                continue;
            }
        };
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());

        let text: &'static str = match std::str::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => {
                eprintln!("⚠ skipping {fixture}: not valid UTF-8");
                continue;
            }
        };
        found += 1;

        let label: &'static str = fixture.trim_end_matches(".html");
        processed.push((label, bytes.len()));
        group.throughput(Throughput::Bytes(bytes.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("sup-xml", label),
            &text,
            |b, &input| b.iter(|| bench_sup_xml_html(input)),
        );
        group.bench_with_input(
            BenchmarkId::new("html5ever-skip-sup-xml", label),
            &text,
            |b, &input| b.iter(|| bench_html5ever_skip_sup_xml(input)),
        );
        group.bench_with_input(
            BenchmarkId::new("libxml2", label),
            &bytes,
            |b, &input| b.iter(|| bench_libxml2_html(input)),
        );
    }

    group.finish();

    if !processed.is_empty() {
        print_summary(&processed);
    }

    if found == 0 {
        eprintln!();
        eprintln!("⚠ NO HTML FIXTURES FOUND");
        eprintln!(
            "  Fetch them with: tests/assets/html/fetch.sh\n  \
             (Bench is wired correctly but produced no measurements.)"
        );
    } else if !skipped.is_empty() {
        eprintln!();
        eprintln!(
            "ℹ {} of {} fixtures missing — skipped: {:?}",
            skipped.len(),
            FIXTURES.len(),
            skipped
        );
        eprintln!("  Fetch missing ones with: tests/assets/html/fetch.sh");
    }
}

// ── post-run summary ──────────────────────────────────────────────────────────
//
// Reads the `mean.point_estimate` (ns/iter) Criterion just wrote to
// target/criterion/html-parse/<fn>/<label>/new/estimates.json for each
// (target, fixture) pair, converts to MB/s, and prints one row per
// fixture plus a geomean row.  Ratios are throughput ratios (sx_mbps /
// other_mbps, equivalent to other_ns / sx_ns) — a value >1 means
// sup-xml is faster on that fixture.
//
// String-searches the JSON for `mean … point_estimate` rather than
// pulling in serde_json — the file shape is stable and the substring
// is unambiguous.  If a file is missing (rare; only if Criterion was
// filtered or a target failed) the cell shows "n/a" and the row is
// excluded from the geomean.

fn print_summary(processed: &[(&'static str, usize)]) {
    let crit_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("criterion")
        .join("html-parse");

    println!();
    println!("── HTML parse summary (MB/s, higher is better) ──────────────────────────────────");
    println!(
        "{:<22}{:>10}{:>12}{:>10}{:>11}{:>11}",
        "fixture", "sup-xml", "html5ever*", "libxml2", "sx vs lx", "sx vs h5e*",
    );

    let mut sx_vs_lx: Vec<f64> = Vec::new();
    let mut sx_vs_h5: Vec<f64> = Vec::new();

    for &(label, bytes) in processed {
        let bytes_f = bytes as f64;
        let mbps = |ns: f64| (bytes_f / (ns / 1e9)) / 1_000_000.0;

        let sx_ns = read_mean_ns(&crit_root.join("sup-xml").join(label));
        let h5_ns = read_mean_ns(&crit_root.join("html5ever-skip-sup-xml").join(label));
        let lx_ns = read_mean_ns(&crit_root.join("libxml2").join(label));

        let cell = |ns: Option<f64>| match ns {
            Some(n) => format!("{:.0}", mbps(n)),
            None    => "n/a".to_string(),
        };
        // Throughput ratio: sx_mbps / other_mbps = other_ns / sx_ns.
        // >1 means sup-xml is faster on this fixture.
        let ratio = |sx: Option<f64>, other: Option<f64>| match (sx, other) {
            (Some(s), Some(o)) => format!("{:.2}x", o / s),
            _                  => "—".to_string(),
        };

        println!(
            "{:<22}{:>10}{:>12}{:>10}{:>11}{:>11}",
            label,
            cell(sx_ns),
            cell(h5_ns),
            cell(lx_ns),
            ratio(sx_ns, lx_ns),
            ratio(sx_ns, h5_ns),
        );

        if let (Some(s), Some(l)) = (sx_ns, lx_ns) {
            sx_vs_lx.push(l / s);
        }
        if let (Some(s), Some(h)) = (sx_ns, h5_ns) {
            sx_vs_h5.push(h / s);
        }
    }

    let geomean = |v: &[f64]| -> Option<f64> {
        if v.is_empty() {
            return None;
        }
        let sum: f64 = v.iter().map(|x| x.ln()).sum();
        Some((sum / v.len() as f64).exp())
    };
    let fmt_geo = |g: Option<f64>| g.map(|x| format!("{:.2}x", x)).unwrap_or_else(|| "—".into());

    println!(
        "{:<22}{:>10}{:>12}{:>10}{:>11}{:>11}",
        "geomean",
        "",
        "",
        "",
        fmt_geo(geomean(&sx_vs_lx)),
        fmt_geo(geomean(&sx_vs_h5)),
    );
    println!();
    println!(
        "* html5ever driven by a no-op TreeSink (discards every node) — calibration"
    );
    println!(
        "  baseline, not a config any real consumer ships.  Ratios are throughput:"
    );
    println!(
        "  >1 means sup-xml is faster on that fixture."
    );
}

/// Extract `mean.point_estimate` (ns/iter) from a Criterion bench's
/// `new/estimates.json`.  Returns `None` if the file is missing or
/// the field isn't parseable.
fn read_mean_ns(bench_dir: &std::path::Path) -> Option<f64> {
    let path = bench_dir.join("new").join("estimates.json");
    let text = std::fs::read_to_string(&path).ok()?;
    // The JSON layout starts with `{"mean":{...,"point_estimate":<num>,...}`.
    // We scan from the start of `"mean":` so we pick up the mean's
    // point_estimate and not (e.g.) the median's.
    let mean_at = text.find("\"mean\":")?;
    let pe_key  = "\"point_estimate\":";
    let pe_at   = text[mean_at..].find(pe_key)? + mean_at + pe_key.len();
    let tail    = &text[pe_at..];
    let end = tail.find(|c: char| {
        !(c.is_ascii_digit() || c == '-' || c == '+' || c == '.' || c == 'e' || c == 'E')
    })?;
    tail[..end].parse().ok()
}

criterion_group!(benches, bench_html);
criterion_main!(benches);
