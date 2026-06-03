//! Micro-benchmark for the numeric / predicate XPath hot path:
//! count, positional predicates, numeric comparison, arithmetic in a
//! predicate, and sum/number coercion. Prints ns/op per expression so
//! a regression in number handling shows up against a recorded
//! baseline. Uses only the public API (`eval_num` returns `f64`), so it
//! can also be checked out against an older commit for an A/B.
//!
//!   cargo run -p sup-xml-bench --example perf_numeric --release

use std::time::Instant;
use sup_xml::{ParseOptions, XPathContext, parse_str};

fn main() {
    println!(
        "sizeof(XPathValue/Value)={}",
        std::mem::size_of::<sup_xml::XPathValue>(),
    );

    // A document with enough numeric-bearing elements that the
    // per-item predicate / coercion cost dominates parse cost.
    let n = 2000;
    let mut xml = String::with_capacity(n * 40);
    xml.push_str("<root>");
    for i in 0..n {
        xml.push_str(&format!("<item v=\"{}\"/>", (i * 37) % 1000));
    }
    xml.push_str("</root>");
    let doc = parse_str(&xml, &ParseOptions::default()).expect("parse");
    // XPath 1.0 strict — the hot path the rework must not regress.
    let ctx = XPathContext::new(&doc);

    // Each expression leans on a different part of the numeric path;
    // wrapping the predicates in count() keeps the result an f64 (pure
    // eval, no stringification) and works identically on both commits.
    let exprs = [
        ("count", "count(//item)"),
        ("positional", "count(//item[position() <= 500])"),
        ("num-compare", "count(//item[@v > 500])"),
        ("arith-pred", "count(//item[(position() mod 7) = 0])"),
        ("sum-attrs", "sum(//item/@v)"),
    ];

    let iters = 3000;
    for (label, expr) in exprs {
        for _ in 0..50 {
            let _ = ctx.eval_num(expr);
        }
        let start = Instant::now();
        let mut sink = 0.0f64;
        for _ in 0..iters {
            sink += ctx.eval_num(expr).expect("eval");
        }
        let ns_per = start.elapsed().as_nanos() as f64 / iters as f64;
        println!("{label:<12} {ns_per:>10.0} ns/op   (sink={sink})");
    }
}
