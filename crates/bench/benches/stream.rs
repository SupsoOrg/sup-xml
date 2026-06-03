//! Streaming-parser benchmark: sup-xml's `StreamParser` vs quick-xml.
//!
//! Workload: iterate every `<Entry>` element under `<root>` in
//! `swiss_prot.xml` (95 MB, 41,877 entries) and count them.  Each iteration
//! emits a single item's subtree — sup-xml as an owned arena `Document`,
//! quick-xml as a sequence of `Event::Start`/`Event::End` events the caller
//! groups by depth.
//!
//! This is a realistic streaming workload (think `<page>` in a Wikipedia
//! dump, `<entry>` in an Atom feed, `<PubmedArticle>` in a Medline dump):
//! the document is too big to fit comfortably in memory, the caller wants
//! per-item processing, and memory should stay bounded by the largest
//! single item rather than the whole file.
//!
//! Run:
//!     SUPXML_STREAM_ITERS=3 cargo bench -p sup-xml-bench --bench stream

use std::time::Instant;

const FIXTURE: &str = "../../tests/assets/xml/swiss_prot.xml";
const EXPECTED_ITEMS: usize = 41877;

// ── peak-RSS helper ──────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn peak_rss_bytes() -> u64 {
    unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut u);
        u.ru_maxrss as u64  // macOS: bytes
    }
}
#[cfg(target_os = "linux")]
fn peak_rss_bytes() -> u64 {
    unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut u);
        (u.ru_maxrss as u64) * 1024  // Linux: kilobytes
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peak_rss_bytes() -> u64 { 0 }

// ── runner: count entries only ────────────────────────────────────────────
//
// "How fast can each parser iterate per item?"  Cheapest comparison: just
// reach each `<Entry>` once.  Note this is unfair to sup-xml — its
// StreamParser builds a full arena Document per item before the caller
// even sees it, whereas quick-xml's event loop just emits tokens.

fn run_sup_xml_count(bytes: &[u8]) -> usize {
    use sup_xml::StreamParser;
    let s = std::str::from_utf8(bytes).expect("UTF-8");
    let mut sp = StreamParser::from_str(s).emit_at_path(&["root", "Entry"]);
    let mut n = 0usize;
    while let Some(doc) = sp.next().expect("stream parse") {
        std::hint::black_box(doc.root().name);
        n += 1;
    }
    n
}

fn run_quick_xml_count(bytes: &[u8]) -> usize {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut reader = Reader::from_reader(bytes);
    let mut n = 0usize;
    let mut depth = 0usize;
    let mut path: Vec<Vec<u8>> = Vec::new();
    loop {
        match reader.read_event().expect("qxml event") {
            Event::Start(e) => {
                depth += 1;
                let name = e.name().as_ref().to_vec();
                path.push(name);
                if depth == 2 && path[0] == b"root" && path[1] == b"Entry" {
                    n += 1;
                }
            }
            Event::Empty(e) => {
                let name = e.name().as_ref().to_vec();
                if depth == 1 && path.first().map(|s| s.as_slice()) == Some(b"root".as_ref())
                    && name == b"Entry"
                {
                    n += 1;
                }
            }
            Event::End(_)  => { path.pop(); depth = depth.saturating_sub(1); }
            Event::Eof     => break,
            _ => {}
        }
    }
    std::hint::black_box(n);
    n
}

// ── runner: extract per-item data ────────────────────────────────────────
//
// Realistic use case: for each `<Entry id="..."` capture its `id` attribute
// plus the text of the first `<AC>` child.  Both parsers have to do
// equivalent work: accumulate per-item state, then process when the item
// closes.  This is the fair "what does it cost to actually USE the items"
// comparison.

fn run_sup_xml_extract(bytes: &[u8]) -> usize {
    use sup_xml::{StreamParser, NodeKind};
    let s = std::str::from_utf8(bytes).expect("UTF-8");
    let mut sp = StreamParser::from_str(s).emit_at_path(&["root", "Entry"]);
    let mut n = 0usize;
    let mut id_len_total = 0usize;
    let mut ac_len_total = 0usize;
    while let Some(doc) = sp.next().expect("stream parse") {
        let entry = doc.root();
        // id attribute (defined on <Entry>)
        if let Some(id) = entry.attributes().find(|a| a.name == "id") {
            id_len_total += id.value.len();
        }
        // First <AC> child element's text content
        if let Some(ac) = entry.children().find(|c| c.kind == NodeKind::Element && c.name == "AC") {
            if let Some(t) = ac.text_content() {
                ac_len_total += t.len();
            }
        }
        n += 1;
    }
    std::hint::black_box((id_len_total, ac_len_total));
    n
}

