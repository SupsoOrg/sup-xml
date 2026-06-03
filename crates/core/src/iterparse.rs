//! Streaming-iterator wrapper for the SAX reader.
//!
//! The lower-level [`crate::XmlReader`] / [`crate::XmlBytesReader`]
//! deliver events whose borrows tie back to the reader's source
//! buffer — that's the right shape for zero-copy consumers, but it
//! prevents the reader from implementing `Iterator` directly
//! (each `next()` would borrow `&mut self`, so callers can hold at
//! most one event at a time).
//!
//! [`Iterparse`] addresses the common case where the caller wants
//! an actual `Iterator` and is willing to pay one `String`
//! allocation per event for owned names and text.  Events arrive
//! alongside the current ancestor path so handlers don't need to
//! maintain their own depth tracker.
//!
//! # Quick example
//!
//! ```no_run
//! use sup_xml_core::iterparse::{Iterparse, IterEvent};
//!
//! let xml = b"<catalog><book id='1'/><book id='2'/></catalog>";
//! for ev in Iterparse::from_bytes(xml).unwrap() {
//!     let ev = ev.unwrap();
//!     if let IterEvent::EndElement { name, path, .. } = ev {
//!         if name == "book" { println!("done with {}", path); }
//!     }
//! }
//! ```
//!
//! For consumers that need byte-exact zero-copy events (every
//! payload borrowed from the source buffer), reach for
//! [`crate::XmlBytesReader`] directly.

use std::collections::HashMap;

use crate::error::XmlError;
use crate::reader::{Attr, Event, XmlReader};
use crate::options::ParseOptions;

/// One streamed event.  Names, attribute values, and text content
/// are owned `String`s so the iterator can yield items
/// independent of the reader's source buffer lifetime.
#[derive(Debug, Clone)]
pub enum IterEvent {
    /// Opening (or empty-element) start tag.  Attributes are
    /// pre-collected; `path` is the current ancestor chain
    /// *including* this element.
    StartElement {
        name: String,
        attrs: Vec<(String, String)>,
        path: String,
        depth: usize,
    },
    /// Closing tag — emitted once for each `StartElement`,
    /// including for empty (`<br/>`) elements.  `path` is the
    /// chain *up to* this element (with the element itself as
    /// the last segment).
    EndElement {
        name: String,
        path: String,
        depth: usize,
    },
    /// Character data between tags.
    Text {
        content: String,
        path: String,
        depth: usize,
    },
    /// CDATA section.
    CData {
        content: String,
        path: String,
        depth: usize,
    },
    /// Comment (no payload exposed by the path — comments don't
    /// add a path segment).
    Comment {
        content: String,
        depth: usize,
    },
    /// Processing instruction.
    Pi {
        target: String,
        data: String,
        depth: usize,
    },
}

/// Owns an [`XmlReader`] plus the bookkeeping needed to produce
/// owned iterator items: a path stack, per-frame sibling counters
/// (so `book[2]` etc. show up in the path), and an attribute
/// scratch buffer.
pub struct Iterparse<'src> {
    reader: XmlReader<'src>,
    /// Element names on the open-element stack (only Element
    /// frames; comments / PIs don't push).
    stack: Vec<StackFrame>,
    /// Indicates whether the EOF event has been delivered; the
    /// iterator returns `None` after that.
    done: bool,
}

struct StackFrame {
    name: String,
    /// Sibling-index counter for child element names, used to build
    /// `/a/b[2]` style paths.
    children: HashMap<String, u32>,
}

impl<'src> Iterparse<'src> {
    /// Construct from a borrowed byte slice.  Defaults to
    /// [`ParseOptions::default()`] — well-formedness checks on,
    /// entities resolved, no external loading.
    pub fn from_bytes(bytes: &'src [u8]) -> Result<Self, XmlError> {
        let reader = XmlReader::from_bytes(bytes)?;
        Ok(Self { reader, stack: Vec::new(), done: false })
    }

    /// Like [`Iterparse::from_bytes`] but consults the supplied
    /// `ParseOptions` (e.g. `recovery_mode: true` for tolerant
    /// parsing, or a configured `external_resolver`).
    pub fn from_bytes_with(bytes: &'src [u8], opts: &ParseOptions) -> Result<Self, XmlError> {
        let reader = XmlReader::from_bytes(bytes)?.with_options(opts.clone());
        Ok(Self { reader, stack: Vec::new(), done: false })
    }

