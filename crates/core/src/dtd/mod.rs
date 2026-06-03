//! DTD validation engine.
//!
//! Captures `<!ELEMENT>` and `<!ATTLIST>` declarations during parse,
//! then validates a parsed [`Document`](crate::dom::Document) against
//! them.
//!
//! # Scope
//!
//! Covers the subset of XML 1.0 § 3.2–3.3.4 that real consumers
//! actually rely on:
//!
//! * Element content models: `EMPTY`, `ANY`, Mixed (`(#PCDATA | a | b)*`),
//!   and children models (`(a, b)`, `(a | b)+`, etc.) with the standard
//!   occurrence indicators (`?`, `*`, `+`, none).
//! * Attribute declarations: every type from § 3.3.1 (`CDATA`, `ID`,
//!   `IDREF`, `IDREFS`, `ENTITY`, `ENTITIES`, `NMTOKEN`, `NMTOKENS`,
//!   `NOTATION`, enumerated), with all four DefaultDecl flavours
//!   (`#REQUIRED`, `#IMPLIED`, `#FIXED "..."`, literal default).
//! * ID uniqueness across the whole document.
//! * IDREF / IDREFS targets must resolve to an attribute typed `ID`.
//!
//! # Not yet covered
//!
//! * Default attribute *injection* on parse (when an ATTLIST default
//!   would supply a missing attribute, our parser ignores it).
//! * Notation cross-references (`NOTATION` decls are stored but the
//!   set of declared notations is not consulted).
//! * Parameter-entity-driven declarations inside the external subset.
//! * Conditional sections (`<![INCLUDE[ ... ]]>`).

pub mod inject;
pub mod model;
pub mod validate;

use std::collections::HashMap;

pub use inject::{inject_defaults, inject_defaults_from};
pub use model::{
    AttDecl, AttDefault, AttType, ContentModel, DeclRef, ElementDecl, EntityDecl, Group, GroupKind,
    Item, Occurrence, Particle,
};
pub use validate::{validate, DtdError};

/// Map element local-name → set of ATTLIST-declared `ID`-type
/// attribute names on that element.  Used by `id()` (XPath 1.0
/// §4.1) to walk only the DTD-typed attributes; without this map
/// every attribute literally named `id` is treated as an ID, which
/// catches some real-world cases but misses DTDs that declare an
/// ID attribute under a different name.
pub fn collect_id_attrs(dtd: &Dtd) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (elem, decls) in &dtd.attlists {
        let ids: Vec<String> = decls.iter()
            .filter(|d| matches!(d.att_type, AttType::Id))
            .map(|d| d.name.clone())
            .collect();
        if !ids.is_empty() {
            out.insert(elem.clone(), ids);
        }
    }
    out
}

/// Map element local-name → set of ATTLIST-declared `IDREF` / `IDREFS`
/// attribute names on that element.  Used by `idref()` (XPath 2.0
/// §14.5.5) to locate the attributes that reference a candidate ID.
pub fn collect_idref_attrs(dtd: &Dtd) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (elem, decls) in &dtd.attlists {
        let refs: Vec<String> = decls.iter()
            .filter(|d| matches!(d.att_type, AttType::IdRef | AttType::IdRefs))
            .map(|d| d.name.clone())
            .collect();
        if !refs.is_empty() {
            out.insert(elem.clone(), refs);
        }
    }
    out
}

