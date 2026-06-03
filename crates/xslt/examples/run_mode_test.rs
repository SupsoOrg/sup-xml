// Reproduce the harness path: read env source content and apply.
fn main() {
    let xsl = std::fs::read_to_string(
        "tests/assets/xslt30-test/tests/attr/mode/mode-0101.xsl"
    ).unwrap();
    let src = " \n<doc>\n  <a test=\"a attribute\">a-text</a>\n</doc>\n";

    let mut opts = sup_xml_core::ParseOptions::default();
    opts.namespace_aware = true;
    let doc = match sup_xml_core::parse_str(src, &opts) {
        Ok(d) => d, Err(e) => { eprintln!("source parse: {e}"); return; }
    };
    let ss = match sup_xml_xslt::Stylesheet::compile_str(&xsl) {
        Ok(s) => s, Err(e) => { eprintln!("compile: {e}"); return; }
    };
    match ss.apply(&doc) {
        Ok(rt) => println!("OUT={}", rt.to_string().unwrap_or_default()),
        Err(e) => eprintln!("apply: {e}"),
    }
}
