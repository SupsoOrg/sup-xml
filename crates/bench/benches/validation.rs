//! UTF-8 validation throughput A/B: `simdutf8::compat::from_utf8` (the
//! SIMD drop-in used at every byte-input gate) versus `std::str::from_utf8`
//! (a scalar reference — std validates word-at-a-time, not via dispatched
//! SIMD), across input shapes and sizes.  Run with:
//!
//!   cargo bench -p sup-xml-bench --bench validation
//!
//! The `ratio` column is the SIMD win: how much faster the dispatched
//! AVX2/SSE4.2/NEON validator is than scalar on the same bytes, in one
//! clean binary.
//!
//! Do NOT try to A/B by recompiling with `-C target-feature=-neon` to
//! force simdutf8's scalar path: on AArch64 the FPU and NEON share one
//! register file, so disabling `neon` disables floating-point codegen for
//! the whole binary and every `f64` throughput figure collapses to zero.
//! (That flag is fine for the integer-only equivalence test in
//! `crates/core/src/parser.rs`, which never touches floating point.)  For
//! a same-binary SIMD-vs-own-scalar comparison, enable simdutf8's
//! `public_imp` feature and call its scalar implementation directly.
//!
//! Input shape dominates the result, so three are swept at three sizes:
//! pure ASCII (best case, near memcpy), typical XML (ASCII with sparse
//! 2-byte text), and CJK-dense (back-to-back 3-byte sequences, where the
//! SIMD advantage narrows).  The 48-byte row sits below a 16/32-byte
//! vector width, exposing per-call dispatch overhead that the megabyte
//! rows hide.

use std::hint::black_box;
use std::time::Instant;

/// Printable ASCII, cycling 0x20..0x7E.
fn make_ascii(n: usize) -> Vec<u8> {
    (0..n).map(|i| 0x20 + (i % 95) as u8).collect()
}

/// Mostly ASCII with a 2-byte `é` (U+00E9) every 16th position — the
/// realistic mix for Western-language XML (ASCII markup, occasional
/// accented text).  Never cut mid-sequence, so the buffer stays valid.
fn make_typical(n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n + 1);
    let mut i = 0usize;
    while v.len() < n {
        if i % 16 == 0 && v.len() + 2 <= n {
            v.push(0xC3);
            v.push(0xA9);
        } else {
            v.push(0x20 + (i % 95) as u8);
        }
        i += 1;
    }
    v.truncate(n);
    v
}

/// Back-to-back `中` (U+4E2D, 3 bytes).  Length is rounded down to a whole
/// number of sequences so the buffer is valid UTF-8.
fn make_cjk(n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n + 3);
    while v.len() + 3 <= n {
        v.push(0xE4);
        v.push(0xB8);
        v.push(0xAD);
    }
    v
}

/// Process ~1 GiB total through `f` over `buf`, returning GiB/s.  The XOR
/// accumulator and `black_box` on both ends keep the optimizer from
/// hoisting or deleting the validation.
fn throughput_gibps<F: Fn(&[u8]) -> bool>(buf: &[u8], f: F) -> f64 {
    const TARGET: u64 = 1 << 30;
    let iters = (TARGET / buf.len().max(1) as u64).max(1);
    for _ in 0..(iters / 10).max(1) {
        black_box(f(black_box(buf)));
    }
    let mut acc = false;
    let t = Instant::now();
    for _ in 0..iters {
        acc ^= f(black_box(buf));
    }
    let el = t.elapsed();
    black_box(acc);
    let total = buf.len() as f64 * iters as f64;
    total / el.as_secs_f64() / (1u64 << 30) as f64
}

fn main() {
    let neon = cfg!(target_feature = "neon");
    eprintln!(
        "# validation A/B — arch={} target_feature=neon={}",
        std::env::consts::ARCH,
        neon,
    );
    eprintln!(
        "# {:<10} {:>10}  {:>12}  {:>12}  {:>7}",
        "shape", "size", "simdutf8", "std", "ratio",
    );

    let sizes = [("48B", 48usize), ("64KiB", 64 * 1024), ("4MiB", 4 * 1024 * 1024)];
    let shapes: [(&str, fn(usize) -> Vec<u8>); 3] = [
        ("ascii", make_ascii),
        ("typical", make_typical),
        ("cjk", make_cjk),
    ];

    for (shape, make) in shapes {
        for (size_name, n) in sizes {
            let buf = make(n);
            assert!(
                std::str::from_utf8(&buf).is_ok(),
                "corpus {shape}/{size_name} must be valid UTF-8 so we time the full validation path",
            );
            let simd = throughput_gibps(&buf, |b| simdutf8::compat::from_utf8(b).is_ok());
            let std_ = throughput_gibps(&buf, |b| std::str::from_utf8(b).is_ok());
            eprintln!(
                "  {:<10} {:>10}  {:>9.2} GiB/s  {:>9.2} GiB/s  {:>6.2}x",
                shape,
                size_name,
                simd,
                std_,
                simd / std_,
            );
        }
    }
}
