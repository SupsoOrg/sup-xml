//! # Head-to-head bench
//!
//! For each competitor XML library we benchmark, this file prints a focused
//! three-column table:
//!
//! * **sup-xml (matched)** — `ParseOptions` dialled to *what we believe the
//!   competitor's safety/validation contract is*, so the comparison is
//!   apples-to-apples.
//! * **competitor** — the library at its own defaults (with any flags that
//!   are universally on, e.g. `roxmltree`'s `allow_dtd: true` so it can read
//!   the same files at all).
//! * **sup-xml (default)** — our parser at its own defaults, included for
//!   context.  Shows what users pay (or save) by running us safely.
//!
//! Each table is preceded by a paragraph explaining our reading of the
//! competitor's contract and the exact options we flipped to match.  The
//! `ratio` column is `sup-xml-matched / competitor` — `>1.00` = we're
//! faster at the same game.
//!
//! Run with the same env knobs as `mini`:
//!     SUPXML_MINI_ITERS=10 cargo bench -p sup-xml-bench --bench head_to_head

use std::ffi::c_void;
use std::os::raw::{c_char, c_int};
use std::time::Instant;

use sup_xml::encoding::IBM037_TO_UNICODE;
// M2: `sup_xml::parse_bytes_unchecked` IS the arena unchecked entry now.
use sup_xml::parse_bytes_unchecked as parse_bytes_unchecked;

// ── libxml2 FFI shim (matches mini.rs) ───────────────────────────────────────

#[allow(non_camel_case_types)]
enum XmlDoc {}
unsafe extern "C" {
    fn xmlParseMemory(buffer: *const c_char, size: c_int) -> *mut XmlDoc;
    fn xmlFreeDoc(doc: *mut XmlDoc);
}

// ── pugixml FFI shim (see crates/bench/pugixml_shim.cc) ──────────────────────
//
// pugixml is C++-only; build.rs compiles a small C++ translation unit that
// re-exports its parse/walk/free entry points with extern "C" linkage.

unsafe extern "C" {
    fn pugixml_bench_parse(buf: *const c_char, len: usize) -> *mut c_void;
    fn pugixml_bench_walk(doc: *mut c_void) -> usize;
    fn pugixml_bench_free(doc: *mut c_void);
}

// ── sup-xml runners ─────────────────────────────────────────────────────────

fn run_sup_xml_default(bytes: &[u8]) {
    // Arena DOM is the only DOM in M2.
    let opts = sup_xml::ParseOptions::default();
    let doc = sup_xml_core::parse_bytes(bytes, &opts).expect("sup-xml parse failed");
    std::hint::black_box(doc);
}

/// sup-xml's bumpalo-backed arena DOM (M2 DOM, see crates/tree/src/arena.rs).
/// Builds a libxml2-shaped tree with `&'doc str` strings — no `Arc<str>`
/// interning, no per-node `malloc`.  Same validation contract as the default
/// path (entity expansion, end-tag check, XML 1.0 § 2.2, name validation).
/// SAFETY: bench harness verifies UTF-8 of every fixture before dispatch.
fn run_sup_xml_arena_default(bytes: &[u8]) {
    let opts = sup_xml::ParseOptions::default();
    let doc = unsafe { parse_bytes_unchecked(bytes, &opts) }
        .expect("sup-xml arena parse failed");
    std::hint::black_box(doc);
}

/// Arena DOM with the same skip-flags as `run_sup_xml_dom_match_roxmltree`
/// — matches the "lighter contract" parsers (roxmltree, pugixml) for an
/// apples-to-apples DOM build.  This is the right number to compare against
/// pugixml: our best DOM (bumpalo arena) with matched validation.
fn run_sup_xml_arena_match_light(bytes: &[u8]) {
    use sup_xml::ParseOptions;
    let opts = ParseOptions {
        skip_xml_char_validation: true,
        skip_name_validation:     true,
        skip_end_tag_check:       true,
        ..ParseOptions::default()
    };
    let doc = unsafe { parse_bytes_unchecked(bytes, &opts) }
        .expect("sup-xml arena parse failed");
    std::hint::black_box(doc);
}

