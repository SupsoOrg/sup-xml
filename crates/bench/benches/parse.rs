//! Parsing benchmarks: sup-xml vs xml-rs vs quick-xml vs libxml2.
//!
//! Run with:
//!   cargo bench -p sup-xml-bench
//!
//! HTML reports land in target/criterion/.
//!
//! # Apples-to-apples notes
//!
//! - **sup-xml** and **libxml2** both build a full in-memory DOM tree.
//! - **xml-rs** and **quick-xml** are SAX/event-stream parsers — they never
//!   allocate a tree, so they have a structural speed advantage.  The
//!   comparison still matters: it shows what you pay for a full DOM.

use std::fmt::Write as FmtWrite;
use std::os::raw::{c_char, c_int, c_void};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

// ── libxml2 FFI ───────────────────────────────────────────────────────────────
// The library is linked by build.rs (via pkg-config).  We only need two
// functions: parse-from-memory and free.

unsafe extern "C" {
    fn xmlParseMemory(buffer: *const c_char, size: c_int) -> *mut c_void;
    fn xmlFreeDoc(cur: *mut c_void);
}

// ── fixture generation ────────────────────────────────────────────────────────

/// Generate a catalog XML document with `n` book entries.
///
/// Each entry has attributes, several child elements with text content, and
/// repeated tag names — a realistic mix for exercising name interning, attribute
/// parsing, and element recursion.
fn generate_catalog(n: usize) -> String {
    const CATEGORIES: &[&str] = &["fiction", "non-fiction", "science", "history", "biography"];
    const LANGS: &[&str] = &["en", "fr", "de", "es", "pt"];

    let mut xml = String::with_capacity(n * 230);
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<catalog version=\"1.0\">\n");
    for i in 0..n {
        let cat  = CATEGORIES[i % CATEGORIES.len()];
        let lang = LANGS[i % LANGS.len()];
        writeln!(
            xml,
            "  <book id=\"{i}\" category=\"{cat}\" xml:lang=\"{lang}\">\n    \
             <title>Book Title {i}</title>\n    \
             <author last=\"Surname{}\" first=\"Forename{}\"/>\n    \
             <year>{}</year>\n    \
             <price currency=\"USD\">{}.{:02}</price>\n    \
             <description>A detailed description of book number {i}.</description>\n  \
             </book>",
            i % 100, i % 50,
            2000 + i % 24,
            10 + i % 90, i % 100,
        ).unwrap();
    }
    xml.push_str("</catalog>\n");
    xml
}

/// UTF-8 → UTF-16 BE bytes (with BOM).
fn utf8_to_utf16be(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2 + 2);
    out.push(0xFE);
    out.push(0xFF);
    let mut buf = [0u16; 2];
    for c in s.chars() {
        for u in c.encode_utf16(&mut buf) {
            out.push((*u >> 8) as u8);
            out.push(*u as u8);
        }
    }
    out
}

/// UTF-8 → IBM037 bytes, using an inverse of the IBM037 → Unicode table.
/// Characters outside the IBM037 repertoire are substituted with `?` (0x6F).
fn utf8_to_ibm037(s: &str) -> Vec<u8> {
    use sup_xml_core::encoding::IBM037_TO_UNICODE;
    // Build the inverse map once.  IBM037 has 256 entries so this is fast.
    let mut inv = [0u8; 0x100];        // for ASCII / low Latin-1 range only
    let mut inv_high = std::collections::HashMap::<u32, u8>::new();
    for (i, &cp) in IBM037_TO_UNICODE.iter().enumerate() {
        if (cp as u32) < 0x100 {
            inv[cp as usize] = i as u8;
        } else {
            inv_high.insert(cp as u32, i as u8);
        }
    }
    // For codepoints in the low Latin-1 range that don't have an entry yet
    // (rare in IBM037 but possible), use the substitution char.
    let sub = 0x6Fu8; // '?' in IBM037
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let cp = c as u32;
        if cp < 0x100 {
            let b = inv[cp as usize];
            out.push(if b == 0 && cp != IBM037_TO_UNICODE[0] as u32 { sub } else { b });
        } else if let Some(&b) = inv_high.get(&cp) {
            out.push(b);
        } else {
            out.push(sub);
        }
    }
    out
}

