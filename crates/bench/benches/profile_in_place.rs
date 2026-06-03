//! Sampling-profiler target for the in-place path.  Parses
//! `swiss_prot.xml` (~95 MB) in a tight loop via
//! [`parse_bytes_in_place`] so a sampling profiler can characterise
//! the hot path that's competing with pugixml.
//!
//! Recommended usage:
//!     cargo bench -p sup-xml-bench --bench profile_in_place --no-run
//!     samply record --save-only -o prof.json -- \
//!         ./target/release/deps/profile_in_place-<hash>
//!     samply load prof.json   # opens browser UI
//!
//! Tweak the loop bound via `SWISSPROT_ITERS` (default 30 → ~7s @ 400 MB/s).

use std::time::Instant;

use sup_xml::{parse_bytes_in_place, ParseOptions};

const FIXTURE: &str = "../../tests/assets/xml/swiss_prot.xml";

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = format!("{manifest}/{FIXTURE}");
    let bytes = std::fs::read(&path).expect("read swiss_prot.xml");
    let iters: usize = std::env::var("SWISSPROT_ITERS")
        .ok().and_then(|s| s.parse().ok())
        .unwrap_or(30);

    let opts = ParseOptions::default();
    eprintln!("# profile_in_place — {} iters of {} bytes", iters, bytes.len());

    let t = Instant::now();
    for _ in 0..iters {
        // Clone per iter — parse_bytes_in_place consumes the buffer.
        let buf = bytes.clone();
        let doc = parse_bytes_in_place(buf, &opts).expect("parse_bytes_in_place");
        std::hint::black_box(doc);
    }
    let el = t.elapsed();
    let mbps = (bytes.len() as f64 * iters as f64) / el.as_secs_f64() / 1e6;
    eprintln!("# in-place: {:.2}s  ({:.0} MB/s avg incl. clone)", el.as_secs_f64(), mbps);
}