/// sup-xml configured to match roxmltree's reading of the document:
/// trust input UTF-8, skip XML 1.0 § 2.2 char validation, skip Name
/// validation, skip end-tag-matches-start-tag verification.  Same arena
/// DOM build as `run_sup_xml_arena_match_light`.
fn run_sup_xml_dom_match_roxmltree(bytes: &[u8]) {
    use sup_xml::ParseOptions;
    let opts = ParseOptions {
        skip_xml_char_validation: true,
        skip_name_validation: true,
        skip_end_tag_check: true,
        ..ParseOptions::default()
    };
    // SAFETY: bench harness already checked UTF-8 of every fixture.
    let doc = unsafe { parse_bytes_unchecked(bytes, &opts) }
        .expect("sup-xml parse failed");
    std::hint::black_box(doc);
}

fn run_sup_xml_sax_default(bytes: &[u8]) {
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
    std::hint::black_box(n);
}

/// sup-xml configured to match quick-xml's default contract: trust UTF-8,
/// skip § 2.2, skip Name validation, skip end-tag check, and leave entity
/// references unexpanded in text events.  Closest analogue to what
/// `quick_xml::Reader::read_event` actually produces.
fn run_sup_xml_sax_match_qxml_raw(bytes: &[u8]) {
    use sup_xml::{Event, ParseOptions, XmlReader};
    // SAFETY: bench harness verified UTF-8 already.
    let mut reader = unsafe { XmlReader::from_bytes_unchecked(bytes) }
        .with_options(ParseOptions {
            skip_xml_char_validation: true,
            skip_name_validation: true,
            skip_end_tag_check: true,
            skip_entity_expansion: true,
            // quick-xml's `Attributes` iterator with `with_checks: true`
            // (their default) only catches duplicate names + unquoted
            // values, and only when the caller iterates.  For an
            // apples-to-apples bytes comparison we disable our own
            // eager attribute validation pass too — both parsers then
            // do the same per-tag work.  See COMPARISON.md and the
            // `qxml_attr_validation_check` bench.
            skip_attr_validation: true,
            ..ParseOptions::default()
        });
    let mut n = 0usize;
    loop {
        match reader.next().unwrap() {
            Event::StartElement { .. } => n += 1,
            Event::Eof => break,
            _ => {}
        }
    }
    std::hint::black_box(n);
}

/// sup-xml's *byte-events* reader configured to match quick-xml's default
/// contract — same skip-flags as `run_sup_xml_sax_match_qxml_raw` but uses
/// `XmlBytesReader` directly, so events carry `Cow<'src, [u8]>` payloads
/// instead of `Cow<'src, str>`.  This is the apples-to-apples comparison
/// against quick-xml's raw event API.
fn run_sup_xml_bytes_match_qxml_raw(bytes: &[u8]) {
    use sup_xml::{BytesEvent, ParseOptions, XmlBytesReader};
    // SAFETY: bench harness verified UTF-8 already.
    let mut reader = unsafe { XmlBytesReader::from_bytes_unchecked(bytes) }
        .with_options(ParseOptions {
            skip_xml_char_validation: true,
            skip_name_validation: true,
            skip_end_tag_check: true,
            skip_entity_expansion: true,
            // quick-xml's `Attributes` iterator with `with_checks: true`
            // (their default) only catches duplicate names + unquoted
            // values, and only when the caller iterates.  For an
            // apples-to-apples bytes comparison we disable our own
            // eager attribute validation pass too — both parsers then
            // do the same per-tag work.  See COMPARISON.md and the
            // `qxml_attr_validation_check` bench.
            skip_attr_validation: true,
            ..ParseOptions::default()
        });
    let mut n = 0usize;
    loop {
        match reader.next().unwrap() {
            BytesEvent::StartElement { .. } => n += 1,
            BytesEvent::Eof => break,
            _ => {}
        }
    }
    std::hint::black_box(n);
}

