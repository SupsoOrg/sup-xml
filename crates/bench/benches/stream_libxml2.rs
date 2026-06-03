//! Streaming-parser head-to-head: sup-xml's [`XmlByteStreamReader`] vs the
//! two streaming parsers people actually reach for today —
//!
//! * **libxml2's `xmlReader`** (`xmlTextReader` API): the canonical C
//!   streaming pull parser, what every other-language XML library
//!   wraps under the hood.
//! * **quick-xml's `Reader`**: the dominant pure-Rust streaming XML
//!   parser, used by `serde-xml-rs`, OOXML readers, RSS/Atom feed
//!   crates, and most "I need to walk a big XML file" Rust code.
//!
//! When a document is bigger than RAM (PubMed Central baseline dumps,
//! UniProt SwissProt, OOXML pptx, XBRL filings, GovTrack) slurping
//! isn't an option — the parser has to pull bytes on demand and
//! discard them once consumed.
//!
//! The bench has two halves, intended to be read together:
//!
//! 1. **Throughput** — drive each parser to EOF over real-world
//!    fixtures (no DOM build) and report MB/s.  Two source shapes per
//!    fixture: `from file` (parser pulls chunks via `read(2)` — the
//!    realistic "doc bigger than RAM" path) and `from memory` (bytes
//!    are already in a `Vec<u8>`; removes I/O, exposes the per-event
//!    state machine cost).
//! 2. **Validation reality check** — feed each parser a small set of
//!    ill-formed inputs that XML 1.0 requires conforming parsers to
//!    reject, and report which ones each parser catches.  This sits in
//!    the same output as the throughput numbers because the two are
//!    coupled: a parser that skips well-formedness checks runs faster
//!    by definition — it's doing less work.  The throughput column and
//!    the spec-compliance column have to be read together; ranking
//!    parsers on speed alone ranks "how much of XML 1.0 did the
//!    parser decide to skip."
//!
//! Run:
//!     cargo bench -p sup-xml-bench --bench stream_libxml2
//!     SUPXML_STREAM_HH_ITERS=5 cargo bench -p sup-xml-bench --bench stream_libxml2

#![allow(clippy::missing_safety_doc)]

use std::ffi::CString;
use std::fs::File;
use std::io::BufReader;
use std::os::raw::{c_char, c_int, c_void};
use std::time::{Duration, Instant};

use sup_xml::{XmlByteStreamReader, DEFAULT_BUFFER_SIZE};

// ── libxml2 xmlReader FFI ────────────────────────────────────────────────────

type XmlTextReaderPtr = *mut c_void;

unsafe extern "C" {
    fn xmlInitParser();

    fn xmlReaderForFile(
        filename: *const c_char,
        encoding: *const c_char,
        options:  c_int,
    ) -> XmlTextReaderPtr;

    fn xmlReaderForMemory(
        buffer:   *const c_char,
        size:     c_int,
        url:      *const c_char,
        encoding: *const c_char,
        options:  c_int,
    ) -> XmlTextReaderPtr;

    fn xmlTextReaderRead(reader: XmlTextReaderPtr) -> c_int;
    fn xmlFreeTextReader(reader: XmlTextReaderPtr);

    fn xmlSetGenericErrorFunc(ctx: *mut c_void, handler: Option<unsafe extern "C" fn()>);
}

/// libxml2's generic error handler is variadic in C; ignoring the
/// trailing varargs is sound here because libxml2 owns argument
/// cleanup.  Swallows all diagnostics so the bench output stays clean.
unsafe extern "C" fn libxml2_swallow() {}

fn libxml2_silence_once() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        xmlInitParser();
        xmlSetGenericErrorFunc(std::ptr::null_mut(), Some(libxml2_swallow));
    });
}

// ── runners: drive each parser to EOF ────────────────────────────────────────

/// `Ok(node_count)` on success.  Counts are informational; libxml2 and
/// quick-xml expose one node/event per call, sup-xml's `validate()`
/// doesn't expose a count and reports `0` here.
type RunResult = Result<u64, String>;

// sup-xml ──

fn run_sup_xml_file(path: &str) -> RunResult {
    let f = File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    let size_hint = f.metadata().ok().map(|m| m.len() as usize);
    let r = XmlByteStreamReader::with_size_hint(f, size_hint, DEFAULT_BUFFER_SIZE)
        .map_err(|e| format!("construct: {e}"))?;
    r.validate().map_err(|e| format!("validate: {e}"))?;
    Ok(0)
}

