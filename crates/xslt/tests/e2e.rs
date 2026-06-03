//! End-to-end XSLT tests — string in, string out.
//! Exercises the full pipeline: compile → apply → serialise.

use sup_xml_core::{parse_str, ParseOptions};
use sup_xml_xslt::Stylesheet;

fn transform(stylesheet: &str, source: &str) -> String {
    let xslt = Stylesheet::compile_str(stylesheet).expect("compile");
    let doc  = parse_str(source, &ParseOptions::default()).expect("source parse");
    let result = xslt.apply(&doc).expect("apply");
    result.to_string().expect("serialise")
}

const HEAD: &str = r#"<xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform">"#;
const TAIL: &str = "</xsl:stylesheet>";
fn wrap(body: &str) -> String { format!("{HEAD}{body}{TAIL}") }

#[test]
fn identity_subset() {
    // Minimal identity-ish transform: copy the root element with
    // its children's text content.
    let out = transform(
        &wrap(r#"<xsl:output method="xml" omit-xml-declaration="yes"/>
            <xsl:template match="/"><out><xsl:apply-templates/></out></xsl:template>"#),
        "<r>hello</r>",
    );
    assert!(out.contains("<out>hello</out>"), "got: {out}");
}

#[test]
fn evaluate_runs_dynamic_xpath_with_params() {
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <out>
                <!-- dynamic XPath built from data, with a bound param -->
                <xsl:evaluate xpath="doc/@expr" context-item="doc"/>
                <sum><xsl:evaluate xpath="'$a + $b'">
                    <xsl:with-param name="a" select="40"/>
                    <xsl:with-param name="b" select="2"/>
                </xsl:evaluate></sum>
            </out>
        </xsl:template>
    </xsl:stylesheet>"##;
    let out = transform(xslt, r#"<doc expr="count(item)"><item/><item/><item/></doc>"#);
    assert_eq!(out, "<out>3<sum>42</sum></out>");
}

#[test]
fn package_root_with_static_param_and_shadow_attribute() {
    // xsl:package as root; a static parameter feeds a shadow attribute
    // (_name) that names an accumulator (XSLT 3.0 §3.5 / §3.9).
    let xslt = r##"<xsl:package
        name="http://example/pkg" package-version="1.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:xs="http://www.w3.org/2001/XMLSchema"
        exclude-result-prefixes="xs" version="3.0">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:param name="acc" static="yes" select="'fignum'"/>
        <xsl:accumulator _name="{$acc}" as="xs:integer" initial-value="0">
            <xsl:accumulator-rule match="fig" select="$value + 1"/>
        </xsl:accumulator>
        <xsl:template match="/"><out><xsl:apply-templates select="//fig"/></out></xsl:template>
        <xsl:template match="fig"><n><xsl:value-of select="accumulator-after('fignum')"/></n></xsl:template>
    </xsl:package>"##;
    let out = transform(xslt, "<doc><fig/><fig/></doc>");
    assert_eq!(out, "<out><n>1</n><n>2</n></out>");
}

#[test]
fn accumulator_numbers_nodes_in_document_order() {
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:xs="http://www.w3.org/2001/XMLSchema"
        exclude-result-prefixes="xs">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:accumulator name="fignum" as="xs:integer" initial-value="0">
            <xsl:accumulator-rule match="fig" select="$value + 1"/>
        </xsl:accumulator>
        <xsl:template match="/"><out><xsl:apply-templates select="//fig"/></out></xsl:template>
        <xsl:template match="fig">
            <f b="{accumulator-before('fignum')}" a="{accumulator-after('fignum')}"/>
        </xsl:template>
    </xsl:stylesheet>"##;
    let out = transform(xslt, "<doc><fig/><sec><fig/></sec><fig/></doc>");
    // pre-descent (before) reflects the fig's own +1 rule; for a leaf
    // fig the post-descent (after) value is the same.
    assert_eq!(out,
        r#"<out><f b="1" a="1"/><f b="2" a="2"/><f b="3" a="3"/></out>"#);
}

#[test]
fn on_empty_and_on_non_empty_depend_on_siblings() {
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <table>
                <xsl:on-non-empty><thead>H</thead></xsl:on-non-empty>
                <xsl:for-each select="data/row"><tr/></xsl:for-each>
                <xsl:on-empty><none/></xsl:on-empty>
            </table>
        </xsl:template>
    </xsl:stylesheet>"##;
    // Rows present: header in (at its position), on-empty out.
    assert_eq!(transform(xslt, "<data><row/><row/></data>"),
        "<table><thead>H</thead><tr/><tr/></table>");
    // No rows: on-empty in, header out.
    assert_eq!(transform(xslt, "<data/>"),
        "<table><none/></table>");
}

#[test]
fn on_empty_select_attribute_is_sequence_shorthand() {
    // `select=` is the shorthand form of a contained xsl:sequence.
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <out><xsl:copy-of select="/in/missing"/><xsl:on-empty select="23"/></out>
        </xsl:template>
    </xsl:stylesheet>"##;
    assert_eq!(transform(xslt, "<in/>"), "<out>23</out>");
}

#[test]
fn merge_interleaves_two_sources_by_key() {
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <out>
                <xsl:merge>
                    <xsl:merge-source select="data/a/item">
                        <xsl:merge-key select="@k"/>
                    </xsl:merge-source>
                    <xsl:merge-source select="data/b/item">
                        <xsl:merge-key select="@k"/>
                    </xsl:merge-source>
                    <xsl:merge-action>
                        <g key="{current-merge-key()}" n="{count(current-merge-group())}"/>
                    </xsl:merge-action>
                </xsl:merge>
            </out>
        </xsl:template>
    </xsl:stylesheet>"##;
    let src = r#"<data>
        <a><item k="1">a1</item><item k="3">a3</item></a>
        <b><item k="2">b2</item><item k="3">b3</item></b>
    </data>"#;
    let out = transform(xslt, src);
    assert_eq!(out,
        r#"<out><g key="1" n="1"/><g key="2" n="1"/><g key="3" n="2"/></out>"#);
}

#[test]
fn key_match_pattern_references_global_variable() {
    // XSLT 2.0 §16.3 — an xsl:key match/use expression is evaluated
    // in the static context, which includes global variables. The
    // index is built after globals are bound so `$threshold` resolves.
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:variable name="threshold" select="3"/>
        <xsl:key name="big" match="n[. gt $threshold]" use="'hit'"/>
        <xsl:template match="/">
            <out><xsl:value-of select="key('big', 'hit')" separator=","/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    assert_eq!(transform(xslt, "<r><n>1</n><n>5</n><n>2</n><n>9</n></r>"),
        "<out>5,9</out>");
}

#[test]
fn key_with_sequence_constructor_body() {
    // XSLT 2.0 §16.3 — an xsl:key may compute its value with a
    // sequence constructor instead of a use= attribute.
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:key name="bylen" match="w">
            <xsl:sequence select="string-length(.)"/>
        </xsl:key>
        <xsl:template match="/">
            <out><xsl:copy-of select="key('bylen', 3)"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    assert_eq!(transform(xslt, "<r><w>cat</w><w>house</w><w>dog</w></r>"),
        "<out><w>cat</w><w>dog</w></out>");
}

#[test]
fn regex_group_survives_apply_templates_but_not_function() {
    // XSLT 2.0 §15.3 — the captured groups of an enclosing
    // xsl:analyze-string survive into a template invoked from the
    // matching-substring body, but a stylesheet function sees none.
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:f="http://x/" exclude-result-prefixes="f">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <out><xsl:analyze-string select="'aXb'" regex="(a)(X)(b)">
                <xsl:matching-substring>
                    <viatmpl><xsl:call-template name="g"/></viatmpl>
                    <viafn><xsl:value-of select="f:g()"/></viafn>
                </xsl:matching-substring>
            </xsl:analyze-string></out>
        </xsl:template>
        <xsl:template name="g"><xsl:value-of select="regex-group(2)"/></xsl:template>
        <xsl:function name="f:g"><xsl:sequence select="regex-group(2)"/></xsl:function>
    </xsl:stylesheet>"#;
    assert_eq!(transform(xslt, "<r/>"),
        "<out><viatmpl>X</viatmpl><viafn/></out>");
}

#[test]
fn iterate_accumulates_param_across_items() {
    // XSLT 3.0 §8.3 — xsl:iterate threads xsl:param values from one
    // iteration to the next via xsl:next-iteration, and xsl:on-completion
    // runs once the sequence is exhausted with the final values bound.
    let xslt = r#"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <out>
                <xsl:iterate select="*/item">
                    <xsl:param name="total" select="0"/>
                    <xsl:on-completion><total><xsl:value-of select="$total"/></total></xsl:on-completion>
                    <xsl:next-iteration>
                        <xsl:with-param name="total" select="$total + number(.)"/>
                    </xsl:next-iteration>
                </xsl:iterate>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r><item>1</item><item>2</item><item>3</item></r>");
    assert!(out.contains("<total>6</total>"), "got: {out}");
}

#[test]
fn xpath2_decimal_arithmetic_is_exact() {
    // XPath 2.0 §3.1.1 — a numeric literal with `.` but no exponent is
    // xs:decimal, and decimal arithmetic must be exact (not f64).  The
    // canonical "is your decimal real" probe: 0.1 + 0.2 must stringify
    // to "0.3", not "0.30000000000000004".  Same for 1.0 + 0.1 - 0.0
    // + 1.01 - 01.010 - 01.10, which f64 collapses to ≈ 2.22e-16.
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/"><out>
            <a><xsl:value-of select="0.1 + 0.2"/></a>
            <b><xsl:value-of select="1.0 + 0.1 - 0.0 + 1.01 - 01.010 - 01.10"/></b>
            <c><xsl:value-of select="0.1 + 0.2 = 0.3"/></c>
        </out></xsl:template>
    </xsl:stylesheet>"#;
    assert_eq!(transform(xslt, "<r/>"),
        "<out><a>0.3</a><b>0</b><c>true</c></out>");
}

#[test]
fn try_catch_exposes_structured_error_code() {
    // Integer division by zero raises err:FOAR0001 (XPath 2.0 §6.2.4);
    // xsl:catch exposes it via $err:code.
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:err="http://www.w3.org/2005/xqt-errors"
        exclude-result-prefixes="err">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:param name="a" select="0"/>
        <xsl:template match="/">
            <o><xsl:try select="10 div $a">
                <xsl:catch errors="*"><code><xsl:value-of select="$err:code"/></code></xsl:catch>
            </xsl:try></o>
        </xsl:template>
    </xsl:stylesheet>"##;
    assert_eq!(transform(xslt, "<x/>"), "<o><code>err:FOAR0001</code></o>");
}

#[test]
fn try_catch_propagates_fn_error_code() {
    // fn:error()'s first argument supplies the err:code QName.
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:err="http://www.w3.org/2005/xqt-errors"
        xmlns:my="urn:my"
        exclude-result-prefixes="err my">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <o><xsl:try select="error(QName('urn:my','my:OUCH9999'), 'boom')">
                <xsl:catch errors="*"><code><xsl:value-of select="$err:code"/></code></xsl:catch>
            </xsl:try></o>
        </xsl:template>
    </xsl:stylesheet>"##;
    assert_eq!(transform(xslt, "<x/>"), "<o><code>err:OUCH9999</code></o>");
}

#[test]
fn try_catch_exposes_cast_and_regex_codes() {
    // A failed cast is err:FORG0001 (F&O §17.1); an invalid regex is
    // err:FORX0002 (F&O §5.6.3).  Both surface through $err:code.
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:err="http://www.w3.org/2005/xqt-errors"
        exclude-result-prefixes="err">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <o>
                <xsl:try select="'not-a-number' cast as xs:integer">
                    <xsl:catch errors="*"><cast><xsl:value-of select="$err:code"/></cast></xsl:catch>
                </xsl:try>
                <xsl:try select="matches('x', '[')">
                    <xsl:catch errors="*"><regex><xsl:value-of select="$err:code"/></regex></xsl:catch>
                </xsl:try>
            </o>
        </xsl:template>
    </xsl:stylesheet>"##;
    assert_eq!(transform(xslt, "<x/>"),
        "<o><cast>err:FORG0001</cast><regex>err:FORX0002</regex></o>");
}

#[test]
fn try_catch_selects_handler_by_error_code() {
    // The structured code lets a handler match a specific error: the
    // err:FORG0001 catch claims the failed cast; the catch-all does not.
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:err="http://www.w3.org/2005/xqt-errors"
        exclude-result-prefixes="err">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <o><xsl:try select="'nope' cast as xs:integer">
                <xsl:catch errors="err:FORG0001"><cast-error/></xsl:catch>
                <xsl:catch errors="*"><other/></xsl:catch>
            </xsl:try></o>
        </xsl:template>
    </xsl:stylesheet>"##;
    assert_eq!(transform(xslt, "<x/>"), "<o><cast-error/></o>");
}

#[test]
fn fallback_in_try_is_ignored_not_an_error() {
    // XSLT 3.0 §3.6 — xsl:fallback is permitted as a child of any
    // instruction and is inert when the instruction is recognised, so
    // it may sit among the xsl:catch handlers without raising XTSE0010.
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <out><xsl:try>
                <xsl:sequence select="2+2"/>
                <xsl:catch errors="*"/>
                <xsl:fallback><xsl:sequence select="2+3"/></xsl:fallback>
            </xsl:try></out>
        </xsl:template>
    </xsl:stylesheet>"##;
    assert_eq!(transform(xslt, "<x/>"), "<out>4</out>");
}

#[test]
fn cast_out_of_range_year_is_fodt0001() {
    // A lexically-valid xs:dateTime whose year exceeds the engine's
    // representable range is an overflow (err:FODT0001), distinct from a
    // malformed lexical form (err:FORG0001).  A large-but-in-range year
    // (here five digits) casts fine — this engine deliberately supports
    // a wide year range, so it's NOT rejected.
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:xs="http://www.w3.org/2001/XMLSchema"
        xmlns:err="http://www.w3.org/2005/xqt-errors"
        exclude-result-prefixes="xs err">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <o>
                <xsl:try select="'999999999999-01-01T00:00:00' cast as xs:dateTime">
                    <xsl:catch errors="*"><over><xsl:value-of select="$err:code"/></over></xsl:catch>
                </xsl:try>
                <in><xsl:value-of select="'21999-06-15' cast as xs:date"/></in>
            </o>
        </xsl:template>
    </xsl:stylesheet>"##;
    assert_eq!(transform(xslt, "<x/>"),
        "<o><over>err:FODT0001</over><in>21999-06-15</in></o>");
}

#[test]
fn number_start_at_offsets_the_sequence() {
    // XSLT 3.0 start-at: the first number is `start-at` instead of 1,
    // i.e. every entry is offset by start-at - 1.
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <out><xsl:for-each select="r/i"><n><xsl:number value="position()" format="1" start-at="0"/></n></xsl:for-each></out>
        </xsl:template>
    </xsl:stylesheet>"##;
    assert_eq!(transform(xslt, "<r><i/><i/><i/></r>"),
        "<out><n>0</n><n>1</n><n>2</n></out>");
}

#[test]
fn merge_keys_each_source_by_its_own_select() {
    // Each merge-source declares its own merge-key select (here `@k`
    // vs `id`), pointing at differently-named data; the streams must
    // still interleave by the common key VALUE, not by source.
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <out>
                <xsl:merge>
                    <xsl:merge-source select="data/a/x">
                        <xsl:merge-key select="@k" data-type="number"/>
                    </xsl:merge-source>
                    <xsl:merge-source select="data/b/y">
                        <xsl:merge-key select="id" data-type="number"/>
                    </xsl:merge-source>
                    <xsl:merge-action>
                        <g><xsl:value-of select="current-merge-group()/@k"/><xsl:value-of select="current-merge-group()/id"/></g>
                    </xsl:merge-action>
                </xsl:merge>
            </out>
        </xsl:template>
    </xsl:stylesheet>"##;
    let src = r#"<data>
        <a><x k="1"/><x k="3"/></a>
        <b><y><id>2</id></y><y><id>3</id></y></b>
    </data>"#;
    // Keys 1,2,3 interleave across sources; key 3 groups x and y together.
    assert_eq!(transform(xslt, src), "<out><g>1</g><g>2</g><g>33</g></out>");
}

#[test]
fn mode_on_no_match_deep_skip_suppresses_unmatched() {
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:mode on-no-match="deep-skip"/>
        <xsl:template match="/"><out><xsl:apply-templates/></out></xsl:template>
    </xsl:stylesheet>"##;
    // Default would copy "text"; deep-skip suppresses the unmatched subtree.
    let out = transform(xslt, "<a>text</a>");
    assert_eq!(out, "<out/>");
}

#[test]
fn mode_on_no_match_shallow_copy_is_identity() {
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:mode on-no-match="shallow-copy"/>
        <xsl:template match="/"><xsl:apply-templates/></xsl:template>
    </xsl:stylesheet>"##;
    let out = transform(xslt, r#"<a x="1"><b>hi</b></a>"#);
    assert_eq!(out, r#"<a x="1"><b>hi</b></a>"#);
}

#[test]
fn mode_on_no_match_deep_copy_copies_subtree() {
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:mode on-no-match="deep-copy"/>
        <xsl:template match="/"><out><xsl:apply-templates/></out></xsl:template>
    </xsl:stylesheet>"##;
    let out = transform(xslt, r#"<a x="1"><b>hi</b></a>"#);
    assert_eq!(out, r#"<out><a x="1"><b>hi</b></a></out>"#);
}

#[test]
fn json_to_xml_builds_fo_vocabulary() {
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="xml" omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <xsl:copy-of select="json-to-xml('{&quot;a&quot;:1,&quot;b&quot;:[true,null]}')"/>
        </xsl:template>
    </xsl:stylesheet>"##;
    let out = transform(xslt, "<r/>");
    // The elements are in the JSON namespace internally (verified by the
    // xml-to-json round-trip test); this checks the FO vocabulary shape.
    assert!(out.contains(r#"<number key="a">1</number>"#), "got: {out}");
    assert!(out.contains(r#"<array key="b">"#), "got: {out}");
    assert!(out.contains("<boolean>true</boolean>"), "got: {out}");
    assert!(out.contains("<null/>"), "got: {out}");
}

#[test]
fn json_to_xml_then_xml_to_json_round_trips() {
    let xslt = r##"<xsl:stylesheet version="3.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:output method="text"/>
        <xsl:template match="/">
            <xsl:value-of select="xml-to-json(json-to-xml('{&quot;n&quot;:42,&quot;s&quot;:&quot;hi&quot;}'))"/>
        </xsl:template>
    </xsl:stylesheet>"##;
    let out = transform(xslt, "<r/>");
    assert_eq!(out, r#"{"n":42,"s":"hi"}"#);
}

#[test]
fn rss_to_html_style_summary() {
    // A realistic-ish transformation: list of items → HTML <ul>.
    let out = transform(
        &wrap(r#"<xsl:output method="xml" omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <ul>
                    <xsl:for-each select="/feed/item">
                        <li>
                            <xsl:value-of select="title"/>
                        </li>
                    </xsl:for-each>
                </ul>
            </xsl:template>"#),
        r#"<feed>
            <item><title>First</title></item>
            <item><title>Second</title></item>
        </feed>"#,
    );
    assert!(out.contains("<li>"));
    assert!(out.contains("First"));
    assert!(out.contains("Second"));
    // Two <li> ... </li> elements.
    assert_eq!(out.matches("<li>").count(), 2);
}

#[test]
fn html_output_method_emits_void_elements_correctly() {
    let out = transform(
        &wrap(r#"<xsl:output method="html"/>
            <xsl:template match="/">
                <html><head><meta charset="utf-8"/></head>
                <body><br/><br/></body></html>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains("<br>"), "got: {out}");
    assert!(out.contains("<meta charset=\"utf-8\">"), "got: {out}");
    assert!(!out.contains("<br/>"));
    assert!(!out.contains("<meta charset=\"utf-8\"/>"));
}

#[test]
fn text_output_method_strips_markup() {
    let out = transform(
        &wrap(r#"<xsl:output method="text"/>
            <xsl:template match="/">
                <wrap>The answer is <xsl:value-of select="/r/n"/>.</wrap>
            </xsl:template>"#),
        "<r><n>42</n></r>",
    );
    assert!(out.contains("The answer is 42."), "got: {out}");
    assert!(!out.contains("<wrap>"));
}

#[test]
fn xml_decl_emitted_by_default() {
    let out = transform(
        &wrap(r#"<xsl:template match="/"><out/></xsl:template>"#),
        "<r/>",
    );
    assert!(out.starts_with("<?xml"), "got: {out}");
}

#[test]
fn xml_decl_suppressed_when_requested() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/"><out/></xsl:template>"#),
        "<r/>",
    );
    assert!(!out.contains("<?xml"), "got: {out}");
    assert!(out.contains("<out/>"));
}

#[test]
fn doctype_emitted_when_configured() {
    let out = transform(
        &wrap(r##"<xsl:output method="html"
            doctype-public="-//W3C//DTD HTML 4.01//EN"
            doctype-system="http://www.w3.org/TR/html4/strict.dtd"/>
            <xsl:template match="/"><html><body/></html></xsl:template>"##),
        "<r/>",
    );
    assert!(out.contains("<!DOCTYPE html PUBLIC"), "got: {out}");
}

#[test]
fn attribute_value_template_substitutes() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <item id="row-{position()}" class="{name(/r)}-class"/>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains(r#"id="row-1""#), "got: {out}");
    assert!(out.contains(r#"class="r-class""#), "got: {out}");
}

#[test]
fn count_function_in_value_of() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <count><xsl:value-of select="count(/r/i)"/></count>
            </xsl:template>"#),
        "<r><i/><i/><i/><i/></r>",
    );
    assert!(out.contains("<count>4</count>"), "got: {out}");
}

#[test]
fn current_returns_xslt_context_not_xpath_inner_context() {
    // Inside a predicate, `.` refers to the inner context node
    // being filtered, but `current()` keeps pointing at the
    // outer XSLT context.  This stylesheet uses that to filter
    // <i>s by their @x matching the outer <r>'s @match.
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/r">
                <out>
                    <xsl:for-each select="i[@x=current()/@match]">
                        <picked><xsl:value-of select="."/></picked>
                    </xsl:for-each>
                </out>
            </xsl:template>"#),
        r#"<r match="A"><i x="A">one</i><i x="B">two</i><i x="A">three</i></r>"#,
    );
    assert!(out.contains("<picked>one</picked>"), "got: {out}");
    assert!(out.contains("<picked>three</picked>"), "got: {out}");
    assert!(!out.contains("<picked>two</picked>"), "got: {out}");
}

#[test]
fn key_function_indexes_by_value() {
    let out = transform(
        &wrap(r##"<xsl:output omit-xml-declaration="yes"/>
            <xsl:key name="by-cat" match="item" use="@cat"/>
            <xsl:template match="/">
                <out>
                    <xsl:for-each select="key('by-cat', 'fruit')">
                        <hit><xsl:value-of select="."/></hit>
                    </xsl:for-each>
                </out>
            </xsl:template>"##),
        r#"<r>
            <item cat="fruit">apple</item>
            <item cat="veggie">kale</item>
            <item cat="fruit">pear</item>
        </r>"#,
    );
    assert!(out.contains("<hit>apple</hit>"), "got: {out}");
    assert!(out.contains("<hit>pear</hit>"),  "got: {out}");
    assert!(!out.contains("<hit>kale</hit>"), "got: {out}");
}

#[test]
fn generate_id_stable_within_a_run() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out>
                    <a><xsl:value-of select="generate-id(/r)"/></a>
                    <b><xsl:value-of select="generate-id(/r)"/></b>
                </out>
            </xsl:template>"#),
        "<r/>",
    );
    // Two generate-id calls on the same node should yield equal strings.
    let a = out.split("<a>").nth(1).unwrap().split("</a>").next().unwrap();
    let b = out.split("<b>").nth(1).unwrap().split("</b>").next().unwrap();
    assert_eq!(a, b);
}

#[test]
fn system_property_returns_xsl_version() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <ver><xsl:value-of select="system-property('xsl:version')"/></ver>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains("<ver>1.0</ver>"), "got: {out}");
}

#[test]
fn format_number_basic() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <o><xsl:value-of select="format-number(1234567.89, '#,##0.00')"/></o>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains("<o>1,234,567.89</o>"), "got: {out}");
}

#[test]
fn format_number_percent() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <o><xsl:value-of select="format-number(0.42, '0%')"/></o>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains("<o>42%</o>"), "got: {out}");
}

#[test]
fn xsl_number_default_counts_siblings() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out>
                    <xsl:for-each select="/r/i">
                        <li><xsl:number/>: <xsl:value-of select="."/></li>
                    </xsl:for-each>
                </out>
            </xsl:template>"#),
        "<r><i>a</i><i>b</i><i>c</i></r>",
    );
    assert!(out.contains("<li>1: a</li>"), "got: {out}");
    assert!(out.contains("<li>2: b</li>"), "got: {out}");
    assert!(out.contains("<li>3: c</li>"), "got: {out}");
}

#[test]
fn xsl_number_with_alpha_format() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out>
                    <xsl:for-each select="/r/i">
                        <li><xsl:number format="A"/>: <xsl:value-of select="."/></li>
                    </xsl:for-each>
                </out>
            </xsl:template>"#),
        "<r><i>a</i><i>b</i></r>",
    );
    assert!(out.contains("<li>A: a</li>"), "got: {out}");
    assert!(out.contains("<li>B: b</li>"), "got: {out}");
}

#[test]
fn strip_space_removes_whitespace_text() {
    // Source has lots of whitespace between elements; default
    // built-in template would copy each whitespace text node.
    // With xsl:strip-space="*" applied to the source, only the
    // significant content remains.
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:strip-space elements="*"/>
            <xsl:template match="/"><out><xsl:apply-templates/></out></xsl:template>
            <xsl:template match="i">[<xsl:value-of select="."/>]</xsl:template>"#),
        r#"<r>
            <i>one</i>
            <i>two</i>
        </r>"#,
    );
    // Should be just <out>[one][two]</out> with no whitespace
    // between the bracketed values — they fired exactly twice.
    assert!(out.contains("<out>[one][two]</out>"),
        "expected stripped output, got: {out}");
}

#[test]
fn for_each_with_sort_sorts_by_select() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out>
                    <xsl:for-each select="/r/i">
                        <xsl:sort select="."/>
                        <li><xsl:value-of select="."/></li>
                    </xsl:for-each>
                </out>
            </xsl:template>"#),
        "<r><i>banana</i><i>apple</i><i>cherry</i></r>",
    );
    let pos_apple = out.find("<li>apple</li>").unwrap();
    let pos_banana = out.find("<li>banana</li>").unwrap();
    let pos_cherry = out.find("<li>cherry</li>").unwrap();
    assert!(pos_apple < pos_banana && pos_banana < pos_cherry, "got: {out}");
}

#[test]
fn for_each_with_numeric_sort() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out>
                    <xsl:for-each select="/r/i">
                        <xsl:sort select="." data-type="number"/>
                        <li><xsl:value-of select="."/></li>
                    </xsl:for-each>
                </out>
            </xsl:template>"#),
        "<r><i>10</i><i>2</i><i>1</i></r>",
    );
    let pos_1  = out.find("<li>1</li>").unwrap();
    let pos_2  = out.find("<li>2</li>").unwrap();
    let pos_10 = out.find("<li>10</li>").unwrap();
    assert!(pos_1 < pos_2 && pos_2 < pos_10, "got: {out}");
}

