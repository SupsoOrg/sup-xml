//! SupXML — a memory-safe, libxml2-compatible XML library for Rust.
//!
//! # Quick start
//!
//! ```
//! use sup_xml::{parse_str, serialize_to_string, xpath_count, ParseOptions};
//!
//! let doc = parse_str(r#"
//!     <catalog>
//!         <book id="1"><title>Dune</title></book>
//!         <book id="2"><title>Foundation</title></book>
//!     </catalog>
//! "#, &ParseOptions::default()).unwrap();
//!
//! // Navigate with XPath.
//! assert_eq!(xpath_count(&doc, "/catalog/book").unwrap(), 2);
//!
//! // Roundtrip back to XML text.
//! let xml = serialize_to_string(&doc);
//! assert!(xml.contains("<title>Dune</title>"));
//! ```
//!
//! # Feature overview
//!
//! | Area | Functions |
//! |---|---|
//! | Parsing | [`parse_str`], [`parse_bytes`] (both take `&ParseOptions`), [`parse_str_with_recovered`], [`parse_bytes_with_recovered`] |
//! | Namespace resolution | [`parse_ns_str`] |
//! | Serialization | [`serialize_to_string`], [`serialize_formatted`], [`serialize_with`] |
//! | XPath 1.0 | [`XPathContext`] (reusable), [`xpath_eval`], [`xpath_str`], [`xpath_bool`], [`xpath_num`], [`xpath_count`] |
//! | Typed serde deserialization | [`de::from_str`], [`de::from_bytes`] (feature `serde`) |
//! | Security | [`ParseOptions`] — entity budget, depth limit, external-entity toggle |
//!
//! # Thread safety
//!
//! A [`Document`] owns its entire arena, so it is **`Send`**: you can parse
//! on one thread and hand the whole document to another for XPath, walking,
//! or serialization. Parsing itself is thread-independent — every call to
//! [`parse_str`] / [`parse_bytes`] builds a self-contained document with no
//! shared mutable state, so any number of threads can parse concurrently.
//!
//! ```
//! use sup_xml::{parse_str, xpath_count, ParseOptions};
//! let doc = parse_str("<r><a/></r>", &ParseOptions::default()).unwrap();
//! // Moving the whole document into a thread is fine — `Document: Send`.
//! let n = std::thread::spawn(move || xpath_count(&doc, "//a").unwrap())
//!     .join()
//!     .unwrap();
//! assert_eq!(n, 1);
//! ```
//!
//! A `Document` is **not `Sync`**, however: its nodes thread themselves
//! together with interior-mutable pointers, so a shared `&Document` cannot
//! be touched from two threads at once. Share work by giving each thread its
//! own document, not a borrow of one.
//!
//! ```compile_fail
//! use sup_xml::{parse_str, xpath_count, ParseOptions};
//! let doc = parse_str("<r><a/></r>", &ParseOptions::default()).unwrap();
//! // `&Document` is not `Sync`, so it cannot be shared across scoped
//! // threads — this fails to compile.
//! std::thread::scope(|s| {
//!     s.spawn(|| xpath_count(&doc, "//a").unwrap());
//!     s.spawn(|| xpath_count(&doc, "//a").unwrap());
//! });
//! ```
//!
//! # Security
//!
//! SupXML is built to parse **untrusted input** safely, and the default
//! [`ParseOptions`] are the hardened stance — you opt *out* of
//! protections, never into them.
//!
//! - **No external fetches (XXE / SSRF).** External DTD loading is off
//!   ([`ParseOptions::load_external_dtd`] defaults to `false`) and no
//!   external entity or DTD is ever retrieved from the network or
//!   filesystem unless you explicitly install an
//!   [`ParseOptions::external_resolver`]. An unresolved external entity
//!   reference is rejected, not silently expanded.
//! - **Bounded entity expansion (billion laughs).**
//!   [`ParseOptions::max_entity_expansion_bytes`] (default 1 MB) caps
//!   total expanded entity text, defeating exponential/quadratic blowup.
//! - **Bounded nesting (stack-exhaustion DoS).**
//!   [`ParseOptions::max_element_depth`] (default 256) caps element
//!   nesting; DTD content models and regular expressions (XSD `pattern`
//!   facets, XPath `matches`/`replace`/`tokenize`) are independently
//!   depth-bounded so malicious schemas and patterns cannot overflow the
//!   parser stack.
//! - **Memory safety by construction.** This crate is
//!   `#![forbid(unsafe_code)]`; the parser cannot produce a buffer
//!   overflow or use-after-free regardless of input.
//!
//! See the Security reference in the SupXML documentation for the full
//! threat model.

