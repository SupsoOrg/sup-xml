//! Fast dev-loop bench: runs each enabled parser × fixture pair a small fixed
//! number of times (default 2, env `SUPXML_MINI_ITERS`) and prints a clean
//! MB/s table.  No criterion machinery, no warmup, no statistics — just
//! quick numbers so you can validate that a code change moved the needle
//! before paying for a full criterion sweep.
//!
//! Run:
//!     cargo bench -p sup-xml-bench --bench mini
//!     SUPXML_MINI_ITERS=5         cargo bench -p sup-xml-bench --bench mini
//!     SUPXML_BENCH_PARSERS=sax-unchecked,quick-xml cargo bench -p sup-xml-bench --bench mini
//!     SUPXML_BENCH_PARSERS=dom,libxml2 SUPXML_BENCH_FIXTURE=cldr_en \
//!         cargo bench -p sup-xml-bench --bench mini
//!
//! The `SUPXML_BENCH_PARSERS` env var is shared with the criterion `parse`
//! bench, so the same selection works here.  `SUPXML_BENCH_FIXTURE` is
//! mini-only — comma-separated label prefixes; matches against the fixture
//! names listed in `fn fixtures()`.

use std::io::Write;
use std::os::raw::{c_char, c_int, c_void};
use std::time::Instant;

// ── libxml2 FFI (duplicated from parse.rs since bench targets can't share code via src/) ──

unsafe extern "C" {
    fn xmlParseMemory(buffer: *const c_char, size: c_int) -> *mut c_void;
    fn xmlFreeDoc(cur: *mut c_void);
}

// ── parser drivers (mirror parse.rs's helpers; kept here verbatim to avoid sharing trickery) ──

// M2: arena DOM is the only DOM; `sup_xml::parse_bytes` and the
// `sup_xml_core::parse_bytes*` family are equivalent.
fn run_sup_xml(bytes: &[u8]) {
    let opts = sup_xml::ParseOptions::default();
    let doc = sup_xml_core::parse_bytes(bytes, &opts).expect("sup-xml parse failed");
    std::hint::black_box(doc);
}

fn run_sup_xml_unchecked(bytes: &[u8]) {
    // SAFETY: caller already established the fixture is UTF-8.
    let opts = sup_xml::ParseOptions::default();
    let doc = unsafe { sup_xml_core::parse_bytes_unchecked(bytes, &opts) }
        .expect("sup-xml parse failed");
    std::hint::black_box(doc);
}

fn run_sup_xml_transcoded(bytes: &[u8]) {
    let opts = sup_xml::ParseOptions::default();
    let utf8 = sup_xml::encoding::transcode_to_utf8(bytes).expect("transcode failed");
    let doc  = sup_xml_core::parse_bytes(&utf8, &opts).expect("sup-xml parse failed");
    std::hint::black_box(doc);
}