#[test]
fn exslt_math_in_value_of_through_full_pipeline() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out>
                    <max><xsl:value-of select="math:max(/r/i)"/></max>
                    <min><xsl:value-of select="math:min(/r/i)"/></min>
                </out>
            </xsl:template>"#),
        "<r><i>7</i><i>3</i><i>9</i><i>1</i></r>",
    );
    assert!(out.contains("<max>9</max>"), "got: {out}");
    assert!(out.contains("<min>1</min>"), "got: {out}");
}

#[test]
fn use_attribute_sets_applies_on_xsl_copy() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:attribute-set name="common">
                <xsl:attribute name="id">x</xsl:attribute>
                <xsl:attribute name="lang">en</xsl:attribute>
            </xsl:attribute-set>
            <xsl:template match="/r">
                <xsl:copy use-attribute-sets="common"/>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains(r#"id="x""#),   "got: {out}");
    assert!(out.contains(r#"lang="en""#), "got: {out}");
}

#[test]
fn use_attribute_sets_applies_on_xsl_element() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:attribute-set name="common">
                <xsl:attribute name="id">x</xsl:attribute>
            </xsl:attribute-set>
            <xsl:template match="/">
                <xsl:element name="thing" use-attribute-sets="common"/>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains(r#"<thing id="x""#), "got: {out}");
}

#[test]
fn str_tokenize_for_each_iterates_every_token() {
    // `xsl:for-each` over a `str:tokenize` result produces one body
    // iteration per token, with `.` resolving to that token's text
    // content.
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out>
                    <xsl:for-each select="str:tokenize('alpha beta  gamma')">
                        <t><xsl:value-of select="."/></t>
                    </xsl:for-each>
                </out>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains("<t>alpha</t>"), "got: {out}");
    assert!(out.contains("<t>beta</t>"),  "got: {out}");
    assert!(out.contains("<t>gamma</t>"), "got: {out}");
    assert_eq!(out.matches("<t>").count(), 3, "got: {out}");
}

#[test]
fn str_split_for_each_uses_literal_separator() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out>
                    <xsl:for-each select="str:split('one--two--three', '--')">
                        <t><xsl:value-of select="."/></t>
                    </xsl:for-each>
                </out>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains("<t>one</t><t>two</t><t>three</t>"), "got: {out}");
}

