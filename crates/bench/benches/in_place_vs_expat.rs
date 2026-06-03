//! Three-way speed + memory comparison across the fixture set:
//!
//!   - `parse_bytes_in_place`               — DOM build, our fastest path
//!   - `XmlBytesReader` (SAX, count events) — our SAX layer with no DOM
//!   - `expat`                              — C SAX parser, no DOM
//!
//! Why three columns: expat is a SAX parser (push events to callbacks),
//! it doesn't build a tree.  Comparing DOM-build to SAX-only isn't
//! apples-to-apples.  The middle column (sup-xml's own SAX layer
//! consuming events without building anything) is the fair comparison
//! against expat — both do the same work shape.  The DOM column then
//! shows the cost of building a tree on top of the same scanner.
//!
//! Run:
//!     SUPXML_VS_EXPAT_ITERS=10 cargo bench -p sup-xml-bench --bench in_place_vs_expat

use std::os::raw::c_char;
use std::time::Instant;

use sup_xml::{parse_bytes_in_place, ParseOptions};
use sup_xml_core::XmlBytesReader;
use sup_xml_core::BytesEvent;

unsafe extern "C" {
    /// Parse with expat, count start-element events; returns SIZE_MAX on error.
    fn expat_bench_parse_count(buf: *const c_char, len: usize) -> usize;
}

fn time_best_of_n(mut f: impl FnMut() -> usize, iters: usize) -> std::time::Duration {
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

fn load_asset(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/assets/xml")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {name}: {e}"))
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let names: &[&str] = &[
        "321gone.xml", "1831893.xml", "bargains_he_5.xml", "chinese1.xml",
        "cldr_en.xml", "customer1.xml", "dblp.xml", "ebay.xml",
        "gazali_maqasid_ar.xml", "maven-pom.xml", "nasa.xml", "osm.xml",
        "podcast_episode_2024_03.xml", "pubmed.xml", "sitemap.xml",
        "swiss_prot.xml", "ubid.xml", "utah_legislature_2024.xml",
        "uwm.xml", "wikipedia_ww2.xml", "yahoo.xml",
    ];
    names.iter().map(|n| {
        let label = n.trim_end_matches(".xml").to_string();
        (label, load_asset(n))
    }).collect()
}

/// SupXML SAX comparison run: same shape as expat's
/// "parse + count start-element events, no DOM build."
fn sax_count_events(bytes: &[u8]) -> usize {
    // SAFETY: every fixture in the bench corpus is valid UTF-8.
    let mut reader = unsafe { XmlBytesReader::from_bytes_unchecked(bytes) };
    let mut count: usize = 0;
    loop {
        match reader.next() {
            Ok(BytesEvent::StartElement(_)) => count += 1,
            Ok(BytesEvent::Eof) => break,
            Ok(_) => {}
            Err(_) => return usize::MAX,
        }
    }
    count
}

fn main() {
    let iters: usize = std::env::var("SUPXML_VS_EXPAT_ITERS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(10);

    let opts = ParseOptions::default();
    println!("# Three-way: parse_bytes_in_place (DOM build) vs sup-xml SAX vs expat SAX");
    println!("# best of {} iters; in-place includes one Vec clone per iter",
        iters);
    println!();
    println!("{:<32}  {:>8}  {:>10}  {:>10}  {:>10}  {:>11}  {:>10}",
        "fixture", "size", "in-place", "sx SAX", "expat", "arena (DOM)", "sx/expat");
    println!("{}", "-".repeat(112));

    let mut tot_bytes = 0usize;
    let mut sum_lean   = 0.0f64;
    let mut sum_sx_sax = 0.0f64;
    let mut sum_expat  = 0.0f64;

    for (name, bytes) in fixtures() {
        tot_bytes += bytes.len();

        // 1) parse_bytes_in_place (Document, DOM build)
        let t_lean = time_best_of_n(|| {
            let buf = bytes.clone();
            let doc = parse_bytes_in_place(buf, &opts).expect("parse_bytes_in_place");
            doc.root().children().count()
        }, iters);

        // Capture arena_bytes once for reporting
        let arena_bytes = {
            let buf = bytes.clone();
            let doc = parse_bytes_in_place(buf, &opts).expect("parse_bytes_in_place");
            doc.memory_bytes()
        };

        // 2) sup-xml SAX: same scanner but no DOM build (count events)
        let t_sx_sax = time_best_of_n(|| sax_count_events(&bytes), iters);

        // 3) expat: SAX parser, count events
        let t_expat = time_best_of_n(|| unsafe {
            let n = expat_bench_parse_count(bytes.as_ptr() as *const c_char, bytes.len());
            assert!(n != usize::MAX, "expat parse failed on {name}");
            n
        }, iters);

        let mbps_lean   = mb_per_sec(bytes.len(), t_lean);
        let mbps_sx_sax = mb_per_sec(bytes.len(), t_sx_sax);
        let mbps_expat  = mb_per_sec(bytes.len(), t_expat);

        sum_lean   += mbps_lean   * bytes.len() as f64;
        sum_sx_sax += mbps_sx_sax * bytes.len() as f64;
        sum_expat  += mbps_expat  * bytes.len() as f64;

        println!("{:<32}  {:>5.0} KB  {:>5.0} MB/s  {:>5.0} MB/s  {:>5.0} MB/s  {:>7.1} MB  {:>9.2}x",
            format!("{name} ({}KB)", bytes.len() / 1024),
            bytes.len() as f64 / 1024.0,
            mbps_lean,
            mbps_sx_sax,
            mbps_expat,
            arena_bytes as f64 / 1_048_576.0,
            mbps_sx_sax / mbps_expat,
        );
    }

    let denom = tot_bytes as f64;
    println!("{}", "-".repeat(112));
    println!("size-weighted avg              {:>8}  {:>5.0} MB/s  {:>5.0} MB/s  {:>5.0} MB/s",
        "",
        sum_lean   / denom,
        sum_sx_sax / denom,
        sum_expat  / denom,
    );

    println!();
    println!("# Memory notes:");
    println!("#   - 'doc (DOM)' column is bumpalo arena size for the lean Document.");
    println!("#     Includes Node + Attribute + Namespace structs + non-source-borrowable strings.");
    println!("#     The source buffer itself is consumed in-place; not counted here.");
    println!("#   - expat is SAX (no tree).  Its working set is ~50-100KB regardless of input size");
    println!("#     (parser state + transient attribute lists during start-element callbacks).");
    println!("#   - sup-xml SAX (XmlBytesReader) is also no-tree: working set is ~Scanner struct (~64B)");
    println!("#     + Vec<(u32, u32)> element_stack growing with depth.");
}