/// Parsed DTD content used by [`validate`].  Built up incrementally
/// by the bytes-reader as it encounters `<!ELEMENT>` / `<!ATTLIST>`
/// declarations in the internal subset.
#[derive(Debug, Clone, Default)]
pub struct Dtd {
    /// Element declarations keyed by element name.
    pub elements: HashMap<String, ElementDecl>,
    /// Element names in declaration order — the order libxml2 keeps in
    /// `xmlDtd.children`, which lxml's `DTD.elements()` exposes.  The
    /// `elements` map is for lookup; this preserves source order.
    pub element_order: Vec<String>,
    /// Attribute declarations keyed by *element* name.  libxml2's
    /// ATTLIST groups can append (multiple `<!ATTLIST elem ...>` for
    /// the same `elem` are merged here).
    pub attlists: HashMap<String, Vec<AttDecl>>,
    /// Root element name from the DOCTYPE header: `<!DOCTYPE root_name
    /// ...>`.  Populated for any successfully-parsed doctype, even
    /// one with no internal subset and no external ID.  Empty when
    /// the document has no DOCTYPE at all.
    pub root_name: String,
    /// `PUBLIC "..."` identifier from the DOCTYPE header, when
    /// present.  `None` for `SYSTEM`-only or no external ID.
    pub public_id: Option<String>,
    /// `SYSTEM "..."` identifier (or the second literal of a `PUBLIC
    /// "..." "..."` form) from the DOCTYPE header.  `None` when the
    /// header has no external ID.
    pub system_id: Option<String>,
    /// Unparsed external general entities — those declared with an
    /// `NDATA` annotation per XML 1.0 § 4.2.2.  Keyed by entity name;
    /// the value carries the SYSTEM identifier (the URI a non-XML
    /// processor would fetch) and the PUBLIC identifier when present.
    /// Used by XSLT's `unparsed-entity-uri()` /
    /// `unparsed-entity-public-id()` functions (XSLT 1.0 § 12.4).
    pub unparsed_entities: HashMap<String, sup_xml_tree::UnparsedEntity>,
    /// General `<!ENTITY>` declarations in source order — what lxml's
    /// `DTD.entities()` exposes and the DTD serializer reconstructs.
    /// Carries internal, external, and unparsed entities (a superset of
    /// [`unparsed_entities`](Self::unparsed_entities)) plus parameter
    /// entities (flagged `parameter`).
    pub entities: Vec<crate::dtd::model::EntityDecl>,
    /// Declaration references in source order, so the DTD serializer can
    /// reproduce libxml2's `xmlDtd.children` ordering.
    pub decl_order: Vec<crate::dtd::model::DeclRef>,
    /// Raw markup declarations from the internal subset, in document
    /// order, each as it appeared in source (`<!ENTITY …>`,
    /// `<!ELEMENT …>`, `<!ATTLIST …>`, `<!NOTATION …>`).  Captured for
    /// round-trip serialization of the DOCTYPE's `[ … ]` body; empty
    /// when the document had no internal subset.  Only declarations
    /// read directly from the source are captured (not those produced
    /// by parameter-entity expansion).
    pub internal_decls: Vec<String>,
    /// Number of document-level comments/PIs that preceded the
    /// `<!DOCTYPE …>` in the prolog.  Lets the compat layer splice the
    /// internal-subset node into the document's sibling chain at its
    /// true position (`misc[..k]` → DOCTYPE → `misc[k..]` → root) so a
    /// comment that came before the DOCTYPE serializes before it, as
    /// libxml2 does.  Zero when the DOCTYPE was the first prolog item
    /// (the common case) or when there is no DOCTYPE.
    pub internal_subset_prolog_index: u32,
}

impl Dtd {
    /// Empty DTD — no declarations.
    pub fn new() -> Self { Self::default() }

