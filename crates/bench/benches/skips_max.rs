//! What does the existing architecture deliver when all `skip_*` flags
//! are turned on?  Gives us the ceiling perf without rewriting any
//! parser code.

use std::time::Instant;
use sup_xml::{parse_bytes, ParseOptions};

const FIXTURE: &str = "../../tests/assets/xml/swiss_prot.xml";

fn time_best_of(f: impl Fn() -> usize, iters: usize) -> std::time::Duration {
    let mut best = std::time::Duration::MAX;
    for _ in 0..iters {
        let t0 = Instant::now();
        let n = f();
        let elapsed = t0.elapsed();
        std::hint::black_box(n);
        if elapsed < best { best = elapsed; }
    }
    best
}

fn mb_per_sec(bytes: usize, t: std::time::Duration) -> f64 {
    (bytes as f64) / t.as_secs_f64() / 1_048_576.0
}

fn main() {
    let path = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), FIXTURE);
    let bytes = std::fs::read(&path).expect("read swiss_prot.xml");
    let iters: usize = std::env::var("SUPXML_SKIPS_ITERS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(10);

    println!("# skips_max — swiss_prot.xml ({:.1} MB), best of {} iters",
        bytes.len() as f64 / 1_048_576.0, iters);

    let cases: &[(&str, ParseOptions)] = &[
        ("default", ParseOptions::default()),
        ("skip xml_char_validation", ParseOptions {
            skip_xml_char_validation: true, ..ParseOptions::default()
        }),
        ("skip name_validation",     ParseOptions {
            skip_name_validation: true, ..ParseOptions::default()
        }),
        ("skip attr_validation",     ParseOptions {
            skip_attr_validation: true, ..ParseOptions::default()
        }),
        ("skip end_tag_check",       ParseOptions {
            skip_end_tag_check: true, ..ParseOptions::default()
        }),
        ("skip ALL",                 ParseOptions {
            skip_xml_char_validation: true,
            skip_name_validation:     true,
            skip_attr_validation:     true,
            skip_end_tag_check:       true,
            skip_entity_expansion:    false,  // can't skip — would break DOM
            ..ParseOptions::default()
        }),
    ];

    for (label, opts) in cases {
        let t = time_best_of(|| {
            let doc = parse_bytes(&bytes, opts).expect("parse");
            std::hint::black_box(doc.root().children().count())
        }, iters);
        println!("{:<32} {:>5.0} MB/s", label, mb_per_sec(bytes.len(), t));
    }
}
