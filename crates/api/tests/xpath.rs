/// XPath 1.0 evaluation tests.
use sup_xml::{
    parse_str, xpath_bool, xpath_count, xpath_num, xpath_str, xpath_strings,
    Document, ParseOptions,
};

fn doc(xml: &str) -> Document {
    parse_str(xml, &ParseOptions::default()).expect("test document must parse")
}

#[test]
fn eval_step_budget_is_tunable() {
    use sup_xml::{XPathContext, XPathOptions};
    let d = doc("<r><a><b/></a><a><b/></a><a><b/></a></r>");
    // Nested absolute-predicate shape: a few thousand charged steps on
    // this tiny doc — trivial under the 20M default, but over a tight cap.
    let expr = "//*[//*[//*[//*]]]";

    // Default budget handles it.
    assert!(
        XPathContext::new(&d).eval(expr).is_ok(),
        "default budget should evaluate this expression"
    );

    // A tightened budget rejects it, and the error reports the
    // *configured* ceiling — proof the knob is wired through.
    let opts = XPathOptions { max_eval_steps: 200, ..Default::default() };
    let err = XPathContext::new_with(&d, opts).eval(expr).unwrap_err();
    assert!(
        err.to_string().contains("budget exceeded (200)"),
        "expected the configured ceiling in the error, got: {err}"
    );
}

#[test]
fn default_xpath_options_has_nonzero_budget() {
    // Guard against a re-derived Default silently zeroing the budget
    // (u64::default() == 0 would reject every expression on step 1).
    assert!(
        sup_xml::XPathOptions::default().max_eval_steps >= 1_000_000,
        "default XPath step budget should be large, not zeroed"
    );
}

// ── basic path selection ──────────────────────────────────────────────────────

#[test]
fn root_element_by_name() {
    let d = doc("<catalog><book/><book/></catalog>");
    assert_eq!(xpath_count(&d, "/catalog").unwrap(), 1);
}

#[test]
fn child_elements_by_name() {
    let d = doc("<catalog><book/><book/><journal/></catalog>");
    assert_eq!(xpath_count(&d, "/catalog/book").unwrap(), 2);
}

#[test]
fn wildcard_children() {
    let d = doc("<root><a/><b/><c/></root>");
    assert_eq!(xpath_count(&d, "/root/*").unwrap(), 3);
}

#[test]
fn descendant_double_slash() {
    let d = doc("<a><b><c/></b><c/></a>");
    assert_eq!(xpath_count(&d, "//c").unwrap(), 2);
}

#[test]
fn absolute_double_slash() {
    let d = doc("<root><x><y/></x><y/></root>");
    assert_eq!(xpath_count(&d, "//y").unwrap(), 2);
}

#[test]
fn self_axis() {
    let d = doc("<root/>");
    assert_eq!(xpath_count(&d, "/root/self::node()").unwrap(), 1);
}

#[test]
fn parent_axis() {
    let d = doc("<a><b/></a>");
    // /a/b/parent::* should be the <a> element
    assert_eq!(xpath_count(&d, "/a/b/parent::*").unwrap(), 1);
    assert_eq!(xpath_str(&d, "name(/a/b/parent::*)").unwrap(), "a");
}

#[test]
fn ancestor_axis() {
    let d = doc("<a><b><c/></b></a>");
    assert_eq!(xpath_count(&d, "/a/b/c/ancestor::*").unwrap(), 2); // b and a
}

#[test]
fn following_sibling_axis() {
    let d = doc("<r><a/><b/><c/></r>");
    assert_eq!(xpath_count(&d, "/r/a/following-sibling::*").unwrap(), 2);
}

#[test]
fn preceding_sibling_axis() {
    let d = doc("<r><a/><b/><c/></r>");
    assert_eq!(xpath_count(&d, "/r/c/preceding-sibling::*").unwrap(), 2);
}

// ── attribute selection ───────────────────────────────────────────────────────

