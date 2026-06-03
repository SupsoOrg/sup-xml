//! End-to-end tests for `Document::adopt_subtree` — deep-copying a
//! subtree from one arena into another, the Rust-side complement of
//! libxml2's free-floating-node + xmlAddChild pattern.

use sup_xml::{parse_str, serialize_to_string, ParseOptions};

fn parse(s: &str) -> sup_xml::Document {
    parse_str(s, &ParseOptions::default()).unwrap()
}

#[test]
fn adopt_element_with_text_child() {
    let src = parse("<scratch><greeting>hello</greeting></scratch>");
    let target = parse("<root/>");

    let greeting = src.root().children().next().unwrap();
    let adopted = target.adopt_subtree(greeting);
    target.append_child(target.root(), adopted);

    let out = serialize_to_string(&target);
    assert!(out.contains("<greeting>hello</greeting>"), "got: {out}");
}

#[test]
fn adopt_carries_attributes() {
    let src = parse(r#"<scratch><book id="b1" lang="en">War and Peace</book></scratch>"#);
    let target = parse("<library/>");

    let book = src.root().children().next().unwrap();
    let adopted = target.adopt_subtree(book);
    target.append_child(target.root(), adopted);

    let out = serialize_to_string(&target);
    assert!(out.contains(r#"id="b1""#), "missing id: {out}");
    assert!(out.contains(r#"lang="en""#), "missing lang: {out}");
    assert!(out.contains("War and Peace"), "missing text: {out}");
}

#[test]
fn adopt_recurses_into_children() {
    let src = parse("<s><a><b><c/></b></a></s>");
    let target = parse("<r/>");
    let a = src.root().children().next().unwrap();

    let adopted = target.adopt_subtree(a);
    target.append_child(target.root(), adopted);

    let out = serialize_to_string(&target);
    assert!(out.contains("<a><b><c/></b></a>") || out.contains("<a><b><c></c></b></a>"),
        "got: {out}");
}

#[test]
fn adopt_preserves_mixed_content() {
    let src = parse("<s><p>before<b>bold</b>after</p></s>");
    let target = parse("<r/>");
    let p = src.root().children().next().unwrap();

    let adopted = target.adopt_subtree(p);
    target.append_child(target.root(), adopted);

    let out = serialize_to_string(&target);
    assert!(out.contains("<p>before<b>bold</b>after</p>"), "got: {out}");
}

#[test]
fn adopt_preserves_comment_and_pi() {
    let src = parse(r#"<s><!-- note --><?php echo "hi" ?><x/></s>"#);
    let target = parse("<r/>");

    // Adopt each child of <s> into target.
    for child in src.root().children() {
        let adopted = target.adopt_subtree(child);
        target.append_child(target.root(), adopted);
    }

    let out = serialize_to_string(&target);
    assert!(out.contains("<!-- note -->"), "missing comment: {out}");
    assert!(out.contains("<?php"), "missing PI: {out}");
    assert!(out.contains("<x/>") || out.contains("<x></x>"), "missing element: {out}");
}

#[test]
fn adopt_preserves_cdata() {
    let src = parse("<s><p><![CDATA[a < b]]></p></s>");
    let target = parse("<r/>");
    let p = src.root().children().next().unwrap();
    let adopted = target.adopt_subtree(p);
    target.append_child(target.root(), adopted);

    let out = serialize_to_string(&target);
    // CDATA may serialize as either literal CDATA or escaped text; both
    // preserve the bytes.
    assert!(out.contains("a < b") || out.contains("a &lt; b"),
        "lost CDATA content: {out}");
}

#[test]
fn adopted_node_is_independent_of_source() {
    // Mutate source after adoption — target shouldn't see the change.
    let src = parse(r#"<s><tag attr="old"/></s>"#);
    let target = parse("<r/>");
    let tag = src.root().children().next().unwrap();
    let adopted = target.adopt_subtree(tag);
    target.append_child(target.root(), adopted);

    // (Source is read-only here; the test just verifies serialization
    // of `target` doesn't reach back into src.  If it did, the bytes
    // would be different after src goes out of scope below.)
    let out = serialize_to_string(&target);
    drop(src);
    let out_after = serialize_to_string(&target);

    assert_eq!(out, out_after,
        "adopted subtree shouldn't depend on source doc staying alive");
    assert!(out.contains(r#"attr="old""#), "got: {out}");
}

#[test]
fn adopt_multiple_into_same_doc() {
    let src = parse("<s><a/><b/><c/></s>");
    let target = parse("<r/>");

    for child in src.root().children() {
        let adopted = target.adopt_subtree(child);
        target.append_child(target.root(), adopted);
    }

    let out = serialize_to_string(&target);
    // All three names should appear inside <r>.
    let r_start = out.find("<r>").or_else(|| out.find("<r ")).unwrap_or(0);
    let body = &out[r_start..];
    assert!(body.contains("<a"), "missing a: {out}");
    assert!(body.contains("<b"), "missing b: {out}");
    assert!(body.contains("<c"), "missing c: {out}");
}
