//! XSD §F regex engine microbench.
//!
//! Measures per-match throughput on representative XSD pattern
//! facets, partitioned by which compile-time dispatch path the
//! engine takes:
//!
//! - **linear**: forward-only fast path for the common
//!   `[class]{quant}[class]{quant}…` shape (most XSD `xs:pattern`
//!   facets in real schemas).
//! - **nfa**: full Pike VM, used for anything with alternation,
//!   grouping, or an unbounded quantifier followed by more atoms.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p sup-xml-bench --bench xsd_regex
//! ```

use std::time::{Duration, Instant};

use sup_xml_core::xsd::regex::Pattern;

struct Case {
    pattern:  &'static str,
    input:    &'static str,
    expected: bool,
}

/// Patterns the linear fast path can handle.
const LINEAR_CASES: &[Case] = &[
    Case { pattern: r"E\d{6}",                            input: "E000123",                          expected: true  },
    Case { pattern: r"E\d{6}",                            input: "X000123",                          expected: false },
    Case { pattern: r"[A-Z]{3}-\d{4}-[a-f0-9]{8}",        input: "ABC-1234-deadbeef",                expected: true  },
    Case { pattern: r"\d{5}",                             input: "12345",                            expected: true  },
    Case { pattern: r"[a-z][a-z0-9_\-\.]*",               input: "org.example.module",               expected: true  },
    Case { pattern: r"\d+",                               input: "1234567890",                       expected: true  },
    Case { pattern: r"[A-Fa-f0-9]{32}",                   input: "d41d8cd98f00b204e9800998ecf8427e", expected: true  },
];

/// Patterns that fall through to the NFA: alternation, grouping,
/// or unbounded quants followed by more atoms.
const NFA_CASES: &[Case] = &[
    Case { pattern: r"\d{5}(-\d{4})?",                    input: "12345-6789",                       expected: true  },
    Case { pattern: r"\d+\.\d+\.\d+(-[A-Za-z0-9\-\.]+)?", input: "1.2.3-rc1",                        expected: true  },
    Case { pattern: r"(cat|dog|bird|fish)",               input: "fish",                             expected: true  },
    Case { pattern: r"\d+5",                              input: "12345",                            expected: true  },
];

fn time_match(pat: &Pattern, input: &str, expected: bool) -> f64 {
    let target = Duration::from_millis(50);
    let mut iters = 1024u64;
    loop {
        let elapsed = run_n(pat, input, iters);
        if elapsed >= target || iters > (1 << 28) { break; }
        iters = iters.saturating_mul(2);
    }
    let elapsed = run_n(pat, input, iters);
    let sanity = pat.is_match(input);
    assert_eq!(sanity, expected, "verdict for {}", pat.src());
    elapsed.as_nanos() as f64 / iters as f64
}

fn run_n(pat: &Pattern, input: &str, iters: u64) -> Duration {
    let start = Instant::now();
    let mut sink = 0u64;
    for _ in 0..iters {
        if std::hint::black_box(pat).is_match(std::hint::black_box(input)) {
            sink = sink.wrapping_add(1);
        } else {
            sink = sink.wrapping_sub(1);
        }
    }
    std::hint::black_box(sink);
    start.elapsed()
}

fn bench(label: &str, cases: &[Case]) {
    println!(
        "\n── {label} ──────────────────────────────────────────────────────────────────",
    );
    println!(
        "  {:<46} {:<28}   {:>10}   {:>10}   {:>7}",
        "pattern", "input", "linear", "nfa", "speedup",
    );
    for c in cases {
        let lin = Pattern::compile(c.pattern)
            .unwrap_or_else(|e| panic!("compile {:?}: {e}", c.pattern));
        let nfa = Pattern::compile_nfa_only(c.pattern)
            .unwrap_or_else(|e| panic!("compile_nfa_only {:?}: {e}", c.pattern));
        let _ = lin.is_match(c.input);
        let _ = nfa.is_match(c.input);
        let ns_lin = time_match(&lin, c.input, c.expected);
        let ns_nfa = time_match(&nfa, c.input, c.expected);
        let ratio = ns_nfa / ns_lin;
        println!(
            "  {:<46} {:<28}   {:>7.1} ns   {:>7.1} ns   {:>6.2}×",
            truncate(c.pattern, 46),
            truncate(c.input,   28),
            ns_lin,
            ns_nfa,
            ratio,
        );
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() }
    else {
        let mut out: String = s.chars().take(n - 1).collect();
        out.push('…');
        out
    }
}

fn main() {
    bench("linear fast path", LINEAR_CASES);
    bench("Pike VM (NFA)",   NFA_CASES);
}