// ── benchmark helpers ─────────────────────────────────────────────────────────

// M2: arena DOM is the only DOM.  The bench helpers run against the
// arena entry points; the legacy v1 path has been removed.
fn bench_sup_xml(bytes: &[u8]) {
    let opts = sup_xml::ParseOptions::default();
    let doc = sup_xml_core::parse_bytes(bytes, &opts).expect("sup-xml parse failed");
    criterion::black_box(doc);
}

fn bench_sup_xml_unchecked(bytes: &[u8]) {
    // SAFETY: fixture is valid UTF-8.
    let opts = sup_xml::ParseOptions::default();
    let doc = unsafe { sup_xml_core::parse_bytes_unchecked(bytes, &opts) }
        .expect("sup-xml parse failed");
    criterion::black_box(doc);
}

/// DOM with auto-encoding detection.  For UTF-8 input this adds a ~100-byte
/// detection step and otherwise behaves like [`bench_sup_xml`].  For
/// non-UTF-8 input (e.g. ISO-8859-1) it transcodes to UTF-8 first.
fn bench_sup_xml_transcoded(bytes: &[u8]) {
    let opts = sup_xml::ParseOptions::default();
    let utf8 = sup_xml::encoding::transcode_to_utf8(bytes).expect("transcode failed");
    let doc  = sup_xml_core::parse_bytes(&utf8, &opts).expect("sup-xml parse failed");
    criterion::black_box(doc);
}

fn bench_xml_rs(bytes: &[u8]) {
    use xml::reader::{EventReader, XmlEvent};
    let mut n = 0usize;
    for event in EventReader::new(bytes).into_iter().flatten() {
        if matches!(event, XmlEvent::StartElement { .. }) {
            n += 1;
        }
    }
    criterion::black_box(n);
}

fn bench_libxml2(bytes: &[u8]) {
    unsafe {
        let doc = xmlParseMemory(bytes.as_ptr() as *const c_char, bytes.len() as c_int);
        assert!(!doc.is_null(), "libxml2 parse failed");
        xmlFreeDoc(doc);
    }
}

fn bench_sup_xml_sax(bytes: &[u8]) {
    use sup_xml::{Event, XmlReader};
    let mut reader = XmlReader::from_bytes(bytes).expect("valid UTF-8");
    let mut n = 0usize;
    loop {
        match reader.next().unwrap() {
            Event::StartElement { .. } => n += 1,
            Event::Eof => break,
            _ => {}
        }
    }
    criterion::black_box(n);
}

fn bench_sup_xml_sax_unchecked(bytes: &[u8]) {
    use sup_xml::{Event, XmlReader};
    // SAFETY: fixture is valid UTF-8.
    let mut reader = unsafe { XmlReader::from_bytes_unchecked(bytes) };
    let mut n = 0usize;
    loop {
        match reader.next().unwrap() {
            Event::StartElement { .. } => n += 1,
            Event::Eof => break,
            _ => {}
        }
    }
    criterion::black_box(n);
}

/// SAX with auto-encoding detection.  See [`bench_sup_xml_transcoded`] for
/// the rationale.
fn bench_sup_xml_sax_transcoded(bytes: &[u8]) {
    use sup_xml::{Event, XmlReader};
    let utf8 = sup_xml::encoding::transcode_to_utf8(bytes).expect("transcode failed");
    let mut reader = XmlReader::from_bytes(&utf8).expect("valid UTF-8 after transcode");
    let mut n = 0usize;
    loop {
        match reader.next().unwrap() {
            Event::StartElement { .. } => n += 1,
            Event::Eof => break,
            _ => {}
        }
    }
    criterion::black_box(n);
}

fn bench_quick_xml(bytes: &[u8]) {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut reader = Reader::from_reader(bytes);
    let mut n = 0usize;
    loop {
        match reader.read_event().unwrap() {
            Event::Start(_) => n += 1,
            Event::Eof => break,
            _ => {}
        }
    }
    criterion::black_box(n);
}

