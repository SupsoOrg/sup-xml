#![cfg(feature = "serde")]

use serde::Deserialize;
use sup_xml::de::{from_str, DeOptions, from_str_opts};

// ── scalars ──────────────────────────────────────────────────────────────────

#[test]
fn string_scalar() {
    let s: String = from_str("<x>hello</x>").unwrap();
    assert_eq!(s, "hello");
}

#[test]
fn empty_string_scalar() {
    let s: String = from_str("<x></x>").unwrap();
    assert_eq!(s, "");
}

#[test]
fn self_closing_string_scalar() {
    let s: String = from_str("<x/>").unwrap();
    assert_eq!(s, "");
}

#[test]
fn int_scalar() {
    let n: i32 = from_str("<x>42</x>").unwrap();
    assert_eq!(n, 42);
}

#[test]
fn negative_int() {
    let n: i64 = from_str("<x>-12345</x>").unwrap();
    assert_eq!(n, -12345);
}

#[test]
fn unsigned_int() {
    let n: u16 = from_str("<x>65000</x>").unwrap();
    assert_eq!(n, 65000);
}

#[test]
fn float_scalar() {
    let n: f64 = from_str("<x>3.14159</x>").unwrap();
    assert!((n - 3.14159).abs() < 1e-9);
}

#[test]
fn bool_true_scalar() {
    let b: bool = from_str("<x>true</x>").unwrap();
    assert!(b);
}

#[test]
fn bool_false_scalar() {
    let b: bool = from_str("<x>false</x>").unwrap();
    assert!(!b);
}

#[test]
fn bool_one_zero_scalar() {
    let b: bool = from_str("<x>1</x>").unwrap();
    assert!(b);
    let b: bool = from_str("<x>0</x>").unwrap();
    assert!(!b);
}

#[test]
fn int_with_surrounding_whitespace() {
    let n: i32 = from_str("<x>  42  </x>").unwrap();
    assert_eq!(n, 42);
}

// ── structs ──────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Deserialize)]
struct Page {
    #[serde(rename = "@id")]
    id: u32,
    title: String,
}

#[test]
fn struct_with_attribute_and_child() {
    let xml = r#"<page id="7"><title>hello</title></page>"#;
    let p: Page = from_str(xml).unwrap();
    assert_eq!(p, Page { id: 7, title: "hello".into() });
}

#[derive(Debug, PartialEq, Deserialize)]
struct OnlyAttrs {
    #[serde(rename = "@a")]
    a: i32,
    #[serde(rename = "@b")]
    b: String,
}

#[test]
fn struct_with_only_attributes() {
    let xml = r#"<x a="1" b="hi"/>"#;
    let v: OnlyAttrs = from_str(xml).unwrap();
    assert_eq!(v, OnlyAttrs { a: 1, b: "hi".into() });
}

#[derive(Debug, PartialEq, Deserialize)]
struct WithText {
    #[serde(rename = "@kind")]
    kind: String,
    #[serde(rename = "$text")]
    body: String,
}

#[test]
fn struct_with_text_field() {
    let xml = r#"<note kind="warning">be careful</note>"#;
    let v: WithText = from_str(xml).unwrap();
    assert_eq!(v, WithText { kind: "warning".into(), body: "be careful".into() });
}

#[derive(Debug, PartialEq, Deserialize)]
struct Outer {
    inner: Inner,
}

#[derive(Debug, PartialEq, Deserialize)]
struct Inner {
    #[serde(rename = "@id")]
    id: u32,
    name: String,
}

#[test]
fn nested_struct() {
    let xml = r#"<outer><inner id="3"><name>foo</name></inner></outer>"#;
    let v: Outer = from_str(xml).unwrap();
    assert_eq!(v, Outer { inner: Inner { id: 3, name: "foo".into() } });
}

#[test]
fn struct_skips_comments_and_pis() {
    let xml = r#"<page id="1"><!-- comment --><?proc inst?><title>hi</title></page>"#;
    let p: Page = from_str(xml).unwrap();
    assert_eq!(p, Page { id: 1, title: "hi".into() });
}

#[test]
fn struct_ignores_inter_element_whitespace() {
    let xml = "<page id=\"1\">\n  <title>hi</title>\n</page>";
    let p: Page = from_str(xml).unwrap();
    assert_eq!(p, Page { id: 1, title: "hi".into() });
}

// ── Vec ──────────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Deserialize)]
struct Items {
    item: Vec<Item>,
}

#[derive(Debug, PartialEq, Deserialize)]
struct Item {
    #[serde(rename = "@id")]
    id: u32,
}

#[test]
fn vec_of_structs() {
    let xml = r#"<items><item id="1"/><item id="2"/><item id="3"/></items>"#;
    let v: Items = from_str(xml).unwrap();
    assert_eq!(v.item.len(), 3);
    assert_eq!(v.item[1].id, 2);
}

