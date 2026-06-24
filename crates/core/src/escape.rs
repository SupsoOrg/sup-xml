//! SIMD-accelerated XML text / attribute escaping.
//!
//! Escaping content for the common UTF-8 output charset reduces to: scan for
//! the next byte that needs replacing, bulk-copy the clean run before it, emit
//! the replacement, repeat.  Every byte that needs escaping is ASCII and never
//! appears inside a multibyte UTF-8 sequence, so the scan can run over raw
//! bytes and clean runs (multibyte content included) copy verbatim.
//!
//! Finding the next special byte is what this module vectorizes: [`std::arch`]
//! SIMD compares classify 16 bytes per step (NEON on aarch64, SSE2 on
//! x86_64), so long clean runs — the overwhelmingly common case — are skipped
//! a vector at a time instead of byte by byte.  Each clean run is still copied
//! with a single `extend_from_slice` (one `memcpy`), and every replacement is
//! a length-known byte-string literal so its copy inlines.
//!
//! # Unsafe
//! This module is exempt from the crate's usual `#![forbid(unsafe_code)]`
//! (see CONTRIBUTING.md § "Unsafe policy") because it calls target SIMD
//! intrinsics.  The unsafe is confined to the per-architecture `mask_*`
//! helpers in [`simd`]; each reads exactly the 16 bytes of a fixed-size stack
//! array the caller sliced safely, so no out-of-bounds access is possible.
//! Under Miri — which cannot execute the intrinsics — the SIMD chunk loop is
//! compiled out and the scalar path runs instead, keeping memory behavior
//! checkable.  Correctness of the SIMD path is pinned by `simd_matches_scalar`
//! below, which diffs it against a naive reference across chunk boundaries.

/// Per-architecture SIMD that classifies 16 bytes at once, returning a 16-bit
/// mask whose bit `k` is set iff byte `k` of the chunk needs escaping.
#[cfg(all(any(target_arch = "aarch64", target_arch = "x86_64"), not(miri)))]
mod simd {
    /// Bytes consumed per SIMD step.
    pub const LANES: usize = 16;

