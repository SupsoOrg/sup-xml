//! Sampling-profiler target.  Parses `swiss_prot.xml` (~95 MB, our slowest
//! fixture at ~150 MB/s) in a tight loop so a sampling profiler can capture
//! enough stacks to characterise the hot path.
//!
//! Recommended usage:
//!     cargo bench -p sup-xml-bench --bench profile_swiss_prot --no-run
//!     samply record -- ./target/release/deps/profile_swiss_prot-<hash>
//!
//! Tweak the loop bound via `SWISSPROT_ITERS` (default 20 → ~12s @ 150 MB/s).

use std::time::Instant;

use sup_xml::ParseOptions;
// M2: arena DOM is the only DOM.
use sup_xml_core::parse_bytes_unchecked;

const FIXTURE: &str = "../../tests/assets/xml/swiss_prot.xml";

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = format!("{manifest}/{FIXTURE}");
    let bytes = std::fs::read(&path).expect("read swiss_prot.xml");
    let iters: usize = std::env::var("SWISSPROT_ITERS")
        .ok().and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let opts = ParseOptions::default();
    eprintln!("# profile_swiss_prot — {} iters of {} bytes", iters, bytes.len());

    // Arena-DOM path: builds an arena-allocated tree via the SAX layer.
    let t = Instant::now();
    for _ in 0..iters {
        let doc = unsafe { parse_bytes_unchecked(&bytes, &opts) }.expect("parse");
        std::hint::black_box(doc);
    }
    let el = t.elapsed();
    let mbps = (bytes.len() as f64 * iters as f64) / el.as_secs_f64() / 1e6;
    eprintln!("# arena DOM:    {:.2}s  ({:.0} MB/s avg)", el.as_secs_f64(), mbps);
}
