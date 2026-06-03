//! Profile / reduce the XPath eval slow-unit captured by the fuzz
//! corpus.  Times the original artifact + a battery of reductions
//! to isolate which sub-pattern dominates the step budget.
//!
//! Usage:
//!     cargo run --release --example profile_xpath_slow_unit -- \
//!         crates/core/fuzz/artifacts/fuzz_xpath_eval/slow-unit-<hash>

use std::time::Instant;
use sup_xml_core::{parse_str, ParseOptions};
use sup_xml_core::xpath::XPathContext;

const FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<catalog xmlns:ex="urn:example" xml:lang="en">
  <book id="b1" price="9.99">
    <title>The Pragmatic Programmer</title>
    <author>Hunt</author>
    <author>Thomas</author>
    <year>1999</year>
    <tags><tag>classic</tag><tag>craft</tag></tags>
  </book>
  <book id="b2" price="42">
    <title lang="en">Compilers</title>
    <author>Aho</author>
    <author>Sethi</author>
    <author>Ullman</author>
    <year>2006</year>
    <ex:rating>5</ex:rating>
    <!-- a comment node for comment() tests -->
    <?xml-stylesheet href="x.xsl"?>
    <desc><![CDATA[<not really xml>]]></desc>
  </book>
  <book id="b3" price="0">
    <title/>
    <year>-1</year>
    <empty></empty>
    <nested><a><b><c><d>deep</d></c></b></a></nested>
  </book>
  <unicode>café αβγ 中文 𝛼</unicode>
</catalog>"#;

fn time_one(ctx: &XPathContext, label: &str, expr: &str) {
    let mut best = std::time::Duration::MAX;
    let mut last_result = String::new();
    for _ in 0..3 {
        let t = Instant::now();
        let r = ctx.eval(expr);
        let dt = t.elapsed();
        if dt < best { best = dt; }
        last_result = match &r {
            Ok(_)  => "Ok".to_string(),
            Err(e) => format!("Err: {}", e.message.chars().take(60).collect::<String>()),
        };
    }
    println!("{:>9.2?}  [{:>4} B]  {label}   →  {last_result}",
             best, expr.len());
}

fn main() {
    let path = std::env::args().nth(1)
        .expect("usage: profile_xpath_slow_unit <artifact-path>");
    let bytes = std::fs::read(&path).expect("read artifact");
    let full = std::str::from_utf8(&bytes).expect("artifact is UTF-8");

    let doc = parse_str(FIXTURE, &ParseOptions::default()).expect("fixture parses");
    let ctx = XPathContext::new(&doc);

    // Report the predicate-nesting depth of the original artifact so we
    // can pick a sensible MAX_PREDICATE_NESTING_DEPTH.
    use sup_xml_core::xpath::ast::max_predicate_nesting;
    use sup_xml_core::xpath::parse_xpath;
    match parse_xpath(full) {
        Ok(e)  => println!("# original artifact parses; predicate-nesting depth = {}", max_predicate_nesting(&e)),
        Err(e) => println!("# original artifact parse-error: {}",
                           e.message.chars().take(80).collect::<String>()),
    }

    println!("# best-of-3 timing of slow-unit reductions");
    println!("{:>9}  {:>8}  {}", "time", "len", "expression  →  result");
    println!("{}", "-".repeat(78));

    // ── baseline ──────────────────────────────────────────────────────────
    time_one(&ctx, "FULL artifact (1018 B)", full);

    // ── strip from the right, halving each time ──────────────────────────
    time_one(&ctx, "first 512 B",  &full[..full.len().min(512)]);
    time_one(&ctx, "first 256 B",  &full[..full.len().min(256)]);
    time_one(&ctx, "first 128 B",  &full[..full.len().min(128)]);
    time_one(&ctx, "first  64 B",  &full[..full.len().min(64)]);

    // ── isolate specific patterns ────────────────────────────────────────
    time_one(&ctx, "just nested substring chains",
        "r*substring(/*substring(/*substring(/*substring(/*substring(/*substring(/*substring(//book[author='Hu'])))))))");
    time_one(&ctx, "deep //*/*/* descent chain",
        "//*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*/*");
    time_one(&ctx, "nested predicates (the worst case)",
        "//*[//*[//*[//*[//*[//*[//*[.='x']]]]]]]");
    time_one(&ctx, "nested //*/* in predicate",
        "//*[/*//*//*//*//*//*//*//*//*//*]");
    time_one(&ctx, "wildcard arithmetic chain",
        "//*/*/*/* * * * * * * * * * //*/*/*");
    time_one(&ctx, "many top-level *",
        "* * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * * *");

    // ── single-step costs for comparison ─────────────────────────────────
    time_one(&ctx, "trivial root",                                 "/");
    time_one(&ctx, "trivial wildcard",                             "/*");
    time_one(&ctx, "all elements",                                 "//*");
    time_one(&ctx, "all elements with trivial predicate",          "//*[true()]");
    time_one(&ctx, "all elements with attribute access",           "//*[@id]");
    time_one(&ctx, "books filtered by author",                     "//book[author='Hunt']");
    time_one(&ctx, "substring on string literal",                  "substring('hello', 2, 3)");
    time_one(&ctx, "substring on node text",                       "substring(//title, 2, 3)");
}
