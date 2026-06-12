//! Serde-driven XML deserialization.
//!
//! Map XML directly into typed Rust values:
//!
//! ```
//! use serde::Deserialize;
//!
//! #[derive(Deserialize, Debug, PartialEq)]
//! struct Page {
//!     #[serde(rename = "@id")]
//!     id: u32,
//!     title: String,
//! }
//!
//! let xml = r#"<page id="7"><title>hello</title></page>"#;
//! let page: Page = sup_xml::de::from_str(xml).unwrap();
//! assert_eq!(page, Page { id: 7, title: "hello".into() });
//! ```
//!
//! # XML → serde conventions
//!
//! | XML construct                | Serde concept             | Field name         |
//! |------------------------------|---------------------------|--------------------|
//! | Element attribute            | struct field              | `@name`            |
//! | Element text-only content    | scalar / string field     | (the field itself) |
//! | Element with children        | struct                    | element name       |
//! | Repeated same-name children  | `Vec<T>`                  | element name       |
//! | Optional element             | `Option<T>`               | element name       |
//! | Mixed text + child elements  | struct field              | `$text`            |
//! | Heterogeneous element body   | enum sequence             | `$value`           |
//!
//! `$text` collects all text/CDATA between the start and end tag — atomic
//! content only (primitives, strings, unit enums, space-delimited lists).
//! `$value` collects child *elements* — each one becomes a sequence item,
//! and for enum types the element tag picks the variant.  If both are
//! declared on the same struct, text routes to `$text` and remaining child
//! elements route to `$value`.
//!
//! # Borrowed deserialization
//!
//! Deserializing into `&'de str` borrows directly from the source.  This
//! works only when **all** of the following hold for the field's content:
//!
//! 1. The input is UTF-8 (always true for [`from_str`]).
//! 2. The text contains no entity references (`&amp;`, etc.).
//! 3. The text contains no character references (`&#xNN;`).
//! 4. The text comes from a *single* `Text` or `CData` event — adjacent
//!    text and CDATA segments get merged into an owned `String` and lose
//!    borrow eligibility.
//!
//! Use `String` (or `Cow<'_, str>`) to handle either case.
//!
//! # Options
//!
//! Pass [`DeOptions`] to [`from_str_opts`] / [`from_bytes_opts`] to tune:
//! the underlying `ParseOptions`, the magic field names, the attribute
//! prefix, unknown-field handling, and `xsi:nil` behaviour.

use std::fmt;

use serde::de::{self, Deserialize, Error as _};
use sup_xml_core::ParseOptions;

mod deserializer;

pub use deserializer::XmlDeserializer;

// ── public entry points ──────────────────────────────────────────────────────

/// Deserialize a Rust value from an XML string.  Borrows from the input
/// where possible (see the [borrowed deserialization](crate::de#borrowed-deserialization)
/// section for the precise constraints).
pub fn from_str<'de, T: Deserialize<'de>>(s: &'de str) -> Result<T, DeError> {
    from_str_opts(s, DeOptions::default())
}

/// Deserialize from an XML byte slice.  The bytes must be valid UTF-8.
pub fn from_bytes<'de, T: Deserialize<'de>>(b: &'de [u8]) -> Result<T, DeError> {
    from_bytes_opts(b, DeOptions::default())
}

/// Like [`from_str`], with caller-supplied [`DeOptions`].
pub fn from_str_opts<'de, T: Deserialize<'de>>(s: &'de str, opts: DeOptions) -> Result<T, DeError> {
    let mut de = XmlDeserializer::from_str_opts(s, opts);
    let t = T::deserialize(&mut de)?;
    Ok(t)
}

/// Like [`from_bytes`], with caller-supplied [`DeOptions`].
pub fn from_bytes_opts<'de, T: Deserialize<'de>>(b: &'de [u8], opts: DeOptions) -> Result<T, DeError> {
    let s = simdutf8::compat::from_utf8(b).map_err(|e| DeError::custom(format!("invalid UTF-8: {e}")))?;
    from_str_opts(s, opts)
}

// ── options ──────────────────────────────────────────────────────────────────

/// Tunables for the deserializer.  Defaults match quick-xml's conventions
/// for drop-in compatibility.
#[derive(Debug, Clone)]
pub struct DeOptions {
    /// Forwarded to the underlying [`XmlReader`](crate::XmlReader).
    pub parse: ParseOptions,
    /// Field name that collects element text content.  Default `"$text"`.
    pub text_field_name: &'static str,
    /// Field name that collects heterogeneous child elements as a sequence.
    /// Default `"$value"`.
    pub value_field_name: &'static str,
    /// Prefix that marks a field as mapping to an XML attribute.
    /// Default `'@'`.
    pub attribute_prefix: char,
    /// If false, unknown attributes and unknown child elements produce an
    /// error instead of being skipped.  Default `true`.
    pub allow_unknown_fields: bool,
    /// If true, an element carrying `xsi:nil="true"` deserializes to
    /// serde `None` and its content is skipped.  Default `true`.
    pub honor_xsi_nil: bool,
}

impl Default for DeOptions {
    fn default() -> Self {
        Self {
            parse:                ParseOptions::default(),
            text_field_name:      "$text",
            value_field_name:     "$value",
            attribute_prefix:     '@',
            allow_unknown_fields: true,
            honor_xsi_nil:        true,
        }
    }
}

// ── error type ───────────────────────────────────────────────────────────────

/// Error returned by the deserializer.
#[derive(Debug, Clone)]
pub struct DeError {
    /// Human-readable description of the failure.  Includes both serde-side
    /// errors (type mismatch, missing field, bad number format) and
    /// parser-side errors (malformed XML, unterminated tag) bubbled up
    /// from the underlying [`XmlReader`](crate::XmlReader).
    pub message: String,
}

impl DeError {
    pub(crate) fn msg(s: impl Into<String>) -> Self {
        Self { message: s.into() }
    }
}

impl fmt::Display for DeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DeError {}

impl de::Error for DeError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self { message: msg.to_string() }
    }
}

impl From<sup_xml_core::XmlError> for DeError {
    fn from(e: sup_xml_core::XmlError) -> Self {
        Self { message: e.to_string() }
    }
}