fn run_sup_xml_mem(bytes: &[u8]) -> RunResult {
    let r = XmlByteStreamReader::with_size_hint(bytes, Some(bytes.len()), DEFAULT_BUFFER_SIZE)
        .map_err(|e| format!("construct: {e}"))?;
    r.validate().map_err(|e| format!("validate: {e}"))?;
    Ok(0)
}

// libxml2 ──

fn run_libxml2_file(path: &str) -> RunResult {
    libxml2_silence_once();
    let cpath = CString::new(path).map_err(|e| format!("path: {e}"))?;
    unsafe {
        let reader = xmlReaderForFile(cpath.as_ptr(), std::ptr::null(), 0);
        if reader.is_null() {
            return Err(format!("xmlReaderForFile returned null for {path}"));
        }
        let mut nodes: u64 = 0;
        loop {
            match xmlTextReaderRead(reader) {
                1  => nodes += 1,
                0  => break,
                _  => {
                    xmlFreeTextReader(reader);
                    return Err("xmlTextReaderRead returned -1".into());
                }
            }
        }
        xmlFreeTextReader(reader);
        Ok(nodes)
    }
}

fn run_libxml2_mem(bytes: &[u8]) -> RunResult {
    libxml2_silence_once();
    unsafe {
        let reader = xmlReaderForMemory(
            bytes.as_ptr() as *const c_char,
            bytes.len() as c_int,
            std::ptr::null(),
            std::ptr::null(),
            0,
        );
        if reader.is_null() {
            return Err("xmlReaderForMemory returned null".into());
        }
        let mut nodes: u64 = 0;
        loop {
            match xmlTextReaderRead(reader) {
                1  => nodes += 1,
                0  => break,
                _  => {
                    xmlFreeTextReader(reader);
                    return Err("xmlTextReaderRead returned -1".into());
                }
            }
        }
        xmlFreeTextReader(reader);
        Ok(nodes)
    }
}

// quick-xml ──
//
// quick-xml's `Reader::read_event_into(&mut Vec<u8>)` is the API that
// works uniformly across `&[u8]` and `BufRead` sources (the
// `read_event()` shortcut only exists on the slice specialization).
// We count every event the reader emits — including text/whitespace —
// to keep the workload symmetric with libxml2's per-node count.

fn run_quick_xml_file(path: &str) -> RunResult {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let f = File::open(path).map_err(|e| format!("open {path}: {e}"))?;
    let mut reader = Reader::from_reader(BufReader::new(f));
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut events: u64 = 0;
    loop {
        match reader.read_event_into(&mut buf).map_err(|e| format!("qxml: {e}"))? {
            Event::Eof => break,
            _ => events += 1,
        }
        buf.clear();
    }
    Ok(events)
}

fn run_quick_xml_mem(bytes: &[u8]) -> RunResult {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut reader = Reader::from_reader(bytes);
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut events: u64 = 0;
    loop {
        match reader.read_event_into(&mut buf).map_err(|e| format!("qxml: {e}"))? {
            Event::Eof => break,
            _ => events += 1,
        }
        buf.clear();
    }
    Ok(events)
}

// ── timing harness ───────────────────────────────────────────────────────────

struct Measurement {
    best: Duration,
}

fn time_best_of<F: FnMut() -> RunResult>(mut f: F, iters: usize) -> Measurement {
    let mut best = Duration::MAX;
    for _ in 0..iters {
        let t0 = Instant::now();
        let _ = f().expect("parser failed");
        let elapsed = t0.elapsed();
        if elapsed < best { best = elapsed; }
    }
    Measurement { best }
}

fn mb_per_sec(bytes: usize, t: Duration) -> f64 {
    let secs = t.as_secs_f64();
    if secs <= 0.0 { 0.0 } else { (bytes as f64) / secs / 1_048_576.0 }
}

// ── fixtures ────────────────────────────────────────────────────────────────

struct Fixture {
    label: &'static str,
    rel:   &'static str,
}

const FIXTURES: &[Fixture] = &[
    Fixture { label: "pubmed.xml      (~600 KB)", rel: "../../tests/assets/xml/pubmed.xml" },
    Fixture { label: "osm.xml         (~1.3 MB)", rel: "../../tests/assets/xml/osm.xml" },
    Fixture { label: "nasa.xml         (~24 MB)", rel: "../../tests/assets/xml/nasa.xml" },
    Fixture { label: "swiss_prot.xml   (~93 MB)", rel: "../../tests/assets/xml/swiss_prot.xml" },
];