#![forbid(unsafe_code)]  // see CONTRIBUTING.md § "Unsafe policy"

// Errors
pub use sup_xml_core::{ErrorDomain, ErrorLevel, XmlError};
pub use sup_xml_core::error::Result;

// Licensing — parsing requires a valid license certificate (verified
// once per process and cached).  `verify_license()` checks eagerly for
// fail-fast startup; otherwise the first parse triggers the check.
pub use sup_xml_core::verify_license;

// Encoding detection / transcoding (Tier 1: UTF-8, ASCII, Latin-1, Windows-1252)
pub use sup_xml_core::encoding;

// ─── Parsing ───────────────────────────────────────────────────────────────
//
// `parse_bytes` / `parse_str` and their `*_opts` / `*_unchecked` siblings
// return [`Document`] — the bump-allocated DOM in `sup_xml_tree::dom`.
// All nodes, attributes, and strings produced by the parser live in a
// single shared allocation, freed wholesale when the `Document` drops.
pub use sup_xml_core::parse_bytes            as parse_bytes;
pub use sup_xml_core::parse_bytes_unchecked  as parse_bytes_unchecked;
pub use sup_xml_core::parse_bytes_in_place   as parse_bytes_in_place;
pub use sup_xml_core::parse_str              as parse_str;
pub use sup_xml_core::parse_ns_str           as parse_ns_str;
pub use sup_xml_core::parse_ns_bytes         as parse_ns_bytes;
pub use sup_xml_core::parse_str_with_recovered as parse_str_with_recovered;
pub use sup_xml_core::parse_bytes_with_recovered as parse_bytes_with_recovered;
pub use sup_xml_core::XmlDeclInfo;
pub use sup_xml_core::StreamParser;
pub use sup_xml_core::{XmlByteStreamReader, DEFAULT_BUFFER_SIZE, HUGE_BUFFER_SIZE};
pub use sup_xml_core::ParseOptions;

// Streaming SAX-style readers (unchanged — these never built a DOM).
pub use sup_xml_core::{Attr, Attrs, Event, EventInto, XmlReader, unescape};
pub use sup_xml_core::iterparse::{IterEvent, Iterparse};
pub use sup_xml_core::{ParseSelectorError, Selector};
pub use sup_xml_core::{BytesAttr, BytesAttrs, BytesEvent, BytesEventInto, XmlBytesReader, unescape_bytes};

// Serialization.
pub use sup_xml_core::{
    serialize_formatted, serialize_to_bytes, serialize_to_string, serialize_with,
};

// XML Catalogs — OASIS-format public/system identifier resolution.
// See COMPARISON.md § "XML Catalogs" for the rationale and what's
// supported.
pub use sup_xml_core::{Catalog, discover_catalog_paths, load_default_catalog};

// External entity / DTD loading — opt in by setting
// `ParseOptions::external_resolver` to one of these.  Without a
// resolver configured, the parser refuses every external
// reference (XXE prevention).  See COMPARISON.md § "External
// entity / DTD loading".
pub use sup_xml_core::{
    ChainedResolver, EntityResolver, FilesystemResolver, InMemoryResolver, ResolveError,
};
#[cfg(feature = "network-resolver")]
pub use sup_xml_core::entity_resolver::NetworkResolver;

// HTML5 round-trip serializer.
pub use sup_xml_core::serialize_html_to_string as serialize_html_to_string;
pub use sup_xml_core::SerializeOptions;
pub use sup_xml_core::OutputCharset;

// Canonical XML (W3C C14N 1.0 + Exclusive C14N 1.0).
pub use sup_xml_core::canonical::{
    canonicalize_node_to_bytes as canonicalize_node_to_bytes,
    canonicalize_node_with     as canonicalize_node_with,
    canonicalize_to_bytes      as canonicalize_to_bytes,
    canonicalize_with          as canonicalize_with,
    include_all                as canonicalize_include_all,
    VisitTarget                as CanonicalizeVisitTarget,
};
pub use sup_xml_core::{C14nMode, CanonicalizeOptions};

// XPath 1.0.
pub use sup_xml_core::{
    xpath_bool    as xpath_bool,
    xpath_count   as xpath_count,
    xpath_eval    as xpath_eval,
    xpath_num     as xpath_num,
    xpath_str     as xpath_str,
    xpath_strings as xpath_strings,
    XPathContext   as XPathContext,
    parse_xpath, parse_xpath_with, XPathOptions, XPathValue,
    XPathBindingsBuilder,
};