#[test]
fn vec_of_strings() {
    #[derive(Deserialize, Debug, PartialEq)]
    struct V {
        word: Vec<String>,
    }
    let xml = "<v><word>foo</word><word>bar</word><word>baz</word></v>";
    let v: V = from_str(xml).unwrap();
    assert_eq!(v.word, vec!["foo", "bar", "baz"]);
}

#[test]
fn empty_vec_when_field_absent() {
    #[derive(Deserialize, Debug, PartialEq)]
    struct V {
        #[serde(default)]
        item: Vec<Item>,
    }
    let xml = "<v/>";
    let v: V = from_str(xml).unwrap();
    assert_eq!(v.item.len(), 0);
}

#[test]
fn vec_with_mixed_siblings() {
    #[derive(Deserialize, Debug, PartialEq)]
    struct V {
        a: Vec<Item>,
        b: Item,
    }
    let xml = r#"<v><a id="1"/><a id="2"/><b id="9"/></v>"#;
    let v: V = from_str(xml).unwrap();
    assert_eq!(v.a.len(), 2);
    assert_eq!(v.b.id, 9);
}

#[test]
fn vec_with_inter_element_whitespace() {
    let xml = "<items>\n  <item id=\"1\"/>\n  <item id=\"2\"/>\n</items>";
    let v: Items = from_str(xml).unwrap();
    assert_eq!(v.item.len(), 2);
}

// ── Option ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, PartialEq)]
struct Maybe {
    a: Option<String>,
    b: Option<String>,
}

#[test]
fn option_present_and_absent() {
    let xml = "<m><a>hi</a></m>";
    let v: Maybe = from_str(xml).unwrap();
    assert_eq!(v.a, Some("hi".into()));
    assert_eq!(v.b, None);
}