// ── main ────────────────────────────────────────────────────────────────────

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let iters: usize = std::env::var("SUPXML_STREAM_HH_ITERS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(3);

    println!("# streaming head-to-head — drive each parser to EOF (no DOM build)");
    println!("# contenders:");
    println!("#   sup-xml    — XmlByteStreamReader::validate()");
    println!("#   quick-xml  — Reader::read_event_into() to Eof   (the Rust default)");
    println!("#   libxml2    — xmlTextReaderRead to EOF            (the C default)");
    println!("# best of {iters} iters");
    println!();

    // ── from-file streaming ──────────────────────────────────────────────────
    println!("## (1) stream from File — bytes pulled on demand via the OS read(2)");
    println!("##     mirrors the 'document bigger than RAM' use case");
    println!();
    print_throughput_header();
    for f in FIXTURES {
        let path = format!("{manifest}/{}", f.rel);
        if !std::path::Path::new(&path).exists() {
            println!("{:<28}  missing — skipping", f.label);
            continue;
        }
        let bytes_len = std::fs::metadata(&path).expect("stat").len() as usize;
        let m_sx = time_best_of(|| run_sup_xml_file(&path),   iters);
        let m_qx = time_best_of(|| run_quick_xml_file(&path), iters);
        let m_lx = time_best_of(|| run_libxml2_file(&path),   iters);
        print_throughput_row(f.label, bytes_len, &m_sx, &m_qx, &m_lx);
    }

    // ── from-memory streaming ────────────────────────────────────────────────
    println!();
    println!("## (2) stream from memory — bytes already in RAM");
    println!("##     removes I/O; measures the parser state machine itself");
    println!();
    print_throughput_header();
    for f in FIXTURES {
        let path = format!("{manifest}/{}", f.rel);
        if !std::path::Path::new(&path).exists() {
            println!("{:<28}  missing — skipping", f.label);
            continue;
        }
        let bytes = std::fs::read(&path).expect("read fixture");
        let m_sx = time_best_of(|| run_sup_xml_mem(&bytes),   iters);
        let m_qx = time_best_of(|| run_quick_xml_mem(&bytes), iters);
        let m_lx = time_best_of(|| run_libxml2_mem(&bytes),   iters);
        print_throughput_row(f.label, bytes.len(), &m_sx, &m_qx, &m_lx);
    }

    // ── validation reality check ─────────────────────────────────────────────
    println!();
    println!("## (3) validation reality check");
    println!("##     ill-formed inputs each parser is REQUIRED to reject per XML 1.0.");
    println!("##     A streaming parser that silently accepts these is faster because");
    println!("##     it skipped the check — read the throughput tables above with this");
    println!("##     column in mind.");
    println!();
    run_validation_reality_check();

    println!();
    println!("# Notes:");
    println!("# - All three parsers do well-formedness only; no DOM build, no");
    println!("#   DTD/XSD validation, no external entity resolution.");
    println!("# - All three are O(1) memory in the input size by design — none");
    println!("#   slurp the whole document.  Per-parser ceilings:");
    println!("#     sup-xml   — DEFAULT_BUFFER_SIZE = 10 MB rolling window");
    println!("#                 (also the max single-token size; matches");
    println!("#                  libxml2's XML_MAX_TEXT_LENGTH)");
    println!("#     quick-xml — caller-owned reusable buffer (64 KB here)");
    println!("#     libxml2   — internal ~4 KB read-ahead chunks");
    println!("# - For the full WFC matrix (15+ cases across 6 parsers) see");
    println!("#   `text_validation_check`; for the attribute-WFC matrix see");
    println!("#   `qxml_attr_validation_check`.  This section shows a focused");
    println!("#   subset to contextualize the throughput numbers above.");
}

