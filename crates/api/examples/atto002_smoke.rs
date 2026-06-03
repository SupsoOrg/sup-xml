fn main() {
    let arg = std::env::args().nth(1).expect("usage: atto002_smoke <path/to/schema.xsd>");
    let src = std::fs::read_to_string(&arg).unwrap();
    match sup_xml::xsd::Schema::compile_str(&src) {
        Ok(_) => println!("compiled OK (no error)"),
        Err(e) => println!("rejected: {e}"),
    }
}
