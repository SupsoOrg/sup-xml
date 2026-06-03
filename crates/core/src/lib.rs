//! Core parsing, serialization, namespace resolution, and XPath evaluation for SupXML.
//!
//! This crate is the implementation heart of SupXML.  Most users should
//! depend on the `sup-xml` crate instead, which re-exports everything here
//! through a stable public surface.

pub mod canonical;
pub mod catalog;
pub mod dtd;
pub mod iterparse;
pub mod relaxng;
pub mod xinclude;
pub mod charsets;
pub mod encoding;
pub mod entity_resolver;
pub mod error;
pub mod options;
pub mod output;
pub mod parser;
pub mod serializer;
pub mod stream_parser;
pub mod streaming_reader;
pub mod ns_helpers;
pub mod reader;
pub mod regex;
pub mod selector;
pub mod xml_bytes_reader;
pub(crate) mod scanner;
/// Byte-offset → `(line, column)` translation over source bytes, for
/// consumers that compute `node.line` / `Element.sourceline` outside the
/// parser (e.g. the C-ABI incremental push parser).
pub use scanner::compute_line_col;
pub mod types;
pub mod xpath;

#[cfg(feature = "xsd")]
pub mod xsd;

#[cfg(feature = "html")]
pub mod html;
pub mod license_gate;

/// Re-export of the [`rust_decimal`] crate that backs XPath 2.0's
/// `xs:decimal` arithmetic.  Exposed at the crate root so embedders
/// can pattern-match on [`xpath::eval::Numeric::Decimal`] payloads
/// and use the value with their own [`rust_decimal`]-aware code
/// (sqlx, sea-orm, postgres, …) without adding a separate dependency
/// and risking version skew.  Tied to `rust_decimal` 1.x; bumping it
/// is a major version of this crate.
pub use rust_decimal;

pub use canonical::{C14nMode, CanonicalizeOptions};
pub use catalog::{Catalog, discover_catalog_paths, load_default as load_default_catalog};
pub use xinclude::{XIncludeOptions, XINCLUDE_NS};
pub use entity_resolver::{
    ChainedResolver, EntityResolver, FilesystemResolver, InMemoryResolver, ResolveError,
};
pub use error::{ErrorDomain, ErrorLevel, XmlError};
pub use license_gate::verify_license;
pub use options::ParseOptions;
pub use parser::{
    parse_bytes, parse_bytes_unchecked, parse_str,
    parse_bytes_in_place,
    parse_ns_str, parse_ns_bytes,
    parse_str_with_recovered, parse_bytes_with_recovered,
};
pub use serializer::serialize_html_to_string;
pub use serializer::{
    serialize_formatted, serialize_to_bytes, serialize_to_string,
    serialize_with, SerializeOptions,
};
pub use output::OutputCharset;
pub use stream_parser::StreamParser;
pub use streaming_reader::{XmlByteStreamReader, DEFAULT_BUFFER_SIZE, HUGE_BUFFER_SIZE};
pub use reader::{Attr, Attrs, Event, EventInto, XmlReader, unescape};
pub use selector::{ParseSelectorError, Selector};
pub use xml_bytes_reader::{BytesAttr, BytesAttrs, BytesEvent, BytesEventInto, XmlBytesReader, XmlDeclInfo, resolve_uri, unescape_bytes};
pub use types::XmlChar;
pub use xpath::{parse_xpath, parse_xpath_with, XPathOptions, XPathValue};
pub use xpath::XPathBindingsBuilder;
pub use xpath::{
    xpath_bool, xpath_count, xpath_eval, xpath_num,
    xpath_str, xpath_strings, XPathContext,
};