#[test]
fn str_tokenize_value_of_returns_first_token() {
    // `str:tokenize` returns a node-set of tokens; `xsl:value-of`
    // over a node-set emits the first node's string-value
    // (XPath 1.0 §7.6.1).
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out><xsl:value-of select="str:tokenize('alpha beta gamma')"/></out>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains("<out>alpha</out>"), "got: {out}");
}

#[test]
fn str_tokenize_honours_custom_delim_in_value_of() {
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out><xsl:value-of select="str:tokenize('a,b;c', ',;')"/></out>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains("<out>a</out>"), "got: {out}");
}

#[test]
fn str_split_returns_nodeset_first_via_value_of() {
    // `str:split` uses the second arg as a literal substring
    // (default " ").
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out><xsl:value-of select="str:split('one--two--three', '--')"/></out>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains("<out>one</out>"), "got: {out}");
}

#[test]
fn unparsed_entity_uri_returns_system_id_from_dtd() {
    // Doc DTD declares two unparsed external entities (NDATA); the
    // XSLT looks each up by name and the function returns the
    // entity's SYSTEM identifier.  Stylesheets that reference an
    // undeclared name still produce empty output (per spec).
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out>
                    <a><xsl:value-of select="unparsed-entity-uri('logo')"/></a>
                    <b><xsl:value-of select="unparsed-entity-uri('photo')"/></b>
                    <c><xsl:value-of select="unparsed-entity-uri('missing')"/></c>
                </out>
            </xsl:template>"#),
        r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!NOTATION png SYSTEM "image/png">
  <!ENTITY logo  SYSTEM "logo.png"  NDATA png>
  <!ENTITY photo SYSTEM "photo.jpg" NDATA png>
]>
<r/>"#,
    );
    assert!(out.contains("<a>logo.png</a>"),   "got: {out}");
    assert!(out.contains("<b>photo.jpg</b>"),  "got: {out}");
    assert!(out.contains("<c/>"),              "got: {out}");
}