fn run_quick_xml_extract(bytes: &[u8]) -> usize {
    use quick_xml::Reader;
    use quick_xml::events::Event;
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut n = 0usize;
    let mut id_len_total = 0usize;
    let mut ac_len_total = 0usize;
    let mut depth = 0usize;
    let mut in_entry = false;
    let mut current_ac_seen = false;        // already grabbed AC for this Entry?
    let mut want_ac_text = false;           // next Text event belongs to <AC>
    let mut buf = String::new();            // reused per text capture
    loop {
        match reader.read_event().expect("qxml event") {
            Event::Start(e) => {
                depth += 1;
                let name = e.name();
                if depth == 2 && name.as_ref() == b"Entry" {
                    in_entry = true;
                    current_ac_seen = false;
                    // Walk attrs for id
                    for a in e.attributes().with_checks(false) {
                        if let Ok(a) = a {
                            if a.key.as_ref() == b"id" {
                                id_len_total += a.value.len();
                            }
                        }
                    }
                } else if in_entry && depth == 3 && name.as_ref() == b"AC" && !current_ac_seen {
                    want_ac_text = true;
                    buf.clear();
                }
            }
            Event::Empty(e) => {
                let name = e.name();
                if depth == 1 && name.as_ref() == b"Entry" {
                    for a in e.attributes().with_checks(false) {
                        if let Ok(a) = a {
                            if a.key.as_ref() == b"id" { id_len_total += a.value.len(); }
                        }
                    }
                    n += 1;
                }
            }
            Event::Text(t) => {
                if want_ac_text {
                    let s = std::str::from_utf8(&t).expect("AC text utf8");
                    buf.push_str(s);
                }
            }
            Event::End(_) => {
                if want_ac_text {
                    ac_len_total += buf.len();
                    want_ac_text = false;
                    current_ac_seen = true;
                }
                if in_entry && depth == 2 {
                    in_entry = false;
                    n += 1;
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    std::hint::black_box((id_len_total, ac_len_total));
    n
}

fn time_best_of(f: fn(&[u8]) -> usize, bytes: &[u8], iters: usize) -> (std::time::Duration, usize, u64) {
    let mut best = std::time::Duration::MAX;
    let mut last_n = 0;
    let baseline_rss = peak_rss_bytes();
    for _ in 0..iters {
        let t0 = Instant::now();
        let n = f(bytes);
        let elapsed = t0.elapsed();
        if elapsed < best { best = elapsed; }
        last_n = n;
    }
    let peak = peak_rss_bytes();
    (best, last_n, peak.saturating_sub(baseline_rss))
}

fn mb_per_sec(bytes: usize, t: std::time::Duration) -> f64 {
    let secs = t.as_secs_f64();
    if secs <= 0.0 { 0.0 } else { (bytes as f64) / secs / 1_048_576.0 }
}

fn items_per_sec(items: usize, t: std::time::Duration) -> f64 {
    let secs = t.as_secs_f64();
    if secs <= 0.0 { 0.0 } else { items as f64 / secs }
}

fn main() {
    let path = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), FIXTURE);
    let bytes = std::fs::read(&path).expect("read swiss_prot.xml");
    let iters: usize = std::env::var("SUPXML_STREAM_ITERS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(3);

    println!("# stream bench — swiss_prot.xml ({:.1} MB), best of {} iters",
        bytes.len() as f64 / 1_048_576.0, iters);
    println!("# emit each <Entry> at depth 1; expected {EXPECTED_ITEMS} items");

    let report = |label: &str, f: fn(&[u8]) -> usize| -> f64 {
        let (t, n, dr) = time_best_of(f, &bytes, iters);
        assert_eq!(n, EXPECTED_ITEMS, "{label} missed items: {n}");
        let mbps = mb_per_sec(bytes.len(), t);
        let ips  = items_per_sec(n, t);
        println!("{:<36}  {:>10}  {:>9.0} MB/s  {:>11.0}  ΔRSS {:>5.1} MB",
            label, n, mbps, ips, dr as f64 / 1_048_576.0);
        mbps
    };

    println!();
    println!("## (1) count items only  —  cheapest workload");
    println!("{:<36}  {:>10}  {:>14}  {:>14}", "runner", "items", "throughput", "items/sec");
    println!("{}", "-".repeat(78));
    let s1 = report("sup_xml::StreamParser",       run_sup_xml_count);
    let q1 = report("quick-xml (raw events)",       run_quick_xml_count);
    println!("ratio super/qxml: {:.2}x  (>1 = sup-xml faster)", s1 / q1);

    println!();
    println!("## (2) extract id attr + first <AC> text per Entry  —  realistic use");
    println!("{:<36}  {:>10}  {:>14}  {:>14}", "runner", "items", "throughput", "items/sec");
    println!("{}", "-".repeat(78));
    let s2 = report("sup_xml::StreamParser",       run_sup_xml_extract);
    let q2 = report("quick-xml (manual state mgmt)", run_quick_xml_extract);
    println!("ratio super/qxml: {:.2}x  (>1 = sup-xml faster)", s2 / q2);

    println!();
    println!("# Notes:");
    println!("# - In (1), quick-xml just emits tokens; sup-xml builds a full arena Document per item.");
    println!("# - In (2), both parsers do equivalent per-item work (extract attribute + child text).");
    println!("# - Use case is 'pull each <Entry> out of a multi-GB feed and process it.'");
}
