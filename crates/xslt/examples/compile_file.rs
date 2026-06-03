fn main() {
    let path = std::env::args().nth(1).expect("usage: compile_file <path>");
    let text = std::fs::read_to_string(&path).unwrap();
    match sup_xml_xslt::Stylesheet::compile_str(&text) {
        Ok(s)  => println!("OK — {} templates", s.ast.templates.len()),
        Err(e) => println!("ERR: {e}"),
    }
}