#[test]
fn xsl_number_level_any_counts_preceding_matching_nodes() {
    // Three <item> elements scattered across the tree; level="any"
    // produces a flat document-order index regardless of nesting.
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out><xsl:apply-templates select="//item"/></out>
            </xsl:template>
            <xsl:template match="item">
                <n><xsl:number level="any" count="item"/></n>
            </xsl:template>"#),
        "<r><g><item/></g><item/><g><item/></g></r>",
    );
    assert!(out.contains("<n>1</n>"), "got: {out}");
    assert!(out.contains("<n>2</n>"), "got: {out}");
    assert!(out.contains("<n>3</n>"), "got: {out}");
}

#[test]
fn xsl_number_level_multiple_produces_hierarchical_index() {
    // Three levels of <s> (section) nesting; level="multiple" with
    // count="s" and format="1.1.1" yields the hierarchical number.
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out><xsl:apply-templates select="//leaf"/></out>
            </xsl:template>
            <xsl:template match="leaf">
                <n><xsl:number level="multiple" count="s" format="1.1.1"/></n>
            </xsl:template>"#),
        // Tree:
        //   r
        //    s (1)
        //      s (1.1)
        //        s (1.1.1)
        //          leaf      → "1.1.1"
        //      s (1.2)
        //        leaf        → "1.2"
        //    s (2)
        //      leaf          → "2"
        "<r>\
          <s><s><s><leaf/></s></s><s><leaf/></s></s>\
          <s><leaf/></s>\
        </r>",
    );
    assert!(out.contains("<n>1.1.1</n>"), "innermost section number: {out}");
    assert!(out.contains("<n>1.2</n>"),   "two-level section number: {out}");
    assert!(out.contains("<n>2</n>"),     "top-level section number: {out}");
}

#[test]
fn xsl_number_from_scopes_the_walk() {
    // level="any" with from="chapter" resets the count at each
    // chapter boundary.
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out><xsl:apply-templates select="//figure"/></out>
            </xsl:template>
            <xsl:template match="figure">
                <n><xsl:number level="any" count="figure" from="chapter"/></n>
            </xsl:template>"#),
        "<book>\
          <chapter><figure/><figure/></chapter>\
          <chapter><figure/></chapter>\
        </book>",
    );
    // Per-chapter numbering: 1, 2, then 1 again in the next chapter.
    let mut nums: Vec<&str> = out.matches("<n>").map(|_| "").collect();
    let _ = nums.pop();
    // Extract the integers between <n> and </n>.
    let parts: Vec<&str> = out.split("<n>").skip(1)
        .map(|s| s.split("</n>").next().unwrap()).collect();
    assert_eq!(parts, vec!["1", "2", "1"], "got: {out}");
}

#[test]
fn apply_imports_invokes_lower_precedence_template() {
    // Outer template matches `r` and wraps `<xsl:apply-imports/>`;
    // the imported stylesheet has its own `r` template emitting
    // `<imported/>`.  apply-imports must re-run selection capped
    // at precedence < outer, so the imported template fires.
    use sup_xml_xslt::loader::InMemoryLoader;
    let inner = r##"<xsl:stylesheet version="1.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="r"><imported/></xsl:template>
    </xsl:stylesheet>"##;
    let outer = r##"<xsl:stylesheet version="1.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:import href="inner.xsl"/>
        <xsl:output omit-xml-declaration="yes"/>
        <xsl:template match="r"><outer><xsl:apply-imports/></outer></xsl:template>
    </xsl:stylesheet>"##;
    let loader = InMemoryLoader::new().with("inner.xsl", inner);
    let xslt = Stylesheet::compile_str_with_loader(outer, &loader, None).unwrap();
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    let out = xslt.apply(&doc).unwrap().to_string().unwrap();
    assert!(out.contains("<outer>"), "outer wrapper missing: {out}");
    assert!(out.contains("<imported/>"), "imported template not invoked: {out}");
}

#[test]
fn apply_imports_with_no_lower_precedence_falls_through_to_builtins() {
    // When no imported template matches, XSLT 1.0 §5.6 says the
    // built-in template rules apply.  Here the only template
    // matches `r`; apply-imports has no lower-precedence template
    // to invoke, so the built-in element rule (recurse into
    // children) fires.  The `<v>x</v>` child's string-value
    // surfaces through the default text rule.
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/r">
                <out><xsl:apply-imports/></out>
            </xsl:template>"#),
        "<r><v>x</v></r>",
    );
    assert!(out.contains("<out>x</out>"),
        "builtin recurse-into-children rule must fire after empty apply-imports: {out}");
}

#[test]
fn key_resolves_prefixed_name_against_static_namespaces() {
    // `xsl:key name="my:idx"` declares a key under expanded name
    // `{urn:my}idx`.  The XSLT 1.0 spec says `key('my:idx', x)`
    // must look up using the same expanded form — i.e. the prefix
    // is resolved against the call's static namespace context.
    // We build that context from the stylesheet root, so the
    // `my` prefix must be declared there.
    let xslt = r##"<xsl:stylesheet version="1.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:my="urn:my">
        <xsl:output omit-xml-declaration="yes"/>
        <xsl:key name="my:idx" match="item" use="@id"/>
        <xsl:template match="/">
            <out>
                <xsl:value-of select="key('my:idx', 'a')/text()"/>
            </out>
        </xsl:template>
    </xsl:stylesheet>"##;
    let out = transform(xslt, r#"<r><item id="a">A</item><item id="b">B</item></r>"#);
    // Per XSLT 1.0 §7.1.1 the literal `<out>` inherits the stylesheet's
    // in-scope namespaces, so the `my` prefix declaration propagates
    // to the result (regardless of whether it's used).  Match around
    // the optional namespace declaration.
    assert!(out.contains(">A</out>"), "got: {out}");
}

#[test]
fn format_number_uses_named_decimal_format() {
    // European-style: `,` is the decimal separator, `.` the
    // grouping separator.  Without the named lookup wired,
    // format-number would use the default and emit `1,234.50`.
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:decimal-format name="euro"
                decimal-separator="," grouping-separator="."/>
            <xsl:template match="/">
                <out><xsl:value-of select="format-number(1234.5, '#.##0,00', 'euro')"/></out>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains("<out>1.234,50</out>"), "got: {out}");
}