// ── parser selection ──────────────────────────────────────────────────────────

/// Short tags for each parser, used in the `SUPXML_BENCH_PARSERS` env var.
const ALL_PARSERS: &[&str] = &[
    "dom", "dom-unchecked", "dom-transcoded",
    "libxml2",
    "sax", "sax-unchecked", "sax-transcoded",
    "quick-xml", "xml-rs",
];

/// Parsers enabled by default (xml-rs is opt-in — it's an order of magnitude
/// slower than everything else and inflates total bench time on the big
/// fixtures).
const DEFAULT_PARSERS: &[&str] = &[
    "dom", "dom-unchecked", "dom-transcoded",
    "libxml2",
    "sax", "sax-unchecked", "sax-transcoded",
    "quick-xml",
];

/// Parsers that accept non-UTF-8 input.  Only libxml2 (native iconv) and our
/// `*-transcoded` variants survive non-UTF-8 fixtures; everything else returns
/// or panics on the upfront UTF-8 check, so we skip those benches for files
/// like `transitions_tutorial.xml` (declared ISO-8859-1).
fn parser_accepts_non_utf8(tag: &str) -> bool {
    matches!(tag, "libxml2" | "dom-transcoded" | "sax-transcoded")
}

/// Return the set of parsers to bench, as resolved from the
/// `SUPXML_BENCH_PARSERS` env var.
///
/// Accepted values:
/// * unset → [`DEFAULT_PARSERS`]
/// * `"all"` → every parser including xml-rs
/// * comma-separated list of tags (`"dom,sax,quick-xml"`) → exactly those
fn enabled_parsers() -> std::collections::HashSet<String> {
    use std::collections::HashSet;
    match std::env::var("SUPXML_BENCH_PARSERS").as_deref() {
        Err(_)        => DEFAULT_PARSERS.iter().map(|s| s.to_string()).collect(),
        Ok("all")     => ALL_PARSERS.iter().map(|s| s.to_string()).collect(),
        Ok(list) => {
            let want: HashSet<&str> = list.split(',').map(str::trim).collect();
            for tag in &want {
                if !ALL_PARSERS.contains(tag) {
                    panic!(
                        "SUPXML_BENCH_PARSERS: unknown parser tag '{tag}' \
                         (valid: {})",
                        ALL_PARSERS.join(", ")
                    );
                }
            }
            want.into_iter().map(|s| s.to_string()).collect()
        }
    }
}

// ── criterion groups ──────────────────────────────────────────────────────────

fn bench_parse(c: &mut Criterion) {
    let enabled = enabled_parsers();

    let sizes: &[(usize, &str)] = &[
        (1_000,  "1k-books"),
        (5_000,  "5k-books"),
    ];

    let mut group = c.benchmark_group("parse");

    for &(n, label) in sizes {
        let xml   = generate_catalog(n);
        let bytes = xml.as_bytes();

        group.throughput(Throughput::Bytes(bytes.len() as u64));

        if enabled.contains("dom") {
            group.bench_with_input(
                BenchmarkId::new("sup-xml", label),
                bytes,
                |b, bytes| b.iter(|| bench_sup_xml(bytes)),
            );
        }

        if enabled.contains("sax") {
            group.bench_with_input(
                BenchmarkId::new("sup-xml (SAX)", label),
                bytes,
                |b, bytes| b.iter(|| bench_sup_xml_sax(bytes)),
            );
        }

        if enabled.contains("xml-rs") {
            group.bench_with_input(
                BenchmarkId::new("xml-rs (SAX)", label),
                bytes,
                |b, bytes| b.iter(|| bench_xml_rs(bytes)),
            );
        }

        if enabled.contains("libxml2") {
            group.bench_with_input(
                BenchmarkId::new("libxml2", label),
                bytes,
                |b, bytes| b.iter(|| bench_libxml2(bytes)),
            );
        }

        if enabled.contains("quick-xml") {
            group.bench_with_input(
                BenchmarkId::new("quick-xml (SAX)", label),
                bytes,
                |b, bytes| b.iter(|| bench_quick_xml(bytes)),
            );
        }
    }

    group.finish();
}

