//! Sampling-profiler target for the gazali_maqasid_ar fixture (599 KB
//! Arabic text).  Parses it in a tight loop so a sampling profiler
//! captures enough stacks to characterise the hot path.
//!
//! Recommended usage:
//!     cargo bench -p sup-xml-bench --bench profile_gazali --no-run
//!     samply record -- ./target/release/deps/profile_gazali-<hash>
//!
//! Tweak the loop bound via `GAZALI_ITERS` (default 200 → ~8s at the
//! currently-observed ~15 MB/s DOM rate on this fixture).

use std::time::Instant;

use sup_xml::ParseOptions;
use sup_xml_core::parse_bytes_unchecked;

const FIXTURE: &str = "../../tests/assets/xml/gazali_maqasid_ar.xml";

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = format!("{manifest}/{FIXTURE}");
    let bytes = std::fs::read(&path).expect("read gazali_maqasid_ar.xml");
    let iters: usize = std::env::var("GAZALI_ITERS")
        .ok().and_then(|s| s.parse().ok())
        .unwrap_or(200);

    let opts = ParseOptions::default();
    eprintln!("# profile_gazali — {} iters of {} bytes", iters, bytes.len());

    let t = Instant::now();
    for _ in 0..iters {
        let doc = unsafe { parse_bytes_unchecked(&bytes, &opts) }.expect("parse");
        std::hint::black_box(doc);
    }
    let el = t.elapsed();
    let mbps = (bytes.len() as f64 * iters as f64) / el.as_secs_f64() / 1e6;
    eprintln!("# DOM: {:.2}s  ({:.0} MB/s avg)", el.as_secs_f64(), mbps);
}