#[test]
fn format_number_unknown_decimal_format_errors() {
    // libxslt errors when format-number references an undeclared
    // named format; we match that behaviour.
    let xslt = Stylesheet::compile_str(&wrap(r#"<xsl:output omit-xml-declaration="yes"/>
        <xsl:template match="/">
            <xsl:value-of select="format-number(1, '0', 'missing')"/>
        </xsl:template>"#)).unwrap();
    let doc = parse_str("<r/>", &ParseOptions::default()).unwrap();
    assert!(xslt.apply(&doc).is_err(),
        "format-number with undeclared decimal-format name must error");
}

#[test]
fn use_attribute_sets_chains_through_inner_set() {
    // `outer` pulls in `inner` first; both add attributes. Outer's
    // attribute is written *after* inner's (XSLT order), so outer
    // wins on the shared `class` attribute.
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:attribute-set name="inner">
                <xsl:attribute name="class">low</xsl:attribute>
                <xsl:attribute name="role">leaf</xsl:attribute>
            </xsl:attribute-set>
            <xsl:attribute-set name="outer" use-attribute-sets="inner">
                <xsl:attribute name="class">high</xsl:attribute>
            </xsl:attribute-set>
            <xsl:template match="/r">
                <xsl:copy use-attribute-sets="outer"/>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains(r#"role="leaf""#),   "inner attribute should land: {out}");
    assert!(out.contains(r#"class="high""#),  "outer must override inner: {out}");
    assert!(!out.contains(r#"class="low""#),  "inner class shadowed: {out}");
}

#[test]
fn dyn_evaluate_runs_string_as_xpath() {
    // Stylesheet uses `dyn:evaluate` to compute a node-set from a
    // string-typed expression — the canonical use case (a "select"
    // attribute that's data-driven rather than hard-coded).
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/r">
                <out><xsl:value-of select="dyn:evaluate('count(i)')"/></out>
            </xsl:template>"#),
        "<r><i/><i/><i/></r>",
    );
    assert!(out.contains("<out>3</out>"), "got: {out}");
}

#[test]
fn dyn_evaluate_threads_context_node_so_relative_paths_resolve() {
    // Confirms `dyn:evaluate` evaluates against the XSLT current
    // context node, not the document root — a relative path like
    // `@id` returns the context element's id attribute.
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/r">
                <xsl:for-each select="item">
                    <picked><xsl:value-of select="dyn:evaluate('@id')"/></picked>
                </xsl:for-each>
            </xsl:template>"#),
        r#"<r><item id="x"/><item id="y"/><item id="z"/></r>"#,
    );
    assert!(out.contains("<picked>x</picked>"), "got: {out}");
    assert!(out.contains("<picked>z</picked>"), "got: {out}");
}

#[test]
fn exsl_node_set_wraps_string_into_traversable_nodeset() {
    // exsl:node-set on a scalar string yields a one-element node-set
    // whose string-value is the original string.  The for-each
    // iterates once and emits one <t> with that text.
    let out = transform(
        &wrap(r#"<xsl:output omit-xml-declaration="yes"/>
            <xsl:template match="/">
                <out>
                    <xsl:for-each select="exsl:node-set('hello')">
                        <t><xsl:value-of select="."/></t>
                    </xsl:for-each>
                </out>
            </xsl:template>"#),
        "<r/>",
    );
    assert!(out.contains("<t>hello</t>"), "got: {out}");
}

// ── XSLT 2.0 incremental support (gated on version="2.0") ────────

/// `<xsl:function>` with a single `<xsl:sequence select="…"/>` body —
/// the pure-XPath form the initial 2.0 slice supports.  Called from
/// an XPath expression inside `<xsl:value-of>`.
#[test]
fn xslt2_user_function_called_from_value_of() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:my="urn:my">
        <xsl:function name="my:double">
            <xsl:param name="x"/>
            <xsl:sequence select="$x * 2"/>
        </xsl:function>
        <xsl:template match="/">
            <out><xsl:value-of select="my:double(21)"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">42<"), "got: {out}");
}

/// `xsl:call-template` inside an `xsl:function` body — the named
/// template's value (here a doubled number) becomes the function's
/// return.  The with-param is evaluated in the caller's context and
/// bound to the template's declared param.
#[test]
fn xslt2_call_template_in_function_body() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:my="urn:my">
        <xsl:function name="my:viaTemplate">
            <xsl:param name="x"/>
            <xsl:call-template name="dbl">
                <xsl:with-param name="n" select="$x"/>
            </xsl:call-template>
        </xsl:function>
        <xsl:template name="dbl">
            <xsl:param name="n"/>
            <xsl:sequence select="$n * 2"/>
        </xsl:template>
        <xsl:template match="/">
            <out><xsl:value-of select="my:viaTemplate(21)"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">42<"), "got: {out}");
}

/// An unsupplied, non-required template param falls back to its
/// `select=` default when called from a function body.
#[test]
fn xslt2_call_template_in_function_uses_param_default() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:my="urn:my">
        <xsl:function name="my:greet">
            <xsl:call-template name="greet"/>
        </xsl:function>
        <xsl:template name="greet">
            <xsl:param name="who" select="'world'"/>
            <xsl:sequence select="concat('hi ', $who)"/>
        </xsl:template>
        <xsl:template match="/">
            <out><xsl:value-of select="my:greet()"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">hi world<"), "got: {out}");
}

/// A template invoked from a function body may construct a result
/// tree; the constructed nodes flow back as the function's value and
/// `xsl:copy-of` reproduces them.
#[test]
fn xslt2_call_template_in_function_returns_nodes() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:my="urn:my">
        <xsl:function name="my:wrap">
            <xsl:param name="x"/>
            <xsl:call-template name="wrap">
                <xsl:with-param name="v" select="$x"/>
            </xsl:call-template>
        </xsl:function>
        <xsl:template name="wrap">
            <xsl:param name="v"/>
            <w><xsl:value-of select="$v"/></w>
        </xsl:template>
        <xsl:template match="/">
            <out><xsl:copy-of select="my:wrap('hi')"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("<w>hi</w>"), "got: {out}");
}

/// Mutual recursion across the function/template boundary: a function
/// calls a template that calls the function again.  Exercises both the
/// caller-context with-param evaluation (`my:fact($n - 1)`) and the
/// depth guard for a legitimately deep-but-finite recursion.
#[test]
fn xslt2_function_template_mutual_recursion() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:my="urn:my">
        <xsl:function name="my:fact">
            <xsl:param name="n"/>
            <xsl:choose>
                <xsl:when test="$n &lt;= 1"><xsl:sequence select="1"/></xsl:when>
                <xsl:otherwise>
                    <xsl:call-template name="mul">
                        <xsl:with-param name="a" select="$n"/>
                        <xsl:with-param name="b" select="my:fact($n - 1)"/>
                    </xsl:call-template>
                </xsl:otherwise>
            </xsl:choose>
        </xsl:function>
        <xsl:template name="mul">
            <xsl:param name="a"/>
            <xsl:param name="b"/>
            <xsl:sequence select="$a * $b"/>
        </xsl:template>
        <xsl:template match="/">
            <out><xsl:value-of select="my:fact(5)"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">120<"), "got: {out}");
}

/// XPath 2.0 `if (cond) then a else b` works inside an XSLT 2.0
/// stylesheet's select expression.
#[test]
fn xslt2_xpath_if_then_else_in_select() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="if (1=1) then 'yes' else 'no'"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("<out>yes</out>"), "got: {out}");
}

/// XPath 2.0 `for $v in seq return body` — the for-expression yields
/// one body value per input item.  We check the *count* of the result
/// rather than its serialised form so the test doesn't depend on
/// XSLT 2.0 `xsl:value-of` separator semantics (which the 1.0
/// value-of we still ship doesn't implement).
#[test]
fn xslt2_xpath_for_return_over_nodeset() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/r">
            <out><xsl:value-of select="count(for $i in item return concat($i, '!'))"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r><item>a</item><item>b</item></r>");
    assert!(out.contains("<out>2</out>"),
        "expected `for` to produce two items, got: {out}");
}

/// XPath 2.0 `matches(input, pattern)` — basic regex truthiness check.
#[test]
fn xslt2_xpath_matches_regex() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="matches('hello123', '\d+')"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("true"), "got: {out}");
}

/// XPath 2.0 `replace(input, pattern, replacement)` — regex substitute.
#[test]
fn xslt2_xpath_replace_regex() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="replace('abc123def', '\d+', 'X')"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("abcXdef"), "got: {out}");
}

/// XSLT 2.0 §3.5 backwards-compatible behaviour: an XSLT 2.0
/// processor accepts XPath 2.0 grammar even when the stylesheet
/// declared `version="1.0"`.  The runtime applies 1.0 semantics
/// for things like the value-of "first item wins" rule and the
/// untyped-to-number coercions, but the surface XPath grammar
/// is still the 2.0 grammar — `if (..) then .. else ..`,
/// `for $x in ..`, `1 to 5`, sequence parens, `eq` / `ne`, etc.
/// all compile.  W3C XSLT 2.0 suite tests (misc/backwards-*) rely
/// on this.
#[test]
fn xslt2_processor_accepts_xpath_2_0_in_1_0_stylesheet() {
    let xslt = r#"<xsl:stylesheet version="1.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="if (1=1) then 'yes' else 'no'"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let res = Stylesheet::compile_str(xslt);
    assert!(res.is_ok(),
        "XSLT 2.0 processor must accept XPath 2.0 grammar in a 1.0 stylesheet \
         per §3.5 backwards-compat behaviour; got {res:?}");
}

/// XSLT 2.0 §3.5 backwards-compat behaviour: an XSLT 2.0
/// processor running a `version="1.0"` stylesheet still
/// recognises and compiles 2.0 instructions like xsl:function.
/// The function is registered and callable; semantics that differ
/// between 1.0 and 2.0 (value-of "first item only", untyped→number
/// coercion, etc.) revert to 1.0 inside the call site, but the
/// declaration itself works.  Misc/backwards-* tests rely on this.
#[test]
fn xslt2_processor_compiles_xsl_function_in_1_0_stylesheet() {
    let xslt = r#"<xsl:stylesheet version="1.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:my="urn:my">
        <xsl:function name="my:foo">
            <xsl:param name="x"/>
            <xsl:sequence select="$x"/>
        </xsl:function>
        <xsl:template match="/">
            <out><xsl:value-of select="my:foo(1)"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let style = Stylesheet::compile_str(xslt).expect("compile must succeed");
    let doc = parse_str("<r/>", &ParseOptions::default()).expect("source");
    let rt = style.apply(&doc).expect("apply must succeed");
    let serialised = rt.to_string().expect("serialise");
    assert!(serialised.contains(">1</out>"),
        "xsl:function must be registered and callable in BC mode; got {serialised}");
}