fn run_sup_xml_sax(bytes: &[u8]) {
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

fn run_sup_xml_sax_unchecked(bytes: &[u8]) {
    use sup_xml::{Event, XmlReader};
    // SAFETY: caller verified UTF-8.
    let mut reader = unsafe { XmlReader::from_bytes_unchecked(bytes) };
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

/// SAX + lazy entity expansion: `Event::Text` carries the raw source slice
/// with `&amp;` etc. left unexpanded.  Same API contract as quick-xml's
/// default (raw text, decode-on-demand) but with UTF-8 still validated.
fn run_sup_xml_sax_raw(bytes: &[u8]) {
    use sup_xml::{Event, ParseOptions, XmlReader};
    let mut reader = XmlReader::from_bytes(bytes).expect("valid UTF-8")
        .with_options(ParseOptions {
            skip_entity_expansion: true,
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

/// SAX + unchecked + lax validation — closest analogue to quick-xml's mode
/// (no UTF-8 check, no name validation, no end-tag matching).
fn run_sup_xml_sax_lax(bytes: &[u8]) {
    use sup_xml::{Event, ParseOptions, XmlReader};
    // SAFETY: caller verified UTF-8.
    let mut reader = unsafe { XmlReader::from_bytes_unchecked(bytes) }
        .with_options(ParseOptions {
            skip_name_validation: true,
            skip_end_tag_check:   true,
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

fn run_sup_xml_sax_transcoded(bytes: &[u8]) {
    use sup_xml::{Event, XmlReader};
    let utf8 = sup_xml::encoding::transcode_to_utf8(bytes).expect("transcode failed");
    let mut reader = XmlReader::from_bytes(&utf8).expect("valid UTF-8");
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

fn run_libxml2(bytes: &[u8]) {
    unsafe {
        let doc = xmlParseMemory(bytes.as_ptr() as *const c_char, bytes.len() as c_int);
        assert!(!doc.is_null(), "libxml2 parse failed");
        xmlFreeDoc(doc);
    }
}

fn run_quick_xml(bytes: &[u8]) {
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

/// quick-xml + the UTF-8 validation a real user pays to turn events into
/// `&str` — closer to "apples to apples" against `sup-xml-sax`, whose
/// events are already valid `Cow<'src, str>`.  quick-xml hands out raw
/// `&[u8]` for element names, attribute names/values, and text content;
/// none of those bytes have been UTF-8-checked, so any code that wants to
/// log, compare, JSON-encode, etc. must `std::str::from_utf8` each one.
/// That cost is the gap our default SAX bench was paying for the user.
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
                // Attribute names + values as &str (lazy iterator, same shape
                // as our Attrs).
                for attr in e.attributes() {
                    let attr = attr.unwrap();
                    let key_q = attr.key;
                    let key = std::str::from_utf8(key_q.as_ref()).expect("attr key not UTF-8");
                    let val = std::str::from_utf8(&attr.value).expect("attr value not UTF-8");
                    std::hint::black_box((key, val));
                }
                n += 1;
            }
            Event::End(e) => {
                let qname = e.name();
                let name = std::str::from_utf8(qname.as_ref()).expect("end name not UTF-8");
                std::hint::black_box(name);
            }
            Event::Text(t) => {
                let s = std::str::from_utf8(&t).expect("text not UTF-8");
                std::hint::black_box(s);
            }
            Event::CData(c) => {
                let s = std::str::from_utf8(&c).expect("cdata not UTF-8");
                std::hint::black_box(s);
            }
            Event::Comment(c) => {
                let s = std::str::from_utf8(&c).expect("comment not UTF-8");
                std::hint::black_box(s);
            }
            Event::PI(p) => {
                let s = std::str::from_utf8(&p).expect("pi not UTF-8");
                std::hint::black_box(s);
            }
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

/// roxmltree — pull-based DOM (read-only tree).  Parses to a `Document`
/// holding a flat arena of nodes plus borrowed `&str` references into the
/// input.  Treats input as `&str`, so the caller pays UTF-8 validation up
/// front (we use `std::str::from_utf8` to mirror what a real user would do).
///
/// Enables `allow_dtd: true` so fixtures with a DTD subset don't get
/// rejected.  roxmltree still doesn't *expand* general entities defined
/// in the subset (no entity-resolver supplied), but it accepts the
/// presence of the subset itself.
fn run_roxmltree(bytes: &[u8]) {
    let s = std::str::from_utf8(bytes).expect("not UTF-8");
    let opt = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(s, opt)
        .expect("roxmltree parse failed");
    // Walk the tree to make sure parsing isn't lazily deferred.
    let n = doc.descendants().count();
    std::hint::black_box(n);
}

fn always_supports(_: &[u8]) -> bool { true }

// ── parser registry ──────────────────────────────────────────────────────────

/// (env tag, display name, accepts non-UTF-8?, run fn, supports check).
type Parser = (&'static str, &'static str, bool, fn(&[u8]), fn(&[u8]) -> bool);

const ALL_PARSERS: &[Parser] = &[
    ("dom",            "sup-xml (DOM)",             false, run_sup_xml,                 always_supports),
    ("dom-unchecked",  "sup-xml (DOM, unchecked)",  false, run_sup_xml_unchecked,       always_supports),
    ("dom-transcoded", "sup-xml (DOM, transcoded)", true,  run_sup_xml_transcoded,      always_supports),
    ("libxml2",        "libxml2",                    true,  run_libxml2,                  always_supports),
    ("sax",            "sup-xml (SAX)",             false, run_sup_xml_sax,             always_supports),
    ("sax-unchecked",  "sup-xml (SAX, unchecked)",  false, run_sup_xml_sax_unchecked,   always_supports),
    ("sax-raw",        "sup-xml (SAX, raw)",        false, run_sup_xml_sax_raw,         always_supports),
    ("sax-lax",        "sup-xml (SAX, lax)",        false, run_sup_xml_sax_lax,         always_supports),
    ("sax-transcoded", "sup-xml (SAX, transcoded)", true,  run_sup_xml_sax_transcoded,  always_supports),
    ("quick-xml",      "quick-xml (SAX)",            false, run_quick_xml,                always_supports),
    ("quick-xml-val",  "quick-xml (SAX, validated)", false, run_quick_xml_validated,      always_supports),
    ("xml-rs",         "xml-rs (SAX)",               false, run_xml_rs,                   always_supports),
    ("roxmltree",      "roxmltree (DOM)",            false, run_roxmltree,                always_supports),
];

const DEFAULT_TAGS: &[&str] = &[
    "dom", "dom-unchecked", "dom-transcoded",
    "libxml2",
    "sax", "sax-unchecked", "sax-raw", "sax-lax", "sax-transcoded",
    "quick-xml", "quick-xml-val",
    "roxmltree",
    // xml-rs intentionally excluded — orders of magnitude slower on
    // large fixtures (10×+ on swiss_prot), pads bench runtime without
    // adding signal.  Opt in with SUPXML_BENCH_PARSERS=...,xml-rs.
];

fn enabled_tags() -> std::collections::HashSet<String> {
    match std::env::var("SUPXML_BENCH_PARSERS").as_deref() {
        Err(_)    => DEFAULT_TAGS.iter().map(|s| s.to_string()).collect(),
        Ok("all") => ALL_PARSERS.iter().map(|(t, ..)| t.to_string()).collect(),
        Ok(list)  => list.split(',').map(|s| s.trim().to_string()).collect(),
    }
}

// ── fixtures ─────────────────────────────────────────────────────────────────

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

    // Synthetic non-UTF-8 fixtures — exercise the transcoded paths.  Generated
    // here so we don't have to commit binary files; the source is a small
    // ASCII XML, re-encoded as UTF-16 BE and IBM037 EBCDIC.
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
    use sup_xml_core::encoding::IBM037_TO_UNICODE;
    let mut inv = [0u8; 0x100];
    for (i, &cp) in IBM037_TO_UNICODE.iter().enumerate() {
        if (cp as u32) < 0x100 {
            inv[cp as usize] = i as u8;
        }
    }
    let sub = 0x6Fu8; // '?' in IBM037
    s.chars().map(|c| {
        let cp = c as u32;
        if cp < 0x100 {
            let b = inv[cp as usize];
            // 0 in inv means "unmapped" unless cp is actually U+0000 mapping
            // to byte 0 (which is correct for IBM037 NUL).
            if b == 0 && cp != IBM037_TO_UNICODE[0] as u32 { sub } else { b }
        } else {
            sub
        }
    }).collect()
}

// ── runner ───────────────────────────────────────────────────────────────────

/// Run `f(bytes)` `iters` times and return the best (fastest) elapsed.  Best
/// of N is the standard "noise floor" estimator — it dampens system
/// interference better than mean over a tiny sample.
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
    (bytes as f64) / t.as_secs_f64() / 1e6
}

fn main() {
    let iters: usize = std::env::var("SUPXML_MINI_ITERS")
        .ok().and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let enabled = enabled_tags();
    let parsers: Vec<&Parser> = ALL_PARSERS.iter()
        .filter(|(tag, ..)| enabled.contains(*tag))
        .collect();

    let fixtures = {
        let all = fixtures();
        match std::env::var("SUPXML_BENCH_FIXTURE").ok() {
            None => all,
            Some(filter) => {
                // Comma-separated substrings; a fixture matches if any
                // substring is a prefix or full-name match of its label.
                let wants: Vec<String> = filter.split(',').map(|s| s.trim().to_string()).collect();
                let kept: Vec<_> = all.into_iter()
                    .filter(|(label, _)| wants.iter().any(|w| label.starts_with(w) || label == w))
                    .collect();
                if kept.is_empty() {
                    eprintln!("SUPXML_BENCH_FIXTURE={filter} matched no fixtures; \
                               available labels: see crates/bench/benches/mini.rs fn fixtures()");
                    std::process::exit(2);
                }
                kept
            }
        }
    };

    // Compute total MB to crunch up-front so we can report a real
    // progress percentage per fixture row.  We only count cells we'll
    // actually run — skip non-UTF-8 fixtures for parsers that don't
    // accept them, and rely on `supports` for parser-level skips.
    let total_bytes_to_process: u64 = fixtures.iter().map(|(_, bytes)| {
        let is_utf8 = std::str::from_utf8(bytes).is_ok();
        let cells = parsers.iter().filter(|(_, _, accepts_non_utf8, _, supports)| {
            (is_utf8 || *accepts_non_utf8) && supports(bytes)
        }).count();
        (bytes.len() as u64) * (cells as u64) * (iters as u64)
    }).sum();
    let total_mb = (total_bytes_to_process as f64) / 1e6;

    println!("# mini bench — best of {iters} iter(s) each, no warmup");
    println!("# total work: {:.1} MB across {} fixture(s) × {} parser(s)",
             total_mb, fixtures.len(), parsers.len());
    println!();
    print!("{:<32}", "fixture");
    for (_, name, ..) in &parsers { print!("  {:>22}", name); }
    print!("  {:>6}", "%");
    println!();
    print!("{:<32}", "-".repeat(32));
    for _ in &parsers { print!("  {:>22}", "-".repeat(22)); }
    print!("  {:>6}", "-".repeat(6));
    println!();
    let _ = std::io::stdout().flush();

    let overall_start = Instant::now();
    let mut bytes_done: u64 = 0;
    for (label, bytes) in &fixtures {
        let is_utf8 = std::str::from_utf8(bytes).is_ok();
        print!("{:<32}", format!("{} ({}KB)", label, bytes.len() / 1024));
        for &(_, _, accepts_non_utf8, f, supports) in &parsers {
            if !is_utf8 && !accepts_non_utf8 {
                print!("  {:>22}", "-");
                continue;
            }
            if !supports(bytes) {
                print!("  {:>22}", "-");
                continue;
            }
            let t = time_best_of(*f, bytes, iters);
            print!("  {:>17.0} MB/s", mb_per_sec(bytes.len(), t));
            bytes_done += (bytes.len() as u64) * (iters as u64);
            // Flush so streamed-to-file output shows progress as we go,
            // instead of dumping the whole table at process exit.
            let _ = std::io::stdout().flush();
        }
        let pct = if total_bytes_to_process == 0 { 100.0 }
                  else { 100.0 * (bytes_done as f64) / (total_bytes_to_process as f64) };
        print!("  {:>5.1}%", pct);
        println!();
        let _ = std::io::stdout().flush();
    }
    println!();
    println!("# total wall time: {:.1}s", overall_start.elapsed().as_secs_f64());
}