    /// `true` when no element or attribute declarations have been
    /// captured.  Used as a fast-path skip in the validator.
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty() && self.attlists.is_empty()
    }

    /// Insert (or replace) an element declaration.  libxml2's
    /// behaviour on duplicate `<!ELEMENT name ...>` for the same
    /// `name` is to emit a warning and keep the first declaration;
    /// we keep the *first* too (this matches the de-facto standard
    /// across major parsers).
    pub fn add_element(&mut self, decl: ElementDecl) {
        // libxml2 keeps the first declaration on a duplicate `<!ELEMENT>`.
        if !self.elements.contains_key(&decl.name) {
            self.element_order.push(decl.name.clone());
            self.decl_order.push(crate::dtd::model::DeclRef::Element(decl.name.clone()));
            self.elements.insert(decl.name.clone(), decl);
        }
    }

    /// Append attribute declarations for an element.  Multiple
    /// `<!ATTLIST elem ...>` blocks merge — XML 1.0 § 3.3.
    pub fn add_attlist(&mut self, element: String, mut decls: Vec<AttDecl>) {
        // Record the ATTLIST's source position once per element (merged
        // attributes serialize together at the first declaration site).
        if !self.attlists.contains_key(&element) {
            self.decl_order.push(crate::dtd::model::DeclRef::Attlist(element.clone()));
        }
        let entry = self.attlists.entry(element).or_insert_with(Vec::new);
        entry.append(&mut decls);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::ParseOptions;
    use crate::parser::{parse_bytes_with_dtd, parse_external_subset};

    fn parse(src: &str) -> (sup_xml_tree::dom::Document, Dtd) {
        let opts = ParseOptions { namespace_aware: false, ..ParseOptions::default() };
        parse_bytes_with_dtd(src.as_bytes(), &opts).expect("parse failed")
    }

    #[test]
    fn external_subset_captures_element_and_attlist() {
        // A standalone DTD — no `<!DOCTYPE>` wrapper — is the external
        // subset: bare declarations parse directly.
        let dtd = parse_external_subset(
            b"<!ELEMENT a (b)>\n<!ATTLIST a x CDATA #IMPLIED>\n<!ELEMENT b EMPTY>",
            &ParseOptions::default(),
        ).expect("external subset should parse");
        assert!(dtd.elements.contains_key("a"));
        assert!(dtd.elements.contains_key("b"));
        assert_eq!(dtd.attlists.get("a").map(Vec::len), Some(1));
    }

    #[test]
    fn external_subset_allows_conditional_sections() {
        // Conditional sections are legal in the external subset but not
        // the internal one; the standalone parser must accept them.
        let dtd = parse_external_subset(
            b"<![INCLUDE[ <!ELEMENT keep EMPTY> ]]>\n<![IGNORE[ <!ELEMENT drop EMPTY> ]]>",
            &ParseOptions::default(),
        ).expect("conditional sections should parse");
        assert!(dtd.elements.contains_key("keep"));
        assert!(!dtd.elements.contains_key("drop"));
    }

    #[test]
    fn captures_empty_element_decl() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
]>
<r/>"#;
        let (_doc, dtd) = parse(src);
        assert!(matches!(dtd.elements.get("r").map(|e| &e.content), Some(ContentModel::Empty)));
    }

    #[test]
    fn captures_mixed_content() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (#PCDATA | em | b)*>
  <!ELEMENT em (#PCDATA)>
  <!ELEMENT b (#PCDATA)>
]>
<r>text <em>emp</em> tail</r>"#;
        let (_doc, dtd) = parse(src);
        match &dtd.elements.get("r").unwrap().content {
            ContentModel::Mixed { choices } => {
                assert_eq!(choices, &vec!["em".to_string(), "b".to_string()]);
            }
            other => panic!("expected Mixed, got {:?}", other),
        }
    }

    #[test]
    fn captures_children_with_quantifier() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a, b+, c?)>
  <!ELEMENT a EMPTY>
  <!ELEMENT b EMPTY>
  <!ELEMENT c EMPTY>
]>
<r><a/><b/><b/></r>"#;
        let (_doc, dtd) = parse(src);
        match &dtd.elements.get("r").unwrap().content {
            ContentModel::Children(g) => {
                assert_eq!(g.kind, GroupKind::Sequence);
                assert_eq!(g.items.len(), 3);
                assert_eq!(g.items[0].occur, Occurrence::One);
                assert_eq!(g.items[1].occur, Occurrence::OneOrMore);
                assert_eq!(g.items[2].occur, Occurrence::ZeroOrOne);
            }
            other => panic!("expected Children, got {:?}", other),
        }
    }

    #[test]
    fn captures_attlist() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r
    id    ID       #REQUIRED
    type  (a | b)  "a"
    note  CDATA    #IMPLIED>
]>
<r id="x1" type="b"/>"#;
        let (_doc, dtd) = parse(src);
        let attrs = dtd.attlists.get("r").expect("attrs");
        assert_eq!(attrs.len(), 3);
        assert!(matches!(attrs[0].att_type, AttType::Id));
        assert!(matches!(attrs[0].default, AttDefault::Required));
        assert!(matches!(&attrs[1].att_type, AttType::Enumeration(v) if v == &vec!["a".to_string(), "b".to_string()]));
        assert!(matches!(&attrs[1].default, AttDefault::Default(s) if s == "a"));
        assert!(matches!(attrs[2].att_type, AttType::CData));
        assert!(matches!(attrs[2].default, AttDefault::Implied));
    }

    #[test]
    fn validates_valid_document() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a+, b?)>
  <!ELEMENT a EMPTY>
  <!ELEMENT b (#PCDATA)>
  <!ATTLIST a id ID #REQUIRED>
]>
<r>
  <a id="x1"/>
  <a id="x2"/>
  <b>hi</b>