/// sup-xml byte-events with `skip_inter_element_whitespace: true` —
/// the contract that matches quick-xml's `Reader::trim_text(true)`.
/// Pure-whitespace runs between tags don't emit Text events on either
/// side, so per-event counts line up.
fn run_sup_xml_bytes_match_qxml_trim(bytes: &[u8]) {
    use sup_xml::{BytesEvent, ParseOptions, XmlBytesReader};
    let mut reader = unsafe { XmlBytesReader::from_bytes_unchecked(bytes) }
        .with_options(ParseOptions {
            skip_xml_char_validation: true,
            skip_name_validation: true,
            skip_end_tag_check: true,
            skip_entity_expansion: true,
            skip_inter_element_whitespace: true,
            // see other matched runners — apples-to-apples on attrs.
            skip_attr_validation: true,
            ..ParseOptions::default()
        });
    let mut n = 0usize;
    loop {
        match reader.next().unwrap() {
            BytesEvent::StartElement { .. } => n += 1,
            BytesEvent::Eof => break,
            _ => {}
        }
    }
    std::hint::black_box(n);
}

// ── competitor runners ───────────────────────────────────────────────────────

fn run_libxml2(bytes: &[u8]) {
    unsafe {
        let doc = xmlParseMemory(bytes.as_ptr() as *const c_char, bytes.len() as c_int);
        assert!(!doc.is_null(), "libxml2 parse failed");
        xmlFreeDoc(doc);
    }
}

/// pugixml at its default settings (`parse_default`, UTF-8): expands the 5
/// builtin entities, normalises newlines, decodes CDATA, normalises attribute
/// whitespace — same shape of DOM build as libxml2/sup-xml-default.  The
/// walk + element count keeps the optimiser honest (the parse has to actually
/// build a tree we then traverse).
fn run_pugixml(bytes: &[u8]) {
    unsafe {
        let doc = pugixml_bench_parse(bytes.as_ptr() as *const c_char, bytes.len());
        assert!(!doc.is_null(), "pugixml parse failed");
        let n = pugixml_bench_walk(doc);
        std::hint::black_box(n);
        pugixml_bench_free(doc);
    }
}

fn run_roxmltree(bytes: &[u8]) {
    let s = std::str::from_utf8(bytes).expect("not UTF-8");
    let opt = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(s, opt).expect("roxmltree parse failed");
    let n = doc.descendants().count();
    std::hint::black_box(n);
}

/// xmloxide (Document::parse_bytes):  a pure-Rust libxml2 reimplementation.
/// Detects encoding, transcodes to UTF-8, runs a full W3C XML 1.0 (5th ed.)
/// well-formedness parse, expands the builtin entities, and builds an
/// arena-allocated owned DOM — the same validation contract as libxml2 and
/// sup-xml's default path.  The descendants walk + count keeps the optimiser
/// honest (the parse has to produce a tree we then traverse).
fn run_xmloxide(bytes: &[u8]) {
    let doc = xmloxide::Document::parse_bytes(bytes).expect("xmloxide parse failed");
    let n = doc.descendants(doc.root()).count();
    std::hint::black_box(n);
}

fn run_quick_xml_raw(bytes: &[u8]) {
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
    std::hint::black_box(n);
}

/// quick-xml with `trim_text(true)` — suppresses pure-whitespace text
/// events between tags.  Paired with sup-xml's
/// `skip_inter_element_whitespace: true` for an equal-event-count
/// comparison on indented data XML.
fn run_quick_xml_trim(bytes: &[u8]) {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut n = 0usize;
    loop {
        match reader.read_event().unwrap() {
            Event::Start(_) => n += 1,
            Event::Eof => break,
            _ => {}
        }
    }
    std::hint::black_box(n);
}