// ── real-world corpus ─────────────────────────────────────────────────────────
//
// Six files downloaded from public sources, covering structurally different XML:
//
//   osm.xml        OpenStreetMap – many small nodes, dense attribute sets
//   pubmed.xml     PubMed biomedical articles – deeply nested, mixed content
//   dblp.xml       DBLP bibliography – repeated flat records, many namespaces
//   cldr-en.xml    Unicode CLDR locale data – hierarchical, many short elements
//   wikipedia.xml  Wikipedia MediaWiki export – long text nodes, mixed content
//   maven-pom.xml  Apache Maven POM – dependency tree, attribute-light

fn load_asset(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/assets/xml")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {name}: {e}"))
}

/// Read a small positive integer from an env var, or fall back to `default`.
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

/// Read a float (seconds) from an env var, or fall back to `default`.
fn env_secs(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn bench_real_world(c: &mut Criterion) {
    // Fast defaults so the full corpus finishes in a few minutes instead of
    // ~20.  Override via env vars when you want tighter confidence intervals:
    //
    //   SUPXML_BENCH_SAMPLES=100  cargo bench -p sup-xml-bench -- real-world
    //   SUPXML_BENCH_TIME=5       cargo bench -p sup-xml-bench -- real-world
    //   SUPXML_BENCH_WARMUP=3     cargo bench -p sup-xml-bench -- real-world
    //
    // `sample_size` must be >= 10 (criterion enforces this).
    let sample_size = env_usize("SUPXML_BENCH_SAMPLES", 10).max(10);
    let meas_secs   = env_secs("SUPXML_BENCH_TIME", 1.0);
    let warm_secs   = env_secs("SUPXML_BENCH_WARMUP", 1.0);

    let corpus: &[(&str, &str)] = &[
        ("321gone.xml",        "321gone"),
        ("1831893.xml",        "1831893"),
        ("bargains_he_5.xml",  "bargains_he"),
        ("chinese1.xml",       "chinese1"),
        ("cldr_en.xml",      "cldr"),
        ("customer1.xml",       "customer1"),
        ("dblp.xml",         "dblp"),
        ("ebay.xml",         "ebay"),
        ("gazali_maqasid_ar.xml", "gazali_maqasid_ar"),
        ("maven-pom.xml",    "maven-pom"),
        ("nasa.xml",          "nasa"),
        ("osm.xml",          "osm"),
        ("podcast_episode_2024_03.xml", "podcast_episode_2024_03"),
        ("pubmed.xml",       "pubmed"),
        ("sitemap.xml",       "sitemap"),
        ("swiss_prot.xml",     "swiss_prot"),
        // ISO-8859-1: only libxml2 and sup-xml's `*-transcoded` benches survive
        // this file; the UTF-8-only paths are skipped at the per-fixture check
        // below.
        ("transitions_tutorial.xml", "transitions_tutorial"),
        ("ubid.xml",          "ubid"),
        ("utah_legislature_2024.xml", "utah_legislature_2024"),
        ("uwm.xml",           "uwm"),
        ("wikipedia_ww2.xml", "wikipedia"),
        ("yahoo.xml",         "yahoo"),
    ];

    let enabled = enabled_parsers();

    // Synthetic non-UTF-8 fixtures generated from a UTF-8 catalog and
    // re-encoded.  Exercises the transcoded paths and libxml2's native
    // multi-encoding handling — quick-xml and our non-transcoded paths are
    // skipped because they can't decode these bytes.
    //
    // The source keeps its `<?xml ... encoding="UTF-8"?>` declaration so that
    // the EBCDIC bytes start with the autodetect signature `4C 6F A7 94` (=
    // "<?xm" in IBM037).  After transcoding back to UTF-8 the embedded
    // declaration matches the actual byte encoding, so the parser is happy.
    let synthetic_utf8 = generate_catalog(500);
    let synthetic_utf16be: &'static [u8] =
        Box::leak(utf8_to_utf16be(&synthetic_utf8).into_boxed_slice());
    let synthetic_ebcdic: &'static [u8] =
        Box::leak(utf8_to_ibm037(&synthetic_utf8).into_boxed_slice());

    // (bytes, label) pairs we feed into the main loop alongside file fixtures.
    let synthetic: &[(&'static [u8], &str)] = &[
        (synthetic_utf16be, "synthetic-utf16be"),
        (synthetic_ebcdic,  "synthetic-ebcdic"),
    ];

    let mut group = c.benchmark_group("real-world");
    group.sample_size(sample_size);
    group.measurement_time(std::time::Duration::from_secs_f64(meas_secs));
    group.warm_up_time(std::time::Duration::from_secs_f64(warm_secs));

    // Treat file fixtures and synthetic ones uniformly: both produce a
    // `(bytes, label)` tuple consumed by the same loop body below.
    let file_fixtures: Vec<(&'static [u8], &str)> = corpus
        .iter()
        .map(|&(filename, label)| {
            let bytes: Vec<u8> = load_asset(filename);
            let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
            (bytes, label)
        })
        .collect();

    for &(bytes, label) in file_fixtures.iter().chain(synthetic.iter()) {

        // One quick UTF-8 check per fixture.  Parsers that can't handle
        // non-UTF-8 input (everything except libxml2 and our `*-transcoded`
        // variants) are skipped for that file so the bench doesn't panic.
        let is_utf8 = std::str::from_utf8(bytes).is_ok();
        let run = |tag: &str| enabled.contains(tag) && (is_utf8 || parser_accepts_non_utf8(tag));

        group.throughput(Throughput::Bytes(bytes.len() as u64));

        if run("dom") {
            group.bench_with_input(
                BenchmarkId::new("sup-xml (DOM)", label),
                bytes,
                |b, bytes| b.iter(|| bench_sup_xml(bytes)),
            );
        }

        if run("dom-unchecked") {
            group.bench_with_input(
                BenchmarkId::new("sup-xml (DOM, unchecked)", label),
                bytes,
                |b, bytes| b.iter(|| bench_sup_xml_unchecked(bytes)),
            );
        }

        if run("dom-transcoded") {
            group.bench_with_input(
                BenchmarkId::new("sup-xml (DOM, transcoded)", label),
                bytes,
                |b, bytes| b.iter(|| bench_sup_xml_transcoded(bytes)),
            );
        }

        if run("libxml2") {
            group.bench_with_input(
                BenchmarkId::new("libxml2", label),
                bytes,
                |b, bytes| b.iter(|| bench_libxml2(bytes)),
            );
        }

        if run("sax") {
            group.bench_with_input(
                BenchmarkId::new("sup-xml (SAX)", label),
                bytes,
                |b, bytes| b.iter(|| bench_sup_xml_sax(bytes)),
            );
        }

        if run("sax-unchecked") {
            group.bench_with_input(
                BenchmarkId::new("sup-xml (SAX, unchecked)", label),
                bytes,
                |b, bytes| b.iter(|| bench_sup_xml_sax_unchecked(bytes)),
            );
        }

        if run("sax-transcoded") {
            group.bench_with_input(
                BenchmarkId::new("sup-xml (SAX, transcoded)", label),
                bytes,
                |b, bytes| b.iter(|| bench_sup_xml_sax_transcoded(bytes)),
            );
        }

        if run("quick-xml") {
            group.bench_with_input(
                BenchmarkId::new("quick-xml (SAX)", label),
                bytes,
                |b, bytes| b.iter(|| bench_quick_xml(bytes)),
            );
        }

        if run("xml-rs") {
            group.bench_with_input(
                BenchmarkId::new("xml-rs (SAX)", label),
                bytes,
                |b, bytes| b.iter(|| bench_xml_rs(bytes)),
            );
        }
    }

    group.finish();
}

criterion_group!(benches, bench_parse, bench_real_world);
criterion_main!(benches);
