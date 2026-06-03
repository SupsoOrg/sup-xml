#![forbid(unsafe_code)]

//! Event types emitted by [`HtmlReader`](super::stream::HtmlReader)
//! and dispatched to [`HtmlSaxHandler`](super::stream::HtmlSaxHandler).
//!
//! These are tree-construction events (post-html5ever insertion-mode
//! processing), not raw tokens — implicit `<html>`/`<head>`/`<body>`
//! insertion, void-element handling, and tag-soup recovery have all
//! happened by the time you see an event.
//!
//! The events borrow from the reader's internal owned storage, so they
//! are valid until the next [`next()`](super::stream::HtmlReader::next)
//! call.  Materialise anything you need to keep into owned types
//! before advancing.
//!
//! # Naming
//!
//! Struct-variant style mirrors the XML side
//! ([`Event`](crate::reader::Event)).  Separate payload types are
//! avoided to keep the public type surface small and to prevent name
//! clashes (e.g. with [`HtmlDoctype`](sup_xml_tree::HtmlDoctype) on
//! `Document::html_metadata`).

/// A tree-construction event emitted by the streaming HTML parser.
#[derive(Debug)]
pub enum HtmlEvent<'a> {
    /// An element start tag.  Self-closing source forms (`<br/>`)
    /// and void elements (`<br>`) both emit a single `StartElement`;
    /// for void elements no matching `EndElement` follows.  For
    /// non-void elements the matching `EndElement` follows after
    /// the element's content events.
    StartElement {
        /// The lower-cased element name, e.g. `"div"`.
        name: &'a str,
        /// Iterable view over the element's attributes.
        attributes: HtmlAttrs<'a>,
    },
    /// An element end tag.  Emitted for every non-void element that
    /// was opened, including ones implicitly closed by html5ever's
    /// recovery.
    EndElement {
        /// The lower-cased element name being closed.
        name: &'a str,
    },
    /// A character data run.  Adjacent text events from html5ever
    /// are coalesced into a single event by the streaming sink, so
    /// consumers don't see arbitrarily-fragmented text.
    Text(&'a str),
    /// An HTML comment, with the surrounding `<!--` / `-->`
    /// delimiters stripped.
    Comment(&'a str),
    /// The document `<!DOCTYPE>` declaration.  Emitted at most once
    /// per document, before any element events.
    Doctype {
        name: &'a str,
        public_id: &'a str,
        system_id: &'a str,
    },
    /// End of input.  Subsequent calls to
    /// [`HtmlReader::next`](super::stream::HtmlReader::next) keep
    /// returning `Eof`.
    Eof,
}

/// Iterable view over an element's attributes.  Cheap to clone
/// (it's `Copy`).
#[derive(Debug, Clone, Copy)]
pub struct HtmlAttrs<'a> {
    pub(crate) inner: &'a [OwnedAttr],
}

impl<'a> HtmlAttrs<'a> {
    /// Iterate attributes in source order.
    pub fn iter(&self) -> HtmlAttrsIter<'a> {
        HtmlAttrsIter {
            slice: self.inner,
            pos: 0,
        }
    }

    /// Number of attributes on this element.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True if no attributes.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Look up an attribute value by lower-case name.  O(n) over
    /// the attribute list — for small attribute counts this is
    /// faster than constructing a `HashMap`.
    pub fn get(&self, name: &str) -> Option<&'a str> {
        self.inner
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.value.as_str())
    }
}

/// Iterator returned by [`HtmlAttrs::iter`].
pub struct HtmlAttrsIter<'a> {
    slice: &'a [OwnedAttr],
    pos: usize,
}

impl<'a> Iterator for HtmlAttrsIter<'a> {
    type Item = HtmlAttribute<'a>;
    fn next(&mut self) -> Option<HtmlAttribute<'a>> {
        let a = self.slice.get(self.pos)?;
        self.pos += 1;
        Some(HtmlAttribute {
            name: &a.name,
            value: &a.value,
        })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = self.slice.len() - self.pos;
        (rem, Some(rem))
    }
}

impl<'a> ExactSizeIterator for HtmlAttrsIter<'a> {}

/// A single attribute on a start tag.
#[derive(Debug, Clone, Copy)]
pub struct HtmlAttribute<'a> {
    pub name: &'a str,
    pub value: &'a str,
}

// ── internal owned storage ───────────────────────────────────────────────────

use crate::error::XmlError;

/// Owned event variant stored in the streaming sink's queue.  Not
/// public; the public surface is [`HtmlEvent`] which borrows from
/// these.
#[derive(Debug)]
pub(crate) enum OwnedEvent {
    StartElement {
        name: String,
        attrs: Vec<OwnedAttr>,
    },
    EndElement {
        name: String,
    },
    Text(String),
    Comment(String),
    Doctype {
        name: String,
        public_id: String,
        system_id: String,
    },
    /// A parse error reported by html5ever.  Filtered out before
    /// reaching the consumer — surfaces as `Result::Err` in strict
    /// mode or accumulates in `recovered_errors()` in lenient mode.
    ParseError(XmlError),
    Eof,
}

#[derive(Debug)]
pub(crate) struct OwnedAttr {
    pub name: String,
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_attrs() -> Vec<OwnedAttr> {
        vec![
            OwnedAttr { name: "id".into(),    value: "main".into() },
            OwnedAttr { name: "class".into(), value: "x y".into() },
        ]
    }

    #[test]
    fn html_attrs_len_and_is_empty() {
        let attrs = sample_attrs();
        let v = HtmlAttrs { inner: &attrs };
        assert_eq!(v.len(), 2);
        assert!(!v.is_empty());

        let empty: Vec<OwnedAttr> = Vec::new();
        let v2 = HtmlAttrs { inner: &empty };
        assert_eq!(v2.len(), 0);
        assert!(v2.is_empty());
    }

    #[test]
    fn html_attrs_iter_yields_in_source_order() {
        let attrs = sample_attrs();
        let v = HtmlAttrs { inner: &attrs };
        let names: Vec<&str> = v.iter().map(|a| a.name).collect();
        assert_eq!(names, vec!["id", "class"]);
        let values: Vec<&str> = v.iter().map(|a| a.value).collect();
        assert_eq!(values, vec!["main", "x y"]);
    }

    #[test]
    fn html_attrs_iter_size_hint_exact() {
        let attrs = sample_attrs();
        let v = HtmlAttrs { inner: &attrs };
        let mut it = v.iter();
        assert_eq!(it.size_hint(), (2, Some(2)));
        let _ = it.next();
        assert_eq!(it.size_hint(), (1, Some(1)));
        // ExactSizeIterator is implemented — len() should mirror size_hint.
        assert_eq!(it.len(), 1);
        let _ = it.next();
        assert_eq!(it.size_hint(), (0, Some(0)));
        assert!(it.next().is_none());
    }

    #[test]
    fn html_attrs_get_finds_by_lowercase_name() {
        let attrs = sample_attrs();
        let v = HtmlAttrs { inner: &attrs };
        assert_eq!(v.get("id"),    Some("main"));
        assert_eq!(v.get("class"), Some("x y"));
        assert_eq!(v.get("missing"), None);
    }

    #[test]
    fn html_attrs_is_copy() {
        // Smoke test: HtmlAttrs derives Copy, so passing by value
        // doesn't move.
        let attrs = sample_attrs();
        let v = HtmlAttrs { inner: &attrs };
        let v2 = v; // copy
        let v3 = v; // copy again — would fail to compile if not Copy
        assert_eq!(v2.len(), v3.len());
    }
}