/// quick-xml with the UTF-8 validation a real user pays to turn each event's
/// `&[u8]` payload into `&str`.  See mini.rs for rationale.
fn run_quick_xml_validated(bytes: &[u8]) {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut reader = Reader::from_reader(bytes);
    let mut n = 0usize;
    loop {
        match reader.read_event().unwrap() {
            Event::Start(e) | Event::Empty(e) => {
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).expect("name not UTF-8");
                std::hint::black_box(name);
                for attr in e.attributes() {
                    let attr = attr.unwrap();
                    let key_q = attr.key;
                    let key = std::str::from_utf8(key_q.as_ref()).expect("attr key");
                    let val = std::str::from_utf8(&attr.value).expect("attr val");
                    std::hint::black_box((key, val));
                }
                n += 1;
            }
            Event::End(e) => {
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).expect("end name");
                std::hint::black_box(name);
            }
            Event::Text(t)    => { std::hint::black_box(std::str::from_utf8(&t).expect("text")); }
            Event::CData(c)   => { std::hint::black_box(std::str::from_utf8(&c).expect("cdata")); }
            Event::Comment(c) => { std::hint::black_box(std::str::from_utf8(&c).expect("comment")); }
            Event::PI(p)      => { std::hint::black_box(std::str::from_utf8(&p).expect("pi")); }
            Event::Eof => break,
            _ => {}
        }
    }
    std::hint::black_box(n);
}

fn run_xml_rs(bytes: &[u8]) {
    use xml::reader::{EventReader, XmlEvent};
    let mut n = 0usize;
    for event in EventReader::new(bytes).into_iter().flatten() {
        if matches!(event, XmlEvent::StartElement { .. }) {
            n += 1;
        }
    }
    std::hint::black_box(n);
}

/// anyxml uses the Java SAX-style handler model.  We supply a
/// counting handler that ticks on every `start_element` callback —
/// equivalent work to the other competitors counting StartElement
/// events.  anyxml's `parse_str` takes `&str`, so we go through
/// `from_utf8` (the bench harness has already validated the bytes).
fn run_anyxml(bytes: &[u8]) {
    use anyxml::sax::{
        Attributes as AnyAttrs, EntityResolver as AnyEntityResolver,
        ErrorHandler as AnyErrorHandler, SAXHandler as AnySAXHandler,
        XMLReader as AnyXMLReader,
    };

    #[derive(Default)]
    struct Counter { n: usize }
    impl AnyEntityResolver for Counter {}
    impl AnyErrorHandler for Counter {}
    impl AnySAXHandler for Counter {
        fn start_element(&mut self, _: Option<&str>, _: Option<&str>, _: &str, _: &AnyAttrs) {
            self.n += 1;
        }
    }

    let s = std::str::from_utf8(bytes).expect("not UTF-8");
    let mut reader = AnyXMLReader::builder()
        .set_handler(Counter::default())
        .build();
    let _ = reader.parse_str(s, None);
    std::hint::black_box(reader.handler.n);
}

// ── fixtures (copy of mini.rs's loader; kept local to avoid wiring a shared
//    module across bench binaries) ────────────────────────────────────────────

fn load_asset(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/assets/xml")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {name}: {e}"))
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let names: &[&str] = &[
        "321gone.xml", "1831893.xml", "bargains_he_5.xml", "chinese1.xml",
        "cldr_en.xml", "customer1.xml", "dblp.xml", "ebay.xml",
        "gazali_maqasid_ar.xml", "maven-pom.xml", "nasa.xml", "osm.xml",
        "podcast_episode_2024_03.xml", "pubmed.xml", "sitemap.xml",
        "swiss_prot.xml", "transitions_tutorial.xml", "ubid.xml",
        "utah_legislature_2024.xml", "uwm.xml", "wikipedia_ww2.xml", "yahoo.xml",
    ];
    let mut out: Vec<(String, Vec<u8>)> = names.iter().map(|n| {
        let label = n.trim_end_matches(".xml").to_string();
        (label, load_asset(n))
    }).collect();
    let sample = generate_sample_xml(300);
    out.push(("synthetic-utf16be".into(), utf8_to_utf16be(&sample)));
    out.push(("synthetic-ebcdic".into(), utf8_to_ibm037(&sample)));
    out
}

fn generate_sample_xml(n: usize) -> String {
    let mut s = String::with_capacity(n * 120);
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<catalog>\n");
    for i in 0..n {
        s.push_str(&format!(
            "  <book id=\"{i}\"><title>Title {i}</title><author>Author {}</author></book>\n",
            i % 50,
        ));
    }
    s.push_str("</catalog>\n");
    s
}

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

