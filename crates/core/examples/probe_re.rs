use sup_xml_core::regex::{Pattern, Dialect};
fn check(p: &str) {
    match Pattern::compile_with(p, Dialect::Xpath) {
        Ok(_)  => println!("  OK    {p:?}"),
        Err(e) => println!("  ERR   {p:?} → {e}"),
    }
}
fn main() {
    check("a{,2}");
    check("[^[a-b]]");
    check(r"[Ք-՗]+");
    check(r"foo([a-\d]*)bar");
    check(r"(foo)(\077)");
    check(r"(foo)(\777)");
    check(r"(foo)(\1)");
    check(r"^(a{,2})$");
    check("[a-b-]");
    check("a{3,2}");
    check("[]");
    check("[a-]");
    check(r"\p{IsPrivateUse}");
    check("^[abcd]+*$");
    check("^[abcd]?+$");
    check("^[abcd]{1}*$");
    check("[\\w]");
    check("[\\d]");
}