/// XPath 2.0 `1 to 10` range materialises a 10-item sequence; `count`
/// of the for-return over the range confirms iteration.
#[test]
fn xslt2_xpath_range_to_drives_for_return() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="count(for $i in 1 to 10 return $i * $i)"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">10<"), "got: {out}");
}

/// XPath 2.0 sequence literal `(a, b, c)` — count confirms 3 items.
#[test]
fn xslt2_xpath_sequence_literal() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="count(('a', 'b', 'c'))"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">3<"), "got: {out}");
}

/// XPath 2.0 `some $v in seq satisfies test` — existential predicate.
#[test]
fn xslt2_xpath_some_quantified() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/r">
            <out><xsl:value-of select="some $i in item satisfies $i = 'b'"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r><item>a</item><item>b</item><item>c</item></r>");
    assert!(out.contains(">true<"), "got: {out}");
}

/// XPath 2.0 `every $v in seq satisfies test` — universal predicate
/// that fails when the sequence contains a non-matching item.
#[test]
fn xslt2_xpath_every_quantified() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/r">
            <out><xsl:value-of select="every $i in item satisfies $i = 'a'"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r><item>a</item><item>b</item></r>");
    assert!(out.contains(">false<"), "got: {out}");
}

/// XPath 2.0 `string-join` joins atomic items with a separator.
#[test]
fn xslt2_xpath_string_join() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/r">
            <out><xsl:value-of select="string-join(item, '-')"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r><item>a</item><item>b</item><item>c</item></r>");
    assert!(out.contains("a-b-c"), "got: {out}");
}

/// XPath 2.0 `upper-case` / `lower-case` smoke test.
#[test]
fn xslt2_xpath_upper_lower_case() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="concat(upper-case('abc'), '|', lower-case('XYZ'))"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("ABC|xyz"), "got: {out}");
}

/// XPath 2.0 `distinct-values` collapses duplicate string-values.
#[test]
fn xslt2_xpath_distinct_values() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/r">
            <out><xsl:value-of select="count(distinct-values(item))"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r><item>a</item><item>b</item><item>a</item><item>b</item></r>");
    assert!(out.contains(">2<"), "got: {out}");
}

/// XPath 2.0 `abs`, `min`, `max`, `avg` over a numeric sequence.
#[test]
fn xslt2_xpath_numeric_aggregates() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/r">
            <out>
                <xsl:value-of select="abs(-7)"/>|
                <xsl:value-of select="min(item)"/>|
                <xsl:value-of select="max(item)"/>|
                <xsl:value-of select="avg(item)"/>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r><item>3</item><item>5</item><item>10</item></r>");
    assert!(out.contains('7') && out.contains('3') && out.contains("10") && out.contains('6'),
        "got: {out}");
}

/// XPath 2.0 value comparison `eq` is a strict-singleton compare;
/// behaves like `=` on atomic operands which is what most uses are.
#[test]
fn xslt2_xpath_value_compare_eq() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="'foo' eq 'foo'"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">true<"), "got: {out}");
}

/// XPath 2.0 `idiv` integer division — `7 idiv 2 = 3`.
#[test]
fn xslt2_xpath_idiv_integer_quotient() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/"><out><xsl:value-of select="7 idiv 2"/></out></xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">3<"), "got: {out}");
}

/// XPath 2.0 `intersect` — nodes common to both operands.
#[test]
fn xslt2_xpath_intersect_nodesets() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/r"><out><xsl:value-of select="count(item intersect item[1])"/></out></xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r><item>a</item><item>b</item></r>");
    assert!(out.contains(">1<"), "got: {out}");
}

/// XPath 2.0 `except` — nodes in lhs not in rhs.
#[test]
fn xslt2_xpath_except_nodesets() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/r"><out><xsl:value-of select="count(item except item[1])"/></out></xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r><item>a</item><item>b</item><item>c</item></r>");
    assert!(out.contains(">2<"), "got: {out}");
}

/// XPath 2.0 `instance of` — type membership test.
#[test]
fn xslt2_xpath_instance_of_atomic() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:xs="http://www.w3.org/2001/XMLSchema">
        <xsl:template match="/"><out><xsl:value-of select="'abc' instance of xs:string"/></out></xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">true<"), "got: {out}");
}

/// XPath 2.0 `cast as` — convert string to integer.
#[test]
fn xslt2_xpath_cast_as_integer() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:xs="http://www.w3.org/2001/XMLSchema">
        <xsl:template match="/"><out><xsl:value-of select="('42' cast as xs:integer) + 1"/></out></xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">43<"), "got: {out}");
}

/// XSLT 2.0 `xsl:value-of` with `separator=` joins sequence items.
#[test]
fn xslt2_value_of_separator_joins_sequence() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/r">
            <out><xsl:value-of select="item" separator=", "/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r><item>a</item><item>b</item><item>c</item></r>");
    assert!(out.contains("a, b, c"), "got: {out}");
}

/// XSLT 2.0 `xsl:value-of` default separator (single space) when no
/// explicit `separator=` is given.
#[test]
fn xslt2_value_of_default_separator_is_space() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/r">
            <out><xsl:value-of select="item"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r><item>a</item><item>b</item></r>");
    assert!(out.contains("a b"), "got: {out}");
}

/// XPath 2.0 `compare` returns -1 / 0 / 1.
#[test]
fn xslt2_xpath_compare_signed() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="compare('a','b')"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">-1<"), "got: {out}");
}

/// XPath 2.0 `string-to-codepoints` / `codepoints-to-string` round-trip.
#[test]
fn xslt2_xpath_codepoint_round_trip() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="codepoints-to-string(string-to-codepoints('abc'))"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">abc<"), "got: {out}");
}

/// XPath 2.0 `encode-for-uri` percent-encodes reserved chars.
#[test]
fn xslt2_xpath_encode_for_uri() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="encode-for-uri('a b/c')"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("a%20b%2Fc"), "got: {out}");
}