fn utf8_to_ibm037(s: &str) -> Vec<u8> {
    let mut inv = [0u8; 0x100];
    for (i, &cp) in IBM037_TO_UNICODE.iter().enumerate() {
        if (cp as u32) < 0x100 {
            inv[cp as usize] = i as u8;
        }
    }
    let sub = 0x6Fu8;
    s.chars().map(|c| {
        let cp = c as u32;
        if cp < 0x100 {
            let b = inv[cp as usize];
            if b == 0 && cp != IBM037_TO_UNICODE[0] as u32 { sub } else { b }
        } else {
            sub
        }
    }).collect()
}

// ── timing harness ───────────────────────────────────────────────────────────

fn time_best_of(f: fn(&[u8]), bytes: &[u8], iters: usize) -> std::time::Duration {
    let mut best = std::time::Duration::MAX;
    for _ in 0..iters {
        let t0 = Instant::now();
        f(bytes);
        let elapsed = t0.elapsed();
        if elapsed < best { best = elapsed; }
    }
    best
}

fn mb_per_sec(bytes: usize, t: std::time::Duration) -> f64 {
    let secs = t.as_secs_f64();
    if secs <= 0.0 { 0.0 } else { (bytes as f64) / secs / 1_048_576.0 }
}

// ── one head-to-head table ───────────────────────────────────────────────────

struct Pair {
    title:       &'static str,
    explanation: &'static [&'static str],  // paragraph, one element per line
    matched_col: &'static str,
    default_col: &'static str,
    other_col:   &'static str,
    matched_fn:  fn(&[u8]),
    default_fn:  fn(&[u8]),
    other_fn:    fn(&[u8]),
    accepts_non_utf8: bool,
    /// Return `false` to print "-" in the competitor + ratio columns for a
    /// fixture the competitor genuinely can't handle (e.g. xmloxide rejecting
    /// a document that exceeds its 10000-entity-expansion cap).  sup-xml's own
    /// columns are still timed and shown.  `None` means "handles everything".
    other_supports: Option<fn(&[u8]) -> bool>,
}

fn print_pair(pair: &Pair, fixtures: &[(String, Vec<u8>)], iters: usize) {
    println!();
    println!("== {} ==", pair.title);
    for line in pair.explanation { println!("   {line}"); }
    println!();
    print!("{:<32}", "fixture");
    print!("  {:>22}", pair.matched_col);
    print!("  {:>22}", pair.other_col);
    print!("  {:>10}", "ratio");
    print!("  {:>22}", pair.default_col);
    println!();
    print!("{:<32}", "-".repeat(32));
    print!("  {:>22}", "-".repeat(22));
    print!("  {:>22}", "-".repeat(22));
    print!("  {:>10}", "-".repeat(10));
    print!("  {:>22}", "-".repeat(22));
    println!();

    for (label, bytes) in fixtures {
        let is_utf8 = std::str::from_utf8(bytes).is_ok();
        print!("{:<32}", format!("{} ({}KB)", label, bytes.len() / 1024));
        if !is_utf8 && !pair.accepts_non_utf8 {
            print!("  {:>22}", "-");
            print!("  {:>22}", "-");
            print!("  {:>10}", "-");
            print!("  {:>22}", "-");
            println!();
            continue;
        }
        let other_ok = pair.other_supports.is_none_or(|supports| supports(bytes));
        let t_matched = time_best_of(pair.matched_fn, bytes, iters);
        let t_default = time_best_of(pair.default_fn, bytes, iters);
        let m = mb_per_sec(bytes.len(), t_matched);
        let d = mb_per_sec(bytes.len(), t_default);
        print!("  {:>17.0} MB/s", m);
        if other_ok {
            let t_other = time_best_of(pair.other_fn, bytes, iters);
            let o = mb_per_sec(bytes.len(), t_other);
            let ratio = if o > 0.0 { m / o } else { 0.0 };
            print!("  {:>17.0} MB/s", o);
            print!("  {:>9.2}x", ratio);
        } else {
            print!("  {:>22}", "-");
            print!("  {:>10}", "-");
        }
        print!("  {:>17.0} MB/s", d);
        println!();
    }
}

// ── main ─────────────────────────────────────────────────────────────────────

