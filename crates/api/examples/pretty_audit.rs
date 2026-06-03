fn fmt(xml: &str) -> String {
    let doc = sup_xml::parse_str(xml, &sup_xml::ParseOptions::default()).unwrap();
    sup_xml::serialize_with(&doc, &sup_xml::SerializeOptions {
        format: true,
        indent: "  ".into(),
        ..sup_xml::SerializeOptions::default()
    })
}
fn main() {
    let cases = [
        ("simple nested",
         "<r><a><b><c>x</c></b></a></r>"),
        ("mixed content",
         "<p>before <b>bold</b> middle <i>italic</i> after</p>"),
        ("comments + pis",
         "<r><!-- top --><a/><?pi data?><b/></r>"),
        ("CDATA",
         "<r><note><![CDATA[unparsed & raw]]></note></r>"),
        ("attrs + nesting",
         r#"<r id="1" class="x"><child name="foo" value="bar"/></r>"#),
        ("ws-only text between elements",
         "<r>   <a/>   <b/>   </r>"),
        ("text node only",
         "<r>hello</r>"),
        ("empty element",
         "<r><a/></r>"),
        ("deep nesting",
         "<a><b><c><d><e>deep</e></d></c></b></a>"),
    ];
    for (label, xml) in cases {
        println!("=== {} ===", label);
        println!("INPUT:\n{}", xml);
        println!("OUTPUT:\n{}", fmt(xml));
        println!();
    }
}
