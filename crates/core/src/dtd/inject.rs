//! Default attribute injection per XML 1.0 § 3.3.2.
//!
//! When an `<!ATTLIST>` declares an attribute with a literal default
//! (`name CDATA "default"`) or `#FIXED "value"`, every element of the
//! matching type that doesn't supply the attribute explicitly should
//! receive it.  libxml2 performs this *during* parsing; we do it as a
//! post-parse walk over the [`Document`], which keeps the parser
//! itself oblivious to DTD semantics.
//!
//! `#REQUIRED` and `#IMPLIED` produce nothing: the former is an
//! error if the attribute is missing (caught later by
//! [`super::validate`]), the latter has no default.
//!
//! Run this BEFORE [`super::validate`] — validation reads the
//! resulting attribute set, and missing-with-default attrs would
//! otherwise look like `#REQUIRED` violations to the validator.

use sup_xml_tree::dom::Document;

use super::{AttDefault, Dtd};

/// Walk the tree rooted at `doc.root()` and, for every element
/// whose `<!ATTLIST>` declares a literal default or `#FIXED` value,
/// add that attribute when it isn't already present.
///
/// Returns the number of attributes injected — useful in tests and
/// for telemetry, never load-bearing for correctness.
///
/// No-op when `dtd.is_empty()`.
pub fn inject_defaults(doc: &Document, dtd: &Dtd) -> usize {
    if dtd.is_empty() { return 0; }
    inject_defaults_from(doc.root(), dtd, doc)
}

/// Like [`inject_defaults`] but starting from an explicit `root` subtree
/// rather than `doc.root()`.  Used by the incremental push parser, whose
/// document's `root` pointer isn't wired during the streaming build —
/// the caller passes the actual root element from the tree it grew.
/// `doc` supplies the arena for the new attribute nodes.
pub fn inject_defaults_from<'a>(
    root: &'a sup_xml_tree::dom::Node<'a>,
    dtd:  &Dtd,
    doc:  &'a Document,
) -> usize {
    if dtd.is_empty() { return 0; }
    let mut count = 0usize;
    walk(root, dtd, doc, &mut count);
    count
}

fn walk<'a>(
    node:  &'a sup_xml_tree::dom::Node<'a>,
    dtd:   &Dtd,
    doc:   &'a Document,
    count: &mut usize,
) {
    if !node.is_element() { return; }

    if let Some(attlist) = dtd.attlists.get(node.name()) {
        // For each decl with a default value, check whether the
        // attribute is already present on this element.  If not,
        // allocate a new attribute and append it.
        for decl in attlist {
            let default_value: &str = match &decl.default {
                AttDefault::Default(v) | AttDefault::Fixed(v) => v.as_str(),
                AttDefault::Required | AttDefault::Implied   => continue,
            };
            if node.attributes().any(|a| a.name() == decl.name) {
                continue; // explicitly set, keep author value
            }
            let name  = doc.bump().alloc_str(&decl.name);
            let value = doc.bump().alloc_str(default_value);
            let attr = doc.new_attribute(name, value);
            doc.append_attribute(node, attr);
            *count += 1;
        }
    }

    for child in node.children() {
        walk(child, dtd, doc, count);
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::options::ParseOptions;
    use crate::parser::parse_bytes_with_dtd;

    use super::inject_defaults;

    fn parse(src: &str) -> (sup_xml_tree::dom::Document, crate::dtd::Dtd) {
        let opts = ParseOptions { namespace_aware: false, ..ParseOptions::default() };
        parse_bytes_with_dtd(src.as_bytes(), &opts).expect("parse")
    }

    #[test]
    fn injects_literal_default() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r kind CDATA "alpha">
]>
<r/>"#;
        let (doc, dtd) = parse(src);
        let n = inject_defaults(&doc, &dtd);
        assert_eq!(n, 1);
        let root = doc.root();
        let got = root.attributes().find(|a| a.name() == "kind").map(|a| a.value());
        assert_eq!(got, Some("alpha"));
    }

    #[test]
    fn injects_fixed_default() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r version CDATA #FIXED "1.0">
]>
<r/>"#;
        let (doc, dtd) = parse(src);
        assert_eq!(inject_defaults(&doc, &dtd), 1);
        let root = doc.root();
        let got = root.attributes().find(|a| a.name() == "version").map(|a| a.value());
        assert_eq!(got, Some("1.0"));
    }

    #[test]
    fn preserves_explicit_value() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r kind CDATA "alpha">
]>
<r kind="beta"/>"#;
        let (doc, dtd) = parse(src);
        assert_eq!(inject_defaults(&doc, &dtd), 0);
        let got = doc.root().attributes().find(|a| a.name() == "kind").map(|a| a.value());
        assert_eq!(got, Some("beta"));
    }

    #[test]
    fn required_and_implied_skipped() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r
    must  CDATA  #REQUIRED
    maybe CDATA  #IMPLIED>
]>
<r must="x"/>"#;
        let (doc, dtd) = parse(src);
        // Nothing to inject — REQUIRED supplied by author, IMPLIED has no default.
        assert_eq!(inject_defaults(&doc, &dtd), 0);
    }

    #[test]
    fn injects_on_descendants() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a, a)>
  <!ELEMENT a EMPTY>
  <!ATTLIST a tag CDATA "default-tag">
]>
<r><a/><a tag="custom"/></r>"#;
        let (doc, dtd) = parse(src);
        let n = inject_defaults(&doc, &dtd);
        // Only the first `<a/>` gets the injection; the second has tag="custom".
        assert_eq!(n, 1);
        let mut tags: Vec<&str> = Vec::new();
        for child in doc.root().children() {
            if child.is_element() {
                if let Some(a) = child.attributes().find(|a| a.name() == "tag") {
                    tags.push(a.value());
                }
            }
        }
        assert_eq!(tags, vec!["default-tag", "custom"]);
    }
}
