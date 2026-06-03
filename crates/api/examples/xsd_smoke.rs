fn main() {
    let mut args = std::env::args().skip(1);
    let schema_path = args.next().expect("usage: xsd_smoke <schema.xsd> [instance.xml]");
    let instance_path = args.next();

    let src = std::fs::read_to_string(&schema_path).unwrap();
    let dir = std::path::Path::new(&schema_path).parent()
        .unwrap_or(std::path::Path::new(".")).to_path_buf();
    let resolver = sup_xml::xsd::FsResolver::new(dir);
    let schema = match sup_xml::xsd::Schema::compile_with(&src, resolver) {
        Ok(s) => { println!("schema: compiled OK"); s }
        Err(e) => { println!("schema: rejected: {e}"); return; }
    };
    if let Some(p) = instance_path {
        let inst = std::fs::read_to_string(&p).unwrap();
        match schema.validate_str(&inst) {
            Ok(()) => println!("instance: valid"),
            Err(e) => println!("instance: invalid: {}", e.issues.first()
                .map(|i| i.message.as_str()).unwrap_or("(no issue)")),
        }
    }
}
