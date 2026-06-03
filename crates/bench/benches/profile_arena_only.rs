//! Same as `profile_swiss_prot` but exercises *only* the arena DOM path,
//! so a sampling profile attribute-counts that path in isolation rather
//! than mixing it with the legacy DOM.  Used to find what's left after
//! bumpalo already absorbed most per-node allocation.

use std::time::Instant;
use sup_xml::ParseOptions;
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
    eprintln!("# profile_arena_only — {} iters of {} bytes", iters, bytes.len());
    let t = Instant::now();
    for _ in 0..iters {
        let doc = unsafe { parse_bytes_unchecked(&bytes, &opts) }.expect("parse");
        std::hint::black_box(doc);
    }
    let el = t.elapsed();
    let mbps = (bytes.len() as f64 * iters as f64) / el.as_secs_f64() / 1e6;
    eprintln!("# arena DOM only: {:.2}s  ({:.0} MB/s avg)", el.as_secs_f64(), mbps);
}