fn main() {
    let iters: usize = std::env::var("SUPXML_MINI_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let fixtures = fixtures();

    println!("# head-to-head bench — best of {iters} iter(s) each, no warmup");
    println!("# columns: sup-xml (matched to competitor) | competitor | ratio | sup-xml (default, safer)");

    let overall_start = Instant::now();

    print_pair(&Pair {
        title: "sup-xml DOM (arena)  vs  libxml2",
        explanation: &[
            "libxml2 (xmlParseMemory):  validates UTF-8, validates XML 1.0 § 2.2 chars,",
            "expands general entities, enforces end-tag matching, builds an owned tree.",
            "Our default behaviour matches all of these, so 'matched' = 'default' here.",
            "",
            "Both columns use sup-xml's bumpalo-backed arena DOM (M2; see",
            "crates/tree/src/arena.rs).  This is the architecturally fair DOM",
            "comparison — the legacy DOM (parse_bytes) loses ~30% to per-node",
            "malloc/free and Arc interning that the arena path eliminates.",
        ],
        matched_col: "sup-xml (arena)",
        default_col: "sup-xml (arena)",
        other_col:   "libxml2",
        matched_fn:  run_sup_xml_arena_default,
        default_fn:  run_sup_xml_arena_default,
        other_fn:    run_libxml2,
        accepts_non_utf8: false,
        other_supports: None,
    }, &fixtures, iters);

    print_pair(&Pair {
        title: "sup-xml DOM (arena)  vs  xmloxide",
        explanation: &[
            "xmloxide (Document::parse_bytes):  a pure-Rust reimplementation of",
            "libxml2, positioned — like sup-xml — as a memory-safe drop-in",
            "replacement.  Detects encoding, transcodes to UTF-8, runs a full",
            "W3C XML 1.0 (5th ed.) well-formedness parse, expands the builtin",
            "entities, and builds an arena-allocated owned DOM.  Its contract",
            "matches libxml2 and sup-xml's default, so 'matched' = 'default'.",
            "",
            "This is the most apples-to-apples Rust comparison we have: both",
            "sides are full-validation arena-DOM parsers with the same goal.",
            "Both columns use sup-xml's bumpalo arena DOM (crates/tree/src/arena.rs).",
        ],
        matched_col: "sup-xml (arena)",
        default_col: "sup-xml (arena)",
        other_col:   "xmloxide",
        matched_fn:  run_sup_xml_arena_default,
        default_fn:  run_sup_xml_arena_default,
        other_fn:    run_xmloxide,
        accepts_non_utf8: false,
        // xmloxide caps entity expansion at 10000 and rejects documents that
        // exceed it (e.g. chinese1.xml), which libxml2 and sup-xml both accept.
        other_supports: Some(|b| xmloxide::Document::parse_bytes(b).is_ok()),
    }, &fixtures, iters);

    print_pair(&Pair {
        title: "sup-xml DOM (arena)  vs  pugixml",
        explanation: &[
            "pugixml (xml_document::load_buffer, parse_default, encoding_utf8):  expands",
            "the 5 builtin entities, normalises newlines, decodes CDATA into text, and",
            "trims attribute whitespace.  No DTD/external-entity resolution.  Famously",
            "fast: pool allocator, in-place destructive parsing (overwrites the input",
            "buffer with decoded text), pointer-into-buffer node/attribute storage.",
            "",
            "pugixml's parse contract is lighter than libxml2's (no UTF-8 validation,",
            "no XML 1.0 § 2.2 char validation, no name validation) — closer to the",
            "roxmltree column.  We still compare against sup-xml's full-validation",
            "arena DOM here so the ratio shows the *full* gap a strict parser pays.",
            "The walk + element-count keeps the optimiser honest.",
        ],
        matched_col: "sup-xml (arena)",
        default_col: "sup-xml (arena)",
        other_col:   "pugixml",
        matched_fn:  run_sup_xml_arena_default,
        default_fn:  run_sup_xml_arena_default,
        other_fn:    run_pugixml,
        accepts_non_utf8: false,
        other_supports: None,
    }, &fixtures, iters);

    // Apples-to-apples take: sup-xml's arena DOM dialled down to pugixml's
    // lighter contract (skip UTF-8/§2.2/name validation, skip end-tag check).
    // This is the architecturally fair number against pugixml — our best DOM
    // layout with matched validation work.
    print_pair(&Pair {
        title: "sup-xml DOM (arena, matched)  vs  pugixml",
        explanation: &[
            "Apples-to-apples: sup-xml's arena DOM configured with the same",
            "skip-flags as the roxmltree comparison (skip_xml_char_validation,",
            "skip_name_validation, skip_end_tag_check) so both parsers do the",
            "same validation work.  Entities expanded on both sides.",
            "",
            "The remaining gap, if any, is now genuine engine perf: tokenizer",
            "tightness, DOM-construction overhead, and string-copy cost.  The",
            "biggest single architectural lever still on our side is borrowing",
            "text/attr-value slices from the input buffer instead of copying",
            "them into the arena (pugixml's in-place trick).",
        ],
        matched_col: "sup-xml (arena+matched)",
        default_col: "sup-xml (arena)",
        other_col:   "pugixml",
        matched_fn:  run_sup_xml_arena_match_light,
        default_fn:  run_sup_xml_arena_default,
        other_fn:    run_pugixml,
        accepts_non_utf8: false,
        other_supports: None,
    }, &fixtures, iters);

    print_pair(&Pair {
        title: "sup-xml DOM  vs  roxmltree",
        explanation: &[
            "roxmltree (Document::parse_with_options{allow_dtd:true}):  validates only",
            "what Rust's `&str` already enforces (UTF-8); does NOT run XML 1.0 § 2.2",
            "char validation, does NOT enforce end-tag-matches-start-tag, does NOT",
            "expand DTD-defined general entities (no resolver supplied).",
            "",
            "Matched sup-xml uses parse_bytes_opts_unchecked with",
            "  skip_xml_char_validation: true,  skip_name_validation: true,",
            "  skip_end_tag_check:       true.",
            "(We still expand entities — there is no DOM-level skip_entity_expansion.)",
        ],
        matched_col: "sup-xml (matched)",
        default_col: "sup-xml (default)",
        other_col:   "roxmltree",
        matched_fn:  run_sup_xml_dom_match_roxmltree,
        default_fn:  run_sup_xml_default,
        other_fn:    run_roxmltree,
        accepts_non_utf8: false,
        other_supports: None,
    }, &fixtures, iters);

    print_pair(&Pair {
        title: "sup-xml SAX (str events)  vs  quick-xml (raw bytes)",
        explanation: &[
            "quick_xml::Reader (default):  no UTF-8 validation, no § 2.2 check, no",
            "Name validation, no end-tag matching, no entity expansion.  Events carry",
            "raw `&[u8]` slices into the source; the caller decodes on demand.",
            "",
            "Matched sup-xml uses XmlReader::from_bytes_unchecked with",
            "  skip_xml_char_validation: true,  skip_name_validation: true,",
            "  skip_end_tag_check:       true,  skip_entity_expansion:  true,",
            "  skip_attr_validation:     true.",
            "Events are `Cow<'src, str>` — UTF-8 is type-system-enforced via the",
            "unchecked entry point's safety contract.  See the next table for the",
            "true apples-to-apples comparison using XmlBytesReader (byte events).",
        ],
        matched_col: "sup-xml (matched)",
        default_col: "sup-xml (default)",
        other_col:   "quick-xml (raw)",
        matched_fn:  run_sup_xml_sax_match_qxml_raw,
        default_fn:  run_sup_xml_sax_default,
        other_fn:    run_quick_xml_raw,
        accepts_non_utf8: false,
        other_supports: None,
    }, &fixtures, iters);

    print_pair(&Pair {
        title: "sup-xml XmlBytesReader  vs  quick-xml (raw bytes)",
        explanation: &[
            "Apples-to-apples byte-events comparison.  Both readers produce raw",
            "`Cow<'src, [u8]>` event payloads (no UTF-8 cast on the way out) and",
            "skip every validation check that has a flag for it.",
            "",
            "Matched sup-xml uses XmlBytesReader::from_bytes_unchecked with the",
            "same four skip-flags as the str-events comparison above.  The only",
            "structural difference vs that table is the event payload type:",
            "`Cow<'src, [u8]>` instead of `Cow<'src, str>` — both end up as the",
            "same bytes in memory but the byte path avoids a `from_utf8_unchecked`",
            "cast at the event boundary.",
        ],
        matched_col: "sup-xml (bytes)",
        default_col: "sup-xml (default)",
        other_col:   "quick-xml (raw)",
        matched_fn:  run_sup_xml_bytes_match_qxml_raw,
        default_fn:  run_sup_xml_sax_default,
        other_fn:    run_quick_xml_raw,
        accepts_non_utf8: false,
        other_supports: None,
    }, &fixtures, iters);

    print_pair(&Pair {
        title: "sup-xml XmlBytesReader (skip ws)  vs  quick-xml (trim_text)",
        explanation: &[
            "Both parsers configured to drop pure-whitespace text events between",
            "tags — the contract data-XML consumers (SOAP, RSS, POMs, configs)",
            "actually want.  This is the apples-to-apples bytes comparison once",
            "the per-event count matches.",
            "",
            "Matched sup-xml: same flags as the previous bytes table plus",
            "  skip_inter_element_whitespace: true.",
            "Other side:  quick_xml::Reader::config_mut().trim_text(true).",
        ],
        matched_col: "sup-xml (bytes+trim)",
        default_col: "sup-xml (default)",
        other_col:   "quick-xml (trim)",
        matched_fn:  run_sup_xml_bytes_match_qxml_trim,
        default_fn:  run_sup_xml_sax_default,
        other_fn:    run_quick_xml_trim,
        accepts_non_utf8: false,
        other_supports: None,
    }, &fixtures, iters);

    print_pair(&Pair {
        title: "sup-xml SAX  vs  quick-xml (validated by caller)",
        explanation: &[
            "quick_xml + the std::str::from_utf8 calls a real user pays to turn each",
            "event's `&[u8]` payload into `&str`.  Closer to apples-to-apples than the",
            "raw column above, since sup-xml events are already `Cow<'src, str>`.",
            "",
            "No special options needed on our side — our default SAX does the",
            "equivalent work eagerly.",
        ],
        matched_col: "sup-xml (default)",
        default_col: "sup-xml (default)",
        other_col:   "quick-xml (val)",
        matched_fn:  run_sup_xml_sax_default,
        default_fn:  run_sup_xml_sax_default,
        other_fn:    run_quick_xml_validated,
        accepts_non_utf8: false,
        other_supports: None,
    }, &fixtures, iters);

    print_pair(&Pair {
        title: "sup-xml SAX  vs  xml-rs",
        explanation: &[
            "xml-rs (EventReader):  validates UTF-8, validates well-formedness,",
            "expands entities, returns owned `String`s for event payloads.  Roughly",
            "the same contract as our default SAX.",
        ],
        matched_col: "sup-xml (default)",
        default_col: "sup-xml (default)",
        other_col:   "xml-rs",
        matched_fn:  run_sup_xml_sax_default,
        default_fn:  run_sup_xml_sax_default,
        other_fn:    run_xml_rs,
        accepts_non_utf8: false,
        other_supports: None,
    }, &fixtures, iters);

    print_pair(&Pair {
        title: "sup-xml SAX  vs  anyxml",
        explanation: &[
            "anyxml: a Java SAX-style XML library positioned by its author as",
            "\"fully spec-conformant\" — it passes the XML 1.0 conformance test",
            "suite, including every well-formedness check sup-xml currently",
            "implements (and several it doesn't, see COMPARISON.md).  Author",
            "self-reports ~1.8x slower than quick-xml as the cost of strictness.",
            "",
            "No skip-flag matching needed: anyxml validates everything always.",
            "Roughly the same contract as our default SAX.",
        ],
        matched_col: "sup-xml (default)",
        default_col: "sup-xml (default)",
        other_col:   "anyxml",
        matched_fn:  run_sup_xml_sax_default,
        default_fn:  run_sup_xml_sax_default,
        other_fn:    run_anyxml,
        accepts_non_utf8: false,
        other_supports: None,
    }, &fixtures, iters);

    println!();
    println!("# total wall time: {:.1}s", overall_start.elapsed().as_secs_f64());
}
