fn main() {
    use sup_xml_core::{ParseOptions, parse_str};
    use sup_xml_xslt::{FilesystemLoader, Stylesheet};
    use std::path::PathBuf;
    let arg = std::env::args().nth(1).expect("path");
    let tmpl = std::env::args().nth(2).unwrap_or_else(|| "main".into());
    let path = PathBuf::from(&arg);
    let xsl = std::fs::read_to_string(&path).unwrap();
    let dir = path.parent().unwrap().to_path_buf();
    let loader = FilesystemLoader::new(vec![dir.clone()]);
    let base = format!("file://{}/", dir.display());
    let ss = Stylesheet::compile_str_with_loader(&xsl, &loader, Some(&base)).unwrap();
    let opts = ParseOptions::default();
    let src = parse_str("<doc/>", &opts).unwrap();
    let rt = ss.apply_with_params_initial_and_mode(&src, &loader, Some(&base), &[], Some(&tmpl), None).unwrap();
    println!("{}", rt.to_string().unwrap());
}