/// XSLT 2.0 `xsl:for-each-group group-by` — partitions the input by
/// the key and exposes per-group access via `current-grouping-key()`.
#[test]
fn xslt2_for_each_group_by() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/items">
            <out>
                <xsl:for-each-group select="item" group-by="@cat">
                    <group cat="{current-grouping-key()}">
                        <xsl:value-of select="count(current-group())"/>
                    </group>
                </xsl:for-each-group>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt,
        r#"<items>
            <item cat="fruit">apple</item>
            <item cat="fruit">pear</item>
            <item cat="veggie">kale</item>
            <item cat="fruit">peach</item>
            <item cat="veggie">corn</item>
        </items>"#);
    assert!(out.contains(r#"<group cat="fruit">3</group>"#), "got: {out}");
    assert!(out.contains(r#"<group cat="veggie">2</group>"#), "got: {out}");
}

/// XSLT 2.0 §5.7.1 — a variable typed `as="attribute()*"` whose body
/// is a run of `xsl:attribute` instructions binds to parentless
/// attribute nodes; `xsl:copy-of` then attaches them to a constructed
/// element (later same-named attributes win).
#[test]
fn xslt2_parentless_attributes_in_variable_then_copy_of() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <xsl:variable name="q" as="attribute()*">
                <xsl:attribute name="a">1</xsl:attribute>
                <xsl:attribute name="b">2</xsl:attribute>
                <xsl:attribute name="a">3</xsl:attribute>
            </xsl:variable>
            <zzz><xsl:copy-of select="$q"/></zzz>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(r#"<zzz a="3" b="2"/>"#), "got: {out}");
}

/// XSLT 2.0 §14.3 — `xsl:sort` inside `xsl:for-each-group` orders the
/// groups, and its key may reference `current-grouping-key()` /
/// `current-group()`.  The keys must be evaluated with each group's
/// accessors in scope, not the (empty) outer grouping context.
#[test]
fn xslt2_for_each_group_sort_by_grouping_key() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/items">
            <out>
                <xsl:for-each-group select="item" group-by="@cat">
                    <xsl:sort select="current-grouping-key()"/>
                    <g><xsl:value-of select="current-grouping-key()"/></g>
                </xsl:for-each-group>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    // Source order veggie, fruit, dairy → sorted groups dairy, fruit, veggie.
    let out = transform(xslt,
        r#"<items>
            <item cat="veggie">kale</item>
            <item cat="fruit">pear</item>
            <item cat="dairy">milk</item>
            <item cat="fruit">peach</item>
        </items>"#);
    let pos = |needle: &str| out.find(needle);
    assert!(pos("<g>dairy</g>") < pos("<g>fruit</g>")
        && pos("<g>fruit</g>") < pos("<g>veggie</g>"),
        "groups not sorted by grouping key: {out}");
}

/// XSLT 2.0 `xsl:for-each-group group-adjacent` — splits consecutive
/// runs of equal-keyed items into separate groups.
#[test]
fn xslt2_for_each_group_adjacent() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/items">
            <out>
                <xsl:for-each-group select="item" group-adjacent="@cat">
                    <run cat="{current-grouping-key()}" n="{count(current-group())}"/>
                </xsl:for-each-group>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt,
        r#"<items>
            <item cat="fruit">apple</item>
            <item cat="fruit">pear</item>
            <item cat="veggie">kale</item>
            <item cat="fruit">peach</item>
        </items>"#);
    assert_eq!(out.matches("<run").count(), 3,
        "expected three adjacency runs, got: {out}");
}

/// XSLT 2.0 `xsl:analyze-string` — partitions input by regex matches,
/// `regex-group(n)` exposes captures inside `<xsl:matching-substring>`.
#[test]
fn xslt2_analyze_string_with_groups() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out>
                <xsl:analyze-string select="'abc-123-xy-456'" regex="([a-z]+)-(\d+)">
                    <xsl:matching-substring>
                        <hit letters="{regex-group(1)}" digits="{regex-group(2)}"/>
                    </xsl:matching-substring>
                    <xsl:non-matching-substring>
                        <gap><xsl:value-of select="."/></gap>
                    </xsl:non-matching-substring>
                </xsl:analyze-string>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(r#"<hit letters="abc" digits="123""#), "got: {out}");
    assert!(out.contains(r#"<hit letters="xy" digits="456""#),  "got: {out}");
    assert!(out.contains("<gap>-</gap>"), "got: {out}");
}

/// XPath 2.0 `format-date` — picture-string formatting.
#[test]
fn xslt2_format_date_picture() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:xs="http://www.w3.org/2001/XMLSchema">
        <xsl:template match="/">
            <out><xsl:value-of select="format-date('2024-03-14', '[D01]/[M01]/[Y0001]')"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("14/03/2024"), "got: {out}");
}

/// XPath 2.0 `format-dateTime` — full date+time picture.
#[test]
fn xslt2_format_datetime_picture() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="format-dateTime('2024-03-14T15:42:07Z', '[Y]-[M01]-[D01] [H01]:[m01]:[s01][Z]')"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    // XSLT 2.0 §16.5.2 — the default `[Z]` presentation renders the
    // timezone numerically, so UTC is `+00:00` (not the military `Z`).
    assert!(out.contains("2024-03-14 15:42:07+00:00"), "got: {out}");
}

/// XSLT 2.0 §16.5.2 timezone forms: `[z]` adds a `GMT` prefix, and a
/// `,2-2` width suppresses the `:MM` group when the minutes are zero.
#[test]
fn xslt2_format_datetime_timezone_gmt_forms() {
    let f = |pic: &str| {
        let xslt = format!(r#"<xsl:stylesheet version="2.0"
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
            <xsl:template match="/"><out><xsl:value-of
              select="format-dateTime('2024-03-14T15:42:07-05:30', '{pic}')"/></out></xsl:template>
        </xsl:stylesheet>"#);
        transform(&xslt, "<r/>")
    };
    assert!(f("[z]").contains("GMT-05:30"), "got: {}", f("[z]"));
    assert!(f("[Z]").contains("-05:30"), "got: {}", f("[Z]"));
    // Whole-hour offset with `,2-2` shows hours only.
    let xslt = r#"<xsl:stylesheet version="2.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/"><out><xsl:value-of
          select="format-dateTime('2024-03-14T15:42:07-05:00', '[z,2-2]')"/></out></xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("GMT-05") && !out.contains("GMT-05:"), "got: {out}");
}

/// XSLT/XPath 2.0 — `fn:number` returns `xs:double`, whose string form
/// uses scientific notation outside `[1e-6, 1e6)` (`1.0E6`), while an
/// `xs:integer` literal stays decimal even when large.  The integer
/// case is the one that must NOT pick up E-notation.
#[test]
fn xslt2_double_vs_integer_string_form() {
    let f = |sel: &str| {
        let xslt = format!(r#"<xsl:stylesheet version="2.0"
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
            <xsl:template match="/"><out><xsl:value-of select="{sel}"/></out></xsl:template>
        </xsl:stylesheet>"#);
        transform(&xslt, "<r/>")
    };
    // fn:number → xs:double → scientific.
    assert!(f("string(number('1000000'))").contains("<out>1.0E6</out>"),
        "got: {}", f("string(number('1000000'))"));
    assert!(f("string(-1 * number('0.0000001'))").contains("-1.0E-7"),
        "got: {}", f("string(-1 * number('0.0000001'))"));
    // Large integer literal stays decimal (no E-notation).
    assert!(f("string(12345678901234)").contains("<out>12345678901234</out>"),
        "got: {}", f("string(12345678901234)"));
    // In-range double stays decimal too.
    assert!(f("string(number('20000'))").contains("<out>20000</out>"),
        "got: {}", f("string(number('20000'))"));
}

/// XPath 2.0 §3.1.1 — a numeric literal with an exponent is xs:double,
/// and round/floor/ceiling preserve that type, so the result answers
/// `instance of xs:double` and stringifies in scientific form.
#[test]
fn xslt2_double_literal_and_numeric_functions() {
    let f = |sel: &str| {
        let xslt = format!(r#"<xsl:stylesheet version="2.0"
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
            xmlns:xs="http://www.w3.org/2001/XMLSchema">
            <xsl:template match="/"><out><xsl:value-of select="{sel}"/></out></xsl:template>
        </xsl:stylesheet>"#);
        transform(&xslt, "<r/>")
    };
    // Exponent literal is xs:double; round keeps the type.
    assert!(f("1500000.0e0 instance of xs:double").contains(">true</out>"),
        "got: {}", f("1500000.0e0 instance of xs:double"));
    assert!(f("round(1500000.0e0) instance of xs:double").contains(">true</out>"),
        "got: {}", f("round(1500000.0e0) instance of xs:double"));
    assert!(f("string(round(1500000.4e0))").contains(">1.5E6</out>"),
        "got: {}", f("string(round(1500000.4e0))"));
    // An integer literal stringifies decimal, never E-notation.
    assert!(f("string(1500000)").contains(">1500000</out>"),
        "got: {}", f("string(1500000)"));
}

/// XSLT 2.0 §11.9.2 — `xsl:copy-of` of namespace nodes (selected via
/// the `namespace::` axis) adds the corresponding declarations to the
/// element under construction.
#[test]
fn xslt2_copy_of_namespace_nodes() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:copy-of select="//q/namespace::*"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt,
        r#"<doc><q xmlns:a="http://a.uri/" xmlns:b="http://b.uri/"/></doc>"#);
    assert!(out.contains(r#"xmlns:a="http://a.uri/""#)
        && out.contains(r#"xmlns:b="http://b.uri/""#),
        "namespace nodes not copied: {out}");
}

/// XSLT 2.0 §16.5.1 — the `[Y]` year component shows the absolute
/// year; the `[E]` era marker conveys AD/BC, so a proleptic negative
/// year formats as `55BC`, not `-55BC`.
#[test]
fn xslt2_format_date_era_negative_year() {
    let f = |date: &str| {
        let xslt = format!(r#"<xsl:stylesheet version="2.0"
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
            xmlns:xs="http://www.w3.org/2001/XMLSchema">
            <xsl:template match="/"><out><xsl:value-of
              select="format-date(xs:date('{date}'), '[Y][E]')"/></out></xsl:template>
        </xsl:stylesheet>"#);
        transform(&xslt, "<r/>")
    };
    assert!(f("1990-12-01").contains(">1990AD</out>"), "got: {}", f("1990-12-01"));
    assert!(f("-0055-12-01").contains(">55BC</out>"), "got: {}", f("-0055-12-01"));
}

/// XSLT 2.0 §16.5.1 — a picture whose presentation uses a non-ASCII
/// decimal-digit family (here Thai, U+0E50…) renders the component in
/// that family, with the digit count as the zero-padded width.
#[test]
fn xslt2_format_date_thai_digit_family() {
    let xslt = "<xsl:stylesheet version=\"2.0\" \
        xmlns:xsl=\"http://www.w3.org/1999/XSL/Transform\" \
        xmlns:xs=\"http://www.w3.org/2001/XMLSchema\">\
        <xsl:template match=\"/\"><out><xsl:value-of \
          select=\"format-date(xs:date('2003-09-07'), \
          '[Y\u{0e50}\u{0e50}\u{0e50}\u{0e51}]-[M\u{0e50}\u{0e51}]-[D\u{0e50}\u{0e51}]')\"/></out>\
        </xsl:template></xsl:stylesheet>";
    let out = transform(xslt, "<r/>");
    // 2003-09-07 → ๒๐๐๓-๐๙-๐๗ (Thai digits, zero-padded per picture).
    assert!(out.contains("\u{0e52}\u{0e50}\u{0e50}\u{0e53}-\u{0e50}\u{0e59}-\u{0e50}\u{0e57}"),
        "got: {out}");
}

/// XSLT 2.0 §16.5.1 week components: `[W]` is the ISO 8601 week of the
/// year, `[w]` the week within the month.
#[test]
fn xslt2_format_date_week_components() {
    let wk = |pic: &str, date: &str| {
        let xslt = format!(r#"<xsl:stylesheet version="2.0"
            xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
            <xsl:template match="/"><out><xsl:value-of
              select="format-date(xs:date('{date}'), '{pic}')"/></out></xsl:template>
        </xsl:stylesheet>"#);
        transform(&xslt, "<r/>")
    };
    // 2005-01-01 (Saturday) is ISO week 53 of 2004.
    assert!(wk("[W]", "2005-01-01").contains("<out>53</out>"), "got: {}", wk("[W]", "2005-01-01"));
    assert!(wk("[W]", "2005-02-01").contains("<out>5</out>"),  "got: {}", wk("[W]", "2005-02-01"));
    // Week within the month.
    assert!(wk("[w]", "2005-12-13").contains("<out>3</out>"),  "got: {}", wk("[w]", "2005-12-13"));
}

/// XPath 2.0 `format-time` — hour/minute picture.
#[test]
fn xslt2_format_time_picture() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="format-time('09:30:00', '[h]:[m01] [P]')"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    // XSLT 2.0 §16.5.1 — the `[P]` am/pm marker defaults to the
    // lower-case name; only `[PN]` upper-cases it.
    assert!(out.contains("9:30 am"), "got: {out}");
}

/// XSLT 2.0 `xsl:perform-sort` — re-orders a node sequence via
/// xsl:sort and emits the sorted items.
#[test]
fn xslt2_perform_sort_descending() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/r">
            <out><xsl:perform-sort select="item">
                <xsl:sort data-type="number" order="descending"/>
            </xsl:perform-sort></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r><item>30</item><item>10</item><item>20</item></r>");
    // After perform-sort descending: 30, 20, 10.
    let i30 = out.find("30").unwrap();
    let i20 = out.find("20").unwrap();
    let i10 = out.find("10").unwrap();
    assert!(i30 < i20 && i20 < i10, "expected 30,20,10 order; got {out}");
}

/// XSLT 2.0 `xsl:for-each-group group-starting-with` — pattern
/// boundaries open new groups.
#[test]
fn xslt2_for_each_group_starting_with() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/items">
            <out>
                <xsl:for-each-group select="*" group-starting-with="h">
                    <section n="{count(current-group())}"/>
                </xsl:for-each-group>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt,
        r#"<items><h>1</h><p>a</p><p>b</p><h>2</h><p>c</p></items>"#);
    // Two `<h>` starts → two sections, sizes 3 and 2.
    assert!(out.contains(r#"<section n="3"/>"#), "got: {out}");
    assert!(out.contains(r#"<section n="2"/>"#), "got: {out}");
}

/// XSLT 2.0 `mode="#current"` — apply-templates inherits the
/// caller's mode rather than dropping to default.
#[test]
fn xslt2_apply_templates_mode_current() {
    let xslt = r##"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/r"><xsl:apply-templates mode="m"/></xsl:template>
        <xsl:template match="x" mode="m">
            <hit><xsl:apply-templates mode="#current"/></hit>
        </xsl:template>
        <xsl:template match="y" mode="m">[y]</xsl:template>
    </xsl:stylesheet>"##;
    let out = transform(xslt, "<r><x><y/></x></r>");
    // y must dispatch to its mode="m" template via #current, not
    // fall back to the built-in templates.
    assert!(out.contains("<hit>[y]</hit>"), "got: {out}");
}

/// XPath 2.0 `deep-equal` over equal node-set string-values.
#[test]
fn xslt2_xpath_deep_equal() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="deep-equal(('a','b'), ('a','b'))"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">true<"), "got: {out}");
}

/// XPath 2.0 `default-collation` returns the codepoint URI.
#[test]
fn xslt2_xpath_default_collation() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="default-collation()"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("collation/codepoint"), "got: {out}");
}

/// `fn:deep-equal` rejects pairs that aren't comparable by `eq`.
/// xs:integer vs xs:string raises a type error per XPath 2.0
/// §15.3.1; deep-equal treats that as "not deep-equal".  Without
/// the type-family check, the general-comparison fallback would
/// say they ARE equal because both stringify to "3".
#[test]
fn xslt2_deep_equal_rejects_cross_type() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="deep-equal((3), ('3'))"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains(">false<"), "got: {out}");
}

