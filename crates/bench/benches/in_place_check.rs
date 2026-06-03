//! Four-way speed test on swiss_prot + an entity-heavy synthetic doc:
//!
//!   - `parse_bytes`                          (strict, full validation)
//!   - `parse_bytes_in_place` w/ default opts (destructive, full validation)
//!   - `parse_bytes_in_place` w/ skip-all     (destructive, trust the input)
//!   - `pugixml`                              (C++ baseline via FFI shim)
//!
//! The first two share validation behavior; the gap between them shows
//! the *structural* in-place win (no string copy, in-place entity
//! decode).  The third row is the in-place win plus skipped validation
//! — closer to pugixml's contract.  pugixml is the external reference.
//!
//! Run:
//!     SUPXML_INPLACE_ITERS=15 cargo bench -p sup-xml-bench --bench in_place_check

use std::ffi::c_void;
use std::os::raw::c_char;
use std::time::Instant;

use sup_xml::{parse_bytes, parse_bytes_in_place, ParseOptions};

const FIXTURE: &str = "../../tests/assets/xml/swiss_prot.xml";

// ── pugixml FFI (shim built by sup-xml-bench's build.rs) ────────────────────
unsafe extern "C" {
    fn pugixml_bench_parse(buf: *const c_char, len: usize) -> *mut c_void;
    fn pugixml_bench_walk(doc: *mut c_void) -> usize;
    fn pugixml_bench_free(doc: *mut c_void);
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

fn bench_all(label: &str, bytes: &[u8], default_opts: &ParseOptions, iters: usize) {
    println!();
    println!("# {label} ({:.1} MB), best of {} iters", bytes.len() as f64 / 1_048_576.0, iters);

    let skip_all = ParseOptions {
        skip_xml_char_validation: true,
        skip_name_validation:     true,
        skip_attr_validation:     true,
        skip_end_tag_check:       true,
        ..default_opts.clone()
    };

    let t_pb = time_best_of_n(|| {
        let doc = parse_bytes(bytes, default_opts).expect("parse_bytes");
        doc.root().children().count()
    }, iters);
    println!("  parse_bytes                       {:>5.0} MB/s  (strict, full XML 1.0 validation)",
        mb_per_sec(bytes.len(), t_pb));

    let t_ip_default = time_best_of_n(|| {
        let buf = bytes.to_vec();
        let doc = parse_bytes_in_place(buf, default_opts).expect("in-place + default opts");
        doc.root().children().count()
    }, iters);
    println!("  parse_bytes_in_place (default)    {:>5.0} MB/s  (destructive, full validation honored, +1 clone/iter)",
        mb_per_sec(bytes.len(), t_ip_default));

    let t_ip_fast = time_best_of_n(|| {
        let buf = bytes.to_vec();
        let doc = parse_bytes_in_place(buf, &skip_all).expect("in-place + skip-all opts");
        doc.root().children().count()
    }, iters);
    println!("  parse_bytes_in_place (skip-all)   {:>5.0} MB/s  (destructive, trust-input, +1 clone/iter)",
        mb_per_sec(bytes.len(), t_ip_fast));

    let t_pugi = time_best_of_n(|| {
        unsafe {
            let doc = pugixml_bench_parse(bytes.as_ptr() as *const c_char, bytes.len());
            assert!(!doc.is_null(), "pugixml parse failed");
            let n = pugixml_bench_walk(doc);
            pugixml_bench_free(doc);
            n
        }
    }, iters);
    println!("  pugixml                           {:>5.0} MB/s  (baseline, non-conformant)",
        mb_per_sec(bytes.len(), t_pugi));

    let pugi = mb_per_sec(bytes.len(), t_pugi);
    println!("  ────");
    println!("  parse_bytes                    vs pugixml: {:.2}×", mb_per_sec(bytes.len(), t_pb)         / pugi);
    println!("  parse_bytes_in_place (default) vs pugixml: {:.2}×", mb_per_sec(bytes.len(), t_ip_default) / pugi);
    println!("  parse_bytes_in_place (skip-all) vs pugixml: {:.2}×", mb_per_sec(bytes.len(), t_ip_fast)   / pugi);
}

fn main() {
    let iters: usize = std::env::var("SUPXML_INPLACE_ITERS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(15);

    let opts = ParseOptions::default();

    // swiss_prot — large real-world fixture (~92 MB).
    let path = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), FIXTURE);
    let swiss = std::fs::read(&path).expect("read swiss_prot.xml");
    bench_all("swiss_prot.xml", &swiss, &opts, iters);

    // Synthetic entity-heavy doc — exercises the slow-path text-decode loop
    // that parse_bytes_in_place's text_decode_buf reuse + prefix-skip target.
    // 4 builtin entities per item × 50k items.
    let mut synthetic = String::from("<root>");
    for _ in 0..50_000 {
        synthetic.push_str("<item>tom &amp; jerry &lt;chase&gt; &quot;cat&quot;</item>");
    }
    synthetic.push_str("</root>");
    let synth: Vec<u8> = synthetic.into_bytes();
    bench_all("synthetic entity-heavy doc (50k items, 4 builtins each)", &synth, &opts, iters);
}