#[test]
fn attribute_axis() {
    let d = doc(r#"<root id="42" name="foo"/>"#);
    assert_eq!(xpath_count(&d, "/root/@*").unwrap(), 2);
}

#[test]
fn attribute_by_name() {
    let d = doc(r#"<root id="99"/>"#);
    assert_eq!(xpath_str(&d, "/root/@id").unwrap(), "99");
}

#[test]
fn attribute_shorthand() {
    let d = doc(r#"<r a="x" b="y"/>"#);
    assert_eq!(xpath_str(&d, "/r/@a").unwrap(), "x");
}

// ── node type tests ───────────────────────────────────────────────────────────

#[test]
fn text_node_test() {
    let d = doc("<r>hello</r>");
    let strings = xpath_strings(&d, "/r/text()").unwrap();
    assert_eq!(strings, ["hello"]);
}

#[test]
fn comment_node_test() {
    let d = doc("<r><!-- my comment --></r>");
    assert_eq!(xpath_count(&d, "/r/comment()").unwrap(), 1);
    let strings = xpath_strings(&d, "/r/comment()").unwrap();
    assert_eq!(strings, [" my comment "]);
}

#[test]
fn pi_node_test() {
    let d = doc("<?xml version=\"1.0\"?><r><?target content?></r>");
    assert_eq!(xpath_count(&d, "/r/processing-instruction()").unwrap(), 1);
}

#[test]
fn pi_named_node_test() {
    let d = doc("<?xml version=\"1.0\"?><r><?foo data?><?bar other?></r>");
    assert_eq!(xpath_count(&d, "/r/processing-instruction('foo')").unwrap(), 1);
}

#[test]
fn node_type_test_matches_all() {
    let d = doc("<r><a/>text<!-- c --></r>");
    assert!(xpath_count(&d, "/r/node()").unwrap() >= 2);
}

// ── predicates ────────────────────────────────────────────────────────────────

#[test]
fn position_predicate() {
    let d = doc("<r><a/><b/><c/></r>");
    assert_eq!(xpath_count(&d, "/r/*[2]").unwrap(), 1);
    assert_eq!(xpath_str(&d, "name(/r/*[2])").unwrap(), "b");
}

#[test]
fn last_predicate() {
    let d = doc("<r><a/><b/><c/></r>");
    assert_eq!(xpath_count(&d, "/r/*[last()]").unwrap(), 1);
    assert_eq!(xpath_str(&d, "name(/r/*[last()])").unwrap(), "c");
}

#[test]
fn attribute_value_predicate() {
    let d = doc(r#"<r><item id="1"/><item id="2"/><item id="3"/></r>"#);
    assert_eq!(xpath_count(&d, r#"/r/item[@id="2"]"#).unwrap(), 1);
}

#[test]
fn existence_predicate() {
    let d = doc(r#"<r><item x="1"/><item/><item x="2"/></r>"#);
    assert_eq!(xpath_count(&d, "/r/item[@x]").unwrap(), 2);
}

// ── boolean and logical ───────────────────────────────────────────────────────

#[test]
fn xpath_boolean_true_false() {
    let d = doc("<r/>");
    assert!(xpath_bool(&d, "true()").unwrap());
    assert!(!xpath_bool(&d, "false()").unwrap());
    assert!(xpath_bool(&d, "not(false())").unwrap());
}

#[test]
fn xpath_and_or() {
    let d = doc("<r/>");
    assert!(xpath_bool(&d, "true() and true()").unwrap());
    assert!(!xpath_bool(&d, "true() and false()").unwrap());
    assert!(xpath_bool(&d, "false() or true()").unwrap());
    assert!(!xpath_bool(&d, "false() or false()").unwrap());
}

#[test]
fn xpath_eq_ne() {
    let d = doc("<r/>");
    assert!(xpath_bool(&d, "1 = 1").unwrap());
    assert!(!xpath_bool(&d, "1 = 2").unwrap());
    assert!(xpath_bool(&d, "1 != 2").unwrap());
    assert!(xpath_bool(&d, "'hello' = 'hello'").unwrap());
}

#[test]
fn xpath_comparison_operators() {
    let d = doc("<r/>");
    assert!(xpath_bool(&d, "1 < 2").unwrap());
    assert!(xpath_bool(&d, "2 > 1").unwrap());
    assert!(xpath_bool(&d, "1 <= 1").unwrap());
    assert!(xpath_bool(&d, "2 >= 2").unwrap());
    assert!(!xpath_bool(&d, "3 < 2").unwrap());
}

// ── arithmetic ────────────────────────────────────────────────────────────────

#[test]
fn arithmetic_operations() {
    let d = doc("<r/>");
    assert_eq!(xpath_num(&d, "1 + 2").unwrap(), 3.0);
    assert_eq!(xpath_num(&d, "5 - 3").unwrap(), 2.0);
    assert_eq!(xpath_num(&d, "3 * 4").unwrap(), 12.0);
    assert_eq!(xpath_num(&d, "10 div 4").unwrap(), 2.5);
    assert_eq!(xpath_num(&d, "10 mod 3").unwrap(), 1.0);
    assert_eq!(xpath_num(&d, "-5").unwrap(), -5.0);
}

// ── core string functions ─────────────────────────────────────────────────────

#[test]
fn fn_string() {
    let d = doc("<r>hello</r>");
    assert_eq!(xpath_str(&d, "string(/r)").unwrap(), "hello");
}

#[test]
fn fn_concat() {
    let d = doc("<r/>");
    assert_eq!(xpath_str(&d, "concat('a', 'b', 'c')").unwrap(), "abc");
}

#[test]
fn fn_contains() {
    let d = doc("<r/>");
    assert!(xpath_bool(&d, "contains('hello world', 'world')").unwrap());
    assert!(!xpath_bool(&d, "contains('hello', 'xyz')").unwrap());
}

#[test]
fn fn_starts_with() {
    let d = doc("<r/>");
    assert!(xpath_bool(&d, "starts-with('hello', 'hel')").unwrap());
    assert!(!xpath_bool(&d, "starts-with('hello', 'ell')").unwrap());
}

#[test]
fn fn_substring() {
    let d = doc("<r/>");
    assert_eq!(xpath_str(&d, "substring('hello', 2, 3)").unwrap(), "ell");
    assert_eq!(xpath_str(&d, "substring('hello', 3)").unwrap(), "llo");
}

#[test]
fn fn_substring_before_after() {
    let d = doc("<r/>");
    assert_eq!(xpath_str(&d, "substring-before('2024-01-15', '-')").unwrap(), "2024");
    assert_eq!(xpath_str(&d, "substring-after('2024-01-15', '-')").unwrap(), "01-15");
}

#[test]
fn fn_string_length() {
    let d = doc("<r/>");
    assert_eq!(xpath_num(&d, "string-length('hello')").unwrap(), 5.0);
    assert_eq!(xpath_num(&d, "string-length('')").unwrap(), 0.0);
}

#[test]
fn fn_normalize_space() {
    let d = doc("<r/>");
    assert_eq!(
        xpath_str(&d, "normalize-space('  hello   world  ')").unwrap(),
        "hello world"
    );
}

#[test]
fn fn_translate() {
    let d = doc("<r/>");
    assert_eq!(xpath_str(&d, "translate('hello', 'aeiou', 'AEIOU')").unwrap(), "hEllO");
    // Characters in 'from' with no 'to' counterpart are removed
    assert_eq!(xpath_str(&d, "translate('hello', 'aeiou', 'AE')").unwrap(), "hEll");
}

// ── numeric functions ─────────────────────────────────────────────────────────

#[test]
fn fn_number() {
    let d = doc("<r/>");
    assert_eq!(xpath_num(&d, "number('42')").unwrap(), 42.0);
    assert_eq!(xpath_num(&d, "number(true())").unwrap(), 1.0);
    assert_eq!(xpath_num(&d, "number(false())").unwrap(), 0.0);
    assert!(xpath_num(&d, "number('notanumber')").unwrap().is_nan());
}

#[test]
fn fn_sum() {
    let d = doc("<r><v>1</v><v>2</v><v>3</v></r>");
    assert_eq!(xpath_num(&d, "sum(/r/v)").unwrap(), 6.0);
}

#[test]
fn fn_floor_ceiling_round() {
    let d = doc("<r/>");
    assert_eq!(xpath_num(&d, "floor(3.7)").unwrap(), 3.0);
    assert_eq!(xpath_num(&d, "ceiling(3.2)").unwrap(), 4.0);
    assert_eq!(xpath_num(&d, "round(3.5)").unwrap(), 4.0);
    assert_eq!(xpath_num(&d, "round(3.4)").unwrap(), 3.0);
}

// ── node functions ────────────────────────────────────────────────────────────

#[test]
fn fn_name() {
    let d = doc("<catalog><book/></catalog>");
    assert_eq!(xpath_str(&d, "name(/catalog)").unwrap(), "catalog");
    assert_eq!(xpath_str(&d, "name(/catalog/book)").unwrap(), "book");
    // name() with no args on context node (document) returns ""
    assert_eq!(xpath_str(&d, "name()").unwrap(), "");
}

#[test]
fn fn_local_name() {
    let d = doc("<root/>");
    assert_eq!(xpath_str(&d, "local-name(/root)").unwrap(), "root");
}

#[test]
fn fn_count() {
    let d = doc("<r><a/><b/><c/></r>");
    assert_eq!(xpath_num(&d, "count(/r/*)").unwrap(), 3.0);
}

// ── union operator ────────────────────────────────────────────────────────────

#[test]
fn union_operator() {
    let d = doc("<r><a/><b/></r>");
    assert_eq!(xpath_count(&d, "/r/a | /r/b").unwrap(), 2);
}

#[test]
fn union_deduplicates() {
    let d = doc("<r><a/></r>");
    assert_eq!(xpath_count(&d, "/r/a | /r/a").unwrap(), 1);
}

// ── dot and dotdot ───────────────────────────────────────────────────────────

#[test]
fn dot_is_self() {
    let d = doc("<r><item/></r>");
    assert_eq!(xpath_count(&d, "/r/item/.").unwrap(), 1);
}

#[test]
fn dotdot_is_parent() {
    let d = doc("<r><item/></r>");
    assert_eq!(xpath_str(&d, "name(/r/item/..)").unwrap(), "r");
}

// ── deeper nesting + mix ──────────────────────────────────────────────────────

#[test]
fn descendant_then_predicate() {
    let d = doc(r#"<r><x><item id="1"/></x><item id="2"/></r>"#);
    assert_eq!(xpath_count(&d, r#"//item[@id="1"]"#).unwrap(), 1);
}

#[test]
fn count_with_position_predicate() {
    let d = doc("<r><a/><a/><a/></r>");
    // First of three <a> elements
    assert_eq!(xpath_count(&d, "/r/a[1]").unwrap(), 1);
    // Last of three
    assert_eq!(xpath_count(&d, "/r/a[last()]").unwrap(), 1);
    // Position > 1
    assert_eq!(xpath_count(&d, "/r/a[position() > 1]").unwrap(), 2);
}

#[test]
fn mixed_text_element_content() {
    let d = doc("<p>Hello <em>world</em>!</p>");
    // String value of element includes all text
    assert_eq!(xpath_str(&d, "string(/p)").unwrap(), "Hello world!");
    assert_eq!(xpath_count(&d, "/p/text()").unwrap(), 2);
}

// ── parse-only tests (expression parsing without full evaluation) ─────────────

#[test]
fn parse_complex_xpath_expressions() {
    use sup_xml::parse_xpath;
    let exprs = [
        "/root/child::*",
        "//item[@id='x']",
        "descendant-or-self::node()",
        "count(/r/*) > 0",
        "not(contains(., 'bad'))",
        "ancestor::div[1]",
        "/a/b[position() mod 2 = 0]",
        "concat('prefix-', @name, '-suffix')",
        "//item[last()]",
        "substring(string(/r), 1, 5)",
    ];
    for expr in exprs {
        parse_xpath(expr).unwrap_or_else(|e| panic!("failed to parse {expr:?}: {e}"));
    }
}

// ── fuzzer-found regressions ──────────────────────────────────────────────────

/// Regression guard: a 227-byte XPath with deeply nested `//` axes inside
/// predicates is exactly the N^k DoS shape that the eval step budget
/// (`eval::DEFAULT_MAX_EVAL_STEPS`) short-circuits. This test pins that the budget keeps
/// firing — a regression that disables or unhooks the step counter
/// would otherwise hang the process for many minutes (libFuzzer
/// observed ~750s under SanCov+ASan instrumentation).
///
/// Ignored by default because hitting the step budget still takes
/// several seconds in unoptimised debug builds; run with
/// `cargo test --release -- --ignored`.
#[ignore = "slow: walks XPath step-budget; ~400ms release, several seconds debug"]
#[test]
fn fuzz_regression_xpath_eval_nested_descendant_budget_fires() {
    use std::sync::mpsc;
    use std::time::Duration;

    // Byte-exact reproduction from
    // crates/core/fuzz/artifacts/fuzz_xpath_eval/slow-unit-89133cfeb568...
    // Note the embedded form-feed (\x0c) — XPath whitespace, kept faithful
    // to the original fuzzer artifact.
    const SLOW_INPUT: &str = "//book/following::d=z*//*/*/**//*[p0=..=.>.=//*//*/**//*//*[p..//book/following::d=z*//*/*/**//*[p0=..=.>.=//*//*/**//*//*[p..=.=.+gPpt!= \"\"\x0c+c=//preceding::z*//preceding::Zls]*//od*/]=.=./.=.=..=.>..=.>..//*/*/*//..+ls]*//o*/]";

    // Same fixture the fuzz target uses, so we exercise the same eval paths.
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

    // Document holds a !Sync arena — construct it inside the worker thread.
    // Use `eval_strings` (not `xpath_count`) so we get the underlying
    // error message rather than a swallowed-to-0 count.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        use sup_xml::xpath_strings;
        let d = doc(FIXTURE);
        let _ = tx.send(xpath_strings(&d, SLOW_INPUT));
    });

    // Generous hang-detector: real budget firing is well under this on any
    // build mode. If a regression unhooks the step counter the worker would
    // run for minutes, so we'd rather fail in 60s than wedge CI.
    let limit = Duration::from_secs(60);
    let result = rx
        .recv_timeout(limit)
        .unwrap_or_else(|_| panic!(
            "evaluation did not return within {limit:?} — step budget likely \
             unhooked; expression should error in <1s on release builds"
        ));

    let err = result.expect_err(
        "expected the step-budget to fire on this nested-predicate DoS input, \
         but evaluation succeeded — the eval-step counter may have been \
         disabled or the input simplified upstream",
    );
    assert!(
        err.message.contains("step budget exceeded"),
        "expected 'step budget exceeded' error, got: {}",
        err.message,
    );
}

/// Regression guard: a 26-byte XPath panicked inside `following()` with
/// a slice OOB — `//@*[last()]//following::u`. Attribute nodes are not
/// in their parent's `children()` list, so the position-lookup fallback
/// (`unwrap_or(siblings.len())`) plus the subsequent `+ 1` and slice
/// produced `siblings[len + 1 ..]` and panicked.
///
/// Per XPath 1.0 §5, attributes precede their parent's children in
/// document order, so `following::` from an attribute yields all of
/// the parent's children (plus the parent's following-context).
/// There are no `u` elements in this fixture, so the node-set is empty.
///
/// Found by `fuzz_xpath_eval`.
#[test]
fn fuzz_regression_xpath_eval_following_from_attribute() {
    const CRASH_INPUT: &str = "//@*[last()]//following::u";

    let d = doc(
        r#"<?xml version="1.0"?>
<catalog xmlns:ex="urn:example" xml:lang="en">
  <book id="b1"><title>x</title></book>
  <book id="b2"><title>y</title></book>
</catalog>"#,
    );

    let result = xpath_strings(&d, CRASH_INPUT).expect("must not panic or error");
    assert!(result.is_empty(), "no `u` elements exist; got {result:?}");
}

/// Parallel guard for `following-sibling::` from an attribute. Same
/// off-by-one shape as the `following::` bug above lived in
/// `following_siblings()`; the fuzzer hadn't hit it yet but the audit
/// found it sitting next door. XPath 1.0 §2.2 defines this axis as
/// empty for attribute and namespace nodes.
#[test]
fn fuzz_regression_xpath_eval_following_sibling_from_attribute() {
    let d = doc(r#"<r a="1" b="2"><c/><c/></r>"#);
    let result = xpath_strings(&d, "//@*/following-sibling::*")
        .expect("must not panic or error");
    assert!(result.is_empty(), "following-sibling is empty for attributes; got {result:?}");
}
