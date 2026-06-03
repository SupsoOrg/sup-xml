//! Probe whether `parse_xpath` leaks memory.
//!
//! Parses the same input N times and prints RSS at the start, midpoint,
//! and end.  Flat RSS → no leak (the fuzz OOM was libFuzzer/ASAN
//! accumulation).  Growing RSS → real leak we need to fix.

use std::time::Instant;
use sup_xml_core::xpath::parse_xpath;

// Use ps to read RSS — avoids pulling libc as a dep.
fn rss_bytes() -> u64 {
    let pid = std::process::id();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok();
    out.and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: probe_parser_rss <input-path>");
    let bytes = std::fs::read(&path).expect("read input");
    let s = std::str::from_utf8(&bytes).expect("UTF-8");
    let iters: usize = std::env::args().nth(2)
        .and_then(|s| s.parse().ok()).unwrap_or(1_000_000);

    println!("input: {} bytes, {} iters", s.len(), iters);
    println!("rss start:  {:>8.1} MB", rss_bytes() as f64 / 1_048_576.0);

    let t0 = Instant::now();
    let mid = iters / 2;
    for i in 0..iters {
        let _ = parse_xpath(s);
        if i == mid {
            println!("rss mid:    {:>8.1} MB  (after {} parses, {:.2?})",
                rss_bytes() as f64 / 1_048_576.0, i, t0.elapsed());
        }
    }
    let dt = t0.elapsed();
    println!("rss end:    {:>8.1} MB  (after {} parses, {:.2?}, {:.0} parses/sec)",
        rss_bytes() as f64 / 1_048_576.0, iters, dt,
        iters as f64 / dt.as_secs_f64());
}