    /// Build the path string from the current open-element stack.
    /// `/a/b[2]/c` style; sibling indices are emitted only when a
    /// name repeats so single-occurrence paths stay readable.
    fn current_path(&self) -> String {
        if self.stack.is_empty() { return "/".into(); }
        let mut s = String::new();
        // Each frame's `children` map counts *its children*, not
        // its own siblings.  To emit `name[n]` for the frame
        // itself we'd need the parent's counter snapshot at the
        // moment this frame was pushed — which we stash on the
        // frame.  For simplicity here we re-derive from the
        // counters: if parent's count for this name is >= 2, we
        // assume this is the latest of a series.  Good enough for
        // diagnostic paths; the worst that happens on a
        // mis-classification is a slightly-stale index.
        let mut prev_children: Option<&HashMap<String, u32>> = None;
        for f in &self.stack {
            s.push('/');
            s.push_str(&f.name);
            if let Some(counters) = prev_children {
                if let Some(&n) = counters.get(&f.name) {
                    if n >= 2 {
                        s.push_str(&format!("[{n}]"));
                    }
                }
            }
            prev_children = Some(&f.children);
        }
        s
    }
}

impl<'src> Iterator for Iterparse<'src> {
    type Item = Result<IterEvent, XmlError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done { return None; }
        let depth = self.stack.len();
        let ev = match self.reader.next() {
            Ok(ev) => ev,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        match ev {
            Event::Eof => {
                self.done = true;
                None
            }
            Event::StartElement(tag) => {
                let name = tag.name().to_string();
                let attrs: Vec<(String, String)> = tag.attrs()
                    .filter_map(|a: Result<Attr<'_>, _>| a.ok())
                    .map(|a| (a.name().to_string(), a.value().to_string()))
                    .collect();
                // Bump parent's per-name counter so siblings show
                // up as `name[2]`, `name[3]` etc. in `path`.
                if let Some(parent) = self.stack.last_mut() {
                    *parent.children.entry(name.clone()).or_insert(0) += 1;
                }
                self.stack.push(StackFrame {
                    name: name.clone(),
                    children: HashMap::new(),
                });
                let path = self.current_path();
                let new_depth = self.stack.len();
                Some(Ok(IterEvent::StartElement {
                    name, attrs, path, depth: new_depth,
                }))
            }
            Event::EndElement(end) => {
                let path = self.current_path();
                let name = end.name().to_string();
                self.stack.pop();
                Some(Ok(IterEvent::EndElement { name, path, depth }))
            }
            Event::Text(t) => {
                let path = self.current_path();
                Some(Ok(IterEvent::Text {
                    content: t.as_str().to_string(),
                    path, depth,
                }))
            }
            Event::CData(c) => {
                let path = self.current_path();
                Some(Ok(IterEvent::CData {
                    content: c.as_str().to_string(),
                    path, depth,
                }))
            }
            Event::Comment(c) => Some(Ok(IterEvent::Comment {
                content: c.as_str().to_string(),
                depth,
            })),
            Event::Pi(p) => Some(Ok(IterEvent::Pi {
                target: p.target().to_string(),
                data:   p.content().to_string(),
                depth,
            })),
            Event::EntityRef(_) => {
                // Skip — typed streaming consumers don't get a
                // meaningful payload from a bare reference, and
                // the default `resolve_entities: true` path
                // already inlines them as text.  Recurse.
                self.next()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(xml: &[u8]) -> Vec<IterEvent> {
        Iterparse::from_bytes(xml).unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn yields_start_end_text() {
        let events = collect(b"<r>hi</r>");
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0],
            IterEvent::StartElement { name, .. } if name == "r"));
        assert!(matches!(&events[1],
            IterEvent::Text { content, .. } if content == "hi"));
        assert!(matches!(&events[2],
            IterEvent::EndElement { name, .. } if name == "r"));
    }

    #[test]
    fn path_includes_sibling_index() {
        let events = collect(
            b"<r><a/><a><b/></a><a/></r>",
        );
        // Find the EndElement of the middle <a> — should be
        // at path /r/a[2] (it's the 2nd <a> child of <r>).
        let mid_end = events.iter().find_map(|e| match e {
            IterEvent::EndElement { name, path, .. }
                if name == "a" && path.contains("[2]") => Some(path.clone()),
            _ => None,
        });
        assert_eq!(mid_end.as_deref(), Some("/r/a[2]"));
    }

    #[test]
    fn captures_attrs() {
        let events = collect(br#"<r a="1" b="2"/>"#);
        match &events[0] {
            IterEvent::StartElement { attrs, .. } => {
                assert_eq!(attrs.len(), 2);
                assert_eq!(attrs[0], ("a".into(), "1".into()));
                assert_eq!(attrs[1], ("b".into(), "2".into()));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn malformed_input_surfaces_error_then_stops() {
        let mut it = Iterparse::from_bytes(b"<r><unclosed>").unwrap();
        let mut saw_err = false;
        for ev in it.by_ref() {
            if ev.is_err() { saw_err = true; break; }
        }
        assert!(saw_err);
        assert!(it.next().is_none(), "iterator must stop after first error");
    }

    #[test]
    fn comments_and_pis_dont_push_path() {
        let events = collect(b"<r><!-- c --><?pi data?><a/></r>");
        let a_start = events.iter().find_map(|e| match e {
            IterEvent::StartElement { name, path, .. } if name == "a" => Some(path.clone()),
            _ => None,
        });
        assert_eq!(a_start.as_deref(), Some("/r/a"));
    }
}