/// XPath 1.0 + a small subset used for fast streaming-style node matching.
///
/// See [`xpath::pattern::Pattern`] for the libxml2-flavour pattern matcher.
pub mod xpath {
    pub use sup_xml_core::xpath::pattern;
    pub use sup_xml_core::xpath::pattern::Pattern;
}

// Tree types — the DOM shape returned by every parsing entry point.
pub use sup_xml_tree::dom::{
    Attribute, Document, Namespace, Node, NodeKind,
    ChildIter, AttrIter, NsDeclIter, DocumentBuilder,
};
pub use sup_xml_tree::{HtmlDoctype, HtmlMeta, QuirksMode};

// XInclude — `<xi:include href="..."/>` processing.
pub use sup_xml_core::xinclude::process_xincludes as process_xincludes;
pub use sup_xml_core::{XIncludeOptions, XINCLUDE_NS};

// Lenient HTML5 parser (feature `html`).  Driven by html5ever; see
// `sup_xml_core::html` for the full module.  Top-level `parse_html_*`
// functions return [`Document`].
#[cfg(feature = "html")]
pub use sup_xml_core::html::{
    parse_html_str, parse_html_str_opts, parse_html_str_with_recovered,
    parse_html_bytes, parse_html_bytes_opts, parse_html_bytes_with_recovered,
    HtmlAttribute, HtmlAttrs, HtmlBytesReader,
    HtmlEvent, HtmlParseOptions, HtmlReader, HtmlSaxHandler, HtmlSaxParser,
};

// Serde-driven typed deserialization (feature-gated).
#[cfg(feature = "serde")]
pub mod de;

// Async I/O wrappers — `tokio` feature.  Slurp via async read,
// then dispatch to the synchronous parser.  See module docs.
#[cfg(feature = "tokio")]
pub mod async_io;

// XML Schema 1.0 validation (feature-gated).
#[cfg(feature = "xsd")]
pub mod xsd {
    //! XML Schema 1.0 — schema compiler and instance validator.
    //!
    //! Re-exports the public surface of [`sup_xml_core::xsd`].  See that
    //! module for the conventions.
    //!
    //! # Quick start
    //!
    //! ```ignore
    //! use sup_xml::xsd::Schema;
    //!
    //! let schema = Schema::compile_str(r#"
    //!     <xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"
    //!                targetNamespace="urn:demo" xmlns="urn:demo">
    //!       <xs:element name="port" type="xs:int"/>
    //!     </xs:schema>"#)?;
    //!
    //! schema.validate_str(r#"<port xmlns="urn:demo">8080</port>"#)?;
    //! ```
    pub use sup_xml_core::xsd::{
        BuiltinType, FsResolver, InMemoryResolver, NoResolver, Schema, SchemaCompileError,
        SchemaOptions, SchemaResolver, SchemaVersion, ValidationError, ValidationIssue,
        ValidationKind, ValidationOptions, QName, TypeRef,
    };
}

// XSLT 1.0 transforms (feature-gated).
#[cfg(feature = "xslt")]
pub mod xslt {
    //! XSLT 1.0 transform engine.
    //!
    //! Re-exports the public surface of [`sup_xml_xslt`].
    //!
    //! # Quick start
    //!
    //! ```ignore
    //! use sup_xml::{parse_str, ParseOptions};
    //! use sup_xml::xslt::Stylesheet;
    //!
    //! let xsl = r#"<xsl:stylesheet version="1.0"
    //!     xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
    //!   <xsl:template match="/"><out><xsl:value-of select="."/></out></xsl:template>
    //! </xsl:stylesheet>"#;
    //! let style = Stylesheet::compile_str(xsl)?;
    //!
    //! let opts = ParseOptions { namespace_aware: true, ..Default::default() };
    //! let doc  = parse_str("<r>hello</r>", &opts)?;
    //! let result = style.apply(&doc)?;
    //! println!("{}", result.to_string()?);
    //! ```
    pub use sup_xml_xslt::{
        Stylesheet, XsltError,
        loader::{Loader, FilesystemLoader, InMemoryLoader, NullLoader},
        extensions::{ExtensionFunctions, Extensions},
        result_tree::{ResultTree, ResultNode},
    };

    /// Schematron — ISO/IEC 19757-3 rule-based validation.  Built on
    /// top of the XSLT/XPath engine.
    pub mod schematron {
        pub use sup_xml_xslt::schematron::{
            Schematron, ValidationReport, Finding, FindingKind,
        };
    }
}