    #[cfg(target_arch = "aarch64")]
    use std::arch::aarch64::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    /// Collapse a per-lane comparison vector (0xFF where matched) into a
    /// 16-bit lane mask.  NEON has no `movemask`, so AND each lane with its
    /// bit value and horizontally sum the two halves.
    #[cfg(target_arch = "aarch64")]
    #[inline]
    unsafe fn movemask(cmp: uint8x16_t) -> u16 {
        unsafe {
            const BITS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
            let masked = vandq_u8(cmp, vld1q_u8(BITS.as_ptr()));
            let lo = vaddv_u8(vget_low_u8(masked)) as u16;
            let hi = vaddv_u8(vget_high_u8(masked)) as u16;
            lo | (hi << 8)
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[inline]
    unsafe fn any_of(chunk: &[u8; LANES], needles: &[u8]) -> u16 {
        unsafe {
            let v = vld1q_u8(chunk.as_ptr());
            let mut acc = vdupq_n_u8(0);
            for &n in needles {
                acc = vorrq_u8(acc, vceqq_u8(v, vdupq_n_u8(n)));
            }
            movemask(acc)
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[inline]
    unsafe fn any_of(chunk: &[u8; LANES], needles: &[u8]) -> u16 {
        unsafe {
            let v = _mm_loadu_si128(chunk.as_ptr() as *const __m128i);
            let mut acc = _mm_setzero_si128();
            for &n in needles {
                acc = _mm_or_si128(acc, _mm_cmpeq_epi8(v, _mm_set1_epi8(n as i8)));
            }
            _mm_movemask_epi8(acc) as u16
        }
    }

    /// Mask of bytes needing escaping in XML text content: `&`, `<`, `>`, CR.
    #[inline]
    pub unsafe fn mask_text(chunk: &[u8; LANES]) -> u16 {
        unsafe { any_of(chunk, &[b'&', b'<', b'>', b'\r']) }
    }

    /// Mask of bytes needing escaping in an attribute value: `&`, `<`, `"`,
    /// tab, LF, CR.
    #[inline]
    pub unsafe fn mask_attr(chunk: &[u8; LANES]) -> u16 {
        unsafe { any_of(chunk, &[b'&', b'<', b'"', b'\t', b'\n', b'\r']) }
    }
}

/// Generate an escaper that SIMD-scans for the bytes in its replacement table
/// and emits each via a length-known literal.
macro_rules! escaper {
    ($(#[$doc:meta])* $name:ident, $mask:ident, [ $($byte:literal => $rep:literal),+ $(,)? ]) => {
        $(#[$doc])*
        pub(crate) fn $name(out: &mut Vec<u8>, bytes: &[u8]) {
            let mut run_start = 0usize;
            let mut i = 0usize;

            // SIMD fast path: skip clean 16-byte chunks a vector at a time;
            // when a chunk carries specials, emit each before advancing.
            #[cfg(all(any(target_arch = "aarch64", target_arch = "x86_64"), not(miri)))]
            while i + simd::LANES <= bytes.len() {
                // SAFETY: the slice is exactly `LANES` bytes, and the mask
                // helper only reads those bytes — no out-of-bounds access.
                // Why unsafe: this runs on every serialized text node and
                // attribute value; the SIMD scan classifies 16 bytes per step
                // where the safe loop classifies one, and that scan dominates
                // serializer throughput on text-heavy documents.
                let chunk: &[u8; simd::LANES] =
                    bytes[i..i + simd::LANES].try_into().unwrap();
                let mut mask = unsafe { simd::$mask(chunk) };
                while mask != 0 {
                    let pos = i + mask.trailing_zeros() as usize;
                    if pos > run_start {
                        out.extend_from_slice(&bytes[run_start..pos]);
                    }
                    match bytes[pos] {
                        $( $byte => out.extend_from_slice($rep), )+
                        _ => unreachable!("mask bit set on a non-special byte"),
                    }
                    run_start = pos + 1;
                    mask &= mask - 1;
                }
                i += simd::LANES;
            }

            // Scalar remainder — and the whole input on Miri / other targets.
            while i < bytes.len() {
                match bytes[i] {
                    $( $byte => {
                        if i > run_start {
                            out.extend_from_slice(&bytes[run_start..i]);
                        }
                        out.extend_from_slice($rep);
                        run_start = i + 1;
                    } )+
                    _ => {}
                }
                i += 1;
            }
            out.extend_from_slice(&bytes[run_start..]);
        }
    };
}

escaper!(
    /// Append `bytes` to `out` with XML text-content escaping.  `\r` is
    /// emitted as `&#xD;` so a round-trip parse does not normalize CR to LF
    /// (XML § 2.11).
    push_escaped_text_utf8,
    mask_text,
    [b'&' => b"&amp;", b'<' => b"&lt;", b'>' => b"&gt;", b'\r' => b"&#xD;"]
);

escaper!(
    /// Append `bytes` to `out` with XML attribute-value escaping.  Tab / LF /
    /// CR are emitted as char-refs so attribute-value normalization (XML
    /// § 3.3.3) does not collapse them to spaces.
    push_escaped_attr_utf8,
    mask_attr,
    [b'&' => b"&amp;", b'<' => b"&lt;", b'"' => b"&quot;",
     b'\t' => b"&#x9;", b'\n' => b"&#xA;", b'\r' => b"&#xD;"]
);

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_text(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for c in s.chars() {
            match c {
                '&' => out.extend_from_slice(b"&amp;"),
                '<' => out.extend_from_slice(b"&lt;"),
                '>' => out.extend_from_slice(b"&gt;"),
                '\r' => out.extend_from_slice(b"&#xD;"),
                c => out.extend_from_slice(c.encode_utf8(&mut [0u8; 4]).as_bytes()),
            }
        }
        out
    }

    fn ref_attr(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for c in s.chars() {
            match c {
                '&' => out.extend_from_slice(b"&amp;"),
                '<' => out.extend_from_slice(b"&lt;"),
                '"' => out.extend_from_slice(b"&quot;"),
                '\t' => out.extend_from_slice(b"&#x9;"),
                '\n' => out.extend_from_slice(b"&#xA;"),
                '\r' => out.extend_from_slice(b"&#xD;"),
                c => out.extend_from_slice(c.encode_utf8(&mut [0u8; 4]).as_bytes()),
            }
        }
        out
    }

    fn run_text(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        push_escaped_text_utf8(&mut out, s.as_bytes());
        out
    }

    fn run_attr(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        push_escaped_attr_utf8(&mut out, s.as_bytes());
        out
    }

    #[test]
    fn basic_text() {
        assert_eq!(run_text("a & b < c > d\r"), b"a &amp; b &lt; c &gt; d&#xD;");
    }

    #[test]
    fn basic_attr() {
        // '>' is not escaped in attribute values (XML § 2.4).
        assert_eq!(run_attr("x\t\"q\"\n>"), b"x&#x9;&quot;q&quot;&#xA;>");
    }

    #[test]
    fn multibyte_passthrough() {
        assert_eq!(run_text("日本語<🦀>&"), "日本語&lt;🦀&gt;&amp;".as_bytes());
    }

    /// The SIMD path must agree with the naive reference for every input —
    /// especially around the 16-byte chunk boundary and for escape-dense and
    /// multibyte content.
    #[test]
    fn simd_matches_scalar() {
        // Building blocks that mix clean ASCII, all escapable bytes, and
        // multibyte sequences whose lead/continuation bytes must never be
        // mistaken for an ASCII special.
        let units = [
            "a", "&", "<", ">", "\"", "\t", "\n", "\r", " ", "z",
            "日", "🦀", "ñ", "ab", "<>&",
        ];
        // Lengths that straddle the 16-byte SIMD stride from both sides.
        for len in 0..40usize {
            for (seed, _) in units.iter().enumerate() {
                let mut s = String::new();
                let mut k = seed;
                while s.len() < len {
                    s.push_str(units[k % units.len()]);
                    k += 1;
                }
                assert_eq!(run_text(&s), ref_text(&s), "text mismatch for {s:?}");
                assert_eq!(run_attr(&s), ref_attr(&s), "attr mismatch for {s:?}");
            }
        }
    }

    #[test]
    fn escape_dense_and_empty() {
        assert_eq!(run_text(""), b"");
        let dense = "<>&".repeat(20);
        assert_eq!(run_text(&dense), ref_text(&dense));
        let dense_attr = "\t\n\"&".repeat(20);
        assert_eq!(run_attr(&dense_attr), ref_attr(&dense_attr));
    }
}