</r>"#;
        let (doc, dtd) = parse(src);
        validate(&doc, &dtd).expect("should validate");
    }

    #[test]
    fn rejects_missing_required_attr() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r id ID #REQUIRED>
]>
<r/>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("required")), "got: {:?}", errs);
    }

    #[test]
    fn rejects_bad_enum_value() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r EMPTY>
  <!ATTLIST r type (a | b) "a">
]>
<r type="c"/>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("enumeration")), "got: {:?}", errs);
    }

    #[test]
    fn rejects_duplicate_id() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a, a)>
  <!ELEMENT a EMPTY>
  <!ATTLIST a id ID #REQUIRED>
]>
<r><a id="x1"/><a id="x1"/></r>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("duplicate")), "got: {:?}", errs);
    }

    #[test]
    fn rejects_unresolved_idref() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a, b)>
  <!ELEMENT a EMPTY>
  <!ELEMENT b EMPTY>
  <!ATTLIST a id  ID    #REQUIRED>
  <!ATTLIST b ref IDREF #REQUIRED>
]>
<r><a id="x1"/><b ref="missing"/></r>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("IDREF")), "got: {:?}", errs);
    }

    #[test]
    fn rejects_bad_content_model() {
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a, b)>
  <!ELEMENT a EMPTY>
  <!ELEMENT b EMPTY>
]>
<r><b/><a/></r>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert!(errs.iter().any(|e| e.message.contains("does not match")), "got: {:?}", errs);
    }

    #[test]
    fn rejects_deeply_nested_content_model() {
        // A pathologically nested content model must error rather than
        // overflow the recursive-descent stack — the declaration comes
        // from an untrusted DTD, so this is a DoS guard, not a grammar
        // nicety.  `n` sits well above MAX_CONTENT_MODEL_DEPTH (256).
        let n = 400usize;
        let src = format!(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE r [\n  <!ELEMENT r {}a{}>\n]>\n<r/>",
            "(".repeat(n),
            ")".repeat(n),
        );
        let opts = ParseOptions { namespace_aware: false, ..ParseOptions::default() };
        let err = parse_bytes_with_dtd(src.as_bytes(), &opts)
            .expect_err("deeply nested content model should be rejected");
        assert!(
            err.message.contains("nesting depth exceeds limit"),
            "expected depth-limit error, got: {err}"
        );
    }

    #[test]
    fn loads_external_subset() {
        use std::io::Write;
        let mut tmp = std::env::temp_dir();
        tmp.push("sup_xml_dtd_external.dtd");
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(b"<!ELEMENT r (a+)>\n<!ELEMENT a EMPTY>\n<!ATTLIST a id ID #REQUIRED>\n").unwrap();
        drop(f);

        let src = format!(
            r#"<?xml version="1.0"?>
<!DOCTYPE r SYSTEM "{}">
<r><a id="x1"/></r>"#,
            tmp.display()
        );
        let opts = ParseOptions {
            namespace_aware: false,
            load_external_dtd: true,
            ..ParseOptions::default()
        };
        let (doc, dtd) = parse_bytes_with_dtd(src.as_bytes(), &opts).unwrap();
        assert!(dtd.elements.contains_key("r"), "external <!ELEMENT r> missing");
        assert!(dtd.elements.contains_key("a"), "external <!ELEMENT a> missing");
        assert!(dtd.attlists.contains_key("a"), "external <!ATTLIST a> missing");
        // Validation should succeed.
        validate(&doc, &dtd).expect("valid doc against external DTD");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn external_subset_off_by_default() {
        // Without load_external_dtd, SYSTEM path is parsed
        // syntactically but the file is NOT loaded.  Dtd stays
        // empty.
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r SYSTEM "/nonexistent/path.dtd">
<r/>"#;
        let opts = ParseOptions { namespace_aware: false, ..ParseOptions::default() };
        // No load_external_dtd → parse succeeds, dtd empty.
        let (_, dtd) = parse_bytes_with_dtd(src.as_bytes(), &opts).unwrap();
        assert!(dtd.is_empty(), "external DTD should not have been loaded");
    }

    #[test]
    fn missing_external_subset_is_non_fatal() {
        // Path doesn't exist — parse should succeed, dtd empty.
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r SYSTEM "/nonexistent/path-that-does-not-exist.dtd">
<r/>"#;
        let opts = ParseOptions {
            namespace_aware: false,
            load_external_dtd: true,
            ..ParseOptions::default()
        };
        let (_, dtd) = parse_bytes_with_dtd(src.as_bytes(), &opts).unwrap();
        assert!(dtd.is_empty(), "missing file should leave dtd empty");
    }

    #[test]
    fn internal_and_external_subsets_merge() {
        use std::io::Write;
        let mut tmp = std::env::temp_dir();
        tmp.push("sup_xml_dtd_merge.dtd");
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(b"<!ELEMENT a EMPTY>\n").unwrap();
        drop(f);

        let src = format!(
            r#"<?xml version="1.0"?>
<!DOCTYPE r SYSTEM "{}" [
  <!ELEMENT r (a+)>
]>
<r><a/></r>"#,
            tmp.display()
        );
        let opts = ParseOptions {
            namespace_aware: false,
            load_external_dtd: true,
            ..ParseOptions::default()
        };
        let (_, dtd) = parse_bytes_with_dtd(src.as_bytes(), &opts).unwrap();
        assert!(dtd.elements.contains_key("r"), "internal <!ELEMENT r> missing");
        assert!(dtd.elements.contains_key("a"), "external <!ELEMENT a> missing");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn collects_every_error_in_one_pass() {
        // Two distinct violations: required attr on first <a>,
        // duplicate id on the second.  Both should appear.
        let src = r#"<?xml version="1.0"?>
<!DOCTYPE r [
  <!ELEMENT r (a, a)>
  <!ELEMENT a EMPTY>
  <!ATTLIST a id ID #REQUIRED>
]>
<r><a/><a id="dup"/></r>"#;
        let (doc, dtd) = parse(src);
        let errs = validate(&doc, &dtd).unwrap_err();
        assert!(errs.len() >= 1, "expected at least 1 error, got {:?}", errs);
        assert!(errs.iter().any(|e| e.message.contains("required")),
                "missing 'required' error: {:?}", errs);
    }
}
