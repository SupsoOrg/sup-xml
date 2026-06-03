//! Same as profile_swiss_prot but with mimalloc as the global allocator.
//!
//! Cross-check experiment: the profile shows ~60% of legacy-DOM parse time
//! goes to malloc/free.  Switching the system allocator for mimalloc tests
//! whether what's actually slow is allocator quality (the system zone
//! allocator's overhead per call) versus allocator quantity (the sheer
//! number of allocations the parser performs).
//!
//! Run:
//!     cargo bench -p sup-xml-bench --bench profile_swiss_prot_mimalloc

use std::time::Instant;

use sup_xml::ParseOptions;
// M2: arena DOM is the only DOM.
use sup_xml_core::parse_bytes_unchecked;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const FIXTURE: &str = "../../tests/assets/xml/swiss_prot.xml";

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = format!("{manifest}/{FIXTURE}");
    let bytes = std::fs::read(&path).expect("read swiss_prot.xml");
    let iters: usize = std::env::var("SWISSPROT_ITERS")
        .ok().and_then(|s| s.parse().ok())
        .unwrap_or(20);

    eprintln!("# profile_swiss_prot_mimalloc — {} iters of {} bytes", iters, bytes.len());

    let opts = ParseOptions::default();
    let t = Instant::now();
    for _ in 0..iters {
        let doc = unsafe { parse_bytes_unchecked(&bytes, &opts) }.expect("parse");
        std::hint::black_box(doc);
    }
    let el = t.elapsed();
    let mbps = (bytes.len() as f64 * iters as f64) / el.as_secs_f64() / 1e6;
    eprintln!("# arena DOM              + mimalloc: {:.2}s  ({:.0} MB/s avg)", el.as_secs_f64(), mbps);
}
