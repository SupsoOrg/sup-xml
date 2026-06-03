//! Arena XPath throughput.
//!
//! Parses one fixture into the arena DOM, then runs a small query mix against
//! it via the reusable XPath context.  Reports:
//! 1. Context-build time (DocIndex construction).
//! 2. Per-query time, summed over `XPATH_ITERS` repetitions.
//!
//! Run:
//!     cargo bench -p sup-xml-bench --bench profile_arena_xpath

use std::time::Instant;

use sup_xml::{
    parse_bytes_unchecked as parse_bytes_unchecked,
    ParseOptions,
    XPathContext,
};

const FIXTURE: &str = "../../tests/assets/xml/sitemap.xml";
const QUERIES: &[&str] = &[
    "count(//url)",
    "//url/loc",
    "count(//*)",
    "//url[1]/loc",
    "string(//url[1]/lastmod)",
];

fn main() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = format!("{manifest}/{FIXTURE}");
    let bytes = std::fs::read(&path).expect("read fixture");
    let iters: usize = std::env::var("XPATH_ITERS").ok().and_then(|s| s.parse().ok()).unwrap_or(100);

    eprintln!("# profile_arena_xpath — {} ({} KB), {} iters of {} queries",
        FIXTURE, bytes.len() / 1024, iters, QUERIES.len());
    eprintln!();

    // Parse once.
    let opts = ParseOptions::default();
    let arena = unsafe { parse_bytes_unchecked(&bytes, &opts) }.expect("arena parse");

    // Context-build timing.
    let t = Instant::now();
    for _ in 0..iters { let _ = XPathContext::new(&arena); std::hint::black_box(()); }
    let arena_build = t.elapsed().as_secs_f64() / iters as f64 * 1e6;
    eprintln!("context build (avg of {} runs): {:>7.1} µs", iters, arena_build);
    eprintln!();

    // Reusable context for the per-query loop.
    let ctx = XPathContext::new(&arena);

    eprintln!("per-query throughput (best of 3 runs, {} iters each):", iters);
    eprintln!("{:<40}  {:>14}", "query", "arena (µs)");
    eprintln!("{}", "-".repeat(58));
    for &q in QUERIES {
        let t = best_run(|| {
            for _ in 0..iters { std::hint::black_box(ctx.eval(q).unwrap()); }
        });
        let us = t.as_secs_f64() / iters as f64 * 1e6;
        let q_short: String = if q.len() <= 38 { q.to_string() } else { format!("{}…", &q[..37]) };
        eprintln!("{:<40}  {:>14.2}", q_short, us);
    }
}

fn best_run<F: FnMut()>(mut f: F) -> std::time::Duration {
    let t = Instant::now();
    f();
    t.elapsed()
}