#[test]
fn option_attribute_present_and_absent() {
    #[derive(Deserialize, Debug, PartialEq)]
    struct V {
        #[serde(rename = "@a")]
        a: Option<u32>,
        #[serde(rename = "@b")]
        b: Option<u32>,
    }
    let v: V = from_str(r#"<x a="7"/>"#).unwrap();
    assert_eq!(v, V { a: Some(7), b: None });
}

#[test]
fn option_vec() {
    #[derive(Deserialize, Debug, PartialEq)]
    struct V {
        item: Option<Vec<String>>,
    }
    let v: V = from_str("<v><item>x</item><item>y</item></v>").unwrap();
    assert_eq!(v.item, Some(vec!["x".into(), "y".into()]));
}

// ── Map ──────────────────────────────────────────────────────────────────────

#[test]
fn map_of_strings() {
    use std::collections::BTreeMap;
    let xml = "<m><a>1</a><b>2</b><c>3</c></m>";
    let m: BTreeMap<String, String> = from_str(xml).unwrap();
    assert_eq!(m.get("a"), Some(&"1".to_string()));
    assert_eq!(m.get("b"), Some(&"2".to_string()));
    assert_eq!(m.get("c"), Some(&"3".to_string()));
}

// ── Enum ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, PartialEq)]
enum Color {
    Red,
    Green,
    Blue,
}

#[test]
fn unit_variant() {
    let c: Color = from_str("<Red/>").unwrap();
    assert_eq!(c, Color::Red);
    let c: Color = from_str("<Blue/>").unwrap();
    assert_eq!(c, Color::Blue);
}

#[derive(Deserialize, Debug, PartialEq)]
enum Shape {
    Square(u32),
    Rect { w: u32, h: u32 },
}

#[test]
fn newtype_variant() {
    let s: Shape = from_str("<Square>5</Square>").unwrap();
    assert_eq!(s, Shape::Square(5));
}

#[test]
fn struct_variant() {
    let s: Shape = from_str("<Rect><w>3</w><h>4</h></Rect>").unwrap();
    assert_eq!(s, Shape::Rect { w: 3, h: 4 });
}

// ── $value ───────────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug, PartialEq)]
enum Block {
    Para(String),
    Img(String),
}

#[derive(Deserialize, Debug, PartialEq)]
struct Doc {
    #[serde(rename = "$value")]
    body: Vec<Block>,
}

#[test]
fn value_field_collects_heterogeneous_elements() {
    let xml = "<doc><Para>hi</Para><Img>cat.png</Img><Para>bye</Para></doc>";
    let d: Doc = from_str(xml).unwrap();
    assert_eq!(d.body, vec![
        Block::Para("hi".into()),
        Block::Img("cat.png".into()),
        Block::Para("bye".into()),
    ]);
}

#[test]
fn value_field_with_named_field_alongside() {
    #[derive(Deserialize, Debug, PartialEq)]
    struct Page2 {
        #[serde(rename = "@id")]
        id: u32,
        title: String,
        #[serde(rename = "$value")]
        body: Vec<Block>,
    }
    let xml = r#"<page id="1"><title>T</title><Para>p1</Para><Img>i</Img></page>"#;
    let p: Page2 = from_str(xml).unwrap();
    assert_eq!(p.id, 1);
    assert_eq!(p.title, "T");
    assert_eq!(p.body.len(), 2);
}

// ── xsi:nil ──────────────────────────────────────────────────────────────────

#[test]
fn xsi_nil_yields_none() {
    #[derive(Deserialize, Debug, PartialEq)]
    struct V {
        a: Option<String>,
        b: Option<String>,
    }
    let xml = r#"<v><a xsi:nil="true"/><b>here</b></v>"#;
    let v: V = from_str(xml).unwrap();
    assert_eq!(v.a, None);
    assert_eq!(v.b, Some("here".into()));
}

#[test]
fn xsi_nil_can_be_disabled() {
    #[derive(Deserialize, Debug, PartialEq)]
    struct V {
        a: Option<String>,
    }
    let mut opts = DeOptions::default();
    opts.honor_xsi_nil = false;
    let xml = r#"<v><a xsi:nil="true"/></v>"#;
    let v: V = from_str_opts(xml, opts).unwrap();
    // With nil disabled the element is present-but-empty, so we get Some("").
    assert_eq!(v.a, Some(String::new()));
}

// ── options ──────────────────────────────────────────────────────────────────

// ── edge cases ───────────────────────────────────────────────────────────────

#[test]
fn entity_references_in_text() {
    let s: String = from_str("<x>a &amp; b &lt; c</x>").unwrap();
    assert_eq!(s, "a & b < c");
}

#[test]
fn entity_references_in_attribute() {
    #[derive(Deserialize, Debug, PartialEq)]
    struct V {
        #[serde(rename = "@msg")]
        msg: String,
    }
    let v: V = from_str(r#"<x msg="a &amp; b"/>"#).unwrap();
    assert_eq!(v.msg, "a & b");
}

#[test]
fn cdata_in_scalar() {
    let s: String = from_str("<x><![CDATA[<raw> & stuff]]></x>").unwrap();
    assert_eq!(s, "<raw> & stuff");
}

#[test]
fn char_scalar() {
    let c: char = from_str("<x>Z</x>").unwrap();
    assert_eq!(c, 'Z');
}

#[test]
fn char_unicode_scalar() {
    let c: char = from_str("<x>中</x>").unwrap();
    assert_eq!(c, '中');
}

#[test]
fn newtype_struct() {
    #[derive(Deserialize, Debug, PartialEq)]
    struct Wrap(u32);
    let w: Wrap = from_str("<w>42</w>").unwrap();
    assert_eq!(w, Wrap(42));
}

#[test]
fn deeply_nested_struct() {
    #[derive(Deserialize, Debug, PartialEq)]
    struct A { b: B }
    #[derive(Deserialize, Debug, PartialEq)]
    struct B { c: C }
    #[derive(Deserialize, Debug, PartialEq)]
    struct C { #[serde(rename="@v")] v: i32 }

    let a: A = from_str(r#"<a><b><c v="7"/></b></a>"#).unwrap();
    assert_eq!(a, A { b: B { c: C { v: 7 } } });
}

#[test]
fn unknown_element_is_skipped_by_default() {
    // serde-derive on a struct calls deserialize_ignored_any for unknown
    // fields by default — verify our impl walks past them cleanly.
    #[derive(Deserialize, Debug, PartialEq)]
    struct V {
        wanted: String,
    }
    let xml = "<v><junk><inner/></junk><wanted>hi</wanted><tail/></v>";
    let v: V = from_str(xml).unwrap();
    assert_eq!(v.wanted, "hi");
}

#[test]
fn from_bytes_works() {
    let b: &[u8] = b"<x>123</x>";
    let n: i32 = sup_xml::de::from_bytes(b).unwrap();
    assert_eq!(n, 123);
}

#[test]
fn from_bytes_rejects_invalid_utf8() {
    let bad: &[u8] = &[0xff, 0xfe, 0x00];
    let err = sup_xml::de::from_bytes::<i32>(bad).unwrap_err();
    assert!(err.message.contains("UTF-8"));
}

#[test]
fn malformed_xml_returns_error() {
    let bad = "<x><y></x>";
    let r: Result<String, _> = from_str(bad);
    assert!(r.is_err());
}

#[test]
fn custom_attribute_prefix() {
    #[derive(Deserialize, PartialEq, Debug)]
    struct V {
        #[serde(rename = ":id")]
        id: u32,
    }
    let mut opts = DeOptions::default();
    opts.attribute_prefix = ':';
    let xml = r#"<x id="9"/>"#;
    let v: V = from_str_opts(xml, opts).unwrap();
    assert_eq!(v, V { id: 9 });
}
