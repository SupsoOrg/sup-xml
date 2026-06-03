// Quick harness to run one XSLT test by stylesheet path + source.
// Usage: cargo run --release -p sup-xml-xslt --example run_test -- <xsl> <src>
fn main() {
    let xsl_path = std::env::args().nth(1).expect("usage: run_test <xsl> <src>");
    let src_arg  = std::env::args().nth(2);
    let xsl = std::fs::read_to_string(&xsl_path).expect("read xsl");
    let src = match src_arg {
        Some(p) => std::fs::read_to_string(p).expect("read src"),
        None    => "<root/>".to_string(),
    };
    let mut opts = sup_xml_core::ParseOptions::default();
    opts.namespace_aware = true;
    let doc = match sup_xml_core::parse_str(&src, &opts) {
        Ok(d) => d,
        Err(e) => { eprintln!("source parse: {e}"); std::process::exit(2); }
    };
    let base = std::path::PathBuf::from(&xsl_path);
    let dir  = base.parent().map(|d| d.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let loader = sup_xml_xslt::FilesystemLoader::new(vec![dir]);
    let ss = match sup_xml_xslt::Stylesheet::compile_str_with_loader(
        &xsl, &loader, Some(&xsl_path),
    ) {
        Ok(s) => s,
        Err(e) => { eprintln!("compile: {e}"); std::process::exit(3); }
    };
    match ss.apply_with_loader(&doc, &loader, Some(&xsl_path)) {
        Ok(rt) => match rt.to_string() {
            Ok(s)  => println!("{s}"),
            Err(e) => { eprintln!("serialise: {e}"); std::process::exit(5); }
        },
        Err(e) => { eprintln!("apply: {e}"); std::process::exit(4); }
    }
}
