//! Diagnostic micro-bench targeting `1831893.xml` — the one fixture
//! where sup-xml lags quick-xml in the head-to-head bytes comparison
//! (~0.86× across multiple runs).  This isn't a comparison bench; it's
//! a per-event-type breakdown of where sup-xml spends its time so we
//! can decide what (if anything) to optimise next.
//!
//! Run with:
//!     cargo bench -p sup-xml-bench --bench profile_1831893
//!
//! The output is a plain text table: event-counts and per-call median
//! costs over many iterations.

use std::time::Instant;

use sup_xml::{BytesEvent, ParseOptions, XmlBytesReader};

const FIXTURE: &str = "../../tests/assets/xml/1831893.xml";

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = format!("{manifest}/{FIXTURE}");
    let bytes = std::fs::read(&path).expect("read 1831893.xml");

    println!("\n── 1831893.xml profile ──");
    println!("file size:           {:>8} bytes", bytes.len());

    // ── 1.  Total parse time, no event work ────────────────────────────
    let n = 5000;
    let mut best = u128::MAX;
    for _ in 0..n {
        let t = Instant::now();
        let _ = parse_count(&bytes);
        let el = t.elapsed().as_nanos();
        if el < best { best = el; }
    }
    println!("total parse (best):  {:>8.2} µs   ({:>7.1} MB/s)",
        best as f64 / 1e3,
        bytes.len() as f64 / (best as f64 / 1e9) / (1024.0 * 1024.0));

    // ── 2.  Event-type counts ──────────────────────────────────────────
    let counts = count_events(&bytes);
    println!();
    println!("event counts:");
    println!("  StartElement:      {:>6}", counts.start);
    println!("  EndElement:        {:>6}", counts.end);
    println!("  Text:              {:>6}", counts.text);
    println!("  Text (whitespace): {:>6}", counts.ws_text);
    println!("  CData:             {:>6}", counts.cdata);
    println!("  Comment:           {:>6}", counts.comment);
    println!("  Pi:                {:>6}", counts.pi);

    // ── 3.  Approximate per-event cost ─────────────────────────────────
    let total_events = counts.start + counts.end + counts.text + counts.cdata + counts.comment + counts.pi;
    println!();
    println!("approx per-event cost: {:>5.1} ns",
        best as f64 / total_events as f64);

    // ── 4.  Time excluding StartElements (skip past `<` chars) ────────
    // What if we discarded everything except text+end?  Tells us how
    // much of total time is StartElement processing.
    let mut best_no_start = u128::MAX;
    for _ in 0..n {
        let t = Instant::now();
        let _ = parse_count_no_text(&bytes);
        let el = t.elapsed().as_nanos();
        if el < best_no_start { best_no_start = el; }
    }
    println!();
    println!("ignoring text events:");
    println!("  parse time:        {:>8.2} µs   ({:>7.1} MB/s)",
        best_no_start as f64 / 1e3,
        bytes.len() as f64 / (best_no_start as f64 / 1e9) / (1024.0 * 1024.0));

    println!();
    _diag_with_full_ws_emitted(&bytes);
}

#[derive(Default)]
struct Counts {
    start: u32, end: u32, text: u32, ws_text: u32,
    cdata: u32, comment: u32, pi: u32,
}

fn count_events(bytes: &[u8]) -> Counts {
    let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(bytes) }
        .with_options(skip_opts());
    let mut c = Counts::default();
    loop {
        match r.next().unwrap() {
            BytesEvent::StartElement(_) => c.start += 1,
            BytesEvent::EndElement(_)   => c.end   += 1,
            BytesEvent::Text(t) => {
                c.text += 1;
                let bytes = t.as_bytes();
                if !bytes.is_empty() && bytes.iter().all(|b| b.is_ascii_whitespace()) {
                    c.ws_text += 1;
                }
                if c.text <= 3 {
                    eprintln!("  Text event #{}: {:?}", c.text,
                        String::from_utf8_lossy(&bytes[..bytes.len().min(40)]));
                }
            }
            BytesEvent::CData(_)     => c.cdata   += 1,
            BytesEvent::Comment(_)   => c.comment += 1,
            BytesEvent::Pi(_)        => c.pi      += 1,
            BytesEvent::EntityRef(_) => {} // not counted in this profile
            BytesEvent::Eof          => break,
        }
    }
    c
}

fn parse_count(bytes: &[u8]) -> usize {
    let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(bytes) }
        .with_options(skip_opts());
    let mut n = 0usize;
    loop {
        match r.next().unwrap() {
            BytesEvent::StartElement(_) => n += 1,
            BytesEvent::Eof => break,
            _ => {}
        }
    }
    std::hint::black_box(n)
}

fn parse_count_no_text(bytes: &[u8]) -> usize {
    let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(bytes) }
        .with_options(skip_opts());
    let mut n = 0usize;
    loop {
        match r.next().unwrap() {
            BytesEvent::StartElement(_) | BytesEvent::EndElement(_) => n += 1,
            BytesEvent::Eof => break,
            _ => {}
        }
    }
    std::hint::black_box(n)
}

fn skip_opts() -> ParseOptions {
    ParseOptions {
        skip_xml_char_validation: true,
        skip_name_validation: true,
        skip_end_tag_check: true,
        skip_entity_expansion: true,
        ..ParseOptions::default()
    }
}

fn _diag_with_full_ws_emitted(bytes: &[u8]) {
    // For comparison: drop skip_end_tag_check so depth tracking
    // engages and per-call whitespace-skip stops firing.  Shows the
    // *real* event count quick-xml sees.
    let opts = ParseOptions {
        skip_xml_char_validation: true,
        skip_name_validation: true,
        skip_entity_expansion: true,
        ..ParseOptions::default()
    };
    let mut r = unsafe { XmlBytesReader::from_bytes_unchecked(bytes) }.with_options(opts);
    let mut events = 0u32; let mut ws = 0u32;
    loop {
        match r.next().unwrap() {
            BytesEvent::Eof => break,
            BytesEvent::Text(t) => {
                events += 1;
                if !t.as_bytes().is_empty() && t.as_bytes().iter().all(|b| b.is_ascii_whitespace()) {
                    ws += 1;
                }
            }
            _ => events += 1,
        }
    }
    eprintln!("with depth tracking on: {events} events, {ws} pure-whitespace text events");
}