/// `fn:min` / `fn:max` over a string sequence compares lexically,
/// not numerically.  `('1','5','06')` → max="5" (lex), not 6.
/// Exercises the untyped-string detection in `min_max_avg`.
#[test]
fn xslt2_min_max_strings_use_codepoint_collation() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:xs="http://www.w3.org/2001/XMLSchema"
        exclude-result-prefixes="xs">
        <xsl:template match="/">
            <out>
                <max><xsl:value-of select="max(for $x in ('1','5','06') return xs:string($x))"/></max>
                <min><xsl:value-of select="min(for $x in ('a','B','c') return xs:string($x))"/></min>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("<max>5</max>"), "got: {out}");
    // 'B' has a lower codepoint than 'a' (uppercase < lowercase in ASCII).
    assert!(out.contains("<min>B</min>"), "got: {out}");
}

/// `xsl:value-of select="round(-0.0e0)"` preserves the sign of
/// negative zero per XSD §F.3 — Saxon convention.  Without the
/// `is_sign_negative` check in `format_xpath_number`, the
/// integer-format path would render `-0` as `0`.
#[test]
fn xslt2_negative_zero_round_preserves_sign() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/">
            <out><xsl:value-of select="round(-0.0e0)"/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("<out>-0</out>"), "got: {out}");
}

/// xs:duration general equality uses (months, seconds) value-space
/// rather than lexical comparison: P12M = P1Y, P1D = PT24H, etc.
#[test]
fn xslt2_duration_value_equality() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:xs="http://www.w3.org/2001/XMLSchema"
        exclude-result-prefixes="xs">
        <xsl:template match="/">
            <out>
                <a><xsl:value-of select="xs:duration('P12M') = xs:duration('P1Y')"/></a>
                <b><xsl:value-of select="xs:duration('P1D') = xs:duration('PT24H')"/></b>
                <c><xsl:value-of select="xs:duration('-P1DT12H') = xs:duration('-PT36H')"/></c>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("<a>true</a>"), "got: {out}");
    assert!(out.contains("<b>true</b>"), "got: {out}");
    assert!(out.contains("<c>true</c>"), "got: {out}");
}

/// `fn:resolve-uri($rel)` consults the stylesheet's `xml:base` as
/// the static base URI when no second arg is supplied, and uses
/// RFC 3986 §5.3 reference resolution (not the previous lexical
/// "drop last component" join).
#[test]
fn xslt2_resolve_uri_uses_xml_base() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xml:base="http://www.example/tests/">
        <xsl:template match="/">
            <out>
                <a><xsl:value-of select="resolve-uri('foo.xml')"/></a>
                <b><xsl:value-of select="resolve-uri('')"/></b>
                <c><xsl:value-of select="static-base-uri()"/></c>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("<a>http://www.example/tests/foo.xml</a>"), "got: {out}");
    assert!(out.contains("<b>http://www.example/tests/</b>"),        "got: {out}");
    assert!(out.contains("<c>http://www.example/tests/</c>"),        "got: {out}");
}

/// `xsl:template match="document-node()"` binds to the document
/// root, and `document-node()/element()` matches the root's
/// element children — exercises the pattern-matcher shortcuts.
#[test]
fn xslt2_pattern_document_node_matches() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="text()"/>
        <xsl:template match="document-node()/child::element()">
            <elem><xsl:value-of select="name()"/></elem>
        </xsl:template>
        <xsl:template match="document-node()">
            <out>seen<xsl:apply-templates/></out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<doc>text</doc>");
    assert!(out.contains("<out>seen<elem>doc</elem></out>"), "got: {out}");
}

/// `format-date($d, '[D1o]')` adds an English ordinal suffix:
/// 1→1st, 2→2nd, 3→3rd, 11→11th, 21→21st, etc.
#[test]
fn xslt2_format_date_ordinal_day() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:xs="http://www.w3.org/2001/XMLSchema"
        exclude-result-prefixes="xs">
        <xsl:template match="/">
            <out>
                <a><xsl:value-of select="format-date(xs:date('2020-01-01'), '[D1o]')"/></a>
                <b><xsl:value-of select="format-date(xs:date('2020-01-02'), '[D1o]')"/></b>
                <c><xsl:value-of select="format-date(xs:date('2020-01-11'), '[D1o]')"/></c>
                <d><xsl:value-of select="format-date(xs:date('2020-01-21'), '[D1o]')"/></d>
                <e><xsl:value-of select="format-date(xs:date('2020-01-23'), '[D1o]')"/></e>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r/>");
    assert!(out.contains("<a>1st</a>"),  "got: {out}");
    assert!(out.contains("<b>2nd</b>"),  "got: {out}");
    assert!(out.contains("<c>11th</c>"), "got: {out}");
    assert!(out.contains("<d>21st</d>"), "got: {out}");
    assert!(out.contains("<e>23rd</e>"), "got: {out}");
}

/// `fn:nilled` returns the empty sequence for non-element nodes
/// and `false` for an untyped element — XPath 2.0 §15.4.6.
#[test]
fn xslt2_nilled_returns_empty_for_non_elements() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
        xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"
        exclude-result-prefixes="xsi">
        <xsl:variable name="e" as="element()"><e xsi:nil="true"/></xsl:variable>
        <xsl:template match="/">
            <out>
                <elem>[<xsl:value-of select="nilled($e)"/>]</elem>
                <attr>[<xsl:value-of select="nilled(/r/@a)"/>]</attr>
                <empty>[<xsl:value-of select="nilled(())"/>]</empty>
            </out>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, r#"<r a="x"/>"#);
    // An xsi:nil="true" element returns false in untyped mode (no
    // PSVI to consult).
    assert!(out.contains("<elem>[false]</elem>"), "got: {out}");
    // Attribute / empty-sequence → empty sequence → "".
    assert!(out.contains("<attr>[]</attr>"),  "got: {out}");
    assert!(out.contains("<empty>[]</empty>"), "got: {out}");
}

/// XSLT 2.0 tunnel parameters — the `t` param set at the outer
/// xsl:apply-templates flows through `mid` (which doesn't mention it)
/// and is consumed by `inner`'s tunnel-typed xsl:param.
#[test]
fn xslt2_tunnel_parameter_propagates_through_intermediate() {
    let xslt = r#"<xsl:stylesheet version="2.0"
        xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
        <xsl:template match="/r">
            <xsl:apply-templates select="*">
                <xsl:with-param name="t" select="'TUNNEL'" tunnel="yes"/>
            </xsl:apply-templates>
        </xsl:template>
        <xsl:template match="mid">
            <wrap><xsl:apply-templates/></wrap>
        </xsl:template>
        <xsl:template match="inner">
            <xsl:param name="t" tunnel="yes"/>
            <hit><xsl:value-of select="$t"/></hit>
        </xsl:template>
    </xsl:stylesheet>"#;
    let out = transform(xslt, "<r><mid><inner/></mid></r>");
    assert!(out.contains("<hit>TUNNEL</hit>"), "got: {out}");
}