/// The ill-formed inputs below are the ones whose rejection cost shows
/// up in the throughput hot path: text-content scanning (`]]>`, bare
/// `&`), attribute-value scanning (bare `<`), and structural checks
/// (mismatched end tag, two root elements).  Each is forbidden by XML
/// 1.0 — every conforming parser must reject.  Anything that prints
/// "accept" is silently consuming malformed input.
fn run_validation_reality_check() {
    let cases: &[(&str, &str)] = &[
        ("]]> in text content  (§2.4)",        "<r>some]]>more</r>"),
        ("bare & in text       (§4.1)",        "<r>tom & jerry</r>"),
        ("bare < in attr value (§3.1 AttValue)", "<r a=\"<x>\"/>"),
        ("mismatched end tag   (§3.1)",        "<r><a></b></r>"),
        ("two root elements    (§2.1)",        "<a/><b/>"),
        ("unclosed at EOF      (§3.1)",        "<r><x>"),
    ];

    println!(
        "{:<38}  {:>10}  {:>10}  {:>10}",
        "case", "sup-xml", "quick-xml", "libxml2",
    );
    println!("{}", "-".repeat(76));

    let mut sx_reject = 0usize;
    let mut qx_reject = 0usize;
    let mut lx_reject = 0usize;

    for (label, src) in cases {
        let bytes = src.as_bytes();
        let sx = validate_sup_xml(bytes);
        let qx = validate_quick_xml(bytes);
        let lx = validate_libxml2(bytes);
        if sx { sx_reject += 1; }
        if qx { qx_reject += 1; }
        if lx { lx_reject += 1; }
        println!(
            "{:<38}  {:>10}  {:>10}  {:>10}",
            label, verdict(sx), verdict(qx), verdict(lx),
        );
    }
    println!("{}", "-".repeat(76));
    let n = cases.len();
    println!(
        "{:<38}  {:>10}  {:>10}  {:>10}",
        "rejected (higher = more conformant)",
        format!("{sx_reject}/{n}"),
        format!("{qx_reject}/{n}"),
        format!("{lx_reject}/{n}"),
    );
}

fn verdict(rejected: bool) -> &'static str {
    if rejected { "REJECT" } else { "accept" }
}

/// Returns `true` iff the parser rejected the input.
fn validate_sup_xml(bytes: &[u8]) -> bool {
    match XmlByteStreamReader::with_size_hint(bytes, Some(bytes.len()), DEFAULT_BUFFER_SIZE) {
        Err(_) => true,
        Ok(r)  => r.validate().is_err(),
    }
}

fn validate_quick_xml(bytes: &[u8]) -> bool {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut reader = Reader::from_reader(bytes);
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                // Match quick-xml's "default with iteration" mode — the
                // configuration `qxml_attr_validation_check` documents
                // as the most validation-thorough mode quick-xml offers.
                for a in e.attributes() {
                    if a.is_err() { return true; }
                }
            }
            Ok(Event::Eof) => return false,
            Ok(_) => continue,
            Err(_) => return true,
        }
        buf.clear();
    }
}

fn validate_libxml2(bytes: &[u8]) -> bool {
    libxml2_silence_once();
    unsafe {
        let reader = xmlReaderForMemory(
            bytes.as_ptr() as *const c_char,
            bytes.len() as c_int,
            std::ptr::null(),
            std::ptr::null(),
            0,
        );
        if reader.is_null() { return true; }
        let mut rejected = false;
        loop {
            match xmlTextReaderRead(reader) {
                1  => continue,
                0  => break,
                _  => { rejected = true; break; }
            }
        }
        xmlFreeTextReader(reader);
        rejected
    }
}

fn print_throughput_header() {
    println!(
        "{:<28}  {:>9}  {:>11}  {:>11}  {:>11}  {:>11}  {:>11}",
        "fixture", "size",
        "sup-xml", "quick-xml", "libxml2",
        "sx/qxml", "sx/libxml2",
    );
    println!("{}", "-".repeat(108));
}

fn print_throughput_row(
    label: &str,
    bytes: usize,
    m_sx:  &Measurement,
    m_qx:  &Measurement,
    m_lx:  &Measurement,
) {
    let mb       = bytes as f64 / 1_048_576.0;
    let sx_mbps  = mb_per_sec(bytes, m_sx.best);
    let qx_mbps  = mb_per_sec(bytes, m_qx.best);
    let lx_mbps  = mb_per_sec(bytes, m_lx.best);
    let r_qx     = if qx_mbps > 0.0 { sx_mbps / qx_mbps } else { 0.0 };
    let r_lx     = if lx_mbps > 0.0 { sx_mbps / lx_mbps } else { 0.0 };
    println!(
        "{:<28}  {:>7.1} MB  {:>7.0} MB/s  {:>7.0} MB/s  {:>7.0} MB/s  {:>8.2}x  {:>8.2}x",
        label, mb, sx_mbps, qx_mbps, lx_mbps, r_qx, r_lx,
    );
}

